use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::attribution::project_five_hour_analysis;
use crate::config::CollectConfig;
use crate::domain::Provenance;
use crate::health_report::HealthReport;
use crate::history::{HistoryData, HistoryObservation, HistoryStore, default_history_root};
use crate::output::{
    OutputFormat, OutputRequest, Section, render_output, request_is_failure, request_is_partial,
};
use crate::perf::{HistoryMetrics, PerfLog};
use crate::report_output::{
    health_report_is_partial, render_health_report, render_summary_report, render_trends_report,
    summary_report_is_partial, trends_report_is_partial,
};
use crate::rollout::RolloutCache;
use crate::service::{
    RecorderStatusFile, ServiceOptions, default_status_file, install as install_service,
    read_recorder_status, status as service_status, uninstall as uninstall_service,
    write_recorder_status,
};
use crate::snapshot::{
    CollectionResult, collect_limits_snapshot, collect_snapshot, collect_snapshot_cached,
    collect_snapshot_cached_if_changed,
};
use crate::startup::StartupTrace;
use crate::summary_report::{
    SummaryCoverageState, SummaryGrain, SummaryMetric, SummaryRange, SummaryReportQuery,
    build_summary_report, history_view_since, retain_summary_backfill_evidence_buckets,
    summary_backfill_config, summary_backfill_scan_complete, summary_history_backfill_needed,
    summary_history_coverage_complete,
};
use crate::trends::{TrendsReport, build_trends_report};
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

    /// Internal PATH override preserved by service registrations.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        hide = true,
        allow_hyphen_values = true
    )]
    service_path: Option<OsString>,

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
    /// Print the same history-backed project/session/turn Summary as the TUI.
    Summary(SummaryArgs),
    /// Print the same quota and local-usage trend series as the TUI.
    Trends(TrendsArgs),
    /// Print unified snapshot, history, recorder, and service health.
    Health(HealthArgs),
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

#[derive(Clone, Debug, Subcommand)]
enum ServiceAction {
    /// Install and start the current user's background recorder.
    Install,
    /// Show registration state and the recorder's latest heartbeat.
    Status(ServiceStatusArgs),
    /// Stop and remove the current user's background recorder.
    Uninstall,
}

#[derive(Clone, Debug, Args)]
struct ServiceStatusArgs {
    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,

    #[arg(long)]
    compact: bool,
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

    /// Apply the optional API long-context multiplier to quota estimates.
    #[arg(long)]
    long_context: bool,
}

#[derive(Clone, Debug, Args)]
struct SummaryArgs {
    #[command(flatten)]
    output: OutputArgs,

    /// Query window: active weekly cycle, trailing 7 days, or trailing 30 days.
    #[arg(long, value_enum, default_value_t = SummaryRangeArg::Cycle)]
    range: SummaryRangeArg,

    /// Local-wall-clock chart bucket size.
    #[arg(long, value_enum, default_value_t = SummaryGrainArg::Day)]
    grain: SummaryGrainArg,

    /// Metric used for values, shares, and ordering.
    #[arg(long, value_enum, default_value_t = SummaryMetricArg::Tokens)]
    metric: SummaryMetricArg,

    /// Override the history directory selected from the platform state directory.
    #[arg(long, value_name = "DIR")]
    history_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
struct TrendsArgs {
    #[command(flatten)]
    output: OutputArgs,

    /// Select the current aligned 24-hour window or one of the previous 7 days.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u16).range(0..=7))]
    day_offset: u16,

    /// Override the history directory selected from the platform state directory.
    #[arg(long, value_name = "DIR")]
    history_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
struct HealthArgs {
    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,

    #[arg(long)]
    compact: bool,

