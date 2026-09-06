//! Ownership-selected, source-aware history reads.
//!
//! This module is deliberately read-only. It resolves the durable ownership
//! manifest before every query and never combines v1 and v2 data in one
//! result. During cutover an exact manifest change causes a bounded retry, so
//! a `Migrating -> V2Active` transition cannot return an accidental hybrid.

mod aggregation;
mod reconciliation;

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};

#[cfg(test)]
use crate::domain::TokenUsage;
use crate::history::{HistoryData, HistoryStore, LocalHalfHourBucket, WeeklyLocalPoint};
#[cfg(test)]
use crate::history::{LocalProjectUsageGroup, LocalUsageGroup, QuotaPoint};
use crate::history_ownership::{
    HistoryOwnershipManifest, HistoryOwnershipState, HistoryOwnershipStore, OwnershipManifestStatus,
};
use crate::project_mapping::PROJECT_MAPPING_REGISTRATION_FAILED_WARNING;
use crate::project_mapping::{ProjectMappingProjection, ProjectMappingStore};
#[cfg(test)]
use crate::source_history::UsageEventFact;
use crate::source_history::{
    ActiveFactSet, RedactionProfile, SourceBucketChange, SourceHistoryReadBudget,
    SourceHistoryRemoteActiveRef, SourceHistoryStore, SourceKind, SourceMetadata,
    SourceSessionDigest, SourceSessionDigestChange,
};
use crate::source_identity::NodeId;
#[cfg(test)]
use crate::source_model::ObservedProjectKey;
use crate::source_model::ThreadId;
use crate::trace::{TraceFields, TraceOutcome, process_trace_log};

#[cfg(test)]
use aggregation::{
    MAX_WEEKLY_RESET_CYCLES, WeeklyAggregationWork, WeeklySourceCursor, aggregate_source_buckets,
    aggregate_source_weekly_points_with_work, assigned_canonical_reset,
    source_weekly_cumulative_at,
};
use aggregation::{
    aggregate_source_buckets_with_logical_threads, aggregate_source_weekly_points,
    canonical_weekly_resets,
};
use reconciliation::{LogicalReplicaReport, resolve_logical_replicas};
#[cfg(test)]
use reconciliation::{
    ReplicaBucketIndexWork, ReplicaParticipant, add_fact_group, build_source_bucket_indices,
    digest_project_attribution_conflicts, replace_weekly_baselines_with_cycle_markers,
};

const LEGACY_HISTORY_DIRECTORY: &str = "history-v1";
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;
const RESET_DRIFT_SECONDS: i64 = 120;
const QUERY_EVIDENCE_LOOKBACK_DAYS: i64 = 7;
const MAX_STABLE_QUERY_ATTEMPTS: usize = 4;

pub const CROSS_SOURCE_DUPLICATE_WARNING: &str = "cross_source_duplicate_possible";
pub const REDACTED_QUERY_SKIPPED_PREVIEW_SOURCE_WARNING: &str =
    "redacted_query_skipped_preview_source";
pub const SOURCE_SELECTION_UNAVAILABLE_WARNING: &str = "source_selection_unavailable";
pub const SOURCE_SELECTION_EXCLUDED_WARNING: &str = "source_selection_excluded_from_aggregates";
pub const PROJECT_MAPPING_PARTIAL_WARNING: &str = "project_mapping_partial";
pub const PROJECT_MAPPING_UNAVAILABLE_WARNING: &str = "project_mapping_unavailable";
pub const DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING: &str = "duplicate_session_dedup_unavailable";
pub const DUPLICATE_SESSION_FACT_CONFLICT_WARNING: &str = "duplicate_session_fact_conflict";
pub const DUPLICATE_SESSION_PROJECT_CONFLICT_WARNING: &str = "replica_project_conflict";
const DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL: &str = "duplicate_session_model_breakdown_partial";
const DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND: &str =
    "duplicate_session_project_breakdown_lower_bound";
const DUPLICATE_SESSION_WEEKLY_REBUILT_FROM_BUCKETS: &str =
    "duplicate_session_weekly_rebuilt_from_buckets";

/// Physical source projection requested by one history reader.
///
/// `Local` carries the runtime's exact stable node identity. The selected
/// query API also receives the bound local identity and rejects a mismatched
/// value, so a stale or rotated local identity cannot accidentally select a
/// different source that merely has `SourceKind::Local` metadata.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum HistorySourceSelection {
    AllIncluded,
    Local(NodeId),
    Remote(NodeId),
}

/// User-facing source selector that can be resolved after the runtime binds
/// its stable local node identity.
///
/// This is intentionally distinct from [`HistorySourceSelection`]: the CLI
/// spelling `local` must not persist or guess a node ID before the exact
/// history runtime has been opened.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub enum HistorySourceSelector {
    #[default]
    AllIncluded,
    Local,
    Remote(NodeId),
}

impl HistorySourceSelector {
    pub fn resolve(&self, local_source_id: &NodeId) -> HistorySourceSelection {
        match self {
            Self::AllIncluded => HistorySourceSelection::AllIncluded,
            Self::Local => HistorySourceSelection::Local(local_source_id.clone()),
            Self::Remote(source_id) => HistorySourceSelection::Remote(source_id.clone()),
        }
    }
}

impl fmt::Display for HistorySourceSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllIncluded => formatter.write_str("all"),
            Self::Local => formatter.write_str("local"),
            Self::Remote(source_id) => source_id.fmt(formatter),
        }
    }
}

impl FromStr for HistorySourceSelector {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "all" => Ok(Self::AllIncluded),
            "local" => Ok(Self::Local),
            _ => value
                .parse::<NodeId>()
                .map(Self::Remote)
                .map_err(|error| format!("source must be 'all', 'local', or a node ID: {error}")),
        }
    }
}

impl HistorySourceSelection {
    fn source_id(&self) -> Option<&NodeId> {
        match self {
            Self::AllIncluded => None,
            Self::Local(source_id) | Self::Remote(source_id) => Some(source_id),
        }
    }

    fn expected_kind(&self) -> Option<SourceKind> {
        match self {
            Self::AllIncluded => None,
            Self::Local(_) => Some(SourceKind::Local),
            Self::Remote(_) => Some(SourceKind::Ssh),
        }
    }
}

/// Why an exact source projection could not be applied safely.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum HistorySourceUnavailableReason {
    NotFound,
    RedactionIncompatible,
    KindMismatch,
    LocalIdentityMismatch,
    UnsupportedByLegacy,
}

impl HistorySourceUnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::RedactionIncompatible => "redaction_incompatible",
            Self::KindMismatch => "kind_mismatch",
            Self::LocalIdentityMismatch => "local_identity_mismatch",
            Self::UnsupportedByLegacy => "unsupported_by_legacy",
        }
    }
}

/// Whether the requested source projection was applied to this snapshot.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum HistorySourceSelectionStatus {
    Applied,
    /// The exact source was read successfully, but it remains excluded from
    /// `AllIncluded` aggregation and logical-replica authority decisions.
    AppliedExcludedFromAggregates,
    Unavailable(HistorySourceUnavailableReason),
}

impl HistorySourceSelectionStatus {
    pub const fn is_applied(self) -> bool {
        matches!(self, Self::Applied | Self::AppliedExcludedFromAggregates)
    }
}

/// Backend that durably owned the namespace for this query result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnifiedHistoryBackend {
    V1,
    V2,
}

/// One ownership-consistent history projection.
#[derive(Clone, Debug, PartialEq)]
pub struct UnifiedHistorySnapshot {
    pub history: HistoryData,
    pub backend: UnifiedHistoryBackend,
    pub ownership_epoch: u64,
    pub source_selection: HistorySourceSelection,
    pub source_selection_status: HistorySourceSelectionStatus,
    /// V2 source identities read into this projection, in stable lexical
    /// order. An exact source remains here even when it is excluded from
    /// `AllIncluded`; this is empty for v1 because legacy history has no
    /// source dimension.
    pub included_sources: Vec<NodeId>,
    /// Included sources rejected before data access because a redacted query
    /// must never open their preview-enabled namespace.
    pub redaction_skipped_sources: Vec<NodeId>,
}

/// Loads history from exactly one backend selected by durable ownership.
///
/// `legacy` and `source_history` must be the exact sibling stores of
/// `ownership`. The mutable legacy reference exists only because v1 owns its
/// read cache; this function never writes through it.
pub fn load_unified_history_since(
    ownership: &HistoryOwnershipStore,
    legacy: &mut HistoryStore,
    source_history: &SourceHistoryStore,
    since: DateTime<Utc>,
) -> io::Result<UnifiedHistorySnapshot> {
    let mapping = LoadedProjectMappingProjection::default();
    load_unified_history_since_inner(
        ownership,
        legacy,
        source_history,
        &mapping,
        None,
        &HistorySourceSelection::AllIncluded,
        since,
    )
}

/// Runtime-facing variant that loads a detached mapping projection exactly
/// once for this query. A missing or unreadable mapping never causes an
/// identity guess: project rows stay source-scoped and carry an explicit
/// partial diagnostic.
pub fn load_unified_history_since_with_project_mapping_store(
    ownership: &HistoryOwnershipStore,
    legacy: &mut HistoryStore,
    source_history: &SourceHistoryStore,
    project_mapping_store: &ProjectMappingStore,
    since: DateTime<Utc>,
) -> io::Result<UnifiedHistorySnapshot> {
    let mapping = load_project_mapping_projection(project_mapping_store);
    load_unified_history_since_inner(
        ownership,
        legacy,
        source_history,
        &mapping,
        None,
        &HistorySourceSelection::AllIncluded,
        since,
    )
}

/// Loads one ownership-consistent projection for a specific physical source.
///
/// `bound_local_source_id` must be the stable identity owned by the calling
/// runtime. It fences `Local` selections against stale or rotated identities.
/// An unavailable exact selection returns global quota data plus an explicit
/// status and warning, never an all-source or local-usage fallback.
pub fn load_unified_history_since_selected(
    ownership: &HistoryOwnershipStore,
    legacy: &mut HistoryStore,
    source_history: &SourceHistoryStore,
    bound_local_source_id: &NodeId,
    selection: &HistorySourceSelection,
    since: DateTime<Utc>,
) -> io::Result<UnifiedHistorySnapshot> {
    let mapping = LoadedProjectMappingProjection::default();
    load_unified_history_since_inner(
        ownership,
        legacy,
        source_history,
        &mapping,
        Some(bound_local_source_id),
        selection,
        since,
    )
}

pub fn load_unified_history_since_selected_with_project_mapping_store(
    ownership: &HistoryOwnershipStore,
    legacy: &mut HistoryStore,
    source_history: &SourceHistoryStore,
    project_mapping_store: &ProjectMappingStore,
    bound_local_source_id: &NodeId,
    selection: &HistorySourceSelection,
    since: DateTime<Utc>,
) -> io::Result<UnifiedHistorySnapshot> {
    let mapping = load_project_mapping_projection(project_mapping_store);
    load_unified_history_since_inner(
        ownership,
        legacy,
        source_history,
        &mapping,
        Some(bound_local_source_id),
        selection,
        since,
    )
}

#[derive(Default)]
struct LoadedProjectMappingProjection {
    projection: ProjectMappingProjection,
    unavailable: bool,
}

fn load_project_mapping_projection(store: &ProjectMappingStore) -> LoadedProjectMappingProjection {
    match store.load() {
        Ok(mappings) => LoadedProjectMappingProjection {
            projection: mappings.projection(),
            unavailable: false,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => LoadedProjectMappingProjection {
            projection: ProjectMappingProjection::default(),
            unavailable: false,
        },
        Err(_) => LoadedProjectMappingProjection {
            projection: ProjectMappingProjection::default(),
            unavailable: true,
        },
    }
}

fn load_unified_history_since_inner(
    ownership: &HistoryOwnershipStore,
    legacy: &mut HistoryStore,
    source_history: &SourceHistoryStore,
    project_mapping: &LoadedProjectMappingProjection,
    bound_local_source_id: Option<&NodeId>,
    selection: &HistorySourceSelection,
    since: DateTime<Utc>,
) -> io::Result<UnifiedHistorySnapshot> {
    validate_store_bindings(ownership, legacy, source_history)?;
    // One allowance covers the complete logical query, including bounded
    // retries when ownership or source metadata changes during the read.
    let mut source_read_budget = SourceHistoryReadBudget::for_query();

    for _ in 0..MAX_STABLE_QUERY_ATTEMPTS {
        let before = initialized_manifest(ownership)?;
        let local_identity_mismatch = matches!(
            selection,
            HistorySourceSelection::Local(source_id)
                if bound_local_source_id != Some(source_id)
        );
        let (
            history,
            backend,
            included_sources,
            redaction_skipped_sources,
            source_selection_status,
        ) = match before.state() {
            HistoryOwnershipState::V1Active | HistoryOwnershipState::Migrating => {
                let legacy_history = legacy.load_since(since);
                let status = if local_identity_mismatch {
                    HistorySourceSelectionStatus::Unavailable(
                        HistorySourceUnavailableReason::LocalIdentityMismatch,
                    )
                } else if matches!(selection, HistorySourceSelection::Remote(_)) {
                    HistorySourceSelectionStatus::Unavailable(
                        HistorySourceUnavailableReason::UnsupportedByLegacy,
                    )
                } else {
                    HistorySourceSelectionStatus::Applied
                };
                let history = match status {
                    HistorySourceSelectionStatus::Applied
                    | HistorySourceSelectionStatus::AppliedExcludedFromAggregates => legacy_history,
                    HistorySourceSelectionStatus::Unavailable(reason) => {
                        unavailable_v1_history(legacy_history, selection, reason)
                    }
                };
                (
                    history,
                    UnifiedHistoryBackend::V1,
                    Vec::new(),
                    Vec::new(),
                    status,
                )
            }
            HistoryOwnershipState::V2Active => {
                let Some(v2) = load_v2_history_since(
                    &V2HistoryQuery {
                        query_redaction: ownership.redaction_profile(),
                        ownership_epoch: before.epoch(),
                        store: source_history,
                        project_mapping,
                        bound_local_source_id,
                        selection,
                        since,
                    },
                    &mut source_read_budget,
                )?
                else {
                    // Source policy changed while it was being read. A
                    // retry starts from a new complete metadata snapshot.
                    continue;
                };
                (
                    v2.history,
                    UnifiedHistoryBackend::V2,
                    v2.included_sources,
                    v2.redaction_skipped_sources,
                    v2.source_selection_status,
                )
            }
        };

        let after = initialized_manifest(ownership)?;
        if before == after {
            return Ok(UnifiedHistorySnapshot {
                history,
                backend,
                ownership_epoch: after.epoch(),
                source_selection: selection.clone(),
                source_selection_status,
                included_sources,
                redaction_skipped_sources,
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "history ownership or source policy changed repeatedly during the query",
    ))
}

fn unavailable_v1_history(
    legacy_history: HistoryData,
    selection: &HistorySourceSelection,
    reason: HistorySourceUnavailableReason,
) -> HistoryData {
    let mut history = HistoryData {
        // Legacy quota observations are account-global even though v1 stores
        // them beside local usage. They remain valid in an unavailable remote
        // projection; local buckets and weekly points must not cross over.
        quota_points: legacy_history.quota_points,
        read_only: legacy_history.read_only,
        ..HistoryData::default()
    };
    history
        .warnings
        .push(source_selection_unavailable_warning(selection, reason));
    history
}

fn source_selection_unavailable_warning(
    selection: &HistorySourceSelection,
    reason: HistorySourceUnavailableReason,
) -> String {
    let source_id = selection.source_id().map(NodeId::as_str).unwrap_or("all");
    format!(
        "{SOURCE_SELECTION_UNAVAILABLE_WARNING}:{}:{source_id}",
        reason.as_str()
    )
}

fn initialized_manifest(ownership: &HistoryOwnershipStore) -> io::Result<HistoryOwnershipManifest> {
    match ownership.load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest) => Ok(manifest),
        OwnershipManifestStatus::Uninitialized => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "history ownership is uninitialized; initialize the runtime before querying",
        )),
    }
}

