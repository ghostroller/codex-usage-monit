use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::CollectConfig;
use crate::history::{HistoryStore, default_history_root};
use crate::output::{
    OutputFormat, OutputRequest, Section, render_output, request_is_failure, request_is_partial,
};
use crate::perf::{HistoryMetrics, PerfLog};
use crate::rollout::RolloutCache;
use crate::service::{
    RecorderStatusFile, ServiceOptions, default_status_file, install as install_service,
    status as service_status, uninstall as uninstall_service, write_recorder_status,
};
use crate::snapshot::{
    collect_limits_snapshot, collect_snapshot, collect_snapshot_cached,
    collect_snapshot_cached_if_changed,
};
use crate::startup::StartupTrace;
use crate::tui::Theme;

struct PerfLogGuard {
    log: PerfLog,
    path: Option<PathBuf>,
    reported_error: Option<String>,
}

impl PerfLogGuard {
    fn new(log: PerfLog, path: Option<PathBuf>) -> Self {
        Self {
            log,
            path,
            reported_error: None,
        }
    }

    fn report_error(&mut self) {
        let mut stderr = io::stderr().lock();
        let _ = self.report_error_to(&mut stderr);
    }

    fn report_error_to(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let Some(error) = self.log.log_error() else {
            return Ok(());
        };
        if self.reported_error.as_deref() == Some(error.as_str()) {
            return Ok(());
        }

        if let Some(path) = self.path.as_deref() {
            writeln!(writer, "warning: --perf-log disabled for {path:?}: {error}")?;
        } else {
            writeln!(writer, "warning: --perf-log disabled: {error}")?;
        }
        self.reported_error = Some(error);
        Ok(())
    }
}

impl Drop for PerfLogGuard {
    fn drop(&mut self) {
        self.log.finish();
        self.report_error();
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "codex-usage-monit",
    version,
    about = "Monitor local Codex tasks, tokens, and quota windows"
)]
pub struct Cli {
    #[arg(long, value_name = "DIR")]
    codex_home: Option<PathBuf>,

    /// Use this Codex executable for account collection.
    #[arg(long, value_name = "FILE")]
    codex_bin: Option<PathBuf>,

    #[arg(long, default_value_t = 7)]
    days: i64,

    #[arg(long, default_value_t = 500)]
    max_files: usize,

    #[arg(long, default_value_t = 5)]
    active_grace_minutes: u64,

    #[arg(long)]
    offline: bool,

    #[arg(long)]
    redact_content: bool,

    /// Disable the user-level parsed rollout cache.
    #[arg(long, global = true)]
    no_rollout_cache: bool,

    /// TUI color theme; `bright` is an alias for `light`.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,

    /// Write incremental cold-start timing events as JSONL.
    #[arg(long, value_name = "FILE")]
    startup_log: Option<PathBuf>,

    /// Write low-overhead runtime performance samples as JSONL.
    #[arg(long, global = true, value_name = "FILE")]
    perf_log: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print a complete or filtered one-shot snapshot.
    Snapshot(SnapshotArgs),
    /// Print quota windows.
    Limits(OutputArgs),
    /// Print recent tasks.
    Tasks(OutputArgs),
    /// Print turns, optionally restricted to one thread.
    Turns(TurnArgs),
    /// Print model usage for the current quota window.
    Models(OutputArgs),
    /// Print quota attribution and data-quality details.
    Attribution(OutputArgs),
    /// Print per-task, turn, and model usage for each current reset cycle.
    Windows(OutputArgs),
    /// Continuously record local usage and quota history without opening the TUI.
    Record(RecordArgs),
    /// Install, inspect, or remove the optional per-user background recorder.
    Service(ServiceArgs),
    /// Profile the normal TUI cold-start path without entering interactive mode.
    DebugStartup(DebugStartupArgs),
}

