//! One bounded, content-free replica-fact follow-up after aggregate sync.
//!
//! Planning is local-only. At most one physical thread is materialized per
//! host attempt, and local participants are scanned locally rather than sent
//! through SSH. Errors are reduced to sanitized health categories so aggregate
//! pages that were already committed remain authoritative.

use std::io;

use chrono::{DateTime, Days, Utc};

use crate::config::CollectConfig;
use crate::domain::{ApiCostAmount, PicoUsd, TokenUsage};
use crate::history_ownership::{HistoryOwnershipState, OwnershipManifestStatus, TryWriterLease};
use crate::history_runtime::HistoryRuntime;
use crate::logical_replica::{ExpectedReplicaFactBinding, active_facts_cover_digest};
use crate::remote_collection::{REMOTE_COLLECTION_MAX_LOOKBACK_DAYS, collect_remote_rollouts};
use crate::remote_fact_exporter::{
    RemoteFactPrepareError, materialize_complete_session_facts_from_normalized_observation,
};
use crate::remote_fact_sync::{
    PlannedReplicaFactSync, PlannedReplicaFactTarget, RemoteFactSyncError, RemoteFactSyncLimits,
    RemoteFactSyncReport, RemoteFactTransport, ReplicaFactCandidateKey, ReplicaFactSyncPlan,
    plan_next_replica_fact_sync, sync_remote_thread_facts_bounded,
};
use crate::remote_protocol::{
    DeltaPayload, ExportRange, RemoteExportRequest, RemoteSessionFactPayload,
    RemoteSessionUsageMetrics, RemoteUsageEventFact,
};
use crate::remote_sync::RemoteSyncHostSnapshot;
use crate::remote_sync_health::{RemoteSyncErrorCategory, RemoteSyncHealthStore};
use crate::remotes_config::RemotesConfigStore;
use crate::source_export::source_normalized_observation;
use crate::source_history::{
    CompleteFactBatch, FactActivationReport, FactBatchId, FactBatchKind, FactCursor,
    PrevalidatedFactPublication, SessionUsageMetrics, SourceKind, UsageEventFact,
    UsageEventFactRecord,
};
use crate::source_model::SessionReplicaKey;

const FACT_RETENTION_DAYS: u16 = 35;
const REMOTE_FRAME_HEADER_BYTES: usize = 20;

/// Local-only fallback used by focused aggregate tests. Production constructors
/// always install the SSH fact transport explicitly.
#[derive(Default)]
pub(crate) struct DeferredRemoteFactTransport;

