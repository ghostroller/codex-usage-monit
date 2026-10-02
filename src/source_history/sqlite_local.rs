//! SQLite publication of one local observation and its durable revision.
//!
//! The source coordination lock spans the independent revision reservation
//! and the data transaction. No per-family files or application redo batches
//! are published by this path.

use super::*;
use crate::source_history::database::{HistoryDatabase, set_state};

#[cfg(test)]
thread_local! {
    static INSPECT_RESERVED_REVISION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static REVISION_READER_READY: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn inspect_next_reservation(inspector: impl FnOnce() + 'static) {
    INSPECT_RESERVED_REVISION.with(|slot| *slot.borrow_mut() = Some(Box::new(inspector)));
}

#[cfg(test)]
pub(super) fn observe_next_revision_read(ready: std::sync::mpsc::Sender<()>) {
    REVISION_READER_READY.with(|slot| *slot.borrow_mut() = Some(ready));
}

fn database(store: &SourceHistoryStore) -> io::Result<HistoryDatabase> {
    store
        .sqlite_database()
        .ok_or_else(|| invalid_data("local SQLite operation requires a SQLite history store"))
}

fn revision_key(
    database: &HistoryDatabase,
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<String> {
    database.namespace(&local_state_directory(store, source, redaction).join(STATE_FILE))
}

fn committed_key(
    database: &HistoryDatabase,
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<String> {
    database.namespace(&local_state_directory(store, source, redaction).join(COMMITTED_STATE_FILE))
}

fn marker_key(
    database: &HistoryDatabase,
    store: &SourceHistoryStore,
    redaction: RedactionProfile,
) -> io::Result<String> {
    database.namespace(
        &store
            .profile_directory()
            .join(redaction.directory_name())
            .join(MARKER_FILE),
    )
}

fn read_revision(
    connection: &rusqlite::Connection,
    key: &str,
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    redaction: RedactionProfile,
) -> io::Result<u64> {
    let Some(current) =
        sqlite_state_bounded::<LocalRevisionState>(connection, key, MAX_STATE_BYTES, None)?
    else {
        return Ok(0);
    };
    if current.format_version != STATE_VERSION
        || current.profile_id != *store.profile_id()
        || current.source_id != *identity.node_id()
        || current.source_generation != identity.generation()
        || current.redaction_profile != redaction
    {
        return Err(invalid_data(
            "local observation revision state binding mismatch",
        ));
    }
    Ok(current.last_reserved_revision)
}

fn write_revision(
    connection: &rusqlite::Connection,
    key: &str,
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    redaction: RedactionProfile,
    revision: u64,
) -> io::Result<()> {
    set_state(
        connection,
        key,
        &LocalRevisionState {
            format_version: STATE_VERSION,
            profile_id: store.profile_id().clone(),
            source_id: identity.node_id().clone(),
            source_generation: identity.generation(),
            redaction_profile: redaction,
            last_reserved_revision: revision,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_observation(
    writer: &SourceHistoryWriter<'_, '_, '_>,
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    display_label: &str,
    redaction: RedactionProfile,
    observation: &HistoryObservation,
    mode: LocalObservationMode,
    digests: &[SourceSessionDigest],
    digest_scan_complete: bool,
) -> io::Result<LocalObservationWriteReport> {
    let database = database(store)?;
    if database.is_transaction_active() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local observation revision must be reserved outside a history transaction",
        ));
    }
    // Two independent durable commitments need source-level coordination even
    // when callers share the same ownership authority across worker threads.
    let lock_directory = store.source_directory(identity.node_id());
    store.prepare_private_directory(&lock_directory)?;
    let lock = open_lock_file(&lock_directory, STATE_LOCK)?;
    let _lock = lock_exclusive(lock, &lock_directory, STATE_LOCK)?;
    let key = revision_key(&database, store, identity.node_id(), redaction)?;
    let published_key = committed_key(&database, store, identity.node_id(), redaction)?;
    let revision = database.write(|connection| {
        let current = read_revision(connection, &key, store, identity, redaction)?;
        if read_revision(connection, &published_key, store, identity, redaction)? > current {
            return Err(invalid_data(
                "local observation committed revision exceeds its high-water",
            ));
        }
        let revision = current
            .checked_add(1)
            .ok_or_else(|| invalid_data("local observation revision exhausted"))?;
        writer.validate()?;
        write_revision(connection, &key, store, identity, redaction, revision)?;
        Ok(revision)
    })?;
    #[cfg(test)]
    fail_after_local_observation_stage("revision")?;
    #[cfg(test)]
    INSPECT_RESERVED_REVISION.with(|slot| {
        if let Some(inspector) = slot.borrow_mut().take() {
            inspector();
        }
    });

    // Collection parsing and project normalization already ran in the runtime.
    // Resolve bounded existing records and prepare semantic differences before
    // taking the data writer, while the source lock preserves their ordering.
    let (buckets, weekly, bucket_tombstones, weekly_tombstones, digests, digest_tombstones) =
        database.read(|_| {
            let source_exists = match store.load_source_metadata(identity.node_id()) {
                Ok(source) => {
                    if source.kind() != SourceKind::Local || source.display_label() != display_label {
                        return Err(invalid_data(
                            "local observation source metadata does not match identity, kind, and label",
                        ));
                    }
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            let (buckets, weekly, bucket_tombstones, weekly_tombstones) = build_records(
                store,
                identity,
                redaction,
                observation,
                mode,
                revision,
                source_exists,
            )?;
            let (digests, digest_tombstones) = build_session_digest_records(
                store,
                identity,
                redaction,
                observation,
                mode,
                revision,
                digests,
                digest_scan_complete,
                source_exists,
            )?;
            Ok((buckets, weekly, bucket_tombstones, weekly_tombstones, digests, digest_tombstones))
        })?;

    database.write(|connection| {
        if read_revision(connection, &key, store, identity, redaction)? != revision {
            return Err(invalid_data(
                "local observation revision changed before publication",
            ));
        }
        if read_revision(connection, &published_key, store, identity, redaction)? >= revision {
            return Err(invalid_data(
                "local observation committed revision is not older than its publication",
            ));
        }
        prepare_local_metadata(store, identity, display_label, redaction)?;
        let account = store.record_account_points_unfenced(&observation.quota_points)?;
        #[cfg(test)]
        fail_after_local_observation_stage("account")?;
        let bucket_history =
            store.record_source_bucket_changes_unfenced(identity.node_id(), redaction, &buckets)?;
        #[cfg(test)]
        fail_after_local_observation_stage("buckets")?;
        let weekly_history =
            store.record_source_weekly_changes_unfenced(identity.node_id(), redaction, &weekly)?;
        #[cfg(test)]
        fail_after_local_observation_stage("weekly")?;
        let digest_history = store.record_source_session_digest_changes_unfenced(
            identity.node_id(),
            redaction,
            &digests,
        )?;
        #[cfg(test)]
        fail_after_local_observation_stage("session_digests")?;
        writer.validate()?;
        publish_local_metadata(store, identity, display_label, redaction)?;
        #[cfg(test)]
        fail_after_local_observation_stage("metadata")?;
        write_revision(
            connection,
            &published_key,
            store,
            identity,
            redaction,
            revision,
        )?;
        // Recheck authority inside the transaction, immediately before its
        // commit, rather than discovering a stale writer after publication.
        writer.validate()?;
        Ok(LocalObservationWriteReport {
            revision,
            account,
            buckets: bucket_history,
            weekly: weekly_history,
            session_digests: digest_history,
            account_records: observation.quota_points.len(),
            bucket_records: buckets.len(),
            weekly_records: weekly.len(),
            session_digest_records: digests.len(),
            bucket_tombstones,
            weekly_tombstones,
            session_digest_tombstones: digest_tombstones,
            garbage_collection: LocalObservationGarbageCollectionReport::default(),
        })
    })
}

pub(super) fn raise_revision_floor(
    writer: &SourceHistoryWriter<'_, '_, '_>,
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    redaction: RedactionProfile,
    floor: u64,
) -> io::Result<u64> {
    let database = database(store)?;
    let lock_directory = store.source_directory(identity.node_id());
    store.prepare_private_directory(&lock_directory)?;
    let lock = open_lock_file(&lock_directory, STATE_LOCK)?;
    let _lock = lock_exclusive(lock, &lock_directory, STATE_LOCK)?;
    let key = revision_key(&database, store, identity.node_id(), redaction)?;
    let published_key = committed_key(&database, store, identity.node_id(), redaction)?;
    database.write(|connection| {
        let current = read_revision(connection, &key, store, identity, redaction)?;
        if read_revision(connection, &published_key, store, identity, redaction)? > current {
            return Err(invalid_data(
                "local observation committed revision exceeds its high-water",
            ));
        }
        let next = current.max(floor);
        if next != current {
            write_revision(connection, &key, store, identity, redaction, next)?;
        }
        writer.validate()?;
        Ok(next)
    })
}

pub(super) fn load_revision(
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    redaction: RedactionProfile,
) -> io::Result<u64> {
    let database = database(store)?;
    if !database.exists()? {
        return Ok(0);
    }
    let directory = store.source_directory(identity.node_id());
    if !store.private_directory_exists(&directory)? {
        let key = revision_key(&database, store, identity.node_id(), redaction)?;
        return database
            .read(|connection| read_revision(connection, &key, store, identity, redaction));
    }
    #[cfg(test)]
    REVISION_READER_READY.with(|slot| {
        if let Some(ready) = slot.borrow_mut().take() {
            ready.send(()).unwrap();
        }
    });
    let lock = open_lock_file(&directory, STATE_LOCK)?;
    let _lock = lock_shared(lock, &directory, STATE_LOCK)?;
    let key = revision_key(&database, store, identity.node_id(), redaction)?;
    database.read(|connection| read_revision(connection, &key, store, identity, redaction))
}

pub(super) fn load_projection_revision(
    store: &SourceHistoryStore,
    identity: &SourceIdentity,
    redaction: RedactionProfile,
) -> io::Result<u64> {
    let database = database(store)?;
    if !database.exists()? {
        return Ok(0);
    }
    let key = committed_key(&database, store, identity.node_id(), redaction)?;
    let high_water_key = revision_key(&database, store, identity.node_id(), redaction)?;
    database.read(|connection| {
        let high_water = read_revision(connection, &high_water_key, store, identity, redaction)?;
        let published = read_revision(connection, &key, store, identity, redaction)?;
        if published > high_water {
            return Err(invalid_data(
                "local observation committed revision exceeds its high-water",
            ));
        }
        Ok(published)
    })
}

pub(super) fn load_snapshot(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    since: DateTime<Utc>,
    include_digests: bool,
    budget: &mut SourceHistoryReadBudget,
) -> io::Result<LocalObservationSnapshot> {
    database(store)?.read(|_| {
        budget.charge_source()?;
        store.load_local_observation_snapshot_unlocked(
            source,
            redaction,
            since,
            include_digests,
            budget,
        )
    })
}

pub(super) fn mark_backfill(
    writer: &SourceHistoryWriter<'_, '_, '_>,
    store: &SourceHistoryStore,
    completed_at: DateTime<Utc>,
    complete: bool,
) -> io::Result<V2SummaryBackfillAttempt> {
    let database = database(store)?;
    let redaction = writer.redaction_profile();
    let epoch = writer.authority.expected_manifest().epoch();
    let key = marker_key(&database, store, redaction)?;
    database.write(|connection| {
        let current =
            sqlite_state_bounded::<BackfillMarker>(connection, &key, MAX_STATE_BYTES, None)?;
        if let Some(marker) = current.as_ref() {
            validate_marker(store, redaction, epoch, marker)?;
        }
        let requested = BackfillMarker {
            format_version: STATE_VERSION,
            profile_id: store.profile_id().clone(),
            redaction_profile: redaction,
            ownership_epoch: epoch,
            completed_at,
            complete,
        };
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
        set_state(connection, &key, &marker)?;
        writer.validate()?;
        Ok(V2SummaryBackfillAttempt {
            completed_at: marker.completed_at,
            complete: marker.complete,
        })
    })
}

pub(super) fn load_backfill(
    store: &SourceHistoryStore,
    redaction: RedactionProfile,
    epoch: u64,
) -> io::Result<Option<V2SummaryBackfillAttempt>> {
    let database = database(store)?;
    if !database.exists()? {
        return Ok(None);
    }
    let key = marker_key(&database, store, redaction)?;
    database.read(|connection| {
        let Some(marker) =
            sqlite_state_bounded::<BackfillMarker>(connection, &key, MAX_STATE_BYTES, None)?
        else {
            return Ok(None);
        };
        validate_marker(store, redaction, epoch, &marker)?;
        Ok(Some(V2SummaryBackfillAttempt {
            completed_at: marker.completed_at,
            complete: marker.complete,
        }))
    })
}