    /// Override the history directory selected from the platform state directory.
    #[arg(long, value_name = "DIR")]
    history_dir: Option<PathBuf>,
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
enum SummaryRangeArg {
    Cycle,
    #[value(name = "7d")]
    SevenDays,
    #[value(name = "30d")]
    ThirtyDays,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum SummaryGrainArg {
    #[value(name = "1d")]
    Day,
    #[value(name = "12h")]
    Hours12,
    #[value(name = "6h")]
    Hours6,
    #[value(name = "3h")]
    Hours3,
    #[value(name = "1h")]
    Hour,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum SummaryMetricArg {
    Tokens,
    Estimated,
    #[value(name = "api-equivalent", alias = "api")]
    ApiEquivalent,
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

impl From<SummaryRangeArg> for SummaryRange {
    fn from(value: SummaryRangeArg) -> Self {
        match value {
            SummaryRangeArg::Cycle => Self::Cycle,
            SummaryRangeArg::SevenDays => Self::SevenDays,
            SummaryRangeArg::ThirtyDays => Self::ThirtyDays,
        }
    }
}

impl From<SummaryGrainArg> for SummaryGrain {
    fn from(value: SummaryGrainArg) -> Self {
        match value {
            SummaryGrainArg::Day => Self::Day,
            SummaryGrainArg::Hours12 => Self::Hours12,
            SummaryGrainArg::Hours6 => Self::Hours6,
            SummaryGrainArg::Hours3 => Self::Hours3,
            SummaryGrainArg::Hour => Self::Hour,
        }
    }
}

impl From<SummaryMetricArg> for SummaryMetric {
    fn from(value: SummaryMetricArg) -> Self {
        match value {
            SummaryMetricArg::Tokens => Self::Tokens,
            SummaryMetricArg::Estimated => Self::Estimated,
            SummaryMetricArg::ApiEquivalent => Self::ApiEquivalent,
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
    validate_output_path_conflicts(&cli)?;
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
    config.app_server_path = cli.service_path;
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
        Command::Summary(args) => {
            let outcome = run_summary(&config, args);
            finish_report_trace(&trace, "summary", &outcome);
            return outcome;
        }
        Command::Trends(args) => {
            let outcome = run_trends(&config, args);
            finish_report_trace(&trace, "trends", &outcome);
            return outcome;
        }
        Command::Health(args) => {
            let outcome = run_health(&config, args, perf_log_path.as_deref());
            finish_report_trace(&trace, "health", &outcome);
            return outcome;
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
            api_long_context: args.output.long_context,
        },
        Command::Limits(args) => request_for(args, Section::Limits),
        Command::Tasks(args) => request_for(args, Section::Tasks),
        Command::Turns(args) => OutputRequest {
            format: args.output.format.into(),
            compact: args.output.compact,
            sections: BTreeSet::from([Section::Turns]),
            thread_filter: args.thread,
            api_long_context: args.output.long_context,
        },
        Command::Models(args) => request_for(args, Section::Models),
        Command::Attribution(args) => request_for(args, Section::Attribution),
        Command::Windows(args) => request_for(args, Section::Windows),
        Command::Record(_)
        | Command::Service(_)
        | Command::Summary(_)
        | Command::Trends(_)
        | Command::Health(_) => {
            unreachable!("specialized commands are handled before snapshot output routing")
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
    if request.api_long_context {
        apply_api_long_context_projection(&mut result.snapshot);
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

fn apply_api_long_context_projection(snapshot: &mut crate::domain::Snapshot) {
    for analysis in &mut snapshot.window_analyses {
        if let Some(long_context) = analysis.api_long_context.take() {
            *analysis = *long_context;
        }
    }
    let (models, attribution) = project_five_hour_analysis(
        &mut snapshot.tasks,
        &mut snapshot.turns,
        &snapshot.window_analyses,
    );
    snapshot.models = models;
    snapshot.attribution = attribution;
}

fn run_summary(config: &CollectConfig, args: SummaryArgs) -> Result<i32> {
    let result = collect_snapshot(config, None, true);
    let mut query_now = Utc::now().max(result.snapshot.as_of);
    let (mut history_store, mut history) =
        collect_and_load_report_history(config, &result, args.history_dir);
    let range: SummaryRange = args.range.into();
    if range == SummaryRange::ThirtyDays && summary_history_backfill_needed(&history, query_now) {
        let (backfilled_history, observed_at) =
            backfill_summary_history(config, &mut history_store);
        history = backfilled_history;
        query_now = query_now.max(observed_at);
    }

    let query = SummaryReportQuery::new(
        range,
        args.grain.into(),
        args.metric.into(),
        args.output.long_context,
        query_now,
    );
    let report = build_summary_report(&result.snapshot, &history, query);
    let output = render_summary_report(&report, args.output.format.into(), args.output.compact)?;
    if !write_stdout_status(&output)? {
        return Ok(0);
    }

    Ok(if report.coverage.state == SummaryCoverageState::Missing {
        1
    } else if summary_report_is_partial(&report) {
        2
    } else {
        0
    })
}

fn run_trends(config: &CollectConfig, args: TrendsArgs) -> Result<i32> {
    let result = collect_snapshot(config, None, true);
    let query_now = Utc::now().max(result.snapshot.as_of);
    let (_history_store, history) =
        collect_and_load_report_history(config, &result, args.history_dir);
    let report = build_trends_report(
        &history,
        query_now,
        args.day_offset,
        args.output.long_context,
    );
    let output = render_trends_report(&report, args.output.format.into(), args.output.compact)?;
    if !write_stdout_status(&output)? {
        return Ok(0);
    }

    Ok(if !trends_report_has_observations(&report) {
        1
    } else if trends_report_is_partial(&report) {
        2
    } else {
        0
    })
}

fn run_health(config: &CollectConfig, args: HealthArgs, perf_log: Option<&Path>) -> Result<i32> {
    let result = collect_snapshot(config, None, true);
    let now = Utc::now().max(result.snapshot.as_of);
    let (history_store, history) =
        collect_and_load_report_history(config, &result, args.history_dir);
    let (recorder_status, recorder_error) = recorder_status_for_history(&history_store);
    let (service, service_error) = service_status_for_history(config, &history_store, perf_log);
    let report = HealthReport::new_with_service_error(
        &result.snapshot,
        &history,
        recorder_status.as_ref(),
        recorder_error.as_deref(),
        service.as_ref(),
        service_error.as_deref(),
        now,
    );
    let output = render_health_report(&report, args.format.into(), args.compact)?;
    if !write_stdout_status(&output)? {
        return Ok(0);
    }
    Ok(if health_report_is_partial(&report) {
        2
    } else {
        0
    })
}

fn collect_and_load_report_history(
    config: &CollectConfig,
    result: &CollectionResult,
    history_dir: Option<PathBuf>,
) -> (HistoryStore, HistoryData) {
    let total_started = Instant::now();
    let mut store = history_dir.map_or_else(
        || HistoryStore::discover_with_redaction(&config.codex_home, config.redact_content),
        |history_dir| {
            HistoryStore::new_with_redaction(
                absolute_path(history_dir),
                &config.codex_home,
                config.redact_content,
            )
        },
    );
    let observation = report_history_observation(result, config.offline);
    let record_started = Instant::now();
    let write_result = store.record(&observation);
    let record_elapsed = record_started.elapsed();
    let load_started = Instant::now();
    let mut history = store.load_since(history_view_since(result.snapshot.as_of));
    let load_elapsed = load_started.elapsed();

    let mut metrics =
        HistoryMetrics::with_durations(total_started.elapsed(), record_elapsed, Some(load_elapsed));
    metrics.record_performed = true;
    match &write_result {
        Ok(report) => {
            metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
            metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
            metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
            metrics.read_only = report.read_only;
        }
        Err(_) => metrics.warnings = 1,
    }
    merge_history_write_result(&mut history, write_result);
    metrics.quota_points = u64::try_from(history.quota_points.len()).unwrap_or(u64::MAX);
    metrics.local_buckets = u64::try_from(history.half_hour_buckets.len()).unwrap_or(u64::MAX);
    metrics.weekly_local_points =
        u64::try_from(history.weekly_local_points.len()).unwrap_or(u64::MAX);
    metrics.warnings = metrics
        .warnings
        .max(u64::try_from(history.warnings.len()).unwrap_or(u64::MAX));
    metrics.read_only |= history.read_only;
    config.perf_log.record_history(metrics);
    normalize_history_warnings(&mut history);
    (store, history)
}

fn report_history_observation(result: &CollectionResult, offline: bool) -> HistoryObservation {
    let mut observation = result.history_observation.clone();
    let account_limits_are_fresh = result
        .account
        .limits
        .iter()
        .any(|limit| limit.provenance == Provenance::ServerSnapshot);
    if !offline && !account_limits_are_fresh {
        observation.quota_points.clear();
        observation.weekly_local_points.clear();
    }
    observation
}

fn merge_history_write_result(
    history: &mut HistoryData,
    write_result: io::Result<crate::history::HistoryWriteReport>,
) {
    match write_result {
        Ok(report) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings);
        }
        Err(error) => history
            .warnings
            .push(format!("history persistence failed: {error}")),
    }
}

fn backfill_summary_history(
    config: &CollectConfig,
    store: &mut HistoryStore,
) -> (HistoryData, DateTime<Utc>) {
    let worker_config = summary_backfill_config(config);
    let result = collect_snapshot(&worker_config, None, false);
    let scan_complete = summary_backfill_scan_complete(&result.snapshot);
    let observed_at = result.snapshot.as_of;
    let mut observation = result.history_observation;
    // Summary reconstruction is local-only. Offline fallback quota samples
    // must never replace server-backed quota history.
    observation.quota_points.clear();
    observation.weekly_local_points.clear();
    retain_summary_backfill_evidence_buckets(&mut observation);
    store.stage_full_observation(&observation);
    let write_result = store.flush_staged();
    let mut history = store.load_since_with_staged(history_view_since(observed_at));
    match write_result {
        Ok(Some(report)) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings);
        }
        Ok(None) => {}
        Err(error) => history
            .warnings
            .push(format!("summary backfill persistence failed: {error}")),
    }
    let requested_complete =
        scan_complete && summary_history_coverage_complete(&history, observed_at);
    let complete = match store.mark_summary_backfill_attempt(observed_at, requested_complete) {
        Ok(complete) => complete,
        Err(error) => {
            history
                .warnings
                .push(format!("summary backfill marker failed: {error}"));
            requested_complete
        }
    };
    history.summary_backfill_attempted_at = Some(observed_at);
    history.summary_backfill_attempt_complete = Some(complete);
    normalize_history_warnings(&mut history);
    (history, observed_at)
}

fn normalize_history_warnings(history: &mut HistoryData) {
    history.warnings.sort();
    history.warnings.dedup();
}

fn recorder_status_for_history(
    store: &HistoryStore,
) -> (Option<RecorderStatusFile>, Option<String>) {
    let Some(history_root) = store.history_root() else {
        return (
            None,
            Some("recorder state directory is unavailable".to_string()),
        );
    };
    let path = default_status_file(history_root);
    match read_recorder_status(&path) {
        Ok(Some(status))
            if status
                .history_namespace
                .as_deref()
                .is_some_and(|namespace| namespace != store.namespace()) =>
        {
            (
                None,
                Some(format!(
                    "recorder targets history namespace {}, expected {}",
                    status.history_namespace.as_deref().unwrap_or("unknown"),
                    store.namespace()
                )),
            )
        }
        Ok(status) => (status, None),
        Err(error) => (None, Some(format!("{}: {error}", path.display()))),
    }
}

fn service_status_for_history(
    config: &CollectConfig,
    store: &HistoryStore,
    perf_log: Option<&Path>,
) -> (Option<crate::service::ServiceStatus>, Option<String>) {
    let Some(history_root) = store.history_root() else {
        return (
            None,
            Some("service state directory is unavailable".to_string()),
        );
    };
    let history_dir = absolute_path(history_root.to_path_buf());
    let status_file = default_status_file(&history_dir);
    let status = build_service_options(config, history_dir, status_file, perf_log)
        .and_then(|options| service_status(&options));
    match status {
        Ok(status) => (Some(status), None),
        Err(error) => (None, Some(format!("service status failed: {error:#}"))),
    }
}

fn trends_report_has_observations(report: &TrendsReport) -> bool {
    report.weekly_history_present
        || report.half_hour_history_present
        || !report.five_hour_remaining.is_empty()
        || !report.weekly_remaining.is_empty()
        || !report.weekly_tokens.is_empty()
        || !report.weekly_estimated.is_empty()
        || !report.half_hour_tokens.is_empty()
        || !report.half_hour_estimated.is_empty()
        || report.five_hour_remaining_readout.is_some()
        || report.weekly_remaining_readout.is_some()
        || report.weekly_tokens_readout.is_some()
        || report.weekly_estimated_readout.is_some()
}

fn finish_report_trace(trace: &StartupTrace, command: &str, outcome: &Result<i32>) {
    match outcome {
        Ok(exit_code) => trace.finish(
            "startup.complete",
            format!("mode=one_shot command={command} exit_code={exit_code}"),
        ),
        Err(_) => trace.finish("startup.failed", format!("mode=one_shot command={command}")),
    }
}

fn write_stdout(output: &str) -> Result<()> {
    let _ = write_stdout_status(output)?;
    Ok(())
}

fn write_stdout_status(output: &str) -> Result<bool> {
    let mut stdout = io::stdout().lock();
    match write_output(&mut stdout, output) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn run_service(config: &CollectConfig, args: ServiceArgs, perf_log: Option<&Path>) -> Result<i32> {
    let history_dir = default_history_root()
        .map(absolute_path)
        .ok_or_else(|| anyhow::anyhow!("a user state directory is unavailable"))?;
    let status_file = default_status_file(&history_dir);
    let mut options = build_service_options(config, history_dir, status_file, perf_log)?;
    if matches!(&args.action, ServiceAction::Install) && !config.offline {
        options.codex_bin = Some(resolve_service_codex(config)?);
    }
    if matches!(&args.action, ServiceAction::Install) && perf_log.is_some() {
        config.perf_log.finish();
    }
    let (status, output_format) = match args.action {
        ServiceAction::Install => (install_service(&options)?, None),
        ServiceAction::Status(args) => (service_status(&options)?, Some(args)),
        ServiceAction::Uninstall => (uninstall_service(&options)?, None),
    };
    let output = match output_format {
        Some(args) if matches!(args.format, FormatArg::Json) => {
            if args.compact {
                serde_json::to_string(&status)?
            } else {
                serde_json::to_string_pretty(&status)?
            }
        }
        _ => render_service_status_text(&status),
    };
    write_stdout(&output)?;
    Ok(0)
}

fn build_service_options(
    config: &CollectConfig,
    history_dir: PathBuf,
    status_file: PathBuf,
    perf_log: Option<&Path>,
) -> Result<ServiceOptions> {
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
    Ok(options)
}

fn render_service_status_text(status: &crate::service::ServiceStatus) -> String {
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
        "heartbeat recent: {}",
        if status.heartbeat_recent { "yes" } else { "no" }
    ));
    lines.push(format!(
        "detail: {}",
        crate::domain::terminal_safe_text(&status.detail)
    ));
    lines.join("\n")
}

fn resolve_service_codex(config: &CollectConfig) -> Result<PathBuf> {
    let current_dir = std::env::current_dir().map_err(anyhow::Error::from)?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    #[cfg(windows)]
    {
        resolve_windows_service_codex(
            config,
            &path,
            &current_dir,
            crate::app_server::installed_windows_codex_cli(),
        )
    }
    #[cfg(not(windows))]
    {
        crate::session_launch::resolve_executable(
            "codex",
            config.codex_bin.as_deref(),
            &path,
            &current_dir,
        )
        .map_err(anyhow::Error::new)
    }
}

#[cfg(windows)]
fn resolve_windows_service_codex(
    config: &CollectConfig,
    path: &std::ffi::OsStr,
    current_dir: &Path,
    installed: Option<PathBuf>,
) -> Result<PathBuf> {
    match config.codex_bin.as_deref() {
        Some(codex_bin) => {
            crate::session_launch::resolve_executable("codex", Some(codex_bin), path, current_dir)
                .map_err(anyhow::Error::new)
        }
        None => crate::app_server::resolve_automatic_windows_codex_cli_with_installed(
            path,
            current_dir,
            installed,
        ),
    }
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
    let mut history_store =
        HistoryStore::new_with_redaction(history_dir, &config.codex_home, config.redact_content);
    let mut rollout_cache = RolloutCache::new();
    let mut cached_account = None;
    let local_interval = Duration::from_secs(args.local_interval_seconds);
    let account_interval = Duration::from_secs(args.account_interval_seconds);
    let mut next_local = Instant::now();
    let mut next_account = Instant::now();
    let mut account_issue = None;
    let heartbeat_interval_seconds = if config.offline {
        args.local_interval_seconds
    } else {
        args.local_interval_seconds
            .min(args.account_interval_seconds)
    };
    let mut recorder_status = RecorderStatusFile::started_with_interval(
        Utc::now(),
        history_store.namespace().to_string(),
        heartbeat_interval_seconds,
    );
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
                history_metrics.record_performed = true;
                history_metrics.quota_points =
                    u64::try_from(result.history_observation.quota_points.len())
                        .unwrap_or(u64::MAX);
                history_metrics.local_buckets =
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

fn validate_output_path_conflicts(cli: &Cli) -> Result<()> {
    validate_output_path_conflicts_with_current_dir(cli, std::env::current_dir)
}

fn validate_output_path_conflicts_with_current_dir(
    cli: &Cli,
    current_dir: impl FnOnce() -> io::Result<PathBuf>,
) -> Result<()> {
    let paths = output_paths_for_command(cli);
    if paths.len() < 2 {
        return Ok(());
    }
    let current_dir = current_dir().map_err(anyhow::Error::from)?;
    validate_output_paths_from(paths, &current_dir)
}

#[cfg(test)]
fn validate_output_path_conflicts_from(cli: &Cli, current_dir: &Path) -> Result<()> {
    validate_output_paths_from(output_paths_for_command(cli), current_dir)
}

fn output_paths_for_command(cli: &Cli) -> Vec<(&'static str, PathBuf)> {
    let mut paths = Vec::new();
    if let Some(path) = cli.startup_log.as_deref() {
        paths.push(("--startup-log", path.to_path_buf()));
    }
    if let Some(path) = cli.perf_log.as_deref() {
        paths.push(("--perf-log", path.to_path_buf()));
    }
    if let Some(path) = status_file_for_command(cli) {
        let label = match cli.command.as_ref() {
            Some(Command::Record(_)) => "--status-file",
            _ => "recorder status file",
        };
        paths.push((label, path));
    }
    paths
}

fn validate_output_paths_from(
    paths: Vec<(&'static str, PathBuf)>,
    current_dir: &Path,
) -> Result<()> {
    let paths = paths
        .into_iter()
        .map(|(name, path)| (name, normalize_output_path(&path, current_dir)))
        .collect::<Vec<_>>();

    for (index, (left_name, left_path)) in paths.iter().enumerate() {
        for (right_name, right_path) in paths.iter().skip(index + 1) {
            if output_paths_alias(left_path, right_path) {
                bail!(
                    "output path conflict: {left_name} and {right_name} both refer to {}",
                    left_path.display()
                );
            }
        }
    }
    Ok(())
}

fn status_file_for_command(cli: &Cli) -> Option<PathBuf> {
    match cli.command.as_ref() {
        Some(Command::Record(args)) => args.status_file.clone().or_else(|| {
            args.history_dir
                .clone()
                .or_else(default_history_root)
                .map(|path| default_status_file(&path))
        }),
        Some(Command::Health(args)) => args
            .history_dir
            .clone()
            .or_else(default_history_root)
            .map(|path| default_status_file(&path)),
        Some(Command::Service(_)) => default_history_root().map(|path| default_status_file(&path)),
        _ => None,
    }
}

fn normalize_output_path(path: &Path, current_dir: &Path) -> PathBuf {
    let absolute = lexical_normalize(&absolute_path_from(path.to_path_buf(), current_dir));
    canonicalize_existing_ancestor(&absolute).unwrap_or(absolute)
}

fn absolute_path_from(path: PathBuf, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        current_dir.join(path)
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::ParentDir) | None => normalized.push(component.as_os_str()),
                Some(Component::CurDir) => unreachable!("current-directory components are skipped"),
            },
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn canonicalize_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(ancestor) {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return Some(lexical_normalize(&canonical));
        }
        let file_name = ancestor.file_name()?.to_owned();
        suffix.push(file_name);
        ancestor = ancestor.parent()?;
    }
}

fn output_paths_alias(left: &Path, right: &Path) -> bool {
    if paths_equal_for_platform(left, right) {
        return true;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) {
            return left.dev() == right.dev() && left.ino() == right.ino();
        }
    }
    #[cfg(windows)]
    if let (Ok(left), Ok(right)) = (windows_file_identity(left), windows_file_identity(right)) {
        return left == right;
    }
    false
}

#[cfg(windows)]
fn windows_file_identity(path: &Path) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
    };