impl RemoteFactTransport for DeferredRemoteFactTransport {
    fn exchange(
        &mut self,
        _ssh_host: &str,
        _request: &RemoteExportRequest,
        _timeout: std::time::Duration,
        _max_decoded_bytes: usize,
    ) -> Result<
        crate::remote_transport::RemoteExchangeReport<DeltaPayload, RemoteSessionFactPayload>,
        crate::remote_transport::RemoteTransportError,
    > {
        Err(crate::remote_transport::RemoteTransportError::InvalidHost(
            "remote fact transport is unavailable in this execution context".to_owned(),
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplicaFactFollowupState {
    NoWork,
    AwaitingExactDigest,
    LocalActivated,
    RemoteActivated,
    Attention,
}

impl ReplicaFactFollowupState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoWork => "not-needed",
            Self::AwaitingExactDigest => "awaiting-exact-digest",
            Self::LocalActivated => "local-activated",
            Self::RemoteActivated => "remote-activated",
            Self::Attention => "attention",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReplicaFactFollowupReport {
    state: ReplicaFactFollowupState,
    error_category: Option<RemoteSyncErrorCategory>,
    response_bytes: usize,
    exchanges: usize,
    network_may_have_started: bool,
    inventory_too_large: bool,
    retry_candidate_later: bool,
}

#[derive(Debug)]
struct LocalFactMaterializationFailure {
    error: io::Error,
    retry_candidate_later: bool,
    resource_limit: bool,
}

impl LocalFactMaterializationFailure {
    fn transient(error: io::Error) -> Self {
        Self {
            error,
            retry_candidate_later: false,
            resource_limit: false,
        }
    }

    fn candidate(error: io::Error) -> Self {
        Self {
            error,
            retry_candidate_later: true,
            resource_limit: false,
        }
    }

    fn resource(error: io::Error) -> Self {
        Self {
            error,
            retry_candidate_later: true,
            resource_limit: true,
        }
    }
}

impl ReplicaFactFollowupReport {
    fn no_work() -> Self {
        Self {
            state: ReplicaFactFollowupState::NoWork,
            error_category: None,
            response_bytes: 0,
            exchanges: 0,
            network_may_have_started: false,
            inventory_too_large: false,
            retry_candidate_later: false,
        }
    }

    fn awaiting_exact_digest() -> Self {
        Self {
            state: ReplicaFactFollowupState::AwaitingExactDigest,
            error_category: Some(RemoteSyncErrorCategory::LocalState),
            response_bytes: 0,
            exchanges: 0,
            network_may_have_started: false,
            inventory_too_large: false,
            retry_candidate_later: false,
        }
    }

    fn local_activated() -> Self {
        Self {
            state: ReplicaFactFollowupState::LocalActivated,
            ..Self::no_work()
        }
    }

    fn remote_activated(report: &RemoteFactSyncReport) -> Self {
        Self {
            state: ReplicaFactFollowupState::RemoteActivated,
            error_category: None,
            response_bytes: report.response_bytes,
            exchanges: report.exchanges,
            network_may_have_started: true,
            inventory_too_large: false,
            retry_candidate_later: false,
        }
    }

    fn local_attention(error: &io::Error) -> Self {
        Self {
            state: ReplicaFactFollowupState::Attention,
            error_category: Some(local_error_category(error)),
            response_bytes: 0,
            exchanges: 0,
            network_may_have_started: false,
            inventory_too_large: false,
            retry_candidate_later: false,
        }
    }

    fn local_materialization_attention(error: &LocalFactMaterializationFailure) -> Self {
        let mut report = Self::local_attention(&error.error);
        if error.resource_limit {
            report.error_category = Some(RemoteSyncErrorCategory::ResourceLimit);
            report.inventory_too_large = true;
        }
        report
    }

    fn remote_attention(error: &RemoteFactSyncError) -> Self {
        let inventory_too_large = is_inventory_too_large(error);
        let retry_candidate_later = remote_error_should_cool_down_candidate(error);
        Self {
            state: ReplicaFactFollowupState::Attention,
            error_category: Some(if inventory_too_large {
                RemoteSyncErrorCategory::ResourceLimit
            } else if matches!(
                error,
                RemoteFactSyncError::Remote(failure)
                    if failure.kind
                        == crate::remote_protocol::RemoteFailureKind::FactEvidenceUnavailable
            ) {
                RemoteSyncErrorCategory::Busy
            } else if matches!(
                error,
                RemoteFactSyncError::Remote(failure)
                    if failure.kind == crate::remote_protocol::RemoteFailureKind::FactDigestChanged
            ) {
                RemoteSyncErrorCategory::LocalState
            } else {
                RemoteSyncErrorCategory::from_fact_sync_error(error)
            }),
            response_bytes: 0,
            exchanges: 0,
            network_may_have_started: !fact_error_proves_transport_not_started(error),
            inventory_too_large,
            retry_candidate_later,
        }
    }

    pub(crate) fn resource_attention() -> Self {
        Self::attention(RemoteSyncErrorCategory::ResourceLimit)
    }

    pub(crate) fn local_state_attention() -> Self {
        Self::attention(RemoteSyncErrorCategory::LocalState)
    }

    fn attention(category: RemoteSyncErrorCategory) -> Self {
        Self {
            state: ReplicaFactFollowupState::Attention,
            error_category: Some(category),
            response_bytes: 0,
            exchanges: 0,
            network_may_have_started: false,
            inventory_too_large: false,
            retry_candidate_later: false,
        }
    }

    pub(crate) fn mark_local_state_attention(&mut self) {
        self.state = ReplicaFactFollowupState::Attention;
        self.error_category = Some(RemoteSyncErrorCategory::LocalState);
    }

    pub fn state(&self) -> ReplicaFactFollowupState {
        self.state
    }

    pub fn error_category(&self) -> Option<RemoteSyncErrorCategory> {
        self.error_category
    }

    pub fn network_may_have_started(&self) -> bool {
        self.network_may_have_started
    }

    pub fn inventory_too_large(&self) -> bool {
        self.inventory_too_large
    }
}

/// Returns a valid fact limit only when the combined host reservation left at
/// least one complete framed response. No transport is opened for `None`.
pub(crate) fn fact_limits_for_response_budget(
    response_budget: usize,
) -> Option<RemoteFactSyncLimits> {
    let minimum = crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES
        .checked_add(REMOTE_FRAME_HEADER_BYTES)?;
    if response_budget < minimum {
        return None;
    }
    let defaults = RemoteFactSyncLimits::default();
    Some(RemoteFactSyncLimits {
        max_response_bytes: response_budget.min(defaults.max_response_bytes),
        ..defaults
    })
}

pub(crate) struct PreparedReplicaFactFollowup {
    plan: ReplicaFactSyncPlan,
    resume_after: Option<ReplicaFactCandidateKey>,
}

impl PreparedReplicaFactFollowup {
    pub fn requires_remote_transport(&self) -> bool {
        matches!(
            self.plan,
            ReplicaFactSyncPlan::Work(ref plan)
                if plan.target() == PlannedReplicaFactTarget::SelectedRemote
        )
    }
}

/// Performs the local-only candidate selection once so bandwidth admission can
/// reserve a remote fact exchange only when the selected action needs SSH.
///
/// This phase is deliberately read-only. Candidate planning may scan the full
/// bounded digest inventory, so callers must not retain the remotes config lock
/// across it. Any fair-scan cursor mutation is deferred to execution, where it
/// can be fenced by the exact host/revision in a short critical section.
pub(crate) fn prepare_replica_fact_followup(
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
    health_store: &RemoteSyncHealthStore,
    observed_at: DateTime<Utc>,
) -> Result<PreparedReplicaFactFollowup, ReplicaFactFollowupReport> {
    let Some(selected_source) = selected.host().expected_source() else {
        return Err(ReplicaFactFollowupReport::local_attention(&io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected host is unpaired",
        )));
    };
    let excluded = health_store
        .active_fact_resource_exclusions(
            selected.host().id(),
            selected_source,
            runtime.redaction_profile(),
            observed_at,
        )
        .map_err(|error| ReplicaFactFollowupReport::local_attention(&error))?;
    let resume_after = health_store
        .fact_resource_resume_after(
            selected.host().id(),
            selected_source,
            runtime.redaction_profile(),
        )
        .map_err(|error| ReplicaFactFollowupReport::local_attention(&error))?;
    match plan_next_replica_fact_sync(
        runtime.source_history(),
        selected,
        runtime.source_identity().node_id(),
        runtime.redaction_profile(),
        observed_at,
        &excluded,
        resume_after.as_ref(),
    ) {
        Ok(plan) => Ok(PreparedReplicaFactFollowup { plan, resume_after }),
        Err(error) => Err(ReplicaFactFollowupReport::local_attention(&error)),
    }
}

fn clear_prepared_fact_resume_cursor_if_current(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
    health_store: &RemoteSyncHealthStore,
    resume_after: Option<&ReplicaFactCandidateKey>,
) -> Result<(), ReplicaFactFollowupReport> {
    let Some(selected_source) = selected.host().expected_source() else {
        return Err(ReplicaFactFollowupReport::local_attention(&io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected host is unpaired",
        )));
    };
    config_store
        .with_current_host(selected.config_revision(), selected.host(), || {
            health_store.clear_fact_resource_resume_after(
                selected.host().id(),
                selected_source,
                runtime.redaction_profile(),
                resume_after,
            )
        })
        .map_err(|error| ReplicaFactFollowupReport::local_attention(&error))
}

/// Executes one already-selected participant. Aggregate synchronization must
/// already be committed and its own bandwidth reservation settled.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_prepared_replica_fact_followup(
    prepared: PreparedReplicaFactFollowup,
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
    health_store: &RemoteSyncHealthStore,
    collect_config: &CollectConfig,
    transport: &mut impl RemoteFactTransport,
    observed_at: DateTime<Utc>,
    remote_limits: Option<RemoteFactSyncLimits>,
) -> ReplicaFactFollowupReport {
    let PreparedReplicaFactFollowup { plan, resume_after } = prepared;
    if !matches!(&plan, ReplicaFactSyncPlan::Work(_))
        && let Err(attention) = clear_prepared_fact_resume_cursor_if_current(
            config_store,
            selected,
            runtime,
            health_store,
            resume_after.as_ref(),
        )
    {
        return attention;
    }
    let plan = match plan {
        ReplicaFactSyncPlan::NoWork => return ReplicaFactFollowupReport::no_work(),
        ReplicaFactSyncPlan::AwaitingExactDigest => {
            return ReplicaFactFollowupReport::awaiting_exact_digest();
        }
        ReplicaFactSyncPlan::DeferredResourceLimit => {
            return ReplicaFactFollowupReport::resource_attention();
        }
        ReplicaFactSyncPlan::Work(plan) => plan,
    };

    let selected_source = selected
        .host()
        .expected_source()
        .map(|source| &source.node_id);

    match plan.target() {
        PlannedReplicaFactTarget::Local
            if plan.source_id() == runtime.source_identity().node_id() =>
        {
            let candidate_key = plan.candidate_key();
            match materialize_local_thread_facts(
                config_store,
                selected,
                runtime,
                collect_config,
                &plan,
                observed_at,
            ) {
                Ok(_) => ReplicaFactFollowupReport::local_activated(),
                Err(error) => {
                    let mut report =
                        ReplicaFactFollowupReport::local_materialization_attention(&error);
                    if error.retry_candidate_later
                        && selected.host().expected_source().is_none_or(|source| {
                            config_store
                                .with_current_host(
                                    selected.config_revision(),
                                    selected.host(),
                                    || {
                                        health_store.record_fact_resource_cooldown(
                                            selected.host().id(),
                                            source,
                                            runtime.redaction_profile(),
                                            &candidate_key,
                                            observed_at,
                                            error.resource_limit,
                                        )
                                    },
                                )
                                .is_err()
                        })
                    {
                        report.mark_local_state_attention();
                    }
                    report
                }
            }
        }
        PlannedReplicaFactTarget::SelectedRemote if selected_source == Some(plan.source_id()) => {
            let Some(remote_limits) = remote_limits else {
                return ReplicaFactFollowupReport::resource_attention();
            };
            let candidate_key = plan.candidate_key();
            let mut report = match sync_remote_thread_facts_bounded(
                config_store,
                selected,
                runtime.ownership(),
                runtime.source_history(),
                plan.thread_id().clone(),
                plan.validated_digests().to_vec(),
                FACT_RETENTION_DAYS,
                transport,
                remote_limits,
            ) {
                Ok(report) => ReplicaFactFollowupReport::remote_activated(&report),
                Err(error) => ReplicaFactFollowupReport::remote_attention(&error),
            };
            if report.retry_candidate_later
                && selected.host().expected_source().is_none_or(|source| {
                    config_store
                        .with_current_host(selected.config_revision(), selected.host(), || {
                            health_store.record_fact_resource_cooldown(
                                selected.host().id(),
                                source,
                                runtime.redaction_profile(),
                                &candidate_key,
                                observed_at,
                                report.inventory_too_large(),
                            )
                        })
                        .is_err()
                })
            {
                report.mark_local_state_attention();
            }
            report
        }
        PlannedReplicaFactTarget::Local | PlannedReplicaFactTarget::SelectedRemote => {
            ReplicaFactFollowupReport::local_attention(&io::Error::new(
                io::ErrorKind::InvalidData,
                "fact plan target does not match its source",
            ))
        }
    }
}

pub(crate) fn estimated_fact_network_bytes(facts: &ReplicaFactFollowupReport) -> io::Result<usize> {
    let fact_overhead = facts
        .exchanges
        .checked_mul(
            crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fact SSH overhead overflow"))?;
    facts
        .response_bytes
        .checked_add(fact_overhead)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote fact bandwidth estimate overflow",
            )
        })
}

