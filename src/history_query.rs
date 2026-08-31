//! Ownership-selected, source-aware history reads.
//!
//! This module is deliberately read-only. It resolves the durable ownership
//! manifest before every query and never combines v1 and v2 data in one
//! result. During cutover an exact manifest change causes a bounded retry, so
//! a `Migrating -> V2Active` transition cannot return an accidental hybrid.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::str::FromStr;

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::domain::{ApiCostAmount, TokenUsage};
use crate::history::{
    HistoryData, HistoryStore, LocalHalfHourBucket, LocalProjectUsageGroup, LocalUsageGroup,
    QuotaPoint, WeeklyLocalPoint,
};
use crate::history_ownership::{
    HistoryOwnershipManifest, HistoryOwnershipState, HistoryOwnershipStore, OwnershipManifestStatus,
};
use crate::logical_replica::{
    ExpectedReplicaFactBinding, ReplicaCandidate, ReplicaCandidateKind, ReplicaDigestObservation,
    active_facts_cover_digest, detect_replica_candidates,
};
use crate::project_mapping::PROJECT_MAPPING_REGISTRATION_FAILED_WARNING;
use crate::project_mapping::{ProjectMappingProjection, ProjectMappingStore};
use crate::source_history::{
    ActiveFactSet, RedactionProfile, SourceBucketChange, SourceHistoryRemoteActiveRef,
    SourceHistoryStore, SourceKind, SourceMetadata, SourceSessionDigest, SourceSessionDigestChange,
    UsageEventFact,
};
use crate::source_identity::NodeId;
use crate::source_model::{ObservedProjectKey, ThreadId};

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
                    ownership.redaction_profile(),
                    before.epoch(),
                    source_history,
                    project_mapping,
                    bound_local_source_id,
                    selection,
                    since,
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

