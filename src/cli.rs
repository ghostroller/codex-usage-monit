use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::attribution::project_five_hour_analysis;
use crate::automatic_remote_sync::{
    AutomaticRemoteCutoverPolicy, AutomaticRemoteSyncStopToken, AutomaticRemoteSyncWorker,
    AutomaticRemoteSyncWorkerStep, FilesystemAutomaticRemoteSyncExecutor,
    InterruptibleRemoteSyncSleeper,
};
use crate::config::CollectConfig;
use crate::domain::Provenance;
use crate::health_report::HealthReport;
use crate::history::{HistoryData, HistoryObservation, HistoryStore, default_history_root};
use crate::history_ownership::{HistoryOwnershipState, OwnershipManifestStatus};
use crate::history_profile_lease::{
    HistoryProfileLeaseGuard, TryHistoryProfileLease, try_acquire_history_profile_lease,
};
use crate::history_query::{HistorySourceSelector, SOURCE_SELECTION_UNAVAILABLE_WARNING};
use crate::history_runtime::{HistoryRuntime, HistoryRuntimeWriteReport};
use crate::output::{
    OutputFormat, OutputRequest, Section, render_output, request_is_failure, request_is_partial,
};
use crate::perf::{HistoryMetrics, PerfLog};
use crate::project_mapping::ProjectMappingStore;
use crate::remote_bandwidth_budget::{
    RemoteBandwidthAdmission, RemoteBandwidthBudgetPause, RemoteBandwidthBudgetStore,
    RemoteBandwidthTransferKind, budget_pause_from_io_error,
};
use crate::remote_fact_sync::{RemoteFactSyncLimits, RemoteFactTransport, SshRemoteFactTransport};
use crate::remote_protocol::{ProbeResult, RemoteCapability, RemoteExportResponseBody};
use crate::remote_source_metadata::{
    finalize_remote_source_metadata, prepare_remote_source_metadata, purge_detached_remote_source,
    reattach_remote_source_metadata_if_current, remove_remote_host_with_source_policy,
    set_remote_source_in_aggregates, unpair_remote_host_with_source_policy,
};
use crate::remote_sync::{
    FilesystemRemoteDeltaLocalPhases, RemoteSyncCompletion, RemoteSyncError,
    RemoteSyncHostSnapshot, RemoteSyncLimits, RemoteSyncReport, SshRemoteDeltaTransport,
    TryRemoteHostSyncLease, build_remote_delta_ingest_binding, preflight_remote_delta_position,
    sync_remote_delta_bounded, try_acquire_remote_host_sync_lease,
};
use crate::remote_sync_health::{
    RemoteSyncAttemptResult, RemoteSyncErrorCategory, RemoteSyncHealthStore,
};
use crate::remote_sync_scheduler::{
    MonotonicRemoteSyncClock, RemoteSyncScheduler, RemoteSyncSchedulerTick,
};
#[cfg(test)]
use crate::remote_transport::probe_remote_with_environment;
use crate::remote_transport::{
    RemoteProbeOptions, RemoteProbeReport, RemoteTransportError, SshCommandEnvironment,
    ensure_current_process_remote_containment, probe_remote_with_agent_executable_and_environment,
    tui_process_tree_inheritance_is_authorized,
};
use crate::remotes_config::{
    RemoteHostEdit, RemoteHostState, RemotesConfig, RemotesConfigMutation, RemotesConfigStore,
};
#[cfg(test)]
use crate::replica_fact_followup::DeferredRemoteFactTransport;
use crate::replica_fact_followup::{
    ReplicaFactFollowupReport, estimated_fact_network_bytes,
    execute_prepared_replica_fact_followup, fact_limits_for_response_budget,
    prepare_replica_fact_followup,
};
use crate::report_output::{
    health_report_is_partial, render_health_report, render_summary_report, render_trends_report,
    summary_report_is_partial, trends_report_is_partial,
};
use crate::rollout::RolloutCache;
use crate::service::{
    RecorderInstanceLockGuard, RecorderStatusFile, SERVICE_CUTOVER_PROTOCOL, ServiceOptions,
    TryRecorderInstanceLock, current_user_service_definition_observation, default_status_file,
    ensure_no_recorder_cutover_blocker_at, ensure_service_definition_is_trusted_at,
    incompatible_recorder_for_cutover, install as install_service, read_recorder_status,
    service_coordination_root, status as service_status, try_acquire_recorder_instance_lock,
    try_acquire_service_cutover_shared_at, uninstall as uninstall_service,
    validate_service_definition_id, write_recorder_status,
};
use crate::snapshot::{
    CollectionResult, collect_limits_snapshot, collect_snapshot, collect_snapshot_cached,
    collect_snapshot_cached_if_changed,
};
use crate::source_history::LocalObservationMode;
#[cfg(test)]
use crate::source_history::{RedactionProfile, SourceKind};
use crate::source_identity::{NodeId, SourceIdentity, SourceIdentityStore};
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

enum ReportHistoryStore {
    Runtime {
        runtime: Box<HistoryRuntime>,
        profile_lease: Option<HistoryProfileLeaseGuard>,
    },
    LegacyFallback {
        store: Box<HistoryStore>,
        writable: bool,
    },
}

impl ReportHistoryStore {
    fn legacy_history(&self) -> &HistoryStore {
        match self {
            Self::Runtime { runtime, .. } => runtime.legacy_history(),
            Self::LegacyFallback { store, .. } => store,
        }
    }

    fn validated_write_permitted(&self) -> io::Result<bool> {
        match self {
            Self::Runtime {
                profile_lease: Some(profile_lease),
                ..
            } => profile_lease.validate().map(|_| true),
            Self::Runtime {
                profile_lease: None,
                ..
            } => Ok(false),
            Self::LegacyFallback { writable, .. } => Ok(*writable),
        }
    }
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

    /// Internal remotes.json path preserved by service registrations.
    #[arg(long, global = true, value_name = "FILE", hide = true)]
    service_remotes_config: Option<PathBuf>,

    /// Internal: one-shot capability proving that the TUI launcher owns the
    /// complete helper process tree. A bare flag is deliberately invalid.
    #[arg(long, global = true, hide = true, value_name = "TUI_CAPABILITY")]
    inherit_remote_process_tree: Option<String>,

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
    /// Configure and test explicitly allowlisted remote Codex machines.
    Remote(RemoteArgs),
    /// Profile the normal TUI cold-start path without entering interactive mode.
    DebugStartup(DebugStartupArgs),
    /// Internal commands used by the remote usage exporter.
    #[command(hide = true)]
    RemoteAgent(RemoteAgentArgs),
}

#[derive(Clone, Debug, Args)]
struct RemoteAgentArgs {
    #[command(subcommand)]
    action: RemoteAgentAction,
}

#[derive(Clone, Debug, Subcommand)]
enum RemoteAgentAction {
    /// Serve exactly one framed request on stdin/stdout, then exit.
    Export,
    /// Print the stable source identity, creating it if it is genuinely absent.
    NodeId,
    /// Explicitly replace the source identity and invalidate prior cursors.
    RotateNodeId,
}

#[derive(Clone, Debug, Args)]
struct RemoteArgs {
    /// Use the same explicit history-v1 directory as a custom recorder.
    #[arg(long, global = true, value_name = "DIR")]
    history_dir: Option<PathBuf>,

    /// Internal optimistic-concurrency fence used by the interactive UI.
    #[arg(long, hide = true, global = true, value_name = "REVISION")]
    expected_revision: Option<u64>,

    #[command(subcommand)]
    action: RemoteAction,
}

#[derive(Clone, Debug, Subcommand)]
enum RemoteAction {
    /// Show or edit global remote-sync settings without connecting.
    Config(RemoteConfigArgs),
    /// Add one disabled, unpaired SSH alias without connecting.
    Add(RemoteAddArgs),
    /// Edit one configured host without connecting.
    Edit(RemoteEditArgs),
    /// List application allowlist entries without contacting them.
    List(RemoteListArgs),
    /// Probe one configured host and pin its complete source identity.
    Pair(RemoteHostArgs),
    /// Detach retained history, clear one host's identity pin, and disable it without connecting.
    Unpair(RemoteHostArgs),
    /// Probe exactly one configured host without changing configuration.
    Test(RemoteHostArgs),
    /// Synchronize exactly one paired host without changing automatic-sync settings.
    Sync(RemoteSyncArgs),
    /// Opt one already paired host into future automatic scheduling.
    Enable(RemoteHostArgs),
    /// Disable future automatic scheduling for one host.
    Disable(RemoteHostArgs),
    /// Remove one SSH allowlist entry without deleting its synchronized history.
    Remove(RemoteRemoveArgs),
    /// Inspect or change retained SSH source-history aggregation policy.
    Source(RemoteSourceArgs),
}

#[derive(Clone, Debug, Args)]
struct RemoteConfigArgs {
    /// Explicitly enable or disable the global automatic-sync switch.
    #[arg(long, action = clap::ArgAction::Set)]
    auto_sync: Option<bool>,

    #[arg(long, value_parser = clap::value_parser!(u64).range(30..=3600))]
    active_interval_seconds: Option<u64>,

    #[arg(long, value_parser = clap::value_parser!(u64).range(60..=86400))]
    idle_interval_seconds: Option<u64>,

    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,
}

#[derive(Clone, Debug, Args)]
struct RemoteAddArgs {
    id: String,

    /// One SSH config alias. This command never opens a connection.
    #[arg(long)]
    ssh_host: String,

    /// Remote agent executable or shell-safe path (defaults to codex-usage-monit).
    #[arg(long)]
    agent_executable: Option<String>,

    /// Keep exported title/message previews redacted (the default).
    #[arg(long, action = clap::ArgAction::Set)]
    redact_content: Option<bool>,
}

#[derive(Clone, Debug, Args)]
struct RemoteEditArgs {
    id: String,

    #[arg(long)]
    ssh_host: Option<String>,

    /// Remote agent executable or shell-safe path.
    #[arg(long)]
    agent_executable: Option<String>,

    /// Keep exported title/message previews redacted (the default).
    #[arg(long, action = clap::ArgAction::Set)]
    redact_content: Option<bool>,
}

#[derive(Clone, Debug, Args)]
struct RemoteListArgs {
    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,
}

#[derive(Clone, Debug, Args)]
struct RemoteHostArgs {
    id: String,
}

#[derive(Clone, Debug, Args)]
struct RemoteSyncArgs {
    id: String,

    /// Explicitly bypass the rolling 24-hour hard bandwidth cap for this invocation.
    #[arg(long)]
    ignore_budget: bool,
}

#[derive(Clone, Debug, Args)]
struct RemoteRemoveArgs {
    id: String,

    /// Keep already synchronized detached history included in aggregate views.
    #[arg(long)]
    keep_included: bool,
}

#[derive(Clone, Debug, Args)]
struct RemoteSourceArgs {
    #[command(subcommand)]
    action: RemoteSourceAction,
}

#[derive(Clone, Debug, Subcommand)]
enum RemoteSourceAction {
    /// List persisted SSH history sources without opening a connection.
    List(RemoteSourceListArgs),
    /// Include one persisted SSH source in aggregate queries.
    Include(RemoteSourceIdArgs),
    /// Exclude one persisted SSH source without deleting its history.
    Exclude(RemoteSourceIdArgs),
    /// Irreversibly delete one detached SSH source's retained history.
    Purge(RemoteSourceIdArgs),
}

#[derive(Clone, Debug, Args)]
struct RemoteSourceListArgs {
    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,
}

#[derive(Clone, Debug, Args)]
struct RemoteSourceIdArgs {
    source_id: NodeId,
}

#[derive(Clone, Debug, Args)]
struct RecordArgs {
    /// Explicitly run as a foreground process; service managers supervise this process.
    #[arg(long)]
    foreground: bool,

    /// Internal compatibility contract for source-aware service registrations.
    #[arg(long, hide = true, value_name = "VERSION")]
    service_cutover_protocol: Option<String>,

    /// Internal identity binding for the exact service-manager definition.
    #[arg(long, hide = true, value_name = "SHA256")]
    service_definition_id: Option<String>,

    /// Internal project mapping path preserved by service registrations.
    #[arg(long, hide = true, value_name = "FILE")]
    service_project_mapping_file: Option<PathBuf>,

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

    /// Usage source: all included sources, the exact local machine, or one remote node ID.
    #[arg(long, default_value_t = HistorySourceSelector::default())]
    source: HistorySourceSelector,

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

    /// Usage source: all included sources, the exact local machine, or one remote node ID.
    #[arg(long, default_value_t = HistorySourceSelector::default())]
    source: HistorySourceSelector,

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
    let service_remotes_config = cli.service_remotes_config.clone().map(absolute_path);
    let inherit_remote_process_tree =
        tui_process_tree_inheritance_is_authorized(cli.inherit_remote_process_tree.as_deref());
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
        Command::Record(args) => {
            return run_recorder(config, args, service_remotes_config, perf_log_path);
        }
        Command::Service(args) => {
            return run_service(&config, args, perf_log_path.as_deref());
        }
        Command::Remote(args) => {
            return run_remote(&config, args, inherit_remote_process_tree);
        }
        Command::RemoteAgent(args) => return run_remote_agent(&config, args),
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
        | Command::Remote(_)
        | Command::Summary(_)
        | Command::Trends(_)
        | Command::Health(_)
        | Command::RemoteAgent(_) => {
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

fn run_remote_agent(config: &CollectConfig, args: RemoteAgentArgs) -> Result<i32> {
    let store = SourceIdentityStore::discover();
    match args.action {
        RemoteAgentAction::Export => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            crate::remote_agent::serve_export(config, &store, stdin.lock(), &mut stdout.lock())?;
        }
        RemoteAgentAction::NodeId => {
            write_stdout(&render_source_identity(&store.load_or_create()?)?)?;
        }
        RemoteAgentAction::RotateNodeId => {
            write_stdout(&render_source_identity(&store.rotate()?)?)?;
        }
    }
    Ok(0)
}

fn run_remote(
    collect_config: &CollectConfig,
    args: RemoteArgs,
    inherit_remote_process_tree: bool,
) -> Result<i32> {
    let opens_ssh = matches!(
        &args.action,
        RemoteAction::Pair(_) | RemoteAction::Test(_) | RemoteAction::Sync(_)
    );
    if opens_ssh && !inherit_remote_process_tree {
        ensure_current_process_remote_containment().map_err(|error| {
            anyhow::anyhow!("could not establish process-level containment for remote SSH: {error}")
        })?;
    }
    let store = RemotesConfigStore::discover();
    let history_dir = args.history_dir.map(absolute_path);
    let expected_revision = args.expected_revision;
    let termination = RemoteCommandTerminationSignal::install(opens_ssh)?;
    let cancellation = termination.cancellation();
    match args.action {
        RemoteAction::Config(args) => {
            let mut transaction = store.begin_transaction()?;
            ensure_expected_remote_config_revision(
                transaction.config(),
                expected_revision,
                "config update",
            )?;
            if let Some(enabled) = args.auto_sync {
                transaction.apply(RemotesConfigMutation::set_auto_sync_enabled(enabled))?;
            }
            if args.active_interval_seconds.is_some() || args.idle_interval_seconds.is_some() {
                let active = args
                    .active_interval_seconds
                    .unwrap_or(transaction.config().active_interval_seconds());
                let idle = args
                    .idle_interval_seconds
                    .unwrap_or(transaction.config().idle_interval_seconds());
                transaction.apply(RemotesConfigMutation::set_intervals(active, idle))?;
            }
            let config = store.commit(transaction)?;
            write_stdout(&render_remotes_config(&config, args.format)?)?;
            Ok(0)
        }
        RemoteAction::Add(args) => {
            let mut transaction = store.begin_transaction()?;
            ensure_expected_remote_config_revision(
                transaction.config(),
                expected_revision,
                "host add",
            )?;
            let id = args.id;
            transaction.apply(RemotesConfigMutation::add_host(id.clone(), args.ssh_host))?;
            if args.agent_executable.is_some() || args.redact_content.is_some() {
                transaction.apply(RemotesConfigMutation::edit_host(
                    id,
                    RemoteHostEdit {
                        agent_executable: args.agent_executable,
                        redact_content: args.redact_content,
                        ..RemoteHostEdit::default()
                    },
                ))?;
            }
            let config = store.commit(transaction)?;
            write_stdout(&render_remotes_config(&config, FormatArg::Text)?)?;
            Ok(0)
        }
        RemoteAction::Edit(args) => {
            if args.ssh_host.is_none()
                && args.agent_executable.is_none()
                && args.redact_content.is_none()
            {
                bail!(
                    "remote edit requires --ssh-host, --agent-executable, and/or --redact-content"
                );
            }
            let config = update_remotes_config_expected(
                &store,
                expected_revision,
                RemotesConfigMutation::edit_host(
                    args.id,
                    RemoteHostEdit {
                        ssh_host: args.ssh_host,
                        agent_executable: args.agent_executable,
                        redact_content: args.redact_content,
                    },
                ),
            )?;
            write_stdout(&render_remotes_config(&config, FormatArg::Text)?)?;
            Ok(0)
        }
        RemoteAction::List(args) => {
            let config = store.load_or_create()?;
            ensure_expected_remote_config_revision(&config, expected_revision, "host list")?;
            write_stdout(&render_remotes_config(&config, args.format)?)?;
            Ok(0)
        }
        RemoteAction::Pair(args) => run_remote_pair(
            collect_config,
            &store,
            &args.id,
            history_dir.as_deref(),
            expected_revision,
            inherit_remote_process_tree,
            cancellation.as_ref(),
        ),
        RemoteAction::Unpair(args) => run_remote_unpair(
            collect_config,
            &store,
            &args.id,
            history_dir.as_deref(),
            expected_revision,
        ),
        RemoteAction::Test(args) => run_remote_test(
            collect_config,
            &store,
            &args.id,
            history_dir.as_deref(),
            expected_revision,
            inherit_remote_process_tree,
            cancellation.as_ref(),
        ),
        RemoteAction::Sync(args) => run_remote_sync(
            collect_config,
            &store,
            &args.id,
            args.ignore_budget,
            history_dir.as_deref(),
            expected_revision,
            inherit_remote_process_tree,
            cancellation.as_ref(),
        ),
        RemoteAction::Enable(args) => {
            let config = update_remotes_config_expected(
                &store,
                expected_revision,
                RemotesConfigMutation::enable_host(args.id),
            )?;
            write_stdout(&render_remotes_config(&config, FormatArg::Text)?)?;
            Ok(0)
        }
        RemoteAction::Disable(args) => {
            let config = update_remotes_config_expected(
                &store,
                expected_revision,
                RemotesConfigMutation::disable_host(args.id),
            )?;
            write_stdout(&render_remotes_config(&config, FormatArg::Text)?)?;
            Ok(0)
        }
        RemoteAction::Remove(args) => run_remote_remove(
            collect_config,
            &store,
            &args,
            history_dir.as_deref(),
            expected_revision,
        ),
        RemoteAction::Source(args) => {
            run_remote_source(collect_config, &store, args, history_dir.as_deref())
        }
    }
}

/// Converts terminal termination signals into a polled flag long enough for
/// direct remote commands and the recorder's automatic worker to kill and
/// reap independently isolated SSH process groups. Windows uses a
/// kill-on-close Job Object, so parent termination already tears down the
/// complete tree without an in-process handler.
struct RemoteCommandTerminationSignal {
    #[cfg(unix)]
    cancellation: Option<Arc<AtomicBool>>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl RemoteCommandTerminationSignal {
    fn install(enabled: bool) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

            if !enabled {
                return Ok(Self {
                    cancellation: None,
                    registrations: Vec::new(),
                });
            }
            let cancellation = Arc::new(AtomicBool::new(false));
            let mut registrations = Vec::with_capacity(6);
            for signal in [SIGHUP, SIGINT, SIGTERM] {
                // Registration order is significant. The conditional default
                // handler observes `false` on the first signal, after which the
                // flag handler arms both cooperative cancellation and the
                // default action for a second termination signal. This keeps a
                // blocked filesystem/collector phase force-terminable without
                // making the first signal unsafe for SSH/history cleanup.
                let conditional = signal_hook::flag::register_conditional_default(
                    signal,
                    Arc::clone(&cancellation),
                );
                let conditional = match conditional {
                    Ok(registration) => registration,
                    Err(error) => {
                        unregister_signal_handlers(&mut registrations);
                        return Err(error);
                    }
                };
                registrations.push(conditional);
                match signal_hook::flag::register(signal, Arc::clone(&cancellation)) {
                    Ok(registration) => registrations.push(registration),
                    Err(error) => {
                        unregister_signal_handlers(&mut registrations);
                        return Err(error);
                    }
                }
            }
            Ok(Self {
                cancellation: Some(cancellation),
                registrations,
            })
        }

        #[cfg(not(unix))]
        {
            let _ = enabled;
            Ok(Self {})
        }
    }

