//! Bounded center-side synchronization for one physical remote thread's facts.
//!
//! Fact pages are deliberately assembled only in memory. No page token or
//! partial batch is persisted, and the old active fact generation remains the
//! sole readable version until a final activation cursor has been validated.
//! The final stage-and-activate operation runs under both the exact remotes
//! configuration snapshot and one fresh v2 history-writer fence.

use std::cell::Cell;
use std::fmt;
use std::io;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Utc};

use crate::domain::{ApiCostAmount, PicoUsd, TokenUsage};
use crate::history_ownership::{
    HistoryOwnershipState, HistoryOwnershipStore, OwnershipManifestStatus, TryWriterLease,
};
use crate::logical_replica::{
    ExpectedReplicaFactBinding, ReplicaCandidateKind, ReplicaDigestObservation,
    active_facts_cover_digest, detect_replica_candidates,
};
use crate::remote_agent::{current_accepted_revisions, current_revisions};
use crate::remote_fact_exporter::MAX_COMPLETE_FACT_BATCH_RECORDS;
use crate::remote_protocol::{
    DeltaPayload, FactCursor as RemoteFactCursor, FactDeltaPage, FactSnapshotPage,
    MAX_REMOTE_FRAME_ENCODED_BYTES, MIN_REMOTE_RESPONSE_ENCODED_BYTES, ProtocolRevisions,
    REMOTE_PROTOCOL_VERSION, RemoteExportRequest, RemoteExportRequestBody,
    RemoteExportResponseBody, RemoteFailure, RemoteFailureKind, RemoteProtocolError,
    RemoteSessionFactPayload, RemoteSessionFactResponse, RemoteUsageEventFactMutation,
    RemoteUsageEventFactRecord, SessionFactsDigestBinding, SessionFactsPosition,
    SessionFactsRequest, SourceGeneration,
};
use crate::remote_sync::RemoteSyncHostSnapshot;
use crate::remote_transport::{
    RemoteExchangeReport, RemoteTransportError, SshCommandEnvironment,
    exchange_remote_with_frame_limits_and_agent_executable_and_environment,
    exchange_remote_with_frame_limits_and_environment,
};
use crate::remotes_config::{RemoteHostConfig, RemotesConfigStore};
use crate::source_history::{
    ActiveFactSet, ActiveFactVersion, CompleteFactBatch, FactActivationReport, FactBatchId,
    FactBatchKind, FactCursor, FactDigestBinding, PrevalidatedFactPublication, RedactionProfile,
    SessionDigestFingerprint, SessionUsageMetrics, SourceHistoryRemoteBinding, SourceHistoryStore,
    SourceKind, SourceSessionDigest, SourceSessionDigestChange, UsageEventFact,
    UsageEventFactRecord,
};
use crate::source_identity::NodeId;
use crate::source_model::{SessionReplicaKey, ThreadId};

const REMOTE_FRAME_HEADER_BYTES: usize = 20;
const DEFAULT_MAX_PAGES_PER_RUN: usize = 8;
const MAX_PAGES_PER_RUN: usize = 8;
const MAX_RESPONSE_BYTES_PER_RUN: usize = 32 * 1024 * 1024;
const MAX_DECODED_BYTES_PER_RUN: usize = 32 * 1024 * 1024;
// The exporter guarantees that one complete frozen batch fits this count and
// its serialized-byte cap. Keeping the center bound identical prevents a
// legal remote inventory from entering an endless restart-without-activation
// loop when no partial page token is persisted locally.
const MAX_EXCHANGE_TIMEOUT: StdDuration = StdDuration::from_secs(120);
const MAX_REMOTE_FACT_RUN_TIME: StdDuration = StdDuration::from_secs(300);
const MAX_FACT_RETENTION_DAYS: u16 = 35;
const MAX_FACT_PLANNER_SOURCES: usize = 512;
const MAX_FACT_PLANNER_DIGESTS: usize = 65_536;

const MIN_RESPONSE_BUDGET: usize = MIN_REMOTE_RESPONSE_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES;

/// Hard bounds for a single per-thread synchronization invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteFactSyncLimits {
    pub max_pages: NonZeroUsize,
    /// Exact framed response bytes, including the frame header.
    pub max_response_bytes: usize,
    /// Sum of decoded JSON payload bytes across every page in this run.
    pub max_decoded_bytes: usize,
    pub exchange_timeout: StdDuration,
}

impl Default for RemoteFactSyncLimits {
    fn default() -> Self {
        Self {
            max_pages: NonZeroUsize::new(DEFAULT_MAX_PAGES_PER_RUN)
                .expect("the default page limit is non-zero"),
            max_response_bytes: MAX_RESPONSE_BYTES_PER_RUN,
            max_decoded_bytes: MAX_DECODED_BYTES_PER_RUN,
            exchange_timeout: StdDuration::from_secs(60),
        }
    }
}

impl RemoteFactSyncLimits {
    fn validate(self) -> Result<Self, RemoteFactSyncError> {
        if self.max_pages.get() > MAX_PAGES_PER_RUN {
            return Err(RemoteFactSyncError::InvalidLimits(
                "remote fact sync page limit exceeds 8",
            ));
        }
        if !(MIN_RESPONSE_BUDGET..=MAX_RESPONSE_BYTES_PER_RUN).contains(&self.max_response_bytes) {
            return Err(RemoteFactSyncError::InvalidLimits(
                "remote fact sync response-byte limit must be between one minimum frame and 32 MiB",
            ));
        }
        if self.max_decoded_bytes == 0 || self.max_decoded_bytes > MAX_DECODED_BYTES_PER_RUN {
            return Err(RemoteFactSyncError::InvalidLimits(
                "remote fact sync decoded-byte limit must be between 1 byte and 32 MiB",
            ));
        }
        if self.exchange_timeout.is_zero() || self.exchange_timeout > MAX_EXCHANGE_TIMEOUT {
            return Err(RemoteFactSyncError::InvalidLimits(
                "remote fact sync exchange timeout must be between 1ns and 120s",
            ));
        }
        Ok(self)
    }

    /// Maximum SSH exchanges a legal run can make. A stale delta cursor may
    /// consume one bounded delta prefix plus its expiry response before a new
    /// snapshot receives the full data-page allowance.
    pub fn max_exchanges_per_run(self) -> usize {
        self.max_pages.get().saturating_mul(2).saturating_add(1)
    }
}

/// Network boundary injected by tests. Production uses one-shot OpenSSH.
pub trait RemoteFactTransport {
    fn exchange(
        &mut self,
        ssh_host: &str,
        request: &RemoteExportRequest,
        timeout: StdDuration,
        max_decoded_bytes: usize,
    ) -> Result<RemoteExchangeReport<DeltaPayload, RemoteSessionFactPayload>, RemoteTransportError>;

    fn exchange_host(
        &mut self,
        host: &RemoteHostConfig,
        request: &RemoteExportRequest,
        timeout: StdDuration,
        max_decoded_bytes: usize,
    ) -> Result<RemoteExchangeReport<DeltaPayload, RemoteSessionFactPayload>, RemoteTransportError>
    {
        self.exchange(host.ssh_host(), request, timeout, max_decoded_bytes)
    }
}

#[derive(Default)]
pub struct SshRemoteFactTransport {
    environment: SshCommandEnvironment,
}

impl SshRemoteFactTransport {
    pub fn new(environment: SshCommandEnvironment) -> Self {
        Self { environment }
    }
}

impl RemoteFactTransport for SshRemoteFactTransport {
    fn exchange(
        &mut self,
        ssh_host: &str,
        request: &RemoteExportRequest,
        timeout: StdDuration,
        max_decoded_bytes: usize,
    ) -> Result<RemoteExchangeReport<DeltaPayload, RemoteSessionFactPayload>, RemoteTransportError>
    {
        exchange_remote_with_frame_limits_and_environment(
            ssh_host,
            request,
            timeout,
            crate::remote_protocol::RemoteFrameLimits {
                max_decoded_bytes,
                ..crate::remote_protocol::RemoteFrameLimits::default()
            },
            &self.environment,
        )
    }

    fn exchange_host(
        &mut self,
        host: &RemoteHostConfig,
        request: &RemoteExportRequest,
        timeout: StdDuration,
        max_decoded_bytes: usize,
    ) -> Result<RemoteExchangeReport<DeltaPayload, RemoteSessionFactPayload>, RemoteTransportError>
    {
        exchange_remote_with_frame_limits_and_agent_executable_and_environment(
            host.ssh_host(),
            host.agent_executable(),
            request,
            timeout,
            crate::remote_protocol::RemoteFrameLimits {
                max_decoded_bytes,
                ..crate::remote_protocol::RemoteFrameLimits::default()
            },
            &self.environment,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFactSyncReport {
    /// Successful pages plus a validated cursor-expiry response, if any.
    pub exchanges: usize,
    pub response_bytes: usize,
    pub decoded_response_bytes: usize,
    pub records_received: usize,
    pub restarted_from_snapshot: bool,
    pub activation: FactActivationReport,
    pub cursor: FactCursor,
}

/// One bounded physical-thread participant selected from replica evidence.
/// A plan never carries rollout content, project paths, SSH aliases, or
/// message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlannedReplicaFactSync {
    source_id: NodeId,
    thread_id: ThreadId,
    digest: SourceSessionDigest,
    validated_digests: Vec<FactDigestBinding>,
    target: PlannedReplicaFactTarget,
}

impl PlannedReplicaFactSync {
    pub fn source_id(&self) -> &NodeId {
        &self.source_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    pub fn digest(&self) -> &SourceSessionDigest {
        &self.digest
    }

    pub fn validated_digests(&self) -> &[FactDigestBinding] {
        &self.validated_digests
    }

    pub fn target(&self) -> PlannedReplicaFactTarget {
        self.target
    }

    pub(crate) fn candidate_key(&self) -> ReplicaFactCandidateKey {
        ReplicaFactCandidateKey::from_digest(
            self.source_id.clone(),
            self.thread_id.clone(),
            &self.digest,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlannedReplicaFactTarget {
    Local,
    SelectedRemote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplicaFactSyncPlan {
    /// No divergent replica involving this host needs work from either the
    /// selected remote or the center-local source.
    NoWork,
    /// One exact known-event participant can be materialized now. Its range
    /// may remain explicitly incomplete/lower-bound.
    Work(Box<PlannedReplicaFactSync>),
    /// A divergent candidate exists, but its event identity is not exact, so
    /// fetching facts cannot safely union its known events.
    AwaitingExactDigest,
    /// Every otherwise actionable participant is in a bounded, persisted
    /// resource cooldown. The caller should retain an attention signal while
    /// allowing later candidates to make progress on subsequent runs.
    DeferredResourceLimit,
}

/// Content-free identity for one source/day fact candidate. Redaction profile
/// is deliberately supplied by the enclosing history namespace/cooldown store.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReplicaFactCandidateKey {
    source_id: NodeId,
    thread_id: ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    covered_through: DateTime<Utc>,
    coverage_complete: bool,
    fingerprint: SessionDigestFingerprint,
    project_breakdown_fingerprint: SessionDigestFingerprint,
    event_count: u64,
    metric_revision: u32,
    estimator_revision: u32,
    project_breakdown_revision: u32,
    api_pricing_catalog_revision: u32,
}

impl PartialOrd for ReplicaFactCandidateKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReplicaFactCandidateKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.source_id
            .as_str()
            .cmp(other.source_id.as_str())
            .then_with(|| self.thread_id.cmp(&other.thread_id))
            .then_with(|| self.range_start.cmp(&other.range_start))
            .then_with(|| self.range_end.cmp(&other.range_end))
            .then_with(|| self.covered_through.cmp(&other.covered_through))
            .then_with(|| self.coverage_complete.cmp(&other.coverage_complete))
            .then_with(|| self.fingerprint.cmp(&other.fingerprint))
            .then_with(|| {
                self.project_breakdown_fingerprint
                    .cmp(&other.project_breakdown_fingerprint)
            })
            .then_with(|| self.event_count.cmp(&other.event_count))
            .then_with(|| self.metric_revision.cmp(&other.metric_revision))
            .then_with(|| self.estimator_revision.cmp(&other.estimator_revision))
            .then_with(|| {
                self.project_breakdown_revision
                    .cmp(&other.project_breakdown_revision)
            })
            .then_with(|| {
                self.api_pricing_catalog_revision
                    .cmp(&other.api_pricing_catalog_revision)
            })
    }
}

impl ReplicaFactCandidateKey {
    pub(crate) fn from_digest(
        source_id: NodeId,
        thread_id: ThreadId,
        digest: &SourceSessionDigest,
    ) -> Self {
        Self {
            source_id,
            thread_id,
            range_start: digest.range_start(),
            range_end: digest.range_end(),
            covered_through: digest.covered_through(),
            coverage_complete: digest.coverage_complete(),
            fingerprint: digest.fingerprint().clone(),
            project_breakdown_fingerprint: digest.project_breakdown_fingerprint().clone(),
            event_count: digest.event_count(),
            metric_revision: digest.metrics().metric_revision,
            estimator_revision: digest.metrics().estimator_revision,
            project_breakdown_revision: digest.metrics().project_breakdown_revision,
            api_pricing_catalog_revision: digest.metrics().api_pricing_catalog_revision,
        }
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.range_end <= self.range_start
            || self.range_end.signed_duration_since(self.range_start) > chrono::Duration::days(1)
            || self.covered_through < self.range_start
            || self.covered_through > self.range_end
            || (self.coverage_complete && self.covered_through != self.range_end)
            || self.metric_revision == 0
            || self.estimator_revision == 0
            || self.project_breakdown_revision == 0
            || self.api_pricing_catalog_revision == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fact resource cooldown candidate binding is invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct PlannerSourceEvidence {
    source_id: NodeId,
    kind: SourceKind,
    remote_binding: Option<SourceHistoryRemoteBinding>,
    digests: Vec<SourceSessionDigest>,
}

/// Plans at most one content-free fact participant for an already committed
/// and readable aggregate source. This function performs local reads only.
/// It never resolves SSH configuration and never opens a network connection.
pub(crate) fn plan_next_replica_fact_sync(
    history_store: &SourceHistoryStore,
    selected: &RemoteSyncHostSnapshot,
    local_source_id: &NodeId,
    redaction_profile: RedactionProfile,
    observed_at: chrono::DateTime<Utc>,
    excluded_resource_candidates: &std::collections::BTreeSet<ReplicaFactCandidateKey>,
    resume_after: Option<&ReplicaFactCandidateKey>,
) -> io::Result<ReplicaFactSyncPlan> {
    let selected_source = selected
        .host()
        .expected_source()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "selected host is unpaired"))?;
    let since = observed_at
        .checked_sub_days(chrono::Days::new(u64::from(MAX_FACT_RETENTION_DAYS)))
        .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC);
    let metadata = history_store
        .list_source_metadata()?
        .into_iter()
        .filter(|source| {
            source.include_in_aggregates()
                && source.aggregate_redaction_profile() == redaction_profile
        })
        .collect::<Vec<_>>();
    if metadata.len() > MAX_FACT_PLANNER_SOURCES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote fact planner source count exceeds its hard bound",
        ));
    }

