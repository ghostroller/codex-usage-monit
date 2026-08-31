//! Source-local normalization performed before usage records enter a remote
//! delta journal.
//!
//! Legacy history identifies projects with a machine-local FNV digest. That
//! value is neither source-scoped nor suitable for the remote wire format.
//! This module replaces it with the private HMAC identity defined by
//! [`ObservedProjectKey`]. It intentionally refuses to guess when a rollout's
//! project path cannot be resolved.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Days, NaiveDate, TimeZone, Utc};
use sha2::{Digest, Sha256};

use crate::api_cost::ApiCostAccumulator;
use crate::attribution::{estimate_call_weight, is_spark_model};
use crate::domain::{
    AgentInteraction, ApiCostAmount, TaskRecord, TokenUsage, TurnRecord, UsageCall,
};
use crate::git_repository::{GitProjectEvidence, GitProjectEvidenceResolver};
use crate::history::{
    HistoryObservation, LocalHalfHourBucket, LocalProjectUsageGroup, LocalUsageGroup,
};
use crate::remote_agent::current_revisions;
use crate::remote_protocol::{
    RemoteApiCostAmount, RemoteGitRepositoryEvidence, RemoteLiveSnapshot, RemoteLiveTask,
    RemoteLiveTurn, RemoteModelUsageGroup, RemoteProjectDescriptor, RemoteProjectUsageGroup,
    RemoteSessionDigest, RemoteSessionDigestFingerprint, RemoteSessionUsageMetrics,
    RemoteTokenUsage, RemoteU128, RemoteUsageBucket, RemoteUsageEventFact,
};
use crate::source_history::{
    FactDigestBinding, RedactionProfile, SessionDigestFingerprint, SessionUsageMetrics,
    SourceSessionDigest, UsageEventFact, UsageEventId,
};
use crate::source_identity::SourceIdentity;
use crate::source_model::{ObservedProjectKey, ProjectDisplayLabel, SessionReplicaKey, ThreadId};

const SESSION_DIGEST_DOMAIN: &[u8] = b"codex-usage-monit/session-digest/v1\0";
const PROJECT_BREAKDOWN_DIGEST_DOMAIN: &[u8] = b"codex-usage-monit/session-project-breakdown/v1\0";
const SESSION_DIGEST_PREFIX: &str = "session-digest-sha256-v1-";
const FALLBACK_PROJECT_LABEL: &str = "project";
const INVALID_THREAD_REASON: &str = "invalid_thread_id";
const MISSING_EVENT_REASON: &str = "usage_event_identity_missing";
const FALLBACK_EVENT_REASON: &str = "usage_event_identity_fallback";
const CONFLICTING_EVENT_REASON: &str = "usage_event_identity_conflict";
const INCOMPLETE_SCAN_REASON: &str = "rollout_scan_incomplete";
const OPEN_RANGE_REASON: &str = "session_range_open";
const COVERAGE_GAP_REASON: &str = "session_range_coverage_gap";
const EVENT_OUTSIDE_COVERAGE_REASON: &str = "digest_event_outside_coverage";
const PROJECT_ATTRIBUTION_REASON: &str = "project_attribution_unavailable";
const MODEL_FALLBACK_REASON: &str = "unpriced_model_rate_fallback";
const TOKEN_BREAKDOWN_REASON: &str = "token_breakdown_missing";
const LONG_CONTEXT_UNKNOWN_REASON: &str = "long_context_usage_unknown";
const REQUEST_USAGE_INEXACT_REASON: &str = "request_usage_not_exact";
const SUBAGENT_ATTRIBUTION_REASON: &str = "subagent_turn_attribution_unavailable";
const MAX_EXPORTED_PARTIAL_REASONS: usize = 128;
const SESSION_DIGEST_RETENTION_DAYS: u64 = 35;
const MAX_LOCAL_SESSION_DIGESTS: usize = 65_536;

/// Result of replacing legacy project identities in one observation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceProjectNormalizationReport {
    /// Distinct source-scoped project keys referenced by the observation.
    pub observed_project_keys: BTreeSet<ObservedProjectKey>,
    /// Project-bearing groups that were assigned a source-scoped key.
    pub groups_rekeyed: usize,
    /// Groups whose old local project ID was deliberately removed because no
    /// unambiguous canonical path could be established.
    pub groups_unresolved: usize,
}

/// Fully normalized aggregate facts ready to be diffed into the durable
/// exporter journal. Sequence/revision assignment belongs to that journal and
/// is deliberately absent here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaterializedSourceObservation {
    pub project_descriptors: Vec<RemoteProjectDescriptor>,
    pub buckets: Vec<RemoteUsageBucket>,
    pub session_digests: Vec<RemoteSessionDigest>,
    pub project_normalization: SourceProjectNormalizationReport,
    pub invalid_thread_groups: usize,
    pub missing_event_identities: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaterializedLiveSnapshot {
    pub snapshot: RemoteLiveSnapshot,
    pub project_descriptors: Vec<RemoteProjectDescriptor>,
    pub dropped_tasks: usize,
    pub dropped_turns: usize,
}

/// Complete, content-free event inventory for one emitting thread. The
/// caller must publish it only after a complete fixed-domain rollout scan;
/// missing project attribution is an error because every persisted event fact
/// must remain queryable through its source-owned project key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaterializedSessionFacts {
    pub facts: Vec<RemoteUsageEventFact>,
    pub missing_event_identities: usize,
    pub conflicting_event_identities: usize,
}

/// Bounded, content-free digest sidecar produced while rollout calls are still
/// available. Project keys are finalized only after the local source identity
/// has normalized the observation; no call list, path, title, or message is
/// retained in this value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalSessionDigestEvidence {
    observed_at: DateTime<Utc>,
    scan_complete: bool,
    digests: Vec<RemoteSessionDigest>,
}

impl LocalSessionDigestEvidence {
    pub fn empty(observed_at: DateTime<Utc>) -> Self {
        Self {
            observed_at,
            scan_complete: false,
            digests: Vec::new(),
        }
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    pub fn scan_complete(&self) -> bool {
        self.scan_complete
    }

    pub fn digest_count(&self) -> usize {
        self.digests.len()
    }
}

/// Collapses retained usage calls into one digest per physical thread/UTC day
/// before the full rollout dataset is dropped. The input buckets may still
/// contain legacy local project IDs; [`finalize_local_session_digests`] replaces
/// that one attribution component after source-scoped normalization.
pub fn materialize_local_session_digest_evidence(
    calls: &[UsageCall],
    buckets: &[LocalHalfHourBucket],
    observed_at: DateTime<Utc>,
    scan_complete: bool,
) -> io::Result<LocalSessionDigestEvidence> {
    let (digests, _, _) = materialize_session_digests(calls, buckets, observed_at, scan_complete)?;
    if digests.len() > MAX_LOCAL_SESSION_DIGESTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local session digest evidence exceeds its bounded record count",
        ));
    }
    Ok(LocalSessionDigestEvidence {
        observed_at,
        scan_complete,
        digests,
    })
}

/// Binds a content-free digest sidecar to this source and to the project keys
/// in the already-normalized observation.
pub fn finalize_local_session_digests(
    identity: &SourceIdentity,
    evidence: &LocalSessionDigestEvidence,
    normalized_observation: &HistoryObservation,
) -> io::Result<Vec<SourceSessionDigest>> {
    if evidence.observed_at != normalized_observation.observed_at {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local session digest evidence does not match its observation time",
        ));
    }
    let mut digests = evidence.digests.clone();
    replace_digest_project_attribution(&mut digests, &normalized_observation.half_hour_buckets)?;
    digests
        .into_iter()
        .map(|digest| local_source_session_digest(identity, digest))
        .collect()
}

/// Clones one aggregate observation and applies the source-scoped project-key
/// normalization used by durable aggregate publication.
///
/// Fact exporters must derive both their digest proof and their per-event
/// facts from this same normalized clone. Rebuilding either side from the raw
/// observation can bind the digest to a legacy machine-local project ID while
/// the fact carries an `ObservedProjectKey`.
pub(crate) fn source_normalized_observation(
    identity: &SourceIdentity,
    tasks: &[TaskRecord],
    observation: &HistoryObservation,
) -> HistoryObservation {
    let mut normalized = observation.clone();
    normalize_observation_project_keys(identity, tasks, &mut normalized);
    normalized
}

/// Converts one local collection result into path-free, source-safe aggregate
/// protocol facts.
///
/// Account quota data is intentionally ignored: only the center is allowed to
/// collect server quota/reset state. Session digests use stable UTC-day ranges
/// so retention cannot silently change their key as a rolling window moves.
pub fn materialize_source_observation(
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    tasks: &[TaskRecord],
    calls: &[UsageCall],
    observation: HistoryObservation,
    scan_complete: bool,
) -> io::Result<MaterializedSourceObservation> {
    let mut git = GitProjectEvidenceResolver::default();
    git.begin_collection();
    materialize_source_observation_with_git_resolver(
        identity,
        redaction_profile,
        tasks,
        calls,
        observation,
        scan_complete,
        &mut git,
    )
}

pub(crate) fn materialize_source_observation_with_git_resolver(
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    tasks: &[TaskRecord],
    calls: &[UsageCall],
    mut observation: HistoryObservation,
    scan_complete: bool,
    git: &mut GitProjectEvidenceResolver,
) -> io::Result<MaterializedSourceObservation> {
    let normalization = normalize_observation_project_keys(identity, tasks, &mut observation);
    let api_by_bucket = api_costs_by_bucket(calls, observation.observed_at);
    let mut descriptors = BTreeMap::<ObservedProjectKey, ProjectDisplayLabel>::new();
    let mut buckets = Vec::with_capacity(observation.half_hour_buckets.len());
    let mut invalid_thread_groups = 0_usize;

    observation
        .half_hour_buckets
        .sort_by_key(|bucket| bucket.starts_at);
    for bucket in &observation.half_hour_buckets {
        buckets.push(materialize_bucket(
            bucket,
            redaction_profile,
            api_by_bucket.get(&bucket.starts_at),
            &mut descriptors,
            &mut invalid_thread_groups,
        )?);
    }

    let (session_digests, missing_event_identities, invalid_digest_threads) =
        materialize_session_digests(
            calls,
            &observation.half_hour_buckets,
            observation.observed_at,
            scan_complete,
        )?;
    invalid_thread_groups = invalid_thread_groups.saturating_add(invalid_digest_threads);
    let project_descriptors = project_descriptors_with_git_evidence_using(
        descriptors,
        resolved_descriptor_paths(tasks, &observation),
        git,
    );

    Ok(MaterializedSourceObservation {
        project_descriptors,
        buckets,
        session_digests,
        project_normalization: normalization,
        invalid_thread_groups,
        missing_event_identities,
    })
}

/// Materializes the exact additive calls belonging to `requested_thread`.
/// Lineage/project attribution is reused from the same history builder as the
/// aggregate exporter, then stripped down to opaque identifiers and metrics.
#[allow(clippy::too_many_arguments)]
pub fn materialize_session_facts(
    identity: &SourceIdentity,
    requested_thread: &ThreadId,
    retention_days: u16,
    observed_at: DateTime<Utc>,
    tasks: &[TaskRecord],
    turns: &[TurnRecord],
    interactions: &[AgentInteraction],
    calls: &[UsageCall],
    collection_partial_reasons: &[String],
) -> io::Result<MaterializedSessionFacts> {
    let observation =
        HistoryObservation::from_sources_with_tasks_turns_and_interactions_and_coverage(
            observed_at,
            calls,
            tasks,
            turns,
            interactions,
            &[],
            collection_partial_reasons,
            None,
        );
    let observation = source_normalized_observation(identity, tasks, &observation);
    materialize_session_facts_from_normalized_observation(
        requested_thread,
        retention_days,
        observed_at,
        &observation,
        calls,
        collection_partial_reasons,
    )
}

/// Caller-defined envelope for building a complete fact inventory. The byte
/// measure is deliberately supplied by the consumer so construction can use
/// the exact frozen/wire representation without coupling source normalization
/// to a transport format.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SessionFactMaterializationLimits {
    pub maximum_records: usize,
    pub initial_serialized_bytes: usize,
    pub maximum_record_serialized_bytes: usize,
    pub maximum_serialized_bytes: usize,
}

impl SessionFactMaterializationLimits {
    fn unbounded() -> Self {
        Self {
            maximum_records: usize::MAX,
            initial_serialized_bytes: 0,
            maximum_record_serialized_bytes: usize::MAX,
            maximum_serialized_bytes: usize::MAX,
        }
    }

    fn validate(self) -> io::Result<()> {
        if self.maximum_records == 0
            || self.maximum_record_serialized_bytes == 0
            || self.maximum_serialized_bytes == 0
            || self.initial_serialized_bytes > self.maximum_serialized_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session fact materialization limits are invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct SessionFactInventoryLimitExceeded;

impl std::fmt::Display for SessionFactInventoryLimitExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("session fact inventory exceeds its complete-batch limit")
    }
}

impl std::error::Error for SessionFactInventoryLimitExceeded {}

pub(crate) fn is_session_fact_inventory_limit_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<SessionFactInventoryLimitExceeded>())
}

fn session_fact_inventory_limit_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        SessionFactInventoryLimitExceeded,
    )
}

/// Materializes one physical thread from the exact source-normalized
/// observation used to validate its digest proof.
pub(crate) fn materialize_session_facts_from_normalized_observation(
    requested_thread: &ThreadId,
    retention_days: u16,
    observed_at: DateTime<Utc>,
    normalized_observation: &HistoryObservation,
    calls: &[UsageCall],
    collection_partial_reasons: &[String],
) -> io::Result<MaterializedSessionFacts> {
    materialize_session_facts_from_normalized_observation_bounded(
        requested_thread,
        retention_days,
        observed_at,
        normalized_observation,
        calls,
        collection_partial_reasons,
        SessionFactMaterializationLimits::unbounded(),
        |_| Ok(0),
    )
}