    fn cancellation(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        #[cfg(unix)]
        {
            self.cancellation.clone()
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    #[cfg(all(test, unix))]
    fn for_test() -> Self {
        Self {
            cancellation: Some(Arc::new(AtomicBool::new(false))),
            registrations: Vec::new(),
        }
    }

    #[cfg(all(test, unix))]
    fn request_for_test(&self) {
        if let Some(cancellation) = self.cancellation.as_deref() {
            cancellation.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

#[cfg(unix)]
fn unregister_signal_handlers(registrations: &mut Vec<signal_hook::SigId>) {
    for registration in registrations.drain(..) {
        signal_hook::low_level::unregister(registration);
    }
}

impl Drop for RemoteCommandTerminationSignal {
    fn drop(&mut self) {
        #[cfg(unix)]
        unregister_signal_handlers(&mut self.registrations);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_remote_sync(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    host_id: &str,
    ignore_budget: bool,
    history_dir: Option<&Path>,
    expected_revision: Option<u64>,
    inherit_remote_process_tree: bool,
    cancellation: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> Result<i32> {
    let state_root = remote_state_root(history_dir)?;
    let environment =
        remote_ssh_environment(collect_config, inherit_remote_process_tree, cancellation);
    let mut transport = SshRemoteDeltaTransport::new(environment.clone());
    let mut fact_transport = SshRemoteFactTransport::new(environment);
    let outcome = execute_remote_sync_at_state_root_with_transports(
        collect_config,
        store,
        host_id,
        state_root,
        &mut transport,
        &mut fact_transport,
        ignore_budget,
        expected_revision,
    )?;
    write_stdout(&outcome.output)?;
    Ok(outcome.exit_code)
}

#[derive(Debug)]
struct RemoteSyncCommandOutcome {
    exit_code: i32,
    output: String,
}

#[cfg(test)]
fn execute_remote_sync_at_state_root(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    host_id: &str,
    state_root: PathBuf,
    transport: &mut impl crate::remote_sync::RemoteDeltaTransport,
) -> Result<RemoteSyncCommandOutcome> {
    execute_remote_sync_at_state_root_with_budget_override(
        collect_config,
        store,
        host_id,
        state_root,
        transport,
        false,
        None,
    )
}

#[cfg(test)]
fn execute_remote_sync_at_state_root_with_budget_override(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    host_id: &str,
    state_root: PathBuf,
    transport: &mut impl crate::remote_sync::RemoteDeltaTransport,
    ignore_budget: bool,
    expected_revision: Option<u64>,
) -> Result<RemoteSyncCommandOutcome> {
    let mut fact_transport = DeferredRemoteFactTransport;
    execute_remote_sync_at_state_root_with_transports(
        collect_config,
        store,
        host_id,
        state_root,
        transport,
        &mut fact_transport,
        ignore_budget,
        expected_revision,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_remote_sync_at_state_root_with_transports(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    host_id: &str,
    state_root: PathBuf,
    transport: &mut impl crate::remote_sync::RemoteDeltaTransport,
    fact_transport: &mut impl RemoteFactTransport,
    ignore_budget: bool,
    expected_revision: Option<u64>,
) -> Result<RemoteSyncCommandOutcome> {
    let config = store.load_or_create()?;
    ensure_expected_remote_config_revision(&config, expected_revision, "host sync")?;
    let host = config
        .host(host_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("remote host {host_id:?} is not configured"))?;
    let selected =
        RemoteSyncHostSnapshot::capture_manual(&config, &host).map_err(anyhow::Error::new)?;
    if host.redact_content() != collect_config.redact_content {
        bail!(
            "remote sync for {host_id:?} cannot safely activate a history namespace with redact-content={} while the local collector uses redact-content={}; run `codex-usage-monit remote edit {host_id} --redact-content {}` to make the policies match, then retry; no local history was changed and no SSH connection was opened",
            host.redact_content(),
            collect_config.redact_content,
            collect_config.redact_content,
        );
    }
    let legacy_history_root = absolute_path(state_root).join("history-v1");
    let mut runtime = HistoryRuntime::new(
        legacy_history_root,
        &collect_config.codex_home,
        collect_config.redact_content,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} could not bind its local history runtime: {error}; no SSH connection was opened"
        )
    })?;
    // Hold the exact profile/redaction selection across the network exchange.
    // A recorder using the same selection may coexist and all actual writes
    // remain serialized by the short v2 writer lease. An opposite redaction
    // selection is rejected before SSH so shared source metadata cannot race.
    let history_profile_lease = acquire_runtime_profile_lease(&runtime).map_err(|error| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} could not select its local history profile: {error}; no SSH connection was opened"
        )
    })?;
    ensure_remote_sync_runtime_v2(host_id, &mut runtime)?;
    let _host_sync_lease = match try_acquire_remote_host_sync_lease(runtime.state_root(), host_id)
        .map_err(|error| {
            anyhow::anyhow!(
                "remote sync for {host_id:?} could not acquire its host attempt lease: {error}; no SSH connection was opened"
            )
        })?
    {
        TryRemoteHostSyncLease::Acquired(lease) => lease,
        TryRemoteHostSyncLease::Busy => {
            bail!(
                "remote sync for {host_id:?} is already running in another process; retry after that attempt finishes; no SSH connection was opened"
            );
        }
    };
    prepare_remote_source_metadata(store, &selected, &runtime).map_err(anyhow::Error::new)?;
    history_profile_lease.validate().map_err(|error| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} lost its history profile authority before SSH: {error}; no SSH connection was opened"
        )
    })?;

    let health_store = RemoteSyncHealthStore::new(runtime.state_root().to_path_buf());
    let bandwidth_budget = RemoteBandwidthBudgetStore::new(runtime.state_root().to_path_buf());
    let health_ready = prepare_manual_remote_sync_health(&health_store, store);
    let attempted_at = Utc::now();
    let source = host.expected_source().cloned();
    let node_id = source.as_ref().map(|source| &source.node_id);
    let transfer_kind = if ignore_budget {
        RemoteBandwidthTransferKind::ManualOverride
    } else {
        RemoteBandwidthTransferKind::Manual
    };
    let mut limits = RemoteSyncLimits::default();
    let fact_limits = RemoteFactSyncLimits::default();
    let binding = build_remote_delta_ingest_binding(&selected, runtime.profile_id().clone())
        .map_err(anyhow::Error::new)?;
    let mut local =
        FilesystemRemoteDeltaLocalPhases::new(runtime.ownership(), runtime.source_history());
    if let Err(error) =
        preflight_remote_delta_position(store, &selected, &binding, &mut local, attempted_at)
    {
        let health_write = if health_ready {
            health_store
                .record_sync_error_for_config(
                    host.id(),
                    source.as_ref(),
                    Utc::now(),
                    &error,
                    None,
                    selected.host(),
                )
                .map(|_| ())
        } else {
            Ok(())
        };
        finish_manual_remote_sync_health(&health_store, store, health_write.map(|_| ()));
        return Err(anyhow::Error::new(error));
    }
    let reservation = match bandwidth_budget.begin_sync_attempt(
        host.id(),
        node_id,
        attempted_at,
        transfer_kind,
        limits.max_response_bytes,
        limits.max_pages.get(),
    )? {
        RemoteBandwidthAdmission::Granted(reservation) => reservation,
        RemoteBandwidthAdmission::Paused(pause) => {
            let completed_at = Utc::now();
            let health_write = if health_ready {
                health_store
                    .record_pause(
                        host.id(),
                        source.as_ref(),
                        completed_at,
                        pause.level(),
                        pause.resume_at(),
                    )
                    .map(|_| ())
            } else {
                Ok(())
            };
            finish_manual_remote_sync_health(&health_store, store, health_write.map(|_| ()));
            return Err(manual_remote_bandwidth_pause_error(
                host.id(),
                &pause,
                ignore_budget,
            ));
        }
    };
    limits.max_response_bytes = reservation
        .granted_response_bytes()?
        .min(limits.max_response_bytes);
    let sync_result = sync_remote_delta_bounded(
        store,
        &selected,
        runtime.profile_id().clone(),
        &mut local,
        transport,
        attempted_at,
        limits,
    );
    let report = match sync_result {
        Ok(report) => report,
        Err(error) => {
            // Only variants that are structurally guaranteed to precede the
            // first transport exchange may release a reservation. Transport,
            // protocol, configuration-CAS, remote, and local errors can all
            // happen after bytes have crossed the wire, so they retain the
            // conservative 24h reservation.
            if manual_error_proves_transport_not_started(&error) {
                let _ = bandwidth_budget
                    .cancel_attempt(&reservation, Utc::now().max(reservation.started_at()));
            }
            let health_write = if health_ready {
                health_store
                    .record_sync_error_for_config(
                        host.id(),
                        source.as_ref(),
                        Utc::now(),
                        &error,
                        None,
                        selected.host(),
                    )
                    .map(|_| ())
            } else {
                Ok(())
            };
            finish_manual_remote_sync_health(&health_store, store, health_write.map(|_| ()));
            return Err(anyhow::Error::new(error));
        }
    };
    if let Err(error) = bandwidth_budget.complete_report(&reservation, Utc::now(), &report) {
        let health_write = if health_ready {
            health_store
                .record_failure(
                    host.id(),
                    source.as_ref(),
                    Utc::now(),
                    RemoteSyncErrorCategory::LocalState,
                    None,
                )
                .map(|_| ())
        } else {
            Ok(())
        };
        finish_manual_remote_sync_health(&health_store, store, health_write.map(|_| ()));
        return Err(anyhow::anyhow!(
            "remote sync for {host_id:?} received and committed data but could not persist its bandwidth charge: {error}"
        ));
    }

    // Replica facts are a best-effort refinement after the aggregate page is
    // committed and charged. They receive a second admission so they cannot
    // consume bytes that the rolling host budget did not explicitly grant.
    let fact_started_at = Utc::now();
    let (mut fact_followup, fact_reservation) =
        match prepare_replica_fact_followup(&selected, &runtime, &health_store, fact_started_at) {
            Err(attention) => (attention, None),
            Ok(prepared) if !prepared.requires_remote_transport() => (
                execute_prepared_replica_fact_followup(
                    prepared,
                    store,
                    &selected,
                    &runtime,
                    &health_store,
                    collect_config,
                    fact_transport,
                    fact_started_at,
                    None,
                ),
                None,
            ),
            Ok(prepared) => match bandwidth_budget.begin_sync_attempt(
                host.id(),
                node_id,
                fact_started_at,
                transfer_kind,
                fact_limits.max_response_bytes,
                fact_limits.max_exchanges_per_run(),
            ) {
                Err(_) => (ReplicaFactFollowupReport::local_state_attention(), None),
                Ok(RemoteBandwidthAdmission::Paused(_)) => {
                    (ReplicaFactFollowupReport::resource_attention(), None)
                }
                Ok(RemoteBandwidthAdmission::Granted(fact_reservation)) => {
                    match fact_reservation.granted_response_bytes() {
                        Ok(granted) => (
                            execute_prepared_replica_fact_followup(
                                prepared,
                                store,
                                &selected,
                                &runtime,
                                &health_store,
                                collect_config,
                                fact_transport,
                                fact_started_at,
                                fact_limits_for_response_budget(granted),
                            ),
                            Some(fact_reservation),
                        ),
                        Err(_) => (
                            ReplicaFactFollowupReport::local_state_attention(),
                            Some(fact_reservation),
                        ),
                    }
                }
            },
        };
    let fact_completed_at = Utc::now();
    let fact_process_containment =
        fact_followup.error_category() == Some(RemoteSyncErrorCategory::ProcessContainment);
    if fact_process_containment
        && health_ready
        && let Some(expected_source) = source.as_ref()
        && health_store
            .record_fact_sync_process_containment(
                host.id(),
                expected_source,
                fact_completed_at,
                selected.host(),
            )
            .is_err()
    {
        warn_remote_sync_health_persistence_failed();
    }
    if let Some(fact_reservation) = fact_reservation
        && (fact_followup.error_category().is_none() || !fact_followup.network_may_have_started())
        && estimated_fact_network_bytes(&fact_followup)
            .and_then(|actual| {
                bandwidth_budget
                    .complete_attempt(&fact_reservation, fact_completed_at, actual)
                    .map(|_| ())
            })
            .is_err()
    {
        // The aggregate remains committed and charged. Retain a failed fact
        // reservation conservatively and surface the local ledger problem.
        fact_followup.mark_local_state_attention();
    }
    if !fact_process_containment
        && health_ready
        && let Some(expected_source) = source.as_ref()
    {
        let _ = health_store.record_fact_sync_outcome(
            host.id(),
            expected_source,
            fact_completed_at,
            fact_followup.error_category(),
        );
    }
    if let Err(error) = history_profile_lease.validate() {
        let health_write = if health_ready {
            health_store
                .record_failure(
                    host.id(),
                    source.as_ref(),
                    Utc::now(),
                    RemoteSyncErrorCategory::LocalState,
                    None,
                )
                .map(|_| ())
        } else {
            Ok(())
        };
        finish_manual_remote_sync_health(&health_store, store, health_write.map(|_| ()));
        return Err(anyhow::anyhow!(
            "remote sync for {host_id:?} could not revalidate its history profile after SSH: {error}"
        ));
    }
    if let Err(error) = finalize_remote_source_metadata(store, &selected, &runtime) {
        let health_write = if health_ready {
            health_store
                .record_failure(
                    host.id(),
                    source.as_ref(),
                    Utc::now(),
                    RemoteSyncErrorCategory::LocalState,
                    None,
                )
                .map(|_| ())
        } else {
            Ok(())
        };
        finish_manual_remote_sync_health(&health_store, store, health_write.map(|_| ()));
        return Err(anyhow::Error::new(error));
    }
    let completed_at = Utc::now();
    let health_write = if health_ready {
        if fact_process_containment {
            source.as_ref().map_or_else(
                || {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "manual fact containment pause requires an exact source pin",
                    ))
                },
                |expected_source| {
                    health_store.record_success_with_process_containment(
                        host.id(),
                        expected_source,
                        completed_at,
                        &report,
                        selected.host(),
                    )
                },
            )
        } else {
            health_store.record_success(host.id(), source.as_ref(), completed_at, &report, None)
        }
        .map(|_| ())
    } else {
        Ok(())
    };
    finish_manual_remote_sync_health(&health_store, store, health_write.map(|_| ()));
    if fact_process_containment {
        return Err(anyhow::anyhow!(
            "remote sync for {host_id:?} committed its aggregate page, but a session-fact SSH helper could not be fully reclaimed; automatic synchronization for this host is paused until its host configuration changes or an explicit test/sync succeeds"
        ));
    }
    let mut outcome = format_remote_sync_report(&host, &report);
    outcome
        .output
        .push_str(&format!("\nfacts={}", fact_followup.state().label()));
    if fact_followup.inventory_too_large() {
        outcome.output.push_str(" (bounded inventory unavailable)");
    }
    Ok(outcome)
}

fn manual_remote_bandwidth_pause_error(
    host_id: &str,
    pause: &RemoteBandwidthBudgetPause,
    ignore_budget: bool,
) -> anyhow::Error {
    let resume = pause
        .resume_at()
        .map_or_else(|| "unknown".to_owned(), |resume_at| resume_at.to_rfc3339());
    let override_hint = if ignore_budget {
        "the explicit override cannot bypass clock or ledger-integrity safety checks"
    } else {
        "retry later or explicitly run `remote sync <id> --ignore-budget` once"
    };
    anyhow::anyhow!(
        "remote sync for {host_id:?} paused before SSH by the rolling 24-hour bandwidth budget ({:?}/{:?}, {} used, {} limit, resumes {}); {override_hint}",
        pause.level(),
        pause.reason(),
        pause.usage().rolling_bytes(),
        pause.limit_bytes(),
        resume,
    )
}

fn manual_error_proves_transport_not_started(error: &RemoteSyncError) -> bool {
    matches!(
        error,
        RemoteSyncError::HostNotPaired { .. }
            | RemoteSyncError::InvalidLimits(_)
            | RemoteSyncError::PreTransportLocal(_)
    )
}

fn prepare_manual_remote_sync_health(
    health_store: &RemoteSyncHealthStore,
    config_store: &RemotesConfigStore,
) -> bool {
    let reconciled = config_store.load().and_then(|current| {
        health_store
            .reconcile_configured_hosts(&current)
            .map(|_| ())
    });
    if reconciled.is_err() {
        warn_remote_sync_health_persistence_failed();
        return false;
    }
    true
}

fn finish_manual_remote_sync_health(
    health_store: &RemoteSyncHealthStore,
    config_store: &RemotesConfigStore,
    record_result: io::Result<()>,
) {
    let reconcile_result = config_store.load().and_then(|current| {
        health_store
            .reconcile_configured_hosts(&current)
            .map(|_| ())
    });
    if record_result.is_err() || reconcile_result.is_err() {
        warn_remote_sync_health_persistence_failed();
    }
}

fn warn_remote_sync_health_persistence_failed() {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(
        stderr,
        "warning: remote sync health persistence failed; synchronization results are unaffected"
    );
}

fn runtime_requires_v2_cutover(runtime: &HistoryRuntime) -> io::Result<bool> {
    Ok(match runtime.ownership().load_manifest()? {
        OwnershipManifestStatus::Uninitialized => true,
        OwnershipManifestStatus::Initialized(manifest) => {
            manifest.state() != HistoryOwnershipState::V2Active
        }
    })
}

fn ensure_remote_sync_runtime_v2(host_id: &str, runtime: &mut HistoryRuntime) -> Result<()> {
    if !runtime_requires_v2_cutover(runtime).map_err(|error| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} could not inspect local history ownership: {error}; no SSH connection was opened"
        )
    })? {
        return Ok(());
    }

    // Only cutover requires the singleton recorder slot. Normal v2 manual
    // sync may coexist with a same-profile recorder because each local commit
    // is independently fenced by the ownership writer lease.
    let history_root = runtime.legacy_history().history_root().ok_or_else(|| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} has no local legacy history namespace; no SSH connection was opened"
        )
    })?;
    let _cutover_guard =
        match try_acquire_recorder_instance_lock(history_root).map_err(|error| {
            anyhow::anyhow!(
                "remote sync for {host_id:?} could not verify the local cutover lock: {error}; no SSH connection was opened"
            )
        })? {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => {
                bail!(
                    "remote sync for {host_id:?} cannot migrate history while a recorder owns this state; let that recorder finish the source-aware cutover or stop it, then retry; no SSH connection was opened"
                );
            }
        };

    // Pre-v0.4 recorders do not participate in either new lock protocol.
    reject_incompatible_recorder_before_cutover(host_id, runtime)?;
    runtime.ensure_v2_active().map_err(|error| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} could not migrate local history to source-aware v2: {error}; repair the local history state and retry; no SSH connection was opened"
        )
    })?;
    Ok(())
}

fn acquire_runtime_profile_lease(runtime: &HistoryRuntime) -> io::Result<HistoryProfileLeaseGuard> {
    match try_acquire_history_profile_lease(
        runtime.state_root(),
        runtime.profile_id().clone(),
        runtime.redaction_profile(),
    )? {
        TryHistoryProfileLease::Acquired(guard) => Ok(guard),
        TryHistoryProfileLease::Busy { active_profile } => {
            let detail = active_profile.map_or_else(
                || "a profile transition is in progress".to_owned(),
                |active| {
                    format!(
                        "the active history selection uses {:?}",
                        active.redaction_profile()
                    )
                },
            );
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{detail}; retry after the other process exits"),
            ))
        }
    }
}

#[cfg(test)]
fn remote_redaction_profile(host: &crate::remotes_config::RemoteHostConfig) -> RedactionProfile {
    if host.redact_content() {
        RedactionProfile::Redacted
    } else {
        RedactionProfile::PreviewEnabled
    }
}

fn reject_incompatible_recorder_before_cutover(
    host_id: &str,
    runtime: &HistoryRuntime,
) -> Result<()> {
    let legacy_history_root = runtime.legacy_history().history_root().ok_or_else(|| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} has no local legacy history namespace; no SSH connection was opened"
        )
    })?;
    let status_path = default_status_file(legacy_history_root);
    let incompatible = incompatible_recorder_for_cutover(
        &status_path,
        runtime.legacy_history().namespace(),
        Utc::now(),
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "remote sync for {host_id:?} could not verify recorder compatibility from {}: {error}; inspect or repair the recorder status before retrying; no SSH connection was opened",
            status_path.display()
        )
    })?;
    if let Some(status) = incompatible {
        let last_activity = status
            .last_history_heartbeat
            .unwrap_or(status.last_attempt_at);
        bail!(
            "remote sync for {host_id:?} cannot migrate history while a recent legacy recorder may still be writing namespace {:?} (pid {}, last activity {}); run `codex-usage-monit service uninstall` to stop it, reinstall or update this application, then run `codex-usage-monit service install` to restart it with the current version before retrying; no SSH connection was opened",
            runtime.legacy_history().namespace(),
            status.pid,
            last_activity.to_rfc3339()
        );
    }
    Ok(())
}

fn format_remote_sync_report(
    host: &crate::remotes_config::RemoteHostConfig,
    report: &RemoteSyncReport,
) -> RemoteSyncCommandOutcome {
    let (exit_code, status, next) = match &report.completion {
        RemoteSyncCompletion::Complete => (0, "complete", None),
        RemoteSyncCompletion::Continuation(position) => (2, "continuation", Some(position)),
        RemoteSyncCompletion::BootstrapRestarted(position) => {
            (2, "bootstrap-restarted", Some(position))
        }
    };
    let mut output = format!(
        "remote-sync {} via {}\nstatus={} pages={} response={}B",
        host.id(),
        host.ssh_host(),
        status,
        report.pages_committed,
        report.response_bytes
    );
    if let Some(position) = next {
        let cursor = position
            .delta_cursor
            .map(|cursor| format!("{}:{}", cursor.generation, cursor.sequence))
            .unwrap_or_else(|| "bootstrap".to_owned());
        output.push_str(&format!("\nnext-cursor={cursor}"));
        if let Some(range) = &position.exact_range {
            output.push_str(&format!(
                " next-range={}..{}",
                range.from.to_rfc3339(),
                range.to.to_rfc3339()
            ));
        }
        output.push_str("\nrun the same command again to continue");
    }
    RemoteSyncCommandOutcome { exit_code, output }
}

#[cfg(test)]
fn update_remotes_config(
    store: &RemotesConfigStore,
    mutation: RemotesConfigMutation,
) -> Result<RemotesConfig> {
    update_remotes_config_expected(store, None, mutation)
}

fn update_remotes_config_expected(
    store: &RemotesConfigStore,
    expected_revision: Option<u64>,
    mutation: RemotesConfigMutation,
) -> Result<RemotesConfig> {
    let current = store.load_or_create()?;
    ensure_expected_remote_config_revision(&current, expected_revision, "host update")?;
    Ok(store.update(current.config_revision(), mutation)?)
}

fn ensure_expected_remote_config_revision(
    config: &RemotesConfig,
    expected_revision: Option<u64>,
    operation: &str,
) -> Result<()> {
    let Some(expected_revision) = expected_revision else {
        return Ok(());
    };
    if config.config_revision() != expected_revision {
        bail!(
            "remote {operation} was not started because configuration revision changed from {expected_revision} to {}; reopen the remote panel and retry",
            config.config_revision()
        );
    }
    Ok(())
}