fn materialize_local_thread_facts(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
    collect_config: &CollectConfig,
    plan: &PlannedReplicaFactSync,
    observed_at: DateTime<Utc>,
) -> Result<FactActivationReport, LocalFactMaterializationFailure> {
    let thread_id = plan.thread_id();
    let digest = plan.digest();
    let expected_digests = plan.validated_digests();
    if digest.replica().source_id() != runtime.source_identity().node_id()
        || digest.replica().thread_id() != thread_id
    {
        return Err(LocalFactMaterializationFailure::transient(io::Error::new(
            io::ErrorKind::InvalidData,
            "local fact plan does not match the local source replica",
        )));
    }
    let (mut activation, publication) = materialize_then_activate_if_current(
        config_store,
        selected,
        || {
            let range = ExportRange {
                from: observed_at
                    .checked_sub_days(Days::new(REMOTE_COLLECTION_MAX_LOOKBACK_DAYS as u64))
                    .unwrap_or(DateTime::<Utc>::MIN_UTC),
                to: observed_at,
            };
            let collection = collect_remote_rollouts(
                collect_config,
                &range,
                observed_at,
                runtime.redaction_profile(),
            )
            .map_err(|error| {
                LocalFactMaterializationFailure::transient(io::Error::other(format!(
                    "local fact scan failed: {error}"
                )))
            })?;
            materialize_local_thread_fact_batch(
                runtime,
                thread_id,
                expected_digests,
                observed_at,
                collection,
            )
            .and_then(|batch| {
                stage_local_batch(runtime, &batch)
                    .map_err(LocalFactMaterializationFailure::transient)?;
                prevalidate_staged_local_batch(runtime, &batch)
                    .map_err(LocalFactMaterializationFailure::transient)
            })
        },
        |publication| publish_prevalidated_local_batch(runtime, publication),
    )?;
    activation.cleanup_pending |= cleanup_prevalidated_local_batch(runtime, &publication);
    // Reading and validating the complete active fact generation can be much
    // larger than the atomic manifest publication. Keep it outside the
    // remotes lock as well; the published generation remains self-validating.
    verify_activated_local_thread_facts(runtime, thread_id, digest)?;
    Ok(activation)
}