/// Bounded variant used by complete local and remote fact snapshots. Unique
/// candidates are measured as they enter the map; conflicting identities
/// replace their previous measured size, so the running total always matches
/// the final deterministic winner set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_session_facts_from_normalized_observation_bounded<F>(
    requested_thread: &ThreadId,
    retention_days: u16,
    observed_at: DateTime<Utc>,
    normalized_observation: &HistoryObservation,
    calls: &[UsageCall],
    collection_partial_reasons: &[String],
    limits: SessionFactMaterializationLimits,
    mut serialized_fact_bytes: F,
) -> io::Result<MaterializedSessionFacts>
where
    F: FnMut(&RemoteUsageEventFact) -> io::Result<usize>,
{
    limits.validate()?;
    if normalized_observation.observed_at != observed_at {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "normalized session-fact observation does not match its observation time",
        ));
    }
    let retention_start = observed_at
        .checked_sub_days(Days::new(u64::from(retention_days)))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);

    let mut groups =
        BTreeMap::<(DateTime<Utc>, String, Option<String>), &LocalProjectUsageGroup>::new();
    for bucket in &normalized_observation.half_hour_buckets {
        for group in bucket
            .project_groups
            .iter()
            .filter(|group| group.call_count > 0 && group.thread_id == requested_thread.as_str())
        {
            let key = (
                bucket.starts_at,
                group.thread_id.clone(),
                normalized_lookup_text(group.turn_id.as_deref()),
            );
            if groups.insert(key, group).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session fact attribution is ambiguous for an emitting turn",
                ));
            }
        }
    }

    #[derive(Clone)]
    struct Candidate {
        semantic_hash: [u8; 32],
        fact: RemoteUsageEventFact,
        serialized_bytes: usize,
    }

    let revisions = current_revisions();
    let mut candidates = BTreeMap::<UsageEventId, Candidate>::new();
    let mut inventory_serialized_bytes = limits.initial_serialized_bytes;
    let mut missing_event_identities = 0_usize;
    let mut conflicting_event_identities = 0_usize;
    for call in calls.iter().filter(|call| {
        call.thread_id == requested_thread.as_str()
            && call.timestamp > retention_start
            && call.timestamp <= observed_at
    }) {
        let starts_at = bucket_start(call.timestamp).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "session fact timestamp cannot be assigned to a local bucket",
            )
        })?;
        let lookup_turn = normalized_lookup_text(call.turn_id.as_deref());
        let group = groups
            .get(&(starts_at, call.thread_id.clone(), lookup_turn.clone()))
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session fact has no unambiguous project/lineage attribution",
                )
            })?;
        let observed_project_key = group
            .project_id
            .as_deref()
            .and_then(|value| ObservedProjectKey::from_str(value).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session fact has no source-scoped observed project key",
                )
            })?;

        let semantic_hash = digest_event_semantic_hash(call);
        let parsed_event_id = call
            .usage_event_id
            .as_deref()
            .and_then(|value| UsageEventId::from_str(value).ok());
        let event_identity_missing = parsed_event_id.is_none();
        let (event_id, exact_event_identity) = match parsed_event_id {
            Some(event_id) => (event_id, call.usage_event_identity_exact),
            None => {
                missing_event_identities = missing_event_identities.saturating_add(1);
                (
                    UsageEventId::from_str(&format!(
                        "usage-derived-sha256-v1-{}",
                        lower_hex(&semantic_hash)
                    ))
                    .expect("derived event IDs satisfy the bounded opaque grammar"),
                    false,
                )
            }
        };

        let emitting_turn_id = strict_optional_protocol_text(
            call.turn_id.as_deref(),
            256,
            "session fact emitting turn ID",
        )?;
        let parent_thread_id = strict_optional_thread_id(
            group.parent_thread_id.as_deref(),
            "session fact parent thread ID",
        )?;
        let project_session_thread_id = strict_optional_thread_id(
            group.session_thread_id.as_deref(),
            "session fact project session thread ID",
        )?;
        let root_session_thread_id = project_session_thread_id
            .clone()
            .unwrap_or_else(|| requested_thread.clone());
        let root_session_turn_id = strict_optional_protocol_text(
            group.session_turn_id.as_deref(),
            256,
            "session fact root turn ID",
        )?;
        let model =
            strict_optional_protocol_text(call.model.as_deref(), 256, "session fact model")?;
        let service_tier = strict_optional_protocol_text(
            call.service_tier.as_deref(),
            64,
            "session fact service tier",
        )?;

        let mut partial_reasons = normalized_partial_reasons(collection_partial_reasons);
        if !exact_event_identity {
            partial_reasons.insert(FALLBACK_EVENT_REASON.to_owned());
        }
        if event_identity_missing {
            partial_reasons.insert(MISSING_EVENT_REASON.to_owned());
        }
        if !call.request_usage_exact {
            partial_reasons.insert(REQUEST_USAGE_INEXACT_REASON.to_owned());
        }
        if group.session_turn_id.is_none()
            && parent_thread_id.is_some()
            && root_session_thread_id != *requested_thread
        {
            partial_reasons.insert(SUBAGENT_ATTRIBUTION_REASON.to_owned());
        }

        let mut api_cost = ApiCostAccumulator::default();
        api_cost.add_call(call);
        partial_reasons.extend(api_cost.summary().partial_reasons);
        let (token_usage, estimated_cost_units, api_long_context_extra_cost_units) =
            if is_spark_model(call.model.as_deref()) {
                (TokenUsage::default(), 0, 0)
            } else {
                let weight = estimate_call_weight(call);
                if weight.used_model_fallback {
                    partial_reasons.insert(MODEL_FALLBACK_REASON.to_owned());
                }
                if weight.used_token_breakdown_fallback {
                    partial_reasons.insert(TOKEN_BREAKDOWN_REASON.to_owned());
                }
                if weight.used_long_context_detection_fallback {
                    partial_reasons.insert(LONG_CONTEXT_UNKNOWN_REASON.to_owned());
                }
                (
                    call.tokens,
                    weight.units,
                    weight.api_long_context_extra_units,
                )
            };
        let fact = RemoteUsageEventFact {
            event_id: event_id.clone(),
            occurred_at: call.timestamp,
            observed_project_key,
            emitting_thread_id: requested_thread.clone(),
            emitting_turn_id,
            parent_thread_id,
            project_session_thread_id,
            root_session_thread_id,
            root_session_turn_id,
            model,
            service_tier,
            digest_token_usage: remote_token_usage(call.tokens)?,
            request_usage_exact: call.request_usage_exact,
            exact_event_identity,
            metrics: RemoteSessionUsageMetrics {
                token_usage: remote_token_usage(token_usage)?,
                estimated_cost_units: RemoteU128::new(estimated_cost_units),
                api_long_context_extra_cost_units: Some(RemoteU128::new(
                    api_long_context_extra_cost_units,
                )),
                api_equivalent_cost: remote_api_cost(api_cost.amount()),
                call_count: 1,
                metric_revision: revisions.metric,
                estimator_revision: revisions.estimator,
                project_breakdown_revision: revisions.project_breakdown,
                api_pricing_catalog_revision: revisions.api_pricing_catalog,
                partial_reasons: partial_reasons
                    .into_iter()
                    .take(MAX_EXPORTED_PARTIAL_REASONS)
                    .collect(),
            },
        };

        match candidates.get_mut(&event_id) {
            Some(existing) if existing.semantic_hash == semantic_hash => {}
            Some(existing) => {
                conflicting_event_identities = conflicting_event_identities.saturating_add(1);
                let mut winner = if semantic_hash < existing.semantic_hash {
                    Candidate {
                        semantic_hash,
                        fact,
                        serialized_bytes: 0,
                    }
                } else {
                    existing.clone()
                };
                winner.fact.exact_event_identity = false;
                let reasons = &mut winner.fact.metrics.partial_reasons;
                if !reasons
                    .iter()
                    .any(|reason| reason == CONFLICTING_EVENT_REASON)
                {
                    reasons.push(CONFLICTING_EVENT_REASON.to_owned());
                    reasons.sort_unstable();
                    reasons.truncate(MAX_EXPORTED_PARTIAL_REASONS);
                }
                winner.serialized_bytes = serialized_fact_bytes(&winner.fact)?;
                if winner.serialized_bytes > limits.maximum_record_serialized_bytes {
                    return Err(session_fact_inventory_limit_error());
                }
                inventory_serialized_bytes = inventory_serialized_bytes
                    .checked_sub(existing.serialized_bytes)
                    .and_then(|total| total.checked_add(winner.serialized_bytes))
                    .filter(|total| *total <= limits.maximum_serialized_bytes)
                    .ok_or_else(session_fact_inventory_limit_error)?;
                *existing = winner;
            }
            None => {
                if candidates.len() >= limits.maximum_records {
                    return Err(session_fact_inventory_limit_error());
                }
                let serialized_bytes = serialized_fact_bytes(&fact)?;
                if serialized_bytes > limits.maximum_record_serialized_bytes {
                    return Err(session_fact_inventory_limit_error());
                }
                inventory_serialized_bytes = inventory_serialized_bytes
                    .checked_add(serialized_bytes)
                    .filter(|total| *total <= limits.maximum_serialized_bytes)
                    .ok_or_else(session_fact_inventory_limit_error)?;
                candidates.insert(
                    event_id,
                    Candidate {
                        semantic_hash,
                        fact,
                        serialized_bytes,
                    },
                );
            }
        }
    }

    Ok(MaterializedSessionFacts {
        facts: candidates
            .into_values()
            .map(|candidate| candidate.fact)
            .collect(),
        missing_event_identities,
        conflicting_event_identities,
    })
}

/// Builds a path-free live replacement snapshot. Invalid row identities or
/// future/reversed timestamps are omitted and counted; malformed token totals
/// fail the whole snapshot so usage is never silently rewritten.
pub fn materialize_live_snapshot(
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    tasks: &[TaskRecord],
    turns: &[TurnRecord],
    captured_at: DateTime<Utc>,
) -> io::Result<MaterializedLiveSnapshot> {
    let mut git = GitProjectEvidenceResolver::default();
    git.begin_collection();
    materialize_live_snapshot_with_git_resolver(
        identity,
        redaction_profile,
        tasks,
        turns,
        captured_at,
        &mut git,
    )
}

pub(crate) fn materialize_live_snapshot_with_git_resolver(
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    tasks: &[TaskRecord],
    turns: &[TurnRecord],
    captured_at: DateTime<Utc>,
    git: &mut GitProjectEvidenceResolver,
) -> io::Result<MaterializedLiveSnapshot> {
    let mut descriptors = BTreeMap::<ObservedProjectKey, ProjectDisplayLabel>::new();
    let mut descriptor_paths = BTreeMap::<ObservedProjectKey, ResolvedProjectPath>::new();
    let mut live_tasks = BTreeMap::<ThreadId, RemoteLiveTask>::new();
    let mut dropped_tasks = 0_usize;
    for task in tasks {
        let Ok(thread_id) = ThreadId::from_str(&task.thread_id) else {
            dropped_tasks = dropped_tasks.saturating_add(1);
            continue;
        };
        let updated_at = task.updated_at.or(task.created_at).unwrap_or(captured_at);
        if updated_at > captured_at || task.created_at.is_some_and(|created| created > updated_at) {
            dropped_tasks = dropped_tasks.saturating_add(1);
            continue;
        }
        let resolved_project_path = task.cwd.as_ref().and_then(|path| {
            let ResolvedProjectPath::Path(path) = resolve_canonical_project_path(path) else {
                return None;
            };
            Some(path)
        });
        let observed_project_key = resolved_project_path
            .as_ref()
            .and_then(|path| ObservedProjectKey::from_canonical_path(identity, path).ok());
        if let Some(key) = observed_project_key.as_ref() {
            let path_label = task
                .cwd
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|label| label.to_string_lossy().into_owned());
            let label = sanitized_project_label(path_label.as_deref());
            descriptors
                .entry(key.clone())
                .and_modify(|existing| {
                    if label.as_str() < existing.as_str() {
                        *existing = label.clone();
                    }
                })
                .or_insert(label);
            if let Some(path) = resolved_project_path.as_ref() {
                merge_descriptor_path(&mut descriptor_paths, key, path.clone());
            }
        }
        let row = RemoteLiveTask {
            thread_id: thread_id.clone(),
            parent_thread_id: parse_optional_thread_id(task.parent_thread_id.as_deref()),
            observed_project_key,
            title_preview: (redaction_profile == RedactionProfile::PreviewEnabled)
                .then(|| sanitized_preview(Some(&task.title)))
                .flatten(),
            created_at: task.created_at,
            updated_at,
            status: task.status,
            token_usage: remote_token_usage(task.token_usage)?,
            turn_count: u32::try_from(task.turn_count).unwrap_or(u32::MAX),
        };
        if live_tasks.insert(thread_id, row).is_some() {
            dropped_tasks = dropped_tasks.saturating_add(1);
        }
    }

    let mut live_turns = BTreeMap::<(ThreadId, String), RemoteLiveTurn>::new();
    let mut dropped_turns = 0_usize;
    for turn in turns {
        let Ok(thread_id) = ThreadId::from_str(&turn.thread_id) else {
            dropped_turns = dropped_turns.saturating_add(1);
            continue;
        };
        if !live_tasks.contains_key(&thread_id) {
            dropped_turns = dropped_turns.saturating_add(1);
            continue;
        }
        let Some(turn_id) = sanitized_protocol_text_with_limit(Some(&turn.turn_id), 256) else {
            dropped_turns = dropped_turns.saturating_add(1);
            continue;
        };
        if turn.started_at.is_some_and(|started| started > captured_at)
            || turn
                .completed_at
                .is_some_and(|completed| completed > captured_at)
            || matches!((turn.started_at, turn.completed_at), (Some(started), Some(completed)) if completed < started)
        {
            dropped_turns = dropped_turns.saturating_add(1);
            continue;
        }
        let row = RemoteLiveTurn {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            model: sanitized_protocol_text_with_limit(turn.model.as_deref(), 256),
            reasoning_effort: sanitized_protocol_text_with_limit(
                turn.reasoning_effort.as_deref(),
                64,
            ),
            service_tier: sanitized_protocol_text_with_limit(turn.service_tier.as_deref(), 64),
            message_preview: (redaction_profile == RedactionProfile::PreviewEnabled)
                .then(|| sanitized_preview(turn.message_preview.as_deref()))
                .flatten(),
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            status: turn.status,
            token_usage: remote_token_usage(turn.token_usage)?,
        };
        if live_turns.insert((thread_id, turn_id), row).is_some() {
            dropped_turns = dropped_turns.saturating_add(1);
        }
    }

    Ok(MaterializedLiveSnapshot {
        snapshot: RemoteLiveSnapshot {
            captured_at,
            tasks: live_tasks.into_values().collect(),
            turns: live_turns.into_values().collect(),
        },
        project_descriptors: project_descriptors_with_git_evidence_using(
            descriptors,
            descriptor_paths,
            git,
        ),
        dropped_tasks,
        dropped_turns,
    })
}

