//! Production filesystem/SSH adapter and stoppable worker for automatic
//! remote synchronization.
//!
//! Host selection and timing remain in [`crate::remote_sync_scheduler`]. This
//! module binds one selected host to the ownership-aware local history runtime,
//! registers its exact source metadata, and delegates the bounded exchange to
//! [`crate::remote_sync::sync_remote_delta_bounded`]. It never enumerates SSH
//! configuration and never holds a config or history writer lock across SSH.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use crate::config::CollectConfig;
use crate::history_ownership::{HistoryOwnershipState, OwnershipManifestStatus};
use crate::history_profile_lease::{TryHistoryProfileLease, try_acquire_history_profile_lease};
use crate::history_runtime::HistoryRuntime;
use crate::project_mapping::ProjectMappingStore;
use crate::remote_bandwidth_budget::{
    RemoteBandwidthAdmission, RemoteBandwidthBudgetLevel, RemoteBandwidthBudgetPausedError,
    RemoteBandwidthBudgetStore, RemoteBandwidthReservation, RemoteBandwidthTransferKind,
};
use crate::remote_fact_sync::{RemoteFactSyncLimits, RemoteFactTransport, SshRemoteFactTransport};
use crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES;
use crate::remote_source_metadata::{
    finalize_remote_source_metadata, prepare_remote_source_metadata,
};
use crate::remote_sync::{
    FilesystemRemoteDeltaLocalPhases, RemoteDeltaTransport, RemoteSyncError,
    RemoteSyncHostSnapshot, RemoteSyncLimits, RemoteSyncReport, SshRemoteDeltaTransport,
    TryRemoteHostSyncLease, build_remote_delta_ingest_binding, preflight_remote_delta_position,
    sync_remote_delta_bounded, try_acquire_remote_host_sync_lease,
};
#[cfg(test)]
use crate::remote_sync_health::RemoteSyncAttemptResult;
use crate::remote_sync_health::{RemoteSyncErrorCategory, RemoteSyncHealthStore};
use crate::remote_sync_scheduler::{
    AUTOMATIC_REMOTE_SYNC_ACTIVITY_HYSTERESIS, AutomaticRemoteSyncExecutor, RemoteSyncScheduleSeed,
    RemoteSyncScheduler, RemoteSyncSchedulerClock, RemoteSyncSchedulerTick,
};
use crate::remote_transport::{
    RemoteProbeOptions, RemoteProbeReport, RemoteTransportError,
    probe_remote_with_agent_executable_and_environment, probe_remote_with_environment,
};
use crate::remotes_config::{RemoteHostConfig, RemotesConfig, RemotesConfigStore};
use crate::replica_fact_followup::{
    DeferredRemoteFactTransport, ReplicaFactFollowupReport, estimated_fact_network_bytes,
    execute_prepared_replica_fact_followup, fact_limits_for_response_budget,
    prepare_replica_fact_followup,
};

const CONFIG_ERROR_RETRY_BASE: Duration = Duration::from_secs(30);
const CONFIG_ERROR_RETRY_MAX: Duration = Duration::from_secs(5 * 60);
// The scheduler's due time can legitimately be hours away, but remotes.json
// is an independent cross-process control surface. Re-read it promptly so a
// newly enabled host does not wait for the old idle interval and a disabled
// host cannot remain queued behind a stale in-memory selection. This polling
// does not open SSH; the scheduler retains the real per-host due time.
const MAX_CONFIG_RELOAD_SLEEP: Duration = Duration::from_secs(5);

/// Probe boundary kept separate from aggregate-page exchange so tests can
/// prove a hard pause never invokes the data transport. Production still uses
/// the same fixed system-OpenSSH runner and framed Probe protocol.
pub trait AutomaticRemoteProbeTransport {
    fn probe(
        &mut self,
        ssh_host: &str,
        options: &RemoteProbeOptions,
    ) -> Result<RemoteProbeReport, RemoteTransportError>;

    fn probe_host(
        &mut self,
        host: &RemoteHostConfig,
        options: &RemoteProbeOptions,
    ) -> Result<RemoteProbeReport, RemoteTransportError> {
        self.probe(host.ssh_host(), options)
    }
}

impl AutomaticRemoteProbeTransport for SshRemoteDeltaTransport {
    fn probe(
        &mut self,
        ssh_host: &str,
        options: &RemoteProbeOptions,
    ) -> Result<RemoteProbeReport, RemoteTransportError> {
        probe_remote_with_environment(ssh_host, options, self.environment())
    }

    fn probe_host(
        &mut self,
        host: &RemoteHostConfig,
        options: &RemoteProbeOptions,
    ) -> Result<RemoteProbeReport, RemoteTransportError> {
        probe_remote_with_agent_executable_and_environment(
            host.ssh_host(),
            host.agent_executable(),
            options,
            self.environment(),
        )
    }
}

/// Whether this adapter may perform the one-time local v1-to-v2 cutover.
///
/// `RequireV2Active` is the safe default for a background worker. The second
/// variant is an explicit assertion made by startup orchestration after it has
/// stopped or otherwise proven the absence of legacy writers that do not
/// participate in the ownership lease.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutomaticRemoteCutoverPolicy {
    #[default]
    RequireV2Active,
    LegacyWritersQuiescedAndPrevalidated,
}

/// Filesystem-backed automatic executor. `SshRemoteDeltaTransport` is the
/// production default; the generic transport boundary also supports focused
/// tests without opening a network connection.
pub struct FilesystemAutomaticRemoteSyncExecutor<
    T = SshRemoteDeltaTransport,
    F = SshRemoteFactTransport,
> {
    state_root: PathBuf,
    codex_home: PathBuf,
    config_store: RemotesConfigStore,
    local_redact_content: bool,
    cutover_policy: AutomaticRemoteCutoverPolicy,
    bandwidth_budget: RemoteBandwidthBudgetStore,
    durable_schedule_seeds: Option<BTreeMap<String, RemoteSyncScheduleSeed>>,
    process_containment_uncertain: bool,
    transport: T,
    fact_transport: F,
    collect_config: CollectConfig,
    project_mapping_store: ProjectMappingStore,
}

impl FilesystemAutomaticRemoteSyncExecutor<SshRemoteDeltaTransport, SshRemoteFactTransport> {
    pub fn new(
        state_root: PathBuf,
        codex_home: PathBuf,
        config_store: RemotesConfigStore,
        local_redact_content: bool,
        cutover_policy: AutomaticRemoteCutoverPolicy,
    ) -> Self {
        Self::with_transports(
            state_root,
            local_collect_config(codex_home, local_redact_content),
            config_store,
            cutover_policy,
            SshRemoteDeltaTransport::default(),
            SshRemoteFactTransport::default(),
        )
    }
}

impl<T> FilesystemAutomaticRemoteSyncExecutor<T, DeferredRemoteFactTransport> {
    pub fn with_transport(
        state_root: PathBuf,
        codex_home: PathBuf,
        config_store: RemotesConfigStore,
        local_redact_content: bool,
        cutover_policy: AutomaticRemoteCutoverPolicy,
        transport: T,
    ) -> Self {
        Self::with_transports(
            state_root,
            local_collect_config(codex_home, local_redact_content),
            config_store,
            cutover_policy,
            transport,
            DeferredRemoteFactTransport,
        )
    }
}

impl<T, F> FilesystemAutomaticRemoteSyncExecutor<T, F> {
    pub fn with_transports(
        state_root: PathBuf,
        collect_config: CollectConfig,
        config_store: RemotesConfigStore,
        cutover_policy: AutomaticRemoteCutoverPolicy,
        transport: T,
        fact_transport: F,
    ) -> Self {
        Self::with_transports_and_project_mapping_store(
            state_root,
            collect_config,
            config_store,
            cutover_policy,
            transport,
            fact_transport,
            ProjectMappingStore::discover(),
        )
    }

    pub fn with_transports_and_project_mapping_store(
        state_root: PathBuf,
        collect_config: CollectConfig,
        config_store: RemotesConfigStore,
        cutover_policy: AutomaticRemoteCutoverPolicy,
        transport: T,
        fact_transport: F,
        project_mapping_store: ProjectMappingStore,
    ) -> Self {
        let bandwidth_budget = RemoteBandwidthBudgetStore::new(state_root.clone());
        Self {
            state_root,
            codex_home: collect_config.codex_home.clone(),
            config_store,
            local_redact_content: collect_config.redact_content,
            cutover_policy,
            bandwidth_budget,
            durable_schedule_seeds: None,
            process_containment_uncertain: false,
            transport,
            fact_transport,
            collect_config,
            project_mapping_store,
        }
    }

    pub fn state_root(&self) -> &std::path::Path {
        &self.state_root
    }

    pub fn codex_home(&self) -> &std::path::Path {
        &self.codex_home
    }
}