fn run_remote_pair(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    host_id: &str,
    history_dir: Option<&Path>,
    expected_revision: Option<u64>,
    inherit_remote_process_tree: bool,
    cancellation: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> Result<i32> {
    let config = store.load_or_create()?;
    ensure_expected_remote_config_revision(&config, expected_revision, "host pair")?;
    let host = config
        .host(host_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("remote host {host_id:?} is not configured"))?;
    // Bind the exact local persistence domain before opening SSH. A custom
    // recorder root must never be paired into the platform-default history by
    // accident, and an invalid local root must remain a pre-transport error.
    let (runtime, _profile_lease) =
        remote_source_lifecycle_runtime(collect_config, history_dir, "pair")?;
    let report = probe_configured_host(
        collect_config,
        &host,
        inherit_remote_process_tree,
        cancellation,
    )?;
    let RemoteExportResponseBody::Probe(probe) = &report.response.result else {
        unreachable!("the probe transport accepts only probe responses")
    };
    let missing_capabilities = missing_remote_sync_capabilities(&host, probe);
    if !probe.state_writable || !probe.rollout_readable || !missing_capabilities.is_empty() {
        bail!(
            "remote host {host_id:?} is reachable but not ready: state_writable={} rollout_readable={} missing_capabilities={}",
            probe.state_writable,
            probe.rollout_readable,
            if missing_capabilities.is_empty() {
                "none".to_owned()
            } else {
                missing_capabilities.join(",")
            }
        );
    }
    if &report.response.source.node_id == runtime.source_identity().node_id() {
        bail!(
            "remote host {host_id:?} reports the same node identity as this center machine; pairing was not changed"
        );
    }
    match runtime
        .source_history()
        .load_source_metadata(&report.response.source.node_id)
    {
        Ok(metadata) if metadata.kind() != crate::source_history::SourceKind::Ssh => {
            bail!(
                "remote host {host_id:?} collides with a non-SSH local history source; pairing was not changed"
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let source_id = report.response.source.node_id.clone();
    let paired = store.pair_if_current_checked(
        config.config_revision(),
        &host,
        report.response.source.clone(),
        || {
            runtime
                .source_history()
                .ensure_source_not_pending_purge(&source_id)
        },
    )?;
    let host = paired
        .host(host_id)
        .expect("a successful pair mutation preserves the selected host");
    let selected =
        RemoteSyncHostSnapshot::capture_manual(&paired, host).map_err(anyhow::Error::new)?;
    if let Err(error) = reattach_remote_source_metadata_if_current(store, &selected, &runtime) {
        bail!(
            "remote host {host_id:?} was paired, but its retained source metadata could not be reattached: {error}; retry the same pair command after repairing local history state"
        );
    }
    write_stdout(&format_remote_probe("paired", host, &report))?;
    Ok(0)
}

fn run_remote_unpair(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    host_id: &str,
    history_dir: Option<&Path>,
    expected_revision: Option<u64>,
) -> Result<i32> {
    let config = store.load_or_create()?;
    ensure_expected_remote_config_revision(&config, expected_revision, "host unpair")?;
    let host = config
        .host(host_id)
        .ok_or_else(|| anyhow::anyhow!("remote host {host_id:?} is not configured"))?;
    if host.expected_source().is_none() && host.previous_source().is_none() {
        let config = store.update(
            config.config_revision(),
            RemotesConfigMutation::unpair_host(host_id.to_owned()),
        )?;
        write_stdout(&render_remotes_config(&config, FormatArg::Text)?)?;
        return Ok(0);
    }

    let (runtime, _profile_lease) =
        remote_source_lifecycle_runtime(collect_config, history_dir, "unpair")?;
    let outcome =
        unpair_remote_host_with_source_policy(store, config.config_revision(), host_id, &runtime)?;
    write_stdout(&render_remotes_config(outcome.config(), FormatArg::Text)?)?;
    Ok(0)
}

fn run_remote_remove(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    args: &RemoteRemoveArgs,
    history_dir: Option<&Path>,
    expected_revision: Option<u64>,
) -> Result<i32> {
    let config = store.load_or_create()?;
    ensure_expected_remote_config_revision(&config, expected_revision, "host remove")?;
    let host = config
        .host(&args.id)
        .ok_or_else(|| anyhow::anyhow!("remote host {:?} is not configured", args.id))?;
    if host.expected_source().is_none() && host.previous_source().is_none() {
        let config = store.update(
            config.config_revision(),
            RemotesConfigMutation::remove_host(args.id.clone()),
        )?;
        write_stdout(&render_remotes_config(&config, FormatArg::Text)?)?;
        return Ok(0);
    }
    let (runtime, _profile_lease) =
        remote_source_lifecycle_runtime(collect_config, history_dir, "remove")?;
    let outcome = remove_remote_host_with_source_policy(
        store,
        config.config_revision(),
        &args.id,
        &runtime,
        args.keep_included,
    )?;
    write_stdout(&render_remotes_config(outcome.config(), FormatArg::Text)?)?;
    Ok(0)
}

fn run_remote_source(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    args: RemoteSourceArgs,
    history_dir: Option<&Path>,
) -> Result<i32> {
    match args.action {
        RemoteSourceAction::List(args) => {
            let (runtime, _profile_lease) =
                remote_source_lifecycle_runtime(collect_config, history_dir, "source list")?;
            let sources = runtime
                .source_history()
                .list_source_metadata()?
                .into_iter()
                .filter(|source| source.kind() == crate::source_history::SourceKind::Ssh)
                .collect::<Vec<_>>();
            let output =
                match args.format {
                    FormatArg::Json => serde_json::to_string_pretty(&sources)?,
                    FormatArg::Text => {
                        if sources.is_empty() {
                            "No persisted SSH history sources.".to_owned()
                        } else {
                            sources
                                .iter()
                                .map(|source| {
                                    format!(
                                    "{}  {}  {}  {}  {}",
                                    source.source_id(),
                                    if source.detached() { "detached" } else { "attached" },
                                    if source.include_in_aggregates() {
                                        "included"
                                    } else {
                                        "excluded"
                                    },
                                    match source.aggregate_redaction_profile() {
                                        crate::source_history::RedactionProfile::Redacted => {
                                            "redacted"
                                        }
                                        crate::source_history::RedactionProfile::PreviewEnabled => {
                                            "preview"
                                        }
                                    },
                                    source.display_label(),
                                )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    }
                };
            write_stdout(&output)?;
            Ok(0)
        }
        RemoteSourceAction::Include(args) => {
            run_remote_source_policy(collect_config, history_dir, &args.source_id, true)
        }
        RemoteSourceAction::Exclude(args) => {
            run_remote_source_policy(collect_config, history_dir, &args.source_id, false)
        }
        RemoteSourceAction::Purge(args) => {
            let (runtime, _profile_lease) =
                remote_source_lifecycle_runtime(collect_config, history_dir, "source purge")?;
            let outcome = purge_detached_remote_source(store, &runtime, &args.source_id)?;
            write_stdout(&format!(
                "{} purged (ingest namespaces removed: {}, project instances removed: {}{})",
                args.source_id,
                outcome.ingest_namespaces_removed(),
                outcome.project_instances_removed(),
                if outcome.resumed_history_purge() {
                    ", resumed after interruption"
                } else {
                    ""
                }
            ))?;
            Ok(0)
        }
    }
}

fn run_remote_source_policy(
    collect_config: &CollectConfig,
    history_dir: Option<&Path>,
    source_id: &NodeId,
    include: bool,
) -> Result<i32> {
    let (runtime, _profile_lease) =
        remote_source_lifecycle_runtime(collect_config, history_dir, "source policy update")?;
    let source = set_remote_source_in_aggregates(&runtime, source_id, include)?;
    write_stdout(&format!(
        "{} {} ({})",
        source.source_id(),
        if source.include_in_aggregates() {
            "included"
        } else {
            "excluded"
        },
        if source.detached() {
            "detached"
        } else {
            "attached"
        }
    ))?;
    Ok(0)
}

fn remote_source_lifecycle_runtime(
    collect_config: &CollectConfig,
    history_dir: Option<&Path>,
    operation: &str,
) -> Result<(HistoryRuntime, HistoryProfileLeaseGuard)> {
    let runtime = HistoryRuntime::new(
        remote_history_root(history_dir)?,
        &collect_config.codex_home,
        collect_config.redact_content,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "remote {operation} could not bind local source-aware history: {error}; no source lifecycle change was published"
        )
    })?;
    let profile_lease = acquire_runtime_profile_lease(&runtime).map_err(|error| {
        anyhow::anyhow!(
            "remote {operation} could not select the active local history profile: {error}; no source lifecycle change was published"
        )
    })?;
    Ok((runtime, profile_lease))
}

fn remote_history_root(history_dir: Option<&Path>) -> Result<PathBuf> {
    Ok(match history_dir {
        Some(path) => absolute_path(path.to_path_buf()),
        None => absolute_path(
            default_history_root()
                .ok_or_else(|| anyhow::anyhow!("a history state directory is unavailable"))?,
        ),
    })
}

fn remote_state_root(history_dir: Option<&Path>) -> Result<PathBuf> {
    let history_root = remote_history_root(history_dir)?;
    if history_root.file_name() != Some(std::ffi::OsStr::new("history-v1")) {
        bail!(
            "remote operation cannot derive the source-aware history state root from {}",
            history_root.display()
        );
    }
    history_root.parent().map(Path::to_path_buf).ok_or_else(|| {
        anyhow::anyhow!(
            "remote operation cannot derive the source-aware history state root from {}",
            history_root.display()
        )
    })
}

fn run_remote_test(
    collect_config: &CollectConfig,
    store: &RemotesConfigStore,
    host_id: &str,
    history_dir: Option<&Path>,
    expected_revision: Option<u64>,
    inherit_remote_process_tree: bool,
    cancellation: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> Result<i32> {
    let config = store.load_or_create()?;
    ensure_expected_remote_config_revision(&config, expected_revision, "host test")?;
    let host = config
        .host(host_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("remote host {host_id:?} is not configured"))?;
    let report = match probe_configured_host(
        collect_config,
        &host,
        inherit_remote_process_tree,
        cancellation,
    ) {
        Ok(report) => report,
        Err(error) => {
            if error
                .downcast_ref::<RemoteTransportError>()
                .is_some_and(RemoteTransportError::process_containment_uncertain)
                && let Some(source) = host.expected_source()
            {
                let recorded = record_manual_remote_test_containment_pause(
                    history_dir,
                    &host,
                    source,
                    Utc::now(),
                );
                if recorded.is_err() {
                    warn_remote_sync_health_persistence_failed();
                }
            }
            return Err(error);
        }
    };
    let RemoteExportResponseBody::Probe(probe) = &report.response.result else {
        unreachable!("the probe transport accepts only probe responses")
    };
    let current_host = ensure_remote_probe_target_current(store, config.config_revision(), &host)?;
    write_stdout(&format_remote_probe("tested", &current_host, &report))?;
    let ready = probe.state_writable
        && probe.rollout_readable
        && missing_remote_sync_capabilities(&current_host, probe).is_empty();
    if ready {
        let cleared =
            clear_manual_remote_test_containment_pause(history_dir, &current_host, Utc::now());
        if cleared.is_err() {
            warn_remote_sync_health_persistence_failed();
        }
    }
    Ok(if ready { 0 } else { 2 })
}

fn record_manual_remote_test_containment_pause(
    history_dir: Option<&Path>,
    host: &crate::remotes_config::RemoteHostConfig,
    source: &crate::remote_protocol::SourceGeneration,
    observed_at: chrono::DateTime<Utc>,
) -> Result<()> {
    let state_root = remote_state_root(history_dir)?;
    RemoteSyncHealthStore::new(state_root)
        .record_process_containment_pause(host.id(), source, observed_at, host)
        .map(|_| ())
        .map_err(anyhow::Error::new)
}

fn clear_manual_remote_test_containment_pause(
    history_dir: Option<&Path>,
    host: &crate::remotes_config::RemoteHostConfig,
    succeeded_at: chrono::DateTime<Utc>,
) -> Result<bool> {
    let state_root = remote_state_root(history_dir)?;
    RemoteSyncHealthStore::new(state_root)
        .clear_process_containment_pause(host.id(), host.expected_source(), succeeded_at)
        .map_err(anyhow::Error::new)
}

fn missing_remote_sync_capabilities(
    host: &crate::remotes_config::RemoteHostConfig,
    probe: &ProbeResult,
) -> Vec<&'static str> {
    let mut missing = Vec::new();
    for (capability, label) in [
        (RemoteCapability::DeltaJournal, "delta_journal"),
        (RemoteCapability::LiveSnapshot, "live_snapshot"),
        (RemoteCapability::GzipFrame, "gzip_frame"),
        (
            if host.redact_content() {
                RemoteCapability::RedactedContent
            } else {
                RemoteCapability::PreviewContent
            },
            if host.redact_content() {
                "redacted_content"
            } else {
                "preview_content"
            },
        ),
    ] {
        if !probe.capabilities.contains(&capability) {
            missing.push(label);
        }
    }
    missing
}

/// Rejects a probe result if any remote configuration changed while the SSH
/// process was running. A test never commits data, but reporting an old host
/// alias or identity pin as current would still be misleading.
fn ensure_remote_probe_target_current(
    store: &RemotesConfigStore,
    expected_revision: u64,
    expected_host: &crate::remotes_config::RemoteHostConfig,
) -> Result<crate::remotes_config::RemoteHostConfig> {
    let current = store.load()?;
    if current.config_revision() != expected_revision {
        bail!(
            "stale remote test response for {:?}: config revision changed from {} to {}; retry the test",
            expected_host.id(),
            expected_revision,
            current.config_revision()
        );
    }
    let current_host = current.host(expected_host.id()).cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "stale remote test response for {:?}: host was removed; retry after reconfiguration",
            expected_host.id()
        )
    })?;
    if &current_host != expected_host {
        bail!(
            "stale remote test response for {:?}: host configuration changed without a revision advance",
            expected_host.id()
        );
    }
    Ok(current_host)
}

fn probe_configured_host(
    collect_config: &CollectConfig,
    host: &crate::remotes_config::RemoteHostConfig,
    inherit_remote_process_tree: bool,
    cancellation: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> Result<RemoteProbeReport> {
    let options = RemoteProbeOptions {
        redaction_profile: if host.redact_content() {
            crate::source_history::RedactionProfile::Redacted
        } else {
            crate::source_history::RedactionProfile::PreviewEnabled
        },
        expected_source: host.expected_source().cloned(),
        ..RemoteProbeOptions::default()
    };
    probe_remote_with_agent_executable_and_environment(
        host.ssh_host(),
        host.agent_executable(),
        &options,
        &remote_ssh_environment(collect_config, inherit_remote_process_tree, cancellation),
    )
    .map_err(anyhow::Error::new)
}

fn remote_ssh_environment(
    collect_config: &CollectConfig,
    inherit_remote_process_tree: bool,
    cancellation: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> SshCommandEnvironment {
    let environment = if inherit_remote_process_tree {
        SshCommandEnvironment::inheriting_parent_process_tree(
            collect_config.app_server_path.clone(),
        )
    } else {
        SshCommandEnvironment::new(collect_config.app_server_path.clone())
    };
    if let Some(cancellation) = cancellation {
        environment.with_cancellation(Arc::clone(cancellation))
    } else {
        environment
    }
}

fn render_remotes_config(config: &RemotesConfig, format: FormatArg) -> Result<String> {
    match format {
        FormatArg::Json => Ok(serde_json::to_string_pretty(config)?),
        FormatArg::Text => {
            let mut output = format!(
                "auto-sync={} revision={} active={}s idle={}s configured={} enabled={}\n",
                if config.auto_sync_enabled() {
                    "on"
                } else {
                    "off"
                },
                config.config_revision(),
                config.active_interval_seconds(),
                config.idle_interval_seconds(),
                config.hosts().len(),
                config
                    .hosts()
                    .iter()
                    .filter(|host| host.sync_enabled())
                    .count()
            );
            if config.hosts().is_empty() {
                output.push_str("no remote hosts configured");
                return Ok(output);
            }
            output.push_str("ID\tSTATE\tSSH HOST\tAGENT\tCONTENT\tSOURCE\n");
            for host in config.hosts() {
                let source = host
                    .expected_source()
                    .map(|source| format!("{}@{}", source.node_id, source.generation))
                    .unwrap_or_else(|| "-".to_string());
                output.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\n",
                    host.id(),
                    remote_host_state_label(host.state()),
                    host.ssh_host(),
                    host.agent_executable(),
                    if host.redact_content() {
                        "redacted"
                    } else {
                        "preview"
                    },
                    source
                ));
            }
            while output.ends_with('\n') {
                output.pop();
            }
            Ok(output)
        }
    }
}

fn remote_host_state_label(state: RemoteHostState) -> &'static str {
    match state {
        RemoteHostState::ConfiguredUnpaired => "unpaired",
        RemoteHostState::PairedDisabled => "paired-disabled",
        RemoteHostState::PairedEnabled => "paired-enabled",
    }
}

fn format_remote_probe(
    action: &str,
    host: &crate::remotes_config::RemoteHostConfig,
    report: &RemoteProbeReport,
) -> String {
    let RemoteExportResponseBody::Probe(probe) = &report.response.result else {
        unreachable!("the probe transport accepts only probe responses")
    };
    let capabilities = probe
        .capabilities
        .iter()
        .map(|capability| remote_capability_label(*capability))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{action} {} via {}\nsource={}@{}\nstate-writable={} rollout-readable={} capabilities={}\nprotocol={} server={} elapsed={:.3}s payload={}B/{}B",
        host.id(),
        host.ssh_host(),
        report.response.source.node_id,
        report.response.source.generation,
        probe.state_writable,
        probe.rollout_readable,
        capabilities,
        report.response.protocol_version,
        report.response.server_version,
        report.elapsed.as_secs_f64(),
        report.request_bytes,
        report.response_bytes,
    )
}

fn remote_capability_label(capability: RemoteCapability) -> &'static str {
    match capability {
        RemoteCapability::DeltaJournal => "delta_journal",
        RemoteCapability::LiveSnapshot => "live_snapshot",
        RemoteCapability::SessionFactSnapshot => "session_fact_snapshot",
        RemoteCapability::SessionFactDelta => "session_fact_delta",
        RemoteCapability::RedactedContent => "redacted_content",
        RemoteCapability::PreviewContent => "preview_content",
        RemoteCapability::GzipFrame => "gzip_frame",
    }
}

fn render_source_identity(identity: &SourceIdentity) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "nodeId": identity.node_id(),
        "generation": identity.generation(),
    }))?)
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
        collect_and_load_report_history_selected(config, &result, args.history_dir, &args.source);
    let range: SummaryRange = args.range.into();
    if range == SummaryRange::ThirtyDays
        && !matches!(&args.source, HistorySourceSelector::Remote(_))
        && summary_history_backfill_needed(&history, query_now)
    {
        let (backfilled_history, observed_at) =
            backfill_summary_history_selected(config, &mut history_store, &args.source);
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
    let mut report_snapshot = result.snapshot.clone();
    if matches!(&args.source, HistorySourceSelector::Remote(_)) {
        // Live task metadata belongs to this machine. Even a matching thread
        // ID must not relabel a remote session in an exact-source report.
        report_snapshot.tasks.clear();
    }
    let report = build_summary_report(&report_snapshot, &history, query);
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
        collect_and_load_report_history_selected(config, &result, args.history_dir, &args.source);
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
) -> (ReportHistoryStore, HistoryData) {
    collect_and_load_report_history_selected(
        config,
        result,
        history_dir,
        &HistorySourceSelector::AllIncluded,
    )
}

fn collect_and_load_report_history_selected(
    config: &CollectConfig,
    result: &CollectionResult,
    history_dir: Option<PathBuf>,
    source_selector: &HistorySourceSelector,
) -> (ReportHistoryStore, HistoryData) {
    let total_started = Instant::now();
    let (mut store, mut setup_warnings) = report_history_store(config, history_dir);
    let write_permitted = match store.validated_write_permitted() {
        Ok(permitted) => permitted,
        Err(error) => {
            setup_warnings.push(format!(
                "history persistence is read-only because its profile lease could not be revalidated: {error}"
            ));
            false
        }
    };
    let observation = report_history_observation(result, config.offline);
    let stage_started = Instant::now();
    match &mut store {
        ReportHistoryStore::Runtime { runtime, .. } => {
            if let Err(error) = runtime.stage_local_collection(
                &observation,
                &result.snapshot.tasks,
                &result.local_session_digests,
            ) {
                let normalized = runtime
                    .prepare_local_collection_observation(&observation, &result.snapshot.tasks);
                runtime.stage(&normalized);
                setup_warnings.push(format!(
                    "local session digest evidence could not be staged: {error}"
                ));
            }
        }
        ReportHistoryStore::LegacyFallback { store, .. } => store.stage(&observation),
    }
    let stage_elapsed = stage_started.elapsed();
    let record_started = Instant::now();
    let (mut history, mut metrics) = match &mut store {
        ReportHistoryStore::Runtime { runtime, .. } => {
            let write_result = if write_permitted {
                runtime.flush_staged()
            } else {
                Ok(None)
            };
            let record_elapsed = record_started.elapsed();
            let load_started = Instant::now();
            let selection = source_selector.resolve(runtime.source_identity().node_id());
            let mut history = match runtime.load_unified_history_since_with_staged_selected(
                &selection,
                history_view_since(result.snapshot.as_of),
            ) {
                Ok(snapshot) => snapshot.history,
                Err(error) => {
                    let mut history = HistoryData::default();
                    if !matches!(source_selector, HistorySourceSelector::Remote(_)) {
                        runtime.legacy_history().overlay_staged_since(
                            &mut history,
                            history_view_since(result.snapshot.as_of),
                        );
                    }
                    history
                        .warnings
                        .push(format!("history query failed: {error}"));
                    history
                }
            };
            let load_elapsed = load_started.elapsed();
            let mut metrics = HistoryMetrics::with_durations(
                total_started.elapsed(),
                record_elapsed,
                Some(load_elapsed),
            );
            apply_runtime_write_metrics(&mut metrics, &write_result);
            merge_runtime_history_write_result(&mut history, write_result, "history persistence");
            (history, metrics)
        }
        ReportHistoryStore::LegacyFallback { store, writable } => {
            let write_result = if *writable {
                store.flush_staged()
            } else {
                Ok(None)
            };
            let record_elapsed = record_started.elapsed();
            let load_started = Instant::now();
            let history = store.load_since_with_staged(history_view_since(result.snapshot.as_of));
            let mut history = legacy_history_for_source_selector(history, source_selector);
            let load_elapsed = load_started.elapsed();
            let mut metrics = HistoryMetrics::with_durations(
                total_started.elapsed(),
                record_elapsed,
                Some(load_elapsed),
            );
            apply_legacy_write_metrics(&mut metrics, &write_result);
            merge_history_write_result(&mut history, write_result);
            (history, metrics)
        }
    };
    metrics.stage_us = u64::try_from(stage_elapsed.as_micros()).unwrap_or(u64::MAX);
    metrics.record_performed = write_permitted;
    if !write_permitted {
        history.read_only = true;
    }
    history.warnings.extend(setup_warnings);
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

fn legacy_history_for_source_selector(
    history: HistoryData,
    source_selector: &HistorySourceSelector,
) -> HistoryData {
    let HistorySourceSelector::Remote(source_id) = source_selector else {
        return history;
    };
    let mut selected = HistoryData {
        quota_points: history.quota_points,
        read_only: history.read_only,
        ..HistoryData::default()
    };
    selected.warnings.extend(history.warnings);
    selected.warnings.push(format!(
        "{SOURCE_SELECTION_UNAVAILABLE_WARNING}:unsupported_by_legacy:{source_id}"
    ));
    selected
}

fn report_history_store(
    config: &CollectConfig,
    history_dir: Option<PathBuf>,
) -> (ReportHistoryStore, Vec<String>) {
    let explicit_root = history_dir.map(absolute_path);
    let runtime = explicit_root.as_ref().map_or_else(
        || HistoryRuntime::discover(&config.codex_home, config.redact_content),
        |history_root| {
            HistoryRuntime::new(
                history_root.clone(),
                &config.codex_home,
                config.redact_content,
            )
        },
    );
    match runtime {
        Ok(mut runtime) => {
            let (profile_lease, mut warnings) = match acquire_runtime_profile_lease(&runtime) {
                Ok(guard) => (Some(guard), Vec::new()),
                Err(error) => (
                    None,
                    vec![format!(
                        "history persistence is read-only because the requested profile cannot be selected: {error}"
                    )],
                ),
            };
            if profile_lease.is_some() {
                warnings.extend(prepare_report_history_runtime(&mut runtime));
            }
            (
                ReportHistoryStore::Runtime {
                    runtime: Box::new(runtime),
                    profile_lease,
                },
                warnings,
            )
        }
        Err(error) => {
            let store = explicit_root.map_or_else(
                || HistoryStore::discover_with_redaction(&config.codex_home, config.redact_content),
                |history_root| {
                    HistoryStore::new_with_redaction(
                        history_root,
                        &config.codex_home,
                        config.redact_content,
                    )
                },
            );
            (
                ReportHistoryStore::LegacyFallback {
                    store: Box::new(store),
                    writable: false,
                },
                vec![format!(
                    "source-aware history runtime unavailable; using a read-only legacy view; no history will be persisted until the source-aware state is repaired: {error}"
                )],
            )
        }
    }
}

fn prepare_report_history_runtime(runtime: &mut HistoryRuntime) -> Vec<String> {
    let mut warnings = Vec::new();
    match runtime_requires_v2_cutover(runtime) {
        Ok(false) => return warnings,
        Err(error) => {
            warnings.push(format!(
                "source-aware history ownership could not be inspected: {error}"
            ));
            return warnings;
        }
        Ok(true) => {}
    }

    let history_root = runtime
        .legacy_history()
        .history_root()
        .expect("a bound runtime always has a legacy root");
    let _cutover_guard = match try_acquire_recorder_instance_lock(history_root) {
        Ok(TryRecorderInstanceLock::Acquired(guard)) => guard,
        Ok(TryRecorderInstanceLock::Busy) => {
            if let Err(error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!("history ownership initialization failed: {error}"));
            }
            warnings.push(
                "source-aware history cutover deferred while another recorder owns this state"
                    .to_owned(),
            );
            return warnings;
        }
        Err(error) => {
            if let Err(initialization_error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!(
                    "history ownership initialization failed: {initialization_error}"
                ));
            }
            warnings.push(format!(
                "source-aware history cutover deferred because its recorder lock could not be verified: {error}"
            ));
            return warnings;
        }
    };

    let status_path = default_status_file(history_root);
    let incompatible = incompatible_recorder_for_cutover(
        &status_path,
        runtime.legacy_history().namespace(),
        Utc::now(),
    );
    match incompatible {
        Ok(Some(status)) => {
            if let Err(error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!("history ownership initialization failed: {error}"));
            }
            warnings.push(format!(
                "source-aware history cutover deferred while legacy recorder pid {} may still be active",
                status.pid
            ));
        }
        Ok(None) => {
            if let Err(error) = runtime.ensure_v2_active() {
                warnings.push(format!("source-aware history cutover failed: {error}"));
            }
        }
        Err(error) => {
            if let Err(initialization_error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!(
                    "history ownership initialization failed: {initialization_error}"
                ));
            }
            warnings.push(format!(
                "source-aware history cutover deferred because recorder status could not be verified at {}: {error}",
                status_path.display()
            ));
        }
    }
    warnings
}