#[derive(Clone, Debug, Args)]
struct RecordArgs {
    /// Explicitly run as a foreground process; service managers supervise this process.
    #[arg(long)]
    foreground: bool,

    /// Override the history directory selected from the platform state directory.
    #[arg(long, value_name = "DIR")]
    history_dir: Option<PathBuf>,

    /// Write recorder health and heartbeat state to this file.
    #[arg(long, value_name = "FILE")]
    status_file: Option<PathBuf>,

    /// Frequency for rescanning local rollout data.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(5..=3600))]
    local_interval_seconds: u64,

    /// Frequency for refreshing account quota data.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(30..=3600))]
    account_interval_seconds: u64,
}

#[derive(Clone, Debug, Args)]
struct ServiceArgs {
    #[command(subcommand)]
    action: ServiceAction,
}

#[derive(Clone, Copy, Debug, Subcommand)]
enum ServiceAction {
    /// Install and start the current user's background recorder.
    Install,
    /// Show registration state and the recorder's latest heartbeat.
    Status,
    /// Stop and remove the current user's background recorder.
    Uninstall,
}

#[derive(Clone, Debug, Args)]
struct DebugStartupArgs {
    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,

    /// Width of the headless first-frame render.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u16).range(20..=1000))]
    width: u16,

    /// Height of the headless first-frame render.
    #[arg(long, default_value_t = 40, value_parser = clap::value_parser!(u16).range(8..=500))]
    height: u16,
}

#[derive(Clone, Debug, Args)]
struct OutputArgs {
    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,

    #[arg(long)]
    compact: bool,
}

#[derive(Clone, Debug, Args)]
struct SnapshotArgs {
    #[command(flatten)]
    output: OutputArgs,

    #[arg(long, value_enum, value_delimiter = ',')]
    section: Vec<SectionArg>,
}

#[derive(Clone, Debug, Args)]
struct TurnArgs {
    #[command(flatten)]
    output: OutputArgs,

    #[arg(long)]
    thread: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ThemeArg {
    Dark,
    #[value(alias = "bright")]
    Light,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SectionArg {
    Limits,
    Tasks,
    Turns,
    Models,
    Attribution,
    Windows,
    Health,
}

impl From<FormatArg> for OutputFormat {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Text => Self::Text,
            FormatArg::Json => Self::Json,
        }
    }
}

impl From<SectionArg> for Section {
    fn from(value: SectionArg) -> Self {
        match value {
            SectionArg::Limits => Self::Limits,
            SectionArg::Tasks => Self::Tasks,
            SectionArg::Turns => Self::Turns,
            SectionArg::Models => Self::Models,
            SectionArg::Attribution => Self::Attribution,
            SectionArg::Windows => Self::Windows,
            SectionArg::Health => Self::Health,
        }
    }
}

impl From<ThemeArg> for Theme {
    fn from(value: ThemeArg) -> Self {
        match value {
            ThemeArg::Dark => Self::Dark,
            ThemeArg::Light => Self::Light,
        }
    }
}

pub fn run() -> Result<i32> {
    let process_started = Instant::now();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = if error.use_stderr() { 64 } else { 0 };
            error.print()?;
            return Ok(exit_code);
        }
    };
    let parsed_at = Instant::now();
    run_with(cli, process_started, parsed_at)
}