    let selected_metadata = metadata
        .iter()
        .find(|source| source.source_id() == &selected_source.node_id)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "selected remote source metadata is unavailable after aggregate sync",
            )
        })?;
    if selected_metadata.kind() != SourceKind::Ssh {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected remote source metadata is not SSH",
        ));
    }
    let selected_snapshot = history_store.load_remote_history_snapshot_since(
        selected_metadata.source_id(),
        redaction_profile,
        since,
    )?;
    let Some(selected_active) = selected_snapshot.active_ref else {
        return Ok(ReplicaFactSyncPlan::NoWork);
    };
    let selected_digests = current_digest_upserts(selected_snapshot.session_digest_records)?;
    if selected_digests.len() > MAX_FACT_PLANNER_DIGESTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected remote digest inventory exceeds the fact planner bound",
        ));
    }
    if selected_digests.is_empty() {
        return Ok(ReplicaFactSyncPlan::NoWork);
    }
    let selected_keys = selected_digests
        .iter()
        .map(|digest| (digest.replica().thread_id().clone(), digest.range_start()))
        .collect::<std::collections::BTreeSet<_>>();
    let mut evidence = vec![PlannerSourceEvidence {
        source_id: selected_metadata.source_id().clone(),
        kind: SourceKind::Ssh,
        remote_binding: Some(selected_active.binding().clone()),
        digests: selected_digests,
    }];
    let mut digest_count = evidence[0].digests.len();

    for source in metadata
        .iter()
        .filter(|source| source.source_id() != selected_metadata.source_id())
    {
        let (remote_binding, records) = match source.kind() {
            SourceKind::Local => (
                None,
                history_store
                    .load_source_session_digest_records_since(
                        source.source_id(),
                        redaction_profile,
                        since,
                    )?
                    .records,
            ),
            SourceKind::Ssh => {
                let snapshot = history_store.load_remote_history_snapshot_since(
                    source.source_id(),
                    redaction_profile,
                    since,
                )?;
                let Some(active) = snapshot.active_ref else {
                    continue;
                };
                (
                    Some(active.binding().clone()),
                    snapshot.session_digest_records,
                )
            }
        };
        let digests = current_digest_upserts(records)?
            .into_iter()
            .filter(|digest| {
                selected_keys
                    .contains(&(digest.replica().thread_id().clone(), digest.range_start()))
            })
            .collect::<Vec<_>>();
        if digests.is_empty() {
            continue;
        }
        digest_count = digest_count.checked_add(digests.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote fact planner digest count overflowed",
            )
        })?;
        if digest_count > MAX_FACT_PLANNER_DIGESTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote fact planner matched digest inventory exceeds its hard bound",
            ));
        }
        evidence.push(PlannerSourceEvidence {
            source_id: source.source_id().clone(),
            kind: source.kind(),
            remote_binding,
            digests,
        });
    }

    let mut candidates = detect_replica_candidates(evidence.iter().flat_map(|source| {
        source
            .digests
            .iter()
            .map(|digest| ReplicaDigestObservation {
                source_id: &source.source_id,
                digest,
            })
    }))
    .into_iter()
    .filter(|candidate| {
        candidate.kind() == ReplicaCandidateKind::NeedsFacts
            && candidate
                .source_ids()
                .contains(selected_metadata.source_id())
    })
    .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .range_start()
            .cmp(&left.range_start())
            .then_with(|| left.thread_id().cmp(right.thread_id()))
    });

    let mut awaiting_exact_digest = false;
    let mut deferred_resource_candidate = false;
    let mut resume_reached = resume_after.is_none();
    let target_sources = [local_source_id, selected_metadata.source_id()];
    for candidate in candidates {
        for target_source_id in target_sources {
            if !candidate.source_ids().contains(target_source_id) {
                continue;
            }
            let Some(source) = evidence
                .iter()
                .find(|source| &source.source_id == target_source_id)
            else {
                continue;
            };
            let Some(digest) = source.digests.iter().find(|digest| {
                digest.replica().thread_id() == candidate.thread_id()
                    && digest.range_start() == candidate.range_start()
            }) else {
                continue;
            };
            let candidate_key = ReplicaFactCandidateKey::from_digest(
                source.source_id.clone(),
                candidate.thread_id().clone(),
                digest,
            );
            if !resume_reached {
                if resume_after == Some(&candidate_key) {
                    resume_reached = true;
                }
                continue;
            }
            if !digest.exact_event_identity() {
                awaiting_exact_digest = true;
                continue;
            }
            let active = history_store.load_active_fact_set(
                &source.source_id,
                redaction_profile,
                candidate.thread_id(),
            )?;
            let expected_binding = match source.kind {
                SourceKind::Local => ExpectedReplicaFactBinding::Local,
                SourceKind::Ssh => ExpectedReplicaFactBinding::Remote(
                    source.remote_binding.as_ref().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "remote replica evidence is missing its active binding",
                        )
                    })?,
                ),
            };
            if active_facts_cover_digest(digest, active.as_ref(), expected_binding) {
                continue;
            }
            if excluded_resource_candidates.contains(&candidate_key) {
                deferred_resource_candidate = true;
                continue;
            }
            let target = if source.kind == SourceKind::Local {
                PlannedReplicaFactTarget::Local
            } else {
                PlannedReplicaFactTarget::SelectedRemote
            };
            let mut validated_digests = source
                .digests
                .iter()
                .filter(|current| {
                    current.replica().thread_id() == candidate.thread_id()
                        && current.range_end() > since
                        && current.range_start() < observed_at
                        && current.exact_event_identity()
                })
                .map(FactDigestBinding::from_digest)
                .collect::<io::Result<Vec<_>>>()?;
            validated_digests.sort_by_key(FactDigestBinding::range_start);
            if validated_digests.len() > usize::from(MAX_FACT_RETENTION_DAYS) + 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fact planner digest binding count exceeds its hard bound",
                ));
            }
            if validated_digests.is_empty() {
                awaiting_exact_digest = true;
                continue;
            }
            return Ok(ReplicaFactSyncPlan::Work(Box::new(
                PlannedReplicaFactSync {
                    source_id: source.source_id.clone(),
                    thread_id: candidate.thread_id().clone(),
                    digest: digest.clone(),
                    validated_digests,
                    target,
                },
            )));
        }
    }

    Ok(if deferred_resource_candidate {
        ReplicaFactSyncPlan::DeferredResourceLimit
    } else if awaiting_exact_digest {
        ReplicaFactSyncPlan::AwaitingExactDigest
    } else {
        ReplicaFactSyncPlan::NoWork
    })
}

fn current_digest_upserts(
    records: Vec<crate::source_history::SourceSessionDigestRecord>,
) -> io::Result<Vec<SourceSessionDigest>> {
    let mut digests = Vec::new();
    for record in records {
        if let SourceSessionDigestChange::Upsert(digest) = record.change() {
            if digest.replica().thread_id() != record.thread_id()
                || digest.range_start() != record.range_start()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote fact planner observed an inconsistent digest record",
                ));
            }
            digests.push(digest.as_ref().clone());
        }
    }
    Ok(digests)
}

#[derive(Debug)]
pub enum RemoteFactSyncError {
    HostNotPaired {
        host_id: String,
    },
    ConfigurationChanged {
        host_id: String,
    },
    InvalidLimits(&'static str),
    InvalidRetentionDays(u16),
    ResponseBudgetExceeded,
    DecodedBudgetExceeded,
    RecordBudgetExceeded,
    RunBudgetExhausted,
    UnboundResponseEnvelope,
    UnexpectedResponse,
    PageContinuity(&'static str),
    /// Local/configuration validation that is guaranteed to have completed
    /// before the first transport exchange.
    PreTransportLocal(io::Error),
    Local(io::Error),
    Protocol(RemoteProtocolError),
    Transport(RemoteTransportError),
    Remote(RemoteFailure),
}

impl fmt::Display for RemoteFactSyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostNotPaired { host_id } => {
                write!(formatter, "remote host {host_id:?} is not paired")
            }
            Self::ConfigurationChanged { host_id } => write!(
                formatter,
                "remote host {host_id:?} changed while fact synchronization was in flight"
            ),
            Self::InvalidLimits(message) => formatter.write_str(message),
            Self::InvalidRetentionDays(days) => write!(
                formatter,
                "remote fact retention must be between 1 and {MAX_FACT_RETENTION_DAYS} days; got {days}"
            ),
            Self::ResponseBudgetExceeded => {
                formatter.write_str("remote fact response-byte budget was exceeded")
            }
            Self::DecodedBudgetExceeded => {
                formatter.write_str("remote fact decoded-memory budget was exceeded")
            }
            Self::RecordBudgetExceeded => {
                formatter.write_str("remote fact record-count budget was exceeded")
            }
            Self::RunBudgetExhausted => formatter.write_str(
                "remote fact run ended before a complete batch was received; active facts are unchanged",
            ),
            Self::UnboundResponseEnvelope => formatter.write_str(
                "remote fact response does not match the selected source, generation, redaction, and revisions",
            ),
            Self::UnexpectedResponse => {
                formatter.write_str("remote returned a non-fact response to a fact request")
            }
            Self::PageContinuity(message) => write!(formatter, "remote fact pages are discontinuous: {message}"),
            Self::PreTransportLocal(error) => {
                write!(formatter, "remote fact pre-transport local phase failed: {error}")
            }
            Self::Local(error) => write!(formatter, "remote fact local phase failed: {error}"),
            Self::Protocol(error) => write!(formatter, "remote fact protocol failed: {error}"),
            Self::Transport(error) => write!(formatter, "remote fact transport failed: {error}"),
            Self::Remote(failure) => write!(
                formatter,
                "remote fact request failed ({:?}): {}",
                failure.kind, failure.message
            ),
        }
    }
}