fn apply_legacy_write_metrics(
    metrics: &mut HistoryMetrics,
    write_result: &io::Result<Option<crate::history::HistoryWriteReport>>,
) {
    match write_result {
        Ok(Some(report)) => {
            metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
            metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
            metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
            metrics.read_only = report.read_only;
        }
        Ok(None) => {}
        Err(_) => metrics.warnings = 1,
    }
}

fn apply_runtime_write_metrics(
    metrics: &mut HistoryMetrics,
    write_result: &io::Result<Option<HistoryRuntimeWriteReport>>,
) {
    match write_result {
        Ok(Some(HistoryRuntimeWriteReport::V1(report))) => {
            metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
            metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
            metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
            metrics.read_only = report.read_only;
        }
        Ok(Some(HistoryRuntimeWriteReport::V2(report))) => {
            metrics.shards_written = u64::try_from(
                report
                    .account
                    .shards_written
                    .saturating_add(report.buckets.shards_written)
                    .saturating_add(report.weekly.shards_written)
                    .saturating_add(report.session_digests.shards_written),
            )
            .unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(
                report
                    .account
                    .shards_skipped
                    .saturating_add(report.buckets.shards_skipped)
                    .saturating_add(report.weekly.shards_skipped)
                    .saturating_add(report.session_digests.shards_skipped),
            )
            .unwrap_or(u64::MAX);
        }
        Ok(None) => {}
        Err(_) => metrics.warnings = 1,
    }
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
    write_result: io::Result<Option<crate::history::HistoryWriteReport>>,
) {
    match write_result {
        Ok(Some(report)) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings);
        }
        Ok(None) => {}
        Err(error) => history
            .warnings
            .push(format!("history persistence failed: {error}")),
    }
}

fn merge_runtime_history_write_result(
    history: &mut HistoryData,
    write_result: io::Result<Option<HistoryRuntimeWriteReport>>,
    operation: &str,
) {
    match write_result {
        Ok(Some(HistoryRuntimeWriteReport::V1(report))) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings);
        }
        Ok(Some(HistoryRuntimeWriteReport::V2(_))) | Ok(None) => {}
        Err(error) => history
            .warnings
            .push(format!("{operation} failed: {error}")),
    }
}

fn backfill_summary_history_selected(
    config: &CollectConfig,
    store: &mut ReportHistoryStore,
    source_selector: &HistorySourceSelector,
) -> (HistoryData, DateTime<Utc>) {
    let worker_config = summary_backfill_config(config);
    let result = collect_snapshot(&worker_config, None, false);
    let scan_complete = summary_backfill_scan_complete(&result.snapshot);
    let observed_at = result.snapshot.as_of;
    let tasks = result.snapshot.tasks.clone();
    let local_session_digests = result.local_session_digests;
    let mut observation = result.history_observation;
    // Summary reconstruction is local-only. Offline fallback quota samples
    // must never replace server-backed quota history.
    observation.quota_points.clear();
    observation.weekly_local_points.clear();
    retain_summary_backfill_evidence_buckets(&mut observation);
    let since = history_view_since(observed_at);
    let (write_permitted, mut profile_validation_warning) = match store.validated_write_permitted()
    {
        Ok(permitted) => (permitted, None),
        Err(error) => (
            false,
            Some(format!(
                "summary backfill persistence is read-only because its profile lease could not be revalidated: {error}"
            )),
        ),
    };
    let mut history = match store {
        ReportHistoryStore::Runtime { runtime, .. } => {
            if let Err(error) =
                runtime.stage_full_local_collection(&observation, &tasks, &local_session_digests)
            {
                let normalized = runtime.prepare_local_collection_observation(&observation, &tasks);
                runtime.stage_full_observation(&normalized);
                profile_validation_warning
                    .get_or_insert_with(|| {
                        format!(
                            "local session digest evidence could not be staged for reconciliation: {error}"
                        )
                    });
            }
            let write_result = if write_permitted {
                runtime.flush_staged_reconcile(since, observed_at)
            } else {
                Ok(None)
            };
            let selection = source_selector.resolve(runtime.source_identity().node_id());
            let mut history =
                match runtime.load_unified_history_since_with_staged_selected(&selection, since) {
                    Ok(snapshot) => snapshot.history,
                    Err(error) => {
                        let mut history = HistoryData::default();
                        if !matches!(source_selector, HistorySourceSelector::Remote(_)) {
                            runtime
                                .legacy_history()
                                .overlay_staged_since(&mut history, since);
                        }
                        history
                            .warnings
                            .push(format!("summary backfill history query failed: {error}"));
                        history
                    }
                };
            merge_runtime_history_write_result(
                &mut history,
                write_result,
                "summary backfill persistence",
            );
            history
        }
        ReportHistoryStore::LegacyFallback { store, writable } => {
            store.stage_full_observation(&observation);
            let write_result = if *writable {
                store.flush_staged()
            } else {
                Ok(None)
            };
            let mut history = legacy_history_for_source_selector(
                store.load_since_with_staged(since),
                source_selector,
            );
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
            history
        }
    };
    if !write_permitted {
        history.read_only = true;
        if matches!(store, ReportHistoryStore::Runtime { .. }) {
            history.warnings.push(
                "summary backfill persistence deferred because this process does not hold the active history profile lease"
                    .to_owned(),
            );
        }
    }
    if let Some(warning) = profile_validation_warning {
        history.warnings.push(warning);
    }
    let requested_complete =
        scan_complete && summary_history_coverage_complete(&history, observed_at);
    let marker_write_permitted = match store.validated_write_permitted() {
        Ok(permitted) => permitted,
        Err(error) => {
            history.warnings.push(format!(
                "summary backfill marker is read-only because its profile lease could not be revalidated: {error}"
            ));
            false
        }
    };
    let marker = match store {
        ReportHistoryStore::Runtime { runtime, .. } if marker_write_permitted => {
            runtime.mark_summary_backfill_attempt(observed_at, requested_complete)
        }
        ReportHistoryStore::Runtime { .. } => Ok(crate::history::SummaryBackfillAttempt {
            completed_at: observed_at,
            complete: requested_complete,
        }),
        ReportHistoryStore::LegacyFallback { store, writable }
            if marker_write_permitted && *writable =>
        {
            store.mark_summary_backfill_attempt(observed_at, requested_complete)
        }
        ReportHistoryStore::LegacyFallback { .. } => Ok(crate::history::SummaryBackfillAttempt {
            completed_at: observed_at,
            complete: requested_complete,
        }),
    };
    let marker = match marker {
        Ok(marker) => marker,
        Err(error) => {
            history
                .warnings
                .push(format!("summary backfill marker failed: {error}"));
            crate::history::SummaryBackfillAttempt {
                completed_at: observed_at,
                complete: requested_complete,
            }
        }
    };
    history.summary_backfill_attempted_at = Some(marker.completed_at);
    history.summary_backfill_attempt_complete = Some(marker.complete);
    normalize_history_warnings(&mut history);
    (history, observed_at)
}

fn normalize_history_warnings(history: &mut HistoryData) {
    history.warnings.sort();
    history.warnings.dedup();
}

fn recorder_status_for_history(
    store: &ReportHistoryStore,
) -> (Option<RecorderStatusFile>, Option<String>) {
    let store = store.legacy_history();
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
    store: &ReportHistoryStore,
    perf_log: Option<&Path>,
) -> (Option<crate::service::ServiceStatus>, Option<String>) {
    let store = store.legacy_history();
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
    options.remotes_config_file = RemotesConfigStore::discover()
        .path()
        .map(|path| absolute_path(path.to_path_buf()));
    options.project_mapping_file = ProjectMappingStore::discover()
        .path()
        .map(|path| absolute_path(path.to_path_buf()));
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

#[derive(Default)]
struct AutomaticRemoteSyncDiagnosticState {
    global: Option<String>,
    host_failures: BTreeMap<String, String>,
    health_persistence_failed: bool,
}

type AutomaticRemoteSyncDiagnostic = Arc<Mutex<AutomaticRemoteSyncDiagnosticState>>;

struct RecorderRemoteSyncWorkerGuard {
    stop: AutomaticRemoteSyncStopToken,
    worker: Option<thread::JoinHandle<()>>,
}

const RECORDER_REMOTE_WORKER_JOIN_GRACE: Duration = Duration::from_millis(500);
const RECORDER_REMOTE_WORKER_JOIN_POLL: Duration = Duration::from_millis(10);

impl Drop for RecorderRemoteSyncWorkerGuard {
    fn drop(&mut self) {
        self.stop.request_stop();
        let Some(worker) = self.worker.take() else {
            return;
        };
        // A self-join would deadlock. Production owns this guard on the
        // recorder thread, but keep Drop safe if ownership changes later.
        if worker.thread().id() != thread::current().id() {
            let deadline = Instant::now() + RECORDER_REMOTE_WORKER_JOIN_GRACE;
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(RECORDER_REMOTE_WORKER_JOIN_POLL);
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
            // A poisoned filesystem/config lock must not make recorder Drop
            // unbounded. Dropping an unfinished JoinHandle detaches it; the
            // shared stop flag still prevents a second host from starting.
            // In-flight transports receive the same flag and perform bounded
            // process-tree/pipe cleanup; any escaped helper is surfaced as a
            // cleanup diagnostic rather than making this Drop unbounded.
        }
    }
}

fn start_recorder_remote_sync_worker(
    state_root: PathBuf,
    collect_config: &CollectConfig,
    diagnostic: &AutomaticRemoteSyncDiagnostic,
    remotes_config_file: Option<PathBuf>,
    project_mapping_store: ProjectMappingStore,
    stop: AutomaticRemoteSyncStopToken,
) -> Option<RecorderRemoteSyncWorkerGuard> {
    if ensure_current_process_remote_containment().is_err() {
        set_automatic_remote_sync_diagnostic(
            diagnostic,
            Some("automatic remote sync unavailable: process containment failed".into()),
        );
        return None;
    }
    let state_root = match fs::canonicalize(state_root) {
        Ok(state_root) => state_root,
        Err(_) => {
            set_automatic_remote_sync_diagnostic(
                diagnostic,
                Some("automatic remote sync unavailable: state-root validation failed".into()),
            );
            return None;
        }
    };
    let codex_home = match fs::canonicalize(&collect_config.codex_home) {
        Ok(codex_home) => codex_home,
        Err(_) => {
            set_automatic_remote_sync_diagnostic(
                diagnostic,
                Some("automatic remote sync unavailable: Codex home validation failed".into()),
            );
            return None;
        }
    };
    let config_store = remotes_config_file
        .map(RemotesConfigStore::new)
        .unwrap_or_else(RemotesConfigStore::discover);
    let health_store = RemoteSyncHealthStore::new(state_root.clone());
    let mut remote_collect_config = collect_config.clone();
    remote_collect_config.codex_home = codex_home;
    let cancellation = stop.cancellation_flag();
    let environment = remote_ssh_environment(&remote_collect_config, false, Some(&cancellation));
    let executor = FilesystemAutomaticRemoteSyncExecutor::with_transports_and_project_mapping_store(
        state_root,
        remote_collect_config,
        config_store.clone(),
        AutomaticRemoteCutoverPolicy::RequireV2Active,
        SshRemoteDeltaTransport::new(environment.clone()),
        SshRemoteFactTransport::new(environment),
        project_mapping_store,
    );
    let worker_stop = stop.clone();
    let worker_diagnostic = Arc::clone(diagnostic);
    let worker_config_store = config_store.clone();
    let spawn_result = thread::Builder::new()
        .name("codex-remote-sync".to_string())
        .spawn(move || {
            let mut worker = AutomaticRemoteSyncWorker::new(
                RemoteSyncScheduler::new(MonotonicRemoteSyncClock::default()),
                executor,
                config_store,
                InterruptibleRemoteSyncSleeper,
                worker_stop,
            );
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                worker.run_with_observer(|step| {
                    observe_automatic_remote_sync_step_with_health(
                        &worker_diagnostic,
                        &health_store,
                        &worker_config_store,
                        step,
                    );
                });
            }));
            if outcome.is_err() {
                set_automatic_remote_sync_diagnostic(
                    &worker_diagnostic,
                    Some("automatic remote sync worker stopped unexpectedly".into()),
                );
            }
        });
    match spawn_result {
        Ok(worker) => Some(RecorderRemoteSyncWorkerGuard {
            stop,
            worker: Some(worker),
        }),
        Err(_) => {
            set_automatic_remote_sync_diagnostic(
                diagnostic,
                Some("automatic remote sync unavailable: worker thread could not start".into()),
            );
            None
        }
    }
}

fn observe_automatic_remote_sync_step_with_health(
    diagnostic: &AutomaticRemoteSyncDiagnostic,
    health_store: &RemoteSyncHealthStore,
    config_store: &RemotesConfigStore,
    step: &AutomaticRemoteSyncWorkerStep,
) {
    observe_automatic_remote_sync_step(diagnostic, step);
    let persisted = persist_automatic_remote_sync_health(health_store, config_store, step);
    match persisted {
        Ok(Some(config)) => {
            let refreshed = refresh_automatic_remote_sync_failures_from_health(
                diagnostic,
                health_store,
                &config,
            );
            diagnostic
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .health_persistence_failed = refreshed.is_err();
        }
        // A non-scheduler event does not touch the durable health state and
        // therefore must not clear a previous persistence failure.
        Ok(None) => {}
        Err(_) => {
            diagnostic
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .health_persistence_failed = true;
        }
    }
}

fn refresh_automatic_remote_sync_failures_from_health(
    diagnostic: &AutomaticRemoteSyncDiagnostic,
    health_store: &RemoteSyncHealthStore,
    config: &RemotesConfig,
) -> io::Result<()> {
    let eligible = config
        .automatic_hosts()
        .map(|host| host.id())
        .collect::<BTreeSet<_>>();
    let host_failures = health_store
        .list()?
        .into_iter()
        .filter(|health| eligible.contains(health.host_id()) && !health.budget_paused())
        .filter_map(|health| {
            let aggregate_failure = (health.last_result()
                == Some(RemoteSyncAttemptResult::Failure))
            .then(|| health.error_category())
            .flatten()
            .map(|category| remote_sync_health_error_category(category).to_owned());
            let fact_attention = health.fact_sync_error_category().map(|category| {
                format!(
                    "session facts need attention: {}",
                    remote_sync_health_error_category(category)
                )
            });
            aggregate_failure
                .or(fact_attention)
                .map(|category| (health.host_id().to_owned(), category))
        })
        .collect();
    diagnostic
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .host_failures = host_failures;
    Ok(())
}

fn remote_sync_health_error_category(category: RemoteSyncErrorCategory) -> &'static str {
    match category {
        RemoteSyncErrorCategory::Configuration => "configuration changed",
        RemoteSyncErrorCategory::Policy => "policy",
        RemoteSyncErrorCategory::Busy => "local state busy",
        RemoteSyncErrorCategory::ResourceLimit => "request validation",
        RemoteSyncErrorCategory::LocalState => "local state",
        RemoteSyncErrorCategory::Protocol => "protocol",
        RemoteSyncErrorCategory::ProcessContainment => {
            "SSH process containment failed; automatic sync paused"
        }
        RemoteSyncErrorCategory::Transport => "transport",
        RemoteSyncErrorCategory::Remote => "remote exporter",
    }
}

fn persist_automatic_remote_sync_health(
    health_store: &RemoteSyncHealthStore,
    config_store: &RemotesConfigStore,
    step: &AutomaticRemoteSyncWorkerStep,
) -> io::Result<Option<RemotesConfig>> {
    let AutomaticRemoteSyncWorkerStep::Scheduled(tick) = step else {
        return Ok(None);
    };

    // Reconcile before accepting an asynchronous result. A scheduler event is
    // only authoritative for the exact full source pin that is still attached
    // to this host; a removed host or a generation rotation makes the event a
    // harmless late result.
    let before = config_store.load()?;
    health_store.reconcile_configured_hosts(&before)?;
    let record_result: io::Result<()> = if let RemoteSyncSchedulerTick::Attempted {
        host_id,
        config_revision,
        source,
        process_containment_uncertain,
        result,
        next_eligible_in,
        ..
    } = tick
        && before.config_revision() == *config_revision
        && before.auto_sync_enabled()
        && source.as_ref().is_some_and(|attempted_source| {
            before.host(host_id).is_some_and(|host| {
                host.sync_enabled() && host.expected_source() == Some(attempted_source)
            })
        }) {
        let attempted_at = Utc::now();
        let next_eligible_at = TimeDelta::from_std(*next_eligible_in)
            .ok()
            .and_then(|delay| attempted_at.checked_add_signed(delay));
        let configured_host = before
            .host(host_id)
            .expect("the exact attempted automatic host remains configured");
        let result_write = match result {
            Ok(report) if *process_containment_uncertain => health_store
                .record_success_with_process_containment(
                    host_id,
                    source
                        .as_ref()
                        .expect("an automatic containment pause has an exact source pin"),
                    attempted_at,
                    report,
                    configured_host,
                )
                .map(|_| ()),
            Ok(report) => health_store
                .record_success(
                    host_id,
                    source.as_ref(),
                    attempted_at,
                    report,
                    next_eligible_at,
                )
                .map(|_| ()),
            Err(RemoteSyncError::Local(error)) if budget_pause_from_io_error(error).is_some() => {
                let pause = budget_pause_from_io_error(error)
                    .expect("the guarded automatic sync error contains a budget pause");
                if *process_containment_uncertain {
                    health_store
                        .record_pause_with_process_containment(
                            host_id,
                            source
                                .as_ref()
                                .expect("an automatic containment pause has an exact source pin"),
                            attempted_at,
                            pause.level(),
                            pause.resume_at(),
                            configured_host,
                        )
                        .map(|_| ())
                } else {
                    health_store
                        .record_pause(
                            host_id,
                            source.as_ref(),
                            attempted_at,
                            pause.level(),
                            pause.resume_at(),
                        )
                        .map(|_| ())
                }
            }
            Err(error) => health_store
                .record_sync_error_for_config(
                    host_id,
                    source.as_ref(),
                    attempted_at,
                    error,
                    next_eligible_at,
                    configured_host,
                )
                .map(|_| ()),
        };
        result_write?;
        Ok(())
    } else {
        Ok(())
    };

    // Load once more after the write so a config change racing the record wins
    // deterministically. This remains local-only and can never open SSH.
    let post_reconcile = config_store.load().and_then(|current| {
        health_store
            .reconcile_configured_hosts(&current)
            .map(|_| current)
    });
    record_result?;
    post_reconcile.map(Some)
}

fn observe_automatic_remote_sync_step(
    diagnostic: &AutomaticRemoteSyncDiagnostic,
    step: &AutomaticRemoteSyncWorkerStep,
) {
    let mut diagnostic = diagnostic
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match step {
        AutomaticRemoteSyncWorkerStep::Stopped => {
            diagnostic.global = None;
        }
        AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Disabled { .. })
        | AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::NoEligibleHosts {
            ..
        }) => {
            diagnostic.global = None;
            diagnostic.host_failures.clear();
            diagnostic.health_persistence_failed = false;
        }
        AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Waiting { .. }) => {}
        AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
            host_id,
            process_containment_uncertain: true,
            ..
        }) => {
            diagnostic.global = None;
            diagnostic.host_failures.insert(
                host_id.clone(),
                "SSH process containment failed; automatic sync paused".to_owned(),
            );
        }
        AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
            host_id,
            result: Ok(_),
            ..
        }) => {
            diagnostic.global = None;
            diagnostic.host_failures.remove(host_id);
        }
        AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
            host_id,
            result: Err(RemoteSyncError::Local(error)),
            ..
        }) if budget_pause_from_io_error(error).is_some() => {
            diagnostic.global = None;
            diagnostic.host_failures.remove(host_id);
        }
        AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
            host_id,
            result: Err(error),
            ..
        }) => {
            diagnostic.global = None;
            diagnostic.host_failures.insert(
                host_id.clone(),
                automatic_remote_sync_error_category(error).to_owned(),
            );
        }
        AutomaticRemoteSyncWorkerStep::ConfigError { .. } => {
            diagnostic.global = Some("automatic remote sync configuration is unavailable".into());
        }
        AutomaticRemoteSyncWorkerStep::SchedulerError { error, .. } => {
            diagnostic.global = Some(format!(
                "automatic remote sync scheduler failed: {}",
                automatic_remote_sync_error_category(error)
            ));
        }
    }
}

fn automatic_remote_sync_error_category(error: &RemoteSyncError) -> &'static str {
    match error {
        RemoteSyncError::HostNotPaired { .. }
        | RemoteSyncError::HostNotEnabledForAutomaticSync { .. } => "host is not eligible",
        RemoteSyncError::StaleHostSelection { .. }
        | RemoteSyncError::ConfigurationChanged { .. } => "configuration changed",
        RemoteSyncError::InvalidLimits(_) | RemoteSyncError::InvalidStartedAt => {
            "request validation"
        }
        RemoteSyncError::ResponseBudgetExceeded
        | RemoteSyncError::UnboundResponseEnvelope
        | RemoteSyncError::UnexpectedResponse => "response validation",
        RemoteSyncError::PreTransportLocal(_) | RemoteSyncError::Local(_) => "local state",
        RemoteSyncError::Protocol(_) => "protocol",
        RemoteSyncError::ProcessContainment => {
            "SSH process containment failed; automatic sync paused"
        }
        RemoteSyncError::Transport(error) if error.process_containment_uncertain() => {
            "SSH process containment failed; automatic sync paused"
        }
        RemoteSyncError::Transport(_) => "transport",
        RemoteSyncError::Remote(_) => "remote exporter",
    }
}

fn set_automatic_remote_sync_diagnostic(
    diagnostic: &AutomaticRemoteSyncDiagnostic,
    value: Option<String>,
) {
    diagnostic
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .global = value;
}

fn automatic_remote_sync_diagnostic(diagnostic: &AutomaticRemoteSyncDiagnostic) -> Option<String> {
    let diagnostic = diagnostic
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut messages = Vec::new();
    if let Some(global) = diagnostic.global.as_ref() {
        messages.push(global.clone());
    }
    messages.extend(diagnostic.host_failures.iter().map(|(host_id, category)| {
        format!(
            "automatic remote sync host {} failed: {}",
            crate::domain::terminal_safe_text(host_id),
            category
        )
    }));
    if diagnostic.health_persistence_failed {
        messages.push("automatic remote sync health persistence failed".to_owned());
    }
    (!messages.is_empty()).then(|| messages.join("; "))
}

fn combine_recorder_diagnostics(local: Option<&str>, remote: Option<&str>) -> Option<String> {
    match (local, remote) {
        (Some(local), Some(remote)) => Some(format!("{local}; {remote}")),
        (Some(local), None) => Some(local.to_string()),
        (None, Some(remote)) => Some(remote.to_string()),
        (None, None) => None,
    }
}