fn validate_store_bindings(
    ownership: &HistoryOwnershipStore,
    legacy: &HistoryStore,
    source_history: &SourceHistoryStore,
) -> io::Result<()> {
    let expected_legacy_root = ownership.state_root().join(LEGACY_HISTORY_DIRECTORY);
    let expected_redacted = ownership.redaction_profile() == RedactionProfile::Redacted;
    let expected_namespace = if expected_redacted {
        format!("{}-redacted", ownership.profile_id())
    } else {
        ownership.profile_id().as_str().to_owned()
    };
    if legacy.history_root() != Some(expected_legacy_root.as_path())
        || legacy.namespace() != expected_namespace
        || legacy.redact_content_enabled() != expected_redacted
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "legacy history query store does not match ownership",
        ));
    }
    if source_history.state_root() != ownership.state_root()
        || source_history.profile_id() != ownership.profile_id()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source history query store does not match ownership",
        ));
    }
    Ok(())
}

struct V2HistoryRead {
    history: HistoryData,
    included_sources: Vec<NodeId>,
    redaction_skipped_sources: Vec<NodeId>,
    source_selection_status: HistorySourceSelectionStatus,
}

#[derive(Clone)]
struct SourceSlice {
    metadata: SourceMetadata,
    buckets: Vec<LocalHalfHourBucket>,
    weekly_local_points: Vec<WeeklyLocalPoint>,
}

#[derive(Clone)]
struct SourceReplicaEvidence {
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    digests: Vec<SourceSessionDigest>,
    active_remote_ref: Option<SourceHistoryRemoteActiveRef>,
    active_facts: BTreeMap<ThreadId, ActiveFactSet>,
}

#[derive(Clone, Copy)]
struct V2HistoryQuery<'a> {
    query_redaction: RedactionProfile,
    ownership_epoch: u64,
    store: &'a SourceHistoryStore,
    project_mapping: &'a LoadedProjectMappingProjection,
    bound_local_source_id: Option<&'a NodeId>,
    selection: &'a HistorySourceSelection,
    since: DateTime<Utc>,
}

/// Returns `None` when source policy changed during the read.
fn load_v2_history_since(
    query: &V2HistoryQuery<'_>,
    read_budget: &mut SourceHistoryReadBudget,
) -> io::Result<Option<V2HistoryRead>> {
    let trace = process_trace_log().span_with("history.v2.query", || {
        TraceFields::new().label(
            "sourceScope",
            match query.selection {
                HistorySourceSelection::AllIncluded => "all",
                HistorySourceSelection::Local(_) => "local",
                HistorySourceSelection::Remote(_) => "remote",
            },
        )
    });
    let result = load_v2_history_since_inner(query, read_budget);
    match &result {
        Ok(Some(read)) => trace.finish(
            TraceOutcome::Ok,
            TraceFields::new()
                .usize("sourceCount", read.included_sources.len())
                .usize("quotaPointCount", read.history.quota_points.len())
                .usize("bucketCount", read.history.half_hour_buckets.len())
                .usize("weeklyPointCount", read.history.weekly_local_points.len()),
        ),
        Ok(None) => trace.finish(
            TraceOutcome::Partial,
            TraceFields::new().label("reason", "revision_changed"),
        ),
        Err(_) => trace.finish(TraceOutcome::Error, TraceFields::new()),
    }
    result
}

