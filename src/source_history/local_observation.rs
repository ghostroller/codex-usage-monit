use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::*;
use crate::history::{
    HistoryObservation, half_hour_bucket_payload_eq, redacted_history_observation,
    weekly_local_point_payload_eq,
};
use crate::source_identity::SourceIdentity;

const STATE_FILE: &str = "local-observation-state.json";
const STATE_LOCK: &str = "local-observation.lock";
const JOURNAL_FILE: &str = "local-observation-pending.json";
const MAX_JOURNAL_BYTES: u64 = MAX_SHARD_FILE_BYTES;
pub const LOCAL_OBSERVATION_RECOVERY_PENDING_WARNING: &str = "local_observation_recovery_pending";
const MARKER_FILE: &str = "summary-backfill-attempt.json";
const MARKER_LOCK: &str = "summary-backfill-attempt.lock";
const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 16 * 1024;
const WEEKLY_LIVE_TAIL_LOOKBACK_MINUTES: i64 = 30;

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_LOCAL_OBSERVATION_STAGE: std::cell::Cell<Option<&'static str>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn inject_local_observation_failure_after(stage: &'static str) {
    FAIL_AFTER_LOCAL_OBSERVATION_STAGE.with(|fail_after| fail_after.set(Some(stage)));
}

#[cfg(test)]
fn fail_after_local_observation_stage(stage: &'static str) -> io::Result<()> {
    FAIL_AFTER_LOCAL_OBSERVATION_STAGE.with(|fail_after| {
        if fail_after.get() == Some(stage) {
            fail_after.set(None);
            Err(io::Error::other(format!("injected failure after {stage}")))
        } else {
            Ok(())
        }
    })
}

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
    /// Recovery can restore query visibility even when all family writes are
    /// semantic no-ops. Readers must invalidate projections in either case.
    pub recovered_pending: bool,
    pub account: SourceHistoryWriteReport,
    pub buckets: SourceHistoryWriteReport,
    pub weekly: SourceHistoryWriteReport,
    pub session_digests: SourceHistoryWriteReport,
    /// Number of validated records presented to each family writer. Shard
    /// reports below distinguish durable writes from semantic no-ops; these
    /// counts make recorder traces useful without exposing any payload.
    pub account_records: usize,
    pub bucket_records: usize,
    pub weekly_records: usize,
    pub session_digest_records: usize,
    pub bucket_tombstones: usize,
    pub weekly_tombstones: usize,
    pub session_digest_tombstones: usize,
    pub garbage_collection: LocalObservationGarbageCollectionReport,
}

impl LocalObservationWriteReport {
    fn include_recovery(&mut self, recovery: Self) {
        self.recovered_pending |= recovery.recovered_pending;
        for (current, recovered) in [
            (&mut self.account, recovery.account),
            (&mut self.buckets, recovery.buckets),
            (&mut self.weekly, recovery.weekly),
            (&mut self.session_digests, recovery.session_digests),
        ] {
            current.shards_written = current
                .shards_written
                .saturating_add(recovered.shards_written);
            current.shards_skipped = current
                .shards_skipped
                .saturating_add(recovered.shards_skipped);
        }
        for (current, recovered) in [
            (&mut self.account_records, recovery.account_records),
            (&mut self.bucket_records, recovery.bucket_records),
            (&mut self.weekly_records, recovery.weekly_records),
            (
                &mut self.session_digest_records,
                recovery.session_digest_records,
            ),
            (&mut self.bucket_tombstones, recovery.bucket_tombstones),
            (&mut self.weekly_tombstones, recovery.weekly_tombstones),
            (
                &mut self.session_digest_tombstones,
                recovery.session_digest_tombstones,
            ),
        ] {
            *current = current.saturating_add(recovered);
        }
    }
}

/// One revision-consistent read of every local observation family used by a
/// history query. The local writer publishes buckets, weekly points, and
/// session digests under one stable source-level state lock; readers must hold
/// the shared side of that same lock so they cannot splice two observation
/// revisions together, including while the first profile namespace is being
/// created. After an interrupted write, combined reads remain unavailable
/// until an authorized writer replays the durable batch. Individual account
/// samples and raw family inspection APIs have independent read contracts.
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

/// A redo batch contains only the validated differences for one observation.
/// It is published before any family changes and retained until every family
/// and the selected-profile metadata are durable.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingLocalObservation {
    binding: LocalRevisionState,
    display_label: String,
    account_points: Vec<QuotaPoint>,
    bucket_records: Vec<SourceBucketRecord>,
    weekly_records: Vec<SourceWeeklyRecord>,
    session_digest_records: Vec<SourceSessionDigestRecord>,
}

#[derive(Debug)]
struct LocalObservationRecoveryPending;

impl std::fmt::Display for LocalObservationRecoveryPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(LOCAL_OBSERVATION_RECOVERY_PENDING_WARNING)
    }
}

impl std::error::Error for LocalObservationRecoveryPending {}