impl<T, F> AutomaticRemoteSyncExecutor for FilesystemAutomaticRemoteSyncExecutor<T, F>
where
    T: RemoteDeltaTransport + AutomaticRemoteProbeTransport,
    F: RemoteFactTransport,
{
    fn process_containment_paused_hosts(
        &mut self,
        hosts: &[crate::remotes_config::RemoteHostConfig],
    ) -> Result<Option<BTreeMap<String, bool>>, RemoteSyncError> {
        let health = match RemoteSyncHealthStore::new(self.state_root.clone()).list() {
            Ok(health) => health,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(RemoteSyncError::Local(error)),
        };
        let paused = hosts
            .iter()
            .filter_map(|host| {
                health
                    .iter()
                    .find(|entry| entry.host_id() == host.id())
                    .map(|entry| {
                        (
                            host.id().to_owned(),
                            entry.process_containment_paused_for(host),
                        )
                    })
            })
            .collect();
        Ok(Some(paused))
    }

    fn take_process_containment_signal(
        &mut self,
        _host: &crate::remotes_config::RemoteHostConfig,
    ) -> bool {
        std::mem::take(&mut self.process_containment_uncertain)
    }

    fn restore_host_schedule(
        &mut self,
        host: &crate::remotes_config::RemoteHostConfig,
    ) -> Result<Option<RemoteSyncScheduleSeed>, RemoteSyncError> {
        let Some(expected_source) = host.expected_source() else {
            return Ok(None);
        };
        if self.durable_schedule_seeds.is_none() {
            // Load the bounded health file only once even when hundreds of
            // hosts are eligible at process start.
            let now = Utc::now();
            let mut seeds = BTreeMap::new();
            for health in RemoteSyncHealthStore::new(self.state_root.clone())
                .list()
                .map_err(RemoteSyncError::Local)?
            {
                let Some(source) = health.source().cloned() else {
                    continue;
                };
                // A durable hard-pause probe deadline supersedes a stale
                // interval/backoff from the last ordinary attempt. If the
                // rolling window is known to resume sooner, wake then so the
                // executor can retry normal admission; otherwise the fixed
                // probe deadline is the next useful scheduler boundary.
                let deadline = match health.budget_probe_due_at() {
                    Some(probe_due_at) => health
                        .budget_resume_at()
                        .map_or(probe_due_at, |resume_at| resume_at.min(probe_due_at)),
                    None => {
                        let Some(next_eligible_at) = health.next_eligible_at() else {
                            continue;
                        };
                        next_eligible_at
                    }
                };
                let next_eligible_in = deadline
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or(Duration::ZERO);
                // Budget pauses and their probes never change the ordinary
                // transport failure streak. Preserve the durable value even
                // when the latest health event is an independent pause.
                let consecutive_failures = health.consecutive_failures();
                let active_until_in = health.last_activity_at().and_then(|last_activity_at| {
                    let hysteresis =
                        TimeDelta::from_std(AUTOMATIC_REMOTE_SYNC_ACTIVITY_HYSTERESIS).ok()?;
                    last_activity_at
                        .checked_add_signed(hysteresis)?
                        .signed_duration_since(now)
                        .to_std()
                        .ok()
                        .filter(|delay| !delay.is_zero())
                });
                seeds.insert(
                    health.host_id().to_owned(),
                    RemoteSyncScheduleSeed::new(source, next_eligible_in, consecutive_failures)
                        .with_active_until_in(active_until_in),
                );
            }
            self.durable_schedule_seeds = Some(seeds);
        }
        Ok(self
            .durable_schedule_seeds
            .as_mut()
            .and_then(|seeds| seeds.remove(host.id()))
            .filter(|seed| seed.source() == expected_source))
    }

    fn sync_host(
        &mut self,
        selected: &RemoteSyncHostSnapshot,
        limits: RemoteSyncLimits,
    ) -> Result<RemoteSyncReport, RemoteSyncError> {
        self.process_containment_uncertain = false;
        // Reject a stale/disabled selection before creating or migrating local
        // state. The lock is released immediately; later registration and page
        // commits repeat this exact check under their own short critical
        // sections.
        self.config_store
            .with_current_host(selected.config_revision(), selected.host(), || Ok(()))
            .map_err(RemoteSyncError::Local)?;

        if selected.host().redact_content() != self.local_redact_content {
            return Err(RemoteSyncError::Local(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote redaction profile does not match the recorder's local history profile",
            )));
        }

        let legacy_history_root = self.state_root.join("history-v1");
        let mut runtime = HistoryRuntime::new_with_project_mapping_store(
            legacy_history_root,
            &self.codex_home,
            self.local_redact_content,
            self.project_mapping_store.clone(),
        )
        .map_err(RemoteSyncError::Local)?;
        let profile_lease = match try_acquire_history_profile_lease(
            runtime.state_root(),
            runtime.profile_id().clone(),
            runtime.redaction_profile(),
        )
        .map_err(RemoteSyncError::Local)?
        {
            TryHistoryProfileLease::Acquired(guard) => guard,
            TryHistoryProfileLease::Busy { .. } => {
                return Err(RemoteSyncError::Local(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "automatic remote sync cannot select its history redaction profile",
                )));
            }
        };
        let expected_source = selected.host().expected_source().ok_or_else(|| {
            RemoteSyncError::Local(io::Error::new(
                io::ErrorKind::InvalidInput,
                "automatic remote sync selected an unpaired host",
            ))
        })?;
        if runtime.source_identity().node_id() == &expected_source.node_id {
            return Err(RemoteSyncError::Local(io::Error::new(
                io::ErrorKind::InvalidData,
                "paired remote source identity collides with the local machine identity",
            )));
        }

        match runtime
            .ownership()
            .load_manifest()
            .map_err(RemoteSyncError::Local)?
        {
            OwnershipManifestStatus::Initialized(manifest)
                if manifest.state() == HistoryOwnershipState::V2Active => {}
            OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized
                if self.cutover_policy
                    == AutomaticRemoteCutoverPolicy::LegacyWritersQuiescedAndPrevalidated =>
            {
                runtime.ensure_v2_active().map_err(RemoteSyncError::Local)?;
            }
            OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
                return Err(RemoteSyncError::Local(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "automatic remote sync requires v2-active history; startup must first quiesce legacy writers and perform the explicit cutover",
                )));
            }
        }

        let _host_sync_lease =
            match try_acquire_remote_host_sync_lease(runtime.state_root(), selected.host().id())
                .map_err(RemoteSyncError::Local)?
            {
                TryRemoteHostSyncLease::Acquired(lease) => lease,
                TryRemoteHostSyncLease::Busy => {
                    return Err(RemoteSyncError::Local(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "another synchronization attempt is already running for this remote host",
                    )));
                }
            };

        prepare_remote_source_metadata(&self.config_store, selected, &runtime)
            .map_err(RemoteSyncError::Local)?;

        // Recover durable ingest state before reserving network budget. The
        // exact persisted position survives service restarts, so an active
        // cursor with no pagination fence is incremental while bootstrap or
        // continuation positions remain bulk. The orchestrator repeats this
        // short preflight after admission to fence concurrent changes.
        let binding = build_remote_delta_ingest_binding(selected, runtime.profile_id().clone())?;
        let mut local =
            FilesystemRemoteDeltaLocalPhases::new(runtime.ownership(), runtime.source_history());
        let position = preflight_remote_delta_position(
            &self.config_store,
            selected,
            &binding,
            &mut local,
            Utc::now(),
        )?;
        let transfer_kind = automatic_transfer_kind_for_position(&position);
        let budget_now = Utc::now();
        let fact_limits = RemoteFactSyncLimits::default();
        let reservation = match self
            .bandwidth_budget
            .begin_sync_attempt(
                selected.host().id(),
                Some(&expected_source.node_id),
                budget_now,
                transfer_kind,
                limits.max_response_bytes,
                limits.max_pages.get(),
            )
            .map_err(RemoteSyncError::Local)?
        {
            RemoteBandwidthAdmission::Granted(reservation) => reservation,
            RemoteBandwidthAdmission::Paused(pause) => {
                if pause.level() == RemoteBandwidthBudgetLevel::Hard {
                    self.process_containment_uncertain =
                        self.try_due_hard_pause_probe(selected, expected_source, budget_now);
                }
                return Err(RemoteSyncError::Local(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    RemoteBandwidthBudgetPausedError::new(pause),
                )));
            }
        };
        let mut budgeted_limits = limits;
        budgeted_limits.max_response_bytes = reservation
            .granted_response_bytes()
            .map_err(RemoteSyncError::Local)?;

        // Local phases acquire and release their own short writer/config
        // guards. No guard exists while the transport performs one-shot SSH.
        let report = match sync_remote_delta_bounded(
            &self.config_store,
            selected,
            runtime.profile_id().clone(),
            &mut local,
            &mut self.transport,
            Utc::now(),
            budgeted_limits,
        ) {
            Ok(report) => report,
            Err(error) => {
                if RemoteSyncErrorCategory::from_sync_error(&error)
                    == RemoteSyncErrorCategory::ProcessContainment
                {
                    let _ = RemoteSyncHealthStore::new(self.state_root.clone())
                        .record_process_containment_pause(
                            selected.host().id(),
                            expected_source,
                            Utc::now(),
                            selected.host(),
                        );
                    self.process_containment_uncertain = true;
                }
                // Cancel only when the error variant proves that transport
                // could not have started. Every transport, configuration-CAS,
                // protocol, remote, or local error is ambiguous after this
                // boundary and therefore retains its 24h reservation. This is
                // deliberately conservative and prevents repeated failed
                // downloads from silently bypassing the hard cap.
                if automatic_error_proves_transport_not_started(&error) {
                    let _ = self
                        .bandwidth_budget
                        .cancel_attempt(&reservation, Utc::now().max(reservation.started_at()));
                }
                return Err(error);
            }
        };
        settle_automatic_aggregate_attempt_before_final_fence(
            &self.bandwidth_budget,
            &reservation,
            &report,
            Utc::now(),
            &self.config_store,
            selected,
        )?;
        let fact_health = RemoteSyncHealthStore::new(self.state_root.clone());
        let fact_started_at = Utc::now();
        let prepared = prepare_automatic_fact_followup_without_config_lock(
            &self.config_store,
            selected,
            || prepare_replica_fact_followup(selected, &runtime, &fact_health, fact_started_at),
        )?;
        let (mut followup, fact_reservation) = match prepared {
            Err(attention) => (attention, None),
            Ok(prepared) if !prepared.requires_remote_transport() => {
                // Local fact materialization can scan the complete 35-day
                // rollout domain. The executor deliberately does not retain
                // the remotes shared lock across that work; the follow-up
                // fences only its final activation and health mutations.
                let followup = execute_prepared_replica_fact_followup(
                    prepared,
                    &self.config_store,
                    selected,
                    &runtime,
                    &fact_health,
                    &self.collect_config,
                    &mut self.fact_transport,
                    fact_started_at,
                    None,
                );
                (followup, None)
            }
            Ok(prepared) => {
                // A locally compatible fact cursor is not proof that the
                // remote still retains it. The exporter may legitimately
                // answer CursorExpired and restart a complete snapshot in
                // this same bounded run, so every automatic remote fact
                // exchange is admitted at the bulk threshold. This prevents
                // an expired cursor from tunnelling a snapshot through the
                // incremental soft-cap policy.
                let fact_transfer_kind = automatic_fact_transfer_kind();
                match self.bandwidth_budget.begin_sync_attempt(
                    selected.host().id(),
                    Some(&expected_source.node_id),
                    fact_started_at,
                    fact_transfer_kind,
                    fact_limits.max_response_bytes,
                    fact_limits.max_exchanges_per_run(),
                ) {
                    Err(_) => (ReplicaFactFollowupReport::local_state_attention(), None),
                    Ok(RemoteBandwidthAdmission::Paused(_)) => {
                        (ReplicaFactFollowupReport::resource_attention(), None)
                    }
                    Ok(RemoteBandwidthAdmission::Granted(fact_reservation)) => {
                        match fact_reservation.granted_response_bytes() {
                            Err(_) => (
                                ReplicaFactFollowupReport::local_state_attention(),
                                Some(fact_reservation),
                            ),
                            Ok(granted) => (
                                execute_prepared_replica_fact_followup(
                                    prepared,
                                    &self.config_store,
                                    selected,
                                    &runtime,
                                    &fact_health,
                                    &self.collect_config,
                                    &mut self.fact_transport,
                                    fact_started_at,
                                    fact_limits_for_response_budget(granted),
                                ),
                                Some(fact_reservation),
                            ),
                        }
                    }
                }
            }
        };
        let fact_completed_at = Utc::now();
        let fact_process_containment =
            followup.error_category() == Some(RemoteSyncErrorCategory::ProcessContainment);
        if fact_process_containment {
            // Persist the fail-closed host pause before later bandwidth,
            // profile, or metadata bookkeeping can fail. A concurrent host
            // edit changes the fingerprint and therefore cannot inherit it.
            let _ = fact_health.record_fact_sync_process_containment(
                selected.host().id(),
                expected_source,
                fact_completed_at,
                selected.host(),
            );
            self.process_containment_uncertain = true;
        }
        settle_automatic_fact_attempt_before_final_fence(
            &self.bandwidth_budget,
            fact_reservation.as_ref(),
            &mut followup,
            fact_completed_at,
            &self.config_store,
            selected,
        )?;
        with_current_automatic_selection(&self.config_store, selected, || {
            if !fact_process_containment {
                let _ = fact_health.record_fact_sync_outcome(
                    selected.host().id(),
                    expected_source,
                    fact_completed_at,
                    followup.error_category(),
                );
            }
        })?;
        profile_lease.validate().map_err(RemoteSyncError::Local)?;
        let _ = finalize_remote_source_metadata(&self.config_store, selected, &runtime)
            .map_err(RemoteSyncError::Local)?;
        Ok(report)
    }
}

