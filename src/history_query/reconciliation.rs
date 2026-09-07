//! Logical-replica reconciliation for source-aware history reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{
    DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING, DUPLICATE_SESSION_FACT_CONFLICT_WARNING,
    DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL, DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND,
    DUPLICATE_SESSION_PROJECT_CONFLICT_WARNING, DUPLICATE_SESSION_WEEKLY_REBUILT_FROM_BUCKETS,
    REMOTE_MODEL_CATALOG_MISMATCH_WARNING, SourceReplicaEvidence, SourceSlice,
    WEEKLY_WINDOW_MINUTES, mask_session_model_catalog_projection,
};
use crate::domain::{ApiCostAmount, TokenUsage};
use crate::history::{LocalHalfHourBucket, LocalProjectUsageGroup, WeeklyLocalPoint};
use crate::logical_replica::{
    ExpectedReplicaFactBinding, ReplicaCandidate, ReplicaCandidateKind, ReplicaDigestObservation,
    active_facts_cover_digest, detect_replica_candidates,
};
use crate::project_mapping::ProjectMappingProjection;
use crate::source_history::{
    ActiveFactSet, SourceHistoryReadBudget, SourceHistoryStore, SourceSessionDigest, UsageEventFact,
};
use crate::source_identity::NodeId;
use crate::source_model::ThreadId;

pub(super) type LogicalThreadProjection = BTreeMap<(String, String), String>;

#[derive(Default)]
pub(super) struct LogicalReplicaReport {
    pub(super) logical_threads: LogicalThreadProjection,
    pub(super) warnings: Vec<String>,
}

#[derive(Clone)]
pub(super) struct ReplicaParticipant {
    pub(super) source_index: usize,
    pub(super) digest: SourceSessionDigest,
    pub(super) exact_fact_coverage: bool,
}

struct ReplicaResolution {
    participants: Vec<ReplicaParticipant>,
    authority_index: usize,
    union_facts: Option<Vec<(usize, UsageEventFact)>>,
    fact_conflict: bool,
    project_conflict: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BucketProjectResidual {
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    call_count: u64,
}

pub(super) type SourceBucketIndex = HashMap<DateTime<Utc>, usize>;

#[derive(Default)]
pub(super) struct ReplicaBucketIndexWork {
    pub(super) indexed_buckets: usize,
    pub(super) fact_lookups: usize,
}

type ReplicaDigestKey = (ThreadId, DateTime<Utc>);
type SourceReplicaDigestIndex = HashMap<ReplicaDigestKey, Vec<usize>>;
type SourceThreadDigestIndex = HashMap<String, Vec<usize>>;

struct ReplicaEvidenceIndex {
    source_positions: HashMap<NodeId, usize>,
    digest_positions: Vec<SourceReplicaDigestIndex>,
    thread_digest_positions: Vec<SourceThreadDigestIndex>,
}

#[derive(Clone, Copy)]
struct IndexedThreadBucket {
    bucket_position: usize,
    active: bool,
    totals: ProjectGroupTotals,
}

struct ReplicaThreadBucketIndex {
    by_source: Vec<HashMap<String, Vec<IndexedThreadBucket>>>,
    project_group_positions: Vec<Vec<HashMap<String, Vec<usize>>>>,
    removed_project_groups: Vec<Vec<Vec<bool>>>,
    active_project_group_totals: Vec<Vec<ProjectGroupTotals>>,
    buckets_needing_compaction: BTreeSet<(usize, usize)>,
    opaque_positions: Vec<Vec<usize>>,
    opaque_active: Vec<Vec<bool>>,
    active_opaque_counts: Vec<usize>,
    authority_marked: Vec<Vec<bool>>,
    bucket_residuals: Vec<Vec<Option<BucketProjectResidual>>>,
}

fn build_replica_evidence_index(
    evidence: &[SourceReplicaEvidence],
) -> io::Result<ReplicaEvidenceIndex> {
    let mut source_positions = HashMap::new();
    source_positions
        .try_reserve(evidence.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica evidence source index: {error}"
            ))
        })?;
    let mut digest_positions = Vec::new();
    digest_positions
        .try_reserve(evidence.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica evidence digest indices: {error}"
            ))
        })?;
    let mut thread_digest_positions = Vec::new();
    thread_digest_positions
        .try_reserve(evidence.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate per-thread digest indices: {error}"
            ))
        })?;
    for (source_index, source) in evidence.iter().enumerate() {
        source_positions
            .entry(source.source_id.clone())
            .or_insert(source_index);
        let mut source_digests = SourceReplicaDigestIndex::new();
        source_digests
            .try_reserve(source.digests.len())
            .map_err(|error| {
                io::Error::other(format!("could not allocate source digest index: {error}"))
            })?;
        let mut source_thread_digests = SourceThreadDigestIndex::new();
        source_thread_digests
            .try_reserve(source.digests.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source thread digest index: {error}"
                ))
            })?;
        for (digest_index, digest) in source.digests.iter().enumerate() {
            let positions = source_digests
                .entry((digest.replica().thread_id().clone(), digest.range_start()))
                .or_default();
            positions.try_reserve(1).map_err(|error| {
                io::Error::other(format!(
                    "could not grow source digest candidate positions: {error}"
                ))
            })?;
            positions.push(digest_index);

            let thread = digest.replica().thread_id().as_str();
            let mut thread_key = String::new();
            thread_key.try_reserve(thread.len()).map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source thread digest key: {error}"
                ))
            })?;
            thread_key.push_str(thread);
            let positions = source_thread_digests.entry(thread_key).or_default();
            positions.try_reserve(1).map_err(|error| {
                io::Error::other(format!(
                    "could not grow source thread digest positions: {error}"
                ))
            })?;
            positions.push(digest_index);
        }
        digest_positions.push(source_digests);
        thread_digest_positions.push(source_thread_digests);
    }
    Ok(ReplicaEvidenceIndex {
        source_positions,
        digest_positions,
        thread_digest_positions,
    })
}