/// Replaces every legacy bucket project ID with a source-scoped opaque key.
///
/// A group's own task path wins. A descendant without a path may inherit the
/// nearest unambiguous parent path. Conflicting parent claims, cycles,
/// relative paths, missing paths, and failed canonicalization all resolve to
/// no project key. In particular, this function never leaves the old
/// enumerable `project-*` hash in a would-be remote record.
pub fn normalize_observation_project_keys(
    identity: &SourceIdentity,
    tasks: &[TaskRecord],
    observation: &mut HistoryObservation,
) -> SourceProjectNormalizationReport {
    let mut resolver = ProjectPathResolver::new(tasks, observation);
    let mut report = SourceProjectNormalizationReport::default();

    for bucket in &mut observation.half_hour_buckets {
        for group in &mut bucket.project_groups {
            let had_project_evidence = group.project_id.is_some() || group.project_label.is_some();
            // Never allow a legacy machine-local identity to survive an
            // attempted remote normalization.
            group.project_id = None;

            let Some(path) = resolver.resolve(&group.thread_id) else {
                if had_project_evidence {
                    report.groups_unresolved = report.groups_unresolved.saturating_add(1);
                }
                continue;
            };
            let Ok(project_key) = ObservedProjectKey::from_canonical_path(identity, &path) else {
                if had_project_evidence {
                    report.groups_unresolved = report.groups_unresolved.saturating_add(1);
                }
                continue;
            };
            group.project_id = Some(project_key.as_str().to_owned());
            report.observed_project_keys.insert(project_key);
            report.groups_rekeyed = report.groups_rekeyed.saturating_add(1);
        }
    }

    report
}

/// Rebuilds sanitized descriptors for a normalized local observation while
/// task cwd evidence is still available. Git probing is deliberately
/// best-effort: failure removes only the merge suggestion metadata and never
/// blocks usage registration.
#[cfg(test)]
pub(crate) fn local_project_descriptors(
    tasks: &[TaskRecord],
    observation: &HistoryObservation,
) -> Vec<RemoteProjectDescriptor> {
    let mut git = GitProjectEvidenceResolver::default();
    git.begin_collection();
    local_project_descriptors_with_resolver(tasks, observation, &mut git)
}

pub(crate) fn local_project_descriptors_with_resolver(
    tasks: &[TaskRecord],
    observation: &HistoryObservation,
    git: &mut GitProjectEvidenceResolver,
) -> Vec<RemoteProjectDescriptor> {
    let mut descriptors = BTreeMap::<ObservedProjectKey, ProjectDisplayLabel>::new();
    for group in observation
        .half_hour_buckets
        .iter()
        .flat_map(|bucket| &bucket.project_groups)
    {
        let Some(key) = group
            .project_id
            .as_deref()
            .and_then(|value| ObservedProjectKey::from_str(value).ok())
        else {
            continue;
        };
        let label = sanitized_project_label(group.project_label.as_deref());
        descriptors
            .entry(key)
            .and_modify(|existing| {
                if label.as_str() < existing.as_str() {
                    *existing = label.clone();
                }
            })
            .or_insert(label);
    }
    project_descriptors_with_git_evidence_using(
        descriptors,
        resolved_descriptor_paths(tasks, observation),
        git,
    )
}

fn resolved_descriptor_paths(
    tasks: &[TaskRecord],
    observation: &HistoryObservation,
) -> BTreeMap<ObservedProjectKey, ResolvedProjectPath> {
    let mut resolver = ProjectPathResolver::new(tasks, observation);
    let mut paths = BTreeMap::new();
    for group in observation
        .half_hour_buckets
        .iter()
        .flat_map(|bucket| &bucket.project_groups)
    {
        let Some(key) = group
            .project_id
            .as_deref()
            .and_then(|value| ObservedProjectKey::from_str(value).ok())
        else {
            continue;
        };
        let resolved = resolver
            .resolve(&group.thread_id)
            .map_or(ResolvedProjectPath::Missing, ResolvedProjectPath::Path);
        match resolved {
            ResolvedProjectPath::Path(path) => merge_descriptor_path(&mut paths, &key, path),
            ResolvedProjectPath::Missing | ResolvedProjectPath::Ambiguous => {}
        }
    }
    paths
}

fn merge_descriptor_path(
    paths: &mut BTreeMap<ObservedProjectKey, ResolvedProjectPath>,
    key: &ObservedProjectKey,
    incoming: PathBuf,
) {
    use std::collections::btree_map::Entry;
    match paths.entry(key.clone()) {
        Entry::Vacant(entry) => {
            entry.insert(ResolvedProjectPath::Path(incoming));
        }
        Entry::Occupied(mut entry)
            if entry.get() != &ResolvedProjectPath::Path(incoming.clone()) =>
        {
            entry.insert(ResolvedProjectPath::Ambiguous);
        }
        Entry::Occupied(_) => {}
    }
}

fn project_descriptors_with_git_evidence_using(
    descriptors: BTreeMap<ObservedProjectKey, ProjectDisplayLabel>,
    descriptor_paths: BTreeMap<ObservedProjectKey, ResolvedProjectPath>,
    git: &mut GitProjectEvidenceResolver,
) -> Vec<RemoteProjectDescriptor> {
    descriptors
        .into_iter()
        .map(|(observed_project_key, display_label)| {
            let git_evidence = match descriptor_paths.get(&observed_project_key) {
                Some(ResolvedProjectPath::Path(path)) => git.inspect(path),
                Some(ResolvedProjectPath::Missing | ResolvedProjectPath::Ambiguous) | None => {
                    GitProjectEvidence::Unavailable
                }
            };
            let git_evidence = match git_evidence {
                GitProjectEvidence::Unavailable => RemoteGitRepositoryEvidence::Unavailable,
                GitProjectEvidence::ConfirmedNonRepository => {
                    RemoteGitRepositoryEvidence::ConfirmedNonRepository
                }
                GitProjectEvidence::Repository {
                    fingerprint,
                    repository_relative_workspace_root,
                } => RemoteGitRepositoryEvidence::Repository {
                    fingerprint,
                    repository_relative_workspace_root,
                },
            };
            RemoteProjectDescriptor {
                observed_project_key,
                display_label,
                git_evidence,
            }
        })
        .collect()
}

#[derive(Clone, Debug, Default)]
struct BucketApiCosts {
    total: ApiCostAccumulator,
    by_model: BTreeMap<(Option<String>, Option<String>), ApiCostAccumulator>,
}

fn api_costs_by_bucket(
    calls: &[UsageCall],
    observed_at: DateTime<Utc>,
) -> BTreeMap<DateTime<Utc>, BucketApiCosts> {
    let mut result = BTreeMap::<DateTime<Utc>, BucketApiCosts>::new();
    for call in calls {
        if call.timestamp > observed_at {
            continue;
        }
        let Some(starts_at) = bucket_start(call.timestamp) else {
            continue;
        };
        let bucket = result.entry(starts_at).or_default();
        bucket.total.add_call(call);
        bucket
            .by_model
            .entry((call.model.clone(), call.service_tier.clone()))
            .or_default()
            .add_call(call);
    }
    result
}

fn bucket_start(timestamp: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let seconds = timestamp.timestamp().div_euclid(15 * 60) * 15 * 60;
    Utc.timestamp_opt(seconds, 0).single()
}

fn materialize_bucket(
    bucket: &LocalHalfHourBucket,
    redaction_profile: RedactionProfile,
    api_costs: Option<&BucketApiCosts>,
    descriptors: &mut BTreeMap<ObservedProjectKey, ProjectDisplayLabel>,
    invalid_thread_groups: &mut usize,
) -> io::Result<RemoteUsageBucket> {
    let mut partial_reasons = normalized_partial_reasons(&bucket.partial_reasons);
    let mut model_groups = bucket
        .groups
        .iter()
        .map(|group| materialize_model_group(group, api_costs))
        .collect::<io::Result<Vec<_>>>()?;
    model_groups.sort_by(|left, right| {
        (left.model.as_deref(), left.service_tier.as_deref())
            .cmp(&(right.model.as_deref(), right.service_tier.as_deref()))
    });

    let mut project_groups = Vec::with_capacity(bucket.project_groups.len());
    for group in &bucket.project_groups {
        match materialize_project_group(group, redaction_profile, descriptors)? {
            Some(group) => project_groups.push(group),
            None => {
                *invalid_thread_groups = invalid_thread_groups.saturating_add(1);
                partial_reasons.insert(INVALID_THREAD_REASON.to_owned());
            }
        }
    }
    project_groups.sort_by(|left, right| {
        (
            left.observed_project_key
                .as_ref()
                .map(ObservedProjectKey::as_str),
            left.emitting_thread_id.as_str(),
            left.emitting_turn_id.as_deref(),
            left.parent_thread_id.as_ref().map(ThreadId::as_str),
            left.root_session_thread_id.as_ref().map(ThreadId::as_str),
            left.root_session_turn_id.as_deref(),
        )
            .cmp(&(
                right
                    .observed_project_key
                    .as_ref()
                    .map(ObservedProjectKey::as_str),
                right.emitting_thread_id.as_str(),
                right.emitting_turn_id.as_deref(),
                right.parent_thread_id.as_ref().map(ThreadId::as_str),
                right.root_session_thread_id.as_ref().map(ThreadId::as_str),
                right.root_session_turn_id.as_deref(),
            ))
    });

    let revisions = current_revisions();
    Ok(RemoteUsageBucket {
        starts_at: bucket.starts_at,
        ends_at: bucket.ends_at,
        sampled_at: bucket.sampled_at,
        token_usage: remote_token_usage(bucket.token_usage)?,
        estimated_cost_units: RemoteU128::new(bucket.estimated_cost_units),
        api_long_context_extra_cost_units: bucket
            .api_long_context_extra_cost_units
            .map(RemoteU128::new),
        long_context_usage_unknown: bucket.long_context_usage_unknown,
        api_equivalent_cost: remote_api_cost(
            api_costs.map_or_else(ApiCostAmount::default, |costs| costs.total.amount()),
        ),
        call_count: bucket.call_count,
        metric_revision: revisions.metric,
        estimator_revision: nonzero_revision(bucket.estimator_revision, "bucket estimator")?,
        project_breakdown_revision: nonzero_revision(
            bucket.project_breakdown_revision,
            "bucket project breakdown",
        )?,
        api_pricing_catalog_revision: nonzero_revision(
            bucket.api_pricing_catalog_revision,
            "bucket API pricing catalog",
        )?,
        model_groups,
        project_groups,
        partial_reasons: partial_reasons.into_iter().collect(),
    })
}

fn materialize_model_group(
    group: &LocalUsageGroup,
    api_costs: Option<&BucketApiCosts>,
) -> io::Result<RemoteModelUsageGroup> {
    let key = (group.model.clone(), group.service_tier.clone());
    let api_cost = api_costs
        .and_then(|costs| costs.by_model.get(&key))
        .map_or_else(ApiCostAmount::default, ApiCostAccumulator::amount);
    Ok(RemoteModelUsageGroup {
        model: group.model.clone(),
        service_tier: group.service_tier.clone(),
        token_usage: remote_token_usage(group.token_usage)?,
        estimated_cost_units: RemoteU128::new(group.estimated_cost_units),
        api_long_context_extra_cost_units: group
            .api_long_context_extra_cost_units
            .map(RemoteU128::new),
        api_equivalent_cost: remote_api_cost(api_cost),
        call_count: group.call_count,
        used_model_fallback: group.used_model_fallback,
        used_token_breakdown_fallback: group.used_token_breakdown_fallback,
        used_long_context_pricing: group.used_long_context_pricing,
        used_long_context_detection_fallback: group.used_long_context_detection_fallback,
    })
}