impl std::error::Error for RemoteFactSyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PreTransportLocal(error) | Self::Local(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for RemoteFactSyncError {
    fn from(error: io::Error) -> Self {
        Self::Local(error)
    }
}

impl From<RemoteProtocolError> for RemoteFactSyncError {
    fn from(error: RemoteProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl From<RemoteTransportError> for RemoteFactSyncError {
    fn from(error: RemoteTransportError) -> Self {
        Self::Transport(error)
    }
}

fn as_pre_transport_error(error: RemoteFactSyncError) -> RemoteFactSyncError {
    match error {
        RemoteFactSyncError::PreTransportLocal(_) => error,
        RemoteFactSyncError::Local(error) => RemoteFactSyncError::PreTransportLocal(error),
        RemoteFactSyncError::ConfigurationChanged { .. } => {
            RemoteFactSyncError::PreTransportLocal(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote fact configuration changed before transport",
            ))
        }
        RemoteFactSyncError::Protocol(_) => RemoteFactSyncError::PreTransportLocal(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote fact request could not be built before transport",
        )),
        error => error,
    }
}

#[derive(Clone, Debug)]
struct RemoteFactBinding {
    source: SourceGeneration,
    redaction_profile: RedactionProfile,
    revisions: ProtocolRevisions,
    history_binding: SourceHistoryRemoteBinding,
}

#[derive(Default)]
struct PageAccumulator {
    snapshot_records: Vec<RemoteUsageEventFactRecord>,
    delta_records: Vec<RemoteUsageEventFactRecord>,
    last_snapshot_event_id: Option<String>,
    last_delta_sequence: Option<u64>,
}

/// Synchronizes the facts emitted by exactly one physical remote thread.
///
/// Subagent threads must be requested separately. Their root-session fields
/// remain untouched so a later center-side projection can relate them to the
/// root turn without collapsing their physical source/thread replica.
#[allow(clippy::too_many_arguments)]
pub fn sync_remote_thread_facts_bounded(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    ownership_store: &HistoryOwnershipStore,
    history_store: &SourceHistoryStore,
    thread_id: ThreadId,
    validated_digests: Vec<FactDigestBinding>,
    retention_days: u16,
    transport: &mut impl RemoteFactTransport,
    limits: RemoteFactSyncLimits,
) -> Result<RemoteFactSyncReport, RemoteFactSyncError> {
    let limits = limits.validate()?;
    if retention_days == 0 || retention_days > MAX_FACT_RETENTION_DAYS {
        return Err(RemoteFactSyncError::InvalidRetentionDays(retention_days));
    }
    let binding = build_binding(selected).map_err(as_pre_transport_error)?;
    let expected_digests =
        wire_digest_bindings(&validated_digests).map_err(as_pre_transport_error)?;
    preflight_local_state(
        config_store,
        selected,
        ownership_store,
        history_store,
        &binding,
    )
    .map_err(as_pre_transport_error)?;

    let active = history_store
        .load_active_fact_set(
            &binding.source.node_id,
            binding.redaction_profile,
            &thread_id,
        )
        .map_err(RemoteFactSyncError::PreTransportLocal)?;
    let expected_active_version = active.as_ref().map(|facts| facts.version.clone());
    let initial_cursor = active
        .as_ref()
        .filter(|facts| facts.remote_binding.as_ref() == Some(&binding.history_binding))
        .map(active_wire_cursor)
        .transpose()
        .map_err(as_pre_transport_error)?;
    let mut position = initial_cursor.map_or(SessionFactsPosition::SnapshotStart, |fact_cursor| {
        SessionFactsPosition::DeltaStart { fact_cursor }
    });
    let mut accumulator = PageAccumulator::default();
    let mut exchanges = 0usize;
    let mut data_pages = 0usize;
    let mut response_bytes = 0usize;
    let mut decoded_response_bytes = 0usize;
    let mut restarted_from_snapshot = false;
    let deadline = Instant::now() + MAX_REMOTE_FACT_RUN_TIME;

    // A validated cursor-expiry may follow an initial bounded delta prefix.
    // Those discarded pages still count toward bytes/time/exchanges, but a
    // fresh snapshot receives its own complete data-page budget.
    let max_exchanges = limits.max_exchanges_per_run();
    for _ in 0..max_exchanges {
        let remaining_run_time = deadline.saturating_duration_since(Instant::now());
        if remaining_run_time.is_zero() {
            break;
        }
        let remaining_response_bytes = limits.max_response_bytes.saturating_sub(response_bytes);
        let remaining_decoded_bytes = limits
            .max_decoded_bytes
            .saturating_sub(decoded_response_bytes);
        if remaining_response_bytes < MIN_RESPONSE_BUDGET {
            break;
        }
        if remaining_decoded_bytes == 0 {
            break;
        }
        ensure_config_is_current(config_store, selected).map_err(|error| {
            if exchanges == 0 {
                as_pre_transport_error(error)
            } else {
                error
            }
        })?;
        let max_page_bytes = remaining_response_bytes
            .saturating_sub(REMOTE_FRAME_HEADER_BYTES)
            .min(MAX_REMOTE_FRAME_ENCODED_BYTES) as u32;
        let request = build_request(
            &binding,
            thread_id.clone(),
            retention_days,
            &expected_digests,
            position.clone(),
            max_page_bytes,
        )
        .map_err(|error| {
            if exchanges == 0 {
                as_pre_transport_error(error)
            } else {
                error
            }
        })?;

        // Every filesystem/config guard has been dropped before this call.
        let exchange = transport.exchange_host(
            selected.host(),
            &request,
            limits.exchange_timeout.min(remaining_run_time),
            remaining_decoded_bytes,
        )?;
        exchanges = exchanges.saturating_add(1);
        response_bytes = response_bytes
            .checked_add(exchange.response_bytes)
            .ok_or(RemoteFactSyncError::ResponseBudgetExceeded)?;
        if response_bytes > limits.max_response_bytes {
            return Err(RemoteFactSyncError::ResponseBudgetExceeded);
        }
        decoded_response_bytes = decoded_response_bytes
            .checked_add(exchange.response_decoded_bytes)
            .ok_or(RemoteFactSyncError::DecodedBudgetExceeded)?;
        if decoded_response_bytes > limits.max_decoded_bytes {
            return Err(RemoteFactSyncError::DecodedBudgetExceeded);
        }
        exchange.response.validate_for_request(&request)?;
        validate_bound_envelope(&binding, &request, &exchange.response)?;

        match exchange.response.result {
            RemoteExportResponseBody::FactSnapshot { page, payload } => {
                data_pages = data_pages.saturating_add(1);
                if data_pages > limits.max_pages.get() {
                    return Err(RemoteFactSyncError::RunBudgetExhausted);
                }
                let RemoteSessionFactPayload::Snapshot(payload) = payload else {
                    return Err(RemoteFactSyncError::UnexpectedResponse);
                };
                append_snapshot_page(&mut accumulator, payload.records)?;
                if page.has_more {
                    if data_pages == limits.max_pages.get() {
                        return Err(RemoteFactSyncError::RunBudgetExhausted);
                    }
                    position = snapshot_continuation(&page)?;
                    continue;
                }
                let cursor =
                    page.activate_fact_cursor
                        .ok_or(RemoteFactSyncError::PageContinuity(
                            "final snapshot omitted its activation cursor",
                        ))?;
                return commit_complete_batch(
                    config_store,
                    selected,
                    ownership_store,
                    history_store,
                    &binding,
                    &thread_id,
                    validated_digests.clone(),
                    expected_active_version,
                    FactBatchKind::Snapshot,
                    cursor,
                    accumulator.snapshot_records,
                    exchanges,
                    response_bytes,
                    decoded_response_bytes,
                    restarted_from_snapshot,
                );
            }
            RemoteExportResponseBody::FactDelta { page, payload } => {
                data_pages = data_pages.saturating_add(1);
                if data_pages > limits.max_pages.get() {
                    return Err(RemoteFactSyncError::RunBudgetExhausted);
                }
                let RemoteSessionFactPayload::Delta(payload) = payload else {
                    return Err(RemoteFactSyncError::UnexpectedResponse);
                };
                let base_cursor = initial_cursor.ok_or(RemoteFactSyncError::PageContinuity(
                    "delta response has no active local cursor",
                ))?;
                let page_is_empty = payload.changes.is_empty();
                append_delta_page(&mut accumulator, base_cursor, payload.changes)?;
                if page.has_more {
                    if page_is_empty {
                        return Err(RemoteFactSyncError::PageContinuity(
                            "an intermediate delta page is empty",
                        ));
                    }
                    if data_pages == limits.max_pages.get() {
                        return Err(RemoteFactSyncError::RunBudgetExhausted);
                    }
                    position = delta_continuation(&page, base_cursor)?;
                    continue;
                }
                let cursor =
                    page.activate_fact_cursor
                        .ok_or(RemoteFactSyncError::PageContinuity(
                            "final delta omitted its activation cursor",
                        ))?;
                ensure_delta_reaches_watermark(&accumulator, base_cursor, cursor)?;
                return commit_complete_batch(
                    config_store,
                    selected,
                    ownership_store,
                    history_store,
                    &binding,
                    &thread_id,
                    validated_digests.clone(),
                    expected_active_version,
                    FactBatchKind::Delta,
                    cursor,
                    accumulator.delta_records,
                    exchanges,
                    response_bytes,
                    decoded_response_bytes,
                    restarted_from_snapshot,
                );
            }
            RemoteExportResponseBody::Failure(failure)
                if failure.kind == RemoteFailureKind::FactCursorExpired
                    && matches!(
                        position,
                        SessionFactsPosition::DeltaStart { .. }
                            | SessionFactsPosition::DeltaContinue { .. }
                    ) =>
            {
                // Failure validation accepts diagnostic envelopes, but the
                // exact-envelope check above intentionally ran first.
                accumulator = PageAccumulator::default();
                data_pages = 0;
                position = SessionFactsPosition::SnapshotStart;
                restarted_from_snapshot = true;
            }
            RemoteExportResponseBody::Failure(failure) => {
                return Err(RemoteFactSyncError::Remote(failure));
            }
            RemoteExportResponseBody::Probe(_) | RemoteExportResponseBody::Delta { .. } => {
                return Err(RemoteFactSyncError::UnexpectedResponse);
            }
        }
    }

    Err(RemoteFactSyncError::RunBudgetExhausted)
}

fn build_binding(
    selected: &RemoteSyncHostSnapshot,
) -> Result<RemoteFactBinding, RemoteFactSyncError> {
    let host = selected.host();
    let source =
        host.expected_source()
            .cloned()
            .ok_or_else(|| RemoteFactSyncError::HostNotPaired {
                host_id: host.id().to_owned(),
            })?;
    let revisions = current_revisions();
    let history_binding = SourceHistoryRemoteBinding::new(source.clone(), revisions.clone())
        .map_err(RemoteFactSyncError::Local)?;
    Ok(RemoteFactBinding {
        source,
        redaction_profile: if host.redact_content() {
            RedactionProfile::Redacted
        } else {
            RedactionProfile::PreviewEnabled
        },
        revisions,
        history_binding,
    })
}

fn preflight_local_state(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    ownership_store: &HistoryOwnershipStore,
    history_store: &SourceHistoryStore,
    binding: &RemoteFactBinding,
) -> Result<(), RemoteFactSyncError> {
    ensure_config_is_current(config_store, selected)?;
    if ownership_store.profile_id() != history_store.profile_id()
        || ownership_store.redaction_profile() != binding.redaction_profile
    {
        return Err(RemoteFactSyncError::Local(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote fact stores do not share the selected profile/redaction namespace",
        )));
    }
    match ownership_store
        .load_manifest()
        .map_err(RemoteFactSyncError::Local)?
    {
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active => {}
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
            return Err(RemoteFactSyncError::Local(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote fact sync requires active v2 history ownership",
            )));
        }
    }
    let source = history_store
        .load_source_metadata(&binding.source.node_id)
        .map_err(RemoteFactSyncError::Local)?;
    if source.kind() != SourceKind::Ssh
        || source.aggregate_redaction_profile() != binding.redaction_profile
    {
        return Err(RemoteFactSyncError::Local(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote fact source metadata is not the exact SSH/redaction namespace",
        )));
    }
    Ok(())
}