fn ensure_automatic_selection_current(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
) -> Result<(), RemoteSyncError> {
    with_current_automatic_selection(config_store, selected, || ())
}

/// Runs the potentially inventory-wide, read-only fact planner without a
/// remotes lock, then admits its result only if the exact selected host and
/// config revision are still current. Execution repeats the same fence around
/// every state-changing cursor/activation step.
fn prepare_automatic_fact_followup_without_config_lock<R>(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    prepare: impl FnOnce() -> R,
) -> Result<R, RemoteSyncError> {
    let prepared = prepare();
    ensure_automatic_selection_current(config_store, selected)?;
    Ok(prepared)
}

/// Settles a successful aggregate response before rejecting a late result on
/// the final exact config fence. The bytes were already transferred, so a
/// concurrent disable/remove/edit must not leave the conservative reservation
/// charged for the rest of its 24-hour lifetime.
fn settle_automatic_aggregate_attempt_before_final_fence(
    bandwidth_budget: &RemoteBandwidthBudgetStore,
    reservation: &RemoteBandwidthReservation,
    report: &RemoteSyncReport,
    completed_at: DateTime<Utc>,
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
) -> Result<(), RemoteSyncError> {
    bandwidth_budget
        .complete_report(
            reservation,
            completed_at.max(reservation.started_at()),
            report,
        )
        .map_err(RemoteSyncError::Local)?;
    ensure_automatic_selection_current(config_store, selected)
}

/// Settles every provably pre-transport or successful fact reservation before
/// the final config fence. A concurrent disable/remove must be able to reject
/// the late result without leaving a synthetic 24-hour reservation behind.
/// Ambiguous post-transport failures remain reserved until expiry by design.
fn settle_automatic_fact_attempt_before_final_fence(
    bandwidth_budget: &RemoteBandwidthBudgetStore,
    reservation: Option<&RemoteBandwidthReservation>,
    followup: &mut ReplicaFactFollowupReport,
    completed_at: DateTime<Utc>,
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
) -> Result<(), RemoteSyncError> {
    if let Some(reservation) = reservation {
        let completed_at = completed_at.max(reservation.started_at());
        let settlement = if !followup.network_may_have_started() {
            Some(
                bandwidth_budget
                    .cancel_attempt(reservation, completed_at)
                    .map(|_| ()),
            )
        } else if followup.error_category().is_none() {
            Some(estimated_fact_network_bytes(followup).and_then(|actual| {
                bandwidth_budget
                    .complete_attempt(reservation, completed_at, actual)
                    .map(|_| ())
            }))
        } else {
            None
        };
        if settlement.is_some_and(|result| result.is_err()) {
            followup.mark_local_state_attention();
        }
    }
    ensure_automatic_selection_current(config_store, selected)
}

fn with_current_automatic_selection<R>(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    operation: impl FnOnce() -> R,
) -> Result<R, RemoteSyncError> {
    let entered = std::cell::Cell::new(false);
    match config_store.with_current_host(selected.config_revision(), selected.host(), || {
        entered.set(true);
        Ok(operation())
    }) {
        Ok(value) => Ok(value),
        Err(_) if !entered.get() => Err(RemoteSyncError::ConfigurationChanged {
            host_id: selected.host().id().to_owned(),
        }),
        Err(error) => Err(RemoteSyncError::Local(error)),
    }
}

impl<T: AutomaticRemoteProbeTransport, F> FilesystemAutomaticRemoteSyncExecutor<T, F> {
    /// Best-effort fixed-size probe. Every path remains represented to the
    /// scheduler as the original budget pause, so neither probe success nor
    /// failure can clear that pause or alter the normal failure streak.
    fn try_due_hard_pause_probe(
        &mut self,
        selected: &RemoteSyncHostSnapshot,
        expected_source: &crate::remote_protocol::SourceGeneration,
        now: chrono::DateTime<Utc>,
    ) -> bool {
        if self
            .config_store
            .with_current_host(selected.config_revision(), selected.host(), || Ok(()))
            .is_err()
        {
            return false;
        }
        let health = RemoteSyncHealthStore::new(self.state_root.clone());
        if !health
            .claim_due_budget_probe(selected.host().id(), expected_source, now)
            .unwrap_or(false)
        {
            return false;
        }

        let reservation = match self.bandwidth_budget.begin_automatic_probe_attempt(
            selected.host().id(),
            Some(&expected_source.node_id),
            now,
        ) {
            Ok(RemoteBandwidthAdmission::Granted(reservation)) => reservation,
            Ok(RemoteBandwidthAdmission::Paused(_)) | Err(_) => return false,
        };
        // Claiming the deadline and reserving bytes are local. Repeat the
        // exact revision/full-row fence immediately before SSH; a stale claim
        // simply waits until its next bounded deadline.
        if self
            .config_store
            .with_current_host(selected.config_revision(), selected.host(), || Ok(()))
            .is_err()
        {
            let _ = self
                .bandwidth_budget
                .cancel_attempt(&reservation, Utc::now().max(reservation.started_at()));
            return false;
        }

        let options = RemoteProbeOptions {
            timeout: crate::remote_sync_scheduler::AUTOMATIC_REMOTE_SYNC_EXCHANGE_TIMEOUT,
            max_response_bytes: MIN_REMOTE_RESPONSE_ENCODED_BYTES,
            check_state_writable: false,
            check_rollout_readable: false,
            redaction_profile: if selected.host().redact_content() {
                crate::source_history::RedactionProfile::Redacted
            } else {
                crate::source_history::RedactionProfile::PreviewEnabled
            },
            expected_source: Some(expected_source.clone()),
        };
        let mut process_containment_uncertain = false;
        let succeeded = match self.transport.probe_host(selected.host(), &options) {
            Ok(report)
                if report.response.source == *expected_source
                    && report.response.redaction_profile == options.redaction_profile =>
            {
                let completed_at = Utc::now().max(reservation.started_at());
                self.bandwidth_budget
                    .complete_automatic_probe_attempt(
                        &reservation,
                        completed_at,
                        report.response_bytes,
                    )
                    .is_ok()
            }
            Err(error) => {
                process_containment_uncertain = error.process_containment_uncertain();
                // The fixed reservation remains charged after an ambiguous
                // transport failure, exactly like a normal sync.
                false
            }
            Ok(_) => {
                // The fixed reservation remains charged after an unbound or
                // ambiguous transport failure, exactly like a normal sync.
                false
            }
        };
        if process_containment_uncertain {
            let _ = health.record_process_containment_pause(
                selected.host().id(),
                expected_source,
                Utc::now(),
                selected.host(),
            );
        }
        // No remote result is committed, but this final read makes the
        // full-row/config-revision fence explicit for a response that raced an
        // alias, enablement, redaction, or source-generation edit.
        if self
            .config_store
            .with_current_host(selected.config_revision(), selected.host(), || Ok(()))
            .is_ok()
        {
            let _ = health.record_budget_probe_result(
                selected.host().id(),
                expected_source,
                now,
                succeeded,
            );
        }
        process_containment_uncertain
    }
}

fn automatic_transfer_kind_for_position(
    position: &crate::remote_ingest_state::RemoteDeltaNextRequestPosition,
) -> RemoteBandwidthTransferKind {
    if position.delta_cursor.is_some() && position.exact_range.is_none() {
        RemoteBandwidthTransferKind::AutomaticIncremental
    } else {
        RemoteBandwidthTransferKind::AutomaticBulk
    }
}

fn automatic_fact_transfer_kind() -> RemoteBandwidthTransferKind {
    RemoteBandwidthTransferKind::AutomaticBulk
}

fn local_collect_config(codex_home: PathBuf, redact_content: bool) -> CollectConfig {
    CollectConfig {
        codex_home,
        redact_content,
        ..CollectConfig::default()
    }
}

fn automatic_error_proves_transport_not_started(error: &RemoteSyncError) -> bool {
    matches!(
        error,
        RemoteSyncError::HostNotPaired { .. }
            | RemoteSyncError::InvalidLimits(_)
            | RemoteSyncError::PreTransportLocal(_)
    )
}

/// Injectable config reload boundary for the worker. The production store
/// implementation performs a fresh, strictly validated read on every step.
pub trait AutomaticRemoteSyncConfigLoader {
    fn load_config(&mut self) -> io::Result<RemotesConfig>;
}

impl AutomaticRemoteSyncConfigLoader for RemotesConfigStore {
    fn load_config(&mut self) -> io::Result<RemotesConfig> {
        self.load_or_create()
    }
}

#[derive(Debug)]
struct StopState {
    requested: Arc<AtomicBool>,
    externally_set: bool,
    wait_lock: Mutex<()>,
    changed: Condvar,
}

impl Default for StopState {
    fn default() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            externally_set: false,
            wait_lock: Mutex::new(()),
            changed: Condvar::new(),
        }
    }
}

/// Cloneable cooperative stop token. A request interrupts the production
/// sleeper immediately, cancels an in-flight production OpenSSH exchange, and
/// prevents a second host from starting.
#[derive(Clone, Debug, Default)]
pub struct AutomaticRemoteSyncStopToken {
    state: Arc<StopState>,
}

impl AutomaticRemoteSyncStopToken {
    /// Shares an existing process termination flag with the worker. This is
    /// used by the recorder so one SIGINT/SIGTERM flag stops both its main
    /// loop and every aggregate/fact OpenSSH exchange.
    pub(crate) fn with_cancellation(requested: Arc<AtomicBool>) -> Self {
        Self {
            state: Arc::new(StopState {
                requested,
                externally_set: true,
                wait_lock: Mutex::new(()),
                changed: Condvar::new(),
            }),
        }
    }