/// Returns `None` when source policy changed during the read.
fn load_v2_history_since(
    query_redaction: RedactionProfile,
    ownership_epoch: u64,
    store: &SourceHistoryStore,
    project_mapping: &LoadedProjectMappingProjection,
    bound_local_source_id: Option<&NodeId>,
    selection: &HistorySourceSelection,
    since: DateTime<Utc>,
) -> io::Result<Option<V2HistoryRead>> {
    let evidence_since = since
        .checked_sub_signed(Duration::days(QUERY_EVIDENCE_LOOKBACK_DAYS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let mut metadata_before = store.list_source_metadata()?;
    metadata_before
        .sort_by(|left, right| left.source_id().as_str().cmp(right.source_id().as_str()));

    // Account quota is global and intentionally loaded once, independently
    // of how many local or SSH sources participate.
    let account = store.load_account_since(evidence_since)?;
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
                    let snapshot = store.load_local_observation_snapshot_since(
                        metadata.source_id(),
                        source_redaction,
                        evidence_since,
                        detect_replicas,
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
                    let snapshot = store.load_remote_history_snapshot_since(
                        metadata.source_id(),
                        source_redaction,
                        evidence_since,
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

    let mut metadata_after = store.list_source_metadata()?;
    metadata_after.sort_by(|left, right| left.source_id().as_str().cmp(right.source_id().as_str()));
    if metadata_before != metadata_after {
        return Ok(None);
    }

    included_sources.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    redaction_skipped_sources.sort_by(|left, right| left.as_str().cmp(right.as_str()));

    let mut replica_report = LogicalReplicaReport::default();
    if matches!(selection, HistorySourceSelection::AllIncluded) {
        replica_report = resolve_logical_replicas(
            store,
            &mut slices,
            &mut replica_evidence,
            &project_mapping.projection,
        )?;
    }

    let weekly_local_points = aggregate_source_weekly_points(&slices, &account.quota_points, since);
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
    let bucket_projection = aggregate_source_buckets_with_logical_threads(
        &slices,
        &project_mapping.projection,
        &replica_report.logical_threads,
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

struct BucketProjection {
    buckets: Vec<LocalHalfHourBucket>,
    project_observations: bool,
    unmapped_projects: bool,
}

type LogicalThreadProjection = BTreeMap<(String, String), String>;

#[derive(Default)]
struct LogicalReplicaReport {
    logical_threads: LogicalThreadProjection,
    warnings: Vec<String>,
}

#[derive(Clone)]
struct ReplicaParticipant {
    source_index: usize,
    digest: SourceSessionDigest,
    exact_fact_coverage: bool,
}

struct ReplicaResolution {
    participants: Vec<ReplicaParticipant>,
    authority_index: usize,
    union_facts: Option<Vec<(usize, UsageEventFact)>>,
    fact_conflict: bool,
    project_conflict: bool,
}

#[derive(Clone, Copy, Debug)]
struct BucketProjectResidual {
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    call_count: u64,
}

fn resolve_logical_replicas(
    store: &SourceHistoryStore,
    slices: &mut [SourceSlice],
    evidence: &mut [SourceReplicaEvidence],
    project_mapping: &ProjectMappingProjection,
) -> io::Result<LogicalReplicaReport> {
    // Bucket project groups are an independent replica signal. In particular,
    // a v1 -> v2 migration can persist buckets before its matching digest, so
    // digest-only candidate detection is not sufficient to make an all-source
    // query additive-safe.
    let observed_thread_sources = observe_thread_sources(slices);
    let candidates = detect_replica_candidates(evidence.iter().flat_map(|source| {
        source
            .digests
            .iter()
            .map(|digest| ReplicaDigestObservation {
                source_id: &source.source_id,
                digest,
            })
    }));
    if candidates.is_empty()
        && !observed_thread_sources
            .values()
            .any(|source_indices| source_indices.len() > 1)
    {
        return Ok(LogicalReplicaReport::default());
    }

    // Facts are a local persistence read. The query path never starts SSH or
    // any other network operation. Only divergent candidates need this read.
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.kind() == ReplicaCandidateKind::NeedsFacts)
    {
        for source_id in candidate.source_ids() {
            let Some(source) = evidence
                .iter_mut()
                .find(|source| &source.source_id == source_id)
            else {
                continue;
            };
            if source.active_facts.contains_key(candidate.thread_id()) {
                continue;
            }
            if let Ok(Some(active)) = store.load_active_fact_set(
                source_id,
                source.redaction_profile,
                candidate.thread_id(),
            ) {
                source
                    .active_facts
                    .insert(candidate.thread_id().clone(), active);
            }
        }
    }

    let mut report = LogicalReplicaReport::default();
    let mut touched = BTreeMap::<(usize, DateTime<Utc>), BucketProjectResidual>::new();
    let mut handled_ranges =
        BTreeMap::<(String, usize), Vec<(DateTime<Utc>, DateTime<Utc>)>>::new();
    let mut thread_authorities = BTreeMap::<String, usize>::new();
    for candidate in &candidates {
        let Some(resolution) = plan_replica_resolution(candidate, evidence, project_mapping) else {
            continue;
        };
        let thread_id = candidate.thread_id();
        let thread_key = thread_id.as_str().to_owned();
        thread_authorities
            .entry(thread_key.clone())
            .or_insert(resolution.authority_index);
        let mut candidate_source_indices = resolution
            .participants
            .iter()
            .map(|participant| participant.source_index)
            .collect::<BTreeSet<_>>();
        if let Some(observed_sources) = observed_thread_sources.get(&thread_key) {
            candidate_source_indices.extend(observed_sources.iter().copied().filter(
                |source_index| {
                    source_has_thread_group_in_range(
                        &slices[*source_index],
                        thread_id,
                        candidate.range_start(),
                        candidate.range_end(),
                    )
                },
            ));
        }
        let logical_id = format!("logical-thread:{}", candidate.thread_id().as_str());
        for source_index in &candidate_source_indices {
            report.logical_threads.insert(
                (
                    evidence[*source_index].source_id.as_str().to_owned(),
                    candidate.thread_id().as_str().to_owned(),
                ),
                logical_id.clone(),
            );
            handled_ranges
                .entry((thread_key.clone(), *source_index))
                .or_default()
                .push((candidate.range_start(), candidate.range_end()));
        }

        let participant_coverage = resolution
            .participants
            .iter()
            .map(|participant| {
                (
                    participant.source_index,
                    replica_groups_cover_digest(
                        &slices[participant.source_index],
                        thread_id,
                        &participant.digest,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut conservative_lower_bound = candidate_source_indices
            .iter()
            .any(|source_index| !participant_coverage.contains_key(source_index));

        match &resolution.union_facts {
            Some(facts) => {
                for participant in &resolution.participants {
                    if participant_coverage[&participant.source_index] {
                        remove_replica_groups(
                            participant.source_index,
                            &mut slices[participant.source_index],
                            thread_id,
                            participant.digest.range_start(),
                            participant.digest.range_end(),
                            &mut touched,
                        );
                    } else {
                        conservative_lower_bound = true;
                        preserve_unrelated_project_groups_in_replica_range(
                            participant.source_index,
                            &mut slices[participant.source_index],
                            thread_id,
                            participant.digest.range_start(),
                            participant.digest.range_end(),
                            &mut touched,
                        );
                    }
                }
                for source_index in candidate_source_indices
                    .iter()
                    .copied()
                    .filter(|source_index| !participant_coverage.contains_key(source_index))
                {
                    suppress_unbound_replica_range(
                        source_index,
                        &mut slices[source_index],
                        thread_id.as_str(),
                        candidate.range_start(),
                        candidate.range_end(),
                        &mut touched,
                    );
                }
                for (source_index, fact) in facts {
                    add_fact_group(
                        *source_index,
                        &mut slices[*source_index],
                        fact,
                        &mut touched,
                    )?;
                }
                if resolution.fact_conflict {
                    report
                        .warnings
                        .push(DUPLICATE_SESSION_FACT_CONFLICT_WARNING.to_string());
                }
            }
            None => {
                if let Some(authority) = resolution
                    .participants
                    .iter()
                    .find(|participant| participant.source_index == resolution.authority_index)
                {
                    ensure_authority_project_consistency(
                        resolution.authority_index,
                        &mut slices[resolution.authority_index],
                        authority.digest.range_start(),
                        authority.digest.range_end(),
                        &mut touched,
                    );
                }
                for participant in &resolution.participants {
                    if participant.source_index == resolution.authority_index {
                        continue;
                    }
                    if participant_coverage[&participant.source_index] {
                        remove_replica_groups(
                            participant.source_index,
                            &mut slices[participant.source_index],
                            thread_id,
                            participant.digest.range_start(),
                            participant.digest.range_end(),
                            &mut touched,
                        );
                    } else {
                        conservative_lower_bound = true;
                        preserve_unrelated_project_groups_in_replica_range(
                            participant.source_index,
                            &mut slices[participant.source_index],
                            thread_id,
                            participant.digest.range_start(),
                            participant.digest.range_end(),
                            &mut touched,
                        );
                    }
                }
                for source_index in candidate_source_indices
                    .iter()
                    .copied()
                    .filter(|source_index| !participant_coverage.contains_key(source_index))
                {
                    suppress_unbound_replica_range(
                        source_index,
                        &mut slices[source_index],
                        thread_id.as_str(),
                        candidate.range_start(),
                        candidate.range_end(),
                        &mut touched,
                    );
                }
                if candidate.kind() == ReplicaCandidateKind::NeedsFacts {
                    report
                        .warnings
                        .push(DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string());
                }
            }
        }
        if conservative_lower_bound {
            report
                .warnings
                .push(DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string());
        }
        if resolution.project_conflict {
            report
                .warnings
                .push(DUPLICATE_SESSION_PROJECT_CONFLICT_WARNING.to_string());
        }
    }

    // Reconcile observations that were not backed by a cross-source digest
    // candidate. This includes a source whose digest is temporarily missing
    // and sessions imported from the v1 bucket family only. Keep one stable
    // authority and remove all other physical copies. Fully decomposed buckets
    // retain their provable non-session groups; an opaque bucket is zeroed so
    // uncertainty can only make the aggregate a lower bound, never a double
    // count.
    for (thread_key, source_indices) in &observed_thread_sources {
        if source_indices.len() < 2 {
            continue;
        }
        let authority_index = thread_authorities
            .get(thread_key)
            .copied()
            .unwrap_or_else(|| {
                choose_observed_thread_authority(thread_key, source_indices, evidence)
            });
        let logical_id = format!("logical-thread:{thread_key}");
        for source_index in source_indices {
            report.logical_threads.insert(
                (
                    evidence[*source_index].source_id.as_str().to_owned(),
                    thread_key.clone(),
                ),
                logical_id.clone(),
            );
        }

        let mut suppressed_unbound_copy = false;
        for source_index in source_indices
            .iter()
            .copied()
            .filter(|source_index| *source_index != authority_index)
        {
            let ranges = handled_ranges
                .get(&(thread_key.clone(), source_index))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if !source_has_unhandled_thread_group(&slices[source_index], thread_key, ranges) {
                continue;
            }
            suppress_unbound_replica_outside_ranges(
                source_index,
                &mut slices[source_index],
                thread_key,
                ranges,
                &mut touched,
            );
            suppressed_unbound_copy = true;
        }
        if suppressed_unbound_copy {
            mark_unhandled_authority_lower_bound(
                &mut slices[authority_index],
                thread_key,
                handled_ranges
                    .get(&(thread_key.clone(), authority_index))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            );
            report
                .warnings
                .push(DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING.to_string());
        }
    }

    for ((source_index, starts_at), residual) in touched {
        if let Some(bucket) = slices[source_index]
            .buckets
            .iter_mut()
            .find(|bucket| bucket.starts_at == starts_at)
        {
            rebuild_bucket_from_project_groups(bucket, residual);
        }
    }
    if !report.logical_threads.is_empty() {
        // A persisted weekly baseline has no per-thread decomposition. The
        // queried bucket window includes the preceding full cycle, so derive
        // the logical projection from adjusted buckets instead of retaining a
        // physically duplicated baseline.
        replace_weekly_baselines_with_cycle_markers(slices);
    }
    report.warnings.sort();
    report.warnings.dedup();
    Ok(report)
}

fn ensure_authority_project_consistency(
    _source_index: usize,
    source: &mut SourceSlice,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    _touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    for bucket in source
        .buckets
        .iter_mut()
        .filter(|bucket| bucket.starts_at >= range_start && bucket.starts_at < range_end)
    {
        if project_groups_match_bucket(bucket) {
            continue;
        }
        bucket
            .partial_reasons
            .push(DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string());
        bucket
            .partial_reasons
            .push(DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string());
    }
}

fn observe_thread_sources(slices: &[SourceSlice]) -> BTreeMap<String, BTreeSet<usize>> {
    let mut observations = BTreeMap::<String, BTreeSet<usize>>::new();
    for (source_index, source) in slices.iter().enumerate() {
        for thread_id in source.buckets.iter().flat_map(|bucket| {
            bucket
                .project_groups
                .iter()
                .map(|group| group.thread_id.as_str())
                .filter(|thread_id| !thread_id.is_empty())
        }) {
            observations
                .entry(thread_id.to_owned())
                .or_default()
                .insert(source_index);
        }
    }
    observations
}

fn source_has_thread_group_in_range(
    source: &SourceSlice,
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> bool {
    source.buckets.iter().any(|bucket| {
        bucket.starts_at >= range_start
            && bucket.starts_at < range_end
            && bucket
                .project_groups
                .iter()
                .any(|group| group.thread_id == thread_id.as_str())
    })
}

fn choose_observed_thread_authority(
    thread_id: &str,
    source_indices: &BTreeSet<usize>,
    evidence: &[SourceReplicaEvidence],
) -> usize {
    let mut digest_authority: Option<(usize, &SourceSessionDigest)> = None;
    for source_index in source_indices {
        for digest in evidence[*source_index]
            .digests
            .iter()
            .filter(|digest| digest.replica().thread_id().as_str() == thread_id)
        {
            let replace = digest_authority.is_none_or(|(best_index, best_digest)| {
                authority_is_better(
                    &evidence[*source_index],
                    digest,
                    active_source_facts_cover_digest(&evidence[*source_index], digest),
                    &evidence[best_index],
                    best_digest,
                    active_source_facts_cover_digest(&evidence[best_index], best_digest),
                )
            });
            if replace {
                digest_authority = Some((*source_index, digest));
            }
        }
    }
    digest_authority.map_or_else(
        || {
            *source_indices
                .iter()
                .min_by_key(|source_index| evidence[**source_index].source_id.as_str())
                .expect("cross-source observation contains a source")
        },
        |(source_index, _)| source_index,
    )
}

fn timestamp_in_ranges(
    timestamp: DateTime<Utc>,
    ranges: &[(DateTime<Utc>, DateTime<Utc>)],
) -> bool {
    ranges
        .iter()
        .any(|(range_start, range_end)| timestamp >= *range_start && timestamp < *range_end)
}

fn source_has_unhandled_thread_group(
    source: &SourceSlice,
    thread_id: &str,
    handled_ranges: &[(DateTime<Utc>, DateTime<Utc>)],
) -> bool {
    source.buckets.iter().any(|bucket| {
        !timestamp_in_ranges(bucket.starts_at, handled_ranges)
            && bucket
                .project_groups
                .iter()
                .any(|group| group.thread_id == thread_id)
    })
}

fn mark_unhandled_authority_lower_bound(
    source: &mut SourceSlice,
    thread_id: &str,
    handled_ranges: &[(DateTime<Utc>, DateTime<Utc>)],
) {
    for bucket in &mut source.buckets {
        if timestamp_in_ranges(bucket.starts_at, handled_ranges)
            || !bucket
                .project_groups
                .iter()
                .any(|group| group.thread_id == thread_id)
        {
            continue;
        }
        bucket
            .partial_reasons
            .push(DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string());
    }
}

fn replace_weekly_baselines_with_cycle_markers(slices: &mut [SourceSlice]) {
    let resets = slices
        .iter()
        .flat_map(|source| {
            source
                .weekly_local_points
                .iter()
                .map(|point| point.resets_at)
        })
        .collect::<BTreeSet<_>>();
    for source in slices.iter_mut() {
        source.weekly_local_points.clear();
    }
    let Some(marker_source) = slices.first_mut() else {
        return;
    };
    for resets_at in resets {
        let observed_at = resets_at
            .checked_sub_signed(Duration::minutes(WEEKLY_WINDOW_MINUTES))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        marker_source.weekly_local_points.push(WeeklyLocalPoint {
            observed_at,
            resets_at,
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
            call_count: 0,
            partial_reasons: vec![DUPLICATE_SESSION_WEEKLY_REBUILT_FROM_BUCKETS.to_string()],
        });
    }
}

fn plan_replica_resolution(
    candidate: &ReplicaCandidate,
    evidence: &[SourceReplicaEvidence],
    project_mapping: &ProjectMappingProjection,
) -> Option<ReplicaResolution> {
    let mut participants = Vec::new();
    for source_id in candidate.source_ids() {
        let source_index = evidence
            .iter()
            .position(|source| &source.source_id == source_id)?;
        let digest = evidence[source_index]
            .digests
            .iter()
            .find(|digest| {
                digest.replica().thread_id() == candidate.thread_id()
                    && digest.range_start() == candidate.range_start()
                    && digest.range_end() <= candidate.range_end()
            })?
            .clone();
        let exact_fact_coverage =
            active_source_facts_cover_digest(&evidence[source_index], &digest);
        participants.push(ReplicaParticipant {
            source_index,
            digest,
            exact_fact_coverage,
        });
    }
    if participants.len() < 2 {
        return None;
    }
    let authority_index = participants
        .iter()
        .map(|participant| participant.source_index)
        .reduce(|best, candidate_index| {
            let best_digest = participants
                .iter()
                .find(|participant| participant.source_index == best)
                .expect("authority index belongs to a participant");
            let candidate_digest = participants
                .iter()
                .find(|participant| participant.source_index == candidate_index)
                .expect("authority index belongs to a participant");
            if authority_is_better(
                &evidence[candidate_index],
                &candidate_digest.digest,
                candidate_digest.exact_fact_coverage,
                &evidence[best],
                &best_digest.digest,
                best_digest.exact_fact_coverage,
            ) {
                candidate_index
            } else {
                best
            }
        })?;

    let mut union_facts = None;
    let mut fact_conflict = false;
    let mut project_conflict =
        digest_project_attribution_conflicts(&participants, evidence, project_mapping);
    if candidate.kind() == ReplicaCandidateKind::NeedsFacts {
        let fact_sets = participants
            .iter()
            .map(|participant| {
                complete_compatible_facts(&evidence[participant.source_index], &participant.digest)
                    .map(|facts| (participant.source_index, facts))
            })
            .collect::<Option<Vec<_>>>();
        if let Some(fact_sets) = fact_sets
            && participant_revisions_compatible(&participants)
        {
            let mut events = BTreeMap::<String, (usize, UsageEventFact)>::new();
            for (source_index, facts) in fact_sets {
                for fact in facts {
                    let key = fact.event_id().as_str().to_owned();
                    match events.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert((source_index, fact));
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            let (existing_source, existing_fact) = entry.get();
                            let conflict = !facts_semantically_equal(existing_fact, &fact);
                            fact_conflict |= conflict;
                            project_conflict |= fact_project_attribution_conflicts(
                                project_mapping,
                                &evidence[*existing_source].source_id,
                                existing_fact,
                                &evidence[source_index].source_id,
                                &fact,
                            );
                            let existing_digest = participants
                                .iter()
                                .find(|participant| participant.source_index == *existing_source)
                                .expect("fact owner is a participant");
                            let incoming_digest = participants
                                .iter()
                                .find(|participant| participant.source_index == source_index)
                                .expect("fact owner is a participant");
                            if authority_is_better(
                                &evidence[source_index],
                                &incoming_digest.digest,
                                incoming_digest.exact_fact_coverage,
                                &evidence[*existing_source],
                                &existing_digest.digest,
                                existing_digest.exact_fact_coverage,
                            ) {
                                entry.insert((source_index, fact));
                            }
                        }
                    }
                }
            }
            union_facts = Some(events.into_values().collect());
        }
    }

    Some(ReplicaResolution {
        participants,
        authority_index,
        union_facts,
        fact_conflict,
        project_conflict,
    })
}

fn fact_project_attribution_conflicts(
    project_mapping: &ProjectMappingProjection,
    left_source: &NodeId,
    left: &UsageEventFact,
    right_source: &NodeId,
    right: &UsageEventFact,
) -> bool {
    let left = project_mapping.resolve(left_source, left.observed_project_key());
    let right = project_mapping.resolve(right_source, right.observed_project_key());
    match (left, right) {
        (Some(left), Some(right)) => left.aggregate_id() != right.aggregate_id(),
        _ => true,
    }
}

fn digest_project_attribution_conflicts(
    participants: &[ReplicaParticipant],
    evidence: &[SourceReplicaEvidence],
    project_mapping: &ProjectMappingProjection,
) -> bool {
    let mut expected: Option<BTreeSet<String>> = None;
    for participant in participants {
        let source_id = &evidence[participant.source_index].source_id;
        let mut aggregate_ids = BTreeSet::new();
        for observed_project_key in participant.digest.observed_project_keys() {
            let Some(project) = project_mapping.resolve(source_id, observed_project_key) else {
                return true;
            };
            aggregate_ids.insert(project.aggregate_id().as_str().to_owned());
        }
        if expected
            .as_ref()
            .is_some_and(|expected| expected != &aggregate_ids)
        {
            return true;
        }
        expected = Some(aggregate_ids);
    }
    false
}

fn participant_revisions_compatible(participants: &[ReplicaParticipant]) -> bool {
    let Some(first) = participants
        .first()
        .map(|participant| participant.digest.metrics())
    else {
        return false;
    };
    participants.iter().all(|participant| {
        let metrics = participant.digest.metrics();
        participant.digest.range_start() == participants[0].digest.range_start()
            && participant.digest.range_end() == participants[0].digest.range_end()
            && metrics.metric_revision == first.metric_revision
            && metrics.estimator_revision == first.estimator_revision
            && metrics.project_breakdown_revision == first.project_breakdown_revision
            && metrics.api_pricing_catalog_revision == first.api_pricing_catalog_revision
    })
}

fn complete_compatible_facts(
    source: &SourceReplicaEvidence,
    digest: &SourceSessionDigest,
) -> Option<Vec<UsageEventFact>> {
    let active = source.active_facts.get(digest.replica().thread_id())?;
    let expected_binding = source
        .active_remote_ref
        .as_ref()
        .map_or(ExpectedReplicaFactBinding::Local, |active_history| {
            ExpectedReplicaFactBinding::Remote(active_history.binding())
        });
    if !active_facts_cover_digest(digest, Some(active), expected_binding) {
        return None;
    }
    let facts = active
        .facts()
        .into_iter()
        .filter(|fact| {
            fact.occurred_at() >= digest.range_start() && fact.occurred_at() < digest.range_end()
        })
        .cloned()
        .collect::<Vec<_>>();
    Some(facts)
}

fn facts_semantically_equal(left: &UsageEventFact, right: &UsageEventFact) -> bool {
    left.event_id() == right.event_id()
        && left.occurred_at() == right.occurred_at()
        && left.emitting_turn_id() == right.emitting_turn_id()
        && left.parent_thread_id() == right.parent_thread_id()
        && left.project_session_thread_id() == right.project_session_thread_id()
        && left.root_session_thread_id() == right.root_session_thread_id()
        && left.root_session_turn_id() == right.root_session_turn_id()
        && left.model() == right.model()
        && left.service_tier() == right.service_tier()
        && left.digest_token_usage() == right.digest_token_usage()
        && left.request_usage_exact() == right.request_usage_exact()
        && left.exact_event_identity() == right.exact_event_identity()
        && left.metrics() == right.metrics()
}

fn authority_is_better(
    left_source: &SourceReplicaEvidence,
    left: &SourceSessionDigest,
    left_fact_coverage: bool,
    right_source: &SourceReplicaEvidence,
    right: &SourceSessionDigest,
    right_fact_coverage: bool,
) -> bool {
    let left_revisions = authority_revisions(left_source, left);
    let right_revisions = authority_revisions(right_source, right);
    let current = crate::remote_agent::current_revisions();
    let current_tuple = (
        current.history_format.get(),
        current.metric.get(),
        current.estimator.get(),
        current.project_breakdown.get(),
        current.api_pricing_catalog.get(),
    );
    let left_key = (
        left_revisions == current_tuple,
        left_revisions,
        left.exact_event_identity(),
        left.coverage_complete(),
        std::cmp::Reverse(hard_partial_count(left)),
        left_fact_coverage,
        left.covered_through(),
        left.range_end(),
    );
    let right_key = (
        right_revisions == current_tuple,
        right_revisions,
        right.exact_event_identity(),
        right.coverage_complete(),
        std::cmp::Reverse(hard_partial_count(right)),
        right_fact_coverage,
        right.covered_through(),
        right.range_end(),
    );
    left_key > right_key
        || (left_key == right_key
            && left_source.source_id.as_str() < right_source.source_id.as_str())
}

fn active_source_facts_cover_digest(
    source: &SourceReplicaEvidence,
    digest: &SourceSessionDigest,
) -> bool {
    let expected_binding = source
        .active_remote_ref
        .as_ref()
        .map_or(ExpectedReplicaFactBinding::Local, |active_history| {
            ExpectedReplicaFactBinding::Remote(active_history.binding())
        });
    active_facts_cover_digest(
        digest,
        source.active_facts.get(digest.replica().thread_id()),
        expected_binding,
    )
}

fn authority_revisions(
    source: &SourceReplicaEvidence,
    digest: &SourceSessionDigest,
) -> (u32, u32, u32, u32, u32) {
    let current = crate::remote_agent::current_revisions();
    let history = source
        .active_remote_ref
        .as_ref()
        .map(|active| active.binding().revisions().history_format.get())
        .unwrap_or_else(|| current.history_format.get());
    let metrics = digest.metrics();
    (
        history,
        metrics.metric_revision,
        metrics.estimator_revision,
        metrics.project_breakdown_revision,
        metrics.api_pricing_catalog_revision,
    )
}

fn hard_partial_count(digest: &SourceSessionDigest) -> usize {
    digest
        .metrics()
        .partial_reasons
        .iter()
        .filter(|reason| {
            matches!(
                reason.as_str(),
                "rollout_local_coverage_unverified"
                    | "coverage_starts_within_local_bucket"
                    | "rollout_scan_incomplete"
                    | "rollout_scan_truncated"
                    | "rollout_unreadable"
                    | "rollout_lines_skipped"
                    | "ambiguous_token_reset"
            )
        })
        .count()
}

fn remove_replica_groups(
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    for bucket in source
        .buckets
        .iter_mut()
        .filter(|bucket| bucket.starts_at >= range_start && bucket.starts_at < range_end)
    {
        if !bucket
            .project_groups
            .iter()
            .any(|group| group.thread_id == thread_id.as_str())
        {
            continue;
        }
        let residual = bucket_project_residual(bucket)
            .expect("replica coverage preflight proved a subtractable project breakdown");
        touched
            .entry((source_index, bucket.starts_at))
            .or_insert(residual);
        bucket
            .project_groups
            .retain(|group| group.thread_id != thread_id.as_str());
        bucket
            .partial_reasons
            .push(DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string());
        if !residual.token_usage.is_zero()
            || residual.estimated_cost_units != 0
            || residual.api_long_context_extra_cost_units != Some(0)
            || residual.call_count != 0
        {
            bucket
                .partial_reasons
                .push(DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string());
        }
        // Keep the in-memory projection coherent for a second logical thread
        // that may share this physical bucket. The final touched pass remains
        // the canonical rebuild, but later conservative preflights must not
        // mistake a scheduled subtraction for an opaque residual.
        rebuild_bucket_from_project_groups(bucket, residual);
    }
}

fn suppress_unbound_replica_range(
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &str,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    suppress_unbound_replica_buckets(
        source_index,
        source,
        thread_id,
        |timestamp| timestamp >= range_start && timestamp < range_end,
        touched,
    );
}

fn suppress_unbound_replica_outside_ranges(
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &str,
    handled_ranges: &[(DateTime<Utc>, DateTime<Utc>)],
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    suppress_unbound_replica_buckets(
        source_index,
        source,
        thread_id,
        |timestamp| !timestamp_in_ranges(timestamp, handled_ranges),
        touched,
    );
}

fn suppress_unbound_replica_buckets(
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &str,
    include_bucket: impl Fn(DateTime<Utc>) -> bool,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    for bucket in source
        .buckets
        .iter_mut()
        .filter(|bucket| include_bucket(bucket.starts_at))
    {
        let has_target_group = bucket
            .project_groups
            .iter()
            .any(|group| group.thread_id == thread_id);
        if has_target_group || !project_groups_match_bucket(bucket) {
            preserve_unrelated_project_groups_in_bucket(source_index, bucket, thread_id, touched);
        }
    }
}

/// Removes only the replica being reconstructed while preserving every
/// explicitly attributed, unrelated project group as a known lower bound.
///
/// A non-subtractable bucket cannot prove how much of its opaque residual came
/// from the target replica, so that residual must be discarded before exact
/// facts are injected. Explicit groups for other threads remain independently
/// attributable and must not be erased with it.
fn preserve_unrelated_project_groups_in_replica_range(
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    suppress_unbound_replica_buckets(
        source_index,
        source,
        thread_id.as_str(),
        |timestamp| timestamp >= range_start && timestamp < range_end,
        touched,
    );
}

fn preserve_unrelated_project_groups_in_bucket(
    source_index: usize,
    bucket: &mut LocalHalfHourBucket,
    thread_id: &str,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    let residual = BucketProjectResidual {
        token_usage: TokenUsage::default(),
        estimated_cost_units: 0,
        api_long_context_extra_cost_units: Some(0),
        call_count: 0,
    };
    touched.insert((source_index, bucket.starts_at), residual);
    bucket.groups.clear();
    bucket.long_context_usage_unknown = false;
    bucket
        .project_groups
        .retain(|group| !group.thread_id.is_empty() && group.thread_id != thread_id);
    bucket
        .partial_reasons
        .push(DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string());
    bucket
        .partial_reasons
        .push(DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string());
    rebuild_bucket_from_project_groups(bucket, residual);
}

fn add_fact_group(
    source_index: usize,
    source: &mut SourceSlice,
    fact: &UsageEventFact,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) -> io::Result<()> {
    let starts_at = quarter_hour_start(fact.occurred_at())?;
    let ends_at = starts_at + Duration::minutes(15);
    let metrics = fact.metrics();
    let bucket_index = source
        .buckets
        .iter()
        .position(|bucket| bucket.starts_at == starts_at)
        .unwrap_or_else(|| {
            source.buckets.push(LocalHalfHourBucket {
                starts_at,
                ends_at,
                sampled_at: ends_at,
                token_usage: TokenUsage::default(),
                estimated_cost_units: 0,
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: metrics.estimator_revision,
                project_breakdown_revision: metrics.project_breakdown_revision,
                api_pricing_catalog_revision: metrics.api_pricing_catalog_revision,
                call_count: 0,
                groups: Vec::new(),
                project_groups: Vec::new(),
                partial_reasons: vec![DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string()],
            });
            source.buckets.len() - 1
        });
    let bucket = &mut source.buckets[bucket_index];
    let residual = bucket_project_residual(bucket).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "replica fact target bucket has a non-subtractable project breakdown",
        )
    })?;
    touched.entry((source_index, starts_at)).or_insert(residual);
    bucket.project_groups.push(LocalProjectUsageGroup {
        thread_id: fact.replica().thread_id().as_str().to_owned(),
        turn_id: fact.emitting_turn_id().map(str::to_owned),
        parent_thread_id: fact
            .parent_thread_id()
            .map(|thread| thread.as_str().to_owned()),
        session_thread_id: Some(fact.root_session_thread_id().as_str().to_owned()),
        session_turn_id: fact.root_session_turn_id().map(str::to_owned),
        project_id: Some(fact.observed_project_key().as_str().to_owned()),
        source: Some(source.metadata.display_label().to_owned()),
        token_usage: metrics.token_usage,
        estimated_cost_units: metrics.estimated_cost_units,
        api_long_context_extra_cost_units: metrics.api_long_context_extra_cost_units,
        api_equivalent_cost: metrics.api_equivalent_cost,
        call_count: metrics.call_count,
        ..LocalProjectUsageGroup::default()
    });
    bucket
        .partial_reasons
        .push(DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string());
    rebuild_bucket_from_project_groups(bucket, residual);
    Ok(())
}

fn quarter_hour_start(timestamp: DateTime<Utc>) -> io::Result<DateTime<Utc>> {
    let seconds = timestamp.timestamp().div_euclid(15 * 60) * 15 * 60;
    Utc.timestamp_opt(seconds, 0).single().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "session fact timestamp cannot be assigned to a history bucket",
        )
    })
}

fn rebuild_bucket_from_project_groups(
    bucket: &mut LocalHalfHourBucket,
    residual: BucketProjectResidual,
) {
    let mut token_usage = residual.token_usage;
    let mut estimated_cost_units = residual.estimated_cost_units;
    let mut long_context = residual.api_long_context_extra_cost_units;
    let mut call_count = residual.call_count;
    for group in &bucket.project_groups {
        token_usage.add_assign(group.token_usage);
        estimated_cost_units = estimated_cost_units.saturating_add(group.estimated_cost_units);
        long_context = add_optional_units(long_context, group.api_long_context_extra_cost_units);
        call_count = call_count.saturating_add(group.call_count);
    }
    bucket.token_usage = token_usage;
    bucket.estimated_cost_units = estimated_cost_units;
    bucket.api_long_context_extra_cost_units = long_context;
    bucket.long_context_usage_unknown |= long_context.is_none();
    bucket.call_count = call_count;
    bucket.groups.clear();
    bucket
        .partial_reasons
        .push(DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string());
    bucket.partial_reasons.sort();
    bucket.partial_reasons.dedup();
}

#[derive(Clone, Copy, Debug, Default)]
struct ProjectGroupTotals {
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    api_equivalent_cost: ApiCostAmount,
    call_count: u64,
}

fn project_group_totals<'a>(
    groups: impl IntoIterator<Item = &'a LocalProjectUsageGroup>,
) -> ProjectGroupTotals {
    let mut totals = ProjectGroupTotals {
        api_long_context_extra_cost_units: Some(0),
        ..ProjectGroupTotals::default()
    };
    for group in groups {
        totals.token_usage.add_assign(group.token_usage);
        totals.estimated_cost_units = totals
            .estimated_cost_units
            .saturating_add(group.estimated_cost_units);
        totals.api_long_context_extra_cost_units = add_optional_units(
            totals.api_long_context_extra_cost_units,
            group.api_long_context_extra_cost_units,
        );
        totals
            .api_equivalent_cost
            .add_assign(group.api_equivalent_cost);
        totals.call_count = totals.call_count.saturating_add(group.call_count);
    }
    totals
}

fn replica_groups_cover_digest(
    source: &SourceSlice,
    thread_id: &ThreadId,
    digest: &SourceSessionDigest,
) -> bool {
    let buckets = source.buckets.iter().filter(|bucket| {
        bucket.starts_at >= digest.range_start() && bucket.starts_at < digest.range_end()
    });
    let mut groups = Vec::new();
    for bucket in buckets {
        let target_groups = bucket
            .project_groups
            .iter()
            .filter(|group| group.thread_id == thread_id.as_str())
            .collect::<Vec<_>>();
        if !target_groups.is_empty() && bucket_project_residual(bucket).is_none() {
            return false;
        }
        groups.extend(target_groups);
    }
    let totals = project_group_totals(groups);
    let expected = digest.metrics();
    totals.token_usage == expected.token_usage
        && totals.estimated_cost_units == expected.estimated_cost_units
        && totals.api_long_context_extra_cost_units == expected.api_long_context_extra_cost_units
        && totals.api_equivalent_cost == expected.api_equivalent_cost
        && totals.call_count == expected.call_count
}

fn bucket_project_residual(bucket: &LocalHalfHourBucket) -> Option<BucketProjectResidual> {
    let totals = project_group_totals(&bucket.project_groups);
    let token_usage = bucket.token_usage.delta_from(totals.token_usage)?;
    let estimated_cost_units = bucket
        .estimated_cost_units
        .checked_sub(totals.estimated_cost_units)?;
    let api_long_context_extra_cost_units = match (
        bucket.api_long_context_extra_cost_units,
        totals.api_long_context_extra_cost_units,
    ) {
        (Some(bucket), Some(groups)) => Some(bucket.checked_sub(groups)?),
        (None, _) => None,
        (Some(_), None) => return None,
    };
    let call_count = bucket.call_count.checked_sub(totals.call_count)?;
    Some(BucketProjectResidual {
        token_usage,
        estimated_cost_units,
        api_long_context_extra_cost_units,
        call_count,
    })
}

fn project_groups_match_bucket(bucket: &LocalHalfHourBucket) -> bool {
    let totals = project_group_totals(&bucket.project_groups);
    totals.token_usage == bucket.token_usage
        && totals.estimated_cost_units == bucket.estimated_cost_units
        && totals.api_long_context_extra_cost_units == bucket.api_long_context_extra_cost_units
        && totals.call_count == bucket.call_count
}

#[cfg(test)]
fn aggregate_source_buckets(
    sources: &[SourceSlice],
    project_mapping: &ProjectMappingProjection,
) -> BucketProjection {
    aggregate_source_buckets_with_logical_threads(
        sources,
        project_mapping,
        &LogicalThreadProjection::default(),
    )
}

fn aggregate_source_buckets_with_logical_threads(
    sources: &[SourceSlice],
    project_mapping: &ProjectMappingProjection,
    logical_threads: &LogicalThreadProjection,
) -> BucketProjection {
    let mut buckets = BTreeMap::<DateTime<Utc>, LocalHalfHourBucket>::new();
    let mut project_observations = false;
    let mut unmapped_projects = false;
    for source in sources {
        for bucket in &source.buckets {
            let mut incoming = bucket.clone();
            let scoped = scope_project_groups(
                &mut incoming.project_groups,
                &source.metadata,
                project_mapping,
                logical_threads,
            );
            project_observations |= scoped.project_observations;
            unmapped_projects |= scoped.unmapped_projects;
            match buckets.entry(incoming.starts_at) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(incoming);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    merge_additive_bucket(entry.get_mut(), incoming);
                }
            }
        }
    }
    BucketProjection {
        buckets: buckets.into_values().collect(),
        project_observations,
        unmapped_projects,
    }
}