fn materialize_local_thread_fact_batch(
    runtime: &HistoryRuntime,
    thread_id: &crate::source_model::ThreadId,
    expected_digests: &[crate::source_history::FactDigestBinding],
    observed_at: DateTime<Utc>,
    collection: crate::remote_collection::RemoteCollection,
) -> Result<CompleteFactBatch, LocalFactMaterializationFailure> {
    if !collection.scan_complete {
        return Err(LocalFactMaterializationFailure::candidate(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local fact scan is incomplete",
        )));
    }
    let publication = collection.aggregate_publication().ok_or_else(|| {
        LocalFactMaterializationFailure::candidate(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local fact scan is incomplete",
        ))
    })?;
    let normalized_observation = source_normalized_observation(
        runtime.source_identity(),
        &collection.dataset.tasks,
        publication.observation(),
    );
    let digest_evidence = crate::source_export::materialize_local_session_digest_evidence(
        &collection.dataset.calls,
        &normalized_observation.half_hour_buckets,
        observed_at,
        true,
    )
    .map_err(LocalFactMaterializationFailure::candidate)?;
    let current_digests = crate::source_export::finalize_local_session_digests(
        runtime.source_identity(),
        &digest_evidence,
        &normalized_observation,
    )
    .map_err(LocalFactMaterializationFailure::candidate)?;
    let mut current_bindings = current_digests
        .iter()
        .filter(|current| {
            current.replica().thread_id() == thread_id && current.exact_event_identity()
        })
        .map(crate::source_history::FactDigestBinding::from_digest)
        .collect::<io::Result<Vec<_>>>()
        .map_err(LocalFactMaterializationFailure::candidate)?;
    current_bindings.sort_by_key(crate::source_history::FactDigestBinding::range_start);
    if current_bindings != expected_digests {
        return Err(LocalFactMaterializationFailure::candidate(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local facts changed after their digest observation",
        )));
    }
    let materialized = materialize_complete_session_facts_from_normalized_observation(
        thread_id,
        FACT_RETENTION_DAYS,
        observed_at,
        &normalized_observation,
        &collection.dataset.calls,
        &collection.partial_reasons,
    )
    .map_err(|error| match error {
        RemoteFactPrepareError::InventoryTooLarge => {
            LocalFactMaterializationFailure::resource(io::Error::new(
                io::ErrorKind::InvalidData,
                "local fact inventory exceeds its complete-batch limit",
            ))
        }
        error => LocalFactMaterializationFailure::candidate(io::Error::other(error.to_string())),
    })?;
    let replica = SessionReplicaKey::new(
        runtime.source_identity().node_id().clone(),
        thread_id.clone(),
    );
    let changes = materialized
        .facts
        .into_iter()
        .enumerate()
        .map(|(index, fact)| {
            let revision = u64::try_from(index)
                .ok()
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "local fact revision overflow")
                })?;
            UsageEventFactRecord::upsert(revision, convert_local_fact(&replica, fact)?)
        })
        .collect::<io::Result<Vec<_>>>()
        .map_err(LocalFactMaterializationFailure::candidate)?;
    let active = runtime
        .source_history()
        .load_active_fact_set(
            runtime.source_identity().node_id(),
            runtime.redaction_profile(),
            thread_id,
        )
        .map_err(LocalFactMaterializationFailure::transient)?;
    let fact_generation = active
        .as_ref()
        .map_or(Ok(1), |active| {
            active
                .cursor
                .fact_generation()
                .checked_add(1)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "local fact generation overflow")
                })
        })
        .map_err(LocalFactMaterializationFailure::transient)?;
    let through_sequence = u64::try_from(changes.len())
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "local fact record count does not fit its cursor",
            )
        })
        .map_err(LocalFactMaterializationFailure::transient)?;
    let batch = CompleteFactBatch {
        batch_id: FactBatchId::generate().map_err(LocalFactMaterializationFailure::transient)?,
        kind: FactBatchKind::Snapshot,
        replica,
        expected_active_version: active.as_ref().map(|active| active.version.clone()),
        remote_binding: None,
        validated_digests: expected_digests.to_vec(),
        activate_cursor: FactCursor::new(fact_generation, through_sequence)
            .map_err(LocalFactMaterializationFailure::transient)?,
        completed_at: observed_at,
        changes,
    };
    batch
        .validate()
        .map_err(LocalFactMaterializationFailure::candidate)?;
    Ok(batch)
}