    pub(crate) fn cancellation_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.state.requested)
    }

    pub fn request_stop(&self) {
        // Serialize the store with the condvar waiter so a cooperative stop
        // cannot be lost between its predicate check and wait registration.
        let _wait_guard = self
            .state
            .wait_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.state.requested.store(true, Ordering::Release);
        self.state.changed.notify_all();
    }

    pub fn is_stop_requested(&self) -> bool {
        self.state.requested.load(Ordering::Acquire)
    }

    pub(crate) fn wait_timeout(&self, duration: Duration) -> bool {
        let wait_guard = self
            .state
            .wait_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_stop_requested() {
            return true;
        }
        // signal-hook updates the shared atomic from a signal handler and
        // cannot notify this condvar. Poll that path promptly while ordinary
        // request_stop calls still wake the condvar immediately.
        const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
        let deadline = std::time::Instant::now()
            .checked_add(duration)
            .unwrap_or_else(std::time::Instant::now);
        let mut wait_guard = wait_guard;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return self.is_stop_requested();
            }
            let wait_for = if self.state.externally_set {
                remaining.min(SIGNAL_POLL_INTERVAL)
            } else {
                remaining
            };
            let (next_guard, _) = self
                .state
                .changed
                .wait_timeout(wait_guard, wait_for)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            wait_guard = next_guard;
            if self.is_stop_requested() {
                return true;
            }
        }
    }
}