fn merge_additive_bucket(target: &mut LocalHalfHourBucket, incoming: LocalHalfHourBucket) {
    target.ends_at = target.ends_at.min(incoming.ends_at);
    // The aggregate is closed only when every contributing source is closed.
    target.sampled_at = target.sampled_at.min(incoming.sampled_at);
    target.token_usage.add_assign(incoming.token_usage);
    target.estimated_cost_units = target
        .estimated_cost_units
        .saturating_add(incoming.estimated_cost_units);
    target.api_long_context_extra_cost_units = add_optional_units(
        target.api_long_context_extra_cost_units,
        incoming.api_long_context_extra_cost_units,
    );
    target.long_context_usage_unknown |= incoming.long_context_usage_unknown;
    if target.estimator_revision != incoming.estimator_revision {
        target.estimator_revision = target.estimator_revision.max(incoming.estimator_revision);
        target
            .partial_reasons
            .push("estimator_revision_changed".to_string());
    }
    target.project_breakdown_revision = target
        .project_breakdown_revision
        .min(incoming.project_breakdown_revision);
    target.api_pricing_catalog_revision = target
        .api_pricing_catalog_revision
        .min(incoming.api_pricing_catalog_revision);
    target.call_count = target.call_count.saturating_add(incoming.call_count);
    merge_usage_groups(&mut target.groups, incoming.groups);
    target.project_groups.extend(incoming.project_groups);
    target.partial_reasons.extend(incoming.partial_reasons);
    target.partial_reasons.sort();
    target.partial_reasons.dedup();
    target.project_groups.sort_by(|left, right| {
        left.project_id
            .cmp(&right.project_id)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
            .then_with(|| left.turn_id.cmp(&right.turn_id))
    });
}