    let file = fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut information = FILE_ID_INFO::default();
    // SAFETY: `file` owns a valid handle for the duration of the call and
    // `information` points to writable storage of the required type.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((
        information.VolumeSerialNumber,
        information.FileId.Identifier,
    ))
}

fn paths_equal_for_platform(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        if left == right {
            return true;
        }
        // A case-sensitive ancestor can contain distinct existing directories
        // whose names differ only by case even when those child directories
        // themselves use normal case-insensitive lookup. Their stable file IDs
        // distinguish that situation before the remaining suffix is folded.
        if let (Some(left_ancestor), Some(right_ancestor)) = (
            windows_nearest_existing_identity(left),
            windows_nearest_existing_identity(right),
        ) && left_ancestor != right_ancestor
        {
            return false;
        }
        // Windows normally compares paths case-insensitively, but NTFS can
        // opt individual directories into case-sensitive lookup. Query the
        // closest existing parent so two not-yet-created output names follow
        // the directory that will contain them.
        windows_path_is_case_sensitive(left) != Some(true)
            && windows_path_is_case_sensitive(right) != Some(true)
            && windows_case_insensitive_paths_equal(left, right)
    }
    #[cfg(target_os = "macos")]
    {
        if left == right {
            return true;
        }
        // Case-folded mount-point names on different volumes are not aliases,
        // even when both volumes themselves use case-insensitive lookup.
        if let (Some(left_volume), Some(right_volume)) =
            (macos_path_volume_id(left), macos_path_volume_id(right))
            && left_volume != right_volume
        {
            return false;
        }
        // Most macOS volumes are case-insensitive, but case-sensitive APFS is
        // supported. Query the existing ancestor so nonexistent final output
        // names follow the actual volume instead of a compile-time guess.
        macos_path_is_case_sensitive(left) != Some(true)
            && macos_path_is_case_sensitive(right) != Some(true)
            && macos_case_insensitive_paths_equal(left, right)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        left == right
    }
}