fn materialize_then_activate_if_current<T, R>(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    materialize: impl FnOnce() -> Result<T, LocalFactMaterializationFailure>,
    activate: impl FnOnce(&T) -> io::Result<R>,
) -> Result<(R, T), LocalFactMaterializationFailure> {
    // Materialization can scan the complete 35-day rollout retention window.
    // It must not retain the remotes shared lock, otherwise disable/remove/edit
    // and the global off switch can block on local disk work. Only the final
    // local activation is linearized against the exact revision/full host row.
    // The complete batch is already durably staged but invisible at this
    // point, so a stale configuration leaves only GC-recoverable staging and
    // cannot publish any late fact generation.
    let materialized = materialize()?;
    let activated = config_store
        .with_current_host(selected.config_revision(), selected.host(), || {
            activate(&materialized)
        })
        .map_err(LocalFactMaterializationFailure::transient)?;
    Ok((activated, materialized))
}

fn verify_activated_local_thread_facts(
    runtime: &HistoryRuntime,
    thread_id: &crate::source_model::ThreadId,
    digest: &crate::source_history::SourceSessionDigest,
) -> Result<(), LocalFactMaterializationFailure> {
    let activated = runtime
        .source_history()
        .load_active_fact_set(
            runtime.source_identity().node_id(),
            runtime.redaction_profile(),
            thread_id,
        )
        .map_err(LocalFactMaterializationFailure::transient)?;
    if !active_facts_cover_digest(
        digest,
        activated.as_ref(),
        ExpectedReplicaFactBinding::Local,
    ) {
        return Err(LocalFactMaterializationFailure::candidate(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local facts changed after their digest observation",
        )));
    }
    Ok(())
}