fn run_recorder(
    config: CollectConfig,
    args: RecordArgs,
    remotes_config_file: Option<PathBuf>,
    perf_log_file: Option<PathBuf>,
) -> Result<i32> {
    validate_service_cutover_contract(
        args.service_cutover_protocol.as_deref(),
        args.service_definition_id.as_deref(),
    )?;
    if let Some(provided_definition_id) = args.service_definition_id.as_deref() {
        let expected = recorder_service_options_for_identity(
            &config,
            &args,
            remotes_config_file.clone(),
            perf_log_file.clone(),
        )?;
        if expected.service_definition_id() != provided_definition_id {
            bail!("service recorder arguments do not match their service definition identity");
        }
    }
    // A manager-started recorder must cross the current-user-global gate
    // before resolving or initializing any history root. During replacement,
    // the exclusive side and durable blocker make eager launchd starts fail
    // closed, including starts aimed at an already-active different history.
    let service_activation_guard = if args.service_cutover_protocol.is_some() {
        let coordination_root = service_coordination_root().map_err(|error| {
            anyhow::anyhow!("could not resolve service activation gate: {error}")
        })?;
        acquire_service_recorder_activation_gate(&coordination_root)?
    } else {
        None
    };
    let termination_signal = RemoteCommandTerminationSignal::install(true)?;
    let recorder_stop = termination_signal.cancellation().map_or_else(
        AutomaticRemoteSyncStopToken::default,
        AutomaticRemoteSyncStopToken::with_cancellation,
    );
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
    let _recorder_instance_lock = match try_acquire_recorder_instance_lock(&history_dir)
        .map_err(|error| anyhow::anyhow!("could not acquire the recorder instance lock: {error}"))?
    {
        TryRecorderInstanceLock::Acquired(guard) => guard,
        TryRecorderInstanceLock::Busy => {
            bail!(
                "another recorder already owns this history state; stop the existing foreground recorder or service before starting another"
            );
        }
    };
    // The history-specific singleton now prevents a competing recorder from
    // entering this root. Release the global startup gate before HistoryRuntime
    // acquires its own source-aware activation gate (the process-local lock
    // registry intentionally rejects nested acquisitions of the same inode).
    drop(service_activation_guard);
    let requested_status_file = args.status_file.map(absolute_path);
    let project_mapping_store = args
        .service_project_mapping_file
        .map(absolute_path)
        .map(ProjectMappingStore::new)
        .unwrap_or_else(ProjectMappingStore::discover);
    let mut history_runtime = HistoryRuntime::new_with_project_mapping_store(
        history_dir,
        &config.codex_home,
        config.redact_content,
        project_mapping_store.clone(),
    )
    .map_err(|error| anyhow::anyhow!("could not bind recorder history runtime: {error}"))?;
    let default_recorder_status = default_status_file(
        history_runtime
            .legacy_history()
            .history_root()
            .expect("a bound recorder runtime always has a legacy history root"),
    );
    let status_file = requested_status_file.unwrap_or_else(|| default_recorder_status.clone());
    let mut compatibility_status_files = vec![default_recorder_status];
    if compatibility_status_files[0] != status_file {
        compatibility_status_files.push(status_file.clone());
    }
    let history_profile_lease = acquire_runtime_profile_lease(&history_runtime).map_err(|error| {
        anyhow::anyhow!(
            "could not select the recorder history profile: {error}; stop the process using the other redaction profile and retry"
        )
    })?;
    reject_incompatible_recorder_before_recorder_cutover(
        &compatibility_status_files,
        &history_runtime,
    )?;
    let active = history_runtime.ensure_v2_active().map_err(|error| {
        anyhow::anyhow!(
            "could not activate source-aware recorder history: {error}; stop any old recorder service, update/reinstall it with the current application, and retry"
        )
    })?;
    history_profile_lease.validate().map_err(|error| {
        anyhow::anyhow!("could not revalidate the recorder history profile: {error}")
    })?;
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
        history_runtime.legacy_history().namespace().to_string(),
        heartbeat_interval_seconds,
    );
    recorder_status
        .bind_source_aware_v2(active.epoch())
        .map_err(|error| anyhow::anyhow!("could not bind recorder ownership status: {error}"))?;
    write_recorder_status(&status_file, &recorder_status)
        .map_err(|error| anyhow::anyhow!("could not initialize recorder status: {error}"))?;
    let remote_sync_diagnostic =
        Arc::new(Mutex::new(AutomaticRemoteSyncDiagnosticState::default()));
    let _remote_sync_worker = start_recorder_remote_sync_worker(
        history_runtime.state_root().to_path_buf(),
        &config,
        &remote_sync_diagnostic,
        remotes_config_file,
        project_mapping_store,
        recorder_stop.clone(),
    );
    let mut local_recorder_issue = None;

    loop {
        if recorder_stop.is_stop_requested() {
            break;
        }
        let now = Instant::now();
        let local_due = now >= next_local;
        let account_due = !config.offline && now >= next_account;
        if local_due || account_due {
            if let Err(error) = history_profile_lease.validate() {
                let failed_at = Utc::now();
                recorder_status.record_error(
                    failed_at,
                    format!("history profile lease validation failed: {error}"),
                );
                let _ = write_recorder_status(&status_file, &recorder_status);
                bail!("recorder history profile lease validation failed: {error}");
            }
            let mut history_persistence_failed = false;
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
                let history_result = history_runtime.record_local_collection_with_session_digests(
                    &result.history_observation,
                    &result.snapshot.tasks,
                    &result.local_session_digests,
                    LocalObservationMode::Incremental,
                );
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
                    history_metrics.shards_written = u64::try_from(
                        report
                            .account
                            .shards_written
                            .saturating_add(report.buckets.shards_written)
                            .saturating_add(report.weekly.shards_written)
                            .saturating_add(report.session_digests.shards_written),
                    )
                    .unwrap_or(u64::MAX);
                    history_metrics.shards_skipped = u64::try_from(
                        report
                            .account
                            .shards_skipped
                            .saturating_add(report.buckets.shards_skipped)
                            .saturating_add(report.weekly.shards_skipped)
                            .saturating_add(report.session_digests.shards_skipped),
                    )
                    .unwrap_or(u64::MAX);
                } else {
                    history_metrics.warnings = 1;
                }
                config.perf_log.record_history(history_metrics);
                match history_result {
                    Ok(_) => {
                        local_recorder_issue = collection_issue;
                    }
                    Err(error) => {
                        local_recorder_issue = Some(format!("history persistence failed: {error}"));
                        history_persistence_failed = true;
                    }
                }
                if let Err(error) = history_profile_lease.validate() {
                    recorder_status.record_error(
                        Utc::now(),
                        format!("history profile lease changed during persistence: {error}"),
                    );
                    let _ = write_recorder_status(&status_file, &recorder_status);
                    bail!("recorder history profile lease changed during persistence: {error}");
                }
            }
            let remote_issue = automatic_remote_sync_diagnostic(&remote_sync_diagnostic);
            let combined_issue = combine_recorder_diagnostics(
                local_recorder_issue.as_deref(),
                remote_issue.as_deref(),
            );
            match combined_issue {
                Some(issue) if history_persistence_failed => {
                    recorder_status.record_error(attempt_at, issue);
                }
                Some(issue) => recorder_status.record_degraded(attempt_at, issue),
                None => recorder_status.record_success(attempt_at),
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
        if recorder_stop.wait_timeout(wake_at.saturating_duration_since(Instant::now())) {
            break;
        }
    }

    Ok(0)
}

fn recorder_service_options_for_identity(
    config: &CollectConfig,
    args: &RecordArgs,
    remotes_config_file: Option<PathBuf>,
    perf_log_file: Option<PathBuf>,
) -> Result<ServiceOptions> {
    let history_dir = args.history_dir.as_ref().ok_or_else(|| {
        anyhow::anyhow!("service recorder identity requires an explicit history directory")
    })?;
    let status_file = args.status_file.as_ref().ok_or_else(|| {
        anyhow::anyhow!("service recorder identity requires an explicit status file")
    })?;
    let mut expected = ServiceOptions::new(
        std::env::current_exe().map_err(anyhow::Error::from)?,
        absolute_path(config.codex_home.clone()),
        absolute_path(history_dir.clone()),
        absolute_path(status_file.clone()),
        perf_log_file.map(absolute_path),
    );
    expected.codex_bin = config.codex_bin.clone().map(absolute_path);
    expected.lookback_days = config.lookback_days;
    expected.max_files = config.max_files;
    expected.active_grace_minutes = config.active_grace.as_secs().div_ceil(60);
    expected.offline = config.offline;
    expected.redact_content = config.redact_content;
    expected.no_rollout_cache = config.rollout_cache_dir.is_none();
    expected.remotes_config_file = remotes_config_file.map(absolute_path);
    expected.project_mapping_file = args.service_project_mapping_file.clone().map(absolute_path);
    #[cfg(windows)]
    {
        expected.environment_path = config
            .app_server_path
            .clone()
            .or_else(|| std::env::var_os("PATH"));
    }
    Ok(expected)
}

fn acquire_service_recorder_activation_gate(
    coordination_root: &Path,
) -> Result<Option<RecorderInstanceLockGuard>> {
    acquire_service_recorder_activation_gate_with(coordination_root, || {
        let observation = current_user_service_definition_observation()
            .map_err(|error| anyhow::anyhow!("could not inspect service definition: {error}"))?;
        ensure_service_definition_is_trusted_at(coordination_root, observation)
            .map_err(|error| anyhow::anyhow!("service definition is not trusted: {error}"))
    })
}

fn acquire_service_recorder_activation_gate_with(
    coordination_root: &Path,
    verify_definition: impl FnOnce() -> Result<()>,
) -> Result<Option<RecorderInstanceLockGuard>> {
    let guard = match try_acquire_service_cutover_shared_at(coordination_root)
        .map_err(|error| anyhow::anyhow!("could not acquire service activation gate: {error}"))?
    {
        TryRecorderInstanceLock::Acquired(guard) => guard,
        TryRecorderInstanceLock::Busy => {
            bail!("service replacement is in progress; recorder activation is fenced")
        }
    };
    ensure_no_recorder_cutover_blocker_at(coordination_root).map_err(|error| {
        anyhow::anyhow!("recorder activation is blocked by service replacement: {error}")
    })?;
    verify_definition()?;
    Ok(Some(guard))
}

fn validate_service_cutover_contract(
    protocol: Option<&str>,
    definition_id: Option<&str>,
) -> Result<()> {
    match (protocol, definition_id) {
        (None, None) => Ok(()),
        (Some(protocol), Some(definition_id)) => {
            if protocol != SERVICE_CUTOVER_PROTOCOL {
                bail!(
                    "unsupported service cutover protocol {protocol:?}; reinstall the background service"
                );
            }
            validate_service_definition_id(definition_id)
        }
        (Some(_), None) => bail!(
            "service cutover protocol is missing its definition identity; reinstall the background service"
        ),
        (None, Some(_)) => bail!(
            "service definition identity is missing its cutover protocol; reinstall the background service"
        ),
    }
}