#[cfg(windows)]
fn windows_nearest_existing_identity(path: &Path) -> Option<(u64, [u8; 16])> {
    let mut ancestor = path;
    loop {
        match windows_file_identity(ancestor) {
            Ok(identity) => return Some(identity),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}

#[cfg(windows)]
fn windows_path_is_case_sensitive(path: &Path) -> Option<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileCaseSensitiveInfo, GetFileInformationByHandleEx,
    };

    const FILE_CS_FLAG_CASE_SENSITIVE_DIR: u32 = 1;

    let mut directory = path;
    while !directory.is_dir() {
        directory = directory.parent()?;
    }
    let directory = fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)
        .ok()?;
    let mut information = FILE_CASE_SENSITIVE_INFO::default();
    // SAFETY: `directory` owns a valid directory handle for the duration of
    // the call and `information` is writable storage of the required type.
    if unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle(),
            FileCaseSensitiveInfo,
            (&mut information as *mut FILE_CASE_SENSITIVE_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>()).unwrap_or(u32::MAX),
        )
    } == 0
    {
        return None;
    }
    Some(information.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0)
}

#[cfg(windows)]
fn windows_case_insensitive_paths_equal(left: &Path, right: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

    let left = left.as_os_str().encode_wide().collect::<Vec<_>>();
    let right = right.as_os_str().encode_wide().collect::<Vec<_>>();
    let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
    else {
        return false;
    };
    // SAFETY: both UTF-16 buffers remain live for their supplied lengths.
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == CSTR_EQUAL
    }
}