fn materialize_project_group(
    group: &LocalProjectUsageGroup,
    redaction_profile: RedactionProfile,
    descriptors: &mut BTreeMap<ObservedProjectKey, ProjectDisplayLabel>,
) -> io::Result<Option<RemoteProjectUsageGroup>> {
    let Ok(emitting_thread_id) = ThreadId::from_str(&group.thread_id) else {
        return Ok(None);
    };
    let parent_thread_id = parse_optional_thread_id(group.parent_thread_id.as_deref());
    let root_session_thread_id = parse_optional_thread_id(group.session_thread_id.as_deref());
    let observed_project_key = group
        .project_id
        .as_deref()
        .and_then(|value| ObservedProjectKey::from_str(value).ok());
    if let Some(key) = observed_project_key.as_ref() {
        let label = sanitized_project_label(group.project_label.as_deref());
        descriptors
            .entry(key.clone())
            .and_modify(|existing| {
                if label.as_str() < existing.as_str() {
                    *existing = label.clone();
                }
            })
            .or_insert(label);
    }
    let (title_preview, message_preview) = if redaction_profile == RedactionProfile::Redacted {
        (None, None)
    } else {
        (
            sanitized_preview(group.title.as_deref()),
            sanitized_preview(group.message_preview.as_deref()),
        )
    };
    Ok(Some(RemoteProjectUsageGroup {
        observed_project_key,
        emitting_thread_id,
        emitting_turn_id: sanitized_protocol_text(group.turn_id.as_deref()),
        parent_thread_id,
        root_session_thread_id,
        root_session_turn_id: sanitized_protocol_text(group.session_turn_id.as_deref()),
        title_preview,
        message_preview,
        token_usage: remote_token_usage(group.token_usage)?,
        estimated_cost_units: RemoteU128::new(group.estimated_cost_units),
        api_long_context_extra_cost_units: group
            .api_long_context_extra_cost_units
            .map(RemoteU128::new),
        api_equivalent_cost: remote_api_cost(group.api_equivalent_cost),
        call_count: group.call_count,
    }))
}

fn parse_optional_thread_id(value: Option<&str>) -> Option<ThreadId> {
    value.and_then(|value| ThreadId::from_str(value).ok())
}

fn strict_optional_thread_id(value: Option<&str>, subject: &str) -> io::Result<Option<ThreadId>> {
    value
        .map(|value| {
            ThreadId::from_str(value.trim()).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{subject}: {error}"))
            })
        })
        .transpose()
}

fn normalized_lookup_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn strict_optional_protocol_text(
    value: Option<&str>,
    maximum_bytes: usize,
    subject: &str,
) -> io::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty()
        || value.len() > maximum_bytes
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2028}'
                        | '\u{2029}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{subject} is not bounded protocol text"),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn sanitized_project_label(value: Option<&str>) -> ProjectDisplayLabel {
    let sanitized = value
        .map(|value| {
            value
                .trim()
                .chars()
                .filter(|character| {
                    !character.is_control()
                        && !matches!(character, '\u{2028}' | '\u{2029}' | '/' | '\\')
                })
                .take(160)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty() && !matches!(value.as_str(), "." | ".."))
        .unwrap_or_else(|| FALLBACK_PROJECT_LABEL.to_owned());
    ProjectDisplayLabel::from_str(&sanitized).unwrap_or_else(|_| {
        ProjectDisplayLabel::from_str(FALLBACK_PROJECT_LABEL)
            .expect("the built-in fallback project label is valid")
    })
}

fn sanitized_protocol_text(value: Option<&str>) -> Option<String> {
    sanitized_protocol_text_with_limit(value, 256)
}

fn sanitized_protocol_text_with_limit(value: Option<&str>, maximum_chars: usize) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .chars()
                .filter(|character| {
                    !character.is_control() && !matches!(character, '\u{2028}' | '\u{2029}')
                })
                .take(maximum_chars)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
}

fn sanitized_preview(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .chars()
                .filter(|character| {
                    !character.is_control() && !matches!(character, '\u{2028}' | '\u{2029}')
                })
                .take(1_024)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
}

fn normalized_partial_reasons(reasons: &[String]) -> BTreeSet<String> {
    let mut normalized = BTreeSet::new();
    for reason in reasons {
        if normalized.len() == MAX_EXPORTED_PARTIAL_REASONS {
            break;
        }
        if valid_machine_code(reason) {
            normalized.insert(reason.clone());
        } else {
            normalized.insert("invalid_source_partial_reason".to_owned());
        }
    }
    normalized
}

fn valid_machine_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.' | b':')
        })
}

fn nonzero_revision(value: u32, subject: &str) -> io::Result<NonZeroU32> {
    NonZeroU32::new(value).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{subject} revision must be nonzero"),
        )
    })
}

fn remote_token_usage(usage: TokenUsage) -> io::Result<RemoteTokenUsage> {
    if usage.cached_input_tokens > usage.input_tokens
        || usage.cache_write_input_tokens > usage.input_tokens
        || usage.reasoning_output_tokens > usage.output_tokens
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token detail exceeds its containing input or output total",
        ));
    }
    let breakdown_total = usage.input_tokens.checked_add(usage.output_tokens);
    if breakdown_total.is_none()
        || breakdown_total.is_some_and(|total| total != 0 && total != usage.total_tokens)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token total does not match its input/output breakdown",
        ));
    }
    Ok(RemoteTokenUsage {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        total_tokens: usage.total_tokens,
    })
}

fn remote_api_cost(cost: ApiCostAmount) -> RemoteApiCostAmount {
    RemoteApiCostAmount {
        minimum_pico_usd: RemoteU128::new(cost.minimum_pico_usd.value()),
        maximum_pico_usd: RemoteU128::new(cost.maximum_pico_usd.value()),
        observed_samples: cost.observed_samples,
        priced_samples: cost.priced_samples,
        observed_tokens: cost.observed_tokens,
        priced_tokens: cost.priced_tokens,
    }
}

#[derive(Clone, Debug)]
struct DigestEvent {
    semantic_hash: [u8; 32],
    call: UsageCall,
}

#[derive(Clone, Debug, Default)]
struct SessionDigestAccumulator {
    events: BTreeMap<String, DigestEvent>,
    observed_project_keys: BTreeSet<ObservedProjectKey>,
    partial_reasons: BTreeSet<String>,
    exact_event_identity: bool,
}

struct DigestCoverage {
    covered_through: DateTime<Utc>,
    complete: bool,
    partial_reasons: BTreeSet<String>,
}

fn materialize_session_digests(
    calls: &[UsageCall],
    buckets: &[LocalHalfHourBucket],
    observed_at: DateTime<Utc>,
    scan_complete: bool,
) -> io::Result<(Vec<RemoteSessionDigest>, usize, usize)> {
    let retention_start = observed_at
        .checked_sub_days(Days::new(SESSION_DIGEST_RETENTION_DAYS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let mut project_keys = BTreeMap::<(ThreadId, NaiveDate), BTreeSet<ObservedProjectKey>>::new();
    let mut bucket_partial_reasons = BTreeMap::<(ThreadId, NaiveDate), BTreeSet<String>>::new();
    for bucket in buckets {
        let day = bucket.starts_at.date_naive();
        let reasons = normalized_partial_reasons(&bucket.partial_reasons);
        for group in bucket
            .project_groups
            .iter()
            .filter(|group| group.call_count > 0)
        {
            let Ok(thread_id) = ThreadId::from_str(&group.thread_id) else {
                continue;
            };
            if let Some(project_key) = group
                .project_id
                .as_deref()
                .and_then(|value| ObservedProjectKey::from_str(value).ok())
            {
                project_keys
                    .entry((thread_id.clone(), day))
                    .or_default()
                    .insert(project_key);
            }
            bucket_partial_reasons
                .entry((thread_id, day))
                .or_default()
                .extend(reasons.iter().cloned());
        }
    }

    let mut accumulators = BTreeMap::<(ThreadId, NaiveDate), SessionDigestAccumulator>::new();
    let mut missing_event_identities = 0_usize;
    let mut invalid_threads = 0_usize;
    for call in calls {
        if call.timestamp > observed_at || call.timestamp < retention_start {
            continue;
        }
        let Ok(thread_id) = ThreadId::from_str(&call.thread_id) else {
            invalid_threads = invalid_threads.saturating_add(1);
            continue;
        };
        let key = (thread_id, call.timestamp.date_naive());
        let accumulator = accumulators
            .entry(key)
            .or_insert_with(|| SessionDigestAccumulator {
                exact_event_identity: true,
                ..SessionDigestAccumulator::default()
            });
        let semantic_hash = digest_event_semantic_hash(call);
        let parsed_event_id = call
            .usage_event_id
            .as_deref()
            .and_then(|value| UsageEventId::from_str(value).ok());
        let (event_id, exact_identity) = if let Some(event_id) = parsed_event_id {
            if !call.usage_event_identity_exact {
                accumulator
                    .partial_reasons
                    .insert(FALLBACK_EVENT_REASON.to_owned());
            }
            (
                event_id.as_str().to_owned(),
                call.usage_event_identity_exact,
            )
        } else {
            missing_event_identities = missing_event_identities.saturating_add(1);
            accumulator
                .partial_reasons
                .insert(MISSING_EVENT_REASON.to_owned());
            accumulator
                .partial_reasons
                .insert(FALLBACK_EVENT_REASON.to_owned());
            (
                format!("usage-derived-sha256-v1-{}", lower_hex(&semantic_hash)),
                false,
            )
        };
        accumulator.exact_event_identity &= exact_identity;

        match accumulator.events.get_mut(&event_id) {
            Some(existing) if existing.semantic_hash == semantic_hash => {}
            Some(existing) => {
                accumulator.exact_event_identity = false;
                accumulator
                    .partial_reasons
                    .insert(CONFLICTING_EVENT_REASON.to_owned());
                // The same supposedly stable ID cannot represent two events.
                // Keep one deterministic lower semantic hash so input order
                // cannot alter either totals or the replica fingerprint.
                if semantic_hash < existing.semantic_hash {
                    *existing = DigestEvent {
                        semantic_hash,
                        call: call.clone(),
                    };
                }
            }
            None => {
                accumulator.events.insert(
                    event_id,
                    DigestEvent {
                        semantic_hash,
                        call: call.clone(),
                    },
                );
            }
        }
    }

    let revisions = current_revisions();
    let mut digests = Vec::with_capacity(accumulators.len());
    for ((thread_id, day), mut accumulator) in accumulators {
        let range_start = utc_day_start(day)?;
        let range_end = range_start.checked_add_days(Days::new(1)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "session digest day overflows")
        })?;
        let coverage = digest_coverage(range_start, range_end, buckets, observed_at, scan_complete);
        accumulator.partial_reasons.extend(coverage.partial_reasons);
        if accumulator
            .events
            .values()
            .any(|event| event.call.timestamp > coverage.covered_through)
        {
            accumulator
                .partial_reasons
                .insert(EVENT_OUTSIDE_COVERAGE_REASON.to_owned());
        }
        if let Some(reasons) = bucket_partial_reasons.get(&(thread_id.clone(), day)) {
            accumulator.partial_reasons.extend(reasons.iter().cloned());
        }
        if let Some(keys) = project_keys.remove(&(thread_id.clone(), day)) {
            accumulator.observed_project_keys.extend(keys);
        }
        if accumulator.observed_project_keys.is_empty() {
            accumulator
                .partial_reasons
                .insert(PROJECT_ATTRIBUTION_REASON.to_owned());
        }

        let mut token_usage = TokenUsage::default();
        let mut estimated_cost_units = 0_u128;
        let mut api_long_context_extra_cost_units = 0_u128;
        let mut api_cost = ApiCostAccumulator::default();
        for event in accumulator.events.values() {
            api_cost.add_call(&event.call);
            if is_spark_model(event.call.model.as_deref()) {
                continue;
            }
            token_usage.add_assign(event.call.tokens);
            let weight = estimate_call_weight(&event.call);
            estimated_cost_units = estimated_cost_units.saturating_add(weight.units);
            api_long_context_extra_cost_units = api_long_context_extra_cost_units
                .saturating_add(weight.api_long_context_extra_units);
            if weight.used_model_fallback {
                accumulator
                    .partial_reasons
                    .insert(MODEL_FALLBACK_REASON.to_owned());
            }
            if weight.used_token_breakdown_fallback {
                accumulator
                    .partial_reasons
                    .insert(TOKEN_BREAKDOWN_REASON.to_owned());
            }
            if weight.used_long_context_detection_fallback {
                accumulator
                    .partial_reasons
                    .insert(LONG_CONTEXT_UNKNOWN_REASON.to_owned());
            }
        }
        accumulator
            .partial_reasons
            .extend(api_cost.summary().partial_reasons);
        let event_count = u64::try_from(accumulator.events.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "too many session digest events")
        })?;
        let fingerprint =
            digest_fingerprint(&thread_id, range_start, range_end, &accumulator.events)?;
        let project_breakdown_fingerprint =
            project_breakdown_fingerprint(&thread_id, range_start, range_end, buckets)?;
        digests.push(RemoteSessionDigest {
            thread_id,
            range_start,
            range_end,
            covered_through: coverage.covered_through,
            fingerprint,
            project_breakdown_fingerprint,
            event_count,
            exact_event_identity: accumulator.exact_event_identity,
            coverage_complete: coverage.complete,
            observed_project_keys: accumulator.observed_project_keys.into_iter().collect(),
            metrics: RemoteSessionUsageMetrics {
                token_usage: remote_token_usage(token_usage)?,
                estimated_cost_units: RemoteU128::new(estimated_cost_units),
                api_long_context_extra_cost_units: Some(RemoteU128::new(
                    api_long_context_extra_cost_units,
                )),
                api_equivalent_cost: remote_api_cost(api_cost.amount()),
                call_count: event_count,
                metric_revision: revisions.metric,
                estimator_revision: revisions.estimator,
                project_breakdown_revision: revisions.project_breakdown,
                api_pricing_catalog_revision: revisions.api_pricing_catalog,
                partial_reasons: accumulator
                    .partial_reasons
                    .into_iter()
                    .take(MAX_EXPORTED_PARTIAL_REASONS)
                    .collect(),
            },
        });
    }
    digests.sort_by(|left, right| {
        (left.thread_id.as_str(), left.range_start)
            .cmp(&(right.thread_id.as_str(), right.range_start))
    });
    Ok((digests, missing_event_identities, invalid_threads))
}