fn stage_local_batch(runtime: &HistoryRuntime, batch: &CompleteFactBatch) -> io::Result<()> {
    let lease = match runtime.ownership().try_acquire_writer_lease()? {
        TryWriterLease::Acquired(lease) => lease,
        TryWriterLease::Busy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer is busy; retry local fact staging later",
            ));
        }
    };
    let manifest = match runtime.ownership().load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active =>
        {
            manifest
        }
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local fact staging requires active v2 history ownership",
            ));
        }
    };
    let authority = runtime.ownership().authorize_v2_write(&lease, &manifest)?;
    let writer = runtime.source_history().writer(&authority)?;
    let source = runtime
        .source_history()
        .load_source_metadata(runtime.source_identity().node_id())?;
    if source.kind() != SourceKind::Local
        || source.aggregate_redaction_profile() != runtime.redaction_profile()
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local fact source metadata changed before staging",
        ));
    }
    writer.stage_complete_fact_batch(
        runtime.source_identity().node_id(),
        runtime.redaction_profile(),
        batch,
    )
}

fn prevalidate_staged_local_batch(
    runtime: &HistoryRuntime,
    batch: &CompleteFactBatch,
) -> io::Result<PrevalidatedFactPublication> {
    let lease = match runtime.ownership().try_acquire_writer_lease()? {
        TryWriterLease::Acquired(lease) => lease,
        TryWriterLease::Busy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer is busy; retry local fact prevalidation later",
            ));
        }
    };
    let manifest = match runtime.ownership().load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active =>
        {
            manifest
        }
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local fact prevalidation requires active v2 history ownership",
            ));
        }
    };
    let authority = runtime.ownership().authorize_v2_write(&lease, &manifest)?;
    let writer = runtime.source_history().writer(&authority)?;
    writer.prevalidate_staged_fact_batch(
        runtime.source_identity().node_id(),
        runtime.redaction_profile(),
        &batch.batch_id,
    )
}

fn publish_prevalidated_local_batch(
    runtime: &HistoryRuntime,
    publication: &PrevalidatedFactPublication,
) -> io::Result<FactActivationReport> {
    let lease = match runtime.ownership().try_acquire_writer_lease()? {
        TryWriterLease::Acquired(lease) => lease,
        TryWriterLease::Busy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer is busy; retry local fact publication later",
            ));
        }
    };
    let manifest = match runtime.ownership().load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active =>
        {
            manifest
        }
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local fact publication requires active v2 history ownership",
            ));
        }
    };
    let authority = runtime.ownership().authorize_v2_write(&lease, &manifest)?;
    let writer = runtime.source_history().writer(&authority)?;
    let source = runtime
        .source_history()
        .load_source_metadata(runtime.source_identity().node_id())?;
    if source.kind() != SourceKind::Local
        || source.aggregate_redaction_profile() != runtime.redaction_profile()
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local fact source metadata changed before publication",
        ));
    }
    writer.publish_prevalidated_fact_batch(publication)
}

fn cleanup_prevalidated_local_batch(
    runtime: &HistoryRuntime,
    publication: &PrevalidatedFactPublication,
) -> bool {
    let Ok(TryWriterLease::Acquired(lease)) = runtime.ownership().try_acquire_writer_lease() else {
        return true;
    };
    let Ok(OwnershipManifestStatus::Initialized(manifest)) = runtime.ownership().load_manifest()
    else {
        return true;
    };
    if manifest.state() != HistoryOwnershipState::V2Active {
        return true;
    }
    let Ok(authority) = runtime.ownership().authorize_v2_write(&lease, &manifest) else {
        return true;
    };
    let Ok(writer) = runtime.source_history().writer(&authority) else {
        return true;
    };
    writer.cleanup_prevalidated_fact_publication(publication)
}

fn convert_local_fact(
    replica: &SessionReplicaKey,
    fact: RemoteUsageEventFact,
) -> io::Result<UsageEventFact> {
    if fact.emitting_thread_id != *replica.thread_id() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "materialized local fact belongs to a different emitting thread",
        ));
    }
    UsageEventFact::new(
        replica.clone(),
        fact.event_id,
        fact.occurred_at,
        fact.observed_project_key,
        fact.emitting_turn_id,
        fact.parent_thread_id,
        fact.project_session_thread_id,
        fact.root_session_thread_id,
        fact.root_session_turn_id,
        fact.model,
        fact.service_tier,
        TokenUsage {
            input_tokens: fact.digest_token_usage.input_tokens,
            cached_input_tokens: fact.digest_token_usage.cached_input_tokens,
            cache_write_input_tokens: fact.digest_token_usage.cache_write_input_tokens,
            output_tokens: fact.digest_token_usage.output_tokens,
            reasoning_output_tokens: fact.digest_token_usage.reasoning_output_tokens,
            total_tokens: fact.digest_token_usage.total_tokens,
        },
        fact.request_usage_exact,
        fact.exact_event_identity,
        convert_metrics(fact.metrics),
    )
}

