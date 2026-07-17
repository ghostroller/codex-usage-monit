use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::CollectConfig;
use crate::output::{
    OutputFormat, OutputRequest, Section, render_output, request_is_failure, request_is_partial,
};
use crate::perf::PerfLog;
use crate::snapshot::{collect_limits_snapshot, collect_snapshot};
use crate::startup::StartupTrace;
use crate::tui::Theme;

struct PerfLogGuard(PerfLog);

impl Drop for PerfLogGuard {
    fn drop(&mut self) {
        self.0.finish();
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
    /// Profile the normal TUI cold-start path without entering interactive mode.
    DebugStartup(DebugStartupArgs),
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
    let perf_log = cli
        .perf_log
        .as_deref()
        .map(PerfLog::enabled)
        .unwrap_or_default();
    let _perf_log_guard = PerfLogGuard(perf_log.clone());
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