fn run_with(cli: Cli, process_started: Instant, parsed_at: Instant) -> Result<i32> {
    let trace_init_started = Instant::now();
    let debug_startup = matches!(cli.command.as_ref(), Some(Command::DebugStartup(_)));
    let trace = if debug_startup || cli.startup_log.is_some() {
        StartupTrace::enabled(process_started, cli.startup_log.as_deref())?
    } else {
        StartupTrace::default()
    };
    let trace_initialized_at = Instant::now();
    let perf_log_path = cli.perf_log.clone();
    let perf_log = perf_log_path
        .as_deref()
        .map(PerfLog::enabled)
        .unwrap_or_default();
    let mut perf_log_guard = PerfLogGuard::new(perf_log.clone(), perf_log_path.clone());
    perf_log_guard.report_error();
    trace.record_interval(
        "cli.parse",
        process_started,
        parsed_at,
        format!("command={}", command_name(cli.command.as_ref())),
    );
    trace.record_interval(
        "startup.trace_init",
        trace_init_started,
        trace_initialized_at,
        format!("file={}", cli.startup_log.is_some()),
    );

    let config_span = trace.span("cli.config");
    let mut config = CollectConfig::default();
    if let Some(codex_home) = cli.codex_home {
        config.codex_home = codex_home;
    }
    config.codex_bin = cli.codex_bin;
    config.lookback_days = cli.days.max(1);
    config.max_files = cli.max_files.max(1);
    config.active_grace = active_grace(cli.active_grace_minutes);
    config.offline = cli.offline;
    config.redact_content = cli.redact_content;
    config.rollout_cache_dir = if cli.no_rollout_cache {
        None
    } else {
        crate::cache::default_rollout_cache_dir()
    };
    config.perf_log = perf_log;
    config.startup_trace = trace.clone();
    config_span.finish_with(|| {
        format!(
            "build={} days={} max_files={} offline={} redact_content={} cache={}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            config.lookback_days,
            config.max_files,
            config.offline,
            config.redact_content,
            if config.rollout_cache_dir.is_some() {
                "enabled"
            } else {
                "disabled"
            }
        )
    });

    let Some(command) = cli.command else {
        if let Some(theme) = cli.theme {
            crate::tui::run_with_theme(config, theme.into())?;
        } else {
            crate::tui::run(config)?;
        }
        return Ok(0);
    };

    if let Command::DebugStartup(args) = &command {
        crate::tui::debug_startup(config, cli.theme.map(Into::into), args.width, args.height)?;
        let output = match args.format {
            FormatArg::Text => trace.render_text(),
            FormatArg::Json => trace.render_json()?,
        };
        write_stdout(&output)?;
        return Ok(0);
    }

    let command = match command {
        Command::Record(args) => return run_recorder(config, args),
        Command::Service(args) => {
            return run_service(&config, args, perf_log_path.as_deref());
        }
        command => command,
    };

    let request = match command {
        Command::Snapshot(args) => OutputRequest {
            format: args.output.format.into(),
            compact: args.output.compact,
            sections: if args.section.is_empty() {
                Section::all()
            } else {
                args.section.into_iter().map(Into::into).collect()
            },
            thread_filter: None,
        },
        Command::Limits(args) => request_for(args, Section::Limits),
        Command::Tasks(args) => request_for(args, Section::Tasks),
        Command::Turns(args) => OutputRequest {
            format: args.output.format.into(),
            compact: args.output.compact,
            sections: BTreeSet::from([Section::Turns]),
            thread_filter: args.thread,
        },
        Command::Models(args) => request_for(args, Section::Models),
        Command::Attribution(args) => request_for(args, Section::Attribution),
        Command::Windows(args) => request_for(args, Section::Windows),
        Command::Record(_) | Command::Service(_) => {
            unreachable!("record and service commands are handled before output routing")
        }
        Command::DebugStartup(_) => unreachable!("debug-startup returned before output routing"),
    };
    let limits_only = request.sections.len() == 1 && request.sections.contains(&Section::Limits);
    let mut result = if limits_only && !config.offline {
        collect_limits_snapshot(&config, None, true)
    } else {
        collect_snapshot(&config, None, true)
    };
    if limits_only && result.snapshot.limits.is_empty() {
        result = collect_snapshot(&config, Some(result.account), false);
    }

    let render_span = trace.span("output.render");
    let output = render_output(&result.snapshot, &request)?;
    render_span.finish_with(|| format!("bytes={}", output.len()));
    let mut stdout = io::stdout().lock();
    let write_span = trace.span("output.write");
    match write_output(&mut stdout, &output) {
        Ok(()) => {
            write_span.finish_with(|| format!("status=ok bytes={}", output.len().saturating_add(1)))
        }
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
            write_span.finish("status=broken_pipe");
            trace.finish("startup.complete", "mode=one_shot status=broken_pipe");
            return Ok(0);
        }
        Err(error) => {
            write_span.finish("status=error");
            trace.finish("startup.failed", "mode=one_shot stage=output.write");
            return Err(error.into());
        }
    }
    trace.finish("startup.complete", "mode=one_shot status=ok");

    Ok(if request_is_failure(&result.snapshot, &request) {
        1
    } else if request_is_partial(&result.snapshot, &request) {
        2
    } else {
        0
    })
}

