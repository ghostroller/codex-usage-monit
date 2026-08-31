use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::*;
use crate::history::{HistoryObservation, redacted_history_observation};
use crate::source_identity::SourceIdentity;

const STATE_FILE: &str = "local-observation-state.json";
const STATE_LOCK: &str = "local-observation.lock";
const MARKER_FILE: &str = "summary-backfill-attempt.json";
const MARKER_LOCK: &str = "summary-backfill-attempt.lock";
const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalObservationMode {
    Incremental,
    Reconcile {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalObservationWriteReport {
    pub revision: u64,
    pub account: SourceHistoryWriteReport,
    pub buckets: SourceHistoryWriteReport,
    pub weekly: SourceHistoryWriteReport,
    pub session_digests: SourceHistoryWriteReport,
    pub bucket_tombstones: usize,
    pub weekly_tombstones: usize,
    pub session_digest_tombstones: usize,
    pub garbage_collection: LocalObservationGarbageCollectionReport,
}

/// One revision-consistent read of every local observation family used by a
/// history query. The local writer publishes buckets, weekly points, and
/// session digests under one stable source-level state lock; readers must hold
/// the shared side of that same lock so they cannot splice two observation
/// revisions together, including while the first profile namespace is being
/// created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalObservationSnapshot {
    pub source: SourceMetadata,
    pub redaction_profile: RedactionProfile,
    pub buckets: Vec<LocalHalfHourBucket>,
    pub weekly_local_points: Vec<WeeklyLocalPoint>,
    pub session_digest_records: Vec<SourceSessionDigestRecord>,
}

/// Bounded, content-free outcome of the retention pass associated with a
/// successful local observation write.
///
/// `attempted == false` means the persistent schedule was not due (or the
/// in-process fast gate skipped checking it). A failed pass carries one
/// bounded warning while leaving the successful observation report intact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalObservationGarbageCollectionReport {
    pub attempted: bool,
    pub duration_us: u64,
    pub shards_pruned: usize,
    pub pruning_deferred: bool,
    pub trusted_at: Option<DateTime<Utc>>,
    pub warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2SummaryBackfillAttempt {
    pub completed_at: DateTime<Utc>,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalRevisionState {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    source_generation: u64,
    redaction_profile: RedactionProfile,
    last_reserved_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackfillMarker {
    format_version: u32,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    ownership_epoch: u64,
    completed_at: DateTime<Utc>,
    complete: bool,
}

impl SourceHistoryWriter<'_, '_, '_> {
    pub fn record_local_observation(
        &self,
        identity: &SourceIdentity,
        display_label: &str,
        redaction_profile: RedactionProfile,
        observation: &HistoryObservation,
        mode: LocalObservationMode,
    ) -> io::Result<LocalObservationWriteReport> {
        self.record_local_observation_with_session_digests(
            identity,
            display_label,
            redaction_profile,
            observation,
            mode,
            &[],
            false,
        )
    }

    /// Persists one aggregate observation and its already-materialized local
    /// session digests under the same ownership fence and revision. The digest
    /// sidecar contains no rollout content or paths.
    #[allow(clippy::too_many_arguments)]
    pub fn record_local_observation_with_session_digests(
        &self,
        identity: &SourceIdentity,
        display_label: &str,
        redaction_profile: RedactionProfile,
        observation: &HistoryObservation,
        mode: LocalObservationMode,
        session_digests: &[SourceSessionDigest],
        session_digest_scan_complete: bool,
    ) -> io::Result<LocalObservationWriteReport> {
        self.validate_redaction(redaction_profile)?;
        validate_reconcile_window(mode)?;
        // Redaction belongs at the lowest local-observation write boundary so
        // direct runtime callers cannot bypass the staging store's sanitizer.
        let redacted = (redaction_profile == RedactionProfile::Redacted)
            .then(|| redacted_history_observation(observation));
        let observation = redacted.as_ref().unwrap_or(observation);
        let identity = identity.clone();
        let display_label = display_label.to_owned();
        self.fenced(|store| {
            // The lock lives in the stable source directory, which is
            // created before source metadata is published. Taking it first
            // ensures that once a first-observation descriptor becomes
            // visible, every reader must wait for all of that observation's
            // families to be published. Keeping it outside the profile
            // namespace also prevents a reader from skipping the lock while
            // the first profile directory is being created.
            let lock_directory = store.source_directory(identity.node_id());
            store.prepare_private_directory(&lock_directory)?;
            let lock = open_lock_file(&lock_directory, STATE_LOCK)?;
            lock_exclusive(&lock, &lock_directory, STATE_LOCK)?;
            prepare_local_metadata(store, &identity, &display_label, redaction_profile)?;
            let state_directory =
                local_state_directory(store, identity.node_id(), redaction_profile);
            store.prepare_private_directory(&state_directory)?;
            cleanup_state_temps(store, &state_directory, STATE_FILE)?;

            // Reserve before touching any shard. A crash after this atomic publish
            // intentionally leaves a gap; the number is never issued again.
            let revision = reserve_revision(store, &state_directory, &identity, redaction_profile)?;
            let (bucket_records, weekly_records, bucket_tombstones, weekly_tombstones) =
                build_records(
                    store,
                    &identity,
                    redaction_profile,
                    observation,
                    mode,
                    revision,
                )?;
            let (session_digest_records, session_digest_tombstones) = build_session_digest_records(
                store,
                &identity,
                redaction_profile,
                observation,
                mode,
                revision,
                session_digests,
                session_digest_scan_complete,
            )?;
            let account = store.record_account_points_unfenced(&observation.quota_points)?;
            let buckets = store.record_source_bucket_changes_unfenced(
                identity.node_id(),
                redaction_profile,
                &bucket_records,
            )?;
            let weekly = store.record_source_weekly_changes_unfenced(
                identity.node_id(),
                redaction_profile,
                &weekly_records,
            )?;
            let session_digests = store.record_source_session_digest_changes_unfenced(
                identity.node_id(),
                redaction_profile,
                &session_digest_records,
            )?;
            // Keep an existing source's selected profile visible until every
            // target-namespace write succeeds and the ownership fence is
            // freshly validated. A failed target write therefore cannot hide
            // the last known-good profile or expose a partial new one.
            self.validate()?;
            publish_local_metadata(store, &identity, &display_label, redaction_profile)?;
            Ok(LocalObservationWriteReport {
                revision,
                account,
                buckets,
                weekly,
                session_digests,
                bucket_tombstones,
                weekly_tombstones,
                session_digest_tombstones,
                garbage_collection: LocalObservationGarbageCollectionReport::default(),
            })
        })
    }

    /// Durably reserves every local-observation revision through `floor`.
    ///
    /// Migration uses this before activation so the first live observation
    /// must be strictly newer than every imported bucket and weekly record.
    /// The state is monotonic and atomically published; a crash may leave a
    /// harmless gap but can never make a revision reusable.
    pub fn ensure_local_observation_revision_floor(
        &self,
        identity: &SourceIdentity,
        redaction_profile: RedactionProfile,
        floor: u64,
    ) -> io::Result<u64> {
        self.validate_redaction(redaction_profile)?;
        if floor == 0 {
            return Err(invalid_data(
                "local observation revision floor must be greater than zero",
            ));
        }
        let identity = identity.clone();
        self.fenced(|store| {
            let state_directory =
                local_state_directory(store, identity.node_id(), redaction_profile);
            let lock_directory = store.source_directory(identity.node_id());
            store.prepare_private_directory(&lock_directory)?;
            let lock = open_lock_file(&lock_directory, STATE_LOCK)?;
            lock_exclusive(&lock, &lock_directory, STATE_LOCK)?;
            store.prepare_private_directory(&state_directory)?;
            cleanup_state_temps(store, &state_directory, STATE_FILE)?;
            raise_revision_floor(store, &state_directory, &identity, redaction_profile, floor)
        })
    }

    pub fn mark_v2_summary_backfill_attempt(
        &self,
        completed_at: DateTime<Utc>,
        complete: bool,
    ) -> io::Result<V2SummaryBackfillAttempt> {
        self.fenced(|store| {
            let directory = store
                .profile_directory()
                .join(self.redaction_profile().directory_name());
            store.prepare_private_directory(&directory)?;
            let lock = open_lock_file(&directory, MARKER_LOCK)?;
            lock_exclusive(&lock, &directory, MARKER_LOCK)?;
            cleanup_state_temps(store, &directory, MARKER_FILE)?;
            let path = directory.join(MARKER_FILE);
            let epoch = self.authority.expected_manifest().epoch();
            let current = read_optional_json_file::<BackfillMarker>(&path, MAX_STATE_BYTES)?;
            if let Some(marker) = current.as_ref() {
                validate_marker(store, self.redaction_profile(), epoch, marker)?;
            }
            let requested = BackfillMarker {
                format_version: STATE_VERSION,
                profile_id: store.profile_id().clone(),
                redaction_profile: self.redaction_profile(),
                ownership_epoch: epoch,
                completed_at,
                complete,
            };
            // Completion is terminal for this exact authority epoch. Otherwise
            // time is monotonic, and complete wins ties.
            let marker = match current {
                Some(current)
                    if current.complete
                        || (current.completed_at, current.complete)
                            >= (requested.completed_at, requested.complete) =>
                {
                    current
                }
                _ => requested,
            };
            write_private_atomically(&path, &encode_pretty_bounded(&marker, MAX_STATE_BYTES)?)?;
            Ok(V2SummaryBackfillAttempt {
                completed_at: marker.completed_at,
                complete: marker.complete,
            })
        })
    }

    pub fn load_v2_summary_backfill_attempt(&self) -> io::Result<Option<V2SummaryBackfillAttempt>> {
        self.fenced(|store| {
            store.load_v2_summary_backfill_attempt(
                self.redaction_profile(),
                self.authority.expected_manifest().epoch(),
            )
        })
    }
}

impl SourceHistoryStore {
    /// Loads all local source families under the shared local-observation
    /// state lock. `include_session_digests=false` avoids opening digest
    /// shards when replica detection is disabled while preserving the same
    /// bucket/weekly consistency boundary.
    pub fn load_local_observation_snapshot_since(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        include_session_digests: bool,
    ) -> io::Result<LocalObservationSnapshot> {
        let lock_directory = self.source_directory(source_id);
        self.validate_private_path(&lock_directory)?;
        let state_lock = open_lock_file(&lock_directory, STATE_LOCK)?;
        lock_shared(&state_lock, &lock_directory, STATE_LOCK)?;

        let snapshot = self.with_source_metadata_shared(source_id, |source| {
            if source.kind() != SourceKind::Local {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local observation snapshot requires a local source",
                ));
            }

            let bucket_records = self.load_source_bucket_records_from_directory(
                source_id,
                redaction_profile,
                since,
                &self.source_buckets_directory(source_id, redaction_profile),
            )?;
            let weekly_records =
                self.load_source_weekly_records_since(source_id, redaction_profile, since)?;
            let session_digest_records = if include_session_digests {
                self.load_source_session_digest_records_from_directory(
                    source_id,
                    redaction_profile,
                    since,
                    &self.source_digests_directory(source_id, redaction_profile),
                )?
            } else {
                Vec::new()
            };

            let mut buckets = bucket_records
                .into_iter()
                .filter_map(|record| match record.change {
                    SourceBucketChange::Upsert(bucket) => Some(*bucket),
                    SourceBucketChange::Tombstone => None,
                })
                .collect::<Vec<_>>();
            let mut weekly_local_points = weekly_records
                .into_iter()
                .filter_map(|record| match record.change {
                    SourceWeeklyChange::Upsert(point) => Some(*point),
                    SourceWeeklyChange::Tombstone => None,
                })
                .collect::<Vec<_>>();
            buckets.sort_by_key(|bucket| bucket.starts_at);
            weekly_local_points.sort_by_key(|point| (point.observed_at, point.resets_at));

            Ok(LocalObservationSnapshot {
                source: source.clone(),
                redaction_profile,
                buckets,
                weekly_local_points,
                session_digest_records,
            })
        });
        drop(state_lock);
        snapshot
    }

    /// Loads the v2 Summary backfill marker without acquiring write
    /// authority. The caller supplies the ownership epoch selected before the
    /// query and must revalidate that manifest after the complete history
    /// read; the marker payload itself is strictly bound to that epoch.
    pub fn load_v2_summary_backfill_attempt(
        &self,
        redaction_profile: RedactionProfile,
        ownership_epoch: u64,
    ) -> io::Result<Option<V2SummaryBackfillAttempt>> {
        if ownership_epoch <= 1 {
            return Err(invalid_data(
                "v2 summary backfill marker requires a cutover ownership epoch",
            ));
        }
        let directory = self
            .profile_directory()
            .join(redaction_profile.directory_name());
        if !self.private_directory_exists(&directory)? {
            return Ok(None);
        }
        self.validate_private_path(&directory)?;
        let lock = open_lock_file(&directory, MARKER_LOCK)?;
        lock_shared(&lock, &directory, MARKER_LOCK)?;
        let Some(marker) = read_optional_json_file::<BackfillMarker>(
            &directory.join(MARKER_FILE),
            MAX_STATE_BYTES,
        )?
        else {
            return Ok(None);
        };
        validate_marker(self, redaction_profile, ownership_epoch, &marker)?;
        Ok(Some(V2SummaryBackfillAttempt {
            completed_at: marker.completed_at,
            complete: marker.complete,
        }))
    }
}