fn build_replica_thread_bucket_index(
    slices: &[SourceSlice],
) -> io::Result<(ReplicaThreadBucketIndex, BTreeMap<String, BTreeSet<usize>>)> {
    let mut by_source = Vec::new();
    by_source.try_reserve(slices.len()).map_err(|error| {
        io::Error::other(format!(
            "could not allocate replica thread bucket indices: {error}"
        ))
    })?;
    let mut opaque_positions = Vec::new();
    opaque_positions
        .try_reserve(slices.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica opaque bucket indices: {error}"
            ))
        })?;
    let mut opaque_active = Vec::new();
    opaque_active.try_reserve(slices.len()).map_err(|error| {
        io::Error::other(format!(
            "could not allocate replica opaque bucket states: {error}"
        ))
    })?;
    let mut active_opaque_counts = Vec::new();
    active_opaque_counts
        .try_reserve(slices.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica opaque bucket counts: {error}"
            ))
        })?;
    let mut authority_marked = Vec::new();
    authority_marked
        .try_reserve(slices.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica authority marker states: {error}"
            ))
        })?;
    let mut bucket_residuals = Vec::new();
    bucket_residuals
        .try_reserve(slices.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica bucket residual indices: {error}"
            ))
        })?;
    let mut project_group_positions = Vec::new();
    project_group_positions
        .try_reserve(slices.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica project-group position indices: {error}"
            ))
        })?;
    let mut removed_project_groups = Vec::new();
    removed_project_groups
        .try_reserve(slices.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica project-group tombstones: {error}"
            ))
        })?;
    let mut active_project_group_totals = Vec::new();
    active_project_group_totals
        .try_reserve(slices.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate active replica project-group totals: {error}"
            ))
        })?;
    let mut observed_sources = BTreeMap::<String, BTreeSet<usize>>::new();
    for (source_index, source) in slices.iter().enumerate() {
        let mut source_threads = HashMap::<String, Vec<IndexedThreadBucket>>::new();
        let mut source_project_group_positions = Vec::new();
        source_project_group_positions
            .try_reserve(source.buckets.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source project-group position indices: {error}"
                ))
            })?;
        let mut source_removed_project_groups = Vec::new();
        source_removed_project_groups
            .try_reserve(source.buckets.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source project-group tombstones: {error}"
                ))
            })?;
        let mut source_active_project_group_totals = Vec::new();
        source_active_project_group_totals
            .try_reserve(source.buckets.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source active project-group totals: {error}"
                ))
            })?;
        let mut source_opaque_positions = Vec::new();
        source_opaque_positions
            .try_reserve(source.buckets.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source opaque bucket positions: {error}"
                ))
            })?;
        let mut source_opaque_active = Vec::new();
        source_opaque_active
            .try_reserve(source.buckets.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source opaque bucket states: {error}"
                ))
            })?;
        source_opaque_active.resize(source.buckets.len(), false);
        let mut source_authority_marked = Vec::new();
        source_authority_marked
            .try_reserve(source.buckets.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source authority marker states: {error}"
                ))
            })?;
        source_authority_marked.resize(source.buckets.len(), false);
        let mut source_bucket_residuals = Vec::new();
        source_bucket_residuals
            .try_reserve(source.buckets.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate source bucket residual index: {error}"
                ))
            })?;
        for (bucket_position, bucket) in source.buckets.iter().enumerate() {
            let residual = bucket_project_residual(bucket);
            if residual.is_none_or(|residual| !bucket_project_residual_is_zero(residual)) {
                source_opaque_positions.push(bucket_position);
                source_opaque_active[bucket_position] = true;
            }
            source_bucket_residuals.push(residual);
            let mut bucket_group_positions = HashMap::<String, Vec<usize>>::new();
            bucket_group_positions
                .try_reserve(bucket.project_groups.len())
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not allocate bucket project-group positions: {error}"
                    ))
                })?;
            let mut bucket_tombstones = Vec::new();
            bucket_tombstones
                .try_reserve(bucket.project_groups.len())
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not allocate bucket project-group tombstones: {error}"
                    ))
                })?;
            bucket_tombstones.resize(bucket.project_groups.len(), false);
            source_active_project_group_totals.push(project_group_totals(&bucket.project_groups));
            for (group_position, group) in bucket.project_groups.iter().enumerate() {
                let group_positions = bucket_group_positions
                    .entry(group.thread_id.clone())
                    .or_default();
                group_positions.try_reserve(1).map_err(|error| {
                    io::Error::other(format!(
                        "could not grow bucket project-group positions: {error}"
                    ))
                })?;
                group_positions.push(group_position);
                if group.thread_id.is_empty() {
                    continue;
                }
                source_threads.try_reserve(1).map_err(|error| {
                    io::Error::other(format!(
                        "could not grow replica thread bucket index: {error}"
                    ))
                })?;
                let mut thread_id = String::new();
                thread_id
                    .try_reserve(group.thread_id.len())
                    .map_err(|error| {
                        io::Error::other(format!(
                            "could not allocate replica thread index key: {error}"
                        ))
                    })?;
                thread_id.push_str(&group.thread_id);
                let positions = source_threads.entry(thread_id.clone()).or_default();
                if positions
                    .last()
                    .is_some_and(|position| position.bucket_position == bucket_position)
                {
                    positions
                        .last_mut()
                        .expect("thread bucket position was just observed")
                        .totals
                        .add_group(group);
                } else {
                    positions.try_reserve(1).map_err(|error| {
                        io::Error::other(format!(
                            "could not grow replica thread bucket positions: {error}"
                        ))
                    })?;
                    let mut totals = ProjectGroupTotals::empty();
                    totals.add_group(group);
                    positions.push(IndexedThreadBucket {
                        bucket_position,
                        active: true,
                        totals,
                    });
                }
                observed_sources
                    .entry(thread_id)
                    .or_default()
                    .insert(source_index);
            }
            source_project_group_positions.push(bucket_group_positions);
            source_removed_project_groups.push(bucket_tombstones);
        }
        for positions in source_threads.values_mut() {
            positions.sort_unstable_by_key(|position| {
                (
                    source.buckets[position.bucket_position].starts_at,
                    position.bucket_position,
                )
            });
        }
        source_opaque_positions
            .sort_unstable_by_key(|position| (source.buckets[*position].starts_at, *position));
        active_opaque_counts.push(source_opaque_positions.len());
        opaque_positions.push(source_opaque_positions);
        opaque_active.push(source_opaque_active);
        authority_marked.push(source_authority_marked);
        bucket_residuals.push(source_bucket_residuals);
        project_group_positions.push(source_project_group_positions);
        removed_project_groups.push(source_removed_project_groups);
        active_project_group_totals.push(source_active_project_group_totals);
        by_source.push(source_threads);
    }
    Ok((
        ReplicaThreadBucketIndex {
            by_source,
            project_group_positions,
            removed_project_groups,
            active_project_group_totals,
            buckets_needing_compaction: BTreeSet::new(),
            opaque_positions,
            opaque_active,
            active_opaque_counts,
            authority_marked,
            bucket_residuals,
        },
        observed_sources,
    ))
}

impl ReplicaThreadBucketIndex {
    fn bucket_is_fully_attributed(&self, source_index: usize, bucket_position: usize) -> bool {
        self.bucket_residuals[source_index][bucket_position]
            .is_some_and(bucket_project_residual_is_zero)
    }

    fn tombstone_project_groups(
        &mut self,
        source_index: usize,
        source: &SourceSlice,
        thread_id: &str,
        bucket_position: usize,
    ) {
        let Self {
            project_group_positions,
            removed_project_groups,
            active_project_group_totals,
            buckets_needing_compaction,
            ..
        } = self;
        let Some(group_positions) =
            project_group_positions[source_index][bucket_position].get(thread_id)
        else {
            return;
        };
        let tombstones = &mut removed_project_groups[source_index][bucket_position];
        let mut removed_totals = ProjectGroupTotals::empty();
        let mut removed_any = false;
        for &group_position in group_positions {
            if tombstones[group_position] {
                continue;
            }
            tombstones[group_position] = true;
            removed_totals
                .add_group(&source.buckets[bucket_position].project_groups[group_position]);
            removed_any = true;
        }
        if !removed_any {
            return;
        }
        active_project_group_totals[source_index][bucket_position] = active_project_group_totals
            [source_index][bucket_position]
            .checked_delta_from(removed_totals)
            .expect("project-group tombstones cannot exceed indexed active totals");
        buckets_needing_compaction.insert((source_index, bucket_position));
    }