fn write_stdout(output: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match write_output(&mut stdout, output) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn run_service(config: &CollectConfig, args: ServiceArgs, perf_log: Option<&Path>) -> Result<i32> {
    let history_dir = default_history_root()
        .map(absolute_path)
        .ok_or_else(|| anyhow::anyhow!("a user state directory is unavailable"))?;
    let status_file = default_status_file(&history_dir);
    let mut options = ServiceOptions::new(
        std::env::current_exe().map_err(anyhow::Error::from)?,
        absolute_path(config.codex_home.clone()),
        history_dir,
        status_file,
        perf_log.map(|path| absolute_path(path.to_path_buf())),
    );
    options.lookback_days = config.lookback_days;
    options.max_files = config.max_files;
    options.active_grace_minutes = config.active_grace.as_secs().div_ceil(60);
    options.offline = config.offline;
    options.redact_content = config.redact_content;
    options.no_rollout_cache = config.rollout_cache_dir.is_none();
    if matches!(args.action, ServiceAction::Install) && !config.offline {
        options.codex_bin = Some(resolve_service_codex(config)?);
    }
    if matches!(args.action, ServiceAction::Install) && perf_log.is_some() {
        config.perf_log.finish();
    }
    let status = match args.action {
        ServiceAction::Install => install_service(&options)?,
        ServiceAction::Status => service_status(&options)?,
        ServiceAction::Uninstall => uninstall_service(&options)?,
    };
    let mut lines = vec![
        format!("recorder service: {}", status.state.label()),
        format!("platform: {}", status.platform),
    ];
    if let Some(path) = status.registration_path.as_deref() {
        lines.push(format!("registration: {}", path.display()));
    }
    lines.push(format!(
        "last history heartbeat: {}",
        status
            .last_history_heartbeat
            .map(|at| at.to_rfc3339())
            .unwrap_or_else(|| "unavailable".to_string())
    ));
    lines.push(format!(
        "detail: {}",
        crate::domain::terminal_safe_text(&status.detail)
    ));
    write_stdout(&lines.join("\n"))?;
    Ok(0)
}

fn resolve_service_codex(config: &CollectConfig) -> Result<PathBuf> {
    let current_dir = std::env::current_dir().map_err(anyhow::Error::from)?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    crate::session_launch::resolve_executable(
        "codex",
        config.codex_bin.as_deref(),
        &path,
        &current_dir,
    )
    .map_err(anyhow::Error::new)
}

fn run_recorder(config: CollectConfig, args: RecordArgs) -> Result<i32> {
    if !args.foreground {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "note: record always stays in the foreground; --foreground is used by service registrations"
        );
    }

    let history_dir = absolute_path(
        args.history_dir
            .or_else(default_history_root)
            .ok_or_else(|| anyhow::anyhow!("a history state directory is unavailable"))?,
    );
    let status_file = absolute_path(
        args.status_file
            .unwrap_or_else(|| default_status_file(&history_dir)),
    );
    let mut history_store = HistoryStore::new(history_dir, &config.codex_home);
    let mut rollout_cache = RolloutCache::new();
    let mut cached_account = None;
    let local_interval = Duration::from_secs(args.local_interval_seconds);
    let account_interval = Duration::from_secs(args.account_interval_seconds);
    let mut next_local = Instant::now();
    let mut next_account = Instant::now();
    let mut account_issue = None;
    let mut recorder_status =
        RecorderStatusFile::started(Utc::now(), history_store.namespace().to_string());
    write_recorder_status(&status_file, &recorder_status)
        .map_err(|error| anyhow::anyhow!("could not initialize recorder status: {error}"))?;

    loop {
        let now = Instant::now();
        let local_due = now >= next_local;
        let account_due = !config.offline && now >= next_account;
        if local_due || account_due {
            let result = if account_due || cached_account.is_none() {
                Some(collect_snapshot_cached(
                    &config,
                    cached_account.clone(),
                    account_due,
                    &mut rollout_cache,
                ))
            } else {
                collect_snapshot_cached_if_changed(
                    &config,
                    cached_account.clone(),
                    &mut rollout_cache,
                )
            };

            let attempt_at = Utc::now();
            if let Some(result) = result {
                cached_account = Some(result.account.clone());
                if account_due {
                    account_issue = recorder_account_issue(&result.snapshot);
                }
                let collection_issue = result
                    .snapshot
                    .errors
                    .first()
                    .map(|error| format!("collection failed: {error}"))
                    .or_else(|| account_issue.clone());
                let history_started = Instant::now();
                let history_result = history_store.record(&result.history_observation);
                let history_elapsed = history_started.elapsed();
                let mut history_metrics =
                    HistoryMetrics::with_durations(history_elapsed, history_elapsed, None);
                history_metrics.quota_points =
                    u64::try_from(result.history_observation.quota_points.len())
                        .unwrap_or(u64::MAX);
                history_metrics.half_hour_buckets =
                    u64::try_from(result.history_observation.half_hour_buckets.len())
                        .unwrap_or(u64::MAX);
                history_metrics.weekly_local_points =
                    u64::try_from(result.history_observation.weekly_local_points.len())
                        .unwrap_or(u64::MAX);
                if let Ok(report) = &history_result {
                    history_metrics.shards_written =
                        u64::try_from(report.shards_written).unwrap_or(u64::MAX);
                    history_metrics.shards_skipped =
                        u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
                    history_metrics.shards_pruned =
                        u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
                    history_metrics.warnings =
                        u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
                    history_metrics.read_only = report.read_only;
                } else {
                    history_metrics.warnings = 1;
                }
                config.perf_log.record_history(history_metrics);
                match history_result {
                    Ok(report) if !report.read_only => {
                        let history_warning = report
                            .warnings
                            .first()
                            .map(|warning| format!("history warning: {warning}"));
                        if let Some(issue) = collection_issue.as_ref().or(history_warning.as_ref())
                        {
                            recorder_status.record_degraded(attempt_at, issue);
                        } else {
                            recorder_status.record_success(attempt_at);
                        }
                    }
                    Ok(report) => recorder_status.record_error(
                        attempt_at,
                        report
                            .warnings
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "history store is read-only".to_string()),
                    ),
                    Err(error) => recorder_status
                        .record_error(attempt_at, format!("history persistence failed: {error}")),
                }
            } else {
                recorder_status.record_heartbeat(attempt_at);
            }
            if let Err(error) = write_recorder_status(&status_file, &recorder_status) {
                let mut stderr = io::stderr().lock();
                let _ = writeln!(stderr, "warning: recorder status write failed: {error}");
            }
            config.perf_log.maybe_sample();

            if local_due {
                next_local = advance_deadline(next_local, local_interval, Instant::now());
            }
            if account_due || config.offline {
                next_account = advance_deadline(next_account, account_interval, Instant::now());
            }
        }

        let wake_at = if config.offline {
            next_local
        } else {
            next_local.min(next_account)
        };
        thread::sleep(wake_at.saturating_duration_since(Instant::now()));
    }
}