fn replace_digest_project_attribution(
    digests: &mut [RemoteSessionDigest],
    buckets: &[LocalHalfHourBucket],
) -> io::Result<()> {
    let mut project_keys = BTreeMap::<(ThreadId, NaiveDate), BTreeSet<ObservedProjectKey>>::new();
    for bucket in buckets {
        let day = bucket.starts_at.date_naive();
        for group in bucket
            .project_groups
            .iter()
            .filter(|group| group.call_count > 0)
        {
            let (Ok(thread_id), Some(project_key)) = (
                ThreadId::from_str(&group.thread_id),
                group
                    .project_id
                    .as_deref()
                    .and_then(|value| ObservedProjectKey::from_str(value).ok()),
            ) else {
                continue;
            };
            project_keys
                .entry((thread_id, day))
                .or_default()
                .insert(project_key);
        }
    }

    for digest in digests {
        digest.observed_project_keys = project_keys
            .remove(&(digest.thread_id.clone(), digest.range_start.date_naive()))
            .unwrap_or_default()
            .into_iter()
            .collect();
        digest.project_breakdown_fingerprint = project_breakdown_fingerprint(
            &digest.thread_id,
            digest.range_start,
            digest.range_end,
            buckets,
        )?;
        let mut reasons = digest
            .metrics
            .partial_reasons
            .iter()
            .filter(|reason| reason.as_str() != PROJECT_ATTRIBUTION_REASON)
            .cloned()
            .collect::<BTreeSet<_>>();
        if digest.observed_project_keys.is_empty() {
            reasons.insert(PROJECT_ATTRIBUTION_REASON.to_owned());
        }
        digest.metrics.partial_reasons = reasons
            .into_iter()
            .take(MAX_EXPORTED_PARTIAL_REASONS)
            .collect();
    }
    Ok(())
}

fn local_source_session_digest(
    identity: &SourceIdentity,
    digest: RemoteSessionDigest,
) -> io::Result<SourceSessionDigest> {
    SourceSessionDigest::new(
        SessionReplicaKey::new(identity.node_id().clone(), digest.thread_id),
        digest.range_start,
        digest.range_end,
        digest.covered_through,
        SessionDigestFingerprint::from_str(digest.fingerprint.as_str()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("local session digest fingerprint is invalid: {error}"),
            )
        })?,
        SessionDigestFingerprint::from_str(digest.project_breakdown_fingerprint.as_str()).map_err(
            |error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("local session project fingerprint is invalid: {error}"),
                )
            },
        )?,
        digest.event_count,
        digest.exact_event_identity,
        digest.coverage_complete,
        digest.observed_project_keys,
        SessionUsageMetrics {
            token_usage: TokenUsage {
                input_tokens: digest.metrics.token_usage.input_tokens,
                cached_input_tokens: digest.metrics.token_usage.cached_input_tokens,
                cache_write_input_tokens: digest.metrics.token_usage.cache_write_input_tokens,
                output_tokens: digest.metrics.token_usage.output_tokens,
                reasoning_output_tokens: digest.metrics.token_usage.reasoning_output_tokens,
                total_tokens: digest.metrics.token_usage.total_tokens,
            },
            estimated_cost_units: digest.metrics.estimated_cost_units.value(),
            api_long_context_extra_cost_units: digest
                .metrics
                .api_long_context_extra_cost_units
                .map(|value| value.value()),
            api_equivalent_cost: ApiCostAmount {
                minimum_pico_usd: crate::domain::PicoUsd::new(
                    digest.metrics.api_equivalent_cost.minimum_pico_usd.value(),
                ),
                maximum_pico_usd: crate::domain::PicoUsd::new(
                    digest.metrics.api_equivalent_cost.maximum_pico_usd.value(),
                ),
                observed_samples: digest.metrics.api_equivalent_cost.observed_samples,
                priced_samples: digest.metrics.api_equivalent_cost.priced_samples,
                observed_tokens: digest.metrics.api_equivalent_cost.observed_tokens,
                priced_tokens: digest.metrics.api_equivalent_cost.priced_tokens,
            },
            call_count: digest.metrics.call_count,
            metric_revision: digest.metrics.metric_revision.get(),
            estimator_revision: digest.metrics.estimator_revision.get(),
            project_breakdown_revision: digest.metrics.project_breakdown_revision.get(),
            api_pricing_catalog_revision: digest.metrics.api_pricing_catalog_revision.get(),
            partial_reasons: digest.metrics.partial_reasons,
        },
    )
}

fn digest_coverage(
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    buckets: &[LocalHalfHourBucket],
    observed_at: DateTime<Utc>,
    scan_complete: bool,
) -> DigestCoverage {
    let target = observed_at.max(range_start).min(range_end);
    let by_start = buckets
        .iter()
        .filter(|bucket| bucket.starts_at >= range_start && bucket.starts_at < target)
        .map(|bucket| (bucket.starts_at, bucket))
        .collect::<BTreeMap<_, _>>();
    let mut covered_through = range_start;
    let mut partial_reasons = BTreeSet::new();
    while covered_through < target {
        let Some(bucket) = by_start.get(&covered_through) else {
            partial_reasons.insert(COVERAGE_GAP_REASON.to_owned());
            break;
        };
        if bucket
            .partial_reasons
            .iter()
            .any(|reason| is_hard_coverage_reason(reason))
        {
            partial_reasons.extend(normalized_partial_reasons(&bucket.partial_reasons));
            partial_reasons.insert(COVERAGE_GAP_REASON.to_owned());
            break;
        }
        let interval_end = bucket.ends_at.min(target);
        if interval_end <= covered_through {
            partial_reasons.insert(COVERAGE_GAP_REASON.to_owned());
            break;
        }
        covered_through = if target < bucket.ends_at {
            bucket.sampled_at.max(bucket.starts_at).min(interval_end)
        } else {
            interval_end
        };
    }
    if !scan_complete {
        partial_reasons.insert(INCOMPLETE_SCAN_REASON.to_owned());
    }
    if observed_at < range_end {
        partial_reasons.insert(OPEN_RANGE_REASON.to_owned());
    }
    let complete = scan_complete
        && target == range_end
        && covered_through == range_end
        && partial_reasons.is_empty();
    DigestCoverage {
        covered_through,
        complete,
        partial_reasons,
    }
}

fn is_hard_coverage_reason(reason: &str) -> bool {
    matches!(
        reason,
        "rollout_local_coverage_unverified"
            | "coverage_starts_within_local_bucket"
            | "rollout_scan_incomplete"
            | "rollout_scan_truncated"
            | "rollout_unreadable"
            | "rollout_lines_skipped"
            | "ambiguous_token_reset"
    )
}

fn utc_day_start(day: NaiveDate) -> io::Result<DateTime<Utc>> {
    let naive = day.and_hms_opt(0, 0, 0).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "session digest day is invalid")
    })?;
    Ok(Utc.from_utc_datetime(&naive))
}

fn digest_event_semantic_hash(call: &UsageCall) -> [u8; 32] {
    digest_event_semantic_hash_fields(
        &call.thread_id,
        call.timestamp,
        call.turn_id.as_deref(),
        call.model.as_deref(),
        call.service_tier.as_deref(),
        call.tokens,
        call.request_usage_exact,
    )
}

fn digest_event_semantic_hash_fields(
    thread_id: &str,
    timestamp: DateTime<Utc>,
    turn_id: Option<&str>,
    model: Option<&str>,
    service_tier: Option<&str>,
    tokens: TokenUsage,
    request_usage_exact: bool,
) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(SESSION_DIGEST_DOMAIN);
    digest_field(&mut digest, thread_id.as_bytes());
    digest_field(&mut digest, timestamp.to_rfc3339().as_bytes());
    digest_optional_field(&mut digest, canonical_digest_optional_text(turn_id));
    digest_optional_field(&mut digest, canonical_digest_optional_text(model));
    digest_optional_field(&mut digest, canonical_digest_optional_text(service_tier));
    for value in [
        tokens.input_tokens,
        tokens.cached_input_tokens,
        tokens.cache_write_input_tokens,
        tokens.output_tokens,
        tokens.reasoning_output_tokens,
        tokens.total_tokens,
    ] {
        digest.update(value.to_be_bytes());
    }
    digest.update([u8::from(request_usage_exact)]);
    digest.finalize().into()
}

fn canonical_digest_optional_text(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn digest_fingerprint(
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    events: &BTreeMap<String, DigestEvent>,
) -> io::Result<RemoteSessionDigestFingerprint> {
    let fingerprint = digest_fingerprint_value(
        thread_id,
        range_start,
        range_end,
        events
            .iter()
            .map(|(event_id, event)| (event_id.as_str(), event.semantic_hash)),
    );
    RemoteSessionDigestFingerprint::from_str(&fingerprint)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn digest_fingerprint_value<'a>(
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    events: impl IntoIterator<Item = (&'a str, [u8; 32])>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(SESSION_DIGEST_DOMAIN);
    digest_field(&mut digest, thread_id.as_str().as_bytes());
    digest_field(&mut digest, range_start.to_rfc3339().as_bytes());
    digest_field(&mut digest, range_end.to_rfc3339().as_bytes());
    for (event_id, semantic_hash) in events {
        digest_field(&mut digest, event_id.as_bytes());
        digest.update(semantic_hash);
    }
    format!("{SESSION_DIGEST_PREFIX}{}", lower_hex(&digest.finalize()))
}

fn project_breakdown_fingerprint(
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    buckets: &[LocalHalfHourBucket],
) -> io::Result<RemoteSessionDigestFingerprint> {
    let groups = buckets
        .iter()
        .filter(|bucket| bucket.starts_at >= range_start && bucket.starts_at < range_end)
        .flat_map(|bucket| {
            bucket
                .project_groups
                .iter()
                .filter(|group| group.call_count > 0 && group.thread_id == thread_id.as_str())
                .map(move |group| (bucket.starts_at, group))
        })
        .collect::<Vec<_>>();
    let fingerprint =
        project_breakdown_fingerprint_value(thread_id, range_start, range_end, groups);
    RemoteSessionDigestFingerprint::from_str(&fingerprint)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn project_breakdown_fingerprint_value<'a>(
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    groups: impl IntoIterator<Item = (DateTime<Utc>, &'a LocalProjectUsageGroup)>,
) -> String {
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by(|(left_at, left), (right_at, right)| {
        left_at
            .cmp(right_at)
            .then_with(|| left.project_id.cmp(&right.project_id))
            .then_with(|| left.turn_id.cmp(&right.turn_id))
            .then_with(|| left.parent_thread_id.cmp(&right.parent_thread_id))
            .then_with(|| left.session_thread_id.cmp(&right.session_thread_id))
            .then_with(|| left.session_turn_id.cmp(&right.session_turn_id))
            .then_with(|| {
                left.token_usage
                    .input_tokens
                    .cmp(&right.token_usage.input_tokens)
            })
            .then_with(|| {
                left.token_usage
                    .cached_input_tokens
                    .cmp(&right.token_usage.cached_input_tokens)
            })
            .then_with(|| {
                left.token_usage
                    .cache_write_input_tokens
                    .cmp(&right.token_usage.cache_write_input_tokens)
            })
            .then_with(|| {
                left.token_usage
                    .output_tokens
                    .cmp(&right.token_usage.output_tokens)
            })
            .then_with(|| {
                left.token_usage
                    .reasoning_output_tokens
                    .cmp(&right.token_usage.reasoning_output_tokens)
            })
            .then_with(|| {
                left.token_usage
                    .total_tokens
                    .cmp(&right.token_usage.total_tokens)
            })
            .then_with(|| left.call_count.cmp(&right.call_count))
            .then_with(|| left.estimated_cost_units.cmp(&right.estimated_cost_units))
            .then_with(|| {
                left.api_long_context_extra_cost_units
                    .cmp(&right.api_long_context_extra_cost_units)
            })
            .then_with(|| {
                left.api_equivalent_cost
                    .minimum_pico_usd
                    .cmp(&right.api_equivalent_cost.minimum_pico_usd)
            })
            .then_with(|| {
                left.api_equivalent_cost
                    .maximum_pico_usd
                    .cmp(&right.api_equivalent_cost.maximum_pico_usd)
            })
            .then_with(|| {
                left.api_equivalent_cost
                    .observed_samples
                    .cmp(&right.api_equivalent_cost.observed_samples)
            })
            .then_with(|| {
                left.api_equivalent_cost
                    .priced_samples
                    .cmp(&right.api_equivalent_cost.priced_samples)
            })
            .then_with(|| {
                left.api_equivalent_cost
                    .observed_tokens
                    .cmp(&right.api_equivalent_cost.observed_tokens)
            })
            .then_with(|| {
                left.api_equivalent_cost
                    .priced_tokens
                    .cmp(&right.api_equivalent_cost.priced_tokens)
            })
    });

    let mut digest = Sha256::new();
    digest.update(PROJECT_BREAKDOWN_DIGEST_DOMAIN);
    digest_field(&mut digest, thread_id.as_str().as_bytes());
    digest_field(&mut digest, range_start.to_rfc3339().as_bytes());
    digest_field(&mut digest, range_end.to_rfc3339().as_bytes());
    for (starts_at, group) in groups {
        digest_field(&mut digest, starts_at.to_rfc3339().as_bytes());
        digest_optional_field(&mut digest, group.project_id.as_deref());
        digest_optional_field(&mut digest, group.turn_id.as_deref());
        digest_optional_field(&mut digest, group.parent_thread_id.as_deref());
        digest_optional_field(&mut digest, group.session_thread_id.as_deref());
        digest_optional_field(&mut digest, group.session_turn_id.as_deref());
        for value in [
            group.token_usage.input_tokens,
            group.token_usage.cached_input_tokens,
            group.token_usage.cache_write_input_tokens,
            group.token_usage.output_tokens,
            group.token_usage.reasoning_output_tokens,
            group.token_usage.total_tokens,
            group.call_count,
            group.api_equivalent_cost.observed_samples,
            group.api_equivalent_cost.priced_samples,
            group.api_equivalent_cost.observed_tokens,
            group.api_equivalent_cost.priced_tokens,
        ] {
            digest.update(value.to_be_bytes());
        }
        digest.update(group.estimated_cost_units.to_be_bytes());
        match group.api_long_context_extra_cost_units {
            Some(value) => {
                digest.update([1]);
                digest.update(value.to_be_bytes());
            }
            None => digest.update([0]),
        }
        digest.update(
            group
                .api_equivalent_cost
                .minimum_pico_usd
                .value()
                .to_be_bytes(),
        );
        digest.update(
            group
                .api_equivalent_cost
                .maximum_pico_usd
                .value()
                .to_be_bytes(),
        );
    }
    format!("{SESSION_DIGEST_PREFIX}{}", lower_hex(&digest.finalize()))
}

/// Independently proves that every advertised digest binding is represented
/// by the canonical event and project-breakdown inputs in `facts`.
///
/// Aggregate counters and a sender-provided binding are not sufficient: a
/// forged event model/timestamp or project attribution can preserve totals
/// while changing either canonical digest. This verifier deliberately calls
/// the same hash builders used by source-side digest materialization.
pub(crate) fn validate_fact_digest_bindings_against_facts(
    replica: &SessionReplicaKey,
    facts: &[&UsageEventFact],
    bindings: &[FactDigestBinding],
    retained_since: Option<DateTime<Utc>>,
) -> io::Result<()> {
    let mut facts = facts.to_vec();
    facts.sort_by(|left, right| {
        left.occurred_at()
            .cmp(&right.occurred_at())
            .then_with(|| left.event_id().as_str().cmp(right.event_id().as_str()))
    });

    for binding in bindings {
        if retained_since.is_some_and(|cutoff| cutoff > binding.range_start()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fact digest fingerprint binding starts before retained evidence",
            ));
        }
        let first = facts.partition_point(|fact| fact.occurred_at() < binding.range_start());
        let last = facts.partition_point(|fact| fact.occurred_at() < binding.range_end());
        let range_facts = &facts[first..last];
        if range_facts.len() != usize::try_from(binding.event_count()).unwrap_or(usize::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fact digest fingerprint event count does not match its active facts",
            ));
        }

        for fact in range_facts {
            let metrics = fact.metrics();
            if metrics.metric_revision != binding.metric_revision()
                || metrics.estimator_revision != binding.estimator_revision()
                || metrics.project_breakdown_revision != binding.project_breakdown_revision()
                || metrics.api_pricing_catalog_revision != binding.api_pricing_catalog_revision()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fact digest fingerprint inputs do not match their binding",
                ));
            }
        }
        let (event_fingerprint, project_fingerprint) = canonical_fact_fingerprint_values(
            replica,
            binding.range_start(),
            binding.range_end(),
            range_facts,
        )?;
        if event_fingerprint != binding.fingerprint().as_str() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "active fact event fingerprint does not match its validated digest",
            ));
        }

        if project_fingerprint != binding.project_breakdown_fingerprint().as_str() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "active fact project-breakdown fingerprint does not match its validated digest",
            ));
        }
    }
    Ok(())
}