fn merge_usage_groups(target: &mut Vec<LocalUsageGroup>, incoming: Vec<LocalUsageGroup>) {
    let mut groups = BTreeMap::<(Option<String>, Option<String>), LocalUsageGroup>::new();
    for group in target.drain(..).chain(incoming) {
        let key = (group.model.clone(), group.service_tier.clone());
        match groups.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(group);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                existing.token_usage.add_assign(group.token_usage);
                existing.estimated_cost_units = existing
                    .estimated_cost_units
                    .saturating_add(group.estimated_cost_units);
                existing.api_long_context_extra_cost_units = add_optional_units(
                    existing.api_long_context_extra_cost_units,
                    group.api_long_context_extra_cost_units,
                );
                existing.call_count = existing.call_count.saturating_add(group.call_count);
                existing.used_model_fallback |= group.used_model_fallback;
                existing.used_token_breakdown_fallback |= group.used_token_breakdown_fallback;
                existing.used_long_context_pricing |= group.used_long_context_pricing;
                existing.used_long_context_detection_fallback |=
                    group.used_long_context_detection_fallback;
            }
        }
    }
    *target = groups.into_values().collect();
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProjectScopeReport {
    project_observations: bool,
    unmapped_projects: bool,
}

fn scope_project_groups(
    groups: &mut [LocalProjectUsageGroup],
    source: &SourceMetadata,
    project_mapping: &ProjectMappingProjection,
    logical_threads: &LogicalThreadProjection,
) -> ProjectScopeReport {
    let mut report = ProjectScopeReport::default();
    for group in groups {
        let logicalized = logical_identity(&group.thread_id, source.source_id(), logical_threads);
        group.thread_id = logicalized
            .clone()
            .unwrap_or_else(|| scoped_value(&group.thread_id, source.source_id()));
        group.turn_id = group
            .turn_id
            .as_deref()
            .map(|value| scoped_value(value, source.source_id()));
        group.parent_thread_id = group.parent_thread_id.as_deref().map(|value| {
            logical_identity(value, source.source_id(), logical_threads)
                .unwrap_or_else(|| scoped_value(value, source.source_id()))
        });
        group.session_thread_id = group.session_thread_id.as_deref().map(|value| {
            logical_identity(value, source.source_id(), logical_threads)
                .unwrap_or_else(|| scoped_value(value, source.source_id()))
        });
        group.session_turn_id = group
            .session_turn_id
            .as_deref()
            .map(|value| scoped_value(value, source.source_id()));
        if logicalized.is_some() {
            group.source = Some(source.display_label().to_owned());
        }
        let raw_project = group.project_id.clone();
        let raw_label = group.project_label.clone();
        let projection = raw_project
            .as_deref()
            .and_then(|value| value.parse::<ObservedProjectKey>().ok())
            .and_then(|key| project_mapping.resolve(source.source_id(), &key));
        if raw_project.is_some() {
            report.project_observations = true;
        }
        if let Some(projection) = projection {
            group.project_id = Some(projection.aggregate_id().as_str().to_owned());
            group.project_label = projection
                .display_label()
                .map(|label| label.as_str().to_owned())
                .or(raw_label);
        } else {
            report.unmapped_projects |= raw_project.is_some();
            let raw_project = raw_project.as_deref().unwrap_or("unknown");
            group.project_id = Some(scoped_value(raw_project, source.source_id()));
            let raw_label = raw_label.as_deref().unwrap_or("unknown");
            group.project_label = Some(format!("{raw_label} @ {}", source.display_label()));
        }
    }
    report
}