fn active_wire_cursor(active: &ActiveFactSet) -> Result<RemoteFactCursor, RemoteFactSyncError> {
    Ok(RemoteFactCursor {
        fact_generation: NonZeroU64::new(active.cursor.fact_generation()).ok_or_else(|| {
            RemoteFactSyncError::Local(io::Error::new(
                io::ErrorKind::InvalidData,
                "active fact cursor has a zero generation",
            ))
        })?,
        through_sequence: active.cursor.through_sequence(),
    })
}

fn build_request(
    binding: &RemoteFactBinding,
    thread_id: ThreadId,
    retention_days: u16,
    expected_digests: &[SessionFactsDigestBinding],
    position: SessionFactsPosition,
    max_page_bytes: u32,
) -> Result<RemoteExportRequest, RemoteFactSyncError> {
    Ok(RemoteExportRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        client_version: env!("CARGO_PKG_VERSION").parse()?,
        expected_source: Some(binding.source.clone()),
        redaction_profile: binding.redaction_profile,
        max_page_bytes,
        accepted_revisions: current_accepted_revisions(),
        request: RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
            thread_id,
            retention_days,
            expected_digests: expected_digests.to_vec(),
            position,
        }),
    })
}

fn wire_digest_bindings(
    bindings: &[FactDigestBinding],
) -> Result<Vec<SessionFactsDigestBinding>, RemoteFactSyncError> {
    if bindings.is_empty() || bindings.len() > usize::from(MAX_FACT_RETENTION_DAYS) + 1 {
        return Err(RemoteFactSyncError::Local(io::Error::new(
            io::ErrorKind::InvalidData,
            "fact sync requires a bounded nonempty digest binding set",
        )));
    }
    bindings
        .iter()
        .map(|binding| {
            Ok(SessionFactsDigestBinding {
                range_start: binding.range_start(),
                range_end: binding.range_end(),
                covered_through: binding.covered_through(),
                coverage_complete: binding.coverage_complete(),
                fingerprint: binding.fingerprint().as_str().parse()?,
                project_breakdown_fingerprint: binding
                    .project_breakdown_fingerprint()
                    .as_str()
                    .parse()?,
                event_count: binding.event_count(),
                metric_revision: NonZeroU32::new(binding.metric_revision()).ok_or_else(|| {
                    RemoteFactSyncError::Local(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "fact digest metric revision is zero",
                    ))
                })?,
                estimator_revision: NonZeroU32::new(binding.estimator_revision()).ok_or_else(
                    || {
                        RemoteFactSyncError::Local(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "fact digest estimator revision is zero",
                        ))
                    },
                )?,
                project_breakdown_revision: NonZeroU32::new(binding.project_breakdown_revision())
                    .ok_or_else(|| {
                    RemoteFactSyncError::Local(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "fact digest project revision is zero",
                    ))
                })?,
                api_pricing_catalog_revision: NonZeroU32::new(
                    binding.api_pricing_catalog_revision(),
                )
                .ok_or_else(|| {
                    RemoteFactSyncError::Local(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "fact digest pricing revision is zero",
                    ))
                })?,
            })
        })
        .collect()
}

fn validate_bound_envelope(
    binding: &RemoteFactBinding,
    request: &RemoteExportRequest,
    response: &RemoteSessionFactResponse,
) -> Result<(), RemoteFactSyncError> {
    if request.expected_source.as_ref() != Some(&binding.source)
        || request.redaction_profile != binding.redaction_profile
        || response.source != binding.source
        || response.redaction_profile != binding.redaction_profile
        || response.revisions != binding.revisions
    {
        return Err(RemoteFactSyncError::UnboundResponseEnvelope);
    }
    Ok(())
}

fn ensure_config_is_current(
    store: &RemotesConfigStore,
    expected: &RemoteSyncHostSnapshot,
) -> Result<(), RemoteFactSyncError> {
    let current = store.load().map_err(RemoteFactSyncError::Local)?;
    if current.config_revision() != expected.config_revision()
        || current.host(expected.host().id()) != Some(expected.host())
    {
        return Err(RemoteFactSyncError::ConfigurationChanged {
            host_id: expected.host().id().to_owned(),
        });
    }
    Ok(())
}

fn snapshot_continuation(
    page: &FactSnapshotPage,
) -> Result<SessionFactsPosition, RemoteFactSyncError> {
    Ok(SessionFactsPosition::SnapshotContinue {
        snapshot_id: page.snapshot_id.clone(),
        fact_generation: page.fact_generation,
        snapshot_watermark: page.snapshot_watermark,
        page_token: page
            .next_page_token
            .clone()
            .ok_or(RemoteFactSyncError::PageContinuity(
                "intermediate snapshot omitted its page token",
            ))?,
    })
}

fn delta_continuation(
    page: &FactDeltaPage,
    fact_cursor: RemoteFactCursor,
) -> Result<SessionFactsPosition, RemoteFactSyncError> {
    Ok(SessionFactsPosition::DeltaContinue {
        fact_cursor,
        batch_id: page.batch_id.clone(),
        delta_watermark: page.delta_watermark,
        page_token: page
            .next_page_token
            .clone()
            .ok_or(RemoteFactSyncError::PageContinuity(
                "intermediate delta omitted its page token",
            ))?,
    })
}

fn append_snapshot_page(
    accumulator: &mut PageAccumulator,
    records: Vec<RemoteUsageEventFactRecord>,
) -> Result<(), RemoteFactSyncError> {
    ensure_record_capacity(accumulator.snapshot_records.len(), records.len())?;
    for record in records {
        if accumulator
            .last_snapshot_event_id
            .as_deref()
            .is_some_and(|last| last >= record.event_id.as_str())
        {
            return Err(RemoteFactSyncError::PageContinuity(
                "snapshot event IDs are not globally sorted and unique",
            ));
        }
        accumulator.last_snapshot_event_id = Some(record.event_id.as_str().to_owned());
        accumulator.snapshot_records.push(record);
    }
    Ok(())
}

fn append_delta_page(
    accumulator: &mut PageAccumulator,
    base_cursor: RemoteFactCursor,
    changes: Vec<crate::remote_protocol::RemoteUsageEventFactDeltaChange>,
) -> Result<(), RemoteFactSyncError> {
    ensure_record_capacity(accumulator.delta_records.len(), changes.len())?;
    for change in changes {
        let previous = accumulator
            .last_delta_sequence
            .unwrap_or(base_cursor.through_sequence);
        let expected = previous
            .checked_add(1)
            .ok_or(RemoteFactSyncError::PageContinuity(
                "delta sequence overflowed",
            ))?;
        if change.sequence.get() != expected {
            return Err(RemoteFactSyncError::PageContinuity(
                "delta sequences contain a gap, duplicate, or regression",
            ));
        }
        accumulator.last_delta_sequence = Some(change.sequence.get());
        accumulator.delta_records.push(change.record);
    }
    Ok(())
}

fn ensure_record_capacity(current: usize, incoming: usize) -> Result<(), RemoteFactSyncError> {
    if current
        .checked_add(incoming)
        .is_none_or(|total| total > MAX_COMPLETE_FACT_BATCH_RECORDS)
    {
        return Err(RemoteFactSyncError::RecordBudgetExceeded);
    }
    Ok(())
}

fn ensure_delta_reaches_watermark(
    accumulator: &PageAccumulator,
    base_cursor: RemoteFactCursor,
    activate_cursor: RemoteFactCursor,
) -> Result<(), RemoteFactSyncError> {
    if accumulator
        .last_delta_sequence
        .unwrap_or(base_cursor.through_sequence)
        != activate_cursor.through_sequence
    {
        return Err(RemoteFactSyncError::PageContinuity(
            "delta changes do not reach the activation watermark",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_complete_batch(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    ownership_store: &HistoryOwnershipStore,
    history_store: &SourceHistoryStore,
    binding: &RemoteFactBinding,
    thread_id: &ThreadId,
    validated_digests: Vec<FactDigestBinding>,
    expected_active_version: Option<ActiveFactVersion>,
    kind: FactBatchKind,
    remote_cursor: RemoteFactCursor,
    wire_records: Vec<RemoteUsageEventFactRecord>,
    exchanges: usize,
    response_bytes: usize,
    decoded_response_bytes: usize,
    restarted_from_snapshot: bool,
) -> Result<RemoteFactSyncReport, RemoteFactSyncError> {
    let records_received = wire_records.len();
    let replica = SessionReplicaKey::new(binding.source.node_id.clone(), thread_id.clone());
    let changes = wire_records
        .into_iter()
        .map(|record| convert_record(&replica, record))
        .collect::<io::Result<Vec<_>>>()
        .map_err(RemoteFactSyncError::Local)?;
    let cursor = FactCursor::new(
        remote_cursor.fact_generation.get(),
        remote_cursor.through_sequence,
    )
    .map_err(RemoteFactSyncError::Local)?;
    let batch = CompleteFactBatch {
        batch_id: FactBatchId::generate().map_err(RemoteFactSyncError::Local)?,
        kind,
        replica,
        expected_active_version,
        remote_binding: Some(binding.history_binding.clone()),
        validated_digests,
        activate_cursor: cursor,
        completed_at: Utc::now(),
        changes,
    };
    batch.validate().map_err(RemoteFactSyncError::Local)?;

    // Merge/sort/write, generation validation, digest validation, and the
    // namespace-cap traversal all happen while the candidate is invisible and
    // before acquiring the remotes config fence.
    let publication =
        stage_and_prevalidate_under_fresh_writer(ownership_store, history_store, binding, &batch)
            .map_err(RemoteFactSyncError::Local)?;

    // Distinguish a stale config snapshot (the callback never started) from a
    // storage/ownership error inside the exact-config critical section.
    let entered = Cell::new(false);
    let result =
        config_store.with_current_host(selected.config_revision(), selected.host(), || {
            entered.set(true);
            publish_under_fresh_writer(ownership_store, history_store, binding, &publication)
        });
    let mut activation = match result {
        Ok(report) => report,
        Err(_error) if !entered.get() => {
            return Err(RemoteFactSyncError::ConfigurationChanged {
                host_id: selected.host().id().to_owned(),
            });
        }
        Err(error) => return Err(RemoteFactSyncError::Local(error)),
    };
    activation.cleanup_pending |=
        cleanup_under_fresh_writer(ownership_store, history_store, &publication);

    Ok(RemoteFactSyncReport {
        exchanges,
        response_bytes,
        decoded_response_bytes,
        records_received,
        restarted_from_snapshot,
        activation,
        cursor,
    })
}

fn publish_under_fresh_writer(
    ownership_store: &HistoryOwnershipStore,
    history_store: &SourceHistoryStore,
    binding: &RemoteFactBinding,
    publication: &PrevalidatedFactPublication,
) -> io::Result<FactActivationReport> {
    let lease = match ownership_store.try_acquire_writer_lease()? {
        TryWriterLease::Acquired(lease) => lease,
        TryWriterLease::Busy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer is busy; retry remote fact publication later",
            ));
        }
    };
    let manifest = match ownership_store.load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active =>
        {
            manifest
        }
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote fact sync requires active v2 history ownership",
            ));
        }
    };
    let authority = ownership_store.authorize_v2_write(&lease, &manifest)?;
    let writer = history_store.writer(&authority)?;
    let source = history_store.load_source_metadata(&binding.source.node_id)?;
    if source.kind() != SourceKind::Ssh
        || source.aggregate_redaction_profile() != binding.redaction_profile
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote fact source metadata changed before publication",
        ));
    }
    writer.publish_prevalidated_fact_batch(publication)
}