fn canonical_fact_fingerprint_values(
    replica: &SessionReplicaKey,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    facts: &[&UsageEventFact],
) -> io::Result<(String, String)> {
    let mut events = BTreeMap::<String, [u8; 32]>::new();
    let mut project_groups = BTreeMap::<
        (
            DateTime<Utc>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
        LocalProjectUsageGroup,
    >::new();
    for fact in facts {
        let metrics = fact.metrics();
        if fact.replica() != replica || !fact.exact_event_identity() || metrics.call_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fact digest fingerprint requires exact single-call inputs",
            ));
        }

        let digest_tokens = fact.digest_token_usage();
        // Apply the same token-shape validation used before values enter the
        // wire protocol. Accounting metrics cannot substitute for these
        // values because excluded models may intentionally be zero.
        let _ = remote_token_usage(digest_tokens)?;
        let semantic_hash = digest_event_semantic_hash_fields(
            replica.thread_id().as_str(),
            fact.occurred_at(),
            fact.emitting_turn_id(),
            fact.model(),
            fact.service_tier(),
            digest_tokens,
            fact.request_usage_exact(),
        );
        if events
            .insert(fact.event_id().as_str().to_owned(), semantic_hash)
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fact digest fingerprint contains a duplicate event identity",
            ));
        }

        let starts_at = bucket_start(fact.occurred_at()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fact project fingerprint timestamp cannot be bucketed",
            )
        })?;
        let project_session_thread_id = fact
            .project_session_thread_id()
            .map(|thread_id| thread_id.as_str().to_owned());
        let key = (
            starts_at,
            fact.emitting_turn_id().map(str::to_owned),
            project_session_thread_id.clone(),
            fact.root_session_turn_id().map(str::to_owned),
        );
        let project_id = fact.observed_project_key().as_str().to_owned();
        let parent_thread_id = fact
            .parent_thread_id()
            .map(|thread_id| thread_id.as_str().to_owned());
        let group = project_groups
            .entry(key)
            .or_insert_with(|| LocalProjectUsageGroup {
                thread_id: replica.thread_id().as_str().to_owned(),
                turn_id: fact.emitting_turn_id().map(str::to_owned),
                parent_thread_id: parent_thread_id.clone(),
                session_thread_id: project_session_thread_id.clone(),
                session_turn_id: fact.root_session_turn_id().map(str::to_owned),
                project_id: Some(project_id.clone()),
                ..LocalProjectUsageGroup::default()
            });
        if group.project_id.as_deref() != Some(project_id.as_str())
            || group.parent_thread_id != parent_thread_id
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fact project fingerprint attribution is ambiguous within one group",
            ));
        }
        let long_context = metrics.api_long_context_extra_cost_units.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fact project fingerprint is missing long-context input",
            )
        })?;
        group.token_usage.add_assign(metrics.token_usage);
        group.estimated_cost_units = group
            .estimated_cost_units
            .saturating_add(metrics.estimated_cost_units);
        group.api_long_context_extra_cost_units = Some(
            group
                .api_long_context_extra_cost_units
                .unwrap_or_default()
                .saturating_add(long_context),
        );
        group
            .api_equivalent_cost
            .add_assign(metrics.api_equivalent_cost);
        group.call_count = group.call_count.saturating_add(metrics.call_count);
    }

    let event_fingerprint = digest_fingerprint_value(
        replica.thread_id(),
        range_start,
        range_end,
        events
            .iter()
            .map(|(event_id, semantic_hash)| (event_id.as_str(), *semantic_hash)),
    );
    let project_fingerprint = project_breakdown_fingerprint_value(
        replica.thread_id(),
        range_start,
        range_end,
        project_groups
            .iter()
            .map(|((starts_at, _, _, _), group)| (*starts_at, group)),
    );
    Ok((event_fingerprint, project_fingerprint))
}

#[cfg(test)]
pub(crate) fn canonical_fact_fingerprints_for_test(
    replica: &SessionReplicaKey,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    facts: &[&UsageEventFact],
) -> io::Result<(SessionDigestFingerprint, SessionDigestFingerprint)> {
    let (event, project) =
        canonical_fact_fingerprint_values(replica, range_start, range_end, facts)?;
    Ok((
        SessionDigestFingerprint::from_str(&event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        SessionDigestFingerprint::from_str(&project)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    ))
}

fn digest_optional_field(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1]);
            digest_field(digest, value.as_bytes());
        }
        None => digest.update([0]),
    }
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ResolvedProjectPath {
    Path(PathBuf),
    Missing,
    Ambiguous,
}

struct ProjectPathResolver {
    own_paths: HashMap<String, ResolvedProjectPath>,
    parents: HashMap<String, String>,
    ambiguous_parents: HashSet<String>,
    cache: HashMap<String, ResolvedProjectPath>,
}

impl ProjectPathResolver {
    fn new(tasks: &[TaskRecord], observation: &HistoryObservation) -> Self {
        let mut resolver = Self {
            own_paths: HashMap::new(),
            parents: HashMap::new(),
            ambiguous_parents: HashSet::new(),
            cache: HashMap::new(),
        };

        for task in tasks {
            let path = task
                .cwd
                .as_ref()
                .map(resolve_canonical_project_path)
                .unwrap_or(ResolvedProjectPath::Missing);
            merge_path_claim(&mut resolver.own_paths, &task.thread_id, path);
            if let Some(parent) = task.parent_thread_id.as_deref() {
                merge_parent_claim(
                    &mut resolver.parents,
                    &mut resolver.ambiguous_parents,
                    &task.thread_id,
                    parent,
                );
            }
        }

        for group in observation
            .half_hour_buckets
            .iter()
            .flat_map(|bucket| bucket.project_groups.iter())
        {
            if let Some(parent) = group.parent_thread_id.as_deref() {
                merge_parent_claim(
                    &mut resolver.parents,
                    &mut resolver.ambiguous_parents,
                    &group.thread_id,
                    parent,
                );
            }
        }
        resolver
    }

    fn resolve(&mut self, thread_id: &str) -> Option<PathBuf> {
        match self.resolve_inner(thread_id, &mut HashSet::new()) {
            ResolvedProjectPath::Path(path) => Some(path),
            ResolvedProjectPath::Missing | ResolvedProjectPath::Ambiguous => None,
        }
    }

    fn resolve_inner(
        &mut self,
        thread_id: &str,
        visiting: &mut HashSet<String>,
    ) -> ResolvedProjectPath {
        if let Some(cached) = self.cache.get(thread_id) {
            return cached.clone();
        }
        if self.ambiguous_parents.contains(thread_id) || !visiting.insert(thread_id.to_owned()) {
            return ResolvedProjectPath::Ambiguous;
        }

        let own = self
            .own_paths
            .get(thread_id)
            .cloned()
            .unwrap_or(ResolvedProjectPath::Missing);
        let resolved = match own {
            ResolvedProjectPath::Path(_) | ResolvedProjectPath::Ambiguous => own,
            ResolvedProjectPath::Missing => match self.parents.get(thread_id).cloned() {
                Some(parent) => self.resolve_inner(&parent, visiting),
                None => ResolvedProjectPath::Missing,
            },
        };
        visiting.remove(thread_id);
        self.cache.insert(thread_id.to_owned(), resolved.clone());
        resolved
    }
}

fn resolve_canonical_project_path(path: &PathBuf) -> ResolvedProjectPath {
    if !path.is_absolute() {
        return ResolvedProjectPath::Ambiguous;
    }
    match fs::canonicalize(path) {
        Ok(path) if path.is_absolute() => ResolvedProjectPath::Path(path),
        Ok(_) | Err(_) => ResolvedProjectPath::Ambiguous,
    }
}

fn merge_path_claim(
    paths: &mut HashMap<String, ResolvedProjectPath>,
    thread_id: &str,
    incoming: ResolvedProjectPath,
) {
    use std::collections::hash_map::Entry;
    match paths.entry(thread_id.to_owned()) {
        Entry::Vacant(entry) => {
            entry.insert(incoming);
        }
        Entry::Occupied(mut entry) if entry.get() != &incoming => {
            entry.insert(ResolvedProjectPath::Ambiguous);
        }
        Entry::Occupied(_) => {}
    }
}