    fn active_totals(&self, source_index: usize, bucket_position: usize) -> ProjectGroupTotals {
        self.active_project_group_totals[source_index][bucket_position]
    }

    fn mark_bucket_transparent(&mut self, source_index: usize, bucket_position: usize) {
        if self.opaque_active[source_index][bucket_position] {
            self.opaque_active[source_index][bucket_position] = false;
            self.active_opaque_counts[source_index] =
                self.active_opaque_counts[source_index].saturating_sub(1);
        }
        self.bucket_residuals[source_index][bucket_position] = Some(BucketProjectResidual {
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: Some(0),
            call_count: 0,
        });
    }

    fn deactivate_thread_bucket(
        &mut self,
        source_index: usize,
        source: &SourceSlice,
        thread_id: &str,
        bucket_position: usize,
    ) {
        let target = (source.buckets[bucket_position].starts_at, bucket_position);
        let active = self.by_source[source_index]
            .get(thread_id)
            .and_then(|positions| {
                positions
                    .binary_search_by_key(&target, |position| {
                        (
                            source.buckets[position.bucket_position].starts_at,
                            position.bucket_position,
                        )
                    })
                    .ok()
                    .map(|position| positions[position].active)
            })
            .unwrap_or(false);
        if !active {
            return;
        }
        self.tombstone_project_groups(source_index, source, thread_id, bucket_position);
        let positions = self.by_source[source_index]
            .get_mut(thread_id)
            .expect("active thread bucket must remain indexed");
        let position = positions
            .binary_search_by_key(&target, |position| {
                (
                    source.buckets[position.bucket_position].starts_at,
                    position.bucket_position,
                )
            })
            .expect("active thread bucket must remain indexed");
        positions[position].active = false;
        positions[position].totals = ProjectGroupTotals::empty();
    }

    fn record(
        &mut self,
        source_index: usize,
        source: &SourceSlice,
        thread_id: &str,
        bucket_position: usize,
        totals: ProjectGroupTotals,
    ) -> io::Result<()> {
        let bucket = source.buckets.get(bucket_position).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "replica thread index bucket is out of range",
            )
        })?;
        let required_len = bucket_position.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "replica thread index bucket position overflowed",
            )
        })?;
        let bucket_was_indexed =
            self.active_project_group_totals[source_index].len() >= required_len;
        for states in [
            &mut self.opaque_active[source_index],
            &mut self.authority_marked[source_index],
        ] {
            if states.len() < required_len {
                states
                    .try_reserve(required_len.saturating_sub(states.len()))
                    .map_err(|error| {
                        io::Error::other(format!(
                            "could not grow replica bucket state index: {error}"
                        ))
                    })?;
                states.resize(required_len, false);
            }
        }
        let residuals = &mut self.bucket_residuals[source_index];
        if residuals.len() < required_len {
            residuals
                .try_reserve(required_len.saturating_sub(residuals.len()))
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not grow replica bucket residual index: {error}"
                    ))
                })?;
            residuals.resize(required_len, None);
            residuals[bucket_position] = bucket_project_residual(bucket);
        }
        let position_indices = &mut self.project_group_positions[source_index];
        if position_indices.len() < required_len {
            position_indices
                .try_reserve(required_len.saturating_sub(position_indices.len()))
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not grow replica project-group position index: {error}"
                    ))
                })?;
        }
        let tombstone_indices = &mut self.removed_project_groups[source_index];
        if tombstone_indices.len() < required_len {
            tombstone_indices
                .try_reserve(required_len.saturating_sub(tombstone_indices.len()))
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not grow replica project-group tombstone index: {error}"
                    ))
                })?;
        }
        while self.project_group_positions[source_index].len() < required_len {
            self.project_group_positions[source_index].push(HashMap::new());
        }
        while self.removed_project_groups[source_index].len() < required_len {
            self.removed_project_groups[source_index].push(Vec::new());
        }
        let group_position = bucket.project_groups.len().checked_sub(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "replica fact target bucket has no project group",
            )
        })?;
        let tombstones = &mut self.removed_project_groups[source_index][bucket_position];
        if tombstones.len() < bucket.project_groups.len() {
            tombstones
                .try_reserve(bucket.project_groups.len().saturating_sub(tombstones.len()))
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not grow bucket project-group tombstones: {error}"
                    ))
                })?;
            tombstones.resize(bucket.project_groups.len(), false);
        }
        let group_positions = self.project_group_positions[source_index][bucket_position]
            .entry(thread_id.to_owned())
            .or_default();
        group_positions.try_reserve(1).map_err(|error| {
            io::Error::other(format!(
                "could not grow bucket project-group positions: {error}"
            ))
        })?;
        group_positions.push(group_position);
        let active_totals = &mut self.active_project_group_totals[source_index];
        if !bucket_was_indexed {
            active_totals
                .try_reserve(required_len.saturating_sub(active_totals.len()))
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not grow active project-group totals: {error}"
                    ))
                })?;
            active_totals.resize(required_len, ProjectGroupTotals::empty());
            active_totals[bucket_position] = project_group_totals(&bucket.project_groups);
        } else {
            active_totals[bucket_position].add_totals(totals);
        }

        let source_threads = self.by_source.get_mut(source_index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "replica thread index source is out of range",
            )
        })?;
        source_threads.try_reserve(1).map_err(|error| {
            io::Error::other(format!(
                "could not grow replica thread bucket index: {error}"
            ))
        })?;
        let mut key = String::new();
        key.try_reserve(thread_id.len()).map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica thread index key: {error}"
            ))
        })?;
        key.push_str(thread_id);
        let positions = source_threads.entry(key).or_default();
        let target = (bucket.starts_at, bucket_position);
        match positions.binary_search_by_key(&target, |position| {
            (
                source.buckets[position.bucket_position].starts_at,
                position.bucket_position,
            )
        }) {
            Ok(position) => {
                if !positions[position].active {
                    positions[position].totals = ProjectGroupTotals::empty();
                    positions[position].active = true;
                }
                positions[position].totals.add_totals(totals);
            }
            Err(insert_at) => {
                positions.try_reserve(1).map_err(|error| {
                    io::Error::other(format!(
                        "could not grow replica thread bucket positions: {error}"
                    ))
                })?;
                positions.insert(
                    insert_at,
                    IndexedThreadBucket {
                        bucket_position,
                        active: true,
                        totals,
                    },
                );
            }
        }
        Ok(())
    }
}

fn thread_bucket_positions_in_range<'a>(
    index: &'a ReplicaThreadBucketIndex,
    source_index: usize,
    source: &SourceSlice,
    thread_id: &str,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> &'a [IndexedThreadBucket] {
    let Some(positions) = index.by_source[source_index].get(thread_id) else {
        return &[];
    };
    let start = positions.partition_point(|position| {
        source.buckets[position.bucket_position].starts_at < range_start
    });
    let end = positions
        .partition_point(|position| source.buckets[position.bucket_position].starts_at < range_end);
    &positions[start..end]
}