#[cfg(target_os = "macos")]
fn macos_path_volume_id(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor.parent()?;
    }
    fs::metadata(ancestor).ok().map(|metadata| metadata.dev())
}

#[cfg(target_os = "macos")]
fn macos_path_is_case_sensitive(path: &Path) -> Option<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor.parent()?;
    }
    let path = CString::new(ancestor.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid, NUL-terminated filesystem path and remains
    // alive for the duration of the call.
    match unsafe { libc::pathconf(path.as_ptr(), libc::_PC_CASE_SENSITIVE) } {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn macos_case_insensitive_paths_equal(left: &Path, right: &Path) -> bool {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;

    type CfStringRef = *const c_void;
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const CF_COMPARE_CASE_INSENSITIVE: usize = 1;
    const CF_COMPARE_NONLITERAL: usize = 16;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            allocator: *const c_void,
            bytes: *const u8,
            byte_count: isize,
            encoding: u32,
            external_representation: u8,
        ) -> CfStringRef;
        fn CFStringCompare(left: CfStringRef, right: CfStringRef, options: usize) -> isize;
        fn CFRelease(value: *const c_void);
    }

    fn create_cf_string(value: &str) -> Option<CfStringRef> {
        let byte_count = isize::try_from(value.len()).ok()?;
        // SAFETY: the byte slice is valid for `byte_count`, UTF-8 was already
        // validated by `str`, and the returned object is released below.
        let value = unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                value.as_ptr(),
                byte_count,
                CF_STRING_ENCODING_UTF8,
                0,
            )
        };
        (!value.is_null()).then_some(value)
    }

    let (Some(left), Some(right)) = (left.to_str(), right.to_str()) else {
        return left
            .as_os_str()
            .as_bytes()
            .eq_ignore_ascii_case(right.as_os_str().as_bytes());
    };
    let Some(left) = create_cf_string(left) else {
        return false;
    };
    let Some(right) = create_cf_string(right) else {
        // SAFETY: `left` was created successfully above and is still owned.
        unsafe { CFRelease(left) };
        return false;
    };
    // SAFETY: both references are live Core Foundation string objects.
    let equal = unsafe {
        CFStringCompare(
            left,
            right,
            CF_COMPARE_CASE_INSENSITIVE | CF_COMPARE_NONLITERAL,
        ) == 0
    };
    // SAFETY: each object is released exactly once after the comparison.
    unsafe {
        CFRelease(left);
        CFRelease(right);
    }
    equal
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
        Some(Command::Summary(_)) => "summary",
        Some(Command::Trends(_)) => "trends",
        Some(Command::Health(_)) => "health",
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
        api_long_context: args.long_context,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_cost::API_PRICING_CATALOG_REVISION;
    use crate::domain::{
        AccountSnapshot, ApiPricingMetadata, AttributionSummary, CollectionStats, Confidence,
        LimitBucket, ModelUsage, Snapshot, TokenUsage, WindowAnalysis, WindowDescriptor,
    };
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, LocalHalfHourBucket,
        QuotaPoint, WeeklyLocalPoint,
    };

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
    fn summary_parses_every_range_grain_metric_and_report_option() {
        for (value, expected) in [
            ("cycle", SummaryRangeArg::Cycle),
            ("7d", SummaryRangeArg::SevenDays),
            ("30d", SummaryRangeArg::ThirtyDays),
        ] {
            let cli =
                Cli::try_parse_from(["codex-usage-monit", "summary", "--range", value]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Summary(SummaryArgs { range, .. })) if range == expected
            ));
        }

        for (value, expected) in [
            ("1d", SummaryGrainArg::Day),
            ("12h", SummaryGrainArg::Hours12),
            ("6h", SummaryGrainArg::Hours6),
            ("3h", SummaryGrainArg::Hours3),
            ("1h", SummaryGrainArg::Hour),
        ] {
            let cli =
                Cli::try_parse_from(["codex-usage-monit", "summary", "--grain", value]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Summary(SummaryArgs { grain, .. })) if grain == expected
            ));
        }

        for (value, expected) in [
            ("tokens", SummaryMetricArg::Tokens),
            ("estimated", SummaryMetricArg::Estimated),
            ("api-equivalent", SummaryMetricArg::ApiEquivalent),
            ("api", SummaryMetricArg::ApiEquivalent),
        ] {
            let cli =
                Cli::try_parse_from(["codex-usage-monit", "summary", "--metric", value]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Summary(SummaryArgs { metric, .. })) if metric == expected
            ));
        }

        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "summary",
            "--range",
            "30d",
            "--grain",
            "1h",
            "--metric",
            "estimated",
            "--long-context",
            "--format",
            "json",
            "--compact",
            "--history-dir",
            "state with spaces/history",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Summary(SummaryArgs {
                output: OutputArgs {
                    format: FormatArg::Json,
                    compact: true,
                    long_context: true,
                },
                range: SummaryRangeArg::ThirtyDays,
                grain: SummaryGrainArg::Hour,
                metric: SummaryMetricArg::Estimated,
                history_dir: Some(path),
            })) if path == Path::new("state with spaces/history")
        ));
    }

    #[test]
    fn trends_parses_every_day_offset_and_report_option_and_rejects_out_of_range() {
        for day_offset in 0_u16..=7 {
            let value = day_offset.to_string();
            let cli = Cli::try_parse_from([
                "codex-usage-monit",
                "trends",
                "--day-offset",
                value.as_str(),
            ])
            .unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Trends(TrendsArgs {
                    day_offset: parsed,
                    ..
                })) if parsed == day_offset
            ));
        }

        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "trends",
            "--day-offset",
            "7",
            "--long-context",
            "--format",
            "json",
            "--compact",
            "--history-dir",
            "state with spaces/history",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Trends(TrendsArgs {
                output: OutputArgs {
                    format: FormatArg::Json,
                    compact: true,
                    long_context: true,
                },
                day_offset: 7,
                history_dir: Some(path),
            })) if path == Path::new("state with spaces/history")
        ));

        for invalid in ["--day-offset=-1", "--day-offset=8", "--day-offset=65536"] {
            let error = Cli::try_parse_from(["codex-usage-monit", "trends", invalid]).unwrap_err();
            assert!(error.use_stderr(), "{invalid} should be a usage error");
        }
    }

    #[test]
    fn health_parses_json_compact_and_history_directory() {
        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "health",
            "--format",
            "json",
            "--compact",
            "--history-dir",
            "state with spaces/history",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Health(HealthArgs {
                format: FormatArg::Json,
                compact: true,
                history_dir: Some(path),
            })) if path == Path::new("state with spaces/history")
        ));
    }

    #[test]
    fn legacy_one_shot_commands_all_accept_long_context() {
        for command in [
            "snapshot",
            "limits",
            "tasks",
            "turns",
            "models",
            "attribution",
            "windows",
        ] {
            let cli =
                Cli::try_parse_from(["codex-usage-monit", command, "--long-context"]).unwrap();
            let long_context = match cli.command.unwrap() {
                Command::Snapshot(args) => args.output.long_context,
                Command::Limits(args)
                | Command::Tasks(args)
                | Command::Models(args)
                | Command::Attribution(args)
                | Command::Windows(args) => args.long_context,
                Command::Turns(args) => args.output.long_context,
                parsed => panic!("unexpected command for {command}: {parsed:?}"),
            };
            assert!(long_context, "{command} dropped --long-context");
        }
    }

    #[test]
    fn record_and_service_commands_parse_cross_platform_paths() {
        let codex_bin = PathBuf::from("tools with spaces/codex.cmd");
        let service_path = OsString::from(r"C:\Portable Node & Tools;C:\Windows\System32");
        let record = Cli::try_parse_from([
            "codex-usage-monit",
            "--codex-bin",
            codex_bin.to_str().unwrap(),
            "--service-path",
            service_path.to_str().unwrap(),
            "record",
            "--foreground",
            "--history-dir",
            "state with spaces/history",
            "--status-file",
            "state with spaces/recorder-status.json",
        ])
        .unwrap();
        assert_eq!(record.codex_bin, Some(codex_bin));
        assert_eq!(record.service_path, Some(service_path));
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
    fn service_status_owns_json_options_without_leaking_them_to_mutating_actions() {
        let status = Cli::try_parse_from([
            "codex-usage-monit",
            "service",
            "status",
            "--format",
            "json",
            "--compact",
        ])
        .unwrap();
        assert!(matches!(
            status.command,
            Some(Command::Service(ServiceArgs {
                action: ServiceAction::Status(ServiceStatusArgs {
                    format: FormatArg::Json,
                    compact: true,
                }),
            }))
        ));

        for action in ["install", "uninstall"] {
            let error =
                Cli::try_parse_from(["codex-usage-monit", "service", action, "--format", "json"])
                    .unwrap_err();
            assert!(error.use_stderr(), "service {action} accepted --format");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_service_uses_shared_auto_discovery_but_honors_explicit_bin() {
        let temp = tempfile::tempdir().unwrap();
        let resources = temp
            .path()
            .join("WindowsApps")
            .join("OpenAI.Codex_26.818.0.0_x64__test")
            .join("app")
            .join("resources");
        let npm_bin = temp.path().join("npm-bin");
        let installed_bin = temp.path().join("installed-bin");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(&npm_bin).unwrap();
        std::fs::create_dir_all(&installed_bin).unwrap();
        let desktop = resources.join("codex.exe");
        let npm = npm_bin.join("codex.cmd");
        let installed = installed_bin.join("codex.exe");
        std::fs::write(&desktop, b"").unwrap();
        std::fs::write(&npm, b"@echo off\r\nexit /b 0\r\n").unwrap();
        std::fs::write(&installed, b"").unwrap();
        let path = std::env::join_paths([resources, npm_bin]).unwrap();
        let mut config = CollectConfig::default();

        let automatic =
            resolve_windows_service_codex(&config, &path, temp.path(), Some(installed.clone()))
                .unwrap();
        assert_eq!(automatic, installed);

        config.codex_bin = Some(desktop.clone());
        let explicit =
            resolve_windows_service_codex(&config, &path, temp.path(), Some(installed)).unwrap();
        assert_eq!(explicit, std::fs::canonicalize(desktop).unwrap());
    }

    #[test]
    fn service_path_accepts_a_leading_hyphen() {
        let service_path = OsString::from(r"-C:\Portable Node & Tools;C:\Windows\System32");
        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "--service-path",
            service_path.to_str().unwrap(),
            "record",
            "--foreground",
        ])
        .unwrap();

        assert_eq!(cli.service_path, Some(service_path));
        assert!(matches!(
            cli.command,
            Some(Command::Record(RecordArgs {
                foreground: true,
                ..
            }))
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
    fn output_logs_reject_relative_and_absolute_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let current_dir = temp.path().join("working");
        std::fs::create_dir(&current_dir).unwrap();
        let absolute = current_dir.join("logs/events.jsonl");
        let cli = Cli::try_parse_from([
            OsString::from("codex-usage-monit"),
            OsString::from("--startup-log"),
            absolute.into_os_string(),
            OsString::from("--perf-log"),
            OsString::from("./logs/../logs/events.jsonl"),
            OsString::from("snapshot"),
        ])
        .unwrap();

        let error = validate_output_path_conflicts_from(&cli, &current_dir).unwrap_err();

        assert!(error.to_string().contains("--startup-log and --perf-log"));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn output_logs_reject_case_only_aliases_before_files_exist() {
        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "--startup-log",
            "logs/Events.JSONL",
            "--perf-log",
            "logs/events.jsonl",
            "snapshot",
        ])
        .unwrap();

        let current_dir = Path::new("/tmp/work");
        let result = validate_output_path_conflicts_from(&cli, current_dir);
        #[cfg(windows)]
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("--startup-log and --perf-log")
        );
        #[cfg(target_os = "macos")]
        if macos_path_is_case_sensitive(current_dir) == Some(true) {
            assert!(result.is_ok());
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("--startup-log and --perf-log")
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_path_comparison_uses_unicode_case_folding_and_normalization() {
        assert!(macos_case_insensitive_paths_equal(
            Path::new("/tmp/recorder-\u{017f}tatus.json"),
            Path::new("/tmp/recorder-status.json")
        ));
        assert!(macos_case_insensitive_paths_equal(
            Path::new("/tmp/e\u{301}vents.jsonl"),
            Path::new("/tmp/évents.jsonl")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_comparison_uses_unicode_ordinal_case_folding() {
        assert!(windows_case_insensitive_paths_equal(
            Path::new(r"C:\logs\Évents.jsonl"),
            Path::new(r"c:\LOGS\évents.JSONL")
        ));
    }

    #[test]
    fn fewer_than_two_output_paths_do_not_require_current_directory() {
        let no_outputs = Cli::try_parse_from(["codex-usage-monit", "snapshot"]).unwrap();
        let one_output = Cli::try_parse_from([
            "codex-usage-monit",
            "--startup-log",
            "events.jsonl",
            "snapshot",
        ])
        .unwrap();

        for cli in [&no_outputs, &one_output] {
            validate_output_path_conflicts_with_current_dir(cli, || -> io::Result<PathBuf> {
                panic!("current directory must not be read for fewer than two output paths")
            })
            .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn output_logs_reject_aliases_with_parent_components_above_root() {
        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "--startup-log",
            "/tmp/events.jsonl",
            "--perf-log",
            "/../tmp/events.jsonl",
            "snapshot",
        ])
        .unwrap();

        let error = validate_output_path_conflicts_from(&cli, Path::new("/")).unwrap_err();

        assert!(error.to_string().contains("--startup-log and --perf-log"));
    }

    #[cfg(windows)]
    #[test]
    fn lexical_normalize_keeps_windows_prefix_and_root() {
        assert_eq!(
            lexical_normalize(Path::new(r"C:\..\logs\events.jsonl")),
            PathBuf::from(r"C:\logs\events.jsonl")
        );
    }

    #[test]
    fn conflicting_output_paths_are_rejected_before_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        std::fs::write(&path, b"keep this file\n").unwrap();
        let cli = Cli::try_parse_from([
            OsString::from("codex-usage-monit"),
            OsString::from("--startup-log"),
            path.as_os_str().to_owned(),
            OsString::from("--perf-log"),
            path.as_os_str().to_owned(),
            OsString::from("snapshot"),
        ])
        .unwrap();
        let started = Instant::now();

        let error = run_with(cli, started, started).unwrap_err();

        assert!(error.to_string().contains("output path conflict"));
        assert_eq!(std::fs::read(&path).unwrap(), b"keep this file\n");
    }

    #[test]
    fn recorder_status_rejects_aliases_with_either_log() {
        let temp = tempfile::tempdir().unwrap();
        let current_dir = temp.path();
        for log_flag in ["--startup-log", "--perf-log"] {
            let status_file = current_dir.join("state/recorder.json");
            let cli = Cli::try_parse_from([
                OsString::from("codex-usage-monit"),
                OsString::from(log_flag),
                status_file.into_os_string(),
                OsString::from("record"),
                OsString::from("--status-file"),
                OsString::from("state/./recorder.json"),
            ])
            .unwrap();

            let error = validate_output_path_conflicts_from(&cli, current_dir).unwrap_err();

            assert!(error.to_string().contains(log_flag));
            assert!(error.to_string().contains("--status-file"));
        }
    }

    #[test]
    fn health_recorder_status_rejects_aliases_with_either_log() {
        let temp = tempfile::tempdir().unwrap();
        let current_dir = temp.path();
        let history_dir = current_dir.join("state/history");
        let status_file = default_status_file(&history_dir);
        for log_flag in ["--startup-log", "--perf-log"] {
            let cli = Cli::try_parse_from([
                OsString::from("codex-usage-monit"),
                OsString::from(log_flag),
                status_file.as_os_str().to_owned(),
                OsString::from("health"),
                OsString::from("--history-dir"),
                history_dir.as_os_str().to_owned(),
            ])
            .unwrap();

            let error = validate_output_path_conflicts_from(&cli, current_dir).unwrap_err();
            assert!(error.to_string().contains(log_flag));
            assert!(error.to_string().contains("recorder status file"));
        }
    }

    #[test]
    fn expired_quota_series_counts_as_a_trends_observation() {
        use crate::trends::{TrendPoint, TrendReadoutValue};

        let now = Utc::now();
        let point = TrendPoint {
            at: now - chrono::Duration::hours(1),
            value: 50.0,
            readout_value: TrendReadoutValue::Percent(50.0),
            sampled_at: Some(now - chrono::Duration::hours(1)),
            interval: None,
            partial: false,
        };
        let report = TrendsReport {
            schema_version: crate::trends::TRENDS_REPORT_SCHEMA_VERSION,
            as_of: now,
            day_offset: 0,
            five_hour_remaining: vec![point],
            weekly_remaining: Vec::new(),
            weekly_tokens: Vec::new(),
            weekly_estimated: Vec::new(),
            half_hour_tokens: Vec::new(),
            half_hour_estimated: Vec::new(),
            five_hour_remaining_readout: None,
            weekly_remaining_readout: None,
            weekly_tokens_readout: None,
            weekly_estimated_readout: None,
            half_hour_bounds: [now - chrono::Duration::days(1), now],
            weekly_history_present: false,
            half_hour_history_present: false,
            history_warning_count: 0,
            history_warnings: Vec::new(),
            history_read_only: false,
            api_long_context_multiplier: false,
        };

        assert!(trends_report_has_observations(&report));
    }

    #[cfg(unix)]
    #[test]
    fn output_logs_reject_symlinked_parent_and_hard_link_aliases() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();
        let cli = Cli::try_parse_from([
            OsString::from("codex-usage-monit"),
            OsString::from("--startup-log"),
            real.join("events.jsonl").into_os_string(),
            OsString::from("--perf-log"),
            alias.join("events.jsonl").into_os_string(),
            OsString::from("snapshot"),
        ])
        .unwrap();
        assert!(validate_output_path_conflicts_from(&cli, temp.path()).is_err());

        let first = real.join("first.jsonl");
        let second = real.join("second.jsonl");
        std::fs::write(&first, b"existing").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        let cli = Cli::try_parse_from([
            OsString::from("codex-usage-monit"),
            OsString::from("--startup-log"),
            first.into_os_string(),
            OsString::from("--perf-log"),
            second.into_os_string(),
            OsString::from("snapshot"),
        ])
        .unwrap();
        assert!(validate_output_path_conflicts_from(&cli, temp.path()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn output_logs_reject_hard_link_aliases_on_windows() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original.jsonl");
        let alias = temp.path().join("alias.jsonl");
        fs::write(&original, b"existing\n").unwrap();
        fs::hard_link(&original, &alias).unwrap();
        let cli = Cli::try_parse_from([
            OsString::from("codex-usage-monit"),
            OsString::from("--startup-log"),
            original.into_os_string(),
            OsString::from("--perf-log"),
            alias.into_os_string(),
            OsString::from("snapshot"),
        ])
        .unwrap();

        let error = validate_output_path_conflicts_from(&cli, temp.path()).unwrap_err();
        assert!(error.to_string().contains("--startup-log and --perf-log"));
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

    fn report_history_collection_result(account_provenance: Provenance) -> CollectionResult {
        let now = DateTime::parse_from_rfc3339("2026-08-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let account_limit = LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: Some("Codex".to_string()),
            plan_type: Some("test".to_string()),
            primary: None,
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            provenance: account_provenance,
            as_of: now,
        };
        let history_observation = HistoryObservation {
            observed_at: now,
            quota_points: vec![QuotaPoint {
                observed_at: now,
                limit_id: "codex".to_string(),
                duration_mins: 300,
                resets_at: now + chrono::Duration::hours(5),
                used_percent: 25.0,
                remaining_percent: 75.0,
                provenance: account_provenance,
            }],
            half_hour_buckets: vec![LocalHalfHourBucket {
                starts_at: now - chrono::Duration::minutes(15),
                ends_at: now,
                sampled_at: now,
                token_usage: TokenUsage {
                    total_tokens: 42,
                    ..TokenUsage::default()
                },
                estimated_cost_units: 7,
                api_long_context_extra_cost_units: Some(3),
                long_context_usage_unknown: false,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
                api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
                call_count: 1,
                groups: Vec::new(),
                project_groups: Vec::new(),
                partial_reasons: Vec::new(),
            }],
            weekly_local_points: vec![WeeklyLocalPoint {
                observed_at: now,
                resets_at: now + chrono::Duration::days(7),
                token_usage: TokenUsage {
                    total_tokens: 42,
                    ..TokenUsage::default()
                },
                estimated_cost_units: 7,
                api_long_context_extra_cost_units: Some(3),
                long_context_usage_unknown: false,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                call_count: 1,
                partial_reasons: Vec::new(),
            }],
        };

        CollectionResult {
            snapshot: Snapshot {
                schema_version: 2,
                api_pricing: ApiPricingMetadata::default(),
                api_equivalent_cost: None,
                as_of: now,
                partial: false,
                codex_home: PathBuf::from("/tmp/.codex"),
                sources: Vec::new(),
                limits: vec![account_limit.clone()],
                rate_limit_reset_credits: None,
                rate_limit_reset_credits_partial: false,
                account_usage: None,
                tasks: Vec::new(),
                turns: Vec::new(),
                models: Vec::new(),
                attribution: AttributionSummary::default(),
                window_analyses: Vec::new(),
                stats: CollectionStats::default(),
                warnings: Vec::new(),
                errors: Vec::new(),
            },
            account: AccountSnapshot {
                limits: vec![account_limit],
                ..AccountSnapshot::default()
            },
            history_observation,
        }
    }

    #[test]
    fn report_history_observation_persists_fresh_account_samples_online() {
        let result = report_history_collection_result(Provenance::ServerSnapshot);

        assert_eq!(
            report_history_observation(&result, false),
            result.history_observation
        );
    }

    #[test]
    fn report_history_observation_drops_stale_quota_online_but_keeps_offline_fallback() {
        let result = report_history_collection_result(Provenance::Stale);
        let expected = result.history_observation.clone();

        let online = report_history_observation(&result, false);
        assert_eq!(online.observed_at, expected.observed_at);
        assert!(online.quota_points.is_empty());
        assert!(online.weekly_local_points.is_empty());
        assert_eq!(online.half_hour_buckets, expected.half_hour_buckets);
        assert_eq!(result.history_observation, expected);

        assert_eq!(report_history_observation(&result, true), expected);
    }

    #[test]
    fn long_context_projection_selects_the_alternative_window_and_legacy_fields() {
        let mut snapshot = report_history_collection_result(Provenance::ServerSnapshot).snapshot;
        let now = snapshot.as_of;
        let analysis = |estimated_quota_percent| WindowAnalysis {
            duration_mins: 300,
            attribution: AttributionSummary {
                window: Some(WindowDescriptor {
                    limit_id: "codex".to_string(),
                    label: "5h".to_string(),
                    starts_at: now - chrono::Duration::hours(1),
                    ends_at: now + chrono::Duration::hours(4),
                    used_percent: 25.0,
                }),
                proxy_projected_percent: estimated_quota_percent,
                ..AttributionSummary::default()
            },
            partial: false,
            partial_reasons: Vec::new(),
            threads: Vec::new(),
            turns: Vec::new(),
            models: vec![ModelUsage {
                model: "gpt-test".to_string(),
                token_usage: TokenUsage {
                    total_tokens: 42,
                    ..TokenUsage::default()
                },
                local_token_share_percent: 100.0,
                estimated_quota_percent,
                quota_confidence: Confidence::Low,
                api_equivalent_cost: Default::default(),
            }],
            api_equivalent_cost: Default::default(),
            api_pricing: Default::default(),
            api_long_context: None,
        };
        let mut base = analysis(1.25);
        base.api_long_context = Some(Box::new(analysis(2.5)));
        snapshot.window_analyses = vec![base];

        apply_api_long_context_projection(&mut snapshot);

        assert_eq!(
            snapshot.window_analyses[0]
                .attribution
                .proxy_projected_percent,
            2.5
        );
        assert!(snapshot.window_analyses[0].api_long_context.is_none());
        assert_eq!(snapshot.models[0].estimated_quota_percent, 2.5);
        assert_eq!(snapshot.attribution.proxy_projected_percent, 2.5);
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