fn logical_identity(
    value: &str,
    source_id: &NodeId,
    logical_threads: &LogicalThreadProjection,
) -> Option<String> {
    logical_threads
        .get(&(source_id.as_str().to_owned(), value.to_owned()))
        .cloned()
}

fn scoped_value(value: &str, source_id: &NodeId) -> String {
    format!("{value}@{}", source_id.as_str())
}

fn add_optional_units(left: Option<u128>, right: Option<u128>) -> Option<u128> {
    Some(left?.saturating_add(right?))
}

fn aggregate_source_weekly_points(
    sources: &[SourceSlice],
    account_quota: &[QuotaPoint],
    since: DateTime<Utc>,
) -> Vec<WeeklyLocalPoint> {
    let resets = canonical_weekly_resets(sources, account_quota);
    let mut points = Vec::new();
    for resets_at in resets {
        let cycle_starts_at = resets_at
            .checked_sub_signed(Duration::minutes(WEEKLY_WINDOW_MINUTES))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let mut timeline = BTreeSet::new();
        for source in sources {
            timeline.extend(
                source
                    .weekly_local_points
                    .iter()
                    .filter(|point| reset_matches(point.resets_at, resets_at))
                    .filter(|point| point.observed_at >= cycle_starts_at)
                    .filter(|point| point.observed_at < resets_at)
                    .map(|point| point.observed_at),
            );
            timeline.extend(
                source
                    .buckets
                    .iter()
                    .filter(|bucket| {
                        bucket.starts_at >= cycle_starts_at && bucket.ends_at <= resets_at
                    })
                    .map(|bucket| bucket.ends_at),
            );
        }

        for observed_at in timeline
            .into_iter()
            .filter(|observed_at| *observed_at >= since && *observed_at < resets_at)
        {
            let mut aggregate = WeeklyAccumulator::default();
            for source in sources {
                if let Some(component) =
                    source_weekly_cumulative_at(source, cycle_starts_at, resets_at, observed_at)
                {
                    aggregate.add_assign(component);
                }
            }
            if !aggregate.present {
                continue;
            }
            if aggregate.estimator_revisions.len() > 1 {
                aggregate
                    .partial_reasons
                    .insert("estimator_revision_changed".to_string());
            }
            points.push(WeeklyLocalPoint {
                observed_at,
                resets_at,
                token_usage: aggregate.token_usage,
                estimated_cost_units: aggregate.estimated_cost_units,
                api_long_context_extra_cost_units: aggregate.api_long_context_extra_cost_units,
                long_context_usage_unknown: aggregate.long_context_usage_unknown,
                estimator_revision: aggregate
                    .estimator_revisions
                    .iter()
                    .next_back()
                    .copied()
                    .unwrap_or_default(),
                call_count: aggregate.call_count,
                partial_reasons: aggregate.partial_reasons.into_iter().collect(),
            });
        }
    }
    points.sort_by_key(|point| (point.observed_at, point.resets_at));
    points
}