fn bucket_position_bounds_in_range(
    positions: &[usize],
    source: &SourceSlice,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> (usize, usize) {
    let start =
        positions.partition_point(|position| source.buckets[*position].starts_at < range_start);
    let end = positions.partition_point(|position| source.buckets[*position].starts_at < range_end);
    (start, end)
}

pub(super) fn build_source_bucket_indices(
    slices: &[SourceSlice],
    work: &mut ReplicaBucketIndexWork,
) -> io::Result<Vec<SourceBucketIndex>> {
    let mut source_indices = Vec::new();
    source_indices.try_reserve(slices.len()).map_err(|error| {
        io::Error::other(format!(
            "could not allocate replica source bucket indices: {error}"
        ))
    })?;
    for source in slices {
        let mut index = HashMap::new();
        index.try_reserve(source.buckets.len()).map_err(|error| {
            io::Error::other(format!("could not allocate replica bucket index: {error}"))
        })?;
        for (position, bucket) in source.buckets.iter().enumerate() {
            // Preserve the former `.position` contract when malformed input
            // contains duplicate starts: the first vector entry remains the
            // fact target.
            index.entry(bucket.starts_at).or_insert(position);
            work.indexed_buckets = work.indexed_buckets.saturating_add(1);
        }
        source_indices.push(index);
    }
    Ok(source_indices)
}

pub(super) fn resolve_logical_replicas(
    store: &SourceHistoryStore,
    slices: &mut [SourceSlice],
    evidence: &mut [SourceReplicaEvidence],
    project_mapping: &ProjectMappingProjection,
    weekly_cycle_resets: &[DateTime<Utc>],
    read_budget: &mut SourceHistoryReadBudget,
) -> io::Result<LogicalReplicaReport> {
    // Bucket project groups are an independent replica signal. In particular,
    // a v1 -> v2 migration can persist buckets before its matching digest, so
    // digest-only candidate detection is not sufficient to make an all-source
    // query additive-safe.
    let (mut thread_bucket_index, observed_thread_sources) =
        build_replica_thread_bucket_index(slices)?;
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

    let mut bucket_index_work = ReplicaBucketIndexWork::default();
    let mut source_bucket_indices = build_source_bucket_indices(slices, &mut bucket_index_work)?;
    let evidence_index = build_replica_evidence_index(evidence)?;

    // Facts are a local persistence read. The query path never starts SSH or
    // any other network operation. Only divergent candidates need this read.
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.kind() == ReplicaCandidateKind::NeedsFacts)
    {
        for source_id in candidate.source_ids() {
            let Some(source_index) = evidence_index.source_positions.get(source_id).copied() else {
                continue;
            };
            let source = &mut evidence[source_index];
            if source.active_facts.contains_key(candidate.thread_id()) {
                continue;
            }
            if let Some(active) =
                optional_active_fact_evidence(store.load_active_fact_set_with_budget(
                    source_id,
                    source.redaction_profile,
                    candidate.thread_id(),
                    read_budget,
                ))?
            {
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
        let Some(resolution) =
            plan_replica_resolution(candidate, evidence, &evidence_index, project_mapping)?
        else {
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
                        &thread_bucket_index,
                        *source_index,
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
                        &thread_bucket_index,
                        participant.source_index,
                        &slices[participant.source_index],
                        thread_id,
                        &participant.digest,
                        evidence[participant.source_index].model_catalog_compatible,
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
                            &mut thread_bucket_index,
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
                            &mut thread_bucket_index,
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
                        &mut thread_bucket_index,
                        source_index,
                        &mut slices[source_index],
                        thread_id.as_str(),
                        candidate.range_start(),
                        candidate.range_end(),
                        &mut touched,
                    );
                }
                for (source_index, fact) in facts {
                    let projected_metrics = if evidence[*source_index].model_catalog_compatible {
                        fact.metrics().clone()
                    } else {
                        mask_session_model_catalog_projection(fact.metrics())
                    };
                    let bucket_position = add_fact_group(
                        *source_index,
                        &mut slices[*source_index],
                        &mut source_bucket_indices[*source_index],
                        fact,
                        &projected_metrics,
                        &mut touched,
                        &mut bucket_index_work,
                    )?;
                    if !evidence[*source_index].model_catalog_compatible {
                        push_partial_reason_once(
                            &mut slices[*source_index].buckets[bucket_position],
                            REMOTE_MODEL_CATALOG_MISMATCH_WARNING,
                        );
                    }
                    thread_bucket_index.record(
                        *source_index,
                        &slices[*source_index],
                        fact.replica().thread_id().as_str(),
                        bucket_position,
                        ProjectGroupTotals {
                            token_usage: projected_metrics.token_usage,
                            estimated_cost_units: projected_metrics.estimated_cost_units,
                            api_long_context_extra_cost_units: projected_metrics
                                .api_long_context_extra_cost_units,
                            api_equivalent_cost: projected_metrics.api_equivalent_cost,
                            call_count: projected_metrics.call_count,
                        },
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
                        &mut thread_bucket_index,
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
                            &mut thread_bucket_index,
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
                            &mut thread_bucket_index,
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
                        &mut thread_bucket_index,
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
    for ranges in handled_ranges.values_mut() {
        normalize_time_ranges(ranges);
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
                choose_observed_thread_authority(
                    thread_key,
                    source_indices,
                    evidence,
                    &evidence_index,
                )
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
            if !source_has_unhandled_thread_group(
                &thread_bucket_index,
                source_index,
                &slices[source_index],
                thread_key,
                ranges,
            ) {
                continue;
            }
            suppress_unbound_replica_outside_ranges(
                &mut thread_bucket_index,
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
                &thread_bucket_index,
                authority_index,
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

    compact_tombstoned_project_groups(&mut thread_bucket_index, slices);
    for ((source_index, starts_at), residual) in touched {
        if let Some(position) = source_bucket_indices[source_index].get(&starts_at).copied()
            && let Some(bucket) = slices[source_index].buckets.get_mut(position)
        {
            rebuild_bucket_from_project_groups(bucket, residual);
        }
    }
    if !report.logical_threads.is_empty() {
        // A persisted weekly baseline has no per-thread decomposition. The
        // queried bucket window includes the preceding full cycle, so derive
        // the logical projection from adjusted buckets instead of retaining a
        // physically duplicated baseline.
        replace_weekly_baselines_with_cycle_markers(slices, weekly_cycle_resets);
    }
    report.warnings.sort();
    report.warnings.dedup();
    Ok(report)
}

fn optional_active_fact_evidence(
    result: io::Result<Option<ActiveFactSet>>,
) -> io::Result<Option<ActiveFactSet>> {
    match result {
        Err(error) if SourceHistoryReadBudget::is_exhaustion(&error) => Err(error),
        // Corrupt or unavailable optional evidence retains the existing
        // conservative-authority fallback. Only query budget exhaustion
        // aborts the complete projection.
        Err(_) => Ok(None),
        result => result,
    }
}

fn ensure_authority_project_consistency(
    index: &mut ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    _touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    let (start, end) = bucket_position_bounds_in_range(
        &index.opaque_positions[source_index],
        source,
        range_start,
        range_end,
    );
    let ReplicaThreadBucketIndex {
        opaque_positions,
        opaque_active,
        active_opaque_counts,
        authority_marked,
        bucket_residuals,
        ..
    } = index;
    for position in &opaque_positions[source_index][start..end] {
        if !opaque_active[source_index][*position] {
            continue;
        }
        let bucket = &mut source.buckets[*position];
        if bucket_residuals[source_index][*position].is_some_and(bucket_project_residual_is_zero) {
            opaque_active[source_index][*position] = false;
            active_opaque_counts[source_index] =
                active_opaque_counts[source_index].saturating_sub(1);
            continue;
        }
        if authority_marked[source_index][*position] {
            continue;
        }
        bucket
            .partial_reasons
            .push(DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL.to_string());
        bucket
            .partial_reasons
            .push(DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string());
        authority_marked[source_index][*position] = true;
    }
}

fn source_has_thread_group_in_range(
    index: &ReplicaThreadBucketIndex,
    source_index: usize,
    source: &SourceSlice,
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> bool {
    thread_bucket_positions_in_range(
        index,
        source_index,
        source,
        thread_id.as_str(),
        range_start,
        range_end,
    )
    .iter()
    .any(|position| position.active)
}

fn choose_observed_thread_authority(
    thread_id: &str,
    source_indices: &BTreeSet<usize>,
    evidence: &[SourceReplicaEvidence],
    evidence_index: &ReplicaEvidenceIndex,
) -> usize {
    let mut digest_authority: Option<(usize, &SourceSessionDigest)> = None;
    for source_index in source_indices {
        let Some(digest_positions) =
            evidence_index.thread_digest_positions[*source_index].get(thread_id)
        else {
            continue;
        };
        for digest_position in digest_positions {
            let digest = &evidence[*source_index].digests[*digest_position];
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
    let next = ranges.partition_point(|(range_start, _)| *range_start <= timestamp);
    next > 0 && timestamp < ranges[next - 1].1
}

fn normalize_time_ranges(ranges: &mut Vec<(DateTime<Utc>, DateTime<Utc>)>) {
    ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
    let mut output_len = 0_usize;
    for input in 0..ranges.len() {
        let (start, end) = ranges[input];
        if output_len > 0 && start <= ranges[output_len - 1].1 {
            ranges[output_len - 1].1 = ranges[output_len - 1].1.max(end);
        } else {
            ranges[output_len] = (start, end);
            output_len = output_len.saturating_add(1);
        }
    }
    ranges.truncate(output_len);
}

fn source_has_unhandled_thread_group(
    index: &ReplicaThreadBucketIndex,
    source_index: usize,
    source: &SourceSlice,
    thread_id: &str,
    handled_ranges: &[(DateTime<Utc>, DateTime<Utc>)],
) -> bool {
    index.by_source[source_index]
        .get(thread_id)
        .is_some_and(|positions| {
            positions.iter().any(|position| {
                position.active
                    && !timestamp_in_ranges(
                        source.buckets[position.bucket_position].starts_at,
                        handled_ranges,
                    )
            })
        })
}

fn mark_unhandled_authority_lower_bound(
    index: &ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &str,
    handled_ranges: &[(DateTime<Utc>, DateTime<Utc>)],
) {
    let Some(positions) = index.by_source[source_index].get(thread_id) else {
        return;
    };
    for position in positions {
        if !position.active {
            continue;
        }
        let bucket = &mut source.buckets[position.bucket_position];
        if timestamp_in_ranges(bucket.starts_at, handled_ranges) {
            continue;
        }
        bucket
            .partial_reasons
            .push(DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND.to_string());
    }
}

pub(super) fn replace_weekly_baselines_with_cycle_markers(
    slices: &mut [SourceSlice],
    canonical_resets: &[DateTime<Utc>],
) {
    for source in slices.iter_mut() {
        source.weekly_local_points.clear();
    }
    let Some(marker_source) = slices.first_mut() else {
        return;
    };
    for &resets_at in canonical_resets {
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
            estimator_revision: crate::history::current_history_estimator_revision(),
            call_count: 0,
            partial_reasons: vec![DUPLICATE_SESSION_WEEKLY_REBUILT_FROM_BUCKETS.to_string()],
        });
    }
}

fn plan_replica_resolution(
    candidate: &ReplicaCandidate,
    evidence: &[SourceReplicaEvidence],
    evidence_index: &ReplicaEvidenceIndex,
    project_mapping: &ProjectMappingProjection,
) -> io::Result<Option<ReplicaResolution>> {
    let mut participants = Vec::new();
    participants
        .try_reserve(candidate.source_ids().len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica resolution participants: {error}"
            ))
        })?;
    for source_id in candidate.source_ids() {
        let Some(source_index) = evidence_index.source_positions.get(source_id).copied() else {
            return Ok(None);
        };
        let digest_key = (candidate.thread_id().clone(), candidate.range_start());
        let Some(digest_index) = evidence_index.digest_positions[source_index]
            .get(&digest_key)
            .and_then(|positions| {
                positions.iter().copied().find(|digest_index| {
                    evidence[source_index].digests[*digest_index].range_end()
                        <= candidate.range_end()
                })
            })
        else {
            return Ok(None);
        };
        let digest = &evidence[source_index].digests[digest_index];
        let exact_fact_coverage = active_source_facts_cover_digest(&evidence[source_index], digest);
        participants.push(ReplicaParticipant {
            source_index,
            digest: digest.clone(),
            exact_fact_coverage,
        });
    }
    if participants.len() < 2 {
        return Ok(None);
    }
    let authority_index = participants
        .iter()
        .reduce(|best, candidate_participant| {
            if authority_is_better(
                &evidence[candidate_participant.source_index],
                &candidate_participant.digest,
                candidate_participant.exact_fact_coverage,
                &evidence[best.source_index],
                &best.digest,
                best.exact_fact_coverage,
            ) {
                candidate_participant
            } else {
                best
            }
        })
        .map(|participant| participant.source_index)
        .expect("replica resolution has at least two participants");

    let mut participant_positions = Vec::new();
    participant_positions
        .try_reserve(evidence.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica participant index: {error}"
            ))
        })?;
    participant_positions.resize(evidence.len(), None);
    for (position, participant) in participants.iter().enumerate() {
        participant_positions[participant.source_index] = Some(position);
    }

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
                            let existing_digest = &participants[participant_positions
                                [*existing_source]
                                .expect("fact owner is a participant")];
                            let incoming_digest = &participants[participant_positions
                                [source_index]
                                .expect("fact owner is a participant")];
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

    Ok(Some(ReplicaResolution {
        participants,
        authority_index,
        union_facts,
        fact_conflict,
        project_conflict,
    }))
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

pub(super) fn digest_project_attribution_conflicts(
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
        left_source.model_catalog_compatible,
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
        right_source.model_catalog_compatible,
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
    index: &mut ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    let Some(positions) = index.by_source[source_index].get(thread_id.as_str()) else {
        return;
    };
    let start = positions.partition_point(|position| {
        source.buckets[position.bucket_position].starts_at < range_start
    });
    let end = positions
        .partition_point(|position| source.buckets[position.bucket_position].starts_at < range_end);
    for offset in start..end {
        let indexed = index.by_source[source_index][thread_id.as_str()][offset];
        if !indexed.active {
            continue;
        }
        let position = indexed.bucket_position;
        let residual = index.bucket_residuals[source_index][position]
            .expect("replica coverage preflight proved a subtractable project breakdown");
        let starts_at = source.buckets[position].starts_at;
        touched.entry((source_index, starts_at)).or_insert(residual);
        index.deactivate_thread_bucket(source_index, source, thread_id.as_str(), position);
        let active_totals = index.active_totals(source_index, position);
        let bucket = &mut source.buckets[position];
        push_partial_reason_once(bucket, DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL);
        if !residual.token_usage.is_zero()
            || residual.estimated_cost_units != 0
            || residual.api_long_context_extra_cost_units != Some(0)
            || residual.call_count != 0
        {
            push_partial_reason_once(bucket, DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND);
        }
        // Keep the in-memory projection coherent for a second logical thread
        // that may share this physical bucket. The final touched pass remains
        // the canonical rebuild, but later conservative preflights must not
        // mistake a scheduled subtraction for an opaque residual.
        rebuild_bucket_from_project_group_totals(bucket, residual, active_totals);
    }
}

fn suppress_unbound_replica_range(
    index: &mut ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &str,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    suppress_unbound_replica_buckets(
        index,
        source_index,
        source,
        thread_id,
        ReplicaBucketSelection::Range {
            start: range_start,
            end: range_end,
        },
        touched,
    );
}

fn suppress_unbound_replica_outside_ranges(
    index: &mut ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &str,
    handled_ranges: &[(DateTime<Utc>, DateTime<Utc>)],
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    suppress_unbound_replica_buckets(
        index,
        source_index,
        source,
        thread_id,
        ReplicaBucketSelection::Outside(handled_ranges),
        touched,
    );
}

#[derive(Clone, Copy)]
enum ReplicaBucketSelection<'a> {
    Range {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
    Outside(&'a [(DateTime<Utc>, DateTime<Utc>)]),
}

impl ReplicaBucketSelection<'_> {
    fn contains(self, timestamp: DateTime<Utc>) -> bool {
        match self {
            Self::Range { start, end } => timestamp >= start && timestamp < end,
            Self::Outside(ranges) => !timestamp_in_ranges(timestamp, ranges),
        }
    }

    fn bounds(self, positions: &[usize], source: &SourceSlice) -> (usize, usize) {
        match self {
            Self::Range { start, end } => {
                bucket_position_bounds_in_range(positions, source, start, end)
            }
            Self::Outside(_) => (0, positions.len()),
        }
    }

    fn thread_bounds(
        self,
        positions: &[IndexedThreadBucket],
        source: &SourceSlice,
    ) -> (usize, usize) {
        match self {
            Self::Range { start, end } => {
                let first = positions.partition_point(|position| {
                    source.buckets[position.bucket_position].starts_at < start
                });
                let last = positions.partition_point(|position| {
                    source.buckets[position.bucket_position].starts_at < end
                });
                (first, last)
            }
            Self::Outside(_) => (0, positions.len()),
        }
    }
}

fn suppress_unbound_replica_buckets(
    index: &mut ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &str,
    selection: ReplicaBucketSelection<'_>,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    if let Some(positions) = index.by_source[source_index].get(thread_id) {
        let (start, end) = selection.thread_bounds(positions, source);
        for offset in start..end {
            let indexed = index.by_source[source_index][thread_id][offset];
            if !indexed.active {
                continue;
            }
            let position = indexed.bucket_position;
            if !selection.contains(source.buckets[position].starts_at) {
                continue;
            }
            preserve_unrelated_project_groups_in_bucket(
                index,
                source_index,
                source,
                position,
                thread_id,
                touched,
            );
        }
    }

    if index.active_opaque_counts[source_index] == 0 {
        return;
    }
    let (start, end) = selection.bounds(&index.opaque_positions[source_index], source);
    for offset in start..end {
        let position = index.opaque_positions[source_index][offset];
        if !index.opaque_active[source_index][position]
            || !selection.contains(source.buckets[position].starts_at)
        {
            continue;
        }
        if !index.bucket_is_fully_attributed(source_index, position) {
            preserve_unrelated_project_groups_in_bucket(
                index,
                source_index,
                source,
                position,
                thread_id,
                touched,
            );
        } else {
            index.mark_bucket_transparent(source_index, position);
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
    index: &mut ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    thread_id: &ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    suppress_unbound_replica_buckets(
        index,
        source_index,
        source,
        thread_id.as_str(),
        ReplicaBucketSelection::Range {
            start: range_start,
            end: range_end,
        },
        touched,
    );
}

fn preserve_unrelated_project_groups_in_bucket(
    index: &mut ReplicaThreadBucketIndex,
    source_index: usize,
    source: &mut SourceSlice,
    bucket_position: usize,
    thread_id: &str,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
) {
    let residual = BucketProjectResidual {
        token_usage: TokenUsage::default(),
        estimated_cost_units: 0,
        api_long_context_extra_cost_units: Some(0),
        call_count: 0,
    };
    let starts_at = source.buckets[bucket_position].starts_at;
    touched.insert((source_index, starts_at), residual);
    index.tombstone_project_groups(source_index, source, "", bucket_position);
    index.deactivate_thread_bucket(source_index, source, thread_id, bucket_position);
    index.mark_bucket_transparent(source_index, bucket_position);
    let active_totals = index.active_totals(source_index, bucket_position);
    let bucket = &mut source.buckets[bucket_position];
    bucket.groups.clear();
    bucket.long_context_usage_unknown = false;
    push_partial_reason_once(bucket, DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL);
    push_partial_reason_once(bucket, DUPLICATE_SESSION_PROJECT_BREAKDOWN_LOWER_BOUND);
    rebuild_bucket_from_project_group_totals(bucket, residual, active_totals);
}

pub(super) fn add_fact_group(
    source_index: usize,
    source: &mut SourceSlice,
    bucket_index: &mut SourceBucketIndex,
    fact: &UsageEventFact,
    metrics: &crate::source_history::SessionUsageMetrics,
    touched: &mut BTreeMap<(usize, DateTime<Utc>), BucketProjectResidual>,
    work: &mut ReplicaBucketIndexWork,
) -> io::Result<usize> {
    let starts_at = quarter_hour_start(fact.occurred_at())?;
    let ends_at = starts_at + Duration::minutes(15);
    work.fact_lookups = work.fact_lookups.saturating_add(1);
    let position = if let Some(position) = bucket_index.get(&starts_at).copied() {
        position
    } else {
        source.buckets.try_reserve(1).map_err(|error| {
            io::Error::other(format!(
                "could not allocate replica fact target bucket: {error}"
            ))
        })?;
        bucket_index.try_reserve(1).map_err(|error| {
            io::Error::other(format!("could not grow replica bucket index: {error}"))
        })?;
        let position = source.buckets.len();
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
        bucket_index.insert(starts_at, position);
        position
    };
    let bucket = source.buckets.get_mut(position).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "replica bucket index points outside the source bucket set",
        )
    })?;
    let residual = touched
        .get(&(source_index, starts_at))
        .copied()
        .or_else(|| bucket_project_residual(bucket))
        .ok_or_else(|| {
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
    add_project_group_totals_to_bucket(
        bucket,
        ProjectGroupTotals {
            token_usage: metrics.token_usage,
            estimated_cost_units: metrics.estimated_cost_units,
            api_long_context_extra_cost_units: metrics.api_long_context_extra_cost_units,
            api_equivalent_cost: metrics.api_equivalent_cost,
            call_count: metrics.call_count,
        },
    );
    Ok(position)
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
    let totals = project_group_totals(&bucket.project_groups);
    rebuild_bucket_from_project_group_totals(bucket, residual, totals);
}

fn rebuild_bucket_from_project_group_totals(
    bucket: &mut LocalHalfHourBucket,
    residual: BucketProjectResidual,
    project_totals: ProjectGroupTotals,
) {
    bucket.token_usage = residual.token_usage;
    bucket.token_usage.add_assign(project_totals.token_usage);
    bucket.estimated_cost_units = residual
        .estimated_cost_units
        .saturating_add(project_totals.estimated_cost_units);
    bucket.api_long_context_extra_cost_units = add_optional_units(
        residual.api_long_context_extra_cost_units,
        project_totals.api_long_context_extra_cost_units,
    );
    bucket.long_context_usage_unknown |= bucket.api_long_context_extra_cost_units.is_none();
    bucket.call_count = residual
        .call_count
        .saturating_add(project_totals.call_count);
    bucket.groups.clear();
    push_partial_reason_once(bucket, DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL);
    bucket.partial_reasons.sort();
    bucket.partial_reasons.dedup();
}

fn add_project_group_totals_to_bucket(
    bucket: &mut LocalHalfHourBucket,
    project_totals: ProjectGroupTotals,
) {
    bucket.token_usage.add_assign(project_totals.token_usage);
    bucket.estimated_cost_units = bucket
        .estimated_cost_units
        .saturating_add(project_totals.estimated_cost_units);
    bucket.api_long_context_extra_cost_units = add_optional_units(
        bucket.api_long_context_extra_cost_units,
        project_totals.api_long_context_extra_cost_units,
    );
    bucket.long_context_usage_unknown |= bucket.api_long_context_extra_cost_units.is_none();
    bucket.call_count = bucket.call_count.saturating_add(project_totals.call_count);
    bucket.groups.clear();
    push_partial_reason_once(bucket, DUPLICATE_SESSION_MODEL_BREAKDOWN_PARTIAL);
}

fn push_partial_reason_once(bucket: &mut LocalHalfHourBucket, reason: &str) {
    if !bucket
        .partial_reasons
        .iter()
        .any(|existing| existing == reason)
    {
        bucket.partial_reasons.push(reason.to_owned());
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProjectGroupCompactionWork {
    buckets_compacted: usize,
    groups_examined: usize,
}

fn compact_tombstoned_project_groups(
    index: &mut ReplicaThreadBucketIndex,
    slices: &mut [SourceSlice],
) -> ProjectGroupCompactionWork {
    let pending = std::mem::take(&mut index.buckets_needing_compaction);
    let mut work = ProjectGroupCompactionWork::default();
    for (source_index, bucket_position) in pending {
        let Some(bucket) = slices
            .get_mut(source_index)
            .and_then(|source| source.buckets.get_mut(bucket_position))
        else {
            continue;
        };
        let tombstones = &index.removed_project_groups[source_index][bucket_position];
        debug_assert_eq!(tombstones.len(), bucket.project_groups.len());
        let mut group_position = 0_usize;
        bucket.project_groups.retain(|_| {
            let keep = !tombstones[group_position];
            group_position = group_position.saturating_add(1);
            keep
        });
        work.buckets_compacted = work.buckets_compacted.saturating_add(1);
        work.groups_examined = work.groups_examined.saturating_add(group_position);
    }
    work
}

#[derive(Clone, Copy, Debug, Default)]
struct ProjectGroupTotals {
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    api_equivalent_cost: ApiCostAmount,
    call_count: u64,
}

impl ProjectGroupTotals {
    fn empty() -> Self {
        Self {
            api_long_context_extra_cost_units: Some(0),
            ..Self::default()
        }
    }

    fn add_group(&mut self, group: &LocalProjectUsageGroup) {
        self.token_usage.add_assign(group.token_usage);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(group.estimated_cost_units);
        self.api_long_context_extra_cost_units = add_optional_units(
            self.api_long_context_extra_cost_units,
            group.api_long_context_extra_cost_units,
        );
        self.api_equivalent_cost
            .add_assign(group.api_equivalent_cost);
        self.call_count = self.call_count.saturating_add(group.call_count);
    }

    fn add_totals(&mut self, other: Self) {
        self.token_usage.add_assign(other.token_usage);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(other.estimated_cost_units);
        self.api_long_context_extra_cost_units = add_optional_units(
            self.api_long_context_extra_cost_units,
            other.api_long_context_extra_cost_units,
        );
        self.api_equivalent_cost
            .add_assign(other.api_equivalent_cost);
        self.call_count = self.call_count.saturating_add(other.call_count);
    }

    fn checked_delta_from(self, removed: Self) -> Option<Self> {
        Some(Self {
            token_usage: self.token_usage.delta_from(removed.token_usage)?,
            estimated_cost_units: self
                .estimated_cost_units
                .checked_sub(removed.estimated_cost_units)?,
            api_long_context_extra_cost_units: match (
                self.api_long_context_extra_cost_units,
                removed.api_long_context_extra_cost_units,
            ) {
                (Some(total), Some(removed)) => Some(total.checked_sub(removed)?),
                (None, _) => None,
                (Some(_), None) => return None,
            },
            api_equivalent_cost: self
                .api_equivalent_cost
                .checked_delta_from(removed.api_equivalent_cost)?,
            call_count: self.call_count.checked_sub(removed.call_count)?,
        })
    }
}

fn project_group_totals<'a>(
    groups: impl IntoIterator<Item = &'a LocalProjectUsageGroup>,
) -> ProjectGroupTotals {
    let mut totals = ProjectGroupTotals::empty();
    for group in groups {
        totals.add_group(group);
    }
    totals
}

fn bucket_project_residual_is_zero(residual: BucketProjectResidual) -> bool {
    residual.token_usage.is_zero()
        && residual.estimated_cost_units == 0
        && residual.api_long_context_extra_cost_units == Some(0)
        && residual.call_count == 0
}

fn replica_groups_cover_digest(
    index: &ReplicaThreadBucketIndex,
    source_index: usize,
    source: &SourceSlice,
    thread_id: &ThreadId,
    digest: &SourceSessionDigest,
    model_catalog_compatible: bool,
) -> bool {
    let mut totals = ProjectGroupTotals::empty();
    for indexed in thread_bucket_positions_in_range(
        index,
        source_index,
        source,
        thread_id.as_str(),
        digest.range_start(),
        digest.range_end(),
    ) {
        if !indexed.active {
            continue;
        }
        totals.add_totals(indexed.totals);
        if index.bucket_residuals[source_index][indexed.bucket_position].is_none() {
            return false;
        }
    }
    let expected = digest.metrics();
    totals.token_usage == expected.token_usage
        && totals.call_count == expected.call_count
        && (!model_catalog_compatible
            || (totals.estimated_cost_units == expected.estimated_cost_units
                && totals.api_long_context_extra_cost_units
                    == expected.api_long_context_extra_cost_units
                && totals.api_equivalent_cost == expected.api_equivalent_cost))
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

pub(super) fn add_optional_units(left: Option<u128>, right: Option<u128>) -> Option<u128> {
    Some(left?.saturating_add(right?))
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::source_history::SourceMetadata;

    fn indexed_bucket(
        starts_at: DateTime<Utc>,
        thread_id: &str,
        opaque: bool,
    ) -> LocalHalfHourBucket {
        let group_usage = TokenUsage {
            input_tokens: 1,
            total_tokens: 1,
            ..TokenUsage::default()
        };
        let bucket_usage = TokenUsage {
            input_tokens: u64::from(opaque) + 1,
            total_tokens: u64::from(opaque) + 1,
            ..TokenUsage::default()
        };
        LocalHalfHourBucket {
            starts_at,
            ends_at: starts_at + Duration::minutes(15),
            sampled_at: starts_at + Duration::minutes(15),
            token_usage: bucket_usage,
            estimated_cost_units: u128::from(opaque) + 1,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
            call_count: 1,
            groups: Vec::new(),
            project_groups: vec![LocalProjectUsageGroup {
                thread_id: thread_id.to_owned(),
                token_usage: group_usage,
                estimated_cost_units: 1,
                api_long_context_extra_cost_units: Some(0),
                call_count: 1,
                ..LocalProjectUsageGroup::default()
            }],
            partial_reasons: Vec::new(),
        }
    }

    #[test]
    fn ordinary_optional_fact_error_keeps_conservative_fallback() {
        let result = optional_active_fact_evidence(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "optional fact shard is corrupt",
        )))
        .unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn query_budget_exhaustion_is_not_downgraded_to_missing_facts() {
        let mut budget = SourceHistoryReadBudget::with_limits(1, 1, 0);
        let budget_error = budget.charge_source().unwrap_err();
        assert!(SourceHistoryReadBudget::is_exhaustion(&budget_error));

        let propagated = optional_active_fact_evidence(Err(budget_error)).unwrap_err();
        assert!(SourceHistoryReadBudget::is_exhaustion(&propagated));
    }

    #[test]
    fn conservative_range_uses_only_target_and_opaque_bucket_indices() {
        let base = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).single().unwrap();
        let target_position = 5_000_usize;
        let mut buckets = Vec::new();
        buckets.try_reserve(10_000).unwrap();
        for position in 0..10_000_usize {
            let starts_at =
                base + Duration::minutes(i64::try_from(position.saturating_mul(15)).unwrap());
            buckets.push(indexed_bucket(
                starts_at,
                if position == target_position {
                    "target-thread"
                } else {
                    ""
                },
                position == target_position,
            ));
        }
        let mut source = SourceSlice {
            metadata: SourceMetadata::new(
                "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
                crate::source_history::SourceKind::Local,
                "local",
            )
            .unwrap(),
            buckets,
            weekly_local_points: Vec::new(),
        };
        let (mut index, observed) =
            build_replica_thread_bucket_index(std::slice::from_ref(&source)).unwrap();
        assert_eq!(
            index.by_source[0]["target-thread"]
                .iter()
                .map(|position| position.bucket_position)
                .collect::<Vec<_>>(),
            [target_position]
        );
        assert_eq!(index.opaque_positions[0], [target_position]);
        assert_eq!(index.active_opaque_counts[0], 1);
        assert_eq!(observed["target-thread"], BTreeSet::from([0]));

        let starts_at = source.buckets[target_position].starts_at;
        let mut touched = BTreeMap::new();
        suppress_unbound_replica_range(
            &mut index,
            0,
            &mut source,
            "target-thread",
            starts_at,
            starts_at + Duration::minutes(15),
            &mut touched,
        );
        assert_eq!(touched.len(), 1);
        assert_eq!(source.buckets[target_position].project_groups.len(), 1);
        assert!(index.removed_project_groups[0][target_position][0]);
        assert_eq!(index.active_opaque_counts[0], 0);

        let reasons = source.buckets[target_position].partial_reasons.clone();
        suppress_unbound_replica_range(
            &mut index,
            0,
            &mut source,
            "target-thread",
            starts_at,
            starts_at + Duration::minutes(15),
            &mut touched,
        );
        assert_eq!(touched.len(), 1);
        assert_eq!(source.buckets[target_position].partial_reasons, reasons);

        let work = compact_tombstoned_project_groups(&mut index, std::slice::from_mut(&mut source));
        assert_eq!(
            work,
            ProjectGroupCompactionWork {
                buckets_compacted: 1,
                groups_examined: 1,
            }
        );
        assert!(source.buckets[target_position].project_groups.is_empty());
    }

    #[test]
    fn many_candidate_threads_in_one_bucket_compact_groups_once() {
        const GROUP_COUNT: usize = 4_096;

        let starts_at = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).single().unwrap();
        let mut bucket = indexed_bucket(starts_at, "thread-0", false);
        bucket.project_groups.clear();
        bucket.project_groups.try_reserve(GROUP_COUNT).unwrap();
        for position in 0..GROUP_COUNT {
            bucket.project_groups.push(LocalProjectUsageGroup {
                thread_id: format!("thread-{position}"),
                token_usage: TokenUsage {
                    input_tokens: 1,
                    total_tokens: 1,
                    ..TokenUsage::default()
                },
                estimated_cost_units: 1,
                api_long_context_extra_cost_units: Some(0),
                call_count: 1,
                ..LocalProjectUsageGroup::default()
            });
        }
        bucket.token_usage = TokenUsage {
            input_tokens: GROUP_COUNT as u64,
            total_tokens: GROUP_COUNT as u64,
            ..TokenUsage::default()
        };
        bucket.estimated_cost_units = GROUP_COUNT as u128;
        bucket.call_count = GROUP_COUNT as u64;
        let mut source = SourceSlice {
            metadata: SourceMetadata::new(
                "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
                crate::source_history::SourceKind::Local,
                "local",
            )
            .unwrap(),
            buckets: vec![bucket],
            weekly_local_points: Vec::new(),
        };
        let (mut index, _) =
            build_replica_thread_bucket_index(std::slice::from_ref(&source)).unwrap();
        let mut touched = BTreeMap::new();

        for position in 0..GROUP_COUNT {
            suppress_unbound_replica_range(
                &mut index,
                0,
                &mut source,
                &format!("thread-{position}"),
                starts_at,
                starts_at + Duration::minutes(15),
                &mut touched,
            );
        }

        // Mutations update indexed totals and record tombstones in constant
        // time per candidate. The physical vector is traversed only by this
        // one final compaction, independent of the candidate count.
        assert_eq!(source.buckets[0].project_groups.len(), GROUP_COUNT);
        assert_eq!(source.buckets[0].token_usage, TokenUsage::default());
        let work = compact_tombstoned_project_groups(&mut index, std::slice::from_mut(&mut source));
        assert_eq!(work.buckets_compacted, 1);
        assert_eq!(work.groups_examined, GROUP_COUNT);
        assert!(source.buckets[0].project_groups.is_empty());
    }

    #[test]
    fn handled_ranges_are_merged_for_binary_membership() {
        let base = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).single().unwrap();
        let mut ranges = vec![
            (base + Duration::hours(2), base + Duration::hours(3)),
            (base, base + Duration::hours(2)),
            (base + Duration::hours(1), base + Duration::hours(4)),
            (base + Duration::hours(5), base + Duration::hours(6)),
        ];
        normalize_time_ranges(&mut ranges);

        assert_eq!(
            ranges,
            vec![
                (base, base + Duration::hours(4)),
                (base + Duration::hours(5), base + Duration::hours(6)),
            ]
        );
        assert!(timestamp_in_ranges(base + Duration::hours(3), &ranges));
        assert!(!timestamp_in_ranges(base + Duration::hours(4), &ranges));
        assert!(timestamp_in_ranges(base + Duration::hours(5), &ranges));
    }
}