fn convert_metrics(metrics: RemoteSessionUsageMetrics) -> SessionUsageMetrics {
    SessionUsageMetrics {
        token_usage: TokenUsage {
            input_tokens: metrics.token_usage.input_tokens,
            cached_input_tokens: metrics.token_usage.cached_input_tokens,
            cache_write_input_tokens: metrics.token_usage.cache_write_input_tokens,
            output_tokens: metrics.token_usage.output_tokens,
            reasoning_output_tokens: metrics.token_usage.reasoning_output_tokens,
            total_tokens: metrics.token_usage.total_tokens,
        },
        estimated_cost_units: metrics.estimated_cost_units.value(),
        api_long_context_extra_cost_units: metrics
            .api_long_context_extra_cost_units
            .map(|value| value.value()),
        api_equivalent_cost: ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(metrics.api_equivalent_cost.minimum_pico_usd.value()),
            maximum_pico_usd: PicoUsd::new(metrics.api_equivalent_cost.maximum_pico_usd.value()),
            observed_samples: metrics.api_equivalent_cost.observed_samples,
            priced_samples: metrics.api_equivalent_cost.priced_samples,
            observed_tokens: metrics.api_equivalent_cost.observed_tokens,
            priced_tokens: metrics.api_equivalent_cost.priced_tokens,
        },
        call_count: metrics.call_count,
        metric_revision: metrics.metric_revision.get(),
        estimator_revision: metrics.estimator_revision.get(),
        project_breakdown_revision: metrics.project_breakdown_revision.get(),
        api_pricing_catalog_revision: metrics.api_pricing_catalog_revision.get(),
        partial_reasons: metrics.partial_reasons,
    }
}

fn local_error_category(error: &io::Error) -> RemoteSyncErrorCategory {
    match error.kind() {
        io::ErrorKind::PermissionDenied => RemoteSyncErrorCategory::Policy,
        io::ErrorKind::WouldBlock => RemoteSyncErrorCategory::Busy,
        io::ErrorKind::InvalidData => RemoteSyncErrorCategory::Protocol,
        _ => RemoteSyncErrorCategory::LocalState,
    }
}

fn fact_error_proves_transport_not_started(error: &RemoteFactSyncError) -> bool {
    matches!(
        error,
        RemoteFactSyncError::HostNotPaired { .. }
            | RemoteFactSyncError::InvalidLimits(_)
            | RemoteFactSyncError::InvalidRetentionDays(_)
            | RemoteFactSyncError::PreTransportLocal(_)
    )
}

fn is_inventory_too_large(error: &RemoteFactSyncError) -> bool {
    matches!(
        error,
        RemoteFactSyncError::Remote(failure)
            if failure.kind
                == crate::remote_protocol::RemoteFailureKind::FactInventoryTooLarge
    )
}