fn stage_and_prevalidate_under_fresh_writer(
    ownership_store: &HistoryOwnershipStore,
    history_store: &SourceHistoryStore,
    binding: &RemoteFactBinding,
    batch: &CompleteFactBatch,
) -> io::Result<PrevalidatedFactPublication> {
    stage_and_prevalidate_under_fresh_writer_with_hook(
        ownership_store,
        history_store,
        binding,
        batch,
        || {},
    )
}

fn stage_and_prevalidate_under_fresh_writer_with_hook(
    ownership_store: &HistoryOwnershipStore,
    history_store: &SourceHistoryStore,
    binding: &RemoteFactBinding,
    batch: &CompleteFactBatch,
    before_stage: impl FnOnce(),
) -> io::Result<PrevalidatedFactPublication> {
    let lease = match ownership_store.try_acquire_writer_lease()? {
        TryWriterLease::Acquired(lease) => lease,
        TryWriterLease::Busy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer is busy; retry remote fact staging later",
            ));
        }
    };
    let manifest = match ownership_store.load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active =>
        {
            manifest
        }
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote fact staging requires active v2 history ownership",
            ));
        }
    };
    let authority = ownership_store.authorize_v2_write(&lease, &manifest)?;
    let writer = history_store.writer(&authority)?;
    let source = history_store.load_source_metadata(&binding.source.node_id)?;
    if source.kind() != SourceKind::Ssh
        || source.aggregate_redaction_profile() != binding.redaction_profile
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote fact source metadata changed before staging",
        ));
    }
    before_stage();
    writer.stage_complete_fact_batch(&binding.source.node_id, binding.redaction_profile, batch)?;
    writer.prevalidate_staged_fact_batch(
        &binding.source.node_id,
        binding.redaction_profile,
        &batch.batch_id,
    )
}

fn cleanup_under_fresh_writer(
    ownership_store: &HistoryOwnershipStore,
    history_store: &SourceHistoryStore,
    publication: &PrevalidatedFactPublication,
) -> bool {
    let Ok(TryWriterLease::Acquired(lease)) = ownership_store.try_acquire_writer_lease() else {
        return true;
    };
    let Ok(OwnershipManifestStatus::Initialized(manifest)) = ownership_store.load_manifest() else {
        return true;
    };
    if manifest.state() != HistoryOwnershipState::V2Active {
        return true;
    }
    let Ok(authority) = ownership_store.authorize_v2_write(&lease, &manifest) else {
        return true;
    };
    let Ok(writer) = history_store.writer(&authority) else {
        return true;
    };
    writer.cleanup_prevalidated_fact_publication(publication)
}

fn convert_record(
    replica: &SessionReplicaKey,
    record: RemoteUsageEventFactRecord,
) -> io::Result<UsageEventFactRecord> {
    let revision = record.revision.get();
    match record.mutation {
        RemoteUsageEventFactMutation::Upsert(fact) => {
            let fact = UsageEventFact::new(
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
            )?;
            UsageEventFactRecord::upsert(revision, fact)
        }
        RemoteUsageEventFactMutation::Tombstone => {
            UsageEventFactRecord::tombstone(record.event_id, record.occurred_at, revision)
        }
    }
}