fn canonical_weekly_resets(
    sources: &[SourceSlice],
    account_quota: &[QuotaPoint],
) -> Vec<DateTime<Utc>> {
    let mut resets = Vec::new();
    // Account reset timestamps are canonical when available.
    let mut account_resets = account_quota
        .iter()
        .filter(|point| {
            point.duration_mins == WEEKLY_WINDOW_MINUTES
                && point.limit_id.trim().eq_ignore_ascii_case("codex")
        })
        .map(|point| point.resets_at)
        .collect::<Vec<_>>();
    account_resets.sort();
    for candidate in account_resets {
        push_reset_if_distinct(&mut resets, candidate);
    }
    for candidate in sources
        .iter()
        .flat_map(|source| &source.weekly_local_points)
        .map(|point| point.resets_at)
    {
        push_reset_if_distinct(&mut resets, candidate);
    }
    resets.sort();
    resets
}

fn push_reset_if_distinct(resets: &mut Vec<DateTime<Utc>>, candidate: DateTime<Utc>) {
    if !resets
        .iter()
        .any(|existing| reset_matches(*existing, candidate))
    {
        resets.push(candidate);
    }
}

fn reset_matches(left: DateTime<Utc>, right: DateTime<Utc>) -> bool {
    left.signed_duration_since(right)
        .num_seconds()
        .unsigned_abs()
        <= RESET_DRIFT_SECONDS as u64
}