fn remote_error_should_cool_down_candidate(error: &RemoteFactSyncError) -> bool {
    matches!(
        error,
        RemoteFactSyncError::Remote(failure)
            if matches!(
                failure.kind,
                crate::remote_protocol::RemoteFailureKind::FactEvidenceUnavailable
                    | crate::remote_protocol::RemoteFailureKind::FactDigestChanged
                    | crate::remote_protocol::RemoteFailureKind::FactInventoryTooLarge
            )
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use super::*;
    use crate::remote_fact_sync::RemoteFactSyncReport;
    use crate::remote_protocol::{RemoteFailure, RemoteFailureKind, SourceGeneration};
    use crate::remotes_config::{RemotesConfigMutation, RemotesConfigStore};
    use tempfile::tempdir;

    #[test]
    fn fact_response_budget_never_admits_a_partial_minimum_frame() {
        let minimum =
            crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES;
        assert!(fact_limits_for_response_budget(minimum - 1).is_none());
        let limits = fact_limits_for_response_budget(minimum).unwrap();
        assert_eq!(limits.max_response_bytes, minimum);
        assert_eq!(limits.max_pages, NonZeroUsize::new(8).unwrap());
        let defaults = RemoteFactSyncLimits::default();
        assert_eq!(
            fact_limits_for_response_budget(usize::MAX)
                .unwrap()
                .max_response_bytes,
            defaults.max_response_bytes
        );
    }

    #[test]
    fn fact_bandwidth_counts_each_exchange_and_only_validated_bytes() {
        let remote = RemoteFactSyncReport {
            exchanges: 3,
            response_bytes: 200,
            decoded_response_bytes: 400,
            records_received: 2,
            restarted_from_snapshot: false,
            activation: FactActivationReport {
                activated: true,
                cleanup_pending: false,
            },
            cursor: FactCursor::new(1, 2).unwrap(),
        };
        let followup = ReplicaFactFollowupReport::remote_activated(&remote);
        let overhead =
            crate::remote_bandwidth_budget::REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES;
        assert_eq!(
            estimated_fact_network_bytes(&followup).unwrap(),
            200 + (3 * overhead)
        );
    }

    #[test]
    fn ambiguous_transport_attention_keeps_reservation_and_inventory_is_sanitized() {
        let transport = RemoteFactSyncError::Transport(
            crate::remote_transport::RemoteTransportError::InvalidHost("secret path".to_owned()),
        );
        let report = ReplicaFactFollowupReport::remote_attention(&transport);
        assert!(report.network_may_have_started());
        assert_eq!(
            report.error_category(),
            Some(RemoteSyncErrorCategory::Transport)
        );

        let escaped_helper = RemoteFactSyncError::Transport(
            crate::remote_transport::RemoteTransportError::Cancelled {
                cleanup_error: Some(io::Error::other("escaped helper")),
            },
        );
        let report = ReplicaFactFollowupReport::remote_attention(&escaped_helper);
        assert!(report.network_may_have_started());
        assert_eq!(
            report.error_category(),
            Some(RemoteSyncErrorCategory::ProcessContainment)
        );

        let inventory = RemoteFactSyncError::Remote(RemoteFailure {
            kind: RemoteFailureKind::FactInventoryTooLarge,
            message: "remote fact inventory exceeds the bounded complete-batch limit".to_owned(),
            retry_after_seconds: None,
        });
        let report = ReplicaFactFollowupReport::remote_attention(&inventory);
        assert!(report.inventory_too_large());
        assert_eq!(
            report.error_category(),
            Some(RemoteSyncErrorCategory::ResourceLimit)
        );
        assert!(report.retry_candidate_later);

        for (kind, expected_category) in [
            (
                RemoteFailureKind::FactEvidenceUnavailable,
                RemoteSyncErrorCategory::Busy,
            ),
            (
                RemoteFailureKind::FactDigestChanged,
                RemoteSyncErrorCategory::LocalState,
            ),
        ] {
            let error = RemoteFactSyncError::Remote(RemoteFailure {
                kind,
                message: "sanitized fact evidence failure".to_owned(),
                retry_after_seconds: None,
            });
            let report = ReplicaFactFollowupReport::remote_attention(&error);
            assert!(report.retry_candidate_later);
            assert_eq!(report.error_category(), Some(expected_category));
        }
    }

    #[test]
    fn explicit_pre_transport_local_error_releases_the_fact_reservation() {
        let pre_transport = RemoteFactSyncError::PreTransportLocal(io::Error::new(
            io::ErrorKind::NotFound,
            "local metadata unavailable",
        ));
        let report = ReplicaFactFollowupReport::remote_attention(&pre_transport);
        assert!(!report.network_may_have_started());

        let ambiguous_local = RemoteFactSyncError::Local(io::Error::new(
            io::ErrorKind::NotFound,
            "post-response activation state unavailable",
        ));
        let report = ReplicaFactFollowupReport::remote_attention(&ambiguous_local);
        assert!(report.network_may_have_started());
    }

    #[test]
    fn blocking_local_collector_does_not_block_concurrent_automatic_disable() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
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
                RemotesConfigMutation::pair_pin(
                    "dev",
                    SourceGeneration {
                        node_id: "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
                        generation: std::num::NonZeroU64::new(1).unwrap(),
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
        let selected =
            RemoteSyncHostSnapshot::capture_for_automatic(&config, config.host("dev").unwrap())
                .unwrap();

        let (collector_started_tx, collector_started_rx) = mpsc::channel();
        let (release_collector_tx, release_collector_rx) = mpsc::channel();
        let staged = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(AtomicBool::new(false));
        let worker_staged = Arc::clone(&staged);
        let worker_activated = Arc::clone(&activated);
        let worker_store = store.clone();
        let worker = thread::spawn(move || {
            materialize_then_activate_if_current(
                &worker_store,
                &selected,
                || {
                    collector_started_tx.send(()).unwrap();
                    release_collector_rx.recv().unwrap();
                    // Models the real invisible staging write, which also
                    // remains outside the remotes config lock.
                    worker_staged.store(true, Ordering::SeqCst);
                    Ok(())
                },
                |&()| {
                    worker_activated.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
        });
        collector_started_rx
            .recv_timeout(StdDuration::from_secs(1))
            .expect("the synthetic local collector should block before activation");

        let disable_store = store.clone();
        let disable_revision = config.config_revision();
        let (disable_done_tx, disable_done_rx) = mpsc::channel();
        let disabler = thread::spawn(move || {
            let result = disable_store.update(
                disable_revision,
                RemotesConfigMutation::set_auto_sync_enabled(false),
            );
            disable_done_tx.send(result).unwrap();
        });
        let disabled = disable_done_rx.recv_timeout(StdDuration::from_secs(1));
        if disabled.is_err() {
            // Always release and join both threads so a regression reports a
            // normal test failure instead of leaving a blocked test process.
            release_collector_tx.send(()).unwrap();
            let _ = worker.join();
            let _ = disabler.join();
            panic!("automatic disable was blocked by local fact collection");
        }
        disabled.unwrap().unwrap();
        disabler.join().unwrap();

        release_collector_tx.send(()).unwrap();
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.error.kind(), io::ErrorKind::WouldBlock);
        assert!(
            staged.load(Ordering::SeqCst),
            "an interrupted attempt may leave only invisible GC-recoverable staging"
        );
        assert!(
            !activated.load(Ordering::SeqCst),
            "a late local fact batch must not activate after automatic sync is disabled"
        );
    }
}