fn load_v2_history_since_inner(
    query: &V2HistoryQuery<'_>,
    read_budget: &mut SourceHistoryReadBudget,
) -> io::Result<Option<V2HistoryRead>> {
    let V2HistoryQuery {
        query_redaction,
        ownership_epoch,
        store,
        project_mapping,
        bound_local_source_id,
        selection,
        since,
    } = *query;
    let evidence_since = since
        .checked_sub_signed(Duration::days(QUERY_EVIDENCE_LOOKBACK_DAYS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let metadata_trace = process_trace_log().span(
        "history.v2.metadata_load",
        TraceFields::new().label("phase", "before"),
    );
    let metadata_result = store.list_source_metadata();
    match &metadata_result {
        Ok(metadata) => metadata_trace.finish(
            TraceOutcome::Ok,
            TraceFields::new().usize("sourceCount", metadata.len()),
        ),
        Err(_) => metadata_trace.finish(TraceOutcome::Error, TraceFields::new()),
    }
    let mut metadata_before = metadata_result?;
    metadata_before
        .sort_by(|left, right| left.source_id().as_str().cmp(right.source_id().as_str()));
    // Account quota is global and intentionally loaded once, independently
    // of how many local or SSH sources participate.
    let account_trace = process_trace_log().span("history.v2.account_load", TraceFields::new());
    let account_result = store.load_account_since_with_budget(evidence_since, read_budget);
    match &account_result {
        Ok(account) => account_trace.finish(
            TraceOutcome::Ok,
            TraceFields::new().usize("recordCount", account.quota_points.len()),
        ),
        Err(_) => account_trace.finish(TraceOutcome::Error, TraceFields::new()),
    }
    let account = account_result?;
    let mut slices = Vec::new();
    let mut replica_evidence = Vec::new();
    let mut included_sources = Vec::new();
    let mut redaction_skipped_sources = Vec::new();
    let mut source_selection_status = HistorySourceSelectionStatus::Applied;
    let detect_replicas = matches!(selection, HistorySourceSelection::AllIncluded);

    let selected_metadata = if let HistorySourceSelection::Local(source_id) = selection
        && bound_local_source_id != Some(source_id)
    {
        source_selection_status = HistorySourceSelectionStatus::Unavailable(
            HistorySourceUnavailableReason::LocalIdentityMismatch,
        );
        Vec::new()
    } else if let Some(source_id) = selection.source_id() {
        match metadata_before
            .iter()
            .find(|metadata| metadata.source_id() == source_id)
        {
            None => {
                source_selection_status = HistorySourceSelectionStatus::Unavailable(
                    HistorySourceUnavailableReason::NotFound,
                );
                Vec::new()
            }
            Some(metadata) if Some(metadata.kind()) != selection.expected_kind() => {
                source_selection_status = HistorySourceSelectionStatus::Unavailable(
                    HistorySourceUnavailableReason::KindMismatch,
                );
                Vec::new()
            }
            Some(metadata) if !metadata.include_in_aggregates() => {
                source_selection_status =
                    HistorySourceSelectionStatus::AppliedExcludedFromAggregates;
                vec![metadata]
            }
            Some(metadata)
                if query_redaction == RedactionProfile::Redacted
                    && metadata.aggregate_redaction_profile()
                        == RedactionProfile::PreviewEnabled =>
            {
                redaction_skipped_sources.push(metadata.source_id().clone());
                source_selection_status = HistorySourceSelectionStatus::Unavailable(
                    HistorySourceUnavailableReason::RedactionIncompatible,
                );
                Vec::new()
            }
            Some(metadata) => vec![metadata],
        }
    } else {
        metadata_before
            .iter()
            .filter(|metadata| metadata.include_in_aggregates())
            .collect()
    };

    let local_source_count = selected_metadata
        .iter()
        .filter(|metadata| metadata.kind() == SourceKind::Local)
        .count();
    let remote_source_count = selected_metadata
        .iter()
        .filter(|metadata| metadata.kind() == SourceKind::Ssh)
        .count();
    let sources_trace = process_trace_log().span(
        "history.v2.source_families_load",
        TraceFields::new()
            .usize("localSourceCount", local_source_count)
            .usize("remoteSourceCount", remote_source_count),
    );
    let sources_result = (|| -> io::Result<Option<()>> {
        for metadata in selected_metadata {
            let source_redaction = metadata.aggregate_redaction_profile();
            if query_redaction == RedactionProfile::Redacted
                && source_redaction == RedactionProfile::PreviewEnabled
            {
                // Do not even open the preview namespace from a redacted query.
                redaction_skipped_sources.push(metadata.source_id().clone());
                continue;
            }

            let (buckets, weekly_local_points, digest_records, active_remote_ref) =
                match metadata.kind() {
                    SourceKind::Local => {
                        let snapshot = store.load_local_observation_snapshot_since_with_budget(
                            metadata.source_id(),
                            source_redaction,
                            evidence_since,
                            detect_replicas,
                            read_budget,
                        )?;
                        if snapshot.source != *metadata {
                            return Ok(None);
                        }
                        (
                            snapshot.buckets,
                            snapshot.weekly_local_points,
                            snapshot.session_digest_records,
                            None,
                        )
                    }
                    SourceKind::Ssh => {
                        // One combined snapshot call per remote source. It holds the
                        // remote active-generation lock across bucket and digest
                        // families, so a generation switch cannot splice them. The
                        // current exporter has no remote weekly wire family; weekly
                        // cumulative points are derived from these source buckets.
                        let snapshot = store.load_remote_history_snapshot_since_with_budget(
                            metadata.source_id(),
                            source_redaction,
                            evidence_since,
                            read_budget,
                        )?;
                        let buckets = snapshot
                            .bucket_records
                            .iter()
                            .filter_map(|record| match record.change() {
                                SourceBucketChange::Upsert(bucket) => Some((**bucket).clone()),
                                SourceBucketChange::Tombstone => None,
                            })
                            .collect();
                        (
                            buckets,
                            Vec::new(),
                            snapshot.session_digest_records,
                            snapshot.active_ref,
                        )
                    }
                };
            let digests = digest_records
                .into_iter()
                .filter_map(|record| match record.change() {
                    SourceSessionDigestChange::Upsert(digest) => Some((**digest).clone()),
                    SourceSessionDigestChange::Tombstone => None,
                })
                .collect();
            included_sources.push(metadata.source_id().clone());
            slices.push(SourceSlice {
                metadata: metadata.clone(),
                buckets,
                weekly_local_points,
            });
            replica_evidence.push(SourceReplicaEvidence {
                source_id: metadata.source_id().clone(),
                redaction_profile: source_redaction,
                digests,
                active_remote_ref,
                active_facts: BTreeMap::new(),
            });
        }
        Ok(Some(()))
    })();
    let loaded_bucket_count = slices.iter().map(|slice| slice.buckets.len()).sum();
    let loaded_weekly_count = slices
        .iter()
        .map(|slice| slice.weekly_local_points.len())
        .sum();
    let loaded_digest_count = replica_evidence
        .iter()
        .map(|evidence| evidence.digests.len())
        .sum();
    match &sources_result {
        Ok(Some(())) => sources_trace.finish(
            TraceOutcome::Ok,
            TraceFields::new()
                .usize("bucketRecordCount", loaded_bucket_count)
                .usize("weeklyRecordCount", loaded_weekly_count)
                .usize("digestRecordCount", loaded_digest_count),
        ),
        Ok(None) => sources_trace.finish(TraceOutcome::Partial, TraceFields::new()),
        Err(_) => sources_trace.finish(TraceOutcome::Error, TraceFields::new()),
    }
    if sources_result?.is_none() {
        return Ok(None);
    }

    let metadata_trace = process_trace_log().span(
        "history.v2.metadata_load",
        TraceFields::new().label("phase", "after"),
    );
    let metadata_result = store.list_source_metadata();
    match &metadata_result {
        Ok(metadata) => metadata_trace.finish(
            TraceOutcome::Ok,
            TraceFields::new().usize("sourceCount", metadata.len()),
        ),
        Err(_) => metadata_trace.finish(TraceOutcome::Error, TraceFields::new()),
    }
    let mut metadata_after = metadata_result?;
    metadata_after.sort_by(|left, right| left.source_id().as_str().cmp(right.source_id().as_str()));
    if metadata_before != metadata_after {
        return Ok(None);
    }

    included_sources.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    redaction_skipped_sources.sort_by(|left, right| left.as_str().cmp(right.as_str()));

    let mut replica_report = LogicalReplicaReport::default();
    if matches!(selection, HistorySourceSelection::AllIncluded) {
        // Preserve the reset-cycle interpretation from the physical inputs.
        // Logical replica resolution clears cumulative baselines because they
        // cannot be decomposed per thread; canonicalizing first prevents each
        // rolling zero-usage estimate from becoming an anchored marker.
        let weekly_cycle_resets = canonical_weekly_resets(&slices, &account.quota_points)?;
        let replica_trace = process_trace_log().span_with("history.v2.replica_resolve", || {
            TraceFields::new()
                .usize("sourceCount", slices.len())
                .usize("digestRecordCount", loaded_digest_count)
        });
        let replica_result = resolve_logical_replicas(
            store,
            &mut slices,
            &mut replica_evidence,
            &project_mapping.projection,
            &weekly_cycle_resets,
            read_budget,
        );
        match &replica_result {
            Ok(report) => replica_trace.finish(
                TraceOutcome::Ok,
                TraceFields::new()
                    .usize("logicalThreadCount", report.logical_threads.len())
                    .usize("warningCount", report.warnings.len()),
            ),
            Err(_) => replica_trace.finish(TraceOutcome::Error, TraceFields::new()),
        }
        replica_report = replica_result?;
    }

    let weekly_local_points =
        aggregate_source_weekly_points(&slices, &account.quota_points, since)?;
    // The durable Summary backfill marker describes reconstruction of this
    // machine's local rollout history. It is meaningful for the all-source or
    // exact-current-local projections, but must not make a remote-only view
    // appear locally backfilled.
    let marker = if source_selection_status.is_applied()
        && matches!(
            selection,
            HistorySourceSelection::AllIncluded | HistorySourceSelection::Local(_)
        ) {
        store.load_v2_summary_backfill_attempt(query_redaction, ownership_epoch)?
    } else {
        None
    };
    let aggregate_trace = process_trace_log().span_with("history.v2.bucket_aggregate", || {
        TraceFields::new()
            .usize("sourceCount", slices.len())
            .usize("inputBucketCount", loaded_bucket_count)
    });
    let bucket_projection = aggregate_source_buckets_with_logical_threads(
        &slices,
        &project_mapping.projection,
        &replica_report.logical_threads,
    );
    aggregate_trace.finish(
        TraceOutcome::Ok,
        TraceFields::new().usize("outputBucketCount", bucket_projection.buckets.len()),
    );
    let mut history = HistoryData {
        quota_points: account
            .quota_points
            .into_iter()
            .filter(|point| point.observed_at >= since)
            .collect(),
        half_hour_buckets: bucket_projection
            .buckets
            .into_iter()
            .filter(|bucket| bucket.ends_at > since)
            .collect(),
        weekly_local_points,
        ..HistoryData::default()
    };
    if let Some(marker) = marker {
        history.summary_backfill_attempted_at = Some(marker.completed_at);
        history.summary_backfill_attempt_complete = Some(marker.complete);
    }
    history.warnings.extend(replica_report.warnings);
    if bucket_projection.unmapped_projects {
        history
            .warnings
            .push(PROJECT_MAPPING_PARTIAL_WARNING.to_string());
    }
    if project_mapping.unavailable && bucket_projection.project_observations {
        history
            .warnings
            .push(PROJECT_MAPPING_UNAVAILABLE_WARNING.to_string());
    }
    if history.half_hour_buckets.iter().any(|bucket| {
        bucket
            .partial_reasons
            .iter()
            .any(|reason| reason == PROJECT_MAPPING_REGISTRATION_FAILED_WARNING)
    }) {
        history
            .warnings
            .push(PROJECT_MAPPING_REGISTRATION_FAILED_WARNING.to_string());
    }
    for source_id in &redaction_skipped_sources {
        history.warnings.push(format!(
            "{REDACTED_QUERY_SKIPPED_PREVIEW_SOURCE_WARNING}:{}",
            source_id.as_str()
        ));
    }
    if let HistorySourceSelectionStatus::Unavailable(reason) = source_selection_status {
        history
            .warnings
            .push(source_selection_unavailable_warning(selection, reason));
    } else if source_selection_status == HistorySourceSelectionStatus::AppliedExcludedFromAggregates
        && let Some(source_id) = selection.source_id()
    {
        history.warnings.push(format!(
            "{SOURCE_SELECTION_EXCLUDED_WARNING}:{}",
            source_id.as_str()
        ));
    }
    history.warnings.sort();
    history.warnings.dedup();

    Ok(Some(V2HistoryRead {
        history,
        included_sources,
        redaction_skipped_sources,
        source_selection_status,
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::{NonZeroU32, NonZeroU64};
    use std::path::Path;

    use chrono::{FixedOffset, TimeZone, Timelike};

    use super::*;
    use crate::api_cost::API_PRICING_CATALOG_REVISION;
    use crate::domain::{ApiCostAmount, PicoUsd, Provenance};
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_METRIC_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION,
        HistoryObservation,
    };
    use crate::history_ownership::{
        InitializeV1Outcome, OwnershipCasOutcome, OwnershipManifestStatus,
    };
    use crate::project_mapping::{ProjectMappingStore, ProjectObservation, SourceObservedProject};
    use crate::remote_protocol::{ProtocolRevisions, SourceGeneration};
    use crate::source_history::{
        CompleteFactBatch, FactBatchId, FactBatchKind, FactCursor, SessionDigestFingerprint,
        SessionUsageMetrics, SourceBucketRecord, SourceHistoryRemoteBinding,
        SourceHistoryRemoteGenerationId, SourceSessionDigestRecord, SourceWeeklyRecord,
        UsageEventFactRecord, UsageEventId,
    };
    use crate::source_model::{SessionReplicaKey, ThreadId};
    use crate::summary::{SummarySample, SummaryWindow, summarize_samples};

    const SOURCE_A: &str = "node-0123456789abcdef0123456789abcdef";
    const SOURCE_B: &str = "node-fedcba9876543210fedcba9876543210";
    const SOURCE_C: &str = "node-11111111111111111111111111111111";

    fn observed(hex: char) -> ObservedProjectKey {
        format!("opk-hmac-sha256-v1-{}", hex.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    fn mapping_observation(
        source: &str,
        key: ObservedProjectKey,
        label: &str,
    ) -> ProjectObservation {
        ProjectObservation::new(SourceObservedProject::new(source.parse().unwrap(), key))
            .with_display_label(Some(label.parse().unwrap()))
    }

    #[test]
    fn user_source_selector_resolves_keywords_and_exact_remote_node() {
        let local: NodeId = SOURCE_A.parse().unwrap();
        let remote: NodeId = SOURCE_B.parse().unwrap();

        assert_eq!(
            "all"
                .parse::<HistorySourceSelector>()
                .unwrap()
                .resolve(&local),
            HistorySourceSelection::AllIncluded
        );
        assert_eq!(
            "local"
                .parse::<HistorySourceSelector>()
                .unwrap()
                .resolve(&local),
            HistorySourceSelection::Local(local.clone())
        );
        let parsed = SOURCE_B.parse::<HistorySourceSelector>().unwrap();
        assert_eq!(parsed.to_string(), SOURCE_B);
        assert_eq!(
            parsed.resolve(&local),
            HistorySourceSelection::Remote(remote)
        );
        assert!("server.example".parse::<HistorySourceSelector>().is_err());
    }

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn source(
        id: &str,
        label: &str,
        kind: SourceKind,
        redaction: RedactionProfile,
    ) -> SourceMetadata {
        SourceMetadata::new_with_redaction_profile(id.parse().unwrap(), kind, label, redaction)
            .unwrap()
    }

    fn bucket(starts_at: DateTime<Utc>, total: u64, project: &str) -> LocalHalfHourBucket {
        let token_usage = TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        };
        LocalHalfHourBucket {
            starts_at,
            ends_at: starts_at + Duration::minutes(15),
            sampled_at: starts_at + Duration::minutes(15),
            token_usage,
            estimated_cost_units: u128::from(total),
            api_long_context_extra_cost_units: Some(u128::from(total / 2)),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 1,
            groups: vec![LocalUsageGroup {
                model: Some("gpt-test".to_string()),
                token_usage,
                estimated_cost_units: u128::from(total),
                api_long_context_extra_cost_units: Some(u128::from(total / 2)),
                call_count: 1,
                ..LocalUsageGroup::default()
            }],
            project_groups: vec![LocalProjectUsageGroup {
                thread_id: "thread".to_string(),
                turn_id: Some("turn".to_string()),
                session_thread_id: Some("thread".to_string()),
                session_turn_id: Some("turn".to_string()),
                project_id: Some(project.to_string()),
                project_label: Some(project.to_string()),
                token_usage,
                estimated_cost_units: u128::from(total),
                api_long_context_extra_cost_units: Some(u128::from(total / 2)),
                api_equivalent_cost: api_amount(total, 1),
                call_count: 1,
                ..LocalProjectUsageGroup::default()
            }],
            partial_reasons: Vec::new(),
        }
    }

    fn bucket_with_calls(
        starts_at: DateTime<Utc>,
        total: u64,
        project: &str,
        call_count: u64,
    ) -> LocalHalfHourBucket {
        let mut value = bucket(starts_at, total, project);
        value.call_count = call_count;
        value.groups[0].call_count = call_count;
        value.project_groups[0].call_count = call_count;
        value.project_groups[0].api_equivalent_cost = api_amount(total, call_count);
        value
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn incomplete_mixed_bucket(
        starts_at: DateTime<Utc>,
        target_project: &str,
        unrelated_project: &str,
    ) -> LocalHalfHourBucket {
        let mut mixed = bucket_with_calls(starts_at, 120, target_project, 5);
        let target = &mut mixed.project_groups[0];
        target.token_usage = TokenUsage {
            input_tokens: 25,
            total_tokens: 25,
            ..TokenUsage::default()
        };
        target.estimated_cost_units = 25;
        target.api_long_context_extra_cost_units = Some(12);
        target.api_equivalent_cost = api_amount(25, 2);
        target.call_count = 2;

        let mut unrelated = bucket(starts_at, 90, unrelated_project)
            .project_groups
            .remove(0);
        unrelated.thread_id = "other-thread".to_owned();
        unrelated.turn_id = Some("other-turn".to_owned());
        unrelated.session_thread_id = Some("other-thread".to_owned());
        unrelated.session_turn_id = Some("other-turn".to_owned());
        mixed.project_groups.push(unrelated);

        let mut unbound = bucket(starts_at, 2, unrelated_project)
            .project_groups
            .remove(0);
        unbound.thread_id.clear();
        unbound.turn_id = None;
        unbound.session_thread_id = None;
        unbound.session_turn_id = None;
        mixed.project_groups.push(unbound);
        mixed
    }

    fn api_amount(total: u64, call_count: u64) -> ApiCostAmount {
        ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(u128::from(total)),
            maximum_pico_usd: PicoUsd::new(u128::from(total)),
            observed_samples: call_count,
            priced_samples: call_count,
            observed_tokens: total,
            priced_tokens: total,
        }
    }

    fn session_metrics(total: u64, call_count: u64) -> SessionUsageMetrics {
        SessionUsageMetrics {
            token_usage: TokenUsage {
                input_tokens: total,
                total_tokens: total,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(total),
            api_long_context_extra_cost_units: Some(u128::from(total / 2)),
            api_equivalent_cost: api_amount(total, call_count),
            call_count,
            metric_revision: HISTORY_METRIC_REVISION,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            ..SessionUsageMetrics::default()
        }
    }

    fn session_digest(
        source_id: &NodeId,
        range_start: DateTime<Utc>,
        fingerprint: char,
        total: u64,
        event_count: u64,
        project: ObservedProjectKey,
    ) -> SourceSessionDigest {
        let range_end = range_start + Duration::days(1);
        SourceSessionDigest::new(
            SessionReplicaKey::new(source_id.clone(), ThreadId::from_str("thread").unwrap()),
            range_start,
            range_end,
            range_end,
            SessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                fingerprint.to_string().repeat(64)
            ))
            .unwrap(),
            SessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                fingerprint.to_string().repeat(64)
            ))
            .unwrap(),
            event_count,
            true,
            true,
            vec![project],
            session_metrics(total, event_count),
        )
        .unwrap()
    }

    fn usage_fact(
        source_id: &NodeId,
        event_id: &str,
        occurred_at: DateTime<Utc>,
        total: u64,
        project: ObservedProjectKey,
    ) -> UsageEventFactRecord {
        let replica =
            SessionReplicaKey::new(source_id.clone(), ThreadId::from_str("thread").unwrap());
        UsageEventFactRecord::upsert(
            1,
            UsageEventFact::new(
                replica,
                UsageEventId::from_str(event_id).unwrap(),
                occurred_at,
                project,
                Some("turn".to_string()),
                None,
                Some(ThreadId::from_str("thread").unwrap()),
                ThreadId::from_str("thread").unwrap(),
                Some("turn".to_string()),
                Some("gpt-test".to_string()),
                None,
                session_metrics(total, 1).token_usage,
                true,
                true,
                session_metrics(total, 1),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn install_local_replica(
        store: &SourceHistoryStore,
        metadata: &SourceMetadata,
        bucket: LocalHalfHourBucket,
        digest: SourceSessionDigest,
        facts: Vec<UsageEventFactRecord>,
    ) {
        install_local_replica_history(store, metadata, vec![bucket], vec![digest], facts);
    }

    fn install_local_replica_history(
        store: &SourceHistoryStore,
        metadata: &SourceMetadata,
        buckets: Vec<LocalHalfHourBucket>,
        mut digests: Vec<SourceSessionDigest>,
        facts: Vec<UsageEventFactRecord>,
    ) {
        store.save_source_metadata(metadata).unwrap();
        let bucket_records = buckets
            .into_iter()
            .map(|bucket| SourceBucketRecord::upsert(1, bucket).unwrap())
            .collect::<Vec<_>>();
        store
            .record_source_bucket_changes(
                metadata.source_id(),
                metadata.aggregate_redaction_profile(),
                &bucket_records,
            )
            .unwrap();
        if !facts.is_empty() {
            for digest in &mut digests {
                let range_facts = facts
                    .iter()
                    .filter_map(|record| match record.change() {
                        crate::source_history::UsageEventFactChange::Upsert(fact)
                            if fact.occurred_at() >= digest.range_start()
                                && fact.occurred_at() < digest.range_end() =>
                        {
                            Some(fact.as_ref())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let (fingerprint, project_fingerprint) =
                    crate::source_export::canonical_fact_fingerprints_for_test(
                        digest.replica(),
                        digest.range_start(),
                        digest.range_end(),
                        &range_facts,
                    )
                    .unwrap();
                *digest = SourceSessionDigest::new(
                    digest.replica().clone(),
                    digest.range_start(),
                    digest.range_end(),
                    digest.covered_through(),
                    fingerprint,
                    project_fingerprint,
                    digest.event_count(),
                    digest.exact_event_identity(),
                    digest.coverage_complete(),
                    digest.observed_project_keys().to_vec(),
                    digest.metrics().clone(),
                )
                .unwrap();
            }
        }
        let validated_digests = digests
            .iter()
            .map(crate::source_history::FactDigestBinding::from_digest)
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        let digest_records = digests
            .into_iter()
            .map(|digest| SourceSessionDigestRecord::upsert(1, digest).unwrap())
            .collect::<Vec<_>>();
        store
            .record_source_session_digest_changes(
                metadata.source_id(),
                metadata.aggregate_redaction_profile(),
                &digest_records,
            )
            .unwrap();
        if facts.is_empty() {
            return;
        }
        let batch = CompleteFactBatch {
            batch_id: FactBatchId::generate().unwrap(),
            kind: FactBatchKind::Snapshot,
            replica: SessionReplicaKey::new(
                metadata.source_id().clone(),
                ThreadId::from_str("thread").unwrap(),
            ),
            expected_active_version: None,
            remote_binding: None,
            validated_digests,
            activate_cursor: FactCursor::new(1, facts.len() as u64).unwrap(),
            completed_at: at(31, 23, 59),
            changes: facts,
        };
        store
            .stage_complete_fact_batch(
                metadata.source_id(),
                metadata.aggregate_redaction_profile(),
                &batch,
            )
            .unwrap();
        store
            .activate_staged_fact_batch(
                metadata.source_id(),
                metadata.aggregate_redaction_profile(),
                &batch.batch_id,
            )
            .unwrap();
    }

    fn weekly(
        observed_at: DateTime<Utc>,
        resets_at: DateTime<Utc>,
        total: u64,
    ) -> WeeklyLocalPoint {
        WeeklyLocalPoint {
            observed_at,
            resets_at,
            token_usage: TokenUsage {
                input_tokens: total,
                total_tokens: total,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(total),
            api_long_context_extra_cost_units: Some(u128::from(total / 2)),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            call_count: 1,
            partial_reasons: Vec::new(),
        }
    }

    fn quota(observed_at: DateTime<Utc>, resets_at: DateTime<Utc>) -> QuotaPoint {
        QuotaPoint {
            observed_at,
            limit_id: "codex".to_string(),
            duration_mins: WEEKLY_WINDOW_MINUTES,
            resets_at,
            used_percent: 25.0,
            remaining_percent: 75.0,
            provenance: Provenance::ServerSnapshot,
        }
    }

    fn prepare_state_root(root: &Path) {
        fs::create_dir_all(root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    fn stores(
        root: &Path,
        codex_home: &Path,
        redaction: RedactionProfile,
    ) -> (HistoryStore, HistoryOwnershipStore, SourceHistoryStore) {
        prepare_state_root(root);
        fs::create_dir_all(codex_home).unwrap();
        let redact = redaction == RedactionProfile::Redacted;
        let legacy = HistoryStore::new_with_redaction(
            root.join(LEGACY_HISTORY_DIRECTORY),
            codex_home,
            redact,
        );
        let profile_text = if redact {
            legacy.namespace().strip_suffix("-redacted").unwrap()
        } else {
            legacy.namespace()
        };
        let profile = profile_text
            .parse::<crate::source_history::HistoryProfileId>()
            .unwrap();
        let ownership = HistoryOwnershipStore::new(root.to_path_buf(), profile.clone(), redaction);
        let source_history = SourceHistoryStore::new(root.to_path_buf(), profile);
        (legacy, ownership, source_history)
    }

    fn initialize_v1(ownership: &HistoryOwnershipStore) -> HistoryOwnershipManifest {
        let lease = ownership.acquire_writer_lease().unwrap();
        match ownership.initialize_v1_active(&lease).unwrap() {
            InitializeV1Outcome::Initialized(manifest)
            | InitializeV1Outcome::Existing(manifest) => manifest,
        }
    }

    fn activate_v2(ownership: &HistoryOwnershipStore) -> HistoryOwnershipManifest {
        let lease = ownership.acquire_writer_lease().unwrap();
        let v1 = match ownership.load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            OwnershipManifestStatus::Uninitialized => {
                match ownership.initialize_v1_active(&lease).unwrap() {
                    InitializeV1Outcome::Initialized(manifest)
                    | InitializeV1Outcome::Existing(manifest) => manifest,
                }
            }
        };
        let migrating = match ownership.begin_migration(&lease, &v1).unwrap() {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(current) => {
                panic!("unexpected migration conflict: {current:?}")
            }
        };
        match ownership
            .compare_and_transition(&lease, &migrating, HistoryOwnershipState::V2Active)
            .unwrap()
        {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(current) => {
                panic!("unexpected activation conflict: {current:?}")
            }
        }
    }

    fn install_remote_bucket(
        ownership: &HistoryOwnershipStore,
        store: &SourceHistoryStore,
        active: &HistoryOwnershipManifest,
        metadata: &SourceMetadata,
        starts_at: DateTime<Utc>,
        total: u64,
    ) {
        let lease = ownership.acquire_writer_lease().unwrap();
        let authority = ownership.authorize_v2_write(&lease, active).unwrap();
        let writer = store.writer(&authority).unwrap();
        writer.save_source_metadata(metadata).unwrap();
        let generation: SourceHistoryRemoteGenerationId =
            "ingest-gen-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .unwrap();
        let one = NonZeroU32::new(1).unwrap();
        let binding = SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: metadata.source_id().clone(),
                generation: NonZeroU64::new(1).unwrap(),
            },
            ProtocolRevisions {
                history_format: one,
                metric: one,
                estimator: one,
                project_breakdown: one,
                api_pricing_catalog: one,
            },
        )
        .unwrap();
        let mut physical_bucket = bucket(starts_at, total, metadata.display_label());
        physical_bucket.project_groups[0].thread_id =
            format!("thread-{}", metadata.source_id().as_str());
        physical_bucket.project_groups[0].session_thread_id =
            Some(physical_bucket.project_groups[0].thread_id.clone());
        writer
            .ensure_remote_history_generation(
                metadata.source_id(),
                metadata.aggregate_redaction_profile(),
                &generation,
                &binding,
            )
            .unwrap();
        writer
            .apply_remote_history_generation_page(
                metadata.source_id(),
                metadata.aggregate_redaction_profile(),
                &generation,
                &binding,
                &[SourceBucketRecord::upsert(1, physical_bucket).unwrap()],
                &[],
            )
            .unwrap();
        writer
            .activate_remote_history_generation(
                metadata.source_id(),
                metadata.aggregate_redaction_profile(),
                None,
                &generation,
                &binding,
                starts_at + Duration::minutes(20),
            )
            .unwrap();
    }

    #[test]
    fn additive_bucket_projection_scopes_project_session_and_model_data() {
        let starts_at = at(2, 10, 0);
        let mut open = bucket(starts_at, 20, "same-project");
        open.sampled_at = starts_at + Duration::minutes(7);
        open.api_long_context_extra_cost_units = None;
        let slices = vec![
            SourceSlice {
                metadata: source(
                    SOURCE_A,
                    "alpha",
                    SourceKind::Local,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![bucket(starts_at, 10, "same-project")],
                weekly_local_points: Vec::new(),
            },
            SourceSlice {
                metadata: source(
                    SOURCE_B,
                    "beta",
                    SourceKind::Ssh,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![open],
                weekly_local_points: Vec::new(),
            },
        ];

        let result =
            aggregate_source_buckets(&slices, &ProjectMappingProjection::default()).buckets;
        assert_eq!(result.len(), 1);
        let aggregate = &result[0];
        assert_eq!(aggregate.token_usage.total_tokens, 30);
        assert_eq!(aggregate.groups.len(), 1);
        assert_eq!(aggregate.groups[0].token_usage.total_tokens, 30);
        assert_eq!(aggregate.sampled_at, starts_at + Duration::minutes(7));
        assert_eq!(aggregate.api_long_context_extra_cost_units, None);
        assert_eq!(aggregate.project_groups.len(), 2);
        assert_eq!(
            aggregate.project_groups[0].project_id.as_deref(),
            Some(format!("same-project@{SOURCE_A}").as_str())
        );
        assert_eq!(
            aggregate.project_groups[1].project_id.as_deref(),
            Some(format!("same-project@{SOURCE_B}").as_str())
        );
        assert_ne!(
            aggregate.project_groups[0].thread_id,
            aggregate.project_groups[1].thread_id
        );
        assert_eq!(
            aggregate.project_groups[0].project_label.as_deref(),
            Some("same-project @ alpha")
        );
    }

    #[test]
    fn explicit_logical_mapping_merges_summary_projects_without_rewriting_history() {
        let directory = tempfile::tempdir().unwrap();
        let mapping_store =
            ProjectMappingStore::new(directory.path().join("config/project-mappings.json"));
        let key_a = observed('a');
        let key_b = observed('b');
        let discovered = mapping_store
            .resolve_or_create_batch(
                0,
                vec![
                    mapping_observation(SOURCE_A, key_a.clone(), "alpha remote"),
                    mapping_observation(SOURCE_B, key_b.clone(), "beta remote"),
                ],
            )
            .unwrap();
        let instance_ids = discovered.instance_ids().to_vec();
        let source_buckets = vec![
            SourceSlice {
                metadata: source(
                    SOURCE_A,
                    "host-a",
                    SourceKind::Local,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![bucket(at(2, 10, 0), 10, key_a.as_str())],
                weekly_local_points: Vec::new(),
            },
            SourceSlice {
                metadata: source(
                    SOURCE_B,
                    "host-b",
                    SourceKind::Ssh,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![bucket(at(2, 10, 0), 20, key_b.as_str())],
                weekly_local_points: Vec::new(),
            },
        ];

        let before = aggregate_source_buckets(&source_buckets, &discovered.mappings().projection());
        assert_eq!(before.buckets[0].token_usage.total_tokens, 30);
        assert_ne!(
            before.buckets[0].project_groups[0].project_id,
            before.buckets[0].project_groups[1].project_id
        );
        assert!(!before.unmapped_projects);

        let merged = mapping_store
            .merge_instances(
                discovered.mappings().revision(),
                None,
                Some("unified project".parse().unwrap()),
                &instance_ids,
            )
            .unwrap();
        // The exact same immutable source buckets project differently after a
        // mapping-only CAS; no history record is rewritten.
        let after = aggregate_source_buckets(&source_buckets, &merged.mappings().projection());
        assert_eq!(after.buckets[0].token_usage.total_tokens, 30);
        assert_eq!(after.buckets[0].project_groups.len(), 2);
        assert_eq!(
            after.buckets[0].project_groups[0].project_id,
            after.buckets[0].project_groups[1].project_id
        );
        assert!(
            after.buckets[0]
                .project_groups
                .iter()
                .all(|group| group.project_label.as_deref() == Some("unified project"))
        );
        let summary_samples = after.buckets[0]
            .project_groups
            .iter()
            .map(|group| SummarySample {
                timestamp: after.buckets[0].starts_at,
                thread_id: group.thread_id.clone(),
                parent_thread_id: group.parent_thread_id.clone(),
                turn_id: group.turn_id.clone(),
                session_thread_id: group.session_thread_id.clone(),
                session_turn_id: group.session_turn_id.clone(),
                message_preview: group.message_preview.clone(),
                turn_started_at: group.turn_started_at,
                project_key: group.project_id.clone(),
                project_label: group.project_label.clone(),
                cwd: None,
                title: group.title.clone(),
                source: group.source.clone(),
                token_usage: group.token_usage,
                estimated_cost_units: group.estimated_cost_units,
                api_long_context_extra_cost_units: group
                    .api_long_context_extra_cost_units
                    .unwrap_or_default(),
                api_equivalent_cost: group.api_equivalent_cost,
                call_count: group.call_count,
            })
            .collect::<Vec<_>>();
        let summary = summarize_samples(
            &summary_samples,
            SummaryWindow::new(at(2, 9, 0), at(2, 11, 0)).unwrap(),
            FixedOffset::east_opt(0).unwrap(),
        );
        assert_eq!(summary.projects.len(), 1);
        assert_eq!(summary.projects[0].totals.token_usage.total_tokens, 30);
    }

    #[test]
    fn logical_mapping_resolves_replica_project_attribution_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let mapping_store =
            ProjectMappingStore::new(directory.path().join("config/project-mappings.json"));
        let source_a: NodeId = SOURCE_A.parse().unwrap();
        let source_b: NodeId = SOURCE_B.parse().unwrap();
        let project_a = observed('a');
        let project_b = observed('b');
        let digest_a = session_digest(&source_a, at(2, 0, 0), 'a', 10, 1, project_a.clone());
        let digest_b = session_digest(&source_b, at(2, 0, 0), 'a', 10, 1, project_b.clone());
        let evidence = vec![
            SourceReplicaEvidence {
                source_id: source_a,
                redaction_profile: RedactionProfile::Redacted,
                digests: vec![digest_a.clone()],
                active_remote_ref: None,
                active_facts: BTreeMap::new(),
            },
            SourceReplicaEvidence {
                source_id: source_b,
                redaction_profile: RedactionProfile::Redacted,
                digests: vec![digest_b.clone()],
                active_remote_ref: None,
                active_facts: BTreeMap::new(),
            },
        ];
        let participants = vec![
            ReplicaParticipant {
                source_index: 0,
                digest: digest_a,
                exact_fact_coverage: false,
            },
            ReplicaParticipant {
                source_index: 1,
                digest: digest_b,
                exact_fact_coverage: false,
            },
        ];
        assert!(digest_project_attribution_conflicts(
            &participants,
            &evidence,
            &ProjectMappingProjection::default(),
        ));

        let discovered = mapping_store
            .resolve_or_create_batch(
                0,
                vec![
                    mapping_observation(SOURCE_A, project_a, "alpha"),
                    mapping_observation(SOURCE_B, project_b, "beta"),
                ],
            )
            .unwrap();
        assert!(digest_project_attribution_conflicts(
            &participants,
            &evidence,
            &discovered.mappings().projection(),
        ));
        let merged = mapping_store
            .merge_instances(
                discovered.mappings().revision(),
                None,
                Some("logical".parse().unwrap()),
                discovered.instance_ids(),
            )
            .unwrap();
        assert!(!digest_project_attribution_conflicts(
            &participants,
            &evidence,
            &merged.mappings().projection(),
        ));
    }

    #[test]
    fn unmapped_observations_never_merge_and_are_reported_partial() {
        let starts_at = at(2, 10, 0);
        let sources = vec![
            SourceSlice {
                metadata: source(
                    SOURCE_A,
                    "host-a",
                    SourceKind::Local,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![bucket(starts_at, 10, observed('c').as_str())],
                weekly_local_points: Vec::new(),
            },
            SourceSlice {
                metadata: source(
                    SOURCE_B,
                    "host-b",
                    SourceKind::Ssh,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![bucket(starts_at, 20, observed('c').as_str())],
                weekly_local_points: Vec::new(),
            },
        ];
        let projection = aggregate_source_buckets(&sources, &ProjectMappingProjection::default());
        assert!(projection.project_observations);
        assert!(projection.unmapped_projects);
        assert_ne!(
            projection.buckets[0].project_groups[0].project_id,
            projection.buckets[0].project_groups[1].project_id
        );
        assert_eq!(projection.buckets[0].token_usage.total_tokens, 30);
    }

    #[test]
    fn identical_exact_replica_is_counted_once_and_source_only_stays_physical() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let starts_at = at(2, 10, 0);
        let range_start = at(2, 0, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(source_a.source_id(), range_start, 'a', 10, 1, project_a),
            Vec::new(),
        );
        install_local_replica(
            &store,
            &source_b,
            bucket(starts_at, 10, project_b.as_str()),
            session_digest(source_b.source_id(), range_start, 'a', 10, 1, project_b),
            Vec::new(),
        );
        activate_v2(&ownership);

        let all = load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        assert_eq!(
            all.history.half_hour_buckets[0].token_usage.total_tokens,
            10
        );
        assert_eq!(all.history.half_hour_buckets[0].project_groups.len(), 1);
        assert_eq!(
            all.history.half_hour_buckets[0].project_groups[0].thread_id,
            "logical-thread:thread"
        );
        assert_eq!(
            all.history.half_hour_buckets[0].project_groups[0]
                .source
                .as_deref(),
            Some("alpha")
        );
        assert!(
            !all.history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
        let (mut restarted_legacy, restarted_ownership, restarted_store) =
            stores(&root, &codex_home, RedactionProfile::Redacted);
        let after_restart = load_unified_history_since(
            &restarted_ownership,
            &mut restarted_legacy,
            &restarted_store,
            range_start,
        )
        .unwrap();
        assert_eq!(
            after_restart.history.half_hour_buckets,
            all.history.half_hour_buckets
        );
        assert_eq!(after_restart.history.warnings, all.history.warnings);

        let source_b_only = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &store,
            source_b.source_id(),
            &HistorySourceSelection::Local(source_b.source_id().clone()),
            range_start,
        )
        .unwrap();
        assert_eq!(
            source_b_only.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );
        assert!(
            source_b_only.history.half_hour_buckets[0].project_groups[0]
                .thread_id
                .ends_with(SOURCE_B)
        );
    }

    #[test]
    fn replica_replacement_preserves_unrelated_bucket_usage_and_unknown_residual() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let range_start = at(2, 0, 0);
        let candidate_at = at(2, 10, 0);
        let unrelated_at = at(2, 11, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        install_local_replica(
            &store,
            &source_a,
            bucket(candidate_at, 10, project_a.as_str()),
            session_digest(source_a.source_id(), range_start, 'a', 10, 1, project_a),
            Vec::new(),
        );

        let mut candidate_with_residual = bucket(candidate_at, 100, project_b.as_str());
        candidate_with_residual.project_groups[0].token_usage = TokenUsage {
            input_tokens: 10,
            total_tokens: 10,
            ..TokenUsage::default()
        };
        candidate_with_residual.project_groups[0].estimated_cost_units = 10;
        candidate_with_residual.project_groups[0].api_long_context_extra_cost_units = Some(5);
        candidate_with_residual.project_groups[0].api_equivalent_cost = api_amount(10, 1);
        let mut unrelated = bucket(unrelated_at, 50, project_b.as_str());
        unrelated.project_groups.clear();
        let unrelated_models = unrelated.groups.clone();
        install_local_replica_history(
            &store,
            &source_b,
            vec![candidate_with_residual, unrelated],
            vec![session_digest(
                source_b.source_id(),
                range_start,
                'a',
                10,
                1,
                project_b,
            )],
            Vec::new(),
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let buckets = result
            .history
            .half_hour_buckets
            .iter()
            .map(|bucket| (bucket.starts_at, bucket))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(buckets[&candidate_at].token_usage.total_tokens, 100);
        assert_eq!(buckets[&candidate_at].estimated_cost_units, 100);
        assert_eq!(
            buckets[&candidate_at].api_long_context_extra_cost_units,
            Some(50)
        );
        assert_eq!(buckets[&unrelated_at].token_usage.total_tokens, 50);
        assert_eq!(buckets[&unrelated_at].groups, unrelated_models);
    }

    #[test]
    fn incomplete_replica_breakdown_drops_non_authority_bucket_to_lower_bound() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let range_start = at(2, 0, 0);
        let starts_at = at(2, 10, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(source_a.source_id(), range_start, 'a', 10, 1, project_a),
            Vec::new(),
        );
        let mut incomplete = bucket(starts_at, 10, project_b.as_str());
        incomplete.project_groups[0].token_usage = TokenUsage {
            input_tokens: 5,
            total_tokens: 5,
            ..TokenUsage::default()
        };
        incomplete.project_groups[0].estimated_cost_units = 5;
        incomplete.project_groups[0].api_long_context_extra_cost_units = Some(2);
        incomplete.project_groups[0].api_equivalent_cost = api_amount(5, 1);
        install_local_replica(
            &store,
            &source_b,
            incomplete,
            session_digest(source_b.source_id(), range_start, 'a', 10, 1, project_b),
            Vec::new(),
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 10);
        assert_eq!(bucket.project_groups.len(), 1);
        assert_eq!(bucket.project_groups[0].token_usage.total_tokens, 10);
        assert!(
            bucket
                .partial_reasons
                .contains(&DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string())
        );
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
    }

    #[test]
    fn incomplete_no_fact_replica_preserves_only_explicit_unrelated_groups() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let range_start = at(2, 0, 0);
        let starts_at = at(2, 10, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        let unrelated_project = observed('c');
        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(source_a.source_id(), range_start, 'a', 10, 1, project_a),
            Vec::new(),
        );
        install_local_replica(
            &store,
            &source_b,
            incomplete_mixed_bucket(starts_at, project_b.as_str(), unrelated_project.as_str()),
            session_digest(source_b.source_id(), range_start, 'b', 30, 2, project_b),
            Vec::new(),
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 100);
        assert_eq!(bucket.estimated_cost_units, 100);
        assert_eq!(bucket.api_long_context_extra_cost_units, Some(50));
        assert_eq!(bucket.call_count, 2);
        assert!(bucket.project_groups.iter().any(|group| {
            group.thread_id.starts_with("other-thread@") && group.token_usage.total_tokens == 90
        }));
        assert!(
            bucket
                .project_groups
                .iter()
                .all(|group| !group.thread_id.is_empty())
        );
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
    }

    #[test]
    fn missing_replica_digests_use_one_deterministic_bucket_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let range_start = at(2, 0, 0);
        let starts_at = at(2, 10, 0);
        install_local_replica_history(
            &store,
            &source_a,
            vec![bucket(starts_at, 10, observed('a').as_str())],
            Vec::new(),
            Vec::new(),
        );
        install_local_replica_history(
            &store,
            &source_b,
            vec![bucket(starts_at, 10, observed('b').as_str())],
            Vec::new(),
            Vec::new(),
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 10);
        assert_eq!(bucket.project_groups.len(), 1);
        assert_eq!(bucket.project_groups[0].thread_id, "logical-thread:thread");
        assert_eq!(bucket.project_groups[0].source.as_deref(), Some("alpha"));
        assert!(
            bucket
                .partial_reasons
                .contains(&DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string())
        );
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
    }

    #[test]
    fn missing_replica_digests_preserve_only_explicit_unrelated_groups() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let range_start = at(2, 0, 0);
        let starts_at = at(2, 10, 0);
        install_local_replica_history(
            &store,
            &source_a,
            vec![bucket(starts_at, 10, observed('a').as_str())],
            Vec::new(),
            Vec::new(),
        );
        install_local_replica_history(
            &store,
            &source_b,
            vec![incomplete_mixed_bucket(
                starts_at,
                observed('b').as_str(),
                observed('c').as_str(),
            )],
            Vec::new(),
            Vec::new(),
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 100);
        assert_eq!(bucket.estimated_cost_units, 100);
        assert_eq!(bucket.api_long_context_extra_cost_units, Some(50));
        assert_eq!(bucket.call_count, 2);
        assert!(bucket.project_groups.iter().any(|group| {
            group.thread_id.starts_with("other-thread@") && group.token_usage.total_tokens == 90
        }));
        assert!(
            bucket
                .project_groups
                .iter()
                .all(|group| !group.thread_id.is_empty())
        );
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
    }

    #[test]
    fn divergent_replica_without_complete_facts_uses_one_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let starts_at = at(2, 10, 0);
        let range_start = at(2, 0, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(source_a.source_id(), range_start, 'a', 10, 1, project_a),
            Vec::new(),
        );
        install_local_replica(
            &store,
            &source_b,
            bucket_with_calls(starts_at, 30, project_b.as_str(), 2),
            session_digest(source_b.source_id(), range_start, 'b', 30, 2, project_b),
            Vec::new(),
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 10);
        assert_eq!(bucket.project_groups.len(), 1);
        assert_eq!(bucket.project_groups[0].token_usage.total_tokens, 10);
        assert_eq!(bucket.project_groups[0].thread_id, "logical-thread:thread");
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
    }

    #[test]
    fn exact_fact_coverage_dominates_the_source_id_authority_tiebreak() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let starts_at = at(2, 10, 0);
        let range_start = at(2, 0, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(source_a.source_id(), range_start, 'a', 10, 1, project_a),
            Vec::new(),
        );
        install_local_replica(
            &store,
            &source_b,
            bucket_with_calls(starts_at, 30, project_b.as_str(), 2),
            session_digest(
                source_b.source_id(),
                range_start,
                'b',
                30,
                2,
                project_b.clone(),
            ),
            vec![
                usage_fact(
                    source_b.source_id(),
                    "event-shared",
                    starts_at + Duration::minutes(1),
                    10,
                    project_b.clone(),
                ),
                usage_fact(
                    source_b.source_id(),
                    "event-unique",
                    starts_at + Duration::minutes(2),
                    20,
                    project_b,
                ),
            ],
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 30);
        assert_eq!(bucket.project_groups.len(), 1);
        assert_eq!(bucket.project_groups[0].token_usage.total_tokens, 30);
        assert_eq!(bucket.project_groups[0].source.as_deref(), Some("beta"));
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
    }

    #[test]
    fn aggregate_filter_is_applied_before_replica_authority_selection() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let mut excluded = source(
            SOURCE_A,
            "excluded",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        excluded.set_include_in_aggregates(false);
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_c = source(
            SOURCE_C,
            "gamma",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let starts_at = at(2, 10, 0);
        let range_start = at(2, 0, 0);
        for (metadata, total, project) in [
            (&excluded, 10, observed('a')),
            (&source_b, 20, observed('b')),
            (&source_c, 20, observed('c')),
        ] {
            install_local_replica(
                &store,
                metadata,
                bucket(starts_at, total, project.as_str()),
                session_digest(metadata.source_id(), range_start, 'a', total, 1, project),
                Vec::new(),
            );
        }
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        assert_eq!(
            result.included_sources,
            vec![source_c.source_id().clone(), source_b.source_id().clone()]
        );
        assert_eq!(
            result.history.half_hour_buckets[0].token_usage.total_tokens,
            20
        );
        assert_eq!(
            result.history.half_hour_buckets[0].project_groups[0]
                .source
                .as_deref(),
            Some("gamma")
        );
    }

    #[test]
    fn divergent_exact_facts_form_one_event_union_and_preserve_sources() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let starts_at = at(2, 10, 0);
        let range_start = at(2, 0, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(
                source_a.source_id(),
                range_start,
                'a',
                10,
                1,
                project_a.clone(),
            ),
            vec![usage_fact(
                source_a.source_id(),
                "event-shared",
                starts_at + Duration::minutes(1),
                10,
                project_a,
            )],
        );
        install_local_replica(
            &store,
            &source_b,
            bucket_with_calls(starts_at, 30, project_b.as_str(), 2),
            session_digest(
                source_b.source_id(),
                range_start,
                'b',
                30,
                2,
                project_b.clone(),
            ),
            vec![
                usage_fact(
                    source_b.source_id(),
                    "event-shared",
                    starts_at + Duration::minutes(1),
                    10,
                    project_b.clone(),
                ),
                usage_fact(
                    source_b.source_id(),
                    "event-unique",
                    starts_at + Duration::minutes(2),
                    20,
                    project_b,
                ),
            ],
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 30);
        assert_eq!(bucket.estimated_cost_units, 30);
        assert_eq!(bucket.api_long_context_extra_cost_units, Some(15));
        assert_eq!(bucket.call_count, 2);
        assert_eq!(
            bucket
                .project_groups
                .iter()
                .map(|group| group.token_usage.total_tokens)
                .sum::<u64>(),
            30
        );
        assert_eq!(
            bucket
                .project_groups
                .iter()
                .map(|group| group.api_equivalent_cost.minimum_pico_usd.value())
                .sum::<u128>(),
            30
        );
        assert!(bucket.groups.is_empty());
        assert!(
            bucket
                .partial_reasons
                .contains(&DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string())
        );
        assert!(
            bucket
                .project_groups
                .iter()
                .all(|group| group.thread_id == "logical-thread:thread")
        );
        assert_eq!(
            bucket
                .project_groups
                .iter()
                .filter_map(|group| group.source.as_deref())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["alpha", "beta"])
        );
        assert!(
            !result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string())
        );
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_PROJECT_CONFLICT_WARNING.to_string())
        );
    }

    #[test]
    fn replica_fact_bucket_index_preserves_first_duplicate_and_scales_with_fact_count() {
        let starts_at = at(1, 0, 0);
        let metadata = source(
            SOURCE_A,
            "local",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let mut duplicate_source = SourceSlice {
            metadata: metadata.clone(),
            buckets: vec![
                bucket(starts_at, 10, "first"),
                bucket(starts_at, 20, "second"),
            ],
            weekly_local_points: Vec::new(),
        };
        let untouched_second = duplicate_source.buckets[1].clone();
        let mut work = ReplicaBucketIndexWork::default();
        let mut indices =
            build_source_bucket_indices(std::slice::from_ref(&duplicate_source), &mut work)
                .unwrap();
        let record = usage_fact(
            metadata.source_id(),
            "event-first-match",
            starts_at,
            7,
            observed('a'),
        );
        let crate::source_history::UsageEventFactChange::Upsert(fact) = record.change() else {
            unreachable!();
        };
        let mut touched = BTreeMap::new();
        add_fact_group(
            0,
            &mut duplicate_source,
            &mut indices[0],
            fact,
            &mut touched,
            &mut work,
        )
        .unwrap();
        assert_eq!(duplicate_source.buckets[1], untouched_second);
        assert_eq!(work.indexed_buckets, 2);
        assert_eq!(work.fact_lookups, 1);

        let bucket_count = 10_000_usize;
        let mut large_source = SourceSlice {
            metadata,
            buckets: (0..bucket_count)
                .map(|offset| {
                    bucket(
                        starts_at
                            + Duration::minutes(i64::try_from(offset.saturating_mul(15)).unwrap()),
                        10,
                        "large",
                    )
                })
                .collect(),
            weekly_local_points: Vec::new(),
        };
        let mut large_work = ReplicaBucketIndexWork::default();
        let mut large_indices =
            build_source_bucket_indices(std::slice::from_ref(&large_source), &mut large_work)
                .unwrap();
        let mut large_touched = BTreeMap::new();
        for offset in 0..bucket_count {
            let occurred_at =
                starts_at + Duration::minutes(i64::try_from(offset.saturating_mul(15)).unwrap());
            let record = usage_fact(
                large_source.metadata.source_id(),
                &format!("event-indexed-{offset}"),
                occurred_at,
                1,
                observed('a'),
            );
            let crate::source_history::UsageEventFactChange::Upsert(fact) = record.change() else {
                unreachable!();
            };
            add_fact_group(
                0,
                &mut large_source,
                &mut large_indices[0],
                fact,
                &mut large_touched,
                &mut large_work,
            )
            .unwrap();
        }
        assert_eq!(large_work.indexed_buckets, bucket_count);
        assert_eq!(large_work.fact_lookups, bucket_count);
        assert_eq!(large_source.buckets.len(), bucket_count);
    }

    #[test]
    fn exact_fact_union_preserves_unrelated_groups_from_an_incomplete_bucket() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let starts_at = at(2, 10, 0);
        let range_start = at(2, 0, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        let unrelated_project = observed('c');

        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(
                source_a.source_id(),
                range_start,
                'a',
                10,
                1,
                project_a.clone(),
            ),
            vec![usage_fact(
                source_a.source_id(),
                "event-shared",
                starts_at + Duration::minutes(1),
                10,
                project_a,
            )],
        );

        let mut mixed_bucket = bucket_with_calls(starts_at, 120, project_b.as_str(), 4);
        {
            let target = &mut mixed_bucket.project_groups[0];
            target.token_usage = TokenUsage {
                input_tokens: 25,
                total_tokens: 25,
                ..TokenUsage::default()
            };
            target.estimated_cost_units = 25;
            target.api_long_context_extra_cost_units = Some(12);
            target.api_equivalent_cost = api_amount(25, 2);
            target.call_count = 2;
        }
        let mut unrelated = bucket(starts_at, 90, unrelated_project.as_str())
            .project_groups
            .remove(0);
        unrelated.thread_id = "other-thread".to_owned();
        unrelated.turn_id = Some("other-turn".to_owned());
        unrelated.session_thread_id = Some("other-thread".to_owned());
        unrelated.session_turn_id = Some("other-turn".to_owned());
        mixed_bucket.project_groups.push(unrelated);
        install_local_replica(
            &store,
            &source_b,
            mixed_bucket,
            session_digest(
                source_b.source_id(),
                range_start,
                'b',
                30,
                2,
                project_b.clone(),
            ),
            vec![
                usage_fact(
                    source_b.source_id(),
                    "event-shared",
                    starts_at + Duration::minutes(1),
                    10,
                    project_b.clone(),
                ),
                usage_fact(
                    source_b.source_id(),
                    "event-unique",
                    starts_at + Duration::minutes(2),
                    20,
                    project_b,
                ),
            ],
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 120);
        assert_eq!(bucket.estimated_cost_units, 120);
        assert_eq!(bucket.api_long_context_extra_cost_units, Some(60));
        assert_eq!(bucket.call_count, 3);
        assert!(bucket.project_groups.iter().any(|group| {
            group.thread_id.starts_with("other-thread@") && group.token_usage.total_tokens == 90
        }));
        assert_eq!(
            bucket
                .project_groups
                .iter()
                .filter(|group| group.thread_id == "logical-thread:thread")
                .map(|group| group.token_usage.total_tokens)
                .sum::<u64>(),
            30
        );
        assert!(
            bucket
                .partial_reasons
                .contains(&DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string())
        );
    }

    #[test]
    fn exact_fact_union_preserves_unrelated_groups_from_a_missing_digest_source() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_c = source(
            SOURCE_C,
            "gamma",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let starts_at = at(2, 10, 0);
        let range_start = at(2, 0, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        let project_c = observed('c');
        let unrelated_project = observed('d');

        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(
                source_a.source_id(),
                range_start,
                'a',
                10,
                1,
                project_a.clone(),
            ),
            vec![usage_fact(
                source_a.source_id(),
                "event-shared",
                starts_at + Duration::minutes(1),
                10,
                project_a,
            )],
        );
        install_local_replica(
            &store,
            &source_b,
            bucket_with_calls(starts_at, 30, project_b.as_str(), 2),
            session_digest(
                source_b.source_id(),
                range_start,
                'b',
                30,
                2,
                project_b.clone(),
            ),
            vec![
                usage_fact(
                    source_b.source_id(),
                    "event-shared",
                    starts_at + Duration::minutes(1),
                    10,
                    project_b.clone(),
                ),
                usage_fact(
                    source_b.source_id(),
                    "event-unique",
                    starts_at + Duration::minutes(2),
                    20,
                    project_b,
                ),
            ],
        );
        install_local_replica_history(
            &store,
            &source_c,
            vec![incomplete_mixed_bucket(
                starts_at,
                project_c.as_str(),
                unrelated_project.as_str(),
            )],
            Vec::new(),
            Vec::new(),
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        let bucket = &result.history.half_hour_buckets[0];
        assert_eq!(bucket.token_usage.total_tokens, 120);
        assert_eq!(bucket.estimated_cost_units, 120);
        assert_eq!(bucket.api_long_context_extra_cost_units, Some(60));
        assert_eq!(bucket.call_count, 3);
        assert!(bucket.project_groups.iter().any(|group| {
            group.thread_id.starts_with("other-thread@") && group.token_usage.total_tokens == 90
        }));
        assert_eq!(
            bucket
                .project_groups
                .iter()
                .filter(|group| group.thread_id == "logical-thread:thread")
                .map(|group| group.token_usage.total_tokens)
                .sum::<u64>(),
            30
        );
        assert!(
            bucket
                .project_groups
                .iter()
                .all(|group| !group.thread_id.is_empty())
        );
        assert!(
            bucket
                .partial_reasons
                .contains(&DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string())
        );
    }

    #[test]
    fn multi_day_replica_facts_are_injected_only_into_their_utc_day() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let day_two = at(2, 0, 0);
        let day_three = at(3, 0, 0);
        let bucket_two = at(2, 23, 45);
        let bucket_three = at(3, 0, 0);
        let project_a = observed('a');
        let project_b = observed('b');

        install_local_replica_history(
            &store,
            &source_a,
            vec![
                bucket(bucket_two, 10, project_a.as_str()),
                bucket(bucket_three, 8, project_a.as_str()),
            ],
            vec![
                session_digest(source_a.source_id(), day_two, 'a', 10, 1, project_a.clone()),
                session_digest(
                    source_a.source_id(),
                    day_three,
                    'c',
                    8,
                    1,
                    project_a.clone(),
                ),
            ],
            vec![
                usage_fact(
                    source_a.source_id(),
                    "event-day-two-shared",
                    bucket_two + Duration::minutes(1),
                    10,
                    project_a.clone(),
                ),
                usage_fact(
                    source_a.source_id(),
                    "event-day-three-shared",
                    bucket_three + Duration::minutes(1),
                    8,
                    project_a,
                ),
            ],
        );
        install_local_replica_history(
            &store,
            &source_b,
            vec![
                bucket_with_calls(bucket_two, 30, project_b.as_str(), 2),
                bucket_with_calls(bucket_three, 12, project_b.as_str(), 2),
            ],
            vec![
                session_digest(source_b.source_id(), day_two, 'b', 30, 2, project_b.clone()),
                session_digest(
                    source_b.source_id(),
                    day_three,
                    'd',
                    12,
                    2,
                    project_b.clone(),
                ),
            ],
            vec![
                usage_fact(
                    source_b.source_id(),
                    "event-day-two-shared",
                    bucket_two + Duration::minutes(1),
                    10,
                    project_b.clone(),
                ),
                usage_fact(
                    source_b.source_id(),
                    "event-day-two-unique",
                    bucket_two + Duration::minutes(2),
                    20,
                    project_b.clone(),
                ),
                usage_fact(
                    source_b.source_id(),
                    "event-day-three-shared",
                    bucket_three + Duration::minutes(1),
                    8,
                    project_b.clone(),
                ),
                usage_fact(
                    source_b.source_id(),
                    "event-day-three-unique",
                    bucket_three + Duration::minutes(2),
                    4,
                    project_b,
                ),
            ],
        );
        activate_v2(&ownership);

        let result = load_unified_history_since(&ownership, &mut legacy, &store, day_two).unwrap();
        let totals = result
            .history
            .half_hour_buckets
            .iter()
            .map(|bucket| (bucket.starts_at, bucket.token_usage.total_tokens))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(totals.get(&bucket_two), Some(&30));
        assert_eq!(totals.get(&bucket_three), Some(&12));
        assert_eq!(totals.len(), 2);
    }

    #[test]
    fn conflicting_event_id_uses_deterministic_authority_and_warns() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, store) = stores(&root, &codex_home, RedactionProfile::Redacted);
        let source_a = source(
            SOURCE_A,
            "alpha",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let source_b = source(
            SOURCE_B,
            "beta",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let range_start = at(2, 0, 0);
        let starts_at = at(2, 10, 0);
        let project_a = observed('a');
        let project_b = observed('b');
        install_local_replica(
            &store,
            &source_a,
            bucket(starts_at, 10, project_a.as_str()),
            session_digest(
                source_a.source_id(),
                range_start,
                'a',
                10,
                1,
                project_a.clone(),
            ),
            vec![usage_fact(
                source_a.source_id(),
                "event-conflict",
                starts_at + Duration::minutes(1),
                10,
                project_a,
            )],
        );
        install_local_replica(
            &store,
            &source_b,
            bucket(starts_at, 12, project_b.as_str()),
            session_digest(
                source_b.source_id(),
                range_start,
                'b',
                12,
                1,
                project_b.clone(),
            ),
            vec![usage_fact(
                source_b.source_id(),
                "event-conflict",
                starts_at + Duration::minutes(1),
                12,
                project_b,
            )],
        );
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &store, range_start).unwrap();
        assert_eq!(
            result.history.half_hour_buckets[0].token_usage.total_tokens,
            10
        );
        assert!(
            result
                .history
                .warnings
                .contains(&DUPLICATE_SESSION_FACT_CONFLICT_WARNING.to_string())
        );
    }

    #[test]
    fn weekly_projection_combines_persisted_baseline_and_remote_bucket_delta() {
        let resets_at = at(8, 0, 0);
        let baseline_at = at(2, 0, 0);
        let next_bucket = at(2, 12, 0);
        let sources = vec![
            SourceSlice {
                metadata: source(
                    SOURCE_A,
                    "alpha",
                    SourceKind::Local,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![bucket(next_bucket, 10, "alpha")],
                weekly_local_points: vec![weekly(baseline_at, resets_at, 100)],
            },
            SourceSlice {
                metadata: source(
                    SOURCE_B,
                    "beta",
                    SourceKind::Ssh,
                    RedactionProfile::Redacted,
                ),
                buckets: vec![bucket(next_bucket, 20, "beta")],
                weekly_local_points: Vec::new(),
            },
        ];

        let result =
            aggregate_source_weekly_points(&sources, &[quota(at(1, 0, 0), resets_at)], at(1, 0, 0))
                .unwrap();
        assert_eq!(result.first().unwrap().token_usage.total_tokens, 100);
        let latest = result.last().unwrap();
        assert_eq!(latest.observed_at, next_bucket + Duration::minutes(15));
        assert_eq!(latest.token_usage.total_tokens, 130);
        assert_eq!(latest.estimated_cost_units, 130);
        assert!(
            latest
                .partial_reasons
                .contains(&"remote_weekly_from_buckets_lower_bound".to_string())
        );
    }

    #[test]
    fn weekly_projection_advances_each_bucket_once_per_cycle() {
        let resets_at = at(8, 0, 0);
        let cycle_starts_at = at(1, 0, 0);
        let mut buckets = Vec::new();
        for index in 0..96_i64 {
            buckets.push(bucket(
                cycle_starts_at + Duration::minutes(index * 15),
                1,
                "local",
            ));
        }
        let sources = vec![SourceSlice {
            metadata: source(
                SOURCE_A,
                "local",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets,
            weekly_local_points: vec![weekly(cycle_starts_at, resets_at, 10)],
        }];

        let (points, work) = aggregate_source_weekly_points_with_work(
            &sources,
            &[quota(cycle_starts_at, resets_at)],
            cycle_starts_at,
        )
        .unwrap();

        assert_eq!(work.reset_cycles, 1);
        assert_eq!(work.bucket_advances, 96);
        assert_eq!(work.weekly_advances, 1);
        assert_eq!(work.source_evaluations, work.timeline_points);
        assert_eq!(points.last().unwrap().token_usage.total_tokens, 106);
        // The former implementation rescanned all 96 buckets for every one
        // of the 97 timeline points. The cursor bound is linear in inputs plus
        // emitted points, rather than their product.
        assert!(work.bucket_advances < work.timeline_points * 2);
    }

    #[test]
    fn weekly_projection_partitions_large_multi_cycle_inputs_once() {
        let first_cycle_start = at(1, 0, 0);
        let mut quota_points = Vec::new();
        let mut weekly_points = Vec::new();
        let mut buckets = Vec::new();
        for cycle in 0_i64..4 {
            let cycle_start = first_cycle_start + Duration::days(cycle * 7);
            let resets_at = cycle_start + Duration::days(7);
            quota_points.push(quota(cycle_start, resets_at));
            weekly_points.push(weekly(cycle_start, resets_at, 10));
            for bucket_index in 0_i64..96 {
                buckets.push(bucket(
                    cycle_start + Duration::minutes(bucket_index * 15),
                    1,
                    "local",
                ));
            }
        }
        let expected_weekly_evaluations = weekly_points.len();
        let expected_bucket_evaluations = buckets.len();
        let source = SourceSlice {
            metadata: source(
                SOURCE_A,
                "local",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets,
            weekly_local_points: weekly_points,
        };

        let (_, work) =
            aggregate_source_weekly_points_with_work(&[source], &quota_points, first_cycle_start)
                .unwrap();
        assert_eq!(work.reset_cycles, 4);
        assert_eq!(
            work.weekly_partition_evaluations,
            expected_weekly_evaluations
        );
        assert_eq!(
            work.bucket_partition_evaluations,
            expected_bucket_evaluations
        );
        assert_eq!(work.weekly_advances, expected_weekly_evaluations);
        assert_eq!(work.bucket_advances, expected_bucket_evaluations);
    }

    #[test]
    fn weekly_projection_cursor_matches_reference_rescan_semantics() {
        let cycle_starts_at = at(1, 0, 0);
        let resets_at = at(8, 0, 0);
        let mut buckets = Vec::new();
        for index in 0..12_i64 {
            buckets.push(bucket(
                cycle_starts_at + Duration::minutes(index * 15),
                u64::try_from(index + 1).unwrap(),
                "local",
            ));
        }
        let source = SourceSlice {
            metadata: source(
                SOURCE_A,
                "local",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets,
            weekly_local_points: vec![
                weekly(cycle_starts_at + Duration::minutes(37), resets_at, 100),
                weekly(cycle_starts_at + Duration::minutes(92), resets_at, 200),
            ],
        };
        let mut timeline = source
            .buckets
            .iter()
            .map(|bucket| bucket.ends_at)
            .chain(
                source
                    .weekly_local_points
                    .iter()
                    .map(|point| point.observed_at),
            )
            .collect::<BTreeSet<_>>();
        let mut cursor = WeeklySourceCursor::new(&source, cycle_starts_at, resets_at);
        let mut work = WeeklyAggregationWork::default();
        for observed_at in std::mem::take(&mut timeline) {
            assert_eq!(
                cursor.advance_to(observed_at, &mut work),
                source_weekly_cumulative_at(&source, cycle_starts_at, resets_at, observed_at),
                "cursor diverged at {observed_at}"
            );
        }
    }

    #[test]
    fn rolling_zero_usage_reset_estimates_collapse_to_the_latest_candidate() {
        let first_observed = at(1, 0, 0);
        let mut quota_points = Vec::new();
        for index in 0..36_i64 {
            let observed_at = first_observed + Duration::minutes(index * 5);
            let mut point = quota(observed_at, observed_at + Duration::days(7));
            point.used_percent = 0.0;
            point.remaining_percent = 100.0;
            quota_points.push(point);
        }

        let resets = canonical_weekly_resets(&[], &quota_points).unwrap();
        assert_eq!(resets.len(), 1);
        assert_eq!(
            resets[0],
            quota_points
                .last()
                .expect("latest rolling estimate")
                .resets_at
        );
    }

    #[test]
    fn weekly_reset_clustering_handles_large_duplicate_input_and_rejects_cycle_overflow() {
        let observed_at = at(1, 0, 0);
        let first_reset = observed_at + Duration::days(7);
        let dense = (0_i64..100_000)
            .map(|offset| quota(observed_at, first_reset + Duration::seconds(offset % 120)))
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_weekly_resets(&[], &dense).unwrap(),
            vec![first_reset]
        );

        let at_limit = (0..MAX_WEEKLY_RESET_CYCLES)
            .map(|offset| {
                quota(
                    observed_at,
                    first_reset
                        + Duration::seconds(
                            i64::try_from(offset * (RESET_DRIFT_SECONDS as usize + 1)).unwrap(),
                        ),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_weekly_resets(&[], &at_limit).unwrap().len(),
            MAX_WEEKLY_RESET_CYCLES
        );

        let mut above_limit = at_limit;
        above_limit.push(quota(
            observed_at,
            first_reset
                + Duration::seconds(
                    i64::try_from(MAX_WEEKLY_RESET_CYCLES * (RESET_DRIFT_SECONDS as usize + 1))
                        .unwrap(),
                ),
        ));
        let error = canonical_weekly_resets(&[], &above_limit).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("canonical reset cycles"));
    }

    #[test]
    fn logical_replica_markers_keep_rolling_zero_resets_canonical() {
        let first_observed = at(1, 0, 0);
        let mut rolling_points = Vec::new();
        for index in 0..3_i64 {
            let observed_at = first_observed + Duration::minutes(index * 30);
            let mut point = weekly(observed_at, observed_at + Duration::days(7), 0);
            point.call_count = 0;
            rolling_points.push(point);
        }
        let mut sources = vec![SourceSlice {
            metadata: source(
                SOURCE_A,
                "local",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets: Vec::new(),
            weekly_local_points: rolling_points,
        }];
        let account_observed = first_observed + Duration::minutes(90);
        let mut account_point = quota(account_observed, account_observed + Duration::days(7));
        account_point.used_percent = 0.0;
        account_point.remaining_percent = 100.0;
        let account_quota = vec![account_point];

        let canonical = canonical_weekly_resets(&sources, &account_quota).unwrap();
        assert_eq!(canonical, vec![account_observed + Duration::days(7)]);

        // Logical replica handling discards physical weekly baselines. It must
        // rebuild only the already-canonical cycles, not turn every rolling
        // zero estimate into a durable-looking anchored cycle marker.
        replace_weekly_baselines_with_cycle_markers(&mut sources, &canonical);
        assert_eq!(sources[0].weekly_local_points.len(), 1);
        assert_eq!(
            canonical_weekly_resets(&sources, &account_quota).unwrap(),
            canonical
        );
        let (_, work) =
            aggregate_source_weekly_points_with_work(&sources, &account_quota, first_observed)
                .unwrap();
        assert_eq!(work.reset_cycles, 1);
    }

    #[test]
    fn anchored_reset_is_retained_alongside_the_latest_idle_cycle() {
        let anchored_reset = at(8, 0, 0);
        let mut anchored = quota(at(2, 0, 0), anchored_reset);
        anchored.used_percent = 1.0;
        anchored.remaining_percent = 99.0;
        let idle_observed = at(9, 0, 0);
        let mut idle = quota(idle_observed, idle_observed + Duration::days(7));
        idle.used_percent = 0.0;
        idle.remaining_percent = 100.0;

        let resets = canonical_weekly_resets(&[], &[anchored, idle.clone()]).unwrap();
        assert_eq!(resets, vec![anchored_reset, idle.resets_at]);
    }

    #[test]
    fn local_weekly_without_a_baseline_is_partial_when_cycle_coverage_has_a_gap() {
        let cycle_starts_at = at(1, 0, 0);
        let resets_at = at(8, 0, 0);
        let starts_at = at(2, 12, 0);
        let source = SourceSlice {
            metadata: source(
                SOURCE_A,
                "local",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets: vec![bucket(starts_at, 10, "local")],
            weekly_local_points: Vec::new(),
        };

        let aggregate = source_weekly_cumulative_at(
            &source,
            cycle_starts_at,
            resets_at,
            starts_at + Duration::minutes(15),
        )
        .unwrap();

        assert!(
            aggregate
                .partial_reasons
                .contains("local_weekly_from_buckets_lower_bound")
        );
    }

    #[test]
    fn local_weekly_without_a_baseline_can_prove_contiguous_closed_bucket_coverage() {
        let cycle_starts_at = at(1, 0, 0);
        let resets_at = at(8, 0, 0);
        let source = SourceSlice {
            metadata: source(
                SOURCE_A,
                "local",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets: vec![bucket(cycle_starts_at, 10, "local")],
            weekly_local_points: Vec::new(),
        };

        let aggregate = source_weekly_cumulative_at(
            &source,
            cycle_starts_at,
            resets_at,
            cycle_starts_at + Duration::minutes(15),
        )
        .unwrap();

        assert!(
            !aggregate
                .partial_reasons
                .contains("local_weekly_from_buckets_lower_bound")
        );
    }

    #[test]
    fn v1_active_and_migrating_never_read_existing_v2_data() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::PreviewEnabled);
        let starts_at = at(2, 10, 0);
        legacy
            .record(&HistoryObservation {
                observed_at: starts_at + Duration::minutes(15),
                half_hour_buckets: vec![bucket(starts_at, 10, "legacy")],
                ..HistoryObservation::default()
            })
            .unwrap();
        let v1 = initialize_v1(&ownership);

        let remote = source(
            SOURCE_A,
            "future-v2",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        source_history.save_source_metadata(&remote).unwrap();
        source_history
            .record_source_bucket_changes(
                remote.source_id(),
                RedactionProfile::Redacted,
                &[SourceBucketRecord::upsert(1, bucket(starts_at, 99, "v2")).unwrap()],
            )
            .unwrap();

        let active =
            load_unified_history_since(&ownership, &mut legacy, &source_history, starts_at)
                .unwrap();
        assert_eq!(active.backend, UnifiedHistoryBackend::V1);
        assert_eq!(
            active.history.half_hour_buckets[0].token_usage.total_tokens,
            10
        );

        let lease = ownership.acquire_writer_lease().unwrap();
        let migrating = match ownership.begin_migration(&lease, &v1).unwrap() {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(current) => panic!("unexpected conflict: {current:?}"),
        };
        let during =
            load_unified_history_since(&ownership, &mut legacy, &source_history, starts_at)
                .unwrap();
        assert_eq!(during.backend, UnifiedHistoryBackend::V1);
        assert_eq!(during.ownership_epoch, migrating.epoch());
        assert_eq!(
            during.history.half_hour_buckets[0].token_usage.total_tokens,
            10
        );
    }

    #[test]
    fn v2_active_aggregates_sources_and_account_without_v1_leakage() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::PreviewEnabled);
        let starts_at = at(2, 10, 0);
        legacy
            .record(&HistoryObservation {
                observed_at: starts_at + Duration::minutes(15),
                half_hour_buckets: vec![bucket(starts_at, 1_000, "legacy")],
                ..HistoryObservation::default()
            })
            .unwrap();

        for (id, label, total, redaction) in [
            (SOURCE_A, "alpha", 10, RedactionProfile::Redacted),
            (SOURCE_B, "beta", 20, RedactionProfile::PreviewEnabled),
        ] {
            let metadata = source(id, label, SourceKind::Local, redaction);
            let mut physical_bucket = bucket(starts_at, total, label);
            physical_bucket.project_groups[0].thread_id = format!("thread-{label}");
            physical_bucket.project_groups[0].session_thread_id =
                Some(physical_bucket.project_groups[0].thread_id.clone());
            source_history.save_source_metadata(&metadata).unwrap();
            source_history
                .record_source_bucket_changes(
                    metadata.source_id(),
                    redaction,
                    &[SourceBucketRecord::upsert(1, physical_bucket).unwrap()],
                )
                .unwrap();
        }
        let resets_at = at(8, 0, 0);
        source_history
            .record_account_points(&[quota(at(2, 9, 0), resets_at)])
            .unwrap();
        let active = activate_v2(&ownership);
        let lease = ownership.acquire_writer_lease().unwrap();
        let authority = ownership.authorize_v2_write(&lease, &active).unwrap();
        source_history
            .writer(&authority)
            .unwrap()
            .mark_v2_summary_backfill_attempt(at(2, 9, 30), true)
            .unwrap();

        let result =
            load_unified_history_since(&ownership, &mut legacy, &source_history, at(2, 0, 0))
                .unwrap();
        assert_eq!(result.backend, UnifiedHistoryBackend::V2);
        assert_eq!(result.ownership_epoch, active.epoch());
        assert_eq!(result.included_sources.len(), 2);
        assert_eq!(result.history.quota_points.len(), 1);
        assert_eq!(
            result.history.summary_backfill_attempted_at,
            Some(at(2, 9, 30))
        );
        assert_eq!(result.history.summary_backfill_attempt_complete, Some(true));
        assert_eq!(result.history.half_hour_buckets.len(), 1);
        assert_eq!(
            result.history.half_hour_buckets[0].token_usage.total_tokens,
            30
        );
        assert!(
            !result
                .history
                .warnings
                .contains(&CROSS_SOURCE_DUPLICATE_WARNING.to_string())
        );
        assert!(
            result
                .history
                .warnings
                .contains(&PROJECT_MAPPING_PARTIAL_WARNING.to_string())
        );
        assert!(
            result
                .history
                .half_hour_buckets
                .iter()
                .all(|bucket| bucket.token_usage.total_tokens != 1_000)
        );
    }

    #[test]
    fn v2_source_selection_filters_before_aggregation_and_keeps_global_quota() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::PreviewEnabled);
        let starts_at = at(2, 10, 0);
        let local = source(
            SOURCE_A,
            "local",
            SourceKind::Local,
            RedactionProfile::PreviewEnabled,
        );
        let remote = source(
            SOURCE_B,
            "remote",
            SourceKind::Ssh,
            RedactionProfile::PreviewEnabled,
        );
        source_history.save_source_metadata(&local).unwrap();
        let mut local_bucket = bucket(starts_at, 10, "local");
        local_bucket.project_groups[0].thread_id = "thread-local".to_string();
        local_bucket.project_groups[0].session_thread_id = Some("thread-local".to_string());
        source_history
            .record_source_bucket_changes(
                local.source_id(),
                RedactionProfile::PreviewEnabled,
                &[SourceBucketRecord::upsert(1, local_bucket).unwrap()],
            )
            .unwrap();
        let reset = at(8, 0, 0);
        source_history
            .record_account_points(&[quota(at(2, 9, 0), reset)])
            .unwrap();
        let active = activate_v2(&ownership);
        install_remote_bucket(&ownership, &source_history, &active, &remote, starts_at, 20);

        let local_id = local.source_id().clone();
        let remote_id = remote.source_id().clone();
        let all = load_unified_history_since(&ownership, &mut legacy, &source_history, at(2, 0, 0))
            .unwrap();
        let local_only = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Local(local_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        let remote_only = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Remote(remote_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();

        assert_eq!(all.source_selection, HistorySourceSelection::AllIncluded);
        assert_eq!(
            all.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert_eq!(
            local_only.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert_eq!(
            remote_only.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert_eq!(
            all.included_sources,
            vec![local_id.clone(), remote_id.clone()]
        );
        assert_eq!(local_only.included_sources, vec![local_id]);
        assert_eq!(remote_only.included_sources, vec![remote_id]);
        assert_eq!(
            all.history.half_hour_buckets[0].token_usage.total_tokens,
            30
        );
        assert_eq!(
            local_only.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );
        assert_eq!(
            remote_only.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            20
        );
        assert_eq!(all.history.quota_points, local_only.history.quota_points);
        assert_eq!(all.history.quota_points, remote_only.history.quota_points);
        assert_eq!(all.history.quota_points.len(), 1);
        assert!(
            !all.history
                .warnings
                .contains(&CROSS_SOURCE_DUPLICATE_WARNING.to_string())
        );
        assert!(
            !local_only
                .history
                .warnings
                .contains(&CROSS_SOURCE_DUPLICATE_WARNING.to_string())
        );
        assert!(
            !remote_only
                .history
                .warnings
                .contains(&CROSS_SOURCE_DUPLICATE_WARNING.to_string())
        );
    }

    #[test]
    fn v2_unavailable_exact_selection_returns_quota_without_usage_or_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::PreviewEnabled);
        let starts_at = at(2, 10, 0);
        let local = source(
            SOURCE_A,
            "local",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        source_history.save_source_metadata(&local).unwrap();
        source_history
            .record_source_bucket_changes(
                local.source_id(),
                RedactionProfile::Redacted,
                &[SourceBucketRecord::upsert(1, bucket(starts_at, 10, "local")).unwrap()],
            )
            .unwrap();
        source_history
            .record_account_points(&[quota(at(2, 9, 0), at(8, 0, 0))])
            .unwrap();
        activate_v2(&ownership);

        let local_id = local.source_id().clone();
        let missing_id: NodeId = SOURCE_B.parse().unwrap();
        let missing = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Remote(missing_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            missing.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(HistorySourceUnavailableReason::NotFound)
        );
        assert!(missing.included_sources.is_empty());
        assert!(missing.history.half_hour_buckets.is_empty());
        assert!(missing.history.weekly_local_points.is_empty());
        assert_eq!(missing.history.quota_points.len(), 1);
        assert!(missing.history.warnings.contains(&format!(
            "{SOURCE_SELECTION_UNAVAILABLE_WARNING}:not_found:{missing_id}"
        )));

        let kind_mismatch = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Remote(local_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            kind_mismatch.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(HistorySourceUnavailableReason::KindMismatch)
        );
        assert!(kind_mismatch.history.half_hour_buckets.is_empty());
        assert_eq!(kind_mismatch.history.quota_points.len(), 1);

        let stale_local = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Local(missing_id),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            stale_local.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(
                HistorySourceUnavailableReason::LocalIdentityMismatch
            )
        );
        assert!(stale_local.history.half_hour_buckets.is_empty());
        assert_eq!(stale_local.history.quota_points.len(), 1);
    }

    #[test]
    fn v2_exact_selection_reads_excluded_source_but_keeps_it_out_of_all() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::Redacted);
        let preview_local = source(
            SOURCE_A,
            "preview-local",
            SourceKind::Local,
            RedactionProfile::PreviewEnabled,
        );
        let mut excluded_remote = source(
            SOURCE_B,
            "excluded-remote",
            SourceKind::Ssh,
            RedactionProfile::Redacted,
        );
        excluded_remote.set_include_in_aggregates(false);
        source_history.save_source_metadata(&preview_local).unwrap();
        source_history
            .save_source_metadata(&excluded_remote)
            .unwrap();
        source_history
            .record_account_points(&[quota(at(2, 9, 0), at(8, 0, 0))])
            .unwrap();

        // If selection policy were applied after source access this corrupt
        // preview shard would fail the redacted query.
        let preview_directory = source_history
            .source_buckets_directory(preview_local.source_id(), RedactionProfile::PreviewEnabled);
        prepare_state_root(&preview_directory);
        fs::write(preview_directory.join("2026-08-02.json"), b"not-json").unwrap();
        let active = activate_v2(&ownership);
        let starts_at = at(2, 10, 0);
        install_remote_bucket(
            &ownership,
            &source_history,
            &active,
            &excluded_remote,
            starts_at,
            42,
        );

        let local_id = preview_local.source_id().clone();
        let redaction_blocked = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Local(local_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            redaction_blocked.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(
                HistorySourceUnavailableReason::RedactionIncompatible
            )
        );
        assert_eq!(redaction_blocked.redaction_skipped_sources, vec![local_id]);
        assert!(redaction_blocked.history.half_hour_buckets.is_empty());
        assert_eq!(redaction_blocked.history.quota_points.len(), 1);

        let remote_id = excluded_remote.source_id().clone();
        let excluded = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            preview_local.source_id(),
            &HistorySourceSelection::Remote(remote_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            excluded.source_selection_status,
            HistorySourceSelectionStatus::AppliedExcludedFromAggregates
        );
        assert_eq!(excluded.included_sources, vec![remote_id.clone()]);
        assert_eq!(excluded.history.half_hour_buckets.len(), 1);
        assert_eq!(
            excluded.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            42
        );
        assert_eq!(excluded.history.quota_points.len(), 1);
        assert!(
            excluded
                .history
                .warnings
                .contains(&format!("{SOURCE_SELECTION_EXCLUDED_WARNING}:{remote_id}"))
        );

        let all = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            preview_local.source_id(),
            &HistorySourceSelection::AllIncluded,
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            all.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert!(all.included_sources.is_empty());
        assert!(all.history.half_hour_buckets.is_empty());
    }

    #[test]
    fn v2_exact_selection_does_not_open_an_unselected_corrupt_source() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::PreviewEnabled);
        let starts_at = at(2, 10, 0);
        let selected = source(
            SOURCE_A,
            "selected",
            SourceKind::Local,
            RedactionProfile::PreviewEnabled,
        );
        let corrupt = source(
            SOURCE_B,
            "corrupt",
            SourceKind::Local,
            RedactionProfile::PreviewEnabled,
        );
        source_history.save_source_metadata(&selected).unwrap();
        source_history.save_source_metadata(&corrupt).unwrap();
        source_history
            .record_source_bucket_changes(
                selected.source_id(),
                RedactionProfile::PreviewEnabled,
                &[SourceBucketRecord::upsert(1, bucket(starts_at, 10, "selected")).unwrap()],
            )
            .unwrap();
        let corrupt_directory = source_history
            .source_buckets_directory(corrupt.source_id(), RedactionProfile::PreviewEnabled);
        prepare_state_root(&corrupt_directory);
        fs::write(corrupt_directory.join("2026-08-02.json"), b"not-json").unwrap();
        activate_v2(&ownership);

        let selected_id = selected.source_id().clone();
        let result = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &selected_id,
            &HistorySourceSelection::Local(selected_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            result.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert_eq!(result.included_sources, vec![selected_id]);
        assert_eq!(
            result.history.half_hour_buckets[0].token_usage.total_tokens,
            10
        );
    }

    #[test]
    fn v1_remote_and_stale_local_selections_never_leak_local_usage() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::PreviewEnabled);
        let starts_at = at(2, 10, 0);
        let observation = HistoryObservation {
            observed_at: starts_at + Duration::minutes(15),
            quota_points: vec![quota(at(2, 9, 0), at(8, 0, 0))],
            half_hour_buckets: vec![bucket(starts_at, 99, "legacy-local")],
            ..HistoryObservation::default()
        };
        legacy.record(&observation).unwrap();
        initialize_v1(&ownership);
        let local_id: NodeId = SOURCE_A.parse().unwrap();
        let remote_id: NodeId = SOURCE_B.parse().unwrap();

        let local = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Local(local_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            local.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert_eq!(
            local.history.half_hour_buckets[0].token_usage.total_tokens,
            99
        );

        let remote = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Remote(remote_id.clone()),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            remote.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(
                HistorySourceUnavailableReason::UnsupportedByLegacy
            )
        );
        assert!(remote.history.half_hour_buckets.is_empty());
        assert!(remote.history.weekly_local_points.is_empty());
        assert_eq!(remote.history.quota_points, local.history.quota_points);
        assert!(remote.history.warnings.contains(&format!(
            "{SOURCE_SELECTION_UNAVAILABLE_WARNING}:unsupported_by_legacy:{remote_id}"
        )));

        let stale_local = load_unified_history_since_selected(
            &ownership,
            &mut legacy,
            &source_history,
            &local_id,
            &HistorySourceSelection::Local(remote_id),
            at(2, 0, 0),
        )
        .unwrap();
        assert_eq!(
            stale_local.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(
                HistorySourceUnavailableReason::LocalIdentityMismatch
            )
        );
        assert!(stale_local.history.half_hour_buckets.is_empty());
        assert_eq!(stale_local.history.quota_points, local.history.quota_points);
    }

    #[test]
    fn redacted_query_skips_preview_source_before_loading_its_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::Redacted);
        let starts_at = at(2, 10, 0);
        let redacted = source(
            SOURCE_A,
            "safe",
            SourceKind::Local,
            RedactionProfile::Redacted,
        );
        let preview = source(
            SOURCE_B,
            "preview",
            SourceKind::Local,
            RedactionProfile::PreviewEnabled,
        );
        source_history.save_source_metadata(&redacted).unwrap();
        source_history.save_source_metadata(&preview).unwrap();
        source_history
            .record_source_bucket_changes(
                redacted.source_id(),
                RedactionProfile::Redacted,
                &[SourceBucketRecord::upsert(1, bucket(starts_at, 10, "safe")).unwrap()],
            )
            .unwrap();
        // A corrupt preview shard would fail the query if the privacy gate
        // opened this namespace. It must remain untouched.
        let preview_directory = source_history
            .source_buckets_directory(preview.source_id(), RedactionProfile::PreviewEnabled);
        prepare_state_root(&preview_directory);
        fs::write(preview_directory.join("2026-08-02.json"), b"not-json").unwrap();
        activate_v2(&ownership);

        let result =
            load_unified_history_since(&ownership, &mut legacy, &source_history, starts_at)
                .unwrap();
        assert_eq!(result.included_sources, vec![SOURCE_A.parse().unwrap()]);
        assert_eq!(
            result.redaction_skipped_sources,
            vec![SOURCE_B.parse().unwrap()]
        );
        assert_eq!(result.history.half_hour_buckets.len(), 1);
        assert!(result.history.warnings.iter().any(|warning| warning
            == &format!("{REDACTED_QUERY_SKIPPED_PREVIEW_SOURCE_WARNING}:{SOURCE_B}")));
    }

    #[test]
    fn uninitialized_ownership_and_cross_bound_stores_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let other_root = directory.path().join("other-state");
        let codex_home = directory.path().join("codex-home");
        let (mut legacy, ownership, source_history) =
            stores(&root, &codex_home, RedactionProfile::PreviewEnabled);

        let uninitialized =
            load_unified_history_since(&ownership, &mut legacy, &source_history, at(1, 0, 0))
                .unwrap_err();
        assert_eq!(uninitialized.kind(), io::ErrorKind::NotFound);

        prepare_state_root(&other_root);
        let other_source = SourceHistoryStore::new(other_root, ownership.profile_id().clone());
        let mismatched =
            load_unified_history_since(&ownership, &mut legacy, &other_source, at(1, 0, 0))
                .unwrap_err();
        assert_eq!(mismatched.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn weekly_reset_clustering_prefers_account_timestamp_within_drift() {
        let account_reset = at(8, 0, 0);
        let source_reset = account_reset + Duration::seconds(90);
        let source = SourceSlice {
            metadata: source(
                SOURCE_A,
                "alpha",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets: Vec::new(),
            weekly_local_points: vec![weekly(at(2, 0, 0), source_reset, 10)],
        };
        let resets =
            canonical_weekly_resets(&[source], &[quota(at(1, 0, 0), account_reset)]).unwrap();
        assert_eq!(resets, vec![account_reset]);
        assert_eq!(account_reset.minute(), 0);
    }

    #[test]
    fn ambiguous_weekly_reset_is_assigned_to_only_one_canonical_cycle() {
        let earlier_reset = at(8, 0, 0);
        let later_reset = earlier_reset + Duration::seconds(200);
        let reported_reset = earlier_reset + Duration::seconds(100);
        let observed_at = at(2, 0, 0);
        let source = SourceSlice {
            metadata: source(
                SOURCE_A,
                "alpha",
                SourceKind::Local,
                RedactionProfile::Redacted,
            ),
            buckets: Vec::new(),
            weekly_local_points: vec![weekly(observed_at, reported_reset, 10)],
        };
        let account = vec![
            quota(at(1, 0, 0), earlier_reset),
            quota(at(1, 0, 1), later_reset),
        ];

        let canonical = canonical_weekly_resets(std::slice::from_ref(&source), &account).unwrap();
        assert_eq!(canonical, vec![earlier_reset, later_reset]);
        assert_eq!(
            assigned_canonical_reset(reported_reset, &canonical),
            Some(earlier_reset),
            "an exact-distance tie must prefer the earlier reset"
        );
        assert_eq!(
            assigned_canonical_reset(
                earlier_reset + Duration::seconds(RESET_DRIFT_SECONDS + 1),
                &[earlier_reset],
            ),
            None
        );

        let (points, work) =
            aggregate_source_weekly_points_with_work(&[source], &account, observed_at).unwrap();
        assert_eq!(work.weekly_advances, 1);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].resets_at, earlier_reset);
        assert_eq!(points[0].token_usage.total_tokens, 10);
    }

    #[test]
    fn source_weekly_tombstones_do_not_enter_live_projection_contract() {
        let observed_at = at(2, 0, 0);
        let resets_at = at(8, 0, 0);
        let upsert = SourceWeeklyRecord::upsert(1, weekly(observed_at, resets_at, 10)).unwrap();
        let tombstone = SourceWeeklyRecord::tombstone(observed_at, resets_at, 2).unwrap();
        assert!(matches!(
            upsert.change(),
            crate::source_history::SourceWeeklyChange::Upsert(_)
        ));
        assert!(matches!(
            tombstone.change(),
            crate::source_history::SourceWeeklyChange::Tombstone
        ));
    }
}