fn recorder_account_issue(snapshot: &crate::domain::Snapshot) -> Option<String> {
    snapshot
        .sources
        .iter()
        .find(|source| source.source == "app_server" && source.status != "ok")
        .map(|source| {
            source.message.as_ref().map_or_else(
                || format!("account collection is {}", source.status),
                |message| format!("account collection is {}: {message}", source.status),
            )
        })
}

fn advance_deadline(mut deadline: Instant, interval: Duration, now: Instant) -> Instant {
    while deadline <= now {
        deadline += interval;
    }
    deadline
}

fn absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(&path))
            .unwrap_or(path)
    }
}

fn command_name(command: Option<&Command>) -> &'static str {
    match command {
        None => "tui",
        Some(Command::Snapshot(_)) => "snapshot",
        Some(Command::Limits(_)) => "limits",
        Some(Command::Tasks(_)) => "tasks",
        Some(Command::Turns(_)) => "turns",
        Some(Command::Models(_)) => "models",
        Some(Command::Attribution(_)) => "attribution",
        Some(Command::Windows(_)) => "windows",
        Some(Command::Record(_)) => "record",
        Some(Command::Service(_)) => "service",
        Some(Command::DebugStartup(_)) => "debug_startup",
    }
}

fn write_output(writer: &mut impl Write, output: &str) -> io::Result<()> {
    writer.write_all(output.as_bytes())?;
    writer.write_all(b"\n")
}