fn merge_parent_claim(
    parents: &mut HashMap<String, String>,
    ambiguous: &mut HashSet<String>,
    child: &str,
    parent: &str,
) {
    if ambiguous.contains(child) {
        return;
    }
    match parents.get(child) {
        Some(existing) if existing != parent => {
            parents.remove(child);
            ambiguous.insert(child.to_owned());
        }
        Some(_) => {}
        None => {
            parents.insert(child.to_owned(), parent.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use tempfile::tempdir;

    use super::*;
    use crate::api_cost::API_PRICING_CATALOG_REVISION;
    use crate::domain::{
        AgentInteractionKind, Confidence, Provenance, TaskStatus, TokenUsage, TurnStatus,
    };
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, LocalHalfHourBucket,
        LocalProjectUsageGroup,
    };
    use crate::source_identity::NodeId;

    fn identity(node: &str, secret: &str) -> SourceIdentity {
        SourceIdentity::from_test_parts(node.parse::<NodeId>().unwrap(), secret)
    }

    fn task(thread_id: &str, parent: Option<&str>, cwd: Option<PathBuf>) -> TaskRecord {
        TaskRecord {
            thread_id: thread_id.to_owned(),
            parent_thread_id: parent.map(str::to_owned),
            archived: false,
            title: "task".to_owned(),
            cwd,
            source: Some("desktop".to_owned()),
            created_at: None,
            updated_at: None,
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage::default(),
            turn_count: 0,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: None,
        }
    }

    fn observation(groups: Vec<LocalProjectUsageGroup>) -> HistoryObservation {
        let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        HistoryObservation {
            observed_at: starts_at + Duration::minutes(15),
            half_hour_buckets: vec![LocalHalfHourBucket {
                starts_at,
                ends_at: starts_at + Duration::minutes(15),
                sampled_at: starts_at + Duration::minutes(15),
                token_usage: TokenUsage::default(),
                estimated_cost_units: 0,
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
                api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
                call_count: 0,
                groups: Vec::new(),
                project_groups: groups,
                partial_reasons: Vec::new(),
            }],
            ..HistoryObservation::default()
        }
    }

    fn group(thread_id: &str, parent: Option<&str>) -> LocalProjectUsageGroup {
        LocalProjectUsageGroup {
            thread_id: thread_id.to_owned(),
            parent_thread_id: parent.map(str::to_owned),
            project_id: Some("project-legacy-enumerable".to_owned()),
            project_label: Some("project".to_owned()),
            ..LocalProjectUsageGroup::default()
        }
    }

    fn usage_call(
        timestamp: DateTime<Utc>,
        event_id: Option<&str>,
        model: &str,
        total_tokens: u64,
    ) -> UsageCall {
        let output_tokens = total_tokens / 10;
        UsageCall {
            timestamp,
            thread_id: "root".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            usage_event_id: event_id.map(str::to_owned),
            usage_event_identity_exact: event_id.is_some(),
            model: Some(model.to_owned()),
            service_tier: Some("standard".to_owned()),
            tokens: TokenUsage {
                input_tokens: total_tokens - output_tokens,
                cached_input_tokens: (total_tokens - output_tokens) / 4,
                cache_write_input_tokens: 0,
                output_tokens,
                reasoning_output_tokens: output_tokens / 2,
                total_tokens,
            },
            request_usage_exact: true,
        }
    }

    fn turn(thread_id: &str, message: &str, captured_at: DateTime<Utc>) -> TurnRecord {
        TurnRecord {
            thread_id: thread_id.to_owned(),
            turn_id: "turn-1".to_owned(),
            model: Some("gpt-5.6-luna".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            service_tier: Some("standard".to_owned()),
            message_preview: Some(message.to_owned()),
            started_at: Some(captured_at - Duration::minutes(2)),
            completed_at: Some(captured_at - Duration::minutes(1)),
            duration_ms: Some(60_000),
            status: TurnStatus::Completed,
            token_usage: TokenUsage::default(),
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: None,
        }
    }

    #[test]
    fn session_fact_request_is_physical_thread_scoped_but_preserves_root_turn_lineage() {
        let directory = tempdir().unwrap();
        let canonical = fs::canonicalize(directory.path()).unwrap();
        let observed_at = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        let tasks = vec![
            task("root", None, Some(canonical)),
            task("child", Some("root"), None),
        ];
        let turns = vec![turn("root", "must never cross the wire", observed_at)];
        let interactions = vec![AgentInteraction {
            kind: AgentInteractionKind::SpawnStarted,
            parent_thread_id: "root".to_owned(),
            parent_turn_id: "turn-1".to_owned(),
            child_thread_id: "child".to_owned(),
            call_id: "spawn-child".to_owned(),
            requested_at: Some(observed_at - Duration::minutes(4)),
            occurred_at: Some(observed_at - Duration::minutes(3)),
            provenance: Provenance::LocalExact,
        }];
        let root_call = usage_call(
            observed_at - Duration::minutes(2),
            Some("native-root-event"),
            "gpt-5.6-luna",
            1_000,
        );
        let mut child_call = usage_call(
            observed_at - Duration::minutes(1),
            Some("native-child-event"),
            "gpt-5.6-luna",
            2_000,
        );
        child_call.thread_id = "child".to_owned();

        let materialized = materialize_session_facts(
            &identity("node-0123456789abcdef0123456789abcdef", &"99".repeat(32)),
            &ThreadId::from_str("child").unwrap(),
            35,
            observed_at,
            &tasks,
            &turns,
            &interactions,
            &[root_call, child_call],
            &[],
        )
        .unwrap();

        assert_eq!(materialized.facts.len(), 1);
        let fact = &materialized.facts[0];
        assert_eq!(fact.event_id.as_str(), "native-child-event");
        assert_eq!(fact.emitting_thread_id.as_str(), "child");
        assert_eq!(
            fact.parent_thread_id.as_ref().map(ThreadId::as_str),
            Some("root")
        );
        assert_eq!(fact.root_session_thread_id.as_str(), "root");
        assert_eq!(fact.root_session_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(fact.emitting_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(fact.metrics.token_usage.total_tokens, 2_000);
    }

    #[test]
    fn bounded_session_fact_materialization_stops_before_growing_past_limits() {
        let directory = tempdir().unwrap();
        let canonical = fs::canonicalize(directory.path()).unwrap();
        let observed_at = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        let tasks = vec![task("root", None, Some(canonical))];
        let turns = vec![turn("root", "private prompt", observed_at)];
        let calls = vec![
            usage_call(
                observed_at - Duration::minutes(2),
                Some("native-event-1"),
                "gpt-5.6-luna",
                1_000,
            ),
            usage_call(
                observed_at - Duration::minutes(1),
                Some("native-event-2"),
                "gpt-5.6-luna",
                2_000,
            ),
        ];
        let observation =
            HistoryObservation::from_sources_with_tasks_turns_and_interactions_and_coverage(
                observed_at,
                &calls,
                &tasks,
                &turns,
                &[],
                &[],
                &[],
                None,
            );
        let identity = identity("node-0123456789abcdef0123456789abcdef", &"88".repeat(32));
        let observation = source_normalized_observation(&identity, &tasks, &observation);

        let mut measured = 0usize;
        let error = materialize_session_facts_from_normalized_observation_bounded(
            &ThreadId::from_str("root").unwrap(),
            35,
            observed_at,
            &observation,
            &calls,
            &[],
            SessionFactMaterializationLimits {
                maximum_records: 1,
                initial_serialized_bytes: 0,
                maximum_record_serialized_bytes: usize::MAX,
                maximum_serialized_bytes: usize::MAX,
            },
            |_| {
                measured += 1;
                Ok(1)
            },
        )
        .unwrap_err();
        assert!(is_session_fact_inventory_limit_error(&error));
        assert_eq!(measured, 1, "second unique fact must fail before measuring");

        let mut measured = 0usize;
        let error = materialize_session_facts_from_normalized_observation_bounded(
            &ThreadId::from_str("root").unwrap(),
            35,
            observed_at,
            &observation,
            &calls,
            &[],
            SessionFactMaterializationLimits {
                maximum_records: 2,
                initial_serialized_bytes: 4,
                maximum_record_serialized_bytes: 4,
                maximum_serialized_bytes: 8,
            },
            |_| {
                measured += 1;
                Ok(5)
            },
        )
        .unwrap_err();
        assert!(is_session_fact_inventory_limit_error(&error));
        assert_eq!(measured, 1, "oversized fact must stop at the first record");
    }

    #[test]
    fn local_digest_sidecar_collapses_root_and_subagent_without_content_or_paths() {
        let directory = tempdir().unwrap();
        let canonical = fs::canonicalize(directory.path()).unwrap();
        let observed_at = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        let tasks = vec![
            task("root", None, Some(canonical)),
            task("child", Some("root"), None),
        ];
        let turns = vec![turn("root", "private root prompt", observed_at)];
        let interactions = vec![AgentInteraction {
            kind: AgentInteractionKind::SpawnStarted,
            parent_thread_id: "root".to_owned(),
            parent_turn_id: "turn-1".to_owned(),
            child_thread_id: "child".to_owned(),
            call_id: "spawn-child".to_owned(),
            requested_at: Some(observed_at - Duration::minutes(4)),
            occurred_at: Some(observed_at - Duration::minutes(3)),
            provenance: Provenance::LocalExact,
        }];
        let root_call = usage_call(
            observed_at - Duration::minutes(2),
            Some("native-root-event"),
            "gpt-5.6-luna",
            1_000,
        );
        let mut child_call = usage_call(
            observed_at - Duration::minutes(1),
            Some("native-child-event"),
            "gpt-5.6-luna",
            2_000,
        );
        child_call.thread_id = "child".to_owned();
        let calls = vec![root_call, child_call];
        let mut observation =
            HistoryObservation::from_sources_with_tasks_turns_and_interactions_and_coverage(
                observed_at,
                &calls,
                &tasks,
                &turns,
                &interactions,
                &[],
                &[],
                None,
            );
        let evidence = materialize_local_session_digest_evidence(
            &calls,
            &observation.half_hour_buckets,
            observed_at,
            true,
        )
        .unwrap();
        assert_eq!(evidence.digest_count(), 2);

        let identity = identity("node-0123456789abcdef0123456789abcdef", &"aa".repeat(32));
        normalize_observation_project_keys(&identity, &tasks, &mut observation);
        let digests = finalize_local_session_digests(&identity, &evidence, &observation).unwrap();
        assert_eq!(digests.len(), 2);
        assert_eq!(digests[0].replica().thread_id().as_str(), "child");
        assert_eq!(digests[1].replica().thread_id().as_str(), "root");
        assert_eq!(digests[0].metrics().token_usage.total_tokens, 2_000);
        assert_eq!(digests[1].metrics().token_usage.total_tokens, 1_000);
        assert_eq!(
            digests[0].observed_project_keys(),
            digests[1].observed_project_keys()
        );
        assert!(
            digests
                .iter()
                .all(|digest| !digest.observed_project_keys().is_empty())
        );
        let debug = format!("{evidence:?}");
        assert!(!debug.contains("private root prompt"));
        assert!(!debug.contains(directory.path().to_string_lossy().as_ref()));
    }

    fn observed_usage(
        observed_at: DateTime<Utc>,
        calls: &[UsageCall],
        tasks: &[TaskRecord],
        coverage_starts_at: DateTime<Utc>,
    ) -> HistoryObservation {
        HistoryObservation::from_sources_with_tasks_and_coverage(
            observed_at,
            calls,
            tasks,
            &[],
            &[],
            Some(coverage_starts_at),
        )
    }

    #[test]
    fn rekeys_root_and_descendant_with_the_same_private_observed_key() {
        let directory = tempdir().unwrap();
        let canonical = fs::canonicalize(directory.path()).unwrap();
        let tasks = vec![
            task("root", None, Some(canonical.clone())),
            task("child", Some("root"), None),
        ];
        let mut observation = observation(vec![group("root", None), group("child", Some("root"))]);
        let identity = identity("node-0123456789abcdef0123456789abcdef", &"11".repeat(32));

        let report = normalize_observation_project_keys(&identity, &tasks, &mut observation);

        assert_eq!(report.groups_rekeyed, 2);
        assert_eq!(report.groups_unresolved, 0);
        assert_eq!(report.observed_project_keys.len(), 1);
        let root = observation.half_hour_buckets[0].project_groups[0]
            .project_id
            .as_deref()
            .unwrap();
        let child = observation.half_hour_buckets[0].project_groups[1]
            .project_id
            .as_deref()
            .unwrap();
        assert_eq!(root, child);
        assert!(root.starts_with("opk-hmac-sha256-v1-"));
        assert!(!root.contains("legacy"));
    }

    #[test]
    fn project_descriptors_export_only_fingerprinted_git_and_relative_workspace_evidence() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        fs::create_dir(&repository).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args([
                    "config",
                    "--local",
                    "remote.origin.url",
                    "https://user:private-token@GitHub.COM/OpenAI/codex.git?secret=value#private",
                ])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        let workspace = repository.join("crates/core");
        fs::create_dir_all(&workspace).unwrap();
        let canonical_workspace = fs::canonicalize(&workspace).unwrap();
        let tasks = vec![task("root", None, Some(canonical_workspace.clone()))];
        let mut normalized_observation = observation(vec![group("root", None)]);
        let identity = identity("node-0123456789abcdef0123456789abcdef", &"12".repeat(32));
        normalize_observation_project_keys(&identity, &tasks, &mut normalized_observation);

        let descriptors = local_project_descriptors(&tasks, &normalized_observation);
        assert_eq!(descriptors.len(), 1);
        let descriptor = &descriptors[0];
        assert!(descriptor.git_evidence.fingerprint().is_some());
        assert_eq!(
            descriptor.git_evidence.repository_relative_workspace_root(),
            Some("crates/core")
        );
        let encoded = serde_json::to_string(&descriptors).unwrap();
        assert!(!encoded.contains("GitHub"));
        assert!(!encoded.contains("private-token"));
        assert!(!encoded.contains("secret=value"));
        assert!(!encoded.contains(canonical_workspace.to_string_lossy().as_ref()));

        let canonical_repository = fs::canonicalize(&repository).unwrap();
        let root_tasks = vec![task("root", None, Some(canonical_repository))];
        let mut root_observation = observation(vec![group("root", None)]);
        normalize_observation_project_keys(&identity, &root_tasks, &mut root_observation);
        let root_descriptors = local_project_descriptors(&root_tasks, &root_observation);
        assert_eq!(
            root_descriptors[0]
                .git_evidence
                .repository_relative_workspace_root(),
            Some(".")
        );
        assert_eq!(
            root_descriptors[0].git_evidence.fingerprint(),
            descriptor.git_evidence.fingerprint()
        );
    }

    #[test]
    fn live_and_aggregate_materialization_share_one_git_collection_cache() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        fs::create_dir(&repository).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args([
                    "config",
                    "--local",
                    "remote.origin.url",
                    "git@github.com:OpenAI/codex.git",
                ])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        let canonical_repository = fs::canonicalize(&repository).unwrap();
        let tasks = vec![task("root", None, Some(canonical_repository))];
        let source_observation = observation(vec![group("root", None)]);
        let captured_at = source_observation.observed_at;
        let identity = identity("node-0123456789abcdef0123456789abcdef", &"13".repeat(32));
        let mut git = GitProjectEvidenceResolver::default();
        git.begin_collection();

        let live = materialize_live_snapshot_with_git_resolver(
            &identity,
            RedactionProfile::PreviewEnabled,
            &tasks,
            &[],
            captured_at,
            &mut git,
        )
        .unwrap();
        assert!(
            live.project_descriptors[0]
                .git_evidence
                .fingerprint()
                .is_some()
        );

        // If aggregate materialization created a second resolver or spawned
        // Git again, the repository would no longer be discoverable here.
        fs::rename(repository.join(".git"), repository.join(".git-hidden")).unwrap();
        let aggregate = materialize_source_observation_with_git_resolver(
            &identity,
            RedactionProfile::PreviewEnabled,
            &tasks,
            &[],
            source_observation,
            true,
            &mut git,
        )
        .unwrap();
        assert_eq!(
            aggregate.project_descriptors[0].git_evidence.fingerprint(),
            live.project_descriptors[0].git_evidence.fingerprint()
        );
    }

    #[test]
    fn source_secret_and_node_identity_both_scope_project_keys() {
        let directory = tempdir().unwrap();
        let canonical = fs::canonicalize(directory.path()).unwrap();
        let tasks = vec![task("root", None, Some(canonical))];
        let mut first = observation(vec![group("root", None)]);
        let mut second = first.clone();

        normalize_observation_project_keys(
            &identity("node-0123456789abcdef0123456789abcdef", &"11".repeat(32)),
            &tasks,
            &mut first,
        );
        normalize_observation_project_keys(
            &identity("node-fedcba9876543210fedcba9876543210", &"22".repeat(32)),
            &tasks,
            &mut second,
        );

        assert_ne!(
            first.half_hour_buckets[0].project_groups[0].project_id,
            second.half_hour_buckets[0].project_groups[0].project_id
        );
    }

    #[test]
    fn unresolved_or_ambiguous_lineage_never_exports_a_legacy_project_id() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("does-not-exist");
        let tasks = vec![
            task("missing", None, Some(missing)),
            task("cycle-a", Some("cycle-b"), None),
            task("cycle-b", Some("cycle-a"), None),
        ];
        let mut observation = observation(vec![
            group("missing", None),
            group("cycle-a", Some("cycle-b")),
            group("orphan", None),
        ]);

        let report = normalize_observation_project_keys(
            &identity("node-0123456789abcdef0123456789abcdef", &"33".repeat(32)),
            &tasks,
            &mut observation,
        );

        assert_eq!(report.groups_rekeyed, 0);
        assert_eq!(report.groups_unresolved, 3);
        assert!(report.observed_project_keys.is_empty());
        assert!(
            observation.half_hour_buckets[0]
                .project_groups
                .iter()
                .all(|group| group.project_id.is_none())
        );
    }

    #[test]
    fn conflicting_parent_claims_do_not_inherit_either_project() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let tasks = vec![
            task("first", None, Some(fs::canonicalize(first.path()).unwrap())),
            task(
                "second",
                None,
                Some(fs::canonicalize(second.path()).unwrap()),
            ),
            task("child", Some("first"), None),
        ];
        let mut child = group("child", Some("second"));
        child.project_label = Some("child".to_owned());
        let mut observation = observation(vec![child]);

        let report = normalize_observation_project_keys(
            &identity("node-0123456789abcdef0123456789abcdef", &"44".repeat(32)),
            &tasks,
            &mut observation,
        );

        assert_eq!(report.groups_unresolved, 1);
        assert!(
            observation.half_hour_buckets[0].project_groups[0]
                .project_id
                .is_none()
        );
    }

    #[test]
    fn copied_session_fingerprint_is_source_independent_but_project_keys_are_not() {
        let directory = tempdir().unwrap();
        let canonical = fs::canonicalize(directory.path()).unwrap();
        let tasks = vec![task("root", None, Some(canonical))];
        let day = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap();
        let calls = vec![
            usage_call(
                day + Duration::hours(1),
                Some("native-event-1"),
                "gpt-5.6-luna",
                1_000,
            ),
            usage_call(
                day + Duration::hours(2),
                Some("native-event-2"),
                "gpt-5.6-luna",
                2_000,
            ),
        ];
        let observed_at = day + Duration::hours(12);
        let observation = observed_usage(observed_at, &calls, &tasks, day);

        let first = materialize_source_observation(
            &identity("node-0123456789abcdef0123456789abcdef", &"55".repeat(32)),
            RedactionProfile::Redacted,
            &tasks,
            &calls,
            observation.clone(),
            true,
        )
        .unwrap();
        let second = materialize_source_observation(
            &identity("node-fedcba9876543210fedcba9876543210", &"66".repeat(32)),
            RedactionProfile::Redacted,
            &tasks,
            &calls.iter().cloned().rev().collect::<Vec<_>>(),
            observation,
            true,
        )
        .unwrap();

        assert_eq!(first.session_digests.len(), 1);
        assert_eq!(second.session_digests.len(), 1);
        assert_eq!(
            first.session_digests[0].fingerprint,
            second.session_digests[0].fingerprint
        );
        assert_eq!(
            first.session_digests[0].metrics,
            second.session_digests[0].metrics
        );
        assert_ne!(
            first.session_digests[0].observed_project_keys,
            second.session_digests[0].observed_project_keys
        );
    }

    #[test]
    fn duplicate_events_dedupe_and_conflicts_are_order_independent() {
        let day = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap();
        let base = usage_call(
            day + Duration::hours(1),
            Some("native-event-1"),
            "gpt-5.6-luna",
            1_000,
        );
        let buckets = observed_usage(
            day + Duration::hours(2),
            std::slice::from_ref(&base),
            &[],
            day,
        )
        .half_hour_buckets;
        let (deduped, _, _) = materialize_session_digests(
            &[base.clone(), base.clone()],
            &buckets,
            day + Duration::hours(2),
            true,
        )
        .unwrap();
        assert_eq!(deduped[0].event_count, 1);
        assert_eq!(deduped[0].metrics.call_count, 1);
        assert_eq!(deduped[0].metrics.token_usage.total_tokens, 1_000);

        let mut conflicting = base.clone();
        conflicting.tokens.input_tokens = 1_800;
        conflicting.tokens.cached_input_tokens = 450;
        conflicting.tokens.output_tokens = 200;
        conflicting.tokens.reasoning_output_tokens = 100;
        conflicting.tokens.total_tokens = 2_000;
        let forward = materialize_session_digests(
            &[base.clone(), conflicting.clone()],
            &buckets,
            day + Duration::hours(2),
            true,
        )
        .unwrap()
        .0;
        let reverse = materialize_session_digests(
            &[conflicting, base],
            &buckets,
            day + Duration::hours(2),
            true,
        )
        .unwrap()
        .0;
        assert_eq!(forward, reverse);
        assert_eq!(forward[0].event_count, 1);
        assert!(!forward[0].exact_event_identity);
        assert!(
            forward[0]
                .metrics
                .partial_reasons
                .iter()
                .any(|reason| reason == CONFLICTING_EVENT_REASON)
        );
    }

    #[test]
    fn digest_coverage_distinguishes_open_closed_and_incomplete_scans() {
        let directory = tempdir().unwrap();
        let tasks = vec![task(
            "root",
            None,
            Some(fs::canonicalize(directory.path()).unwrap()),
        )];
        let day = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap();
        let call = usage_call(
            day + Duration::hours(1),
            Some("native-event-1"),
            "gpt-5.6-luna",
            1_000,
        );
        let current_observed_at = day + Duration::hours(12) + Duration::minutes(5);
        let current_observation = observed_usage(
            current_observed_at,
            std::slice::from_ref(&call),
            &tasks,
            day,
        );
        let current = materialize_session_digests(
            std::slice::from_ref(&call),
            &current_observation.half_hour_buckets,
            current_observed_at,
            true,
        )
        .unwrap()
        .0;
        assert!(!current[0].coverage_complete);
        assert_eq!(current[0].covered_through, current_observed_at);
        assert!(
            current[0]
                .metrics
                .partial_reasons
                .iter()
                .any(|reason| reason == OPEN_RANGE_REASON)
        );

        let closed_observed_at = day + Duration::days(1) + Duration::hours(1);
        let closed_observation =
            observed_usage(closed_observed_at, std::slice::from_ref(&call), &tasks, day);
        let closed = materialize_session_digests(
            std::slice::from_ref(&call),
            &closed_observation.half_hour_buckets,
            closed_observed_at,
            true,
        )
        .unwrap()
        .0;
        assert!(closed[0].coverage_complete);
        assert_eq!(closed[0].covered_through, day + Duration::days(1));

        let incomplete = materialize_session_digests(
            &[call],
            &closed_observation.half_hour_buckets,
            closed_observed_at,
            false,
        )
        .unwrap()
        .0;
        assert!(!incomplete[0].coverage_complete);
        assert!(
            incomplete[0]
                .metrics
                .partial_reasons
                .iter()
                .any(|reason| reason == INCOMPLETE_SCAN_REASON)
        );
    }

    #[test]
    fn missing_event_identity_is_stable_and_explicitly_partial() {
        let day = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap();
        let call = usage_call(day + Duration::hours(1), None, "gpt-5.6-luna", 1_000);
        let observation = observed_usage(
            day + Duration::hours(2),
            std::slice::from_ref(&call),
            &[],
            day,
        );
        let first = materialize_session_digests(
            std::slice::from_ref(&call),
            &observation.half_hour_buckets,
            day + Duration::hours(2),
            true,
        )
        .unwrap();
        let second = materialize_session_digests(
            &[call],
            &observation.half_hour_buckets,
            day + Duration::hours(2),
            true,
        )
        .unwrap();

        assert_eq!(first.0[0].fingerprint, second.0[0].fingerprint);
        assert_eq!(first.1, 1);
        assert!(!first.0[0].exact_event_identity);
        assert!(
            first.0[0]
                .metrics
                .partial_reasons
                .iter()
                .any(|reason| reason == MISSING_EVENT_REASON)
        );
    }

    #[test]
    fn spark_api_population_remains_independent_from_codex_tokens() {
        let directory = tempdir().unwrap();
        let tasks = vec![task(
            "root",
            None,
            Some(fs::canonicalize(directory.path()).unwrap()),
        )];
        let day = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap();
        let call = usage_call(
            day + Duration::hours(1),
            Some("native-spark-event"),
            "gpt-5.3-codex-spark",
            1_000,
        );
        let observed_at = day + Duration::hours(2);
        let observation = observed_usage(observed_at, std::slice::from_ref(&call), &tasks, day);
        let materialized = materialize_source_observation(
            &identity("node-0123456789abcdef0123456789abcdef", &"77".repeat(32)),
            RedactionProfile::Redacted,
            &tasks,
            &[call],
            observation,
            true,
        )
        .unwrap();
        let call_bucket = materialized
            .buckets
            .iter()
            .find(|bucket| bucket.starts_at == day + Duration::hours(1))
            .unwrap();

        assert_eq!(call_bucket.token_usage.total_tokens, 0);
        assert_eq!(call_bucket.api_equivalent_cost.observed_tokens, 1_000);
        assert_eq!(call_bucket.call_count, 0);
        assert_eq!(
            materialized.session_digests[0]
                .metrics
                .token_usage
                .total_tokens,
            0
        );
        assert_eq!(
            materialized.session_digests[0]
                .metrics
                .api_equivalent_cost
                .observed_tokens,
            1_000
        );
    }

    #[test]
    fn live_snapshot_redacts_content_and_never_exports_absolute_paths() {
        let directory = tempdir().unwrap();
        let captured_at = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        let mut root = task(
            "root",
            None,
            Some(fs::canonicalize(directory.path()).unwrap()),
        );
        root.title = "private prompt title".to_owned();
        root.created_at = Some(captured_at - Duration::hours(1));
        root.updated_at = Some(captured_at - Duration::minutes(1));
        let identity = identity("node-0123456789abcdef0123456789abcdef", &"88".repeat(32));
        let turn = turn("root", "private user message", captured_at);

        let redacted = materialize_live_snapshot(
            &identity,
            RedactionProfile::Redacted,
            std::slice::from_ref(&root),
            std::slice::from_ref(&turn),
            captured_at,
        )
        .unwrap();
        assert!(redacted.snapshot.tasks[0].title_preview.is_none());
        assert!(redacted.snapshot.turns[0].message_preview.is_none());
        assert!(
            redacted.snapshot.tasks[0]
                .observed_project_key
                .as_ref()
                .is_some_and(|key| key.as_str().starts_with("opk-hmac-sha256-v1-"))
        );
        let encoded =
            serde_json::to_string(&(&redacted.snapshot, &redacted.project_descriptors)).unwrap();
        assert!(!encoded.contains(&directory.path().to_string_lossy().into_owned()));
        assert!(!encoded.contains("private prompt"));
        assert!(!encoded.contains("private user"));

        let preview = materialize_live_snapshot(
            &identity,
            RedactionProfile::PreviewEnabled,
            &[root],
            &[turn],
            captured_at,
        )
        .unwrap();
        assert_eq!(
            preview.snapshot.tasks[0].title_preview.as_deref(),
            Some("private prompt title")
        );
        assert_eq!(
            preview.snapshot.turns[0].message_preview.as_deref(),
            Some("private user message")
        );
    }
}