fn source_weekly_cumulative_at(
    source: &SourceSlice,
    cycle_starts_at: DateTime<Utc>,
    resets_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
) -> Option<WeeklyAccumulator> {
    let base = source
        .weekly_local_points
        .iter()
        .filter(|point| reset_matches(point.resets_at, resets_at))
        .filter(|point| point.observed_at <= observed_at)
        .max_by_key(|point| point.observed_at);
    let bucket_cutoff = base.map_or(cycle_starts_at, |point| point.observed_at);
    let mut aggregate = WeeklyAccumulator::default();
    if let Some(point) = base {
        aggregate.add_weekly(point);
        if point.observed_at.timestamp().rem_euclid(15 * 60) != 0 {
            aggregate
                .partial_reasons
                .insert("weekly_source_boundary_excludes_partial_bucket".to_string());
        }
    }
    for bucket in source.buckets.iter().filter(|bucket| {
        bucket.starts_at >= cycle_starts_at
            && bucket.starts_at >= bucket_cutoff
            && bucket.ends_at <= observed_at
            && bucket.ends_at <= resets_at
    }) {
        aggregate.add_bucket(bucket);
    }
    if aggregate.present && base.is_none() {
        if source.metadata.kind() == SourceKind::Ssh {
            aggregate
                .partial_reasons
                .insert("remote_weekly_from_buckets_lower_bound".to_string());
        } else if !source_buckets_cover_cycle(
            &source.buckets,
            cycle_starts_at,
            observed_at.min(resets_at),
        ) {
            aggregate
                .partial_reasons
                .insert("local_weekly_from_buckets_lower_bound".to_string());
        }
    }
    aggregate.present.then_some(aggregate)
}