fn validate_reconcile_window(mode: LocalObservationMode) -> io::Result<()> {
    if let LocalObservationMode::Reconcile { from, to } = mode
        && from >= to
    {
        return Err(invalid_data(
            "local observation reconcile window must be non-empty",
        ));
    }
    Ok(())
}

fn prepare_local_metadata(
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    display_label: &str,
    target_redaction_profile: RedactionProfile,
) -> io::Result<()> {
    match store.load_source_metadata(identity.node_id()) {
        Ok(existing) => {
            if existing.kind() != SourceKind::Local || existing.display_label() != display_label {
                return Err(invalid_data(
                    "local observation source metadata does not match identity, kind, and label",
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => store
            .save_source_metadata_unfenced(&SourceMetadata::new_with_redaction_profile(
                identity.node_id().clone(),
                SourceKind::Local,
                display_label,
                target_redaction_profile,
            )?),
        Err(error) => Err(error),
    }
}

fn publish_local_metadata(
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    display_label: &str,
    target_redaction_profile: RedactionProfile,
) -> io::Result<()> {
    store
        .update_source_metadata_unfenced(identity.node_id(), |metadata| {
            if metadata.kind() != SourceKind::Local || metadata.display_label() != display_label {
                return Err(invalid_data(
                    "local observation source metadata changed during persistence",
                ));
            }
            metadata.set_aggregate_redaction_profile(target_redaction_profile);
            Ok(())
        })
        .map(|_| ())
}

fn local_state_directory(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
) -> PathBuf {
    store
        .source_directory(source_id)
        .join(redaction_profile.directory_name())
}

fn reserve_revision(
    store: &SourceHistoryStore,
    directory: &Path,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
) -> io::Result<u64> {
    let last = load_last_reserved_revision(store, directory, identity, redaction_profile)?;
    let revision = last
        .checked_add(1)
        .ok_or_else(|| invalid_data("local observation revision exhausted"))?;
    write_revision_state(store, directory, identity, redaction_profile, revision)?;
    Ok(revision)
}

fn raise_revision_floor(
    store: &SourceHistoryStore,
    directory: &Path,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    floor: u64,
) -> io::Result<u64> {
    let last = load_last_reserved_revision(store, directory, identity, redaction_profile)?;
    let floor = last.max(floor);
    if floor != last {
        write_revision_state(store, directory, identity, redaction_profile, floor)?;
    }
    Ok(floor)
}

fn load_last_reserved_revision(
    store: &SourceHistoryStore,
    directory: &Path,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
) -> io::Result<u64> {
    let current = read_optional_json_file::<LocalRevisionState>(
        &directory.join(STATE_FILE),
        MAX_STATE_BYTES,
    )?;
    match current {
        Some(state) => {
            if state.format_version != STATE_VERSION
                || state.profile_id != *store.profile_id()
                || state.source_id != *identity.node_id()
                || state.source_generation != identity.generation()
                || state.redaction_profile != redaction_profile
            {
                return Err(invalid_data(
                    "local observation revision state binding mismatch",
                ));
            }
            Ok(state.last_reserved_revision)
        }
        None => Ok(0),
    }
}

fn write_revision_state(
    store: &SourceHistoryStore,
    directory: &Path,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    last_reserved_revision: u64,
) -> io::Result<()> {
    let state = LocalRevisionState {
        format_version: STATE_VERSION,
        profile_id: store.profile_id().clone(),
        source_id: identity.node_id().clone(),
        source_generation: identity.generation(),
        redaction_profile,
        last_reserved_revision,
    };
    write_private_atomically(
        &directory.join(STATE_FILE),
        &encode_pretty_bounded(&state, MAX_STATE_BYTES)?,
    )
}

fn build_records(
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    observation: &HistoryObservation,
    mode: LocalObservationMode,
    revision: u64,
) -> io::Result<(
    Vec<SourceBucketRecord>,
    Vec<SourceWeeklyRecord>,
    usize,
    usize,
)> {
    let mut buckets = observation
        .half_hour_buckets
        .iter()
        .cloned()
        .map(|bucket| SourceBucketRecord::upsert(revision, bucket))
        .collect::<io::Result<Vec<_>>>()?;
    let mut weekly = observation
        .weekly_local_points
        .iter()
        .cloned()
        .map(|point| SourceWeeklyRecord::upsert(revision, point))
        .collect::<io::Result<Vec<_>>>()?;
    let mut bucket_tombstones = 0;
    let mut weekly_tombstones = 0;
    if let LocalObservationMode::Reconcile { from, to } = mode {
        let existing =
            store.load_source_records_since(identity.node_id(), redaction_profile, from)?;
        let incoming_buckets = observation
            .half_hour_buckets
            .iter()
            .map(|bucket| bucket.starts_at)
            .collect::<BTreeSet<_>>();
        for record in existing.records {
            if record.starts_at() < to
                && matches!(record.change(), SourceBucketChange::Upsert(_))
                && !incoming_buckets.contains(&record.starts_at())
            {
                buckets.push(SourceBucketRecord::tombstone(record.starts_at(), revision)?);
                bucket_tombstones += 1;
            }
        }
        let incoming_weekly = observation
            .weekly_local_points
            .iter()
            .map(|point| (point.observed_at, point.resets_at))
            .collect::<BTreeSet<_>>();
        for record in existing.weekly_records {
            let key = (record.observed_at(), record.resets_at());
            if record.observed_at() < to
                && matches!(record.change(), SourceWeeklyChange::Upsert(_))
                && !incoming_weekly.contains(&key)
            {
                weekly.push(SourceWeeklyRecord::tombstone(key.0, key.1, revision)?);
                weekly_tombstones += 1;
            }
        }
    }
    Ok((buckets, weekly, bucket_tombstones, weekly_tombstones))
}

#[allow(clippy::too_many_arguments)]
fn build_session_digest_records(
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    observation: &HistoryObservation,
    mode: LocalObservationMode,
    revision: u64,
    incoming: &[SourceSessionDigest],
    scan_complete: bool,
) -> io::Result<(Vec<SourceSessionDigestRecord>, usize)> {
    let reconcile = match mode {
        LocalObservationMode::Reconcile { from, to } if scan_complete => Some((from, to)),
        LocalObservationMode::Incremental | LocalObservationMode::Reconcile { .. } => None,
    };
    let load_since = incoming
        .iter()
        .map(SourceSessionDigest::range_start)
        .chain(reconcile.map(|(from, _)| from))
        .min();
    let existing = match load_since {
        Some(since) => {
            store
                .load_source_session_digest_records_since(
                    identity.node_id(),
                    redaction_profile,
                    since,
                )?
                .records
        }
        None => Vec::new(),
    };
    let existing = existing
        .into_iter()
        .map(|record| ((record.thread_id().clone(), record.range_start()), record))
        .collect::<BTreeMap<_, _>>();

    let mut incoming_by_key = BTreeMap::new();
    for digest in incoming {
        if digest.replica().source_id() != identity.node_id() {
            return Err(invalid_data(
                "local session digest belongs to a different source",
            ));
        }
        let key = (digest.replica().thread_id().clone(), digest.range_start());
        if incoming_by_key.insert(key, digest).is_some() {
            return Err(invalid_data(
                "local session digest evidence contains a duplicate key",
            ));
        }
    }

    let authoritative = reconcile.is_some();
    let mut records = Vec::with_capacity(incoming_by_key.len());
    for (key, digest) in &incoming_by_key {
        let existing_digest = existing.get(key).and_then(|record| match record.change() {
            SourceSessionDigestChange::Upsert(digest) => Some(digest.as_ref()),
            SourceSessionDigestChange::Tombstone => None,
        });
        if existing_digest == Some(*digest) {
            continue;
        }
        if !authoritative
            && existing_digest.is_some_and(|existing| !digest_is_non_decreasing(existing, digest))
        {
            continue;
        }
        records.push(SourceSessionDigestRecord::upsert(
            revision,
            (*digest).clone(),
        )?);
    }

    let mut tombstones = 0_usize;
    if let Some((from, to)) = reconcile {
        for (key, existing_record) in &existing {
            if incoming_by_key.contains_key(key)
                || existing_record.range_start() >= to
                || existing_record.range_end() <= from
                || !matches!(
                    existing_record.change(),
                    SourceSessionDigestChange::Upsert(_)
                )
            {
                continue;
            }
            records.push(SourceSessionDigestRecord::tombstone_with_retention_through(
                existing_record.thread_id().clone(),
                existing_record.range_start(),
                existing_record.range_end(),
                observation.observed_at.max(existing_record.range_start()),
                existing_record
                    .retention_through()
                    .max(observation.observed_at),
                revision,
            )?);
            tombstones = tombstones.saturating_add(1);
        }
    }
    Ok((records, tombstones))
}

fn digest_is_non_decreasing(
    existing: &SourceSessionDigest,
    incoming: &SourceSessionDigest,
) -> bool {
    if existing.range_end() != incoming.range_end()
        || incoming.covered_through() < existing.covered_through()
        || incoming.event_count() < existing.event_count()
        || (existing.coverage_complete() && !incoming.coverage_complete())
        || (existing.exact_event_identity() && !incoming.exact_event_identity())
        || !existing
            .observed_project_keys()
            .iter()
            .all(|key| incoming.observed_project_keys().contains(key))
    {
        return false;
    }
    if incoming.fingerprint() != existing.fingerprint()
        && incoming.event_count() <= existing.event_count()
    {
        return false;
    }
    metrics_are_non_decreasing(existing.metrics(), incoming.metrics())
}

fn metrics_are_non_decreasing(
    existing: &SessionUsageMetrics,
    incoming: &SessionUsageMetrics,
) -> bool {
    let old = existing.token_usage;
    let new = incoming.token_usage;
    let old_api = existing.api_equivalent_cost;
    let new_api = incoming.api_equivalent_cost;
    new.input_tokens >= old.input_tokens
        && new.cached_input_tokens >= old.cached_input_tokens
        && new.cache_write_input_tokens >= old.cache_write_input_tokens
        && new.output_tokens >= old.output_tokens
        && new.reasoning_output_tokens >= old.reasoning_output_tokens
        && new.total_tokens >= old.total_tokens
        && incoming.estimated_cost_units >= existing.estimated_cost_units
        && optional_u128_is_non_decreasing(
            existing.api_long_context_extra_cost_units,
            incoming.api_long_context_extra_cost_units,
        )
        && incoming.call_count >= existing.call_count
        && new_api.minimum_pico_usd >= old_api.minimum_pico_usd
        && new_api.maximum_pico_usd >= old_api.maximum_pico_usd
        && new_api.observed_samples >= old_api.observed_samples
        && new_api.priced_samples >= old_api.priced_samples
        && new_api.observed_tokens >= old_api.observed_tokens
        && new_api.priced_tokens >= old_api.priced_tokens
}

fn optional_u128_is_non_decreasing(existing: Option<u128>, incoming: Option<u128>) -> bool {
    match (existing, incoming) {
        (Some(existing), Some(incoming)) => incoming >= existing,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn validate_marker(
    store: &SourceHistoryStore,
    redaction_profile: RedactionProfile,
    epoch: u64,
    marker: &BackfillMarker,
) -> io::Result<()> {
    if marker.format_version != STATE_VERSION
        || marker.profile_id != *store.profile_id()
        || marker.redaction_profile != redaction_profile
        || marker.ownership_epoch != epoch
    {
        return Err(invalid_data(
            "summary backfill marker authority binding mismatch",
        ));
    }
    Ok(())
}

fn cleanup_state_temps(
    store: &SourceHistoryStore,
    directory: &Path,
    target: &str,
) -> io::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let mut removed = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if atomic_temporary_target_name(&name) == Some(target) {
            store.validate_private_path(directory)?;
            validate_published_private_file(&entry.path())?;
            fs::remove_file(entry.path())?;
            removed = true;
        }
    }
    if removed {
        sync_directory(directory)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;
    use crate::api_cost::API_PRICING_CATALOG_REVISION;
    use crate::domain::TokenUsage;
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, LocalHalfHourBucket,
        LocalProjectUsageGroup, WeeklyLocalPoint,
    };
    use crate::history_ownership::{
        HistoryOwnershipState, HistoryOwnershipStore, InitializeV1Outcome, OwnershipCasOutcome,
    };
    use crate::source_model::SessionReplicaKey;

    const PROFILE: &str = "0123456789abcdef";
    const SOURCE: &str = "node-0123456789abcdef0123456789abcdef";
    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn bucket(starts_at: DateTime<Utc>, total: u64) -> LocalHalfHourBucket {
        LocalHalfHourBucket {
            starts_at,
            ends_at: starts_at + Duration::minutes(15),
            sampled_at: starts_at + Duration::minutes(15),
            token_usage: TokenUsage {
                input_tokens: total,
                total_tokens: total,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(total),
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 1,
            groups: Vec::new(),
            project_groups: Vec::new(),
            partial_reasons: Vec::new(),
        }
    }

    fn weekly(observed_at: DateTime<Utc>, total: u64) -> WeeklyLocalPoint {
        WeeklyLocalPoint {
            observed_at,
            resets_at: observed_at + Duration::days(7),
            token_usage: TokenUsage {
                total_tokens: total,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(total),
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            call_count: 1,
            partial_reasons: Vec::new(),
        }
    }

    fn with_writer(
        test: impl FnOnce(&SourceIdentity, &SourceHistoryStore, &SourceHistoryWriter<'_, '_, '_>),
    ) {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        let profile: HistoryProfileId = PROFILE.parse().unwrap();
        let history = SourceHistoryStore::new(root.clone(), profile.clone());
        let ownership = HistoryOwnershipStore::new(root, profile, RedactionProfile::Redacted);
        let lease = ownership.acquire_writer_lease().unwrap();
        let v1 = match ownership.initialize_v1_active(&lease).unwrap() {
            InitializeV1Outcome::Initialized(value) | InitializeV1Outcome::Existing(value) => value,
        };
        let migrating = match ownership.begin_migration(&lease, &v1).unwrap() {
            OwnershipCasOutcome::Applied(value) => value,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected conflict"),
        };
        let active = match ownership
            .compare_and_transition(&lease, &migrating, HistoryOwnershipState::V2Active)
            .unwrap()
        {
            OwnershipCasOutcome::Applied(value) => value,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected conflict"),
        };
        let authority = ownership.authorize_v2_write(&lease, &active).unwrap();
        let writer = history.writer(&authority).unwrap();
        let identity = SourceIdentity::from_test_parts(SOURCE.parse().unwrap(), SECRET);
        test(&identity, &history, &writer);
    }

    fn session_digest(
        identity: &SourceIdentity,
        thread_id: &str,
        range_start: DateTime<Utc>,
        fingerprint_hex: char,
        total_tokens: u64,
    ) -> SourceSessionDigest {
        let range_end = range_start + Duration::days(1);
        SourceSessionDigest::new(
            SessionReplicaKey::new(identity.node_id().clone(), thread_id.parse().unwrap()),
            range_start,
            range_end,
            range_end,
            format!(
                "session-digest-sha256-v1-{}",
                fingerprint_hex.to_string().repeat(64)
            )
            .parse()
            .unwrap(),
            format!(
                "session-digest-sha256-v1-{}",
                fingerprint_hex.to_string().repeat(64)
            )
            .parse()
            .unwrap(),
            1,
            true,
            true,
            Vec::new(),
            SessionUsageMetrics {
                token_usage: TokenUsage {
                    input_tokens: total_tokens,
                    total_tokens,
                    ..TokenUsage::default()
                },
                estimated_cost_units: u128::from(total_tokens),
                api_long_context_extra_cost_units: Some(0),
                call_count: 1,
                metric_revision: HISTORY_METRIC_REVISION,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
                api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
                ..SessionUsageMetrics::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn combined_local_snapshot_cannot_observe_bucket_digest_revision_splice() {
        with_writer(|identity, history, writer| {
            let starts_at = at(30, 12, 0);
            writer
                .record_local_observation_with_session_digests(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: at(30, 12, 15),
                        half_hour_buckets: vec![bucket(starts_at, 10)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                    &[session_digest(identity, "thread-one", starts_at, 'a', 10)],
                    true,
                )
                .unwrap();

            let lock_directory = history.source_directory(identity.node_id());
            let state_lock = open_lock_file(&lock_directory, STATE_LOCK).unwrap();
            lock_exclusive(&state_lock, &lock_directory, STATE_LOCK).unwrap();

            history
                .record_source_bucket_changes_unfenced(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    &[SourceBucketRecord::upsert(2, bucket(starts_at, 20)).unwrap()],
                )
                .unwrap();

            let reader_store = history.clone();
            let reader_source = identity.node_id().clone();
            let (started_tx, started_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            let reader = thread::spawn(move || {
                started_tx.send(()).unwrap();
                let result = reader_store.load_local_observation_snapshot_since(
                    &reader_source,
                    RedactionProfile::Redacted,
                    starts_at,
                    true,
                );
                result_tx.send(result).unwrap();
            });
            started_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();
            assert!(
                result_rx
                    .recv_timeout(StdDuration::from_millis(100))
                    .is_err(),
                "the combined reader must wait while a local revision is only partially written"
            );

            history
                .record_source_session_digest_changes_unfenced(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    &[SourceSessionDigestRecord::upsert(
                        2,
                        session_digest(identity, "thread-one", starts_at, 'b', 20),
                    )
                    .unwrap()],
                )
                .unwrap();
            drop(state_lock);

            let snapshot = result_rx
                .recv_timeout(StdDuration::from_secs(2))
                .unwrap()
                .unwrap();
            reader.join().unwrap();
            assert_eq!(snapshot.buckets.len(), 1);
            assert_eq!(snapshot.buckets[0].token_usage.total_tokens, 20);
            assert_eq!(snapshot.session_digest_records.len(), 1);
            let SourceSessionDigestChange::Upsert(digest) =
                snapshot.session_digest_records[0].change()
            else {
                panic!("the newest local digest must remain an upsert");
            };
            assert_eq!(digest.metrics().token_usage.total_tokens, 20);
        });
    }

    #[test]
    fn combined_local_snapshot_locks_before_the_first_profile_observation() {
        with_writer(|identity, history, _writer| {
            prepare_local_metadata(history, identity, "local", RedactionProfile::Redacted).unwrap();
            let starts_at = at(30, 12, 0);
            let lock_directory = history.source_directory(identity.node_id());
            let state_lock = open_lock_file(&lock_directory, STATE_LOCK).unwrap();
            lock_exclusive(&state_lock, &lock_directory, STATE_LOCK).unwrap();

            let reader_store = history.clone();
            let reader_source = identity.node_id().clone();
            let (started_tx, started_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            let reader = thread::spawn(move || {
                started_tx.send(()).unwrap();
                let result = reader_store.load_local_observation_snapshot_since(
                    &reader_source,
                    RedactionProfile::Redacted,
                    starts_at,
                    true,
                );
                result_tx.send(result).unwrap();
            });
            started_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();
            assert!(
                result_rx
                    .recv_timeout(StdDuration::from_millis(100))
                    .is_err(),
                "the first-observation reader must acquire the stable source lock"
            );

            history
                .record_source_bucket_changes_unfenced(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    &[SourceBucketRecord::upsert(1, bucket(starts_at, 10)).unwrap()],
                )
                .unwrap();
            history
                .record_source_session_digest_changes_unfenced(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    &[SourceSessionDigestRecord::upsert(
                        1,
                        session_digest(identity, "thread-one", starts_at, 'a', 10),
                    )
                    .unwrap()],
                )
                .unwrap();
            drop(state_lock);

            let snapshot = result_rx
                .recv_timeout(StdDuration::from_secs(2))
                .unwrap()
                .unwrap();
            reader.join().unwrap();
            assert_eq!(snapshot.buckets.len(), 1);
            assert_eq!(snapshot.buckets[0].token_usage.total_tokens, 10);
            assert_eq!(snapshot.session_digest_records.len(), 1);
            let SourceSessionDigestChange::Upsert(digest) =
                snapshot.session_digest_records[0].change()
            else {
                panic!("the first local digest must be visible with its matching bucket");
            };
            assert_eq!(digest.metrics().token_usage.total_tokens, 10);
        });
    }

    #[test]
    fn first_observation_does_not_publish_metadata_before_its_state_lock() {
        with_writer(|identity, history, writer| {
            let starts_at = at(30, 12, 0);
            let lock_directory = history.source_directory(identity.node_id());
            history.prepare_private_directory(&lock_directory).unwrap();
            let (locked_tx, locked_rx) = mpsc::channel();
            let (inspected_tx, inspected_rx) = mpsc::channel();
            let lock_holder = thread::spawn(move || {
                let state_lock = open_lock_file(&lock_directory, STATE_LOCK).unwrap();
                lock_exclusive(&state_lock, &lock_directory, STATE_LOCK).unwrap();
                locked_tx.send(()).unwrap();
                inspected_rx
                    .recv_timeout(StdDuration::from_secs(1))
                    .unwrap();
                drop(state_lock);
            });
            locked_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();

            let inspector_store = history.clone();
            let inspector_source = identity.node_id().clone();
            let inspector = thread::spawn(move || {
                let deadline = std::time::Instant::now() + StdDuration::from_millis(100);
                let mut became_visible = false;
                while std::time::Instant::now() < deadline {
                    match inspector_store.load_source_metadata(&inspector_source) {
                        Ok(_) => {
                            became_visible = true;
                            break;
                        }
                        Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
                    }
                    thread::sleep(StdDuration::from_millis(5));
                }
                inspected_tx.send(()).unwrap();
                became_visible
            });

            // This call remains on the writer-lease thread. While it waits for
            // the state lock above, the inspector verifies that source.json is
            // still absent. The previous metadata-first ordering made the
            // inspector return true before the lock holder released.
            writer
                .record_local_observation_with_session_digests(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: at(30, 12, 15),
                        half_hour_buckets: vec![bucket(starts_at, 10)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                    &[session_digest(identity, "thread-one", starts_at, 'a', 10)],
                    true,
                )
                .unwrap();
            lock_holder.join().unwrap();
            assert!(
                !inspector.join().unwrap(),
                "first-observation metadata became visible before its state lock"
            );

            let snapshot = history
                .load_local_observation_snapshot_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    starts_at,
                    true,
                )
                .unwrap();
            assert_eq!(snapshot.buckets.len(), 1);
            assert_eq!(snapshot.session_digest_records.len(), 1);
        });
    }

    #[test]
    fn reserved_revision_survives_a_crash_as_a_gap_and_is_never_reused() {
        with_writer(|identity, history, writer| {
            prepare_local_metadata(history, identity, "local", RedactionProfile::Redacted).unwrap();
            let directory =
                local_state_directory(history, identity.node_id(), RedactionProfile::Redacted);
            history.prepare_private_directory(&directory).unwrap();
            assert_eq!(
                reserve_revision(history, &directory, identity, RedactionProfile::Redacted)
                    .unwrap(),
                1
            );
            let report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: at(30, 12, 15),
                        half_hour_buckets: vec![bucket(at(30, 12, 0), 10)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(report.revision, 2);
            let records = history
                .load_source_records_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    at(30, 0, 0),
                )
                .unwrap();
            assert_eq!(records.records[0].revision(), 2);
        });
    }

    #[test]
    fn durable_revision_floor_is_monotonic_and_next_write_is_strictly_newer() {
        with_writer(|identity, _history, writer| {
            assert_eq!(
                writer
                    .ensure_local_observation_revision_floor(
                        identity,
                        RedactionProfile::Redacted,
                        5,
                    )
                    .unwrap(),
                5
            );
            assert_eq!(
                writer
                    .ensure_local_observation_revision_floor(
                        identity,
                        RedactionProfile::Redacted,
                        3,
                    )
                    .unwrap(),
                5
            );
            let report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: at(30, 12, 15),
                        half_hour_buckets: vec![bucket(at(30, 12, 0), 10)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(report.revision, 6);
        });
    }

    #[test]
    fn partial_digest_writes_never_lower_or_delete_and_complete_reconcile_tombstones() {
        with_writer(|identity, history, writer| {
            let range_start = at(29, 0, 0);
            let observed_at = at(30, 12, 0);
            let observation = HistoryObservation {
                observed_at,
                ..HistoryObservation::default()
            };
            let strong = session_digest(identity, "thread-a", range_start, 'a', 100);
            let stale = session_digest(identity, "thread-b", range_start, 'b', 200);
            let first = writer
                .record_local_observation_with_session_digests(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                    &[strong.clone(), stale],
                    true,
                )
                .unwrap();
            assert_eq!(first.revision, 1);
            assert_eq!(first.session_digest_tombstones, 0);

            let weaker = session_digest(identity, "thread-a", range_start, 'c', 50);
            writer
                .record_local_observation_with_session_digests(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                    &[weaker],
                    false,
                )
                .unwrap();
            let after_weaker = history
                .load_source_session_digest_records_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    range_start,
                )
                .unwrap();
            let retained = after_weaker
                .records
                .iter()
                .find(|record| record.thread_id().as_str() == "thread-a")
                .unwrap();
            assert_eq!(retained.revision(), 1);
            let SourceSessionDigestChange::Upsert(retained) = retained.change() else {
                panic!("the stronger digest must remain active")
            };
            assert_eq!(retained.metrics().token_usage.total_tokens, 100);

            let incomplete = writer
                .record_local_observation_with_session_digests(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Reconcile {
                        from: range_start,
                        to: observed_at,
                    },
                    std::slice::from_ref(&strong),
                    false,
                )
                .unwrap();
            assert_eq!(incomplete.session_digest_tombstones, 0);
            assert!(
                history
                    .load_source_session_digest_records_since(
                        identity.node_id(),
                        RedactionProfile::Redacted,
                        range_start,
                    )
                    .unwrap()
                    .records
                    .iter()
                    .any(|record| {
                        record.thread_id().as_str() == "thread-b"
                            && matches!(record.change(), SourceSessionDigestChange::Upsert(_))
                    })
            );

            let complete = writer
                .record_local_observation_with_session_digests(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Reconcile {
                        from: range_start,
                        to: observed_at,
                    },
                    &[strong],
                    true,
                )
                .unwrap();
            assert_eq!(complete.session_digest_tombstones, 1);
            assert!(
                history
                    .load_source_session_digest_records_since(
                        identity.node_id(),
                        RedactionProfile::Redacted,
                        range_start,
                    )
                    .unwrap()
                    .records
                    .iter()
                    .any(|record| {
                        record.thread_id().as_str() == "thread-b"
                            && matches!(record.change(), SourceSessionDigestChange::Tombstone)
                    })
            );
        });
    }

    #[test]
    fn metadata_registration_is_idempotent_and_conflicts_fail_closed() {
        with_writer(|identity, history, writer| {
            let observation = HistoryObservation::default();
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(
                history
                    .load_source_metadata(identity.node_id())
                    .unwrap()
                    .kind(),
                SourceKind::Local
            );
            let error = writer
                .record_local_observation(
                    identity,
                    "different",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn redacted_direct_local_write_scrubs_persisted_content() {
        with_writer(|identity, history, writer| {
            let mut private_bucket = bucket(at(30, 11, 0), 10);
            private_bucket.project_groups.push(LocalProjectUsageGroup {
                thread_id: "thread-private".to_owned(),
                title: Some("private customer title".to_owned()),
                message_preview: Some("rotate the private credential".to_owned()),
                ..LocalProjectUsageGroup::default()
            });
            let observation = HistoryObservation {
                observed_at: at(30, 11, 15),
                half_hour_buckets: vec![private_bucket],
                ..HistoryObservation::default()
            };
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                )
                .unwrap();

            // Sanitizing the persisted clone must not mutate the collector's
            // in-memory observation.
            assert_eq!(
                observation.half_hour_buckets[0].project_groups[0]
                    .title
                    .as_deref(),
                Some("private customer title")
            );
            let loaded = history
                .load_source_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    at(30, 10, 0),
                )
                .unwrap();
            let group = &loaded.buckets[0].project_groups[0];
            assert_eq!(group.title.as_deref(), Some("[redacted]"));
            assert_eq!(group.message_preview.as_deref(), Some("[redacted]"));
        });
    }

    #[test]
    fn aggregate_redaction_switch_preserves_all_other_source_policy() {
        with_writer(|identity, history, writer| {
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation::default(),
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            let before = history.load_source_metadata(identity.node_id()).unwrap();
            assert_eq!(
                before.aggregate_redaction_profile(),
                RedactionProfile::Redacted
            );
            let switched = writer
                .update_source_metadata(identity.node_id(), |metadata| {
                    metadata.set_aggregate_redaction_profile(RedactionProfile::PreviewEnabled);
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                switched.aggregate_redaction_profile(),
                RedactionProfile::PreviewEnabled
            );
            assert_eq!(switched.kind(), before.kind());
            assert_eq!(
                switched.include_in_aggregates(),
                before.include_in_aggregates()
            );
            assert_eq!(switched.detached(), before.detached());

            let restored = writer
                .update_source_metadata(identity.node_id(), |metadata| {
                    metadata.set_aggregate_redaction_profile(RedactionProfile::Redacted);
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                restored.aggregate_redaction_profile(),
                RedactionProfile::Redacted
            );
            assert_eq!(restored.kind(), before.kind());
            assert_eq!(
                restored.include_in_aggregates(),
                before.include_in_aggregates()
            );
            assert_eq!(restored.detached(), before.detached());
        });
    }

    #[test]
    fn failed_profile_switch_keeps_the_previous_aggregate_profile_visible() {
        with_writer(|identity, history, writer| {
            history
                .save_source_metadata(
                    &SourceMetadata::new_with_redaction_profile(
                        identity.node_id().clone(),
                        SourceKind::Local,
                        "local",
                        RedactionProfile::PreviewEnabled,
                    )
                    .unwrap(),
                )
                .unwrap();

            let target_directory =
                local_state_directory(history, identity.node_id(), RedactionProfile::Redacted);
            history
                .prepare_private_directory(&target_directory)
                .unwrap();
            let buckets_blocker =
                history.source_buckets_directory(identity.node_id(), RedactionProfile::Redacted);
            fs::write(&buckets_blocker, b"not a directory").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(&buckets_blocker).unwrap().permissions();
                permissions.set_mode(0o600);
                fs::set_permissions(&buckets_blocker, permissions).unwrap();
            }

            let observation = HistoryObservation {
                observed_at: at(30, 12, 15),
                half_hour_buckets: vec![bucket(at(30, 12, 0), 10)],
                ..HistoryObservation::default()
            };
            assert!(
                writer
                    .record_local_observation(
                        identity,
                        "local",
                        RedactionProfile::Redacted,
                        &observation,
                        LocalObservationMode::Incremental,
                    )
                    .is_err()
            );
            assert_eq!(
                history
                    .load_source_metadata(identity.node_id())
                    .unwrap()
                    .aggregate_redaction_profile(),
                RedactionProfile::PreviewEnabled
            );

            fs::remove_file(&buckets_blocker).unwrap();
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(
                history
                    .load_source_metadata(identity.node_id())
                    .unwrap()
                    .aggregate_redaction_profile(),
                RedactionProfile::Redacted
            );
        });
    }

    #[test]
    fn reconcile_tombstones_missing_records_but_incremental_never_deletes() {
        with_writer(|identity, history, writer| {
            let first = at(30, 10, 0);
            let second = at(30, 10, 15);
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: second + Duration::minutes(15),
                        half_hour_buckets: vec![bucket(first, 10), bucket(second, 20)],
                        weekly_local_points: vec![weekly(first, 30), weekly(second, 40)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: second + Duration::minutes(15),
                        half_hour_buckets: vec![bucket(first, 11)],
                        weekly_local_points: vec![weekly(first, 31)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            let before = history
                .load_source_since(identity.node_id(), RedactionProfile::Redacted, first)
                .unwrap();
            assert_eq!(before.buckets.len(), 2);
            assert_eq!(before.weekly_local_points.len(), 2);

            let report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: second + Duration::minutes(15),
                        half_hour_buckets: vec![bucket(first, 11)],
                        weekly_local_points: vec![weekly(first, 31)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Reconcile {
                        from: first,
                        to: second + Duration::minutes(15),
                    },
                )
                .unwrap();
            assert_eq!(report.bucket_tombstones, 1);
            assert_eq!(report.weekly_tombstones, 1);
            let after = history
                .load_source_since(identity.node_id(), RedactionProfile::Redacted, first)
                .unwrap();
            assert_eq!(after.buckets.len(), 1);
            assert_eq!(after.weekly_local_points.len(), 1);
        });
    }

    #[test]
    fn summary_backfill_marker_is_monotonic_and_complete_is_terminal() {
        with_writer(|_identity, history, writer| {
            assert_eq!(writer.load_v2_summary_backfill_attempt().unwrap(), None);
            let first = writer
                .mark_v2_summary_backfill_attempt(at(30, 12, 0), false)
                .unwrap();
            assert!(!first.complete);
            let complete = writer
                .mark_v2_summary_backfill_attempt(at(30, 12, 0), true)
                .unwrap();
            assert!(complete.complete);
            assert_eq!(
                writer.load_v2_summary_backfill_attempt().unwrap(),
                Some(complete)
            );
            assert_eq!(
                history
                    .load_v2_summary_backfill_attempt(
                        RedactionProfile::Redacted,
                        writer.authority.expected_manifest().epoch(),
                    )
                    .unwrap(),
                Some(complete)
            );
            assert!(
                history
                    .load_v2_summary_backfill_attempt(
                        RedactionProfile::Redacted,
                        writer.authority.expected_manifest().epoch() + 1,
                    )
                    .is_err()
            );
            let later_partial = writer
                .mark_v2_summary_backfill_attempt(at(30, 13, 0), false)
                .unwrap();
            assert_eq!(later_partial, complete);
        });
    }
}