fn reject_incompatible_recorder_before_recorder_cutover(
    status_paths: &[PathBuf],
    runtime: &HistoryRuntime,
) -> Result<()> {
    let now = Utc::now();
    for status_path in status_paths {
        if let Some(status) = read_recorder_status(status_path).map_err(|error| {
            anyhow::anyhow!(
                "could not verify recorder singleton state from {}: {error}",
                status_path.display()
            )
        })? && status.writer_may_be_active(now)
            && !(status.history_namespace.as_deref() == Some(runtime.legacy_history().namespace())
                && status.source_aware_v2_epoch().is_some())
        {
            let last_activity = status
                .last_history_heartbeat
                .unwrap_or(status.last_attempt_at);
            bail!(
                "another recorder may already be active (pid {}, namespace {}, last activity {}, status {}); stop the existing foreground recorder or service before starting another",
                status.pid,
                status.history_namespace.as_deref().unwrap_or("unknown"),
                last_activity.to_rfc3339(),
                status_path.display(),
            );
        }
        let incompatible = incompatible_recorder_for_cutover(
            status_path,
            runtime.legacy_history().namespace(),
            now,
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "could not verify recorder compatibility from {}: {error}",
                status_path.display()
            )
        })?;
        if let Some(status) = incompatible {
            let last_activity = status
                .last_history_heartbeat
                .unwrap_or(status.last_attempt_at);
            bail!(
                "cannot activate source-aware history while a recent legacy recorder may still be writing namespace {:?} (pid {}, last activity {}, status {}); stop or uninstall the old service, then reinstall it with the current application before retrying",
                runtime.legacy_history().namespace(),
                status.pid,
                last_activity.to_rfc3339(),
                status_path.display(),
            );
        }
    }
    Ok(())
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
        Some(Command::Remote(_)) => "remote",
        Some(Command::DebugStartup(_)) => "debug_startup",
        Some(Command::RemoteAgent(_)) => "remote_agent",
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
        HISTORY_ESTIMATOR_REVISION, HISTORY_FORMAT_VERSION, HISTORY_PROJECT_BREAKDOWN_REVISION,
        LocalHalfHourBucket, LocalProjectUsageGroup, QuotaPoint, WeeklyLocalPoint,
    };

    struct BrokenPipeWriter;

    const SERVICE_CONFIG_TEST_MODE_ENV: &str = "CODEX_USAGE_MONIT_SERVICE_CONFIG_TEST_MODE";
    const SERVICE_CONFIG_TEST_ROOT_ENV: &str = "CODEX_USAGE_MONIT_SERVICE_CONFIG_TEST_ROOT";
    const SERVICE_CONFIG_TEST_PAYLOAD_ENV: &str = "CODEX_USAGE_MONIT_SERVICE_CONFIG_TEST_PAYLOAD";
    const SERVICE_CONFIG_TEST_RESULT_ENV: &str = "CODEX_USAGE_MONIT_SERVICE_CONFIG_TEST_RESULT";

    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ServiceConfigRestartPayload {
        arguments: Vec<String>,
        definition_id: String,
        codex_home: PathBuf,
        history_dir: PathBuf,
        status_file: PathBuf,
        remotes_config_file: PathBuf,
        project_mapping_file: PathBuf,
    }

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn automatic_remote_sync_test_diagnostic() -> AutomaticRemoteSyncDiagnostic {
        Arc::new(Mutex::new(AutomaticRemoteSyncDiagnosticState::default()))
    }

    #[test]
    fn service_start_is_fenced_before_touching_another_v2_history() {
        let coordination_directory = tempfile::tempdir().unwrap();
        let coordination = coordination_directory.path().join("service-scope");
        let state = tempfile::tempdir().unwrap();
        let codex_home = state.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let history = state.path().join("state/history-v1");
        let mapping = ProjectMappingStore::new(state.path().join("project-mappings.json"));
        let mut runtime = HistoryRuntime::new_with_project_mapping_store(
            history.clone(),
            &codex_home,
            false,
            mapping,
        )
        .unwrap();
        runtime.ensure_v2_active().unwrap();

        let _exclusive = match crate::service::try_acquire_service_cutover_exclusive_at(
            &coordination,
        )
        .unwrap()
        {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test replacement gate must be available"),
        };
        let definition_checked = std::cell::Cell::new(false);
        let error = acquire_service_recorder_activation_gate_with(&coordination, || {
            definition_checked.set(true);
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("activation is fenced"));
        assert!(!definition_checked.get());
        assert!(runtime.source_history().state_root().exists());
    }

    #[test]
    fn durable_service_blocker_fences_an_already_active_alternate_history() {
        let coordination_directory = tempfile::tempdir().unwrap();
        let coordination = coordination_directory.path().join("service-scope");
        let state = tempfile::tempdir().unwrap();
        let codex_home = state.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let history = state.path().join("alternate/history-v1");
        let mapping = ProjectMappingStore::new(state.path().join("project-mappings.json"));
        let mut runtime =
            HistoryRuntime::new_with_project_mapping_store(history, &codex_home, false, mapping)
                .unwrap();
        let active = runtime.ensure_v2_active().unwrap();

        // Prepare the private coordination root through the same lock API,
        // then model an earlier replacement whose manager cleanup was
        // ambiguous. V2Active itself fast-paths history activation, so the
        // service-global blocker must be checked before constructing or
        // consulting the requested runtime.
        let prepared =
            match crate::service::try_acquire_service_cutover_shared_at(&coordination).unwrap() {
                TryRecorderInstanceLock::Acquired(guard) => guard,
                TryRecorderInstanceLock::Busy => panic!("test service gate was unexpectedly busy"),
            };
        drop(prepared);
        fs::write(
            coordination.join("recorder-cutover-blocked.json"),
            b"ambiguous automatic-start cleanup\n",
        )
        .unwrap();
        let definition_checked = std::cell::Cell::new(false);

        let error = acquire_service_recorder_activation_gate_with(&coordination, || {
            definition_checked.set(true);
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("activation is blocked"));
        assert!(!definition_checked.get());
        assert_eq!(
            runtime.ownership().load_manifest().unwrap(),
            OwnershipManifestStatus::Initialized(active)
        );
    }

    #[test]
    fn fresh_v1_service_releases_global_gate_before_history_activation() {
        let coordination_directory = tempfile::tempdir().unwrap();
        let coordination = coordination_directory.path().join("service-scope");
        let state = tempfile::tempdir().unwrap();
        let codex_home = state.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let history = state.path().join("state/history-v1");
        let mapping = ProjectMappingStore::new(state.path().join("project-mappings.json"));
        let mut runtime = HistoryRuntime::new_with_project_mapping_store(
            history.clone(),
            &codex_home,
            false,
            mapping,
        )
        .unwrap();
        runtime.set_service_coordination_root_for_test(coordination.clone());
        assert_eq!(
            runtime.ensure_ownership_initialized().unwrap().state(),
            HistoryOwnershipState::V1Active
        );

        let global = acquire_service_recorder_activation_gate_with(&coordination, || Ok(()))
            .unwrap()
            .unwrap();
        let _history_singleton = match try_acquire_recorder_instance_lock(&history).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("fresh history singleton must be available"),
        };

        // HistoryRuntime deliberately takes the same current-user shared gate
        // for the one-way V1 -> V2 transition. Keeping the service-start gate
        // alive here would therefore look busy to this process's inode
        // registry even though both requests are shared.
        assert_eq!(
            runtime.ensure_v2_active().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(global);
        assert!(matches!(
            crate::service::try_acquire_service_cutover_exclusive_at(&coordination).unwrap(),
            TryRecorderInstanceLock::Acquired(_)
        ));
        assert_eq!(
            runtime.ensure_v2_active().unwrap().state(),
            HistoryOwnershipState::V2Active
        );
    }

    #[test]
    fn automatic_remote_sync_disabled_and_no_eligible_hosts_clear_diagnostics() {
        let diagnostic = automatic_remote_sync_test_diagnostic();
        set_automatic_remote_sync_diagnostic(&diagnostic, Some("previous failure".into()));
        observe_automatic_remote_sync_step(
            &diagnostic,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Disabled {
                next_wake_in: Duration::from_secs(60),
            }),
        );
        assert_eq!(automatic_remote_sync_diagnostic(&diagnostic), None);

        set_automatic_remote_sync_diagnostic(&diagnostic, Some("previous failure".into()));
        observe_automatic_remote_sync_step(
            &diagnostic,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::NoEligibleHosts {
                next_wake_in: Duration::from_secs(60),
            }),
        );
        assert_eq!(automatic_remote_sync_diagnostic(&diagnostic), None);
    }

    #[test]
    fn automatic_remote_sync_attempt_diagnostic_uses_only_host_id_and_error_category() {
        let diagnostic = automatic_remote_sync_test_diagnostic();
        let raw_error = "secret.example /Users/private/.ssh/config";
        observe_automatic_remote_sync_step(
            &diagnostic,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "machine-a".to_string(),
                config_revision: 0,
                source: None,
                process_containment_uncertain: false,
                result: Err(RemoteSyncError::Local(io::Error::other(raw_error))),
                next_eligible_in: Duration::from_secs(60),
                next_wake_in: Duration::from_secs(60),
            }),
        );

        let diagnostic = automatic_remote_sync_diagnostic(&diagnostic).unwrap();
        assert!(diagnostic.contains("machine-a"));
        assert!(diagnostic.contains("local state"));
        assert!(!diagnostic.contains("secret.example"));
        assert!(!diagnostic.contains("/Users/private"));
    }

    #[test]
    fn automatic_remote_sync_success_clears_failure_while_waiting_preserves_it() {
        let diagnostic = automatic_remote_sync_test_diagnostic();
        set_automatic_remote_sync_diagnostic(&diagnostic, Some("previous failure".into()));
        observe_automatic_remote_sync_step(
            &diagnostic,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Waiting {
                next_wake_in: Duration::from_secs(60),
            }),
        );
        assert_eq!(
            automatic_remote_sync_diagnostic(&diagnostic).as_deref(),
            Some("previous failure")
        );

        observe_automatic_remote_sync_step(
            &diagnostic,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "machine-a".to_string(),
                config_revision: 0,
                source: None,
                process_containment_uncertain: false,
                result: Ok(RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 0,
                    live_state_changed: false,
                    response_bytes: 256,
                    completion: RemoteSyncCompletion::Complete,
                }),
                next_eligible_in: Duration::from_secs(60),
                next_wake_in: Duration::from_secs(60),
            }),
        );
        assert_eq!(automatic_remote_sync_diagnostic(&diagnostic), None);
    }

    #[test]
    fn automatic_remote_sync_observer_persists_sanitized_health_and_host_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let (config_store, paired) = paired_disabled_remote_config(directory.path());
        let config = config_store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        let config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        let host = config.host("dev").unwrap();
        let source = host.expected_source().unwrap().clone();
        let health_store = RemoteSyncHealthStore::new(directory.path().join("state"));
        let diagnostic = automatic_remote_sync_test_diagnostic();
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "dev".to_owned(),
                config_revision: config.config_revision(),
                source: Some(source.clone()),
                process_containment_uncertain: false,
                result: Ok(RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 4,
                    live_state_changed: false,
                    response_bytes: 512,
                    completion: RemoteSyncCompletion::Complete,
                }),
                next_eligible_in: Duration::from_secs(30),
                next_wake_in: Duration::from_secs(1),
            }),
        );

        let health = health_store.get("dev").unwrap().unwrap();
        assert!(health.configured());
        assert_eq!(health.source(), Some(&source));
        assert_eq!(
            health.last_result(),
            Some(crate::remote_sync_health::RemoteSyncAttemptResult::Success)
        );
        assert_eq!(health.changes_committed(), 4);
        assert_eq!(
            health.next_eligible_at().unwrap() - health.last_attempt_at().unwrap(),
            TimeDelta::seconds(30)
        );
        assert_eq!(automatic_remote_sync_diagnostic(&diagnostic), None);
    }

    #[test]
    fn automatic_health_ignores_late_result_after_same_node_generation_rotation() {
        use std::num::NonZeroU64;

        use crate::remote_protocol::SourceGeneration;

        let directory = tempfile::tempdir().unwrap();
        let (config_store, paired) = paired_disabled_remote_config(directory.path());
        let old_source = paired
            .host("dev")
            .unwrap()
            .expected_source()
            .unwrap()
            .clone();
        let mut config = config_store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        let health_store = RemoteSyncHealthStore::new(directory.path().join("state"));
        let diagnostic = automatic_remote_sync_test_diagnostic();
        let old_attempt_revision = config.config_revision();
        let success = |config_revision, source, changes| {
            AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "dev".to_owned(),
                config_revision,
                source: Some(source),
                process_containment_uncertain: false,
                result: Ok(RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: changes,
                    live_state_changed: false,
                    response_bytes: 256,
                    completion: RemoteSyncCompletion::Complete,
                }),
                next_eligible_in: Duration::from_secs(60),
                next_wake_in: Duration::from_secs(30),
            })
        };
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &success(config.config_revision(), old_source.clone(), 1),
        );

        config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::unpair_host("dev"),
            )
            .unwrap();
        let new_source = SourceGeneration {
            node_id: old_source.node_id.clone(),
            generation: NonZeroU64::new(old_source.generation.get() + 1).unwrap(),
        };
        config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::pair_pin("dev", new_source.clone()),
            )
            .unwrap();
        config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &success(config.config_revision(), new_source.clone(), 8),
        );
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "dev".to_owned(),
                config_revision: old_attempt_revision,
                source: Some(old_source),
                process_containment_uncertain: false,
                result: Err(RemoteSyncError::InvalidStartedAt),
                next_eligible_in: Duration::from_secs(60),
                next_wake_in: Duration::from_secs(30),
            }),
        );

        let health = health_store.get("dev").unwrap().unwrap();
        assert_eq!(health.source(), Some(&new_source));
        assert_eq!(health.last_result(), Some(RemoteSyncAttemptResult::Success));
        assert_eq!(health.changes_committed(), 8);
        assert_eq!(health.consecutive_failures(), 0);
        assert_eq!(automatic_remote_sync_diagnostic(&diagnostic), None);
    }

    #[test]
    fn automatic_health_ignores_late_result_after_global_disable() {
        let directory = tempfile::tempdir().unwrap();
        let (config_store, paired) = paired_disabled_remote_config(directory.path());
        let source = paired
            .host("dev")
            .unwrap()
            .expected_source()
            .unwrap()
            .clone();
        let mut enabled = config_store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        enabled = config_store
            .update(
                enabled.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        let attempted_revision = enabled.config_revision();
        config_store
            .update(
                attempted_revision,
                RemotesConfigMutation::set_auto_sync_enabled(false),
            )
            .unwrap();
        let health_store = RemoteSyncHealthStore::new(directory.path().join("state"));
        let diagnostic = automatic_remote_sync_test_diagnostic();
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "dev".to_owned(),
                config_revision: attempted_revision,
                source: Some(source),
                process_containment_uncertain: false,
                result: Ok(RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 9,
                    live_state_changed: false,
                    response_bytes: 512,
                    completion: RemoteSyncCompletion::Complete,
                }),
                next_eligible_in: Duration::from_secs(60),
                next_wake_in: Duration::from_secs(30),
            }),
        );

        let health = health_store.get("dev").unwrap().unwrap();
        assert_eq!(health.last_attempt_at(), None);
        assert_eq!(health.changes_committed(), 0);
    }

    #[test]
    fn automatic_budget_pause_is_persisted_without_failure_or_diagnostic() {
        use crate::remote_bandwidth_budget::RemoteBandwidthBudgetPausedError;

        let directory = tempfile::tempdir().unwrap();
        let (config_store, paired) = paired_disabled_remote_config(directory.path());
        let source = paired
            .host("dev")
            .unwrap()
            .expected_source()
            .unwrap()
            .clone();
        let config = config_store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        let config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        let state_root = directory.path().join("state");
        let budget = RemoteBandwidthBudgetStore::new(state_root.clone());
        let now = Utc::now();
        let RemoteBandwidthAdmission::Granted(reservation) = budget
            .begin_attempt(
                "dev",
                Some(&source.node_id),
                now,
                RemoteBandwidthTransferKind::ManualOverride,
                1,
            )
            .unwrap()
        else {
            panic!("manual override must reserve");
        };
        budget
            .complete_attempt(
                &reservation,
                now,
                usize::try_from(crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES)
                    .unwrap(),
            )
            .unwrap();
        let RemoteBandwidthAdmission::Paused(pause) = budget
            .begin_attempt(
                "dev",
                Some(&source.node_id),
                now,
                RemoteBandwidthTransferKind::AutomaticBulk,
                1024,
            )
            .unwrap()
        else {
            panic!("automatic bulk must pause at the soft cap");
        };
        let resume_at = pause.resume_at();
        let step = AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
            host_id: "dev".to_owned(),
            config_revision: config.config_revision(),
            source: Some(source.clone()),
            process_containment_uncertain: false,
            result: Err(RemoteSyncError::Local(io::Error::new(
                io::ErrorKind::WouldBlock,
                RemoteBandwidthBudgetPausedError::new(pause),
            ))),
            next_eligible_in: Duration::from_secs(120),
            next_wake_in: Duration::from_secs(30),
        });
        let health_store = RemoteSyncHealthStore::new(state_root);
        let diagnostic = automatic_remote_sync_test_diagnostic();
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &step,
        );

        let health = health_store.get("dev").unwrap().unwrap();
        assert_eq!(health.source(), Some(&source));
        assert!(health.budget_paused());
        assert_eq!(health.budget_resume_at(), resume_at);
        assert_eq!(health.last_result(), None);
        assert_eq!(health.consecutive_failures(), 0);
        assert_eq!(health.error_category(), None);
        assert_eq!(automatic_remote_sync_diagnostic(&diagnostic), None);
    }

    #[test]
    fn automatic_remote_diagnostics_keep_other_host_failures_until_disabled() {
        use std::num::NonZeroU64;

        use crate::remote_protocol::SourceGeneration;

        let directory = tempfile::tempdir().unwrap();
        let config_store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let mut config = config_store.load_or_create().unwrap();
        for (host_id, node_id) in [
            ("alpha", "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ("beta", "node-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ] {
            config = config_store
                .update(
                    config.config_revision(),
                    RemotesConfigMutation::add_host(host_id, format!("{host_id}-secret-alias")),
                )
                .unwrap();
            config = config_store
                .update(
                    config.config_revision(),
                    RemotesConfigMutation::pair_pin(
                        host_id,
                        SourceGeneration {
                            node_id: node_id.parse().unwrap(),
                            generation: NonZeroU64::new(1).unwrap(),
                        },
                    ),
                )
                .unwrap();
            config = config_store
                .update(
                    config.config_revision(),
                    RemotesConfigMutation::enable_host(host_id),
                )
                .unwrap();
        }
        config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        let alpha_source = config
            .host("alpha")
            .unwrap()
            .expected_source()
            .unwrap()
            .clone();
        let beta_source = config
            .host("beta")
            .unwrap()
            .expected_source()
            .unwrap()
            .clone();
        let health_store = RemoteSyncHealthStore::new(directory.path().join("state"));
        let diagnostic = automatic_remote_sync_test_diagnostic();

        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "alpha".to_owned(),
                config_revision: config.config_revision(),
                source: Some(alpha_source),
                process_containment_uncertain: false,
                result: Err(RemoteSyncError::InvalidStartedAt),
                next_eligible_in: Duration::from_secs(30),
                next_wake_in: Duration::from_secs(1),
            }),
        );
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "beta".to_owned(),
                config_revision: config.config_revision(),
                source: Some(beta_source),
                process_containment_uncertain: false,
                result: Ok(RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 0,
                    live_state_changed: false,
                    response_bytes: 256,
                    completion: RemoteSyncCompletion::Complete,
                }),
                next_eligible_in: Duration::from_secs(60),
                next_wake_in: Duration::from_secs(30),
            }),
        );
        let combined = automatic_remote_sync_diagnostic(&diagnostic).unwrap();
        assert!(combined.contains("alpha"));
        assert!(!combined.contains("beta"));
        assert!(!combined.contains("secret-alias"));

        let config = config_store.load().unwrap();
        config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::disable_host("alpha"),
            )
            .unwrap();
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Waiting {
                next_wake_in: Duration::from_secs(30),
            }),
        );
        assert_eq!(automatic_remote_sync_diagnostic(&diagnostic), None);
    }

    #[cfg(unix)]
    #[test]
    fn automatic_health_failure_only_degrades_the_sanitized_diagnostic() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let (config_store, config) = paired_disabled_remote_config(directory.path());
        let source = config
            .host("dev")
            .unwrap()
            .expected_source()
            .unwrap()
            .clone();
        let state_root = directory.path().join("state");
        fs::create_dir(&state_root).unwrap();
        fs::set_permissions(&state_root, fs::Permissions::from_mode(0o755)).unwrap();
        let health_store = RemoteSyncHealthStore::new(state_root);
        let diagnostic = automatic_remote_sync_test_diagnostic();
        observe_automatic_remote_sync_step_with_health(
            &diagnostic,
            &health_store,
            &config_store,
            &AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
                host_id: "dev".to_owned(),
                config_revision: config.config_revision(),
                source: Some(source),
                process_containment_uncertain: false,
                result: Ok(RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 0,
                    live_state_changed: false,
                    response_bytes: 256,
                    completion: RemoteSyncCompletion::Complete,
                }),
                next_eligible_in: Duration::from_secs(60),
                next_wake_in: Duration::from_secs(60),
            }),
        );

        assert_eq!(
            automatic_remote_sync_diagnostic(&diagnostic).as_deref(),
            Some("automatic remote sync health persistence failed")
        );
    }

    #[test]
    fn recorder_diagnostics_keep_local_and_remote_failures_distinct() {
        assert_eq!(
            combine_recorder_diagnostics(Some("local failure"), Some("remote failure")).as_deref(),
            Some("local failure; remote failure")
        );
        assert_eq!(
            combine_recorder_diagnostics(Some("local failure"), None).as_deref(),
            Some("local failure")
        );
        assert_eq!(
            combine_recorder_diagnostics(None, Some("remote failure")).as_deref(),
            Some("remote failure")
        );
    }

    #[test]
    fn recorder_remote_worker_guard_stops_and_joins_its_thread() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = AutomaticRemoteSyncStopToken::default();
        let worker_stop = stop.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let worker = thread::spawn(move || {
            while !worker_stop.is_stop_requested() {
                thread::yield_now();
            }
            worker_exited.store(true, Ordering::SeqCst);
        });
        let guard = RecorderRemoteSyncWorkerGuard {
            stop,
            worker: Some(worker),
        };

        drop(guard);

        assert!(exited.load(Ordering::SeqCst));
    }

    #[test]
    fn recorder_remote_worker_guard_never_joins_a_stuck_worker_without_a_bound() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = AutomaticRemoteSyncStopToken::default();
        let release = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_release = Arc::clone(&release);
        let worker_exited = Arc::clone(&exited);
        let worker = thread::spawn(move || {
            while !worker_release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            worker_exited.store(true, Ordering::Release);
        });
        let guard = RecorderRemoteSyncWorkerGuard {
            stop,
            worker: Some(worker),
        };

        let started = Instant::now();
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a stuck worker made recorder Drop exceed its join grace"
        );

        release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !exited.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(exited.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn recorder_signal_cancels_automatic_ssh_and_reaps_its_descendant() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;

        let directory = tempfile::tempdir().unwrap();
        let ssh_path = directory.path().join("ssh");
        let descendant_pid_path = directory.path().join("descendant.pid");
        fs::write(
            &ssh_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nsleep 30 &\necho $! > '{}'\nwait\n",
                descendant_pid_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh_path, fs::Permissions::from_mode(0o700)).unwrap();

        let termination = RemoteCommandTerminationSignal::for_test();
        let cancellation = termination.cancellation().unwrap();
        let stop = AutomaticRemoteSyncStopToken::with_cancellation(Arc::clone(&cancellation));
        let mut config = CollectConfig::default();
        let mut search_path = vec![directory.path().to_path_buf()];
        search_path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        config.app_server_path = Some(std::env::join_paths(search_path).unwrap());
        let transport_cancellation = stop.cancellation_flag();
        let environment = remote_ssh_environment(&config, false, Some(&transport_cancellation));
        let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = probe_remote_with_environment(
                "dev-server",
                &RemoteProbeOptions {
                    timeout: Duration::from_secs(5),
                    ..RemoteProbeOptions::default()
                },
                &environment,
            );
            let _ = result_sender.send(result);
        });
        let guard = RecorderRemoteSyncWorkerGuard {
            stop: stop.clone(),
            worker: Some(worker),
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        let descendant_pid = loop {
            if let Ok(pid) = fs::read_to_string(&descendant_pid_path)
                && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "fake SSH did not publish its descendant PID"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, 0);

        termination.request_for_test();
        assert!(cancellation.load(Ordering::Acquire));
        assert!(stop.wait_timeout(Duration::from_secs(1)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !guard.worker.as_ref().unwrap().is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            guard.worker.as_ref().unwrap().is_finished(),
            "recorder termination did not cancel the automatic SSH exchange"
        );
        assert!(matches!(
            result_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            Err(crate::remote_transport::RemoteTransportError::Cancelled { .. })
        ));
        drop(guard);

        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(descendant_pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[cfg(unix)]
    #[test]
    fn remote_termination_signal_subprocess_helper() {
        const HELPER_ENV: &str = "CODEX_USAGE_MONIT_SIGNAL_TEST_HELPER";
        const OBSERVED_ENV: &str = "CODEX_USAGE_MONIT_SIGNAL_TEST_OBSERVED";
        const READY_ENV: &str = "CODEX_USAGE_MONIT_SIGNAL_TEST_READY";

        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let ready = PathBuf::from(std::env::var_os(READY_ENV).unwrap());
        let observed = PathBuf::from(std::env::var_os(OBSERVED_ENV).unwrap());
        let termination = RemoteCommandTerminationSignal::install(true).unwrap();
        let cancellation = termination.cancellation().unwrap();
        let observer = thread::spawn(move || {
            while !cancellation.load(std::sync::atomic::Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            fs::write(observed, b"observed\n").unwrap();
        });
        fs::write(ready, b"ready\n").unwrap();

        // Deliberately model a recorder startup/local-collection phase which
        // cannot poll the cooperative flag. The parent test proves the first
        // SIGTERM is graceful and the second restores the OS default action.
        thread::sleep(Duration::from_secs(30));
        let _ = observer.join();
        drop(termination);
    }

    #[cfg(unix)]
    #[test]
    fn second_real_termination_signal_forces_a_blocked_process_to_exit() {
        use std::os::unix::process::ExitStatusExt as _;
        use std::process::{Command, Stdio};

        const HELPER_ENV: &str = "CODEX_USAGE_MONIT_SIGNAL_TEST_HELPER";
        const OBSERVED_ENV: &str = "CODEX_USAGE_MONIT_SIGNAL_TEST_OBSERVED";
        const READY_ENV: &str = "CODEX_USAGE_MONIT_SIGNAL_TEST_READY";

        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let observed = directory.path().join("observed");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::tests::remote_termination_signal_subprocess_helper",
                "--nocapture",
            ])
            .env(HELPER_ENV, "1")
            .env(READY_ENV, &ready)
            .env(OBSERVED_ENV, &observed)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready.is_file() && Instant::now() < ready_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let helper_started = ready.is_file();
        let first_signal_sent =
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } == 0;
        let observed_deadline = Instant::now() + Duration::from_secs(2);
        while !observed.is_file() && Instant::now() < observed_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let first_was_observed = observed.is_file();
        let alive_after_first = child.try_wait().unwrap().is_none();
        let second_signal_sent =
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } == 0;
        let exit_deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= exit_deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        if status.is_none() {
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
            }
            let _ = child.wait();
        }

        assert!(helper_started, "signal helper did not become ready");
        assert!(first_signal_sent);
        assert!(first_was_observed, "first SIGTERM did not arm cancellation");
        assert!(alive_after_first, "first SIGTERM was not graceful");
        assert!(second_signal_sent);
        assert_eq!(
            status.and_then(|status| status.signal()),
            Some(libc::SIGTERM),
            "second SIGTERM did not restore the default termination action"
        );
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
                source: HistorySourceSelector::AllIncluded,
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
                source: HistorySourceSelector::AllIncluded,
                history_dir: Some(path),
            })) if path == Path::new("state with spaces/history")
        ));

        for invalid in ["--day-offset=-1", "--day-offset=8", "--day-offset=65536"] {
            let error = Cli::try_parse_from(["codex-usage-monit", "trends", invalid]).unwrap_err();
            assert!(error.use_stderr(), "{invalid} should be a usage error");
        }
    }

    #[test]
    fn summary_and_trends_parse_exact_history_source_without_accepting_ssh_aliases() {
        const REMOTE: &str = "node-0123456789abcdef0123456789abcdef";
        for command in ["summary", "trends"] {
            for (value, expected) in [
                ("all", HistorySourceSelector::AllIncluded),
                ("local", HistorySourceSelector::Local),
                (
                    REMOTE,
                    HistorySourceSelector::Remote(REMOTE.parse().unwrap()),
                ),
            ] {
                let cli =
                    Cli::try_parse_from(["codex-usage-monit", command, "--source", value]).unwrap();
                let parsed = match cli.command.unwrap() {
                    Command::Summary(args) => args.source,
                    Command::Trends(args) => args.source,
                    _ => unreachable!(),
                };
                assert_eq!(parsed, expected);
            }
            assert!(
                Cli::try_parse_from(["codex-usage-monit", command, "--source", "prod-server",])
                    .is_err()
            );
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
            "--service-remotes-config",
            "config with spaces/remotes.json",
            "record",
            "--foreground",
            "--service-project-mapping-file",
            "config with spaces/project-mappings.json",
            "--history-dir",
            "state with spaces/history",
            "--status-file",
            "state with spaces/recorder-status.json",
        ])
        .unwrap();
        assert_eq!(record.codex_bin, Some(codex_bin));
        assert_eq!(record.service_path, Some(service_path));
        assert_eq!(
            record.service_remotes_config,
            Some(PathBuf::from("config with spaces/remotes.json"))
        );
        assert!(matches!(
            record.command,
            Some(Command::Record(RecordArgs {
                foreground: true,
                service_project_mapping_file: Some(ref mapping),
                history_dir: Some(_),
                status_file: Some(_),
                ..
            })) if mapping == Path::new("config with spaces/project-mappings.json")
        ));

        for action in ["install", "status", "uninstall"] {
            let service = Cli::try_parse_from(["codex-usage-monit", "service", action]).unwrap();
            assert!(matches!(service.command, Some(Command::Service(_))));
        }
    }

    #[test]
    fn recorder_service_protocol_is_fail_closed_for_unknown_versions() {
        let definition_id = "a".repeat(64);
        validate_service_cutover_contract(None, None).unwrap();
        validate_service_cutover_contract(Some(SERVICE_CUTOVER_PROTOCOL), Some(&definition_id))
            .unwrap();
        assert!(
            validate_service_cutover_contract(Some("legacy-v1"), Some(&definition_id))
                .unwrap_err()
                .to_string()
                .contains("reinstall")
        );
        assert!(validate_service_cutover_contract(Some(SERVICE_CUTOVER_PROTOCOL), None).is_err());
        assert!(validate_service_cutover_contract(None, Some(&definition_id)).is_err());
        for invalid in ["A".repeat(64), "g".repeat(64), "a".repeat(63)] {
            assert!(
                validate_service_cutover_contract(Some(SERVICE_CUTOVER_PROTOCOL), Some(&invalid))
                    .is_err()
            );
        }
    }

    #[test]
    fn service_definition_freezes_custom_config_paths_across_restarts() {
        if let Some(mode) = std::env::var_os(SERVICE_CONFIG_TEST_MODE_ENV) {
            let payload_path = PathBuf::from(
                std::env::var_os(SERVICE_CONFIG_TEST_PAYLOAD_ENV)
                    .expect("service config child requires a payload path"),
            );
            match mode.to_string_lossy().as_ref() {
                "install" => {
                    let root = PathBuf::from(
                        std::env::var_os(SERVICE_CONFIG_TEST_ROOT_ENV)
                            .expect("install child requires a test root"),
                    );
                    let codex_home = root.join("Codex Home");
                    let history_dir = root.join("State Dir/history-v1");
                    let status_file = root.join("State Dir/recorder-status.json");
                    fs::create_dir_all(&codex_home).unwrap();
                    let mut config = CollectConfig {
                        codex_home: codex_home.clone(),
                        offline: true,
                        ..CollectConfig::default()
                    };
                    config.rollout_cache_dir = None;

                    let first = build_service_options(
                        &config,
                        history_dir.clone(),
                        status_file.clone(),
                        None,
                    )
                    .unwrap();
                    let second = build_service_options(
                        &config,
                        history_dir.clone(),
                        status_file.clone(),
                        None,
                    )
                    .unwrap();
                    assert_eq!(first, second);
                    let remotes_config_file = root.join("Custom Config/remotes.json");
                    let project_mapping_file = root.join("Custom Config/project-mappings.json");
                    assert_eq!(
                        first.remotes_config_file.as_deref(),
                        Some(remotes_config_file.as_path())
                    );
                    assert_eq!(
                        first.project_mapping_file.as_deref(),
                        Some(project_mapping_file.as_path())
                    );
                    let arguments = first
                        .recorder_arguments()
                        .into_iter()
                        .map(|argument| {
                            argument
                                .into_string()
                                .expect("validated service arguments are representable")
                        })
                        .collect();
                    let payload = ServiceConfigRestartPayload {
                        arguments,
                        definition_id: first.service_definition_id(),
                        codex_home,
                        history_dir,
                        status_file,
                        remotes_config_file,
                        project_mapping_file,
                    };
                    fs::write(&payload_path, serde_json::to_vec(&payload).unwrap()).unwrap();
                }
                "restart" => {
                    assert!(
                        std::env::var_os("CODEX_USAGE_MONIT_CONFIG_DIR").is_none(),
                        "a manager restart must not depend on the install shell environment"
                    );
                    let payload: ServiceConfigRestartPayload =
                        serde_json::from_slice(&fs::read(&payload_path).unwrap()).unwrap();
                    let cli = Cli::try_parse_from(
                        std::iter::once("codex-usage-monit".to_string())
                            .chain(payload.arguments.iter().cloned()),
                    )
                    .unwrap();
                    assert_eq!(
                        cli.service_remotes_config.as_deref(),
                        Some(payload.remotes_config_file.as_path())
                    );

                    let config = CollectConfig {
                        codex_home: cli.codex_home.clone().unwrap(),
                        codex_bin: cli.codex_bin.clone(),
                        app_server_path: cli.service_path.clone(),
                        lookback_days: cli.days.max(1),
                        max_files: cli.max_files.max(1),
                        active_grace: active_grace(cli.active_grace_minutes),
                        offline: cli.offline,
                        redact_content: cli.redact_content,
                        rollout_cache_dir: (!cli.no_rollout_cache)
                            .then(crate::cache::default_rollout_cache_dir)
                            .flatten(),
                        ..CollectConfig::default()
                    };
                    let Some(Command::Record(record)) = cli.command.as_ref() else {
                        panic!("service definition did not parse as record")
                    };
                    assert_eq!(
                        record.service_definition_id.as_deref(),
                        Some(payload.definition_id.as_str())
                    );
                    assert_eq!(
                        record.service_project_mapping_file.as_deref(),
                        Some(payload.project_mapping_file.as_path())
                    );
                    assert_eq!(
                        record.history_dir.as_deref(),
                        Some(payload.history_dir.as_path())
                    );
                    assert_eq!(
                        record.status_file.as_deref(),
                        Some(payload.status_file.as_path())
                    );

                    let expected = recorder_service_options_for_identity(
                        &config,
                        record,
                        cli.service_remotes_config.clone(),
                        cli.perf_log.clone(),
                    )
                    .unwrap();
                    assert_eq!(expected.service_definition_id(), payload.definition_id);
                    assert_eq!(
                        RemotesConfigStore::new(cli.service_remotes_config.clone().unwrap()).path(),
                        Some(payload.remotes_config_file.as_path())
                    );
                    assert_eq!(
                        ProjectMappingStore::new(
                            record.service_project_mapping_file.clone().unwrap()
                        )
                        .path(),
                        Some(payload.project_mapping_file.as_path())
                    );
                    fs::write(
                        std::env::var_os(SERVICE_CONFIG_TEST_RESULT_ENV).unwrap(),
                        expected.service_definition_id(),
                    )
                    .unwrap();
                }
                mode => panic!("unknown service config child mode {mode}"),
            }
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root with spaces");
        let config_dir = root.join("Custom Config");
        let payload = directory.path().join("service-definition.json");
        let test_name =
            "cli::tests::service_definition_freezes_custom_config_paths_across_restarts";
        let run_child = |mode: &str, result: Option<&Path>| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
                .env(SERVICE_CONFIG_TEST_MODE_ENV, mode)
                .env(SERVICE_CONFIG_TEST_ROOT_ENV, &root)
                .env(SERVICE_CONFIG_TEST_PAYLOAD_ENV, &payload)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if mode == "install" {
                command.env("CODEX_USAGE_MONIT_CONFIG_DIR", &config_dir);
            } else {
                command.env_remove("CODEX_USAGE_MONIT_CONFIG_DIR");
                command.env(SERVICE_CONFIG_TEST_RESULT_ENV, result.unwrap());
            }
            assert!(command.status().unwrap().success(), "{mode} child failed");
        };

        run_child("install", None);
        let installed: ServiceConfigRestartPayload =
            serde_json::from_slice(&fs::read(&payload).unwrap()).unwrap();
        let first_restart = directory.path().join("restart-1.id");
        let second_restart = directory.path().join("restart-2.id");
        run_child("restart", Some(&first_restart));
        run_child("restart", Some(&second_restart));
        assert_eq!(
            fs::read_to_string(first_restart).unwrap(),
            installed.definition_id
        );
        assert_eq!(
            fs::read_to_string(second_restart).unwrap(),
            installed.definition_id
        );
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
    fn direct_cli_cannot_inherit_a_remote_process_tree_with_the_hidden_flag_alone() {
        let help = Cli::try_parse_from(["codex-usage-monit", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!help.contains("inherit-remote-process-tree"));

        assert!(
            Cli::try_parse_from([
                "codex-usage-monit",
                "--inherit-remote-process-tree",
                "remote",
                "test",
                "devbox",
            ])
            .is_err(),
            "the hidden inheritance request must require a TUI capability value"
        );

        let token = "a".repeat(64);
        let cli = Cli::try_parse_from([
            "codex-usage-monit",
            "--inherit-remote-process-tree",
            &token,
            "remote",
            "test",
            "devbox",
        ])
        .unwrap();
        assert_eq!(
            cli.inherit_remote_process_tree.as_deref(),
            Some(token.as_str())
        );
        assert!(matches!(cli.command, Some(Command::Remote(_))));

        assert!(
            !crate::remote_transport::validate_tui_process_tree_inheritance(
                cli.inherit_remote_process_tree.as_deref(),
                None,
                None,
                Some(42),
                true,
            )
        );
        assert!(remote_ssh_environment(&CollectConfig::default(), false, None).owns_process_tree());
    }

    #[test]
    fn tui_remote_process_tree_contract_requires_token_parent_and_containment() {
        let token = "b".repeat(64);
        let parent = OsString::from("42");
        let environment_token = OsString::from(&token);
        let mismatched_environment_token = OsString::from("c".repeat(64));
        assert!(
            crate::remote_transport::validate_tui_process_tree_inheritance(
                Some(&token),
                Some(environment_token.as_os_str()),
                Some(parent.as_os_str()),
                Some(42),
                true,
            )
        );
        assert!(
            !crate::remote_transport::validate_tui_process_tree_inheritance(
                Some(&token),
                Some(mismatched_environment_token.as_os_str()),
                Some(parent.as_os_str()),
                Some(42),
                true,
            )
        );
        assert!(
            !crate::remote_transport::validate_tui_process_tree_inheritance(
                Some(&token),
                Some(environment_token.as_os_str()),
                Some(parent.as_os_str()),
                Some(41),
                true,
            )
        );
        assert!(
            !crate::remote_transport::validate_tui_process_tree_inheritance(
                Some(&token),
                Some(environment_token.as_os_str()),
                Some(parent.as_os_str()),
                Some(42),
                false,
            )
        );
        assert!(!remote_ssh_environment(&CollectConfig::default(), true, None).owns_process_tree());
        assert!(remote_ssh_environment(&CollectConfig::default(), false, None).owns_process_tree());
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
    fn remote_agent_identity_commands_are_hidden_and_parse_explicitly() {
        let help = Cli::try_parse_from(["codex-usage-monit", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!help.contains("remote-agent"));

        for (action, expected) in [
            ("export", RemoteAgentAction::Export),
            ("node-id", RemoteAgentAction::NodeId),
            ("rotate-node-id", RemoteAgentAction::RotateNodeId),
        ] {
            let cli = Cli::try_parse_from(["codex-usage-monit", "remote-agent", action]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::RemoteAgent(RemoteAgentArgs { action }))
                    if std::mem::discriminant(&action) == std::mem::discriminant(&expected)
            ));
        }
    }

    #[test]
    fn remote_agent_identity_output_is_bounded_machine_readable_json() {
        let directory = tempfile::tempdir().unwrap();
        let identity_path = directory.path().join("state/identity.json");
        let identity = SourceIdentityStore::at_path(identity_path.clone())
            .load_or_create()
            .unwrap();
        let output = render_source_identity(&identity).unwrap();
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(json["nodeId"], identity.node_id().as_str());
        assert_eq!(json["generation"], identity.generation());
        assert!(json.get("projectKeySecret").is_none());
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(identity_path).unwrap()).unwrap();
        let secret = persisted["projectKeySecret"].as_str().unwrap();
        assert!(!output.contains(secret));
        assert!(output.len() < 256);
    }

    #[test]
    fn remote_cli_exposes_only_explicit_per_host_actions() {
        let help = Cli::try_parse_from(["codex-usage-monit", "remote", "--help"])
            .unwrap_err()
            .to_string();
        for action in [
            "config", "add", "edit", "list", "pair", "unpair", "test", "sync", "enable", "disable",
            "remove", "source",
        ] {
            assert!(help.contains(action), "missing remote action {action}");
        }
        assert!(!help.contains("sync --all"));

        let add = Cli::try_parse_from([
            "codex-usage-monit",
            "remote",
            "add",
            "dev",
            "--ssh-host",
            "dev-server",
            "--agent-executable",
            "~/.local/bin/codex-usage-monit",
            "--redact-content",
            "false",
        ])
        .unwrap();
        assert!(matches!(
            add.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Add(RemoteAddArgs {
                    id,
                    ssh_host,
                    agent_executable: Some(agent_executable),
                    redact_content: Some(false),
                }),
                ..
            })) if id == "dev"
                && ssh_host == "dev-server"
                && agent_executable == "~/.local/bin/codex-usage-monit"
        ));

        let edit = Cli::try_parse_from([
            "codex-usage-monit",
            "remote",
            "edit",
            "dev",
            "--agent-executable",
            "/home/ubuntu/.local/bin/codex-usage-monit",
        ])
        .unwrap();
        assert!(matches!(
            edit.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Edit(RemoteEditArgs {
                    id,
                    ssh_host: None,
                    agent_executable: Some(agent_executable),
                    redact_content: None,
                }),
                ..
            })) if id == "dev"
                && agent_executable == "/home/ubuntu/.local/bin/codex-usage-monit"
        ));

        let unpair = Cli::try_parse_from(["codex-usage-monit", "remote", "unpair", "dev"]).unwrap();
        assert!(matches!(
            unpair.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Unpair(RemoteHostArgs { id }),
                ..
            })) if id == "dev"
        ));

        let remove = Cli::try_parse_from(["codex-usage-monit", "remote", "remove", "dev"]).unwrap();
        assert!(matches!(
            remove.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Remove(RemoteRemoveArgs {
                    id,
                    keep_included: false,
                }),
                ..
            })) if id == "dev"
        ));
        let remove = Cli::try_parse_from([
            "codex-usage-monit",
            "remote",
            "remove",
            "dev",
            "--keep-included",
        ])
        .unwrap();
        assert!(matches!(
            remove.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Remove(RemoteRemoveArgs {
                    id,
                    keep_included: true,
                }),
                ..
            })) if id == "dev"
        ));

        let source_id = "node-0123456789abcdef0123456789abcdef";
        for (action, include) in [("include", true), ("exclude", false)] {
            let cli =
                Cli::try_parse_from(["codex-usage-monit", "remote", "source", action, source_id])
                    .unwrap();
            let parsed = match cli.command.unwrap() {
                Command::Remote(RemoteArgs {
                    action:
                        RemoteAction::Source(RemoteSourceArgs {
                            action: RemoteSourceAction::Include(args),
                        }),
                    ..
                }) => {
                    assert!(include);
                    args.source_id
                }
                Command::Remote(RemoteArgs {
                    action:
                        RemoteAction::Source(RemoteSourceArgs {
                            action: RemoteSourceAction::Exclude(args),
                        }),
                    ..
                }) => {
                    assert!(!include);
                    args.source_id
                }
                _ => unreachable!(),
            };
            assert_eq!(parsed.as_str(), source_id);
        }
        let purge =
            Cli::try_parse_from(["codex-usage-monit", "remote", "source", "purge", source_id])
                .unwrap();
        assert!(matches!(
            purge.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Source(RemoteSourceArgs {
                    action: RemoteSourceAction::Purge(RemoteSourceIdArgs { source_id: parsed }),
                }),
                ..
            })) if parsed.as_str() == source_id
        ));
        assert!(
            Cli::try_parse_from([
                "codex-usage-monit",
                "remote",
                "source",
                "include",
                "ssh-alias",
            ])
            .is_err()
        );

        let sync = Cli::try_parse_from(["codex-usage-monit", "remote", "sync", "dev"]).unwrap();
        assert!(matches!(
            sync.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Sync(RemoteSyncArgs {
                    id,
                    ignore_budget: false,
                }),
                ..
            })) if id == "dev"
        ));
        let sync_override = Cli::try_parse_from([
            "codex-usage-monit",
            "remote",
            "sync",
            "dev",
            "--ignore-budget",
        ])
        .unwrap();
        assert!(matches!(
            sync_override.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Sync(RemoteSyncArgs {
                    id,
                    ignore_budget: true,
                }),
                ..
            })) if id == "dev"
        ));
        assert!(Cli::try_parse_from(["codex-usage-monit", "remote", "sync"]).is_err());
        assert!(Cli::try_parse_from(["codex-usage-monit", "remote", "sync", "--all"]).is_err());

        let config = Cli::try_parse_from([
            "codex-usage-monit",
            "remote",
            "config",
            "--auto-sync",
            "false",
            "--active-interval-seconds",
            "60",
            "--idle-interval-seconds",
            "300",
        ])
        .unwrap();
        assert!(matches!(
            config.command,
            Some(Command::Remote(RemoteArgs {
                action: RemoteAction::Config(RemoteConfigArgs {
                    auto_sync: Some(false),
                    active_interval_seconds: Some(60),
                    idle_interval_seconds: Some(300),
                    ..
                }),
                ..
            }))
        ));

        let fenced = Cli::try_parse_from([
            "codex-usage-monit",
            "remote",
            "test",
            "dev",
            "--expected-revision",
            "17",
        ])
        .unwrap();
        assert!(matches!(
            fenced.command,
            Some(Command::Remote(RemoteArgs {
                history_dir: None,
                expected_revision: Some(17),
                action: RemoteAction::Test(RemoteHostArgs { id }),
            })) if id == "dev"
        ));
        let custom_history = Cli::try_parse_from([
            "codex-usage-monit",
            "remote",
            "sync",
            "dev",
            "--history-dir",
            "/tmp/custom-state/history-v1",
        ])
        .unwrap();
        assert!(matches!(
            custom_history.command,
            Some(Command::Remote(RemoteArgs {
                history_dir: Some(path),
                action: RemoteAction::Sync(RemoteSyncArgs { id, .. }),
                ..
            })) if id == "dev" && path.as_path() == Path::new("/tmp/custom-state/history-v1")
        ));
        assert!(help.contains("--history-dir"));
        assert!(!help.contains("expected-revision"));
    }

    #[test]
    fn remote_sync_readiness_requires_live_snapshot_capability() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-server"),
            )
            .unwrap();
        let host = configured.host("dev").unwrap();
        let mut probe = ProbeResult {
            capabilities: vec![
                RemoteCapability::DeltaJournal,
                RemoteCapability::GzipFrame,
                RemoteCapability::RedactedContent,
            ],
            state_writable: true,
            rollout_readable: true,
        };

        assert_eq!(
            missing_remote_sync_capabilities(host, &probe),
            vec!["live_snapshot"]
        );
        probe.capabilities.push(RemoteCapability::LiveSnapshot);
        assert!(missing_remote_sync_capabilities(host, &probe).is_empty());
    }

    struct PanicRemoteSyncTransport;

    impl crate::remote_sync::RemoteDeltaTransport for PanicRemoteSyncTransport {
        fn exchange(
            &mut self,
            _ssh_host: &str,
            _request: &crate::remote_protocol::RemoteExportRequest,
            _timeout: std::time::Duration,
        ) -> std::result::Result<
            crate::remote_transport::RemoteExchangeReport<
                crate::remote_protocol::DeltaPayload,
                crate::remote_protocol::EmptyRemotePayload,
            >,
            crate::remote_transport::RemoteTransportError,
        > {
            panic!("remote sync opened SSH before its local preconditions passed")
        }
    }

    #[derive(Default)]
    struct CompleteRemoteSyncTransport {
        exchanges: usize,
    }

    impl crate::remote_sync::RemoteDeltaTransport for CompleteRemoteSyncTransport {
        fn exchange(
            &mut self,
            ssh_host: &str,
            request: &crate::remote_protocol::RemoteExportRequest,
            _timeout: std::time::Duration,
        ) -> std::result::Result<
            crate::remote_transport::RemoteExchangeReport<
                crate::remote_protocol::DeltaPayload,
                crate::remote_protocol::EmptyRemotePayload,
            >,
            crate::remote_transport::RemoteTransportError,
        > {
            use std::num::NonZeroU64;

            use crate::remote_protocol::{
                BinaryVersion, DeltaCursor, DeltaPage, DeltaPayload, REMOTE_PROTOCOL_VERSION,
                RemoteDeltaCoverage, RemoteDeltaStats, RemoteExportRequestBody,
                RemoteExportResponse, RemoteExportResponseBody, RemoteLiveSnapshot,
                RemoteLiveState, RemoteTiming,
            };

            assert_eq!(ssh_host, "dev-server");
            let RemoteExportRequestBody::Delta(delta) = &request.request else {
                panic!("manual remote sync must issue a delta request")
            };
            self.exchanges += 1;
            let generation = delta
                .delta_cursor
                .map_or_else(|| NonZeroU64::new(1).unwrap(), |cursor| cursor.generation);
            let sequence = delta.delta_cursor.map_or(0, |cursor| cursor.sequence);
            let observed_at = delta.range.to;
            Ok(crate::remote_transport::RemoteExchangeReport {
                response: RemoteExportResponse {
                    protocol_version: REMOTE_PROTOCOL_VERSION,
                    server_version: "0.4.0-test".parse::<BinaryVersion>().unwrap(),
                    source: request.expected_source.clone().unwrap(),
                    redaction_profile: request.redaction_profile,
                    revisions: crate::remote_agent::current_revisions(),
                    observed_at,
                    timing: RemoteTiming {
                        remote_received_at: observed_at,
                        remote_sent_at: observed_at,
                    },
                    result: RemoteExportResponseBody::Delta {
                        page: DeltaPage {
                            generation,
                            from_sequence: sequence,
                            through_sequence: sequence,
                            next_delta_cursor: DeltaCursor {
                                generation,
                                sequence,
                            },
                            has_more: false,
                        },
                        payload: DeltaPayload {
                            coverage: RemoteDeltaCoverage {
                                requested_range: delta.range.clone(),
                                covered_range: Some(delta.range.clone()),
                                range_complete: true,
                                partial_reasons: Vec::new(),
                            },
                            project_descriptors: Vec::new(),
                            bucket_changes: Vec::new(),
                            session_digest_changes: Vec::new(),
                            live: delta.include_live.then(|| {
                                let revision = delta
                                    .known_live_revision
                                    .unwrap_or_else(|| NonZeroU64::new(1).unwrap());
                                RemoteLiveState {
                                    live_revision: revision,
                                    snapshot: (delta.known_live_revision != Some(revision)).then(
                                        || RemoteLiveSnapshot {
                                            captured_at: observed_at,
                                            tasks: Vec::new(),
                                            turns: Vec::new(),
                                        },
                                    ),
                                }
                            }),
                            stats: RemoteDeltaStats::default(),
                            warnings: Vec::new(),
                        },
                    },
                },
                elapsed: std::time::Duration::from_millis(1),
                request_bytes: 64,
                response_bytes: 256,
                response_decoded_bytes: 256,
                stderr_bytes: 0,
            })
        }
    }

    struct FailingRemoteSyncTransport;

    impl crate::remote_sync::RemoteDeltaTransport for FailingRemoteSyncTransport {
        fn exchange(
            &mut self,
            _ssh_host: &str,
            _request: &crate::remote_protocol::RemoteExportRequest,
            _timeout: std::time::Duration,
        ) -> std::result::Result<
            crate::remote_transport::RemoteExchangeReport<
                crate::remote_protocol::DeltaPayload,
                crate::remote_protocol::EmptyRemotePayload,
            >,
            crate::remote_transport::RemoteTransportError,
        > {
            Err(crate::remote_transport::RemoteTransportError::ExitFailure {
                code: Some(255),
                diagnostic: "secret.example /Users/private/.ssh/config".to_owned(),
            })
        }
    }

    fn paired_disabled_remote_config(directory: &Path) -> (RemotesConfigStore, RemotesConfig) {
        use std::num::NonZeroU64;

        use crate::remote_protocol::SourceGeneration;

        let store = RemotesConfigStore::new(directory.join("config/remotes.json"));
        let configured =
            update_remotes_config(&store, RemotesConfigMutation::add_host("dev", "dev-server"))
                .unwrap();
        let configured = store
            .update(
                configured.config_revision(),
                RemotesConfigMutation::edit_host(
                    "dev",
                    RemoteHostEdit {
                        redact_content: Some(false),
                        ..RemoteHostEdit::default()
                    },
                ),
            )
            .unwrap();
        let paired = store
            .update(
                configured.config_revision(),
                RemotesConfigMutation::pair_pin(
                    "dev",
                    SourceGeneration {
                        node_id: "node-11111111111111111111111111111111".parse().unwrap(),
                        generation: NonZeroU64::new(7).unwrap(),
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            paired.host("dev").unwrap().state(),
            RemoteHostState::PairedDisabled
        );
        assert!(!paired.auto_sync_enabled());
        (store, paired)
    }

    #[test]
    fn successful_manual_test_clears_the_actual_state_root_containment_pause() {
        let directory = tempfile::tempdir().unwrap();
        let (_store, paired) = paired_disabled_remote_config(directory.path());
        let host = paired.host("dev").unwrap();
        let source = host.expected_source().unwrap();
        let state_root = directory.path().join("custom-state");
        let history_root = state_root.join("history-v1");
        let observed_at = Utc::now();

        record_manual_remote_test_containment_pause(Some(&history_root), host, source, observed_at)
            .unwrap();
        let health_store = RemoteSyncHealthStore::new(state_root);
        assert!(
            health_store
                .get("dev")
                .unwrap()
                .unwrap()
                .process_containment_paused_for(host)
        );

        assert!(
            clear_manual_remote_test_containment_pause(
                Some(&history_root),
                host,
                observed_at + TimeDelta::seconds(1),
            )
            .unwrap()
        );
        assert!(
            !health_store
                .get("dev")
                .unwrap()
                .unwrap()
                .process_containment_paused()
        );
    }

    #[test]
    fn cli_unpair_and_remove_use_the_exact_custom_history_source_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            redact_content: false,
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let custom_history = directory.path().join("custom-state/history-v1");
        let mut runtime = HistoryRuntime::new(
            custom_history.clone(),
            &collect_config.codex_home,
            collect_config.redact_content,
        )
        .unwrap();
        runtime.ensure_v2_active().unwrap();
        let host = paired.host("dev").unwrap();
        let source = host.expected_source().unwrap().clone();
        let selected = RemoteSyncHostSnapshot::capture_manual(&paired, host).unwrap();
        prepare_remote_source_metadata(&store, &selected, &runtime).unwrap();
        drop(runtime);

        assert_eq!(
            run_remote_unpair(
                &collect_config,
                &store,
                "dev",
                Some(&custom_history),
                Some(paired.config_revision()),
            )
            .unwrap(),
            0
        );
        let unpaired = store.load().unwrap();
        let host = unpaired.host("dev").unwrap();
        assert_eq!(host.expected_source(), None);
        assert_eq!(host.previous_source(), Some(&source));
        let runtime = HistoryRuntime::new(
            custom_history.clone(),
            &collect_config.codex_home,
            collect_config.redact_content,
        )
        .unwrap();
        let metadata = runtime
            .source_history()
            .load_source_metadata(&source.node_id)
            .unwrap();
        assert!(metadata.detached());
        assert!(metadata.include_in_aggregates());
        drop(runtime);

        assert_eq!(
            run_remote_remove(
                &collect_config,
                &store,
                &RemoteRemoveArgs {
                    id: "dev".to_owned(),
                    keep_included: false,
                },
                Some(&custom_history),
                Some(unpaired.config_revision()),
            )
            .unwrap(),
            0
        );
        assert!(store.load().unwrap().host("dev").is_none());
        let runtime = HistoryRuntime::new(
            custom_history,
            &collect_config.codex_home,
            collect_config.redact_content,
        )
        .unwrap();
        let metadata = runtime
            .source_history()
            .load_source_metadata(&source.node_id)
            .unwrap();
        assert!(metadata.detached());
        assert!(!metadata.include_in_aggregates());
    }

    #[test]
    fn stale_internal_remote_revision_is_rejected_before_state_or_ssh() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let stale_revision = paired.config_revision();
        let updated = store
            .update(
                stale_revision,
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        assert!(updated.config_revision() > stale_revision);
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            redact_content: false,
            ..CollectConfig::default()
        };
        let state_root = directory.path().join("state");

        let error = execute_remote_sync_at_state_root_with_budget_override(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut PanicRemoteSyncTransport,
            false,
            Some(stale_revision),
        )
        .unwrap_err();

        assert!(error.to_string().contains("configuration revision changed"));
        assert!(!state_root.exists());
    }

    #[test]
    fn manual_remote_sync_rejects_mismatched_redaction_before_state_or_ssh() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let paired = store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::edit_host(
                    "dev",
                    RemoteHostEdit {
                        redact_content: Some(true),
                        ..RemoteHostEdit::default()
                    },
                ),
            )
            .unwrap();
        assert!(paired.host("dev").unwrap().redact_content());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            redact_content: false,
            ..CollectConfig::default()
        };
        let state_root = directory.path().join("state");

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut PanicRemoteSyncTransport,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("cannot safely activate"));
        assert!(message.contains("remote edit dev --redact-content false"));
        assert!(message.contains("no SSH connection was opened"));
        assert!(!state_root.exists());
    }

    #[test]
    fn manual_remote_sync_migrates_uninitialized_history_before_paired_disabled_transport() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());

        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");
        let mut transport = CompleteRemoteSyncTransport::default();
        let outcome = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut transport,
        )
        .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.output.contains("facts=not-needed"));
        assert_eq!(transport.exchanges, 1);
        let runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            paired.host("dev").unwrap().redact_content(),
        )
        .unwrap();
        let manifest = match runtime.ownership().load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            OwnershipManifestStatus::Uninitialized => panic!("remote sync did not initialize v2"),
        };
        assert_eq!(manifest.state(), HistoryOwnershipState::V2Active);
        let source = paired.host("dev").unwrap().expected_source().unwrap();
        let metadata = runtime
            .source_history()
            .load_source_metadata(&source.node_id)
            .unwrap();
        assert_eq!(metadata.kind(), SourceKind::Ssh);
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            remote_redaction_profile(paired.host("dev").unwrap())
        );
        let health = RemoteSyncHealthStore::new(runtime.state_root().to_path_buf())
            .get("dev")
            .unwrap()
            .unwrap();
        assert!(health.configured());
        assert_eq!(health.node_id(), Some(&source.node_id));
        assert_eq!(
            health.last_result(),
            Some(crate::remote_sync_health::RemoteSyncAttemptResult::Success)
        );
        assert_eq!(health.next_eligible_at(), None);
        assert!(health.last_fact_sync_at().is_some());
        assert_eq!(health.fact_sync_error_category(), None);
    }

    #[test]
    fn manual_remote_sync_honors_hard_budget_unless_explicitly_overridden() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");
        let runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            false,
        )
        .unwrap();
        let source = paired.host("dev").unwrap().expected_source().unwrap();
        let budget = RemoteBandwidthBudgetStore::new(runtime.state_root().to_path_buf());
        let now = Utc::now();
        let reservation = match budget
            .begin_attempt(
                "dev",
                Some(&source.node_id),
                now,
                RemoteBandwidthTransferKind::ManualOverride,
                RemoteSyncLimits::default().max_response_bytes,
            )
            .unwrap()
        {
            RemoteBandwidthAdmission::Granted(reservation) => reservation,
            RemoteBandwidthAdmission::Paused(pause) => {
                panic!("test budget unexpectedly paused: {pause:?}")
            }
        };
        budget
            .complete_attempt(
                &reservation,
                now,
                usize::try_from(crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_HARD_LIMIT_BYTES)
                    .unwrap(),
            )
            .unwrap();
        drop(runtime);

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut PanicRemoteSyncTransport,
        )
        .unwrap_err();
        assert!(error.to_string().contains("paused before SSH"));
        assert!(error.to_string().contains("--ignore-budget"));
        let paused_health = RemoteSyncHealthStore::new(state_root.clone())
            .get("dev")
            .unwrap()
            .unwrap();
        assert!(paused_health.budget_paused());
        assert!(paused_health.budget_resume_at().is_some());
        assert_eq!(paused_health.last_result(), None);
        assert_eq!(paused_health.error_category(), None);
        assert_eq!(paused_health.consecutive_failures(), 0);

        let mut transport = CompleteRemoteSyncTransport::default();
        let outcome = execute_remote_sync_at_state_root_with_budget_override(
            &collect_config,
            &store,
            "dev",
            state_root,
            &mut transport,
            true,
            None,
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(transport.exchanges, 1);
    }

    #[test]
    fn manual_remote_sync_records_transport_failure_without_raw_error_text() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut FailingRemoteSyncTransport,
        )
        .unwrap_err();
        assert!(error.to_string().contains("secret.example"));

        let health = RemoteSyncHealthStore::new(state_root)
            .get("dev")
            .unwrap()
            .unwrap();
        assert!(health.configured());
        assert_eq!(
            health.node_id(),
            Some(
                &paired
                    .host("dev")
                    .unwrap()
                    .expected_source()
                    .unwrap()
                    .node_id
            )
        );
        assert_eq!(
            health.last_result(),
            Some(crate::remote_sync_health::RemoteSyncAttemptResult::Failure)
        );
        assert_eq!(
            health.error_category(),
            Some(crate::remote_sync_health::RemoteSyncErrorCategory::Transport)
        );
        assert_eq!(health.next_eligible_at(), None);
    }

    #[test]
    fn manual_remote_sync_does_not_cut_over_while_a_cooperating_recorder_owns_the_state() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");
        let runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            paired.host("dev").unwrap().redact_content(),
        )
        .unwrap();
        let _guard = match try_acquire_recorder_instance_lock(
            runtime.legacy_history().history_root().unwrap(),
        )
        .unwrap()
        {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
        };
        drop(runtime);

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut PanicRemoteSyncTransport,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot migrate history while a recorder owns")
        );
        assert!(error.to_string().contains("no SSH connection was opened"));
        let runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            paired.host("dev").unwrap().redact_content(),
        )
        .unwrap();
        assert!(matches!(
            runtime.ownership().load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        ));
    }

    #[test]
    fn manual_remote_sync_coexists_with_a_same_profile_v2_recorder() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");
        let mut runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            paired.host("dev").unwrap().redact_content(),
        )
        .unwrap();
        runtime.ensure_v2_active().unwrap();
        let _profile_lease = acquire_runtime_profile_lease(&runtime).unwrap();
        let _recorder_guard = match try_acquire_recorder_instance_lock(
            runtime.legacy_history().history_root().unwrap(),
        )
        .unwrap()
        {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
        };
        let mut transport = CompleteRemoteSyncTransport::default();

        let outcome = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root,
            &mut transport,
        )
        .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(transport.exchanges, 1);
    }

    #[test]
    fn manual_remote_sync_rejects_an_opposite_profile_before_ssh() {
        let directory = tempfile::tempdir().unwrap();
        let (store, _) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");
        let opposite = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            true,
        )
        .unwrap();
        let _opposite_profile_lease = acquire_runtime_profile_lease(&opposite).unwrap();

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root,
            &mut PanicRemoteSyncTransport,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("could not select its local history profile")
        );
        assert!(error.to_string().contains("no SSH connection was opened"));
    }

    #[test]
    fn manual_remote_sync_rejects_an_unpaired_host_before_ssh() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        update_remotes_config(&store, RemotesConfigMutation::add_host("dev", "dev-server"))
            .unwrap();
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        let state_root = directory.path().join("state");

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut PanicRemoteSyncTransport,
        )
        .unwrap_err();
        assert!(error.to_string().contains("not paired"));
        assert!(!state_root.exists());
    }

    #[test]
    fn manual_remote_sync_blocks_a_recent_legacy_recorder_before_migration_or_ssh() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");
        let runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            paired.host("dev").unwrap().redact_content(),
        )
        .unwrap();
        let status_path = default_status_file(runtime.legacy_history().history_root().unwrap());
        write_recorder_status(
            &status_path,
            &RecorderStatusFile::started(
                Utc::now(),
                runtime.legacy_history().namespace().to_owned(),
            ),
        )
        .unwrap();
        assert!(matches!(
            runtime.ownership().load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        ));
        drop(runtime);

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root.clone(),
            &mut PanicRemoteSyncTransport,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("recent legacy recorder"));
        assert!(message.contains("service uninstall"));
        assert!(message.contains("service install"));
        assert!(message.contains("no SSH connection was opened"));
        let runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            paired.host("dev").unwrap().redact_content(),
        )
        .unwrap();
        assert!(matches!(
            runtime.ownership().load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        ));
    }

    #[test]
    fn manual_remote_sync_does_not_open_ssh_when_local_migration_fails() {
        let directory = tempfile::tempdir().unwrap();
        let (store, paired) = paired_disabled_remote_config(directory.path());
        let collect_config = CollectConfig {
            codex_home: directory.path().join("codex-home"),
            ..CollectConfig::default()
        };
        fs::create_dir_all(&collect_config.codex_home).unwrap();
        let state_root = directory.path().join("state");
        let runtime = HistoryRuntime::new(
            state_root.join("history-v1"),
            &collect_config.codex_home,
            paired.host("dev").unwrap().redact_content(),
        )
        .unwrap();
        fs::write(runtime.source_history().layout_root(), b"not a directory").unwrap();
        drop(runtime);

        let error = execute_remote_sync_at_state_root(
            &collect_config,
            &store,
            "dev",
            state_root,
            &mut PanicRemoteSyncTransport,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("could not migrate local history"));
        assert!(message.contains("no SSH connection was opened"));
    }

    #[test]
    fn manual_remote_sync_report_is_human_readable_and_reports_completion() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let config =
            update_remotes_config(&store, RemotesConfigMutation::add_host("dev", "dev-server"))
                .unwrap();
        let outcome = format_remote_sync_report(
            config.host("dev").unwrap(),
            &RemoteSyncReport {
                pages_committed: 3,
                changes_committed: 3,
                live_state_changed: false,
                response_bytes: 4096,
                completion: RemoteSyncCompletion::Complete,
            },
        );

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.output.contains("remote-sync dev via dev-server"));
        assert!(outcome.output.contains("status=complete"));
        assert!(outcome.output.contains("pages=3 response=4096B"));
        assert!(!outcome.output.contains("run the same command again"));
    }

    #[test]
    fn remote_allowlist_mutations_unpair_and_listing_never_require_a_connection() {
        use std::num::NonZeroU64;

        use crate::remote_protocol::SourceGeneration;

        let directory = tempfile::tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let config =
            update_remotes_config(&store, RemotesConfigMutation::add_host("dev", "dev-server"))
                .unwrap();
        let host = config.host("dev").unwrap();
        assert_eq!(host.state(), RemoteHostState::ConfiguredUnpaired);
        assert!(!config.auto_sync_enabled());

        let source = SourceGeneration {
            node_id: "node-11111111111111111111111111111111".parse().unwrap(),
            generation: NonZeroU64::new(7).unwrap(),
        };
        let paired = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::pair_pin("dev", source),
            )
            .unwrap();
        assert_eq!(
            paired.host("dev").unwrap().state(),
            RemoteHostState::PairedDisabled
        );
        assert!(!paired.auto_sync_enabled());

        let text = render_remotes_config(&paired, FormatArg::Text).unwrap();
        assert!(text.contains("dev-server"));
        assert!(text.contains("@7"));
        let json = render_remotes_config(&paired, FormatArg::Json).unwrap();
        assert!(json.contains("expectedSource"));
        assert!(!json.contains("expectedNodeId"));

        let enabled = store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        let unpaired = store
            .update(
                enabled.config_revision(),
                RemotesConfigMutation::unpair_host("dev"),
            )
            .unwrap();
        let host = unpaired.host("dev").unwrap();
        assert_eq!(host.state(), RemoteHostState::ConfiguredUnpaired);
        assert!(!host.sync_enabled());
        assert_eq!(host.expected_source(), None);
    }

    #[test]
    fn remote_test_rejects_a_response_after_config_revision_changes() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let configured =
            update_remotes_config(&store, RemotesConfigMutation::add_host("dev", "dev-server"))
                .unwrap();
        let expected_host = configured.host("dev").unwrap().clone();

        let current = ensure_remote_probe_target_current(
            &store,
            configured.config_revision(),
            &expected_host,
        )
        .unwrap();
        assert_eq!(current, expected_host);

        store
            .update(
                configured.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        let error = ensure_remote_probe_target_current(
            &store,
            configured.config_revision(),
            &expected_host,
        )
        .unwrap_err();
        assert!(error.to_string().contains("stale remote test response"));
        assert!(error.to_string().contains("revision changed"));
    }

    #[test]
    fn remote_test_rejects_same_revision_target_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config/remotes.json");
        let store = RemotesConfigStore::new(path.clone());
        let configured =
            update_remotes_config(&store, RemotesConfigMutation::add_host("dev", "dev-server"))
                .unwrap();
        let expected_host = configured.host("dev").unwrap().clone();

        let mut rewritten = serde_json::to_value(&configured).unwrap();
        rewritten["hosts"][0]["sshHost"] = serde_json::json!("replacement-server");
        std::fs::write(&path, serde_json::to_vec_pretty(&rewritten).unwrap()).unwrap();

        let error = ensure_remote_probe_target_current(
            &store,
            configured.config_revision(),
            &expected_host,
        )
        .unwrap_err();
        assert!(error.to_string().contains("stale remote test response"));
        assert!(
            error
                .to_string()
                .contains("changed without a revision advance")
        );
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
            local_session_digests: Default::default(),
        }
    }

    #[test]
    fn legacy_remote_source_projection_keeps_global_quota_without_local_usage() {
        let observation =
            report_history_collection_result(Provenance::ServerSnapshot).history_observation;
        let history = HistoryData {
            quota_points: observation.quota_points,
            half_hour_buckets: observation.half_hour_buckets,
            weekly_local_points: observation.weekly_local_points,
            warnings: vec!["existing".to_owned()],
            read_only: true,
            ..HistoryData::default()
        };
        let local =
            legacy_history_for_source_selector(history.clone(), &HistorySourceSelector::Local);
        assert_eq!(local, history);

        let source_id = "node-0123456789abcdef0123456789abcdef".parse().unwrap();
        let remote =
            legacy_history_for_source_selector(history, &HistorySourceSelector::Remote(source_id));
        assert_eq!(remote.quota_points.len(), 1);
        assert!(remote.half_hour_buckets.is_empty());
        assert!(remote.weekly_local_points.is_empty());
        assert!(remote.read_only);
        assert!(remote.warnings.iter().any(|warning| {
            warning.starts_with("source_selection_unavailable:unsupported_by_legacy:")
        }));
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
    fn report_history_activates_v2_and_persists_the_live_local_source() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("state/history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let config = CollectConfig {
            codex_home,
            ..CollectConfig::default()
        };
        let result = report_history_collection_result(Provenance::ServerSnapshot);

        let (store, history) =
            collect_and_load_report_history(&config, &result, Some(history_root));
        let ReportHistoryStore::Runtime { runtime, .. } = store else {
            panic!("a canonical history-v1 root should use the source-aware runtime");
        };
        let manifest = match runtime.ownership().load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            OwnershipManifestStatus::Uninitialized => panic!("v2 was not activated"),
        };

        assert_eq!(manifest.state(), HistoryOwnershipState::V2Active);
        assert_eq!(history.half_hour_buckets.len(), 1);
        assert_eq!(history.half_hour_buckets[0].token_usage.total_tokens, 42);
        let metadata = runtime
            .source_history()
            .load_source_metadata(runtime.source_identity().node_id())
            .unwrap();
        assert_eq!(metadata.kind(), SourceKind::Local);
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::PreviewEnabled
        );
    }

    #[test]
    fn report_history_defers_cutover_while_a_cooperating_recorder_owns_the_state() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("state/history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        let _guard = match try_acquire_recorder_instance_lock(&history_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
        };
        drop(runtime);
        let config = CollectConfig {
            codex_home,
            ..CollectConfig::default()
        };
        let result = report_history_collection_result(Provenance::ServerSnapshot);

        let (store, history) =
            collect_and_load_report_history(&config, &result, Some(history_root));
        let ReportHistoryStore::Runtime { runtime, .. } = store else {
            panic!("a canonical history-v1 root should use the source-aware runtime");
        };
        let manifest = match runtime.ownership().load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            OwnershipManifestStatus::Uninitialized => panic!("v1 ownership was not initialized"),
        };

        assert_eq!(manifest.state(), HistoryOwnershipState::V1Active);
        assert_eq!(history.half_hour_buckets.len(), 1);
        assert!(history.warnings.iter().any(|warning| {
            warning.contains("source-aware history cutover deferred while another recorder")
        }));
    }

    #[test]
    fn report_history_persists_with_a_same_profile_v2_recorder() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("state/history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        runtime.ensure_v2_active().unwrap();
        let _profile_lease = acquire_runtime_profile_lease(&runtime).unwrap();
        let _recorder_guard = match try_acquire_recorder_instance_lock(&history_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
        };
        let config = CollectConfig {
            codex_home,
            ..CollectConfig::default()
        };
        let result = report_history_collection_result(Provenance::ServerSnapshot);

        let (_, history) = collect_and_load_report_history(&config, &result, Some(history_root));

        assert_eq!(history.half_hour_buckets.len(), 1);
        assert!(!history.read_only);
        assert!(history.warnings.iter().all(|warning| {
            !warning.contains("profile cannot be selected")
                && !warning.contains("recorder owns this state")
        }));
    }

    #[test]
    fn report_history_is_read_only_while_the_opposite_profile_is_active() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("state/history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let opposite = HistoryRuntime::new(history_root.clone(), &codex_home, true).unwrap();
        let _opposite_profile_lease = acquire_runtime_profile_lease(&opposite).unwrap();
        let config = CollectConfig {
            codex_home: codex_home.clone(),
            ..CollectConfig::default()
        };
        let result = report_history_collection_result(Provenance::ServerSnapshot);
        let hidden_v1_shard = HistoryStore::new(history_root.clone(), &codex_home)
            .namespace_dir()
            .unwrap()
            .join(format!("{}.json", result.snapshot.as_of.date_naive()));

        let (_, history) = collect_and_load_report_history(&config, &result, Some(history_root));

        assert!(!hidden_v1_shard.exists());
        assert_eq!(history.half_hour_buckets.len(), 1);
        assert!(history.read_only);
        assert!(history.warnings.iter().any(|warning| {
            warning.contains("history persistence is read-only")
                && warning.contains("active history selection")
        }));
    }

    #[test]
    fn recorder_restart_accepts_a_fresh_exact_v2_status_after_lock_handoff() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("state/history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        let active = runtime.ensure_v2_active().unwrap();
        let status_path = default_status_file(&history_root);
        let mut previous = RecorderStatusFile::started_with_interval(
            Utc::now(),
            runtime.legacy_history().namespace().to_owned(),
            600,
        );
        previous.bind_source_aware_v2(active.epoch()).unwrap();
        write_recorder_status(&status_path, &previous).unwrap();
        let _guard = match try_acquire_recorder_instance_lock(&history_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("released recorder lock remained busy"),
        };

        reject_incompatible_recorder_before_recorder_cutover(&[status_path], &runtime).unwrap();
    }

    #[test]
    fn recorder_restart_still_rejects_a_fresh_v2_status_for_another_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("state/history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        let active = runtime.ensure_v2_active().unwrap();
        let status_path = default_status_file(&history_root);
        let mut previous = RecorderStatusFile::started_with_interval(
            Utc::now(),
            format!("{}-redacted", runtime.profile_id().as_str()),
            600,
        );
        previous.bind_source_aware_v2(active.epoch()).unwrap();
        write_recorder_status(&status_path, &previous).unwrap();
        let _guard = match try_acquire_recorder_instance_lock(&history_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("released recorder lock remained busy"),
        };

        let error = reject_incompatible_recorder_before_recorder_cutover(&[status_path], &runtime)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another recorder may already be active")
        );
    }

    #[test]
    fn recorder_custom_status_still_checks_default_legacy_status() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("state/history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        let default_path = default_status_file(&history_root);
        let custom_path = temp.path().join("custom-recorder-status.json");
        let mut legacy = RecorderStatusFile::started_with_interval(
            Utc::now(),
            runtime.legacy_history().namespace().to_owned(),
            600,
        );
        legacy.record_success(Utc::now());
        write_recorder_status(&default_path, &legacy).unwrap();

        let error = reject_incompatible_recorder_before_recorder_cutover(
            &[default_path.clone(), custom_path],
            &runtime,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another recorder may already be active")
        );
        assert!(
            error
                .to_string()
                .contains(&default_path.display().to_string())
        );
    }

    #[test]
    fn report_history_keeps_live_observation_when_future_format_is_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("history");
        let config = CollectConfig {
            codex_home: temp.path().join("codex"),
            ..CollectConfig::default()
        };
        let result = report_history_collection_result(Provenance::ServerSnapshot);
        let probe = HistoryStore::new(history_root.clone(), &config.codex_home);
        let namespace_dir = probe.namespace_dir().unwrap().to_path_buf();
        std::fs::create_dir_all(&namespace_dir).unwrap();
        let shard_path = namespace_dir.join(format!("{}.json", result.snapshot.as_of.date_naive()));
        let future = serde_json::to_vec(&serde_json::json!({
            "formatVersion": HISTORY_FORMAT_VERSION + 1,
            "namespace": probe.namespace(),
        }))
        .unwrap();
        std::fs::write(&shard_path, &future).unwrap();

        let (_, history) = collect_and_load_report_history(&config, &result, Some(history_root));

        assert!(history.read_only);
        assert!(
            history
                .warnings
                .iter()
                .any(|warning| warning.contains("future history format version"))
        );
        assert_eq!(
            history.quota_points,
            result.history_observation.quota_points
        );
        assert_eq!(
            history.half_hour_buckets,
            result.history_observation.half_hour_buckets
        );
        assert_eq!(
            history.weekly_local_points,
            result.history_observation.weekly_local_points
        );
        assert_eq!(std::fs::read(shard_path).unwrap(), future);
    }

    #[test]
    fn canonical_runtime_failure_after_v2_activation_never_writes_hidden_v1_history() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = temp.path().join("state");
        let history_root = state_root.join("history-v1");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        runtime.ensure_v2_active().unwrap();
        drop(runtime);
        fs::write(
            state_root.join("source-identity.json"),
            b"{broken identity\n",
        )
        .unwrap();

        let config = CollectConfig {
            codex_home: codex_home.clone(),
            ..CollectConfig::default()
        };
        let result = report_history_collection_result(Provenance::ServerSnapshot);
        let legacy_probe = HistoryStore::new(history_root.clone(), &codex_home);
        let hidden_v1_shard = legacy_probe
            .namespace_dir()
            .unwrap()
            .join(format!("{}.json", result.snapshot.as_of.date_naive()));

        let (store, history) =
            collect_and_load_report_history(&config, &result, Some(history_root));

        assert!(matches!(
            store,
            ReportHistoryStore::LegacyFallback {
                writable: false,
                ..
            }
        ));
        assert!(!hidden_v1_shard.exists());
        assert_eq!(history.half_hour_buckets.len(), 1);
        assert!(history.warnings.iter().any(|warning| {
            warning.contains("read-only legacy view")
                && warning.contains("no history will be persisted")
        }));
    }

    #[test]
    fn report_history_keeps_redacted_live_observation_after_persistence_error() {
        let temp = tempfile::tempdir().unwrap();
        let history_root = temp.path().join("history-file");
        let original = b"not a directory\n";
        std::fs::write(&history_root, original).unwrap();
        let config = CollectConfig {
            codex_home: temp.path().join("codex"),
            redact_content: true,
            ..CollectConfig::default()
        };
        let mut result = report_history_collection_result(Provenance::ServerSnapshot);
        result.history_observation.half_hour_buckets[0]
            .project_groups
            .push(LocalProjectUsageGroup {
                thread_id: "root".to_string(),
                title: Some("secret title".to_string()),
                message_preview: Some("secret message".to_string()),
                ..LocalProjectUsageGroup::default()
            });

        let (_, history) =
            collect_and_load_report_history(&config, &result, Some(history_root.clone()));

        assert!(history.read_only);
        assert!(
            history
                .warnings
                .iter()
                .any(|warning| warning.contains("using a read-only legacy view"))
        );
        assert_eq!(
            history.quota_points,
            result.history_observation.quota_points
        );
        assert_eq!(history.half_hour_buckets[0].token_usage.total_tokens, 42);
        assert_eq!(history.half_hour_buckets[0].estimated_cost_units, 7);
        assert_eq!(
            history.half_hour_buckets[0].api_long_context_extra_cost_units,
            Some(3)
        );
        let group = &history.half_hour_buckets[0].project_groups[0];
        assert_eq!(group.title.as_deref(), Some("[redacted]"));
        assert_eq!(group.message_preview.as_deref(), Some("[redacted]"));
        assert_eq!(
            history.weekly_local_points,
            result.history_observation.weekly_local_points
        );
        assert_eq!(std::fs::read(history_root).unwrap(), original);
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

    #[test]
    fn manual_bandwidth_reservation_is_kept_for_ambiguous_post_transport_errors() {
        assert!(manual_error_proves_transport_not_started(
            &RemoteSyncError::InvalidLimits("invalid limits")
        ));
        assert!(manual_error_proves_transport_not_started(
            &RemoteSyncError::HostNotPaired {
                host_id: "dev".to_owned(),
            }
        ));
        assert!(manual_error_proves_transport_not_started(
            &RemoteSyncError::PreTransportLocal(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer busy",
            ))
        ));
        assert!(!manual_error_proves_transport_not_started(
            &RemoteSyncError::ConfigurationChanged {
                host_id: "dev".to_owned(),
            }
        ));
        assert!(!manual_error_proves_transport_not_started(
            &RemoteSyncError::Transport(
                crate::remote_transport::RemoteTransportError::InvalidHost(
                    "transport may already have transferred bytes".to_owned(),
                )
            )
        ));
        assert!(!manual_error_proves_transport_not_started(
            &RemoteSyncError::InvalidStartedAt
        ));
    }
}