fn source_buckets_cover_cycle(
    buckets: &[LocalHalfHourBucket],
    cycle_starts_at: DateTime<Utc>,
    observed_through: DateTime<Utc>,
) -> bool {
    let target =
        observed_through - Duration::seconds(observed_through.timestamp().rem_euclid(15 * 60));
    if target <= cycle_starts_at {
        return target == cycle_starts_at;
    }
    let mut covered_through = cycle_starts_at;
    for bucket in buckets
        .iter()
        .filter(|bucket| bucket.ends_at > cycle_starts_at && bucket.starts_at < target)
    {
        if bucket.starts_at > covered_through {
            return false;
        }
        if bucket.ends_at > covered_through {
            covered_through = bucket.ends_at;
        }
        if covered_through >= target {
            return true;
        }
    }
    false
}

#[derive(Default)]
struct WeeklyAccumulator {
    present: bool,
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    long_context_usage_unknown: bool,
    estimator_revisions: BTreeSet<u32>,
    call_count: u64,
    partial_reasons: BTreeSet<String>,
}

impl WeeklyAccumulator {
    fn add_weekly(&mut self, point: &WeeklyLocalPoint) {
        self.add_values(
            point.token_usage,
            point.estimated_cost_units,
            point.api_long_context_extra_cost_units,
            point.long_context_usage_unknown,
            point.estimator_revision,
            point.call_count,
            &point.partial_reasons,
        );
    }

    fn add_bucket(&mut self, bucket: &LocalHalfHourBucket) {
        self.add_values(
            bucket.token_usage,
            bucket.estimated_cost_units,
            bucket.api_long_context_extra_cost_units,
            bucket.long_context_usage_unknown,
            bucket.estimator_revision,
            bucket.call_count,
            &bucket.partial_reasons,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn add_values(
        &mut self,
        token_usage: TokenUsage,
        estimated_cost_units: u128,
        api_long_context_extra_cost_units: Option<u128>,
        long_context_usage_unknown: bool,
        estimator_revision: u32,
        call_count: u64,
        partial_reasons: &[String],
    ) {
        if !self.present {
            self.api_long_context_extra_cost_units = Some(0);
        }
        self.present = true;
        self.token_usage.add_assign(token_usage);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(estimated_cost_units);
        self.api_long_context_extra_cost_units = add_optional_units(
            self.api_long_context_extra_cost_units,
            api_long_context_extra_cost_units,
        );
        self.long_context_usage_unknown |= long_context_usage_unknown;
        self.estimator_revisions.insert(estimator_revision);
        self.call_count = self.call_count.saturating_add(call_count);
        self.partial_reasons.extend(partial_reasons.iter().cloned());
    }

    fn add_assign(&mut self, other: Self) {
        if !other.present {
            return;
        }
        if !self.present {
            self.api_long_context_extra_cost_units = Some(0);
        }
        self.present = true;
        self.token_usage.add_assign(other.token_usage);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(other.estimated_cost_units);
        self.api_long_context_extra_cost_units = add_optional_units(
            self.api_long_context_extra_cost_units,
            other.api_long_context_extra_cost_units,
        );
        self.long_context_usage_unknown |= other.long_context_usage_unknown;
        self.estimator_revisions.extend(other.estimator_revisions);
        self.call_count = self.call_count.saturating_add(other.call_count);
        self.partial_reasons.extend(other.partial_reasons);
    }
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
            aggregate_source_weekly_points(&sources, &[quota(at(1, 0, 0), resets_at)], at(1, 0, 0));
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
        let resets = canonical_weekly_resets(&[source], &[quota(at(1, 0, 0), account_reset)]);
        assert_eq!(resets, vec![account_reset]);
        assert_eq!(account_reset.minute(), 0);
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