fn convert_metrics(
    metrics: crate::remote_protocol::RemoteSessionUsageMetrics,
) -> SessionUsageMetrics {
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::num::NonZeroU64;
    use std::str::FromStr;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use chrono::{DateTime, Duration, TimeZone};
    use tempfile::TempDir;

    use super::*;
    use crate::history_runtime::HistoryRuntime;
    use crate::remote_protocol::{
        BinaryVersion, FactBatchId as RemoteFactBatchId, FactDeltaPageToken, FactSnapshotId,
        FactSnapshotPageToken, REMOTE_SESSION_FACT_SCHEMA_VERSION, RemoteApiCostAmount,
        RemoteExportResponse, RemoteFactDeltaPayload, RemoteFactSnapshotPayload,
        RemoteSessionUsageMetrics, RemoteTiming, RemoteTokenUsage, RemoteU128,
        RemoteUsageEventFact, RemoteUsageEventFactDeltaChange,
    };
    use crate::remotes_config::{
        RemoteHostConfig, RemoteHostEdit, RemotesConfig, RemotesConfigMutation,
    };
    use crate::source_history::{
        SessionDigestFingerprint, SourceHistoryRemoteGenerationId, SourceMetadata,
        SourceSessionDigestRecord, UsageEventFactChange,
    };
    use crate::source_model::ObservedProjectKey;

    const SOURCE: &str = "node-0123456789abcdef0123456789abcdef";
    const OTHER_SOURCE: &str = "node-fedcba9876543210fedcba9876543210";
    const THREAD: &str = "01a11111-2222-7333-8444-555555555555";

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn source(node_id: &str) -> SourceGeneration {
        SourceGeneration {
            node_id: node_id.parse().unwrap(),
            generation: NonZeroU64::new(7).unwrap(),
        }
    }

    fn paired_config(
        temp: &TempDir,
    ) -> (
        RemotesConfigStore,
        RemotesConfig,
        RemoteHostConfig,
        RemoteSyncHostSnapshot,
    ) {
        let store = RemotesConfigStore::new(temp.path().join("config/remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        let paired = store
            .update(
                configured.config_revision(),
                RemotesConfigMutation::pair_pin("dev", source(SOURCE)),
            )
            .unwrap();
        let host = paired.host("dev").unwrap().clone();
        let selected = RemoteSyncHostSnapshot::capture_manual(&paired, &host).unwrap();
        (store, paired, host, selected)
    }

    struct Fixture {
        _temp: TempDir,
        config_store: RemotesConfigStore,
        selected: RemoteSyncHostSnapshot,
        runtime: HistoryRuntime,
        thread_id: ThreadId,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let codex_home = temp.path().join("codex-home");
            std::fs::create_dir_all(&codex_home).unwrap();
            let mut runtime =
                HistoryRuntime::new(temp.path().join("state/history-v1"), &codex_home, true)
                    .unwrap();
            runtime.ensure_v2_active().unwrap();
            let (config_store, _, _, selected) = paired_config(&temp);
            let metadata = SourceMetadata::new_with_redaction_profile(
                SOURCE.parse().unwrap(),
                SourceKind::Ssh,
                "dev",
                RedactionProfile::Redacted,
            )
            .unwrap();
            with_writer(&runtime, |writer| writer.save_source_metadata(&metadata));
            Self {
                _temp: temp,
                config_store,
                selected,
                runtime,
                thread_id: THREAD.parse().unwrap(),
            }
        }

        fn active(&self) -> Option<ActiveFactSet> {
            self.runtime
                .source_history()
                .load_active_fact_set(
                    &SOURCE.parse().unwrap(),
                    RedactionProfile::Redacted,
                    &self.thread_id,
                )
                .unwrap()
        }

        fn seed(&self, through_sequence: u64) -> ActiveFactSet {
            self.seed_with_binding(through_sequence, test_remote_binding(7))
        }

        fn seed_with_binding(
            &self,
            through_sequence: u64,
            remote_binding: SourceHistoryRemoteBinding,
        ) -> ActiveFactSet {
            let replica = SessionReplicaKey::new(SOURCE.parse().unwrap(), self.thread_id.clone());
            let record =
                convert_record(&replica, remote_record("event-0001", 1, at(29, 8), 11)).unwrap();
            let batch = CompleteFactBatch {
                batch_id: FactBatchId::generate().unwrap(),
                kind: FactBatchKind::Snapshot,
                replica,
                expected_active_version: None,
                remote_binding: Some(remote_binding),
                validated_digests: Vec::new(),
                activate_cursor: FactCursor::new(11, through_sequence).unwrap(),
                completed_at: at(30, 8),
                changes: vec![record],
            };
            with_writer(&self.runtime, |writer| {
                writer
                    .stage_and_activate_complete_fact_batch(
                        &SOURCE.parse().unwrap(),
                        RedactionProfile::Redacted,
                        &batch,
                    )
                    .map(|_| ())
            });
            self.active().unwrap()
        }
    }

    fn test_remote_binding(source_generation: u64) -> SourceHistoryRemoteBinding {
        SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: SOURCE.parse().unwrap(),
                generation: NonZeroU64::new(source_generation).unwrap(),
            },
            current_revisions(),
        )
        .unwrap()
    }

    fn with_writer(
        runtime: &HistoryRuntime,
        operation: impl FnOnce(
            &crate::source_history::SourceHistoryWriter<'_, '_, '_>,
        ) -> io::Result<()>,
    ) {
        let lease = runtime.ownership().acquire_writer_lease().unwrap();
        let manifest = match runtime.ownership().load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            OwnershipManifestStatus::Uninitialized => panic!("runtime must be v2 active"),
        };
        let authority = runtime
            .ownership()
            .authorize_v2_write(&lease, &manifest)
            .unwrap();
        let writer = runtime.source_history().writer(&authority).unwrap();
        operation(&writer).unwrap();
    }

    fn metrics(total: u64) -> RemoteSessionUsageMetrics {
        let revisions = current_revisions();
        RemoteSessionUsageMetrics {
            token_usage: RemoteTokenUsage {
                input_tokens: total.saturating_sub(1),
                cached_input_tokens: 1.min(total.saturating_sub(1)),
                cache_write_input_tokens: 0,
                output_tokens: u64::from(total > 0),
                reasoning_output_tokens: 0,
                total_tokens: total,
            },
            estimated_cost_units: RemoteU128::new(u128::from(total) * 13),
            api_long_context_extra_cost_units: Some(RemoteU128::new(u128::from(total) * 3)),
            api_equivalent_cost: RemoteApiCostAmount {
                minimum_pico_usd: RemoteU128::new(u128::from(total) * 7),
                maximum_pico_usd: RemoteU128::new(u128::from(total) * 9),
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: total,
                priced_tokens: total,
            },
            call_count: 1,
            metric_revision: revisions.metric,
            estimator_revision: revisions.estimator,
            project_breakdown_revision: revisions.project_breakdown,
            api_pricing_catalog_revision: revisions.api_pricing_catalog,
            partial_reasons: vec!["synthetic-test-evidence".to_owned()],
        }
    }

    fn remote_record(
        event_id: &str,
        revision: u64,
        occurred_at: DateTime<Utc>,
        total: u64,
    ) -> RemoteUsageEventFactRecord {
        let event_id: crate::source_history::UsageEventId = event_id.parse().unwrap();
        RemoteUsageEventFactRecord {
            event_id: event_id.clone(),
            occurred_at,
            revision: NonZeroU64::new(revision).unwrap(),
            mutation: RemoteUsageEventFactMutation::Upsert(Box::new(RemoteUsageEventFact {
                event_id,
                occurred_at,
                observed_project_key: observed_project_key(),
                emitting_thread_id: THREAD.parse().unwrap(),
                emitting_turn_id: Some(format!("turn-{revision}")),
                parent_thread_id: Some("01a00000-0000-7000-8000-000000000000".parse().unwrap()),
                project_session_thread_id: Some(
                    "01a99999-9999-7999-8999-999999999999".parse().unwrap(),
                ),
                root_session_thread_id: "01a99999-9999-7999-8999-999999999999".parse().unwrap(),
                root_session_turn_id: Some("root-turn-7".to_owned()),
                model: Some("gpt-5.6-sol".to_owned()),
                service_tier: Some("priority".to_owned()),
                digest_token_usage: metrics(total).token_usage,
                request_usage_exact: true,
                exact_event_identity: true,
                metrics: metrics(total),
            })),
        }
    }

    fn observed_project_key() -> ObservedProjectKey {
        format!("opk-hmac-sha256-v1-{}", "a".repeat(64))
            .parse()
            .unwrap()
    }

    fn digest_record(
        source_id: &str,
        thread_id: &str,
        range_start: DateTime<Utc>,
        fingerprint: char,
        total: u64,
    ) -> SourceSessionDigestRecord {
        let range_end = range_start + Duration::days(1);
        SourceSessionDigestRecord::upsert(
            1,
            SourceSessionDigest::new(
                SessionReplicaKey::new(source_id.parse().unwrap(), thread_id.parse().unwrap()),
                range_start,
                range_end,
                range_end,
                format!(
                    "session-digest-sha256-v1-{}",
                    fingerprint.to_string().repeat(64)
                )
                .parse::<SessionDigestFingerprint>()
                .unwrap(),
                format!(
                    "session-digest-sha256-v1-{}",
                    fingerprint.to_string().repeat(64)
                )
                .parse::<SessionDigestFingerprint>()
                .unwrap(),
                1,
                true,
                true,
                Vec::new(),
                convert_metrics(metrics(total)),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn validated_digest_bindings() -> Vec<FactDigestBinding> {
        validated_digest_bindings_for_records(&[remote_record("event-0001", 1, at(29, 8), 11)])
    }

    fn validated_digest_bindings_for_records(
        records: &[RemoteUsageEventFactRecord],
    ) -> Vec<FactDigestBinding> {
        let replica = SessionReplicaKey::new(SOURCE.parse().unwrap(), THREAD.parse().unwrap());
        let local_records = records
            .iter()
            .cloned()
            .map(|record| convert_record(&replica, record).unwrap())
            .collect::<Vec<_>>();
        let mut by_day = BTreeMap::<_, Vec<_>>::new();
        for record in &local_records {
            let UsageEventFactChange::Upsert(fact) = record.change() else {
                continue;
            };
            by_day
                .entry(fact.occurred_at().date_naive())
                .or_default()
                .push(fact.as_ref());
        }
        let revisions = current_revisions();
        by_day
            .into_iter()
            .map(|(day, facts)| {
                let range_start = Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap());
                let range_end = range_start + Duration::days(1);
                let (fingerprint, project_fingerprint) =
                    crate::source_export::canonical_fact_fingerprints_for_test(
                        &replica,
                        range_start,
                        range_end,
                        &facts,
                    )
                    .unwrap();
                let digest = SourceSessionDigest::new(
                    replica.clone(),
                    range_start,
                    range_end,
                    range_end,
                    fingerprint,
                    project_fingerprint,
                    u64::try_from(facts.len()).unwrap(),
                    true,
                    true,
                    vec![observed_project_key()],
                    SessionUsageMetrics {
                        call_count: u64::try_from(facts.len()).unwrap(),
                        metric_revision: revisions.metric.get(),
                        estimator_revision: revisions.estimator.get(),
                        project_breakdown_revision: revisions.project_breakdown.get(),
                        api_pricing_catalog_revision: revisions.api_pricing_catalog.get(),
                        ..SessionUsageMetrics::default()
                    },
                )
                .unwrap();
                FactDigestBinding::from_digest(&digest).unwrap()
            })
            .collect()
    }

    fn install_remote_digests(
        runtime: &HistoryRuntime,
        source_id: &str,
        label: &str,
        generation_id: &str,
        records: &[SourceSessionDigestRecord],
    ) {
        let source_id: NodeId = source_id.parse().unwrap();
        let metadata = SourceMetadata::new_with_redaction_profile(
            source_id.clone(),
            SourceKind::Ssh,
            label,
            RedactionProfile::Redacted,
        )
        .unwrap();
        let generation: SourceHistoryRemoteGenerationId = generation_id.parse().unwrap();
        let binding = SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: source_id.clone(),
                generation: NonZeroU64::new(7).unwrap(),
            },
            current_revisions(),
        )
        .unwrap();
        with_writer(runtime, |writer| {
            writer.save_source_metadata(&metadata)?;
            writer.ensure_remote_history_generation(
                &source_id,
                RedactionProfile::Redacted,
                &generation,
                &binding,
            )?;
            writer.apply_remote_history_generation_page(
                &source_id,
                RedactionProfile::Redacted,
                &generation,
                &binding,
                &[],
                records,
            )?;
            writer.activate_remote_history_generation(
                &source_id,
                RedactionProfile::Redacted,
                None,
                &generation,
                &binding,
                at(31, 12),
            )
        });
    }

    #[derive(Clone, Debug)]
    enum FakeReply {
        Snapshot {
            generation: u64,
            watermark: u64,
            records: Vec<RemoteUsageEventFactRecord>,
            has_more: bool,
            token: Option<&'static str>,
        },
        Delta {
            generation: u64,
            watermark: u64,
            changes: Vec<RemoteUsageEventFactDeltaChange>,
            has_more: bool,
            token: Option<&'static str>,
        },
        CursorExpired,
        WrongSourceSnapshot,
        TransportFailure,
    }

    struct FakeTransport {
        replies: VecDeque<FakeReply>,
        requests: Vec<RemoteExportRequest>,
        decoded_limits: Vec<usize>,
        on_exchange: Option<Box<dyn FnMut(usize)>>,
    }

    impl FakeTransport {
        fn new(replies: impl IntoIterator<Item = FakeReply>) -> Self {
            Self {
                replies: replies.into_iter().collect(),
                requests: Vec::new(),
                decoded_limits: Vec::new(),
                on_exchange: None,
            }
        }
    }

    impl RemoteFactTransport for FakeTransport {
        fn exchange(
            &mut self,
            ssh_host: &str,
            request: &RemoteExportRequest,
            _timeout: StdDuration,
            max_decoded_bytes: usize,
        ) -> Result<
            RemoteExchangeReport<DeltaPayload, RemoteSessionFactPayload>,
            RemoteTransportError,
        > {
            assert_eq!(ssh_host, "dev-alias");
            self.requests.push(request.clone());
            self.decoded_limits.push(max_decoded_bytes);
            let exchange_number = self.requests.len();
            if let Some(hook) = self.on_exchange.as_mut() {
                hook(exchange_number);
            }
            let reply = self.replies.pop_front().expect("a queued fake reply");
            if matches!(reply, FakeReply::TransportFailure) {
                return Err(RemoteTransportError::InvalidTimeout);
            }
            Ok(RemoteExchangeReport {
                response: fake_response(request, reply),
                elapsed: StdDuration::from_millis(5),
                request_bytes: 256,
                response_bytes: 768,
                response_decoded_bytes: 768,
                stderr_bytes: 0,
            })
        }
    }

    fn fake_response(request: &RemoteExportRequest, reply: FakeReply) -> RemoteSessionFactResponse {
        let RemoteExportRequestBody::SessionFacts(fact_request) = &request.request else {
            panic!("fake fact transport received another request kind")
        };
        let expected_source = request.expected_source.clone().unwrap();
        let wrong_source = matches!(&reply, FakeReply::WrongSourceSnapshot);
        let result = match reply {
            FakeReply::Snapshot {
                generation,
                watermark,
                records,
                has_more,
                token,
            } => RemoteExportResponseBody::FactSnapshot {
                page: FactSnapshotPage {
                    thread_id: fact_request.thread_id.clone(),
                    snapshot_id: FactSnapshotId::from_str("snapshot-one").unwrap(),
                    fact_generation: NonZeroU64::new(generation).unwrap(),
                    snapshot_watermark: watermark,
                    next_page_token: token
                        .map(|token| FactSnapshotPageToken::from_str(token).unwrap()),
                    activate_fact_cursor: (!has_more).then_some(RemoteFactCursor {
                        fact_generation: NonZeroU64::new(generation).unwrap(),
                        through_sequence: watermark,
                    }),
                    has_more,
                },
                payload: RemoteSessionFactPayload::Snapshot(RemoteFactSnapshotPayload {
                    fact_schema_version: REMOTE_SESSION_FACT_SCHEMA_VERSION,
                    records,
                }),
            },
            FakeReply::Delta {
                generation,
                watermark,
                changes,
                has_more,
                token,
            } => RemoteExportResponseBody::FactDelta {
                page: FactDeltaPage {
                    thread_id: fact_request.thread_id.clone(),
                    batch_id: RemoteFactBatchId::from_str("batch-one").unwrap(),
                    fact_generation: NonZeroU64::new(generation).unwrap(),
                    delta_watermark: watermark,
                    next_page_token: token
                        .map(|token| FactDeltaPageToken::from_str(token).unwrap()),
                    activate_fact_cursor: (!has_more).then_some(RemoteFactCursor {
                        fact_generation: NonZeroU64::new(generation).unwrap(),
                        through_sequence: watermark,
                    }),
                    has_more,
                },
                payload: RemoteSessionFactPayload::Delta(RemoteFactDeltaPayload {
                    fact_schema_version: REMOTE_SESSION_FACT_SCHEMA_VERSION,
                    changes,
                }),
            },
            FakeReply::CursorExpired => RemoteExportResponseBody::Failure(RemoteFailure {
                kind: RemoteFailureKind::FactCursorExpired,
                message: "fact cursor expired".to_owned(),
                retry_after_seconds: None,
            }),
            FakeReply::WrongSourceSnapshot => RemoteExportResponseBody::FactSnapshot {
                page: FactSnapshotPage {
                    thread_id: fact_request.thread_id.clone(),
                    snapshot_id: FactSnapshotId::from_str("snapshot-wrong").unwrap(),
                    fact_generation: NonZeroU64::new(12).unwrap(),
                    snapshot_watermark: 0,
                    next_page_token: None,
                    activate_fact_cursor: Some(RemoteFactCursor {
                        fact_generation: NonZeroU64::new(12).unwrap(),
                        through_sequence: 0,
                    }),
                    has_more: false,
                },
                payload: RemoteSessionFactPayload::Snapshot(RemoteFactSnapshotPayload {
                    fact_schema_version: REMOTE_SESSION_FACT_SCHEMA_VERSION,
                    records: Vec::new(),
                }),
            },
            FakeReply::TransportFailure => unreachable!(),
        };
        RemoteExportResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: BinaryVersion::from_str("0.4.0-test").unwrap(),
            source: if wrong_source {
                source(OTHER_SOURCE)
            } else {
                expected_source
            },
            redaction_profile: request.redaction_profile,
            revisions: current_revisions(),
            observed_at: at(31, 12),
            timing: RemoteTiming {
                remote_received_at: at(31, 12),
                remote_sent_at: at(31, 12),
            },
            result,
        }
    }

    fn delta_change(sequence: u64, event_id: &str, total: u64) -> RemoteUsageEventFactDeltaChange {
        RemoteUsageEventFactDeltaChange {
            sequence: NonZeroU64::new(sequence).unwrap(),
            record: remote_record(event_id, sequence, at(30, sequence as u32), total),
        }
    }

    fn run(
        fixture: &Fixture,
        transport: &mut FakeTransport,
    ) -> Result<RemoteFactSyncReport, RemoteFactSyncError> {
        run_with_bindings(fixture, transport, validated_digest_bindings())
    }

    fn run_with_bindings(
        fixture: &Fixture,
        transport: &mut FakeTransport,
        bindings: Vec<FactDigestBinding>,
    ) -> Result<RemoteFactSyncReport, RemoteFactSyncError> {
        sync_remote_thread_facts_bounded(
            &fixture.config_store,
            &fixture.selected,
            fixture.runtime.ownership(),
            fixture.runtime.source_history(),
            fixture.thread_id.clone(),
            bindings,
            35,
            transport,
            RemoteFactSyncLimits::default(),
        )
    }

    #[test]
    fn fact_exchange_budget_covers_expired_delta_plus_full_snapshot() {
        let limits = RemoteFactSyncLimits::default();
        assert_eq!(limits.max_pages.get(), 8);
        assert_eq!(limits.max_exchanges_per_run(), 17);
    }

    #[test]
    fn missing_local_source_metadata_is_explicitly_pre_transport() {
        let temp = TempDir::new().unwrap();
        let codex_home = temp.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let mut runtime =
            HistoryRuntime::new(temp.path().join("state/history-v1"), &codex_home, true).unwrap();
        runtime.ensure_v2_active().unwrap();
        let (config_store, _, _, selected) = paired_config(&temp);
        let mut transport = FakeTransport::new([]);

        let error = sync_remote_thread_facts_bounded(
            &config_store,
            &selected,
            runtime.ownership(),
            runtime.source_history(),
            THREAD.parse().unwrap(),
            validated_digest_bindings(),
            35,
            &mut transport,
            RemoteFactSyncLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, RemoteFactSyncError::PreTransportLocal(_)));
        assert!(transport.requests.is_empty());
    }

    #[test]
    fn excluding_one_oversized_replica_advances_to_the_next_candidate() {
        let fixture = Fixture::new();
        let newest = at(30, 0);
        let older = at(29, 0);
        let newest_thread = "thread-newest";
        let older_thread = "thread-older";
        install_remote_digests(
            &fixture.runtime,
            SOURCE,
            "dev",
            "ingest-gen-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[
                digest_record(SOURCE, newest_thread, newest, 'a', 100),
                digest_record(SOURCE, older_thread, older, 'b', 80),
            ],
        );
        install_remote_digests(
            &fixture.runtime,
            OTHER_SOURCE,
            "other",
            "ingest-gen-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &[
                digest_record(OTHER_SOURCE, newest_thread, newest, 'c', 101),
                digest_record(OTHER_SOURCE, older_thread, older, 'd', 81),
            ],
        );

        let plan = plan_next_replica_fact_sync(
            fixture.runtime.source_history(),
            &fixture.selected,
            fixture.runtime.source_identity().node_id(),
            RedactionProfile::Redacted,
            at(31, 13),
            &BTreeSet::new(),
            None,
        )
        .unwrap();
        let ReplicaFactSyncPlan::Work(plan) = plan else {
            panic!("the newest divergent replica should be selected")
        };
        assert_eq!(plan.thread_id().as_str(), newest_thread);

        let excluded = BTreeSet::from([plan.candidate_key()]);
        let next = plan_next_replica_fact_sync(
            fixture.runtime.source_history(),
            &fixture.selected,
            fixture.runtime.source_identity().node_id(),
            RedactionProfile::Redacted,
            at(31, 13),
            &excluded,
            None,
        )
        .unwrap();
        let ReplicaFactSyncPlan::Work(next) = next else {
            panic!("an excluded newest replica must not starve the next candidate")
        };
        assert_eq!(next.thread_id().as_str(), older_thread);
        assert_eq!(next.target(), PlannedReplicaFactTarget::SelectedRemote);
    }

    #[test]
    fn fair_resume_cursor_reaches_candidate_after_thirty_three_stable_failures() {
        let fixture = Fixture::new();
        let range_start = at(30, 0);
        let selected_records = (0..34)
            .map(|index| {
                digest_record(
                    SOURCE,
                    &format!("thread-{index:02}"),
                    range_start,
                    'a',
                    100 + index,
                )
            })
            .collect::<Vec<_>>();
        let other_records = (0..34)
            .map(|index| {
                digest_record(
                    OTHER_SOURCE,
                    &format!("thread-{index:02}"),
                    range_start,
                    'b',
                    200 + index,
                )
            })
            .collect::<Vec<_>>();
        install_remote_digests(
            &fixture.runtime,
            SOURCE,
            "dev",
            "ingest-gen-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &selected_records,
        );
        install_remote_digests(
            &fixture.runtime,
            OTHER_SOURCE,
            "other",
            "ingest-gen-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &other_records,
        );

        let mut resume_after = None;
        for expected in 0..34 {
            let plan = plan_next_replica_fact_sync(
                fixture.runtime.source_history(),
                &fixture.selected,
                fixture.runtime.source_identity().node_id(),
                RedactionProfile::Redacted,
                at(31, 13),
                &BTreeSet::new(),
                resume_after.as_ref(),
            )
            .unwrap();
            let ReplicaFactSyncPlan::Work(plan) = plan else {
                panic!("fair scan stopped before candidate {expected}")
            };
            assert_eq!(plan.thread_id().as_str(), format!("thread-{expected:02}"));
            resume_after = Some(plan.candidate_key());
        }
    }

    #[test]
    fn snapshot_pages_stay_invisible_until_one_atomic_activation() {
        let fixture = Fixture::new();
        let history = fixture.runtime.source_history().clone();
        let thread = fixture.thread_id.clone();
        let mut transport = FakeTransport::new([
            FakeReply::Snapshot {
                generation: 11,
                watermark: 2,
                records: vec![remote_record("event-0001", 1, at(29, 8), 11)],
                has_more: true,
                token: Some("snapshot-page-2"),
            },
            FakeReply::Snapshot {
                generation: 11,
                watermark: 2,
                records: vec![remote_record("event-0002", 2, at(30, 8), 17)],
                has_more: false,
                token: None,
            },
        ]);
        transport.on_exchange = Some(Box::new(move |exchange| {
            if exchange == 2 {
                assert!(
                    history
                        .load_active_fact_set(
                            &SOURCE.parse().unwrap(),
                            RedactionProfile::Redacted,
                            &thread,
                        )
                        .unwrap()
                        .is_none(),
                    "an intermediate page must not publish an active fact generation"
                );
            }
        }));

        let report = run(&fixture, &mut transport).unwrap();
        assert_eq!(report.exchanges, 2);
        assert_eq!(
            transport.decoded_limits,
            vec![MAX_DECODED_BYTES_PER_RUN, MAX_DECODED_BYTES_PER_RUN - 768]
        );
        assert_eq!(report.records_received, 2);
        assert!(report.activation.activated);
        assert_eq!(report.cursor, FactCursor::new(11, 2).unwrap());
        assert!(matches!(
            transport.requests[0].request,
            RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
                position: SessionFactsPosition::SnapshotStart,
                ..
            })
        ));
        let RemoteExportRequestBody::SessionFacts(second) = &transport.requests[1].request else {
            panic!("second request must be a fact continuation")
        };
        assert!(matches!(
            &second.position,
            SessionFactsPosition::SnapshotContinue {
                snapshot_watermark: 2,
                ..
            }
        ));

        let active = fixture.active().unwrap();
        assert_eq!(active.cursor, FactCursor::new(11, 2).unwrap());
        assert_eq!(active.records.len(), 2);
        assert_eq!(active.replica.source_id().as_str(), SOURCE);
        assert_eq!(active.replica.thread_id(), &fixture.thread_id);
        let UsageEventFactChange::Upsert(fact) = active.records[1].change() else {
            panic!("snapshot fact must be an upsert")
        };
        assert_eq!(fact.root_session_turn_id(), Some("root-turn-7"));
        assert_eq!(fact.model(), Some("gpt-5.6-sol"));
        assert_eq!(fact.metrics().estimated_cost_units, 17 * 13);
        assert_eq!(
            fact.metrics().api_long_context_extra_cost_units,
            Some(17 * 3)
        );
        assert_eq!(
            fact.metrics().api_equivalent_cost.maximum_pico_usd.value(),
            17 * 9
        );
    }

    #[test]
    fn decoded_response_budget_fails_before_any_fact_activation() {
        let fixture = Fixture::new();
        let mut transport = FakeTransport::new([FakeReply::Snapshot {
            generation: 11,
            watermark: 1,
            records: vec![remote_record("event-0001", 1, at(30, 8), 11)],
            has_more: false,
            token: None,
        }]);
        let limits = RemoteFactSyncLimits {
            max_decoded_bytes: 767,
            ..RemoteFactSyncLimits::default()
        };

        assert!(matches!(
            sync_remote_thread_facts_bounded(
                &fixture.config_store,
                &fixture.selected,
                fixture.runtime.ownership(),
                fixture.runtime.source_history(),
                fixture.thread_id.clone(),
                validated_digest_bindings(),
                35,
                &mut transport,
                limits,
            ),
            Err(RemoteFactSyncError::DecodedBudgetExceeded)
        ));
        assert_eq!(transport.decoded_limits, vec![767]);
        assert!(fixture.active().is_none());
    }

    #[test]
    fn fact_record_count_budget_is_checked_without_overflow() {
        assert!(ensure_record_capacity(MAX_COMPLETE_FACT_BATCH_RECORDS, 0).is_ok());
        assert!(matches!(
            ensure_record_capacity(MAX_COMPLETE_FACT_BATCH_RECORDS, 1),
            Err(RemoteFactSyncError::RecordBudgetExceeded)
        ));
        assert!(matches!(
            ensure_record_capacity(usize::MAX, 1),
            Err(RemoteFactSyncError::RecordBudgetExceeded)
        ));
    }

    #[test]
    fn active_cursor_drives_an_exact_delta_and_preserves_the_replica() {
        let fixture = Fixture::new();
        fixture.seed(1);
        let mut transport = FakeTransport::new([FakeReply::Delta {
            generation: 11,
            watermark: 2,
            changes: vec![delta_change(2, "event-0002", 23)],
            has_more: false,
            token: None,
        }]);

        run(&fixture, &mut transport).unwrap();
        let RemoteExportRequestBody::SessionFacts(request) = &transport.requests[0].request else {
            panic!("request must be session facts")
        };
        assert_eq!(
            request.position,
            SessionFactsPosition::DeltaStart {
                fact_cursor: RemoteFactCursor {
                    fact_generation: NonZeroU64::new(11).unwrap(),
                    through_sequence: 1,
                }
            }
        );
        let active = fixture.active().unwrap();
        assert_eq!(active.cursor, FactCursor::new(11, 2).unwrap());
        assert_eq!(active.records.len(), 2);
        assert!(
            active
                .facts()
                .iter()
                .all(|fact| fact.replica().source_id().as_str() == SOURCE)
        );
    }

    #[test]
    fn source_generation_change_forces_snapshot_even_when_fact_cursor_collides() {
        let fixture = Fixture::new();
        fixture.seed_with_binding(1, test_remote_binding(6));
        let snapshot_record = remote_record("event-new-generation", 1, at(30, 9), 41);
        let bindings =
            validated_digest_bindings_for_records(std::slice::from_ref(&snapshot_record));
        let mut transport = FakeTransport::new([FakeReply::Snapshot {
            generation: 11,
            watermark: 1,
            records: vec![snapshot_record],
            has_more: false,
            token: None,
        }]);

        run_with_bindings(&fixture, &mut transport, bindings).unwrap();
        assert!(matches!(
            transport.requests[0].request,
            RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
                position: SessionFactsPosition::SnapshotStart,
                ..
            })
        ));
        let active = fixture.active().unwrap();
        assert_eq!(
            active.remote_binding.as_ref(),
            Some(&test_remote_binding(7))
        );
        assert_eq!(active.cursor, FactCursor::new(11, 1).unwrap());
        assert_eq!(active.records.len(), 1);
        assert_eq!(
            active.records[0].event_id().as_str(),
            "event-new-generation"
        );
    }

    #[test]
    fn nth_page_transport_failure_leaves_active_facts_and_cursor_unchanged() {
        let fixture = Fixture::new();
        let before = fixture.seed(1);
        let mut transport = FakeTransport::new([
            FakeReply::Delta {
                generation: 11,
                watermark: 3,
                changes: vec![delta_change(2, "event-0002", 19)],
                has_more: true,
                token: Some("delta-page-2"),
            },
            FakeReply::TransportFailure,
        ]);

        assert!(matches!(
            run(&fixture, &mut transport),
            Err(RemoteFactSyncError::Transport(_))
        ));
        assert_eq!(fixture.active().unwrap(), before);
    }

    #[test]
    fn stale_config_after_the_network_response_cannot_publish_facts() {
        let fixture = Fixture::new();
        let before = fixture.seed(1);
        let store = fixture.config_store.clone();
        let mut transport = FakeTransport::new([FakeReply::Delta {
            generation: 11,
            watermark: 2,
            changes: vec![delta_change(2, "event-0002", 29)],
            has_more: false,
            token: None,
        }]);
        transport.on_exchange = Some(Box::new(move |_| {
            let current = store.load().unwrap();
            store
                .update(
                    current.config_revision(),
                    RemotesConfigMutation::edit_host(
                        "dev",
                        RemoteHostEdit {
                            ssh_host: Some("changed-alias".to_owned()),
                            ..RemoteHostEdit::default()
                        },
                    ),
                )
                .unwrap();
        }));

        assert!(matches!(
            run(&fixture, &mut transport),
            Err(RemoteFactSyncError::ConfigurationChanged { .. })
        ));
        assert_eq!(fixture.active().unwrap(), before);
    }

    #[test]
    fn blocked_real_fact_staging_does_not_block_config_changes_or_publish_late() {
        for change in ["disable", "remove", "edit", "global-off"] {
            let fixture = Fixture::new();
            let mut config = fixture.config_store.load().unwrap();
            config = fixture
                .config_store
                .update(
                    config.config_revision(),
                    RemotesConfigMutation::enable_host("dev"),
                )
                .unwrap();
            config = fixture
                .config_store
                .update(
                    config.config_revision(),
                    RemotesConfigMutation::set_auto_sync_enabled(true),
                )
                .unwrap();
            let selected =
                RemoteSyncHostSnapshot::capture_manual(&config, config.host("dev").unwrap())
                    .unwrap();
            let binding = build_binding(&selected).unwrap();
            let replica =
                SessionReplicaKey::new(SOURCE.parse().unwrap(), fixture.thread_id.clone());
            let batch = CompleteFactBatch {
                batch_id: FactBatchId::generate().unwrap(),
                kind: FactBatchKind::Snapshot,
                replica: replica.clone(),
                expected_active_version: None,
                remote_binding: Some(binding.history_binding.clone()),
                validated_digests: Vec::new(),
                activate_cursor: FactCursor::new(11, 1).unwrap(),
                completed_at: at(30, 10),
                changes: vec![
                    convert_record(&replica, remote_record("event-0001", 1, at(30, 10), 11))
                        .unwrap(),
                ],
            };
            let source_id: NodeId = SOURCE.parse().unwrap();
            let staging_guard = fixture
                .runtime
                .source_history()
                .acquire_fact_staging_lock_for_test(&source_id, RedactionProfile::Redacted)
                .unwrap();

            let worker_store = fixture.config_store.clone();
            let worker_ownership = fixture.runtime.ownership().clone();
            let worker_history = fixture.runtime.source_history().clone();
            let worker_selected = selected.clone();
            let worker_binding = binding.clone();
            let (stage_started_tx, stage_started_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let publication = stage_and_prevalidate_under_fresh_writer_with_hook(
                    &worker_ownership,
                    &worker_history,
                    &worker_binding,
                    &batch,
                    || stage_started_tx.send(()).unwrap(),
                )
                .map_err(RemoteFactSyncError::Local)?;
                let entered = Cell::new(false);
                match worker_store.with_current_host(
                    worker_selected.config_revision(),
                    worker_selected.host(),
                    || {
                        entered.set(true);
                        publish_under_fresh_writer(
                            &worker_ownership,
                            &worker_history,
                            &worker_binding,
                            &publication,
                        )
                    },
                ) {
                    Ok(report) => Ok(report),
                    Err(_) if !entered.get() => Err(RemoteFactSyncError::ConfigurationChanged {
                        host_id: worker_selected.host().id().to_owned(),
                    }),
                    Err(error) => Err(RemoteFactSyncError::Local(error)),
                }
            });
            stage_started_rx
                .recv_timeout(StdDuration::from_secs(1))
                .expect("the production fact stage should reach its real staging lock");

            let mutation_store = fixture.config_store.clone();
            let revision = selected.config_revision();
            let (changed_tx, changed_rx) = mpsc::channel();
            let mutator = thread::spawn(move || {
                let mutation = match change {
                    "disable" => RemotesConfigMutation::disable_host("dev"),
                    "remove" => RemotesConfigMutation::remove_host("dev"),
                    "edit" => RemotesConfigMutation::edit_host(
                        "dev",
                        RemoteHostEdit {
                            ssh_host: Some("changed-alias".to_owned()),
                            ..RemoteHostEdit::default()
                        },
                    ),
                    "global-off" => RemotesConfigMutation::set_auto_sync_enabled(false),
                    _ => unreachable!(),
                };
                changed_tx
                    .send(mutation_store.update(revision, mutation))
                    .unwrap();
            });
            let changed = changed_rx.recv_timeout(StdDuration::from_secs(1));
            if changed.is_err() {
                let _ = fs2::FileExt::unlock(&staging_guard);
                let _ = worker.join();
                let _ = mutator.join();
                panic!("{change} was blocked by real fact staging");
            }
            changed.unwrap().unwrap();
            mutator.join().unwrap();
            fs2::FileExt::unlock(&staging_guard).unwrap();

            assert!(matches!(
                worker.join().unwrap(),
                Err(RemoteFactSyncError::ConfigurationChanged { .. })
            ));
            assert!(
                fixture.active().is_none(),
                "{change} must reject the late manifest publication"
            );
        }
    }

    #[test]
    fn active_fact_cas_prevents_an_in_flight_delta_from_overwriting_a_newer_writer() {
        let fixture = Fixture::new();
        let before = fixture.seed(1);
        let ownership = fixture.runtime.ownership().clone();
        let history = fixture.runtime.source_history().clone();
        let thread = fixture.thread_id.clone();
        let expected = before.version.clone();
        let mut transport = FakeTransport::new([FakeReply::Delta {
            generation: 11,
            watermark: 2,
            changes: vec![delta_change(2, "event-0002", 29)],
            has_more: false,
            token: None,
        }]);
        transport.on_exchange = Some(Box::new(move |_| {
            let replica = SessionReplicaKey::new(SOURCE.parse().unwrap(), thread.clone());
            let batch = CompleteFactBatch {
                batch_id: FactBatchId::generate().unwrap(),
                kind: FactBatchKind::Delta,
                replica: replica.clone(),
                expected_active_version: Some(expected.clone()),
                remote_binding: Some(test_remote_binding(7)),
                validated_digests: expected.validated_digests().to_vec(),
                activate_cursor: FactCursor::new(11, 2).unwrap(),
                completed_at: at(30, 10),
                changes: vec![
                    convert_record(
                        &replica,
                        remote_record("event-concurrent", 2, at(30, 10), 37),
                    )
                    .unwrap(),
                ],
            };
            let lease = ownership.acquire_writer_lease().unwrap();
            let manifest = match ownership.load_manifest().unwrap() {
                OwnershipManifestStatus::Initialized(manifest) => manifest,
                OwnershipManifestStatus::Uninitialized => panic!("ownership must be active"),
            };
            let authority = ownership.authorize_v2_write(&lease, &manifest).unwrap();
            history
                .writer(&authority)
                .unwrap()
                .stage_and_activate_complete_fact_batch(
                    &SOURCE.parse().unwrap(),
                    RedactionProfile::Redacted,
                    &batch,
                )
                .unwrap();
        }));

        assert!(matches!(
            run(&fixture, &mut transport),
            Err(RemoteFactSyncError::Local(ref error))
                if error.kind() == io::ErrorKind::WouldBlock
        ));
        let active = fixture.active().unwrap();
        assert_eq!(active.cursor, FactCursor::new(11, 2).unwrap());
        assert!(
            active
                .records
                .iter()
                .any(|record| record.event_id().as_str() == "event-concurrent")
        );
    }

    #[test]
    fn exact_cursor_expiry_discards_delta_pages_and_restarts_a_snapshot() {
        let fixture = Fixture::new();
        fixture.seed(1);
        let snapshot_record = remote_record("event-new", 5, at(30, 9), 31);
        let bindings =
            validated_digest_bindings_for_records(std::slice::from_ref(&snapshot_record));
        let mut transport = FakeTransport::new([
            FakeReply::CursorExpired,
            FakeReply::Snapshot {
                generation: 12,
                watermark: 5,
                records: vec![snapshot_record],
                has_more: false,
                token: None,
            },
        ]);

        let report = run_with_bindings(&fixture, &mut transport, bindings).unwrap();
        assert!(report.restarted_from_snapshot);
        assert!(matches!(
            transport.requests[0].request,
            RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
                position: SessionFactsPosition::DeltaStart { .. },
                ..
            })
        ));
        assert!(matches!(
            transport.requests[1].request,
            RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
                position: SessionFactsPosition::SnapshotStart,
                ..
            })
        ));
        let active = fixture.active().unwrap();
        assert_eq!(active.cursor, FactCursor::new(12, 5).unwrap());
        assert_eq!(active.records.len(), 1);
        assert_eq!(active.records[0].event_id().as_str(), "event-new");
    }

    #[test]
    fn cursor_expiry_does_not_consume_the_eight_page_snapshot_budget() {
        let fixture = Fixture::new();
        fixture.seed(1);
        let tokens = [
            "snapshot-page-2",
            "snapshot-page-3",
            "snapshot-page-4",
            "snapshot-page-5",
            "snapshot-page-6",
            "snapshot-page-7",
            "snapshot-page-8",
        ];
        let snapshot_records = (1..=8_u64)
            .map(|index| {
                remote_record(
                    &format!("event-{index:04}"),
                    index,
                    at(30, u32::try_from(index).unwrap()),
                    index * 10,
                )
            })
            .collect::<Vec<_>>();
        let bindings = validated_digest_bindings_for_records(&snapshot_records);
        let mut replies = vec![FakeReply::CursorExpired];
        for (offset, record) in snapshot_records.into_iter().enumerate() {
            let index = u64::try_from(offset).unwrap() + 1;
            let final_page = index == 8;
            replies.push(FakeReply::Snapshot {
                generation: 12,
                watermark: 8,
                records: vec![record],
                has_more: !final_page,
                token: if final_page {
                    None
                } else {
                    Some(tokens[usize::try_from(index - 1).unwrap()])
                },
            });
        }
        let mut transport = FakeTransport::new(replies);

        let report = run_with_bindings(&fixture, &mut transport, bindings).unwrap();
        assert_eq!(report.exchanges, 9);
        assert_eq!(report.records_received, 8);
        assert!(report.restarted_from_snapshot);
        let active = fixture.active().unwrap();
        assert_eq!(active.cursor, FactCursor::new(12, 8).unwrap());
        assert_eq!(active.records.len(), 8);
    }

    #[test]
    fn wrong_source_is_rejected_without_creating_an_active_fact_set() {
        let fixture = Fixture::new();
        let mut transport = FakeTransport::new([FakeReply::WrongSourceSnapshot]);

        assert!(matches!(
            run(&fixture, &mut transport),
            Err(RemoteFactSyncError::Protocol(_))
                | Err(RemoteFactSyncError::UnboundResponseEnvelope)
        ));
        assert!(fixture.active().is_none());
    }
}