fn active_grace(minutes: u64) -> Duration {
    Duration::from_secs(minutes.max(1).saturating_mul(60))
}

fn request_for(args: OutputArgs, section: Section) -> OutputRequest {
    OutputRequest {
        format: args.format.into(),
        compact: args.compact,
        sections: BTreeSet::from([section]),
        thread_filter: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn section_all_contains_every_public_section() {
        assert_eq!(Section::all().len(), 7);
    }

    #[test]
    fn clap_errors_map_to_sysexits_usage() {
        let error = Cli::try_parse_from(["codex-usage-monit", "--not-a-real-option"]).unwrap_err();
        assert!(error.use_stderr());
    }

    #[test]
    fn clap_help_is_successful() {
        let error = Cli::try_parse_from(["codex-usage-monit", "--help"]).unwrap_err();
        assert!(!error.use_stderr());
    }

    #[test]
    fn windows_are_available_as_a_command_and_snapshot_section() {
        let command =
            Cli::try_parse_from(["codex-usage-monit", "windows", "--format", "json"]).unwrap();
        assert!(matches!(command.command, Some(Command::Windows(_))));

        let snapshot =
            Cli::try_parse_from(["codex-usage-monit", "snapshot", "--section", "windows"]).unwrap();
        assert!(matches!(
            snapshot.command,
            Some(Command::Snapshot(SnapshotArgs { section, .. }))
                if matches!(section.as_slice(), [SectionArg::Windows])
        ));
    }

    #[test]
    fn record_and_service_commands_parse_cross_platform_paths() {
        let codex_bin = PathBuf::from("tools with spaces/codex.cmd");
        let record = Cli::try_parse_from([
            "codex-usage-monit",
            "--codex-bin",
            codex_bin.to_str().unwrap(),
            "record",
            "--foreground",
            "--history-dir",
            "state with spaces/history",
            "--status-file",
            "state with spaces/recorder-status.json",
        ])
        .unwrap();
        assert_eq!(record.codex_bin, Some(codex_bin));
        assert!(matches!(
            record.command,
            Some(Command::Record(RecordArgs {
                foreground: true,
                history_dir: Some(_),
                status_file: Some(_),
                ..
            }))
        ));

        for action in ["install", "status", "uninstall"] {
            let service = Cli::try_parse_from(["codex-usage-monit", "service", action]).unwrap();
            assert!(matches!(service.command, Some(Command::Service(_))));
        }
    }

    #[test]
    fn debug_startup_accepts_headless_dimensions_and_log_path() {
        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "--startup-log",
            "/tmp/startup.jsonl",
            "debug-startup",
            "--format",
            "json",
            "--width",
            "100",
            "--height",
            "30",
        ])
        .unwrap();

        assert_eq!(cli.startup_log, Some(PathBuf::from("/tmp/startup.jsonl")));
        assert!(matches!(
            cli.command,
            Some(Command::DebugStartup(DebugStartupArgs {
                format: FormatArg::Json,
                width: 100,
                height: 30,
            }))
        ));
    }

    #[test]
    fn runtime_perf_log_is_optional_and_global() {
        let default = Cli::try_parse_from(["codex-usage-monit"]).unwrap();
        assert_eq!(default.perf_log, None);

        let before = Cli::try_parse_from([
            "codex-usage-monit",
            "--perf-log",
            "/tmp/perf.jsonl",
            "snapshot",
        ])
        .unwrap();
        assert_eq!(before.perf_log, Some(PathBuf::from("/tmp/perf.jsonl")));

        let after = Cli::try_parse_from([
            "codex-usage-monit",
            "snapshot",
            "--perf-log",
            "/tmp/perf.jsonl",
        ])
        .unwrap();
        assert_eq!(after.perf_log, Some(PathBuf::from("/tmp/perf.jsonl")));
    }

    #[test]
    fn runtime_perf_log_initialization_errors_are_reported_once() {
        let temp = tempfile::tempdir().unwrap();
        let log = PerfLog::enabled(temp.path());
        assert!(!log.is_enabled());
        let mut guard = PerfLogGuard::new(log, Some(temp.path().to_owned()));
        let mut warnings = Vec::new();

        guard.report_error_to(&mut warnings).unwrap();
        guard.report_error_to(&mut warnings).unwrap();

        let warnings = String::from_utf8(warnings).unwrap();
        assert_eq!(warnings.lines().count(), 1);
        assert!(warnings.contains("--perf-log disabled"));
        assert!(warnings.contains(&format!("{:?}", temp.path())));
    }

    #[test]
    fn debug_startup_rejects_unbounded_headless_dimensions() {
        let error = Cli::try_parse_from(["codex-usage-monit", "debug-startup", "--width", "1001"])
            .unwrap_err();

        assert!(error.use_stderr());
    }

    #[test]
    fn rollout_cache_can_be_disabled_as_a_global_option() {
        let default = Cli::try_parse_from(["codex-usage-monit"]).unwrap();
        assert!(!default.no_rollout_cache);

        let before_subcommand =
            Cli::try_parse_from(["codex-usage-monit", "--no-rollout-cache", "snapshot"]).unwrap();
        assert!(before_subcommand.no_rollout_cache);

        let after_subcommand =
            Cli::try_parse_from(["codex-usage-monit", "snapshot", "--no-rollout-cache"]).unwrap();
        assert!(after_subcommand.no_rollout_cache);
    }

    #[test]
    fn tui_theme_uses_saved_default_and_accepts_explicit_light_aliases() {
        let default = Cli::try_parse_from(["codex-usage-monit"]).unwrap();
        assert_eq!(default.theme, None);

        let light = Cli::try_parse_from(["codex-usage-monit", "--theme", "light"]).unwrap();
        assert_eq!(light.theme, Some(ThemeArg::Light));

        let bright = Cli::try_parse_from(["codex-usage-monit", "--theme", "bright"]).unwrap();
        assert_eq!(bright.theme, Some(ThemeArg::Light));
    }

    #[test]
    fn output_writes_report_broken_pipes_without_panicking() {
        let error = write_output(&mut BrokenPipeWriter, "large output").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn active_grace_conversion_saturates() {
        assert_eq!(active_grace(u64::MAX), Duration::from_secs(u64::MAX));
    }
}