pub(crate) fn is_local_observation_recovery_pending(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|error| error.is::<LocalObservationRecoveryPending>())
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
            let _lock = lock_exclusive(lock, &lock_directory, STATE_LOCK)?;
            prepare_local_metadata(store, &identity, &display_label, redaction_profile)?;
            let state_directory =
                local_state_directory(store, identity.node_id(), redaction_profile);
            store.prepare_private_directory(&state_directory)?;
            cleanup_state_temps(store, &state_directory, STATE_FILE)?;
            cleanup_state_temps(store, &state_directory, JOURNAL_FILE)?;
            let mut recovered = LocalObservationWriteReport::default();
            if let Some(pending) = read_optional_json_file::<PendingLocalObservation>(
                &state_directory.join(JOURNAL_FILE),
                MAX_JOURNAL_BYTES,
            )? {
                validate_pending_observation(
                    store,
                    &state_directory,
                    &identity,
                    redaction_profile,
                    &pending,
                )?;
                // Replay the original frozen records before inspecting the
                // next input. An empty/partial observation cannot silently
                // discard an older interrupted family's changes.
                recovered.account =
                    store.record_account_points_unfenced(&pending.account_points)?;
                recovered.buckets = store.record_source_bucket_changes_unfenced(
                    identity.node_id(),
                    redaction_profile,
                    &pending.bucket_records,
                )?;
                #[cfg(test)]
                fail_after_local_observation_stage("recovery_buckets")?;
                recovered.weekly = store.record_source_weekly_changes_unfenced(
                    identity.node_id(),
                    redaction_profile,
                    &pending.weekly_records,
                )?;
                recovered.session_digests = store.record_source_session_digest_changes_unfenced(
                    identity.node_id(),
                    redaction_profile,
                    &pending.session_digest_records,
                )?;
                self.validate()?;
                publish_local_metadata(
                    store,
                    &identity,
                    &pending.display_label,
                    redaction_profile,
                )?;
                remove_pending_observation(store, &state_directory)?;
                recovered.recovered_pending = true;
                recovered.account_records = pending.account_points.len();
                recovered.bucket_records = pending.bucket_records.len();
                recovered.weekly_records = pending.weekly_records.len();
                recovered.session_digest_records = pending.session_digest_records.len();
                recovered.bucket_tombstones = pending
                    .bucket_records
                    .iter()
                    .filter(|record| matches!(record.change(), SourceBucketChange::Tombstone))
                    .count();
                recovered.weekly_tombstones = pending
                    .weekly_records
                    .iter()
                    .filter(|record| matches!(record.change(), SourceWeeklyChange::Tombstone))
                    .count();
                recovered.session_digest_tombstones = pending
                    .session_digest_records
                    .iter()
                    .filter(|record| {
                        matches!(record.change(), SourceSessionDigestChange::Tombstone)
                    })
                    .count();
            }

            // Reserve before touching any shard. A crash after this atomic publish
            // intentionally leaves a gap; the number is never issued again.
            let revision = reserve_revision(store, &state_directory, &identity, redaction_profile)?;
            #[cfg(test)]
            fail_after_local_observation_stage("revision")?;
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
            let account_records = observation.quota_points.len();
            let bucket_record_count = bucket_records.len();
            let weekly_record_count = weekly_records.len();
            let session_digest_record_count = session_digest_records.len();
            let pending = PendingLocalObservation {
                binding: LocalRevisionState {
                    format_version: STATE_VERSION,
                    profile_id: store.profile_id().clone(),
                    source_id: identity.node_id().clone(),
                    source_generation: identity.generation(),
                    redaction_profile,
                    last_reserved_revision: revision,
                },
                display_label: display_label.clone(),
                account_points: observation.quota_points.clone(),
                bucket_records,
                weekly_records,
                session_digest_records,
            };
            validate_pending_observation(
                store,
                &state_directory,
                &identity,
                redaction_profile,
                &pending,
            )?;
            let has_changes = !pending.account_points.is_empty()
                || !pending.bucket_records.is_empty()
                || !pending.weekly_records.is_empty()
                || !pending.session_digest_records.is_empty();
            if has_changes {
                write_private_atomically(
                    &state_directory.join(JOURNAL_FILE),
                    &encode_pretty_bounded(&pending, MAX_JOURNAL_BYTES)?,
                )?;
            }
            let account = store.record_account_points_unfenced(&observation.quota_points)?;
            #[cfg(test)]
            fail_after_local_observation_stage("account")?;
            let buckets = store.record_source_bucket_changes_unfenced(
                identity.node_id(),
                redaction_profile,
                &pending.bucket_records,
            )?;
            #[cfg(test)]
            fail_after_local_observation_stage("buckets")?;
            let weekly = store.record_source_weekly_changes_unfenced(
                identity.node_id(),
                redaction_profile,
                &pending.weekly_records,
            )?;
            #[cfg(test)]
            fail_after_local_observation_stage("weekly")?;
            let session_digests = store.record_source_session_digest_changes_unfenced(
                identity.node_id(),
                redaction_profile,
                &pending.session_digest_records,
            )?;
            #[cfg(test)]
            fail_after_local_observation_stage("session_digests")?;
            // Keep an existing source's selected profile visible until every
            // target-namespace write succeeds and the ownership fence is
            // freshly validated. A failed target write therefore cannot hide
            // the last known-good profile or expose a partial new one.
            self.validate()?;
            publish_local_metadata(store, &identity, &display_label, redaction_profile)?;
            #[cfg(test)]
            fail_after_local_observation_stage("metadata")?;
            if has_changes {
                remove_pending_observation(store, &state_directory)?;
            }
            let mut report = LocalObservationWriteReport {
                revision,
                recovered_pending: false,
                account,
                buckets,
                weekly,
                session_digests,
                account_records,
                bucket_records: bucket_record_count,
                weekly_records: weekly_record_count,
                session_digest_records: session_digest_record_count,
                bucket_tombstones,
                weekly_tombstones,
                session_digest_tombstones,
                garbage_collection: LocalObservationGarbageCollectionReport::default(),
            };
            report.include_recovery(recovered);
            Ok(report)
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
            let _lock = lock_exclusive(lock, &lock_directory, STATE_LOCK)?;
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
            let _lock = lock_exclusive(lock, &directory, MARKER_LOCK)?;
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
    /// Loads the monotonic local-observation revision under the same shared
    /// source lock used by combined history reads. TUI projection caches use
    /// this cheap stamp to notice recorder writes without reopening every
    /// historical shard.
    pub fn load_local_observation_revision(
        &self,
        identity: &SourceIdentity,
        redaction_profile: RedactionProfile,
    ) -> io::Result<u64> {
        let lock_directory = self.source_directory(identity.node_id());
        if !self.private_directory_exists(&lock_directory)? {
            return Ok(0);
        }
        self.validate_private_path(&lock_directory)?;
        let state_lock = open_lock_file(&lock_directory, STATE_LOCK)?;
        let state_lock = lock_shared(state_lock, &lock_directory, STATE_LOCK)?;
        let directory = local_state_directory(self, identity.node_id(), redaction_profile);
        let revision = load_last_reserved_revision(self, &directory, identity, redaction_profile);
        drop(state_lock);
        revision
    }

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
        let mut budget = SourceHistoryReadBudget::for_query();
        self.load_local_observation_snapshot_since_with_budget(
            source_id,
            redaction_profile,
            since,
            include_session_digests,
            &mut budget,
        )
    }

    pub(crate) fn load_local_observation_snapshot_since_with_budget(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        include_session_digests: bool,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<LocalObservationSnapshot> {
        budget.charge_source()?;
        let lock_directory = self.source_directory(source_id);
        self.validate_private_path(&lock_directory)?;
        let state_lock = open_lock_file(&lock_directory, STATE_LOCK)?;
        let state_lock = lock_shared(state_lock, &lock_directory, STATE_LOCK)?;

        let state_directory = local_state_directory(self, source_id, redaction_profile);
        let pending_path = state_directory.join(JOURNAL_FILE);
        if self.private_directory_exists(&state_directory)? {
            self.validate_private_path(&state_directory)?;
            match fs::symlink_metadata(&pending_path) {
                Ok(_) => {
                    validate_published_private_file(&pending_path)?;
                    return Err(io::Error::other(LocalObservationRecoveryPending));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        let snapshot = self.with_source_metadata_shared(source_id, |source| {
            if source.kind() != SourceKind::Local {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local observation snapshot requires a local source",
                ));
            }

            let bucket_records = self.load_source_bucket_records_from_directory_with_budget(
                source_id,
                redaction_profile,
                since,
                &self.source_buckets_directory(source_id, redaction_profile),
                budget,
            )?;
            let weekly_records = self.load_source_weekly_records_since_with_budget(
                source_id,
                redaction_profile,
                since,
                budget,
            )?;
            let session_digest_records = if include_session_digests {
                self.load_source_session_digest_records_from_directory_with_budget(
                    source_id,
                    redaction_profile,
                    since,
                    &self.source_digests_directory(source_id, redaction_profile),
                    budget,
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
        let _lock = lock_shared(lock, &directory, MARKER_LOCK)?;
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

fn validate_pending_observation(
    store: &SourceHistoryStore,
    directory: &Path,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    pending: &PendingLocalObservation,
) -> io::Result<()> {
    let binding = &pending.binding;
    let revision = binding.last_reserved_revision;
    if binding.format_version != STATE_VERSION
        || binding.profile_id != *store.profile_id()
        || binding.source_id != *identity.node_id()
        || binding.source_generation != identity.generation()
        || binding.redaction_profile != redaction_profile
        || revision == 0
        || revision > load_last_reserved_revision(store, directory, identity, redaction_profile)?
    {
        return Err(invalid_data("local observation journal binding mismatch"));
    }
    SourceMetadata::new_with_redaction_profile(
        identity.node_id().clone(),
        SourceKind::Local,
        &pending.display_label,
        redaction_profile,
    )?;
    let metadata = store.load_source_metadata(identity.node_id())?;
    if metadata.kind() != SourceKind::Local || metadata.display_label() != pending.display_label {
        return Err(invalid_data("local observation journal metadata mismatch"));
    }
    for point in &pending.account_points {
        validate_account_quota_point(point)?;
    }
    let mut buckets = BTreeSet::new();
    for record in &pending.bucket_records {
        record.validate()?;
        if record.revision() != revision || !buckets.insert(record.starts_at()) {
            return Err(invalid_data(
                "local observation journal has conflicting bucket records",
            ));
        }
    }
    let mut weekly = BTreeSet::new();
    for record in &pending.weekly_records {
        record.validate()?;
        if record.revision() != revision
            || !weekly.insert((record.observed_at(), record.resets_at()))
        {
            return Err(invalid_data(
                "local observation journal has conflicting weekly records",
            ));
        }
    }
    let mut digests = BTreeSet::new();
    for record in &pending.session_digest_records {
        record.validate()?;
        if record.revision() != revision
            || !digests.insert((record.thread_id(), record.range_start()))
        {
            return Err(invalid_data(
                "local observation journal has conflicting digest records",
            ));
        }
        if let SourceSessionDigestChange::Upsert(digest) = record.change()
            && digest.replica().source_id() != identity.node_id()
        {
            return Err(invalid_data(
                "local observation journal digest source mismatch",
            ));
        }
    }
    Ok(())
}

fn remove_pending_observation(store: &SourceHistoryStore, directory: &Path) -> io::Result<()> {
    store.validate_private_path(directory)?;
    let path = directory.join(JOURNAL_FILE);
    validate_published_private_file(&path)?;
    fs::remove_file(path)?;
    sync_directory(directory)
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
    // A complete collection observation carries the whole lookback window.
    // Reissuing every unchanged payload with the newly reserved revision
    // rewrites every historical day on every recorder tick. Read the relevant
    // live records once and only publish actual semantic changes. `sampledAt`
    // is intentionally ignored by the bucket payload comparison: advancing
    // the collection wall clock adds no evidence to an already closed bucket.
    let load_since = observation
        .half_hour_buckets
        .iter()
        .map(|bucket| bucket.starts_at)
        .chain(
            observation
                .weekly_local_points
                .iter()
                // Include the preceding checkpoint even when a stateless
                // observation has no buckets. Without this bounded lookback,
                // each rolling zero-usage reset estimate becomes a new live
                // tail record merely because its timestamp moved one minute.
                .map(|point| {
                    point.observed_at - chrono::Duration::minutes(WEEKLY_LIVE_TAIL_LOOKBACK_MINUTES)
                }),
        )
        .chain(match mode {
            LocalObservationMode::Reconcile { from, .. } => Some(from),
            LocalObservationMode::Incremental => None,
        })
        .min();
    let existing = match load_since {
        Some(since) => {
            Some(store.load_source_records_since(identity.node_id(), redaction_profile, since)?)
        }
        None => None,
    };
    let existing_buckets = existing
        .as_ref()
        .into_iter()
        .flat_map(|history| history.records.iter())
        .map(|record| (record.starts_at(), record))
        .collect::<BTreeMap<_, _>>();
    let mut buckets = Vec::new();
    for bucket in &observation.half_hour_buckets {
        // Validate every incoming fact before deciding whether it is a
        // semantic no-op. Otherwise an invalid lookback sample that happens
        // to match persisted payload fields could bypass the store boundary's
        // validation entirely.
        let record = SourceBucketRecord::upsert(revision, bucket.clone())?;
        let unchanged = existing_buckets
            .get(&bucket.starts_at)
            .and_then(|record| match record.change() {
                SourceBucketChange::Upsert(existing) => Some(existing.as_ref()),
                SourceBucketChange::Tombstone => None,
            })
            .is_some_and(|existing| bucket_record_is_unchanged(existing, bucket));
        if !unchanged {
            buckets.push(record);
        }
    }

    // Non-boundary weekly samples are live-tail checkpoints. When their
    // cumulative payload is unchanged, persisting a new wall-clock key every
    // minute creates an unbounded sequence which later multiplies Summary
    // aggregation work. Preserve exact 30-minute samples, but suppress an
    // equivalent non-boundary checkpoint for the same reset cycle.
    let mut known_weekly = existing
        .as_ref()
        .into_iter()
        .flat_map(|history| history.weekly_records.iter())
        .filter_map(|record| match record.change() {
            SourceWeeklyChange::Upsert(point) => Some((**point).clone()),
            SourceWeeklyChange::Tombstone => None,
        })
        .collect::<Vec<_>>();
    known_weekly.sort_by_key(|point| point.observed_at);
    let mut weekly = Vec::new();
    for point in &observation.weekly_local_points {
        // As with buckets, equivalence is an optimization after validation,
        // never an alternate acceptance path for malformed observations.
        let record = SourceWeeklyRecord::upsert(revision, point.clone())?;
        let same_key = known_weekly.iter().find(|existing| {
            existing.observed_at == point.observed_at && existing.resets_at == point.resets_at
        });
        if same_key.is_some_and(|existing| weekly_local_point_payload_eq(existing, point)) {
            continue;
        }
        let redundant_live_tail = matches!(mode, LocalObservationMode::Incremental)
            && !is_exact_weekly_boundary(point.observed_at)
            && known_weekly
                .iter()
                .rev()
                .find(|existing| existing.observed_at < point.observed_at)
                .is_some_and(|existing| weekly_live_tail_payload_eq(existing, point));
        if redundant_live_tail {
            continue;
        }
        weekly.push(record);
        if let Some(existing) = known_weekly.iter_mut().find(|existing| {
            existing.observed_at == point.observed_at && existing.resets_at == point.resets_at
        }) {
            *existing = point.clone();
        } else {
            known_weekly.push(point.clone());
            known_weekly.sort_by_key(|point| point.observed_at);
        }
    }
    let mut bucket_tombstones = 0;
    let mut weekly_tombstones = 0;
    if let LocalObservationMode::Reconcile { from, to } = mode {
        let incoming_buckets = observation
            .half_hour_buckets
            .iter()
            .map(|bucket| bucket.starts_at)
            .collect::<BTreeSet<_>>();
        for record in existing
            .as_ref()
            .into_iter()
            .flat_map(|history| history.records.iter())
        {
            // Bucket reconciliation is keyed by `starts_at`: only keys in the
            // declared half-open window are authoritative, even though the
            // shared read may include an earlier live-tail lookback.
            if record.starts_at() >= from
                && record.starts_at() < to
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
        for record in existing
            .as_ref()
            .into_iter()
            .flat_map(|history| history.weekly_records.iter())
        {
            let key = (record.observed_at(), record.resets_at());
            if record.observed_at() >= from
                && record.observed_at() < to
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

fn bucket_record_is_unchanged(
    existing: &LocalHalfHourBucket,
    incoming: &LocalHalfHourBucket,
) -> bool {
    if !half_hour_bucket_payload_eq(existing, incoming) {
        return false;
    }
    // A closed sample already carries the strongest timestamp for its bucket;
    // do not let a later lookback observation downgrade or rewrite it. While a
    // bucket is still open, however, a newer `sampled_at` is observable
    // freshness evidence (and the eventual exact end closes the bucket), so
    // only an older/equal open sample is a semantic no-op.
    existing.sampled_at >= existing.ends_at || incoming.sampled_at <= existing.sampled_at
}

fn weekly_live_tail_payload_eq(existing: &WeeklyLocalPoint, incoming: &WeeklyLocalPoint) -> bool {
    if weekly_local_point_payload_eq(existing, incoming) {
        return true;
    }
    // A completely unused server cycle has no stable anchor: the advertised
    // reset moves with every observation. Treat only those two zero-evidence
    // points as equivalent across reset timestamps. Once any usage appears,
    // the exact reset remains part of the persisted identity.
    weekly_point_has_no_usage(existing)
        && weekly_point_has_no_usage(incoming)
        && existing.token_usage == incoming.token_usage
        && existing.estimated_cost_units == incoming.estimated_cost_units
        && existing.api_long_context_extra_cost_units == incoming.api_long_context_extra_cost_units
        && existing.long_context_usage_unknown == incoming.long_context_usage_unknown
        && existing.estimator_revision == incoming.estimator_revision
        && existing.call_count == incoming.call_count
        && existing.partial_reasons == incoming.partial_reasons
}

fn weekly_point_has_no_usage(point: &WeeklyLocalPoint) -> bool {
    point.token_usage.is_zero() && point.estimated_cost_units == 0 && point.call_count == 0
}

fn is_exact_weekly_boundary(timestamp: DateTime<Utc>) -> bool {
    const WEEKLY_SAMPLE_SECONDS: i64 = 30 * 60;
    timestamp.timestamp().rem_euclid(WEEKLY_SAMPLE_SECONDS) == 0
        && timestamp.timestamp_subsec_nanos() == 0
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

    #[test]
    fn unchanged_lookback_does_not_rewrite_historical_shards() {
        with_writer(|identity, history, writer| {
            let old_start = at(29, 23, 45);
            let recent_start = at(30, 12, 0);
            let reset = at(30, 12, 0) + Duration::days(7);
            let mut live_weekly = weekly(at(30, 12, 1), 30);
            live_weekly.resets_at = reset;
            let first_observation = HistoryObservation {
                observed_at: at(30, 12, 1),
                half_hour_buckets: vec![bucket(old_start, 10), bucket(recent_start, 20)],
                weekly_local_points: vec![live_weekly.clone()],
                ..HistoryObservation::default()
            };
            let first = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &first_observation,
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(first.buckets.shards_written, 2);
            assert_eq!(first.weekly.shards_written, 1);

            let old_path = shard_path(
                &history.source_buckets_directory(identity.node_id(), RedactionProfile::Redacted),
                old_start.date_naive(),
            );
            let old_contents = fs::read(&old_path).unwrap();

            let unchanged_old = bucket(old_start, 10);
            let mut unchanged_recent = bucket(recent_start, 20);
            unchanged_recent.sampled_at = at(30, 12, 2);
            let mut next_live_weekly = live_weekly.clone();
            next_live_weekly.observed_at = at(30, 12, 2);
            let second = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: at(30, 12, 2),
                        half_hour_buckets: vec![unchanged_old.clone(), unchanged_recent],
                        weekly_local_points: vec![next_live_weekly.clone()],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(second.revision, first.revision + 1);
            assert_eq!(second.buckets.shards_written, 0);
            assert_eq!(second.weekly.shards_written, 0);
            assert_eq!(fs::read(&old_path).unwrap(), old_contents);

            let third = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: at(30, 12, 3),
                        half_hour_buckets: vec![unchanged_old, bucket(recent_start, 21)],
                        weekly_local_points: vec![next_live_weekly],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(third.buckets.shards_written, 1);
            assert_eq!(third.weekly.shards_written, 0);
            assert_eq!(fs::read(&old_path).unwrap(), old_contents);

            let stored = history
                .load_source_records_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    old_start,
                )
                .unwrap();
            assert_eq!(stored.records.len(), 2);
            assert_eq!(stored.weekly_records.len(), 1);
        });
    }

    #[test]
    fn open_bucket_closure_is_persisted_even_when_payload_is_unchanged() {
        with_writer(|identity, history, writer| {
            let starts_at = at(30, 12, 0);
            let mut open = bucket(starts_at, 20);
            open.sampled_at = starts_at + Duration::minutes(5);
            let first = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: open.sampled_at,
                        half_hour_buckets: vec![open],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(first.buckets.shards_written, 1);

            let mut later_open = bucket(starts_at, 20);
            later_open.sampled_at = starts_at + Duration::minutes(12);
            let later = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: later_open.sampled_at,
                        half_hour_buckets: vec![later_open],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(later.buckets.shards_written, 1);

            let closed = bucket(starts_at, 20);
            let second = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: closed.sampled_at,
                        half_hour_buckets: vec![closed.clone()],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(second.buckets.shards_written, 1);

            let stored = history
                .load_source_records_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    starts_at,
                )
                .unwrap();
            assert_eq!(stored.records.len(), 1);
            let SourceBucketChange::Upsert(stored) = stored.records[0].change() else {
                panic!("expected live bucket");
            };
            assert_eq!(stored.sampled_at, closed.ends_at);
        });
    }

    #[test]
    fn stateless_zero_usage_live_tail_does_not_grow_every_minute() {
        with_writer(|identity, history, writer| {
            let first_at = at(30, 12, 1);
            let mut first = weekly(first_at, 0);
            first.call_count = 0;
            let first_report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: first_at,
                        weekly_local_points: vec![first],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(first_report.weekly.shards_written, 1);

            let second_at = at(30, 12, 2);
            let mut second = weekly(second_at, 0);
            second.call_count = 0;
            let second_report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: second_at,
                        weekly_local_points: vec![second],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(second_report.weekly.shards_written, 0);

            let boundary_at = at(30, 12, 30);
            let mut boundary = weekly(boundary_at, 0);
            boundary.call_count = 0;
            let boundary_report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: boundary_at,
                        weekly_local_points: vec![boundary],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert_eq!(boundary_report.weekly.shards_written, 1);

            let stored = history
                .load_source_records_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    first_at - Duration::minutes(1),
                )
                .unwrap();
            assert_eq!(stored.weekly_records.len(), 2);
        });
    }

    #[test]
    fn reconcile_replaces_an_equivalent_live_tail_instead_of_removing_both() {
        with_writer(|identity, history, writer| {
            let first_at = at(30, 12, 1);
            let first = weekly(first_at, 30);
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: first_at,
                        weekly_local_points: vec![first.clone()],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();

            let second_at = at(30, 12, 2);
            let mut second = first;
            second.observed_at = second_at;
            let report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: second_at,
                        weekly_local_points: vec![second.clone()],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Reconcile {
                        from: at(30, 12, 0),
                        to: at(30, 12, 3),
                    },
                )
                .unwrap();
            assert_eq!(report.weekly.shards_written, 1);
            assert_eq!(report.weekly_tombstones, 1);

            let stored = history
                .load_source_records_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    at(30, 12, 0),
                )
                .unwrap();
            assert_eq!(stored.weekly_records.len(), 2);
            assert!(stored.weekly_records.iter().any(|record| {
                record.observed_at() == first_at
                    && matches!(record.change(), SourceWeeklyChange::Tombstone)
            }));
            assert!(stored.weekly_records.iter().any(|record| {
                record.observed_at() == second_at
                    && matches!(record.change(), SourceWeeklyChange::Upsert(_))
            }));
            let projected = history
                .load_source_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    at(30, 12, 0),
                )
                .unwrap();
            assert_eq!(projected.weekly_local_points, vec![second]);
        });
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
            let state_lock = lock_exclusive(state_lock, &lock_directory, STATE_LOCK).unwrap();

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
    fn interrupted_local_observation_retries_each_family_without_duplicate_records() {
        for (stage_index, stage) in [
            "revision",
            "account",
            "buckets",
            "weekly",
            "session_digests",
            "metadata",
        ]
        .into_iter()
        .enumerate()
        {
            with_writer(|identity, history, writer| {
                let starts_at = at(30, 12, 0);
                let observation = |total| HistoryObservation {
                    observed_at: starts_at + Duration::minutes(15),
                    quota_points: vec![QuotaPoint {
                        observed_at: starts_at,
                        limit_id: "codex".to_string(),
                        duration_mins: 10_080,
                        resets_at: starts_at + Duration::days(7),
                        used_percent: total as f64,
                        remaining_percent: 100.0 - total as f64,
                        provenance: crate::domain::Provenance::ServerSnapshot,
                    }],
                    half_hour_buckets: vec![bucket(starts_at, total)],
                    weekly_local_points: vec![weekly(starts_at, total)],
                };
                let write = |total| {
                    writer.record_local_observation_with_session_digests(
                        identity,
                        "local",
                        RedactionProfile::Redacted,
                        &observation(total),
                        LocalObservationMode::Incremental,
                        &[session_digest(
                            identity,
                            "thread-one",
                            starts_at,
                            'a',
                            total,
                        )],
                        true,
                    )
                };
                assert_eq!(write(10).unwrap().revision, 1);

                inject_local_observation_failure_after(stage);
                let error = write(20).unwrap_err();
                assert_eq!(error.to_string(), format!("injected failure after {stage}"));
                let reopened = SourceHistoryStore::new(
                    history.state_root().to_owned(),
                    history.profile_id().clone(),
                );
                assert_eq!(
                    reopened
                        .load_local_observation_revision(identity, RedactionProfile::Redacted,)
                        .unwrap(),
                    2
                );
                let snapshot = reopened.load_local_observation_snapshot_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    starts_at,
                    true,
                );
                if stage_index == 0 {
                    // Reserving a number alone has not changed any family.
                    assert_eq!(snapshot.unwrap().buckets[0].token_usage.total_tokens, 10);
                } else {
                    assert!(
                        is_local_observation_recovery_pending(&snapshot.unwrap_err()),
                        "{stage}"
                    );
                }
                // Account samples are an independent global series; they
                // remain readable even while the local combination is fenced.
                let account = reopened.load_account_since(starts_at).unwrap();
                assert_eq!(account.quota_points.len(), 1);
                assert_eq!(
                    account.quota_points[0].used_percent,
                    if stage_index == 0 { 10.0 } else { 20.0 }
                );

                let retry = write(20).unwrap();
                assert_eq!(retry.revision, 3);
                assert_eq!(retry.recovered_pending, stage_index > 0);
                assert_eq!(retry.buckets.shards_written, usize::from(stage_index < 2));
                assert_eq!(retry.weekly.shards_written, usize::from(stage_index < 3));
                assert_eq!(
                    retry.session_digests.shards_written,
                    usize::from(stage_index < 4)
                );
                let snapshot = reopened
                    .load_local_observation_snapshot_since(
                        identity.node_id(),
                        RedactionProfile::Redacted,
                        starts_at,
                        true,
                    )
                    .unwrap();
                assert_eq!(snapshot.buckets.len(), 1);
                assert_eq!(snapshot.buckets[0].token_usage.total_tokens, 20);
                assert_eq!(snapshot.weekly_local_points.len(), 1);
                assert_eq!(snapshot.weekly_local_points[0].token_usage.total_tokens, 20);
                assert_eq!(snapshot.session_digest_records.len(), 1);
                assert_eq!(
                    snapshot.session_digest_records[0].revision(),
                    if stage_index == 0 { 3 } else { 2 }
                );
                let SourceSessionDigestChange::Upsert(digest) =
                    snapshot.session_digest_records[0].change()
                else {
                    panic!("retry must preserve the digest upsert");
                };
                assert_eq!(digest.metrics().token_usage.total_tokens, 20);
                let replay = write(20).unwrap();
                assert_eq!(replay.revision, 4);
                assert!(!replay.recovered_pending);
                assert_eq!(replay.buckets.shards_written, 0);
                assert_eq!(replay.weekly.shards_written, 0);
                assert_eq!(replay.session_digests.shards_written, 0);
            });
        }
    }

    #[test]
    fn pending_observation_recovers_before_empty_or_partial_new_input() {
        for add_partial_bucket in [false, true] {
            with_writer(|identity, history, writer| {
                let starts_at = at(30, 12, 0);
                let observation = HistoryObservation {
                    observed_at: starts_at + Duration::minutes(15),
                    half_hour_buckets: vec![bucket(starts_at, 20)],
                    weekly_local_points: vec![weekly(starts_at, 20)],
                    ..HistoryObservation::default()
                };
                inject_local_observation_failure_after("buckets");
                assert!(
                    writer
                        .record_local_observation_with_session_digests(
                            identity,
                            "local",
                            RedactionProfile::Redacted,
                            &observation,
                            LocalObservationMode::Incremental,
                            &[session_digest(identity, "thread-one", starts_at, 'a', 20)],
                            true,
                        )
                        .is_err()
                );
                let next = HistoryObservation {
                    observed_at: starts_at + Duration::hours(1),
                    half_hour_buckets: if add_partial_bucket {
                        vec![bucket(starts_at + Duration::minutes(30), 5)]
                    } else {
                        Vec::new()
                    },
                    ..HistoryObservation::default()
                };
                let write_next = || {
                    writer.record_local_observation(
                        identity,
                        "local",
                        RedactionProfile::Redacted,
                        &next,
                        LocalObservationMode::Incremental,
                    )
                };
                inject_local_observation_failure_after("recovery_buckets");
                assert!(write_next().is_err());
                assert!(is_local_observation_recovery_pending(
                    &history
                        .load_local_observation_snapshot_since(
                            identity.node_id(),
                            RedactionProfile::Redacted,
                            starts_at,
                            true,
                        )
                        .unwrap_err()
                ));
                assert_eq!(
                    history
                        .load_local_observation_revision(identity, RedactionProfile::Redacted)
                        .unwrap(),
                    1
                );

                let report = write_next().unwrap();
                assert_eq!(report.revision, 2);
                assert!(report.recovered_pending);
                let snapshot = history
                    .load_local_observation_snapshot_since(
                        identity.node_id(),
                        RedactionProfile::Redacted,
                        starts_at,
                        true,
                    )
                    .unwrap();
                assert_eq!(snapshot.buckets.len(), 1 + usize::from(add_partial_bucket));
                assert_eq!(snapshot.buckets[0].token_usage.total_tokens, 20);
                assert_eq!(snapshot.weekly_local_points[0].token_usage.total_tokens, 20);
                assert_eq!(snapshot.session_digest_records[0].revision(), 1);
                let SourceSessionDigestChange::Upsert(digest) =
                    snapshot.session_digest_records[0].change()
                else {
                    panic!("the original pending digest must be recovered");
                };
                assert_eq!(digest.metrics().token_usage.total_tokens, 20);
            });
        }
    }

    #[test]
    fn malformed_pending_observation_never_replays_an_earlier_family() {
        for corruption in [
            "version",
            "profile",
            "source",
            "generation",
            "redaction",
            "revision",
            "digest_source",
            "digest_key",
            "oversized",
        ] {
            with_writer(|identity, history, writer| {
                let starts_at = at(30, 12, 0);
                let write = |total| {
                    writer.record_local_observation_with_session_digests(
                        identity,
                        "local",
                        RedactionProfile::Redacted,
                        &HistoryObservation {
                            observed_at: starts_at + Duration::minutes(15),
                            quota_points: vec![QuotaPoint {
                                observed_at: starts_at,
                                limit_id: "codex".to_string(),
                                duration_mins: 10_080,
                                resets_at: starts_at + Duration::days(7),
                                used_percent: total as f64,
                                remaining_percent: 100.0 - total as f64,
                                provenance: crate::domain::Provenance::ServerSnapshot,
                            }],
                            half_hour_buckets: vec![bucket(starts_at, total)],
                            weekly_local_points: vec![weekly(starts_at, total)],
                        },
                        LocalObservationMode::Incremental,
                        &[session_digest(
                            identity,
                            "thread-one",
                            starts_at,
                            'a',
                            total,
                        )],
                        true,
                    )
                };
                write(10).unwrap();
                inject_local_observation_failure_after("account");
                assert!(write(20).is_err());
                let read_families = || {
                    (
                        history.load_account_since(starts_at).unwrap(),
                        history
                            .load_source_records_since(
                                identity.node_id(),
                                RedactionProfile::Redacted,
                                starts_at,
                            )
                            .unwrap(),
                        history
                            .load_source_session_digest_records_since(
                                identity.node_id(),
                                RedactionProfile::Redacted,
                                starts_at,
                            )
                            .unwrap(),
                    )
                };
                let before = read_families();
                let path =
                    local_state_directory(history, identity.node_id(), RedactionProfile::Redacted)
                        .join(JOURNAL_FILE);
                let mut pending =
                    read_optional_json_file::<PendingLocalObservation>(&path, MAX_JOURNAL_BYTES)
                        .unwrap()
                        .unwrap();
                // A wrongly ordered replay would mutate this global account
                // point before noticing the invalid final-family record.
                pending.account_points[0].used_percent = 90.0;
                pending.account_points[0].remaining_percent = 10.0;
                match corruption {
                    "version" => pending.binding.format_version += 1,
                    "profile" => pending.binding.profile_id = "fedcba9876543210".parse().unwrap(),
                    "source" => {
                        pending.binding.source_id =
                            "node-fedcba9876543210fedcba9876543210".parse().unwrap()
                    }
                    "generation" => pending.binding.source_generation += 1,
                    "redaction" => {
                        pending.binding.redaction_profile = RedactionProfile::PreviewEnabled
                    }
                    "revision" => pending.binding.last_reserved_revision += 1,
                    "digest_source" => {
                        let other = SourceIdentity::from_test_parts(
                            "node-fedcba9876543210fedcba9876543210".parse().unwrap(),
                            SECRET,
                        );
                        pending.session_digest_records[0] = SourceSessionDigestRecord::upsert(
                            2,
                            session_digest(&other, "thread-one", starts_at, 'a', 20),
                        )
                        .unwrap();
                    }
                    "digest_key" => pending.session_digest_records.push(
                        SourceSessionDigestRecord::tombstone(
                            "thread-one".parse().unwrap(),
                            starts_at,
                            starts_at + Duration::days(2),
                            starts_at,
                            2,
                        )
                        .unwrap(),
                    ),
                    "oversized" => {}
                    _ => unreachable!(),
                }
                write_private_atomically(
                    &path,
                    &encode_pretty_bounded(&pending, MAX_JOURNAL_BYTES).unwrap(),
                )
                .unwrap();
                if corruption == "oversized" {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len(MAX_JOURNAL_BYTES + 1)
                        .unwrap();
                }
                assert!(write(20).is_err(), "{corruption}");
                assert_eq!(
                    read_families(),
                    before,
                    "{corruption} must be rejected before replaying any family"
                );
                assert!(path.exists());
            });
        }
    }

    #[test]
    fn interrupted_reconcile_replays_tombstones_before_new_incremental_input() {
        with_writer(|identity, history, writer| {
            let starts_at = at(30, 12, 0);
            writer
                .record_local_observation_with_session_digests(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: starts_at + Duration::minutes(15),
                        half_hour_buckets: vec![bucket(starts_at, 20)],
                        weekly_local_points: vec![weekly(starts_at, 20)],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                    &[session_digest(identity, "thread-one", starts_at, 'a', 20)],
                    true,
                )
                .unwrap();
            let empty = HistoryObservation {
                observed_at: starts_at + Duration::days(1),
                ..HistoryObservation::default()
            };
            inject_local_observation_failure_after("buckets");
            assert!(
                writer
                    .record_local_observation_with_session_digests(
                        identity,
                        "local",
                        RedactionProfile::Redacted,
                        &empty,
                        LocalObservationMode::Reconcile {
                            from: starts_at,
                            to: empty.observed_at
                        },
                        &[],
                        true,
                    )
                    .is_err()
            );
            let report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &empty,
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert!(report.recovered_pending);
            assert_eq!(report.revision, 3);
            assert_eq!(report.bucket_tombstones, 1);
            assert_eq!(report.weekly_tombstones, 1);
            assert_eq!(report.session_digest_tombstones, 1);
            let snapshot = history
                .load_local_observation_snapshot_since(
                    identity.node_id(),
                    RedactionProfile::Redacted,
                    starts_at,
                    true,
                )
                .unwrap();
            assert!(snapshot.buckets.is_empty());
            assert!(snapshot.weekly_local_points.is_empty());
            assert_eq!(snapshot.session_digest_records.len(), 1);
            assert_eq!(snapshot.session_digest_records[0].revision(), 2);
            assert!(matches!(
                snapshot.session_digest_records[0].change(),
                SourceSessionDigestChange::Tombstone
            ));
        });
    }

    #[test]
    fn combined_local_snapshot_locks_before_the_first_profile_observation() {
        with_writer(|identity, history, _writer| {
            prepare_local_metadata(history, identity, "local", RedactionProfile::Redacted).unwrap();
            let starts_at = at(30, 12, 0);
            let lock_directory = history.source_directory(identity.node_id());
            let state_lock = open_lock_file(&lock_directory, STATE_LOCK).unwrap();
            let state_lock = lock_exclusive(state_lock, &lock_directory, STATE_LOCK).unwrap();

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
                let state_lock = lock_exclusive(state_lock, &lock_directory, STATE_LOCK).unwrap();
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
            inject_local_observation_failure_after("account");
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
            let journal_path =
                local_state_directory(history, identity.node_id(), RedactionProfile::Redacted)
                    .join(JOURNAL_FILE);
            let journal = fs::read_to_string(&journal_path).unwrap();
            assert!(journal.contains("[redacted]"));
            assert!(!journal.contains("private customer title"));
            assert!(!journal.contains("rotate the private credential"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&journal_path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &observation,
                    LocalObservationMode::Incremental,
                )
                .unwrap();
            assert!(!journal_path.exists());

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
    fn reconcile_tombstones_only_keys_inside_declared_window() {
        with_writer(|identity, history, writer| {
            let before = at(30, 9, 45);
            let from = at(30, 10, 0);
            let to = at(30, 10, 15);
            let retained_weekly_at = at(30, 10, 1);
            let missing_weekly_at = at(30, 10, 5);
            let before_weekly_at = at(30, 9, 50);
            let retained_weekly = weekly(retained_weekly_at, 30);
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: to,
                        half_hour_buckets: vec![bucket(before, 10), bucket(from, 20)],
                        weekly_local_points: vec![
                            weekly(before_weekly_at, 10),
                            retained_weekly.clone(),
                            weekly(missing_weekly_at, 40),
                        ],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();

            // The retained weekly point makes the shared read start 30 minutes
            // before `from`. Records found there are comparison context only;
            // the reconcile authority is still strictly keyed by [from, to).
            let report = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: to,
                        weekly_local_points: vec![retained_weekly],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Reconcile { from, to },
                )
                .unwrap();
            assert_eq!(report.bucket_tombstones, 1);
            assert_eq!(report.weekly_tombstones, 1);

            let projected = history
                .load_source_since(identity.node_id(), RedactionProfile::Redacted, before)
                .unwrap();
            assert_eq!(
                projected
                    .buckets
                    .iter()
                    .map(|bucket| bucket.starts_at)
                    .collect::<Vec<_>>(),
                vec![before]
            );
            assert_eq!(
                projected
                    .weekly_local_points
                    .iter()
                    .map(|point| point.observed_at)
                    .collect::<Vec<_>>(),
                vec![before_weekly_at, retained_weekly_at]
            );
        });
    }

    #[test]
    fn invalid_equivalent_bucket_is_rejected_before_no_op_filtering() {
        with_writer(|identity, _history, writer| {
            let starts_at = at(30, 10, 0);
            let persisted = bucket(starts_at, 10);
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: persisted.sampled_at,
                        half_hour_buckets: vec![persisted.clone()],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();

            let mut invalid = persisted;
            invalid.sampled_at = invalid.ends_at + Duration::minutes(1);
            let error = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: invalid.sampled_at,
                        half_hour_buckets: vec![invalid],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn invalid_zero_weekly_tail_is_rejected_before_equivalence_filtering() {
        with_writer(|identity, _history, writer| {
            let first_at = at(30, 10, 1);
            let mut first = weekly(first_at, 0);
            first.call_count = 0;
            writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: first_at,
                        weekly_local_points: vec![first],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap();

            let second_at = at(30, 10, 2);
            let mut invalid = weekly(second_at, 0);
            invalid.call_count = 0;
            invalid.resets_at = invalid.observed_at;
            let error = writer
                .record_local_observation(
                    identity,
                    "local",
                    RedactionProfile::Redacted,
                    &HistoryObservation {
                        observed_at: second_at,
                        weekly_local_points: vec![invalid],
                        ..HistoryObservation::default()
                    },
                    LocalObservationMode::Incremental,
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