/// Injectable wait boundary. Returning `true` asks the worker to exit.
pub trait AutomaticRemoteSyncSleeper {
    fn sleep_or_stop(&mut self, duration: Duration, stop: &AutomaticRemoteSyncStopToken) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InterruptibleRemoteSyncSleeper;

impl AutomaticRemoteSyncSleeper for InterruptibleRemoteSyncSleeper {
    fn sleep_or_stop(&mut self, duration: Duration, stop: &AutomaticRemoteSyncStopToken) -> bool {
        stop.wait_timeout(duration)
    }
}

/// One observable worker iteration.
#[derive(Debug)]
pub enum AutomaticRemoteSyncWorkerStep {
    Stopped,
    Scheduled(RemoteSyncSchedulerTick),
    ConfigError {
        error: io::Error,
        next_wake_in: Duration,
    },
    SchedulerError {
        error: RemoteSyncError,
        next_wake_in: Duration,
    },
}

impl AutomaticRemoteSyncWorkerStep {
    pub fn next_wake_in(&self) -> Option<Duration> {
        match self {
            Self::Stopped => None,
            Self::Scheduled(tick) => Some(tick.next_wake_in()),
            Self::ConfigError { next_wake_in, .. } | Self::SchedulerError { next_wake_in, .. } => {
                Some(*next_wake_in)
            }
        }
    }
}

/// Single-thread worker driver. It reloads configuration before every
/// scheduler tick and relies on the scheduler to process at most one host.
pub struct AutomaticRemoteSyncWorker<C, E, L, S> {
    scheduler: RemoteSyncScheduler<C>,
    executor: E,
    config_loader: L,
    sleeper: S,
    stop: AutomaticRemoteSyncStopToken,
    consecutive_config_errors: u32,
}

impl<C, E, L, S> AutomaticRemoteSyncWorker<C, E, L, S>
where
    C: RemoteSyncSchedulerClock,
    E: AutomaticRemoteSyncExecutor,
    L: AutomaticRemoteSyncConfigLoader,
    S: AutomaticRemoteSyncSleeper,
{
    pub fn new(
        scheduler: RemoteSyncScheduler<C>,
        executor: E,
        config_loader: L,
        sleeper: S,
        stop: AutomaticRemoteSyncStopToken,
    ) -> Self {
        Self {
            scheduler,
            executor,
            config_loader,
            sleeper,
            stop,
            consecutive_config_errors: 0,
        }
    }

    pub fn stop_token(&self) -> AutomaticRemoteSyncStopToken {
        self.stop.clone()
    }

    /// Executes at most one host and never sleeps.
    pub fn drive_once(&mut self) -> AutomaticRemoteSyncWorkerStep {
        if self.stop.is_stop_requested() {
            return AutomaticRemoteSyncWorkerStep::Stopped;
        }
        let config = match self.config_loader.load_config() {
            Ok(config) => config,
            Err(error) => {
                let next_wake_in = self.next_config_error_delay();
                return AutomaticRemoteSyncWorkerStep::ConfigError {
                    error,
                    next_wake_in,
                };
            }
        };
        match self.scheduler.tick(&config, &mut self.executor) {
            Ok(tick) => {
                self.consecutive_config_errors = 0;
                AutomaticRemoteSyncWorkerStep::Scheduled(tick)
            }
            Err(error) => {
                let next_wake_in = self.next_config_error_delay();
                AutomaticRemoteSyncWorkerStep::SchedulerError {
                    error,
                    next_wake_in,
                }
            }
        }
    }

    /// Runs until the stop token is requested. The observer is called once per
    /// iteration so recorder/service integration can publish diagnostics
    /// without embedding logging policy in this module.
    pub fn run_with_observer(&mut self, mut observe: impl FnMut(&AutomaticRemoteSyncWorkerStep)) {
        loop {
            let step = self.drive_once();
            observe(&step);
            let Some(delay) = step.next_wake_in() else {
                return;
            };
            let delay = delay.min(MAX_CONFIG_RELOAD_SLEEP);
            debug_assert!(!delay.is_zero());
            if self.sleeper.sleep_or_stop(delay, &self.stop) {
                return;
            }
        }
    }

    pub fn run(&mut self) {
        self.run_with_observer(|_| {});
    }

    fn next_config_error_delay(&mut self) -> Duration {
        self.consecutive_config_errors = self.consecutive_config_errors.saturating_add(1);
        let exponent = self.consecutive_config_errors.saturating_sub(1).min(63);
        let seconds = CONFIG_ERROR_RETRY_BASE
            .as_secs()
            .checked_shl(exponent)
            .unwrap_or(u64::MAX)
            .min(CONFIG_ERROR_RETRY_MAX.as_secs());
        Duration::from_secs(seconds.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::num::NonZeroU64;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use tempfile::tempdir;

    use crate::remote_bandwidth_budget::{
        REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES, RemoteBandwidthAdmission, RemoteBandwidthBudgetLevel,
        budget_pause_from_io_error,
    };
    use crate::remote_protocol::{
        ProbeResult, REMOTE_PROTOCOL_VERSION, RemoteExportResponse, RemoteExportResponseBody,
        RemoteTiming, SourceGeneration,
    };
    use crate::remote_sync::{RemoteDeltaTransport, RemoteSyncCompletion};
    use crate::remote_sync_health::RemoteSyncErrorCategory;
    use crate::remote_sync_scheduler::RemoteSyncSchedulerClock;
    use crate::remote_transport::{RemoteExchangeReport, RemoteTransportError};
    use crate::remotes_config::{RemoteHostEdit, RemotesConfigMutation};
    use crate::source_history::{RedactionProfile, SourceKind};
    use crate::source_identity::NodeId;

    const NODE_A: &str = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NODE_B: &str = "node-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[derive(Clone, Default)]
    struct FakeClock {
        seconds: Arc<AtomicUsize>,
    }

    impl RemoteSyncSchedulerClock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_secs(self.seconds.load(Ordering::SeqCst) as u64)
        }
    }

    #[derive(Default)]
    struct FakeExecutor {
        calls: Vec<String>,
    }

    impl AutomaticRemoteSyncExecutor for FakeExecutor {
        fn sync_host(
            &mut self,
            selected: &RemoteSyncHostSnapshot,
            _limits: RemoteSyncLimits,
        ) -> Result<RemoteSyncReport, RemoteSyncError> {
            self.calls.push(selected.host().id().to_owned());
            Ok(RemoteSyncReport {
                pages_committed: 0,
                changes_committed: 0,
                live_state_changed: false,
                response_bytes: 0,
                completion: RemoteSyncCompletion::Complete,
            })
        }
    }

    struct FakeLoader {
        loads: usize,
        values: VecDeque<io::Result<RemotesConfig>>,
    }

    impl FakeLoader {
        fn new(values: impl IntoIterator<Item = io::Result<RemotesConfig>>) -> Self {
            Self {
                loads: 0,
                values: values.into_iter().collect(),
            }
        }
    }

    impl AutomaticRemoteSyncConfigLoader for FakeLoader {
        fn load_config(&mut self) -> io::Result<RemotesConfig> {
            self.loads += 1;
            self.values.pop_front().unwrap_or_else(|| {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no more fake configs",
                ))
            })
        }
    }

    #[derive(Default)]
    struct StopOnFirstSleep {
        sleeps: Vec<Duration>,
    }

    impl AutomaticRemoteSyncSleeper for StopOnFirstSleep {
        fn sleep_or_stop(
            &mut self,
            duration: Duration,
            stop: &AutomaticRemoteSyncStopToken,
        ) -> bool {
            self.sleeps.push(duration);
            stop.request_stop();
            true
        }
    }

    #[derive(Default)]
    struct StopAfterSecondSleep {
        sleeps: Vec<Duration>,
    }

    impl AutomaticRemoteSyncSleeper for StopAfterSecondSleep {
        fn sleep_or_stop(
            &mut self,
            duration: Duration,
            stop: &AutomaticRemoteSyncStopToken,
        ) -> bool {
            self.sleeps.push(duration);
            if self.sleeps.len() >= 2 {
                stop.request_stop();
                true
            } else {
                false
            }
        }
    }

    #[derive(Default)]
    struct RejectingTransport {
        calls: usize,
        probe_calls: usize,
    }

    impl RemoteDeltaTransport for RejectingTransport {
        fn exchange(
            &mut self,
            _ssh_host: &str,
            _request: &crate::remote_protocol::RemoteExportRequest,
            _timeout: Duration,
        ) -> Result<
            RemoteExchangeReport<
                crate::remote_protocol::DeltaPayload,
                crate::remote_protocol::EmptyRemotePayload,
            >,
            RemoteTransportError,
        > {
            self.calls += 1;
            Err(RemoteTransportError::InvalidHost(
                "fake transport rejection".to_owned(),
            ))
        }
    }

    impl AutomaticRemoteProbeTransport for RejectingTransport {
        fn probe(
            &mut self,
            _ssh_host: &str,
            _options: &RemoteProbeOptions,
        ) -> Result<RemoteProbeReport, RemoteTransportError> {
            self.probe_calls += 1;
            Err(RemoteTransportError::InvalidHost(
                "fake probe rejection".to_owned(),
            ))
        }
    }

    #[derive(Default)]
    struct CompleteAutomaticTransport {
        calls: usize,
    }

    impl RemoteDeltaTransport for CompleteAutomaticTransport {
        fn exchange(
            &mut self,
            _ssh_host: &str,
            request: &crate::remote_protocol::RemoteExportRequest,
            _timeout: Duration,
        ) -> Result<
            RemoteExchangeReport<
                crate::remote_protocol::DeltaPayload,
                crate::remote_protocol::EmptyRemotePayload,
            >,
            RemoteTransportError,
        > {
            use crate::remote_protocol::{
                BinaryVersion, DeltaCursor, DeltaPage, DeltaPayload, RemoteDeltaCoverage,
                RemoteDeltaStats, RemoteExportRequestBody, RemoteExportResponse,
                RemoteExportResponseBody, RemoteLiveSnapshot, RemoteLiveState, RemoteTiming,
            };

            let RemoteExportRequestBody::Delta(delta) = &request.request else {
                panic!("automatic aggregate transport received a fact request")
            };
            self.calls += 1;
            let generation = delta
                .delta_cursor
                .map_or_else(|| NonZeroU64::new(1).unwrap(), |cursor| cursor.generation);
            let sequence = delta.delta_cursor.map_or(0, |cursor| cursor.sequence);
            let observed_at = delta.range.to;
            Ok(RemoteExchangeReport {
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
                elapsed: Duration::from_millis(1),
                request_bytes: 64,
                response_bytes: 256,
                response_decoded_bytes: 256,
                stderr_bytes: 0,
            })
        }
    }

    impl AutomaticRemoteProbeTransport for CompleteAutomaticTransport {
        fn probe(
            &mut self,
            _ssh_host: &str,
            _options: &RemoteProbeOptions,
        ) -> Result<RemoteProbeReport, RemoteTransportError> {
            panic!("an admitted ordinary sync must not probe")
        }
    }

    struct DisablingAutomaticTransport {
        inner: CompleteAutomaticTransport,
        store: RemotesConfigStore,
        revision: u64,
    }

    impl RemoteDeltaTransport for DisablingAutomaticTransport {
        fn exchange(
            &mut self,
            ssh_host: &str,
            request: &crate::remote_protocol::RemoteExportRequest,
            timeout: Duration,
        ) -> Result<
            RemoteExchangeReport<
                crate::remote_protocol::DeltaPayload,
                crate::remote_protocol::EmptyRemotePayload,
            >,
            RemoteTransportError,
        > {
            let response = self.inner.exchange(ssh_host, request, timeout)?;
            self.store
                .update(
                    self.revision,
                    RemotesConfigMutation::set_auto_sync_enabled(false),
                )
                .unwrap();
            Ok(response)
        }
    }

    impl AutomaticRemoteProbeTransport for DisablingAutomaticTransport {
        fn probe(
            &mut self,
            _ssh_host: &str,
            _options: &RemoteProbeOptions,
        ) -> Result<RemoteProbeReport, RemoteTransportError> {
            panic!("an admitted ordinary sync must not probe")
        }
    }

    struct ProbeTestTransport {
        probe_succeeds: bool,
        process_containment_uncertain: bool,
        rotate_generation_during_probe: Option<RemotesConfigStore>,
        data_calls: usize,
        probe_calls: usize,
        observed_options: Option<RemoteProbeOptions>,
    }

    impl ProbeTestTransport {
        fn new(probe_succeeds: bool) -> Self {
            Self {
                probe_succeeds,
                process_containment_uncertain: false,
                rotate_generation_during_probe: None,
                data_calls: 0,
                probe_calls: 0,
                observed_options: None,
            }
        }

        fn with_process_containment_uncertain(mut self) -> Self {
            self.process_containment_uncertain = true;
            self
        }
    }

    impl RemoteDeltaTransport for ProbeTestTransport {
        fn exchange(
            &mut self,
            _ssh_host: &str,
            _request: &crate::remote_protocol::RemoteExportRequest,
            _timeout: Duration,
        ) -> Result<
            RemoteExchangeReport<
                crate::remote_protocol::DeltaPayload,
                crate::remote_protocol::EmptyRemotePayload,
            >,
            RemoteTransportError,
        > {
            self.data_calls += 1;
            Err(RemoteTransportError::InvalidHost(
                "data transport must remain paused".to_owned(),
            ))
        }
    }

    impl AutomaticRemoteProbeTransport for ProbeTestTransport {
        fn probe(
            &mut self,
            _ssh_host: &str,
            options: &RemoteProbeOptions,
        ) -> Result<RemoteProbeReport, RemoteTransportError> {
            self.probe_calls += 1;
            self.observed_options = Some(options.clone());
            if let Some(store) = self.rotate_generation_during_probe.as_ref() {
                let config = store.load().unwrap();
                let config = store
                    .update(
                        config.config_revision(),
                        RemotesConfigMutation::unpair_host("dev"),
                    )
                    .unwrap();
                let mut rotated = options.expected_source.clone().unwrap();
                rotated.generation = NonZeroU64::new(rotated.generation.get() + 1).unwrap();
                store
                    .update(
                        config.config_revision(),
                        RemotesConfigMutation::pair_pin("dev", rotated),
                    )
                    .unwrap();
            }
            if !self.probe_succeeds {
                if self.process_containment_uncertain {
                    return Err(RemoteTransportError::Cancelled {
                        cleanup_error: Some(io::Error::other(
                            "fake probe helper escaped process containment",
                        )),
                    });
                }
                return Err(RemoteTransportError::InvalidHost(
                    "fake due probe failure".to_owned(),
                ));
            }
            let observed_at = Utc::now();
            Ok(RemoteProbeReport {
                response: RemoteExportResponse {
                    protocol_version: REMOTE_PROTOCOL_VERSION,
                    server_version: env!("CARGO_PKG_VERSION").parse().unwrap(),
                    source: options.expected_source.clone().unwrap(),
                    redaction_profile: options.redaction_profile,
                    revisions: crate::remote_agent::current_revisions(),
                    observed_at,
                    timing: RemoteTiming {
                        remote_received_at: observed_at,
                        remote_sent_at: observed_at,
                    },
                    result: RemoteExportResponseBody::Probe(ProbeResult {
                        capabilities: Vec::new(),
                        state_writable: true,
                        rollout_readable: true,
                    }),
                },
                elapsed: Duration::from_millis(1),
                request_bytes: 512,
                response_bytes: 1_337,
                stderr_bytes: 0,
            })
        }
    }

    fn source(node_id: &str) -> SourceGeneration {
        SourceGeneration {
            node_id: node_id.parse().unwrap(),
            generation: NonZeroU64::new(1).unwrap(),
        }
    }

    fn automatic_config(node_id: &str) -> RemotesConfig {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "configRevision": 4,
            "autoSyncEnabled": true,
            "activeIntervalSeconds": 30,
            "idleIntervalSeconds": 60,
            "hosts": [{
                "id": "dev",
                "sshHost": "dev-alias",
                "syncEnabled": true,
                "redactContent": false,
                "expectedSource": {"nodeId": node_id, "generation": 1}
            }]
        }))
        .unwrap()
    }

    fn configured_store(
        path: PathBuf,
        source: SourceGeneration,
    ) -> (RemotesConfigStore, RemotesConfig) {
        let store = RemotesConfigStore::new(path);
        let mut config = store.load_or_create().unwrap();
        config = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        config = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::pair_pin("dev", source),
            )
            .unwrap();
        config = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: None,
                        agent_executable: None,
                        redact_content: Some(false),
                    },
                ),
            )
            .unwrap();
        config = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        config = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        (store, config)
    }

    fn seed_due_hard_pause(
        state_root: &std::path::Path,
        expected_source: &SourceGeneration,
        now: chrono::DateTime<Utc>,
    ) {
        let paused_at = now - chrono::TimeDelta::minutes(31);
        let health = RemoteSyncHealthStore::new(state_root.to_path_buf());
        health
            .record_failure(
                "dev",
                Some(expected_source),
                paused_at,
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        health
            .record_pause(
                "dev",
                Some(expected_source),
                paused_at,
                RemoteBandwidthBudgetLevel::Hard,
                Some(now + chrono::TimeDelta::hours(24)),
            )
            .unwrap();

        let budget = RemoteBandwidthBudgetStore::new(state_root.to_path_buf());
        let RemoteBandwidthAdmission::Granted(reservation) = budget
            .begin_attempt(
                "dev",
                Some(&expected_source.node_id),
                now,
                RemoteBandwidthTransferKind::ManualOverride,
                1,
            )
            .unwrap()
        else {
            panic!("manual override must reserve hard-cap fixture bytes")
        };
        budget
            .complete_attempt(
                &reservation,
                now,
                usize::try_from(crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_HARD_LIMIT_BYTES)
                    .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn default_disabled_worker_makes_zero_executor_calls_and_sleeps_positively() {
        let stop = AutomaticRemoteSyncStopToken::default();
        let mut worker = AutomaticRemoteSyncWorker::new(
            RemoteSyncScheduler::new(FakeClock::default()),
            FakeExecutor::default(),
            FakeLoader::new([Ok(RemotesConfig::default())]),
            StopOnFirstSleep::default(),
            stop.clone(),
        );

        worker.run();

        assert!(stop.is_stop_requested());
        assert!(worker.executor.calls.is_empty());
        assert_eq!(worker.config_loader.loads, 1);
        assert_eq!(worker.sleeper.sleeps.len(), 1);
        assert_eq!(worker.sleeper.sleeps[0], MAX_CONFIG_RELOAD_SLEEP);
    }

    #[test]
    fn disabled_worker_reloads_cross_process_configuration_before_the_idle_interval() {
        let stop = AutomaticRemoteSyncStopToken::default();
        let mut worker = AutomaticRemoteSyncWorker::new(
            RemoteSyncScheduler::new(FakeClock::default()),
            FakeExecutor::default(),
            FakeLoader::new([Ok(RemotesConfig::default()), Ok(automatic_config(NODE_A))]),
            StopAfterSecondSleep::default(),
            stop.clone(),
        );

        worker.run();

        assert!(stop.is_stop_requested());
        assert_eq!(worker.config_loader.loads, 2);
        assert_eq!(worker.executor.calls, ["dev"]);
        assert_eq!(worker.sleeper.sleeps[0], MAX_CONFIG_RELOAD_SLEEP);
    }

    #[test]
    fn missing_remotes_config_is_created_disabled_without_an_executor_call() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config/remotes.json");
        let store = RemotesConfigStore::new(config_path.clone());
        let mut worker = AutomaticRemoteSyncWorker::new(
            RemoteSyncScheduler::new(FakeClock::default()),
            FakeExecutor::default(),
            store,
            StopOnFirstSleep::default(),
            AutomaticRemoteSyncStopToken::default(),
        );

        let step = worker.drive_once();

        assert!(matches!(
            step,
            AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Disabled { .. })
        ));
        assert!(worker.executor.calls.is_empty());
        assert!(config_path.is_file());
    }

    #[test]
    fn one_drive_processes_only_the_single_automatic_host() {
        let mut worker = AutomaticRemoteSyncWorker::new(
            RemoteSyncScheduler::new(FakeClock::default()),
            FakeExecutor::default(),
            FakeLoader::new([Ok(automatic_config(NODE_A))]),
            StopOnFirstSleep::default(),
            AutomaticRemoteSyncStopToken::default(),
        );

        let step = worker.drive_once();

        let AutomaticRemoteSyncWorkerStep::Scheduled(RemoteSyncSchedulerTick::Attempted {
            host_id,
            source,
            ..
        }) = step
        else {
            panic!("expected one scheduled host attempt");
        };
        assert_eq!(host_id, "dev");
        let source = source.unwrap();
        assert_eq!(source.node_id.as_str(), NODE_A);
        assert_eq!(source.generation.get(), 1);
        assert_eq!(worker.executor.calls, ["dev"]);
        assert_eq!(worker.config_loader.loads, 1);
    }

    #[test]
    fn config_failures_have_positive_bounded_exponential_backoff() {
        let errors =
            (0..6).map(|_| Err(io::Error::new(io::ErrorKind::InvalidData, "broken config")));
        let mut worker = AutomaticRemoteSyncWorker::new(
            RemoteSyncScheduler::new(FakeClock::default()),
            FakeExecutor::default(),
            FakeLoader::new(errors),
            StopOnFirstSleep::default(),
            AutomaticRemoteSyncStopToken::default(),
        );
        let mut delays = Vec::new();
        for _ in 0..6 {
            let step = worker.drive_once();
            let AutomaticRemoteSyncWorkerStep::ConfigError { next_wake_in, .. } = step else {
                panic!("expected a config error");
            };
            delays.push(next_wake_in);
        }
        assert_eq!(delays[0], Duration::from_secs(30));
        assert_eq!(delays[1], Duration::from_secs(60));
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(delays.iter().all(|delay| !delay.is_zero()));
        assert_eq!(*delays.last().unwrap(), Duration::from_secs(300));
        assert!(worker.executor.calls.is_empty());
    }

    #[test]
    fn pre_requested_stop_does_not_reload_or_execute() {
        let stop = AutomaticRemoteSyncStopToken::default();
        stop.request_stop();
        let mut worker = AutomaticRemoteSyncWorker::new(
            RemoteSyncScheduler::new(FakeClock::default()),
            FakeExecutor::default(),
            FakeLoader::new([Ok(automatic_config(NODE_A))]),
            StopOnFirstSleep::default(),
            stop,
        );
        assert!(matches!(
            worker.drive_once(),
            AutomaticRemoteSyncWorkerStep::Stopped
        ));
        assert_eq!(worker.config_loader.loads, 0);
        assert!(worker.executor.calls.is_empty());
    }

    #[test]
    fn external_termination_flag_interrupts_a_sleeping_worker_promptly() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let stop = AutomaticRemoteSyncStopToken::with_cancellation(Arc::clone(&cancellation));
        let (waiting_sender, waiting_receiver) = mpsc::sync_channel(1);
        let waiter = thread::spawn(move || {
            waiting_sender.send(()).unwrap();
            stop.wait_timeout(Duration::from_secs(30))
        });
        waiting_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        thread::sleep(Duration::from_millis(100));

        let started = std::time::Instant::now();
        cancellation.store(true, Ordering::Release);

        assert!(waiter.join().unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn production_executor_requires_explicit_cutover_before_transport() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let (store, config) =
            configured_store(directory.path().join("config/remotes.json"), source(NODE_A));
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );

        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteSyncError::Local(ref error)
                if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert_eq!(executor.transport.calls, 0);
    }

    #[test]
    fn production_executor_rejects_local_redaction_mismatch_before_state_or_transport() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let (store, config) =
            configured_store(directory.path().join("config/remotes.json"), source(NODE_A));
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            codex_home,
            store,
            true,
            AutomaticRemoteCutoverPolicy::LegacyWritersQuiescedAndPrevalidated,
            RejectingTransport::default(),
        );

        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteSyncError::Local(ref error)
                if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert!(!state_root.exists());
        assert_eq!(executor.transport.calls, 0);
    }

    #[test]
    fn production_executor_rejects_an_opposite_active_history_profile_before_transport() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let mut preview =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        preview.ensure_v2_active().unwrap();
        let remote_node = if preview.source_identity().node_id().as_str() == NODE_A {
            NODE_B
        } else {
            NODE_A
        };
        let redacted =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, true).unwrap();
        let _opposite_profile = match try_acquire_history_profile_lease(
            redacted.state_root(),
            redacted.profile_id().clone(),
            redacted.redaction_profile(),
        )
        .unwrap()
        {
            TryHistoryProfileLease::Acquired(guard) => guard,
            TryHistoryProfileLease::Busy { .. } => panic!("test profile was unexpectedly busy"),
        };
        drop(preview);

        let (store, config) = configured_store(
            directory.path().join("config/remotes.json"),
            source(remote_node),
        );
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );

        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();
        assert!(matches!(
            error,
            RemoteSyncError::Local(ref error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        assert_eq!(executor.transport.calls, 0);
    }

    #[test]
    fn production_executor_rejects_local_identity_collision_before_transport() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        let local_source = SourceGeneration {
            node_id: runtime.source_identity().node_id().clone(),
            generation: NonZeroU64::new(runtime.source_identity().generation()).unwrap(),
        };
        drop(runtime);
        let (store, config) =
            configured_store(directory.path().join("config/remotes.json"), local_source);
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            store,
            false,
            AutomaticRemoteCutoverPolicy::LegacyWritersQuiescedAndPrevalidated,
            RejectingTransport::default(),
        );

        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteSyncError::Local(ref error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(executor.transport.calls, 0);
    }

    #[test]
    fn v2_executor_registers_exact_ssh_metadata_before_transport() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let mut runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        runtime.ensure_v2_active().unwrap();
        let remote_node = if runtime.source_identity().node_id().as_str() == NODE_A {
            NODE_B
        } else {
            NODE_A
        };
        drop(runtime);
        let (store, config) = configured_store(
            directory.path().join("config/remotes.json"),
            source(remote_node),
        );
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            codex_home.clone(),
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );

        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();
        assert!(
            matches!(error, RemoteSyncError::Transport(_)),
            "unexpected sync error: {error:?}"
        );
        assert_eq!(executor.transport.calls, 1);

        let runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        let metadata = runtime
            .source_history()
            .load_source_metadata(&remote_node.parse::<NodeId>().unwrap())
            .unwrap();
        assert_eq!(metadata.kind(), SourceKind::Ssh);
        assert_eq!(metadata.display_label(), "dev");
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::PreviewEnabled
        );
    }

    #[test]
    fn successful_automatic_aggregate_runs_one_fact_plan_and_settles_actual_bytes() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let mut runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        runtime.ensure_v2_active().unwrap();
        let remote_node = if runtime.source_identity().node_id().as_str() == NODE_A {
            NODE_B
        } else {
            NODE_A
        };
        drop(runtime);
        let (store, config) = configured_store(
            directory.path().join("config/remotes.json"),
            source(remote_node),
        );
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let expected_source = selected.host().expected_source().unwrap().clone();
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            codex_home,
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            CompleteAutomaticTransport::default(),
        );

        let report = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap();

        assert_eq!(report.completion, RemoteSyncCompletion::Complete);
        assert_eq!(executor.transport.calls, 1);
        let health = RemoteSyncHealthStore::new(state_root.clone())
            .get("dev")
            .unwrap()
            .unwrap();
        assert!(health.last_fact_sync_at().is_some());
        assert_eq!(health.fact_sync_error_category(), None);
        let usage = RemoteBandwidthBudgetStore::new(state_root)
            .usage("dev", Some(&expected_source.node_id), Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(
            usage.committed_bytes(),
            u64::try_from(
                256 + crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES
            )
            .unwrap()
        );
        assert_eq!(usage.reserved_bytes(), 0);
    }

    #[test]
    fn config_change_after_successful_aggregate_settles_actual_bytes_before_final_fence() {
        for mutation_name in ["disable", "remove", "edit", "global-off"] {
            let directory = tempdir().unwrap();
            let state_root = directory.path().join("state");
            let expected_source = source(NODE_A);
            let (store, config) = configured_store(
                directory.path().join("config/remotes.json"),
                expected_source.clone(),
            );
            let selected =
                RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                    .unwrap();
            let budget = RemoteBandwidthBudgetStore::new(state_root);
            let limits = RemoteSyncLimits::default();
            let started_at = Utc::now();
            let RemoteBandwidthAdmission::Granted(reservation) = budget
                .begin_sync_attempt(
                    "dev",
                    Some(&expected_source.node_id),
                    started_at,
                    RemoteBandwidthTransferKind::AutomaticBulk,
                    limits.max_response_bytes,
                    limits.max_pages.get(),
                )
                .unwrap()
            else {
                panic!("aggregate reservation should be admitted")
            };
            let report = RemoteSyncReport {
                pages_committed: 1,
                changes_committed: 0,
                live_state_changed: false,
                response_bytes: 256,
                completion: RemoteSyncCompletion::Complete,
            };
            let mutation = match mutation_name {
                "disable" => RemotesConfigMutation::disable_host("dev"),
                "remove" => RemotesConfigMutation::remove_host("dev"),
                "edit" => RemotesConfigMutation::edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: Some("changed-alias".to_owned()),
                        agent_executable: None,
                        redact_content: None,
                    },
                ),
                "global-off" => RemotesConfigMutation::set_auto_sync_enabled(false),
                _ => unreachable!(),
            };
            store.update(config.config_revision(), mutation).unwrap();

            let error = settle_automatic_aggregate_attempt_before_final_fence(
                &budget,
                &reservation,
                &report,
                Utc::now(),
                &store,
                &selected,
            )
            .unwrap_err();

            assert!(matches!(
                error,
                RemoteSyncError::ConfigurationChanged { .. }
            ));
            let usage = budget
                .usage("dev", Some(&expected_source.node_id), Utc::now())
                .unwrap()
                .unwrap();
            assert_eq!(
                usage.committed_bytes(),
                u64::try_from(
                    report.response_bytes
                        + crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES
                )
                .unwrap(),
                "{mutation_name} must settle the successful aggregate response"
            );
            assert_eq!(
                usage.reserved_bytes(),
                0,
                "{mutation_name} must not strand the aggregate reservation"
            );
        }
    }

    #[test]
    fn config_change_after_pretransport_fact_admission_releases_reservation_before_fence() {
        for remove_host in [false, true] {
            let directory = tempdir().unwrap();
            let state_root = directory.path().join("state");
            let expected_source = source(NODE_A);
            let (store, config) = configured_store(
                directory.path().join("config/remotes.json"),
                expected_source.clone(),
            );
            let selected =
                RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                    .unwrap();
            let budget = RemoteBandwidthBudgetStore::new(state_root);
            let limits = RemoteFactSyncLimits::default();
            let started_at = Utc::now();
            let RemoteBandwidthAdmission::Granted(reservation) = budget
                .begin_sync_attempt(
                    "dev",
                    Some(&expected_source.node_id),
                    started_at,
                    automatic_fact_transfer_kind(),
                    limits.max_response_bytes,
                    limits.max_exchanges_per_run(),
                )
                .unwrap()
            else {
                panic!("fact reservation should be admitted")
            };
            let mutation = if remove_host {
                RemotesConfigMutation::remove_host("dev")
            } else {
                RemotesConfigMutation::set_auto_sync_enabled(false)
            };
            store.update(config.config_revision(), mutation).unwrap();

            let mut followup = ReplicaFactFollowupReport::local_state_attention();
            let error = settle_automatic_fact_attempt_before_final_fence(
                &budget,
                Some(&reservation),
                &mut followup,
                Utc::now(),
                &store,
                &selected,
            )
            .unwrap_err();

            assert!(matches!(
                error,
                RemoteSyncError::ConfigurationChanged { .. }
            ));
            let usage = budget
                .usage("dev", Some(&expected_source.node_id), Utc::now())
                .unwrap()
                .unwrap();
            assert_eq!(
                usage.reserved_bytes(),
                0,
                "{} must not strand the pre-transport fact reservation",
                if remove_host { "remove" } else { "disable" }
            );
        }
    }

    #[test]
    fn blocking_fact_planner_does_not_block_disable_or_remove_and_late_plan_is_rejected() {
        for remove_host in [false, true] {
            let directory = tempdir().unwrap();
            let (store, config) =
                configured_store(directory.path().join("config/remotes.json"), source(NODE_A));
            let selected =
                RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                    .unwrap();
            let worker_store = store.clone();
            let (planner_started_tx, planner_started_rx) = mpsc::channel();
            let (release_planner_tx, release_planner_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                prepare_automatic_fact_followup_without_config_lock(
                    &worker_store,
                    &selected,
                    || {
                        planner_started_tx.send(()).unwrap();
                        release_planner_rx.recv().unwrap();
                    },
                )
            });
            planner_started_rx
                .recv_timeout(StdDuration::from_secs(1))
                .expect("the synthetic fact planner should block before its post-plan fence");

            let mutation_store = store.clone();
            let revision = config.config_revision();
            let (mutation_done_tx, mutation_done_rx) = mpsc::channel();
            let mutator = thread::spawn(move || {
                let mutation = if remove_host {
                    RemotesConfigMutation::remove_host("dev")
                } else {
                    RemotesConfigMutation::set_auto_sync_enabled(false)
                };
                mutation_done_tx
                    .send(mutation_store.update(revision, mutation))
                    .unwrap();
            });
            let changed = mutation_done_rx.recv_timeout(StdDuration::from_secs(1));
            if changed.is_err() {
                release_planner_tx.send(()).unwrap();
                let _ = worker.join();
                let _ = mutator.join();
                panic!(
                    "{} was blocked by read-only replica-fact planning",
                    if remove_host { "remove" } else { "disable" }
                );
            }
            changed.unwrap().unwrap();
            mutator.join().unwrap();

            release_planner_tx.send(()).unwrap();
            let error = worker.join().unwrap().unwrap_err();
            assert!(matches!(
                error,
                RemoteSyncError::ConfigurationChanged { .. }
            ));
        }
    }

    #[test]
    fn disabling_automatic_sync_during_ssh_discards_response_before_local_commit() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let mut runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        runtime.ensure_v2_active().unwrap();
        let remote_node = if runtime.source_identity().node_id().as_str() == NODE_A {
            NODE_B
        } else {
            NODE_A
        };
        drop(runtime);
        let (store, config) = configured_store(
            directory.path().join("config/remotes.json"),
            source(remote_node),
        );
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let transport = DisablingAutomaticTransport {
            inner: CompleteAutomaticTransport::default(),
            store: store.clone(),
            revision: config.config_revision(),
        };
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            codex_home.clone(),
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            transport,
        );

        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();
        assert!(matches!(
            error,
            RemoteSyncError::ConfigurationChanged { .. }
        ));
        assert_eq!(executor.transport.inner.calls, 1);

        let runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        let snapshot = runtime
            .source_history()
            .load_remote_history_snapshot_since(
                &remote_node.parse().unwrap(),
                RedactionProfile::PreviewEnabled,
                chrono::DateTime::<Utc>::MIN_UTC,
            )
            .unwrap();
        assert!(snapshot.active_ref.is_none());
        assert!(
            RemoteSyncHealthStore::new(state_root)
                .get("dev")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn automatic_bulk_soft_pause_is_structured_and_opens_no_transport() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let mut runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        runtime.ensure_v2_active().unwrap();
        let remote_node = if runtime.source_identity().node_id().as_str() == NODE_A {
            NODE_B
        } else {
            NODE_A
        };
        drop(runtime);
        let (store, config) = configured_store(
            directory.path().join("config/remotes.json"),
            source(remote_node),
        );
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();

        let budget = RemoteBandwidthBudgetStore::new(state_root.clone());
        let now = Utc::now();
        let RemoteBandwidthAdmission::Granted(reservation) = budget
            .begin_attempt(
                "dev",
                Some(&remote_node.parse().unwrap()),
                now,
                RemoteBandwidthTransferKind::ManualOverride,
                1,
            )
            .unwrap()
        else {
            panic!("manual override must reserve budget");
        };
        budget
            .complete_attempt(
                &reservation,
                now,
                usize::try_from(REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES).unwrap(),
            )
            .unwrap();

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();
        let RemoteSyncError::Local(error) = error else {
            panic!("budget pause must use the typed local-error boundary");
        };
        let pause = budget_pause_from_io_error(&error).expect("typed budget pause");
        assert_eq!(pause.level(), RemoteBandwidthBudgetLevel::Soft);
        assert_eq!(executor.transport.calls, 0);
    }

    #[test]
    fn hard_pause_probe_waits_for_durable_deadline_without_connecting() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let (store, config) =
            configured_store(directory.path().join("config/remotes.json"), source(NODE_A));
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let expected_source = selected.host().expected_source().unwrap().clone();
        let now = Utc::now();
        RemoteSyncHealthStore::new(state_root.clone())
            .record_pause(
                "dev",
                Some(&expected_source),
                now,
                RemoteBandwidthBudgetLevel::Hard,
                Some(now + chrono::TimeDelta::hours(24)),
            )
            .unwrap();
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            directory.path().join("codex-home"),
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            ProbeTestTransport::new(true),
        );

        executor.try_due_hard_pause_probe(&selected, &expected_source, now);

        assert_eq!(executor.transport.probe_calls, 0);
        assert_eq!(executor.transport.data_calls, 0);
        let health = RemoteSyncHealthStore::new(state_root)
            .get("dev")
            .unwrap()
            .unwrap();
        assert_eq!(health.budget_last_probe_at(), None);
        assert!(health.budget_paused());
    }

    #[test]
    fn due_probe_success_and_failure_are_fixed_bounded_accounted_and_stay_paused() {
        for succeeds in [true, false] {
            let directory = tempdir().unwrap();
            let state_root = directory.path().join("state");
            let (store, config) =
                configured_store(directory.path().join("config/remotes.json"), source(NODE_A));
            let selected =
                RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                    .unwrap();
            let expected_source = selected.host().expected_source().unwrap().clone();
            let now = Utc::now();
            seed_due_hard_pause(&state_root, &expected_source, now);
            let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
                state_root.clone(),
                directory.path().join("codex-home"),
                store,
                false,
                AutomaticRemoteCutoverPolicy::RequireV2Active,
                ProbeTestTransport::new(succeeds),
            );

            executor.try_due_hard_pause_probe(&selected, &expected_source, now);
            executor.try_due_hard_pause_probe(
                &selected,
                &expected_source,
                now + chrono::TimeDelta::minutes(1),
            );

            assert_eq!(executor.transport.probe_calls, 1);
            assert_eq!(executor.transport.data_calls, 0);
            let options = executor.transport.observed_options.as_ref().unwrap();
            assert_eq!(
                options.max_response_bytes,
                MIN_REMOTE_RESPONSE_ENCODED_BYTES
            );
            assert!(!options.check_state_writable);
            assert!(!options.check_rollout_readable);
            assert_eq!(options.expected_source.as_ref(), Some(&expected_source));
            let health = RemoteSyncHealthStore::new(state_root.clone())
                .get("dev")
                .unwrap()
                .unwrap();
            assert!(health.budget_paused());
            assert_eq!(health.consecutive_failures(), 1);
            assert_eq!(health.last_result(), Some(RemoteSyncAttemptResult::Failure));
            assert_eq!(health.budget_last_probe_at(), Some(now));
            assert_eq!(health.budget_last_probe_succeeded(), Some(succeeds));
            assert_eq!(
                health.budget_probe_due_at(),
                Some(now + chrono::TimeDelta::minutes(30))
            );
            let usage = RemoteBandwidthBudgetStore::new(state_root)
                .usage("dev", Some(&expected_source.node_id), Utc::now())
                .unwrap()
                .unwrap();
            assert!(
                usage.rolling_bytes()
                    > crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_HARD_LIMIT_BYTES
            );
        }
    }

    #[test]
    fn due_probe_containment_failure_durably_pauses_host_and_signals_scheduler() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let (store, config) =
            configured_store(directory.path().join("config/remotes.json"), source(NODE_A));
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let expected_source = selected.host().expected_source().unwrap().clone();
        let now = Utc::now();
        seed_due_hard_pause(&state_root, &expected_source, now);
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            directory.path().join("codex-home"),
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            ProbeTestTransport::new(false).with_process_containment_uncertain(),
        );

        assert!(executor.try_due_hard_pause_probe(&selected, &expected_source, now));

        assert_eq!(executor.transport.probe_calls, 1);
        assert_eq!(executor.transport.data_calls, 0);
        let health = RemoteSyncHealthStore::new(state_root)
            .get("dev")
            .unwrap()
            .unwrap();
        assert!(health.budget_paused());
        assert!(health.process_containment_paused_for(selected.host()));
        assert_eq!(health.budget_last_probe_succeeded(), Some(false));
    }

    #[test]
    fn source_generation_change_during_probe_discards_its_health_result() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let (store, config) =
            configured_store(directory.path().join("config/remotes.json"), source(NODE_A));
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        let expected_source = selected.host().expected_source().unwrap().clone();
        let now = Utc::now();
        seed_due_hard_pause(&state_root, &expected_source, now);
        let mut transport = ProbeTestTransport::new(true);
        transport.rotate_generation_during_probe = Some(store.clone());
        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            directory.path().join("codex-home"),
            store.clone(),
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            transport,
        );

        executor.try_due_hard_pause_probe(&selected, &expected_source, now);

        assert_eq!(executor.transport.probe_calls, 1);
        let health = RemoteSyncHealthStore::new(state_root)
            .get("dev")
            .unwrap()
            .unwrap();
        assert!(health.budget_paused());
        assert_eq!(health.consecutive_failures(), 1);
        assert_eq!(health.budget_last_probe_succeeded(), None);
        let rotated = store.load().unwrap();
        assert_ne!(
            rotated.host("dev").unwrap().expected_source(),
            Some(&expected_source)
        );
    }

    #[test]
    fn failing_transport_preserves_an_existing_aggregate_profile() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let mut runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        runtime.ensure_v2_active().unwrap();
        let remote_node = if runtime.source_identity().node_id().as_str() == NODE_A {
            NODE_B
        } else {
            NODE_A
        };
        let (store, config) = configured_store(
            directory.path().join("config/remotes.json"),
            source(remote_node),
        );
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();
        prepare_remote_source_metadata(&store, &selected, &runtime).unwrap();
        {
            let lease = runtime.ownership().acquire_writer_lease().unwrap();
            let OwnershipManifestStatus::Initialized(manifest) =
                runtime.ownership().load_manifest().unwrap()
            else {
                panic!("test runtime ownership is initialized")
            };
            let authority = runtime
                .ownership()
                .authorize_v2_write(&lease, &manifest)
                .unwrap();
            let writer = runtime.source_history().writer(&authority).unwrap();
            writer
                .update_source_metadata(&remote_node.parse().unwrap(), |metadata| {
                    metadata.set_aggregate_redaction_profile(RedactionProfile::Redacted);
                    metadata.set_include_in_aggregates(false);
                    metadata.set_detached(true);
                    Ok(())
                })
                .unwrap();
        }
        drop(runtime);

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root.clone(),
            codex_home.clone(),
            store,
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        let error = executor
            .sync_host(&selected, RemoteSyncLimits::default())
            .unwrap_err();
        assert!(matches!(error, RemoteSyncError::Transport(_)));

        let runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        let metadata = runtime
            .source_history()
            .load_source_metadata(&remote_node.parse::<NodeId>().unwrap())
            .unwrap();
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::Redacted
        );
        assert!(!metadata.include_in_aggregates());
        assert!(metadata.detached());
    }

    #[test]
    fn bandwidth_reservation_is_released_only_for_provably_pre_transport_errors() {
        assert!(automatic_error_proves_transport_not_started(
            &RemoteSyncError::InvalidLimits("invalid limits")
        ));
        assert!(automatic_error_proves_transport_not_started(
            &RemoteSyncError::HostNotPaired {
                host_id: "dev".to_owned(),
            }
        ));
        assert!(automatic_error_proves_transport_not_started(
            &RemoteSyncError::PreTransportLocal(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer busy",
            ))
        ));
        assert!(!automatic_error_proves_transport_not_started(
            &RemoteSyncError::ConfigurationChanged {
                host_id: "dev".to_owned(),
            }
        ));
        assert!(!automatic_error_proves_transport_not_started(
            &RemoteSyncError::Transport(RemoteTransportError::InvalidHost(
                "transport may already have transferred bytes".to_owned(),
            ))
        ));
        assert!(!automatic_error_proves_transport_not_started(
            &RemoteSyncError::InvalidStartedAt
        ));
    }

    #[test]
    fn filesystem_executor_restores_exact_durable_deadline_and_failure_streak() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        drop(runtime);

        let config = automatic_config(NODE_A);
        let host = config.host("dev").unwrap();
        let expected_source = host.expected_source().unwrap().clone();
        let completed_at = Utc::now();
        let next_eligible_at = completed_at + chrono::TimeDelta::seconds(120);
        let health = RemoteSyncHealthStore::new(state_root.clone());
        health
            .record_failure(
                host.id(),
                Some(&expected_source),
                completed_at,
                RemoteSyncErrorCategory::Transport,
                Some(next_eligible_at),
            )
            .unwrap();
        health
            .record_failure(
                host.id(),
                Some(&expected_source),
                completed_at,
                RemoteSyncErrorCategory::Transport,
                Some(next_eligible_at),
            )
            .unwrap();

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            RemotesConfigStore::new(directory.path().join("config/remotes.json")),
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        let seed = executor.restore_host_schedule(host).unwrap().unwrap();
        assert_eq!(seed.source(), &expected_source);
        assert_eq!(seed.consecutive_failures(), 2);
        assert!(
            (Duration::from_secs(118)..=Duration::from_secs(120))
                .contains(&seed.next_eligible_in())
        );
    }

    #[test]
    fn filesystem_scheduler_restart_honors_host_bound_containment_without_transport() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let config = automatic_config(NODE_A);
        let host = config.host("dev").unwrap();
        let expected_source = host.expected_source().unwrap();
        let paused_at = Utc::now();
        let health = RemoteSyncHealthStore::new(state_root.clone());
        health
            .record_process_containment_pause(host.id(), expected_source, paused_at, host)
            .unwrap();

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            RemotesConfigStore::new(directory.path().join("config/remotes.json")),
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        assert!(matches!(
            scheduler.tick(&config, &mut executor).unwrap(),
            RemoteSyncSchedulerTick::Waiting { .. }
        ));
        assert_eq!(executor.transport.calls, 0);
        assert_eq!(executor.transport.probe_calls, 0);

        health
            .clear_process_containment_pause(
                host.id(),
                Some(expected_source),
                paused_at + TimeDelta::seconds(1),
            )
            .unwrap();
        assert!(matches!(
            scheduler.tick(&config, &mut executor).unwrap(),
            RemoteSyncSchedulerTick::Attempted { .. }
        ));
    }

    #[test]
    fn filesystem_executor_restores_durable_activity_hysteresis() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let config = automatic_config(NODE_A);
        let host = config.host("dev").unwrap();
        let expected_source = host.expected_source().unwrap().clone();
        let completed_at = Utc::now();
        RemoteSyncHealthStore::new(state_root.clone())
            .record_success(
                host.id(),
                Some(&expected_source),
                completed_at,
                &RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 0,
                    live_state_changed: true,
                    response_bytes: 256,
                    completion: RemoteSyncCompletion::Complete,
                },
                Some(completed_at + TimeDelta::seconds(30)),
            )
            .unwrap();

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            RemotesConfigStore::new(directory.path().join("config/remotes.json")),
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        let seed = executor.restore_host_schedule(host).unwrap().unwrap();
        assert_eq!(seed.source(), &expected_source);
        assert!(
            (Duration::from_secs(598)..=AUTOMATIC_REMOTE_SYNC_ACTIVITY_HYSTERESIS)
                .contains(&seed.active_until_in().unwrap())
        );
    }

    #[test]
    fn filesystem_executor_hard_pause_restore_uses_probe_deadline_and_keeps_streak() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();

        let config = automatic_config(NODE_A);
        let host = config.host("dev").unwrap();
        let expected_source = host.expected_source().unwrap().clone();
        let paused_at = Utc::now();
        let stale_idle_deadline = paused_at + chrono::TimeDelta::hours(24);
        let health = RemoteSyncHealthStore::new(state_root.clone());
        for _ in 0..2 {
            health
                .record_failure(
                    host.id(),
                    Some(&expected_source),
                    paused_at,
                    RemoteSyncErrorCategory::Transport,
                    Some(stale_idle_deadline),
                )
                .unwrap();
        }
        health
            .record_pause(
                host.id(),
                Some(&expected_source),
                paused_at,
                RemoteBandwidthBudgetLevel::Hard,
                Some(stale_idle_deadline),
            )
            .unwrap();

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            RemotesConfigStore::new(directory.path().join("config/remotes.json")),
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        let seed = executor.restore_host_schedule(host).unwrap().unwrap();
        let probe_seconds =
            u64::try_from(crate::remote_sync_health::REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS)
                .unwrap();
        assert_eq!(seed.consecutive_failures(), 2);
        assert!(
            (Duration::from_secs(probe_seconds.saturating_sub(2))
                ..=Duration::from_secs(probe_seconds))
                .contains(&seed.next_eligible_in()),
            "a stale 24h ordinary deadline must not suppress the durable 30m probe"
        );
    }

    #[test]
    fn filesystem_executor_restores_hard_pause_without_an_ordinary_deadline() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();

        let config = automatic_config(NODE_A);
        let host = config.host("dev").unwrap();
        let expected_source = host.expected_source().unwrap().clone();
        let paused_at = Utc::now();
        RemoteSyncHealthStore::new(state_root.clone())
            .record_pause(
                host.id(),
                Some(&expected_source),
                paused_at,
                RemoteBandwidthBudgetLevel::Hard,
                None,
            )
            .unwrap();

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            RemotesConfigStore::new(directory.path().join("config/remotes.json")),
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        let seed = executor.restore_host_schedule(host).unwrap().unwrap();
        assert_eq!(seed.consecutive_failures(), 0);
        assert!(!seed.next_eligible_in().is_zero());
        assert!(
            seed.next_eligible_in()
                <= Duration::from_secs(
                    u64::try_from(
                        crate::remote_sync_health::REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS,
                    )
                    .unwrap(),
                )
        );
    }

    #[test]
    fn filesystem_executor_ignores_health_for_another_source_generation() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let runtime =
            HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
        drop(runtime);

        let config = automatic_config(NODE_A);
        let host = config.host("dev").unwrap();
        let mut other_source = host.expected_source().unwrap().clone();
        other_source.generation = NonZeroU64::new(2).unwrap();
        let completed_at = Utc::now();
        RemoteSyncHealthStore::new(state_root.clone())
            .record_failure(
                host.id(),
                Some(&other_source),
                completed_at,
                RemoteSyncErrorCategory::Transport,
                Some(completed_at + chrono::TimeDelta::seconds(120)),
            )
            .unwrap();

        let mut executor = FilesystemAutomaticRemoteSyncExecutor::with_transport(
            state_root,
            codex_home,
            RemotesConfigStore::new(directory.path().join("config/remotes.json")),
            false,
            AutomaticRemoteCutoverPolicy::RequireV2Active,
            RejectingTransport::default(),
        );
        assert!(executor.restore_host_schedule(host).unwrap().is_none());
    }

    #[test]
    fn durable_ingest_position_classifies_incremental_after_process_restart() {
        let cursor = crate::remote_protocol::DeltaCursor {
            generation: NonZeroU64::new(7).unwrap(),
            sequence: 42,
        };
        let incremental = crate::remote_ingest_state::RemoteDeltaNextRequestPosition {
            delta_cursor: Some(cursor),
            exact_range: None,
            known_live_revision: None,
        };
        assert_eq!(
            automatic_transfer_kind_for_position(&incremental),
            RemoteBandwidthTransferKind::AutomaticIncremental
        );

        let continuation = crate::remote_ingest_state::RemoteDeltaNextRequestPosition {
            delta_cursor: Some(cursor),
            exact_range: Some(crate::remote_protocol::ExportRange {
                from: Utc::now() - chrono::TimeDelta::days(1),
                to: Utc::now(),
            }),
            known_live_revision: None,
        };
        assert_eq!(
            automatic_transfer_kind_for_position(&continuation),
            RemoteBandwidthTransferKind::AutomaticBulk
        );
        let bootstrap = crate::remote_ingest_state::RemoteDeltaNextRequestPosition {
            delta_cursor: None,
            exact_range: None,
            known_live_revision: None,
        };
        assert_eq!(
            automatic_transfer_kind_for_position(&bootstrap),
            RemoteBandwidthTransferKind::AutomaticBulk
        );
        assert_eq!(
            automatic_fact_transfer_kind(),
            RemoteBandwidthTransferKind::AutomaticBulk,
            "fact cursors can expire remotely, so automatic facts must never bypass the bulk cap",
        );
    }
}
