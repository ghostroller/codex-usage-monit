//! Fenced activation of SQLite after the legacy v2 namespace is complete.
//! Old v2 files remain an immutable rollback input after version-2 ownership
//! has refused cooperating old binaries in every existing v2 namespace.

use std::io;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::history_ownership::{
    HistoryOwnershipManifest, HistoryOwnershipState, HistoryOwnershipStore, HistoryWriterLease,
    OwnershipCasOutcome, OwnershipManifestStatus, TryWriterLease,
};
use crate::source_history::database::{self, HistoryDatabase};
use crate::source_history::{HistoryProfileId, RedactionProfile, SourceHistoryStore};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SqliteMigrationReceipt {
    format_version: u32,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    ownership_epoch: u64,
    record_count: u64,
    record_bytes: u64,
    records_sha256: String,
    sqlite_version: String,
    sqlite_source_id: String,
    completed_at: DateTime<Utc>,
}

pub(crate) fn activate_sqlite_history(
    ownership: &HistoryOwnershipStore,
    lease: &HistoryWriterLease,
    expected: &HistoryOwnershipManifest,
    legacy: &SourceHistoryStore,
) -> io::Result<(HistoryOwnershipManifest, SourceHistoryStore)> {
    activate_sqlite_history_with_hook(ownership, lease, expected, legacy, || Ok(()))
}

fn activate_sqlite_history_with_hook(
    ownership: &HistoryOwnershipStore,
    lease: &HistoryWriterLease,
    expected: &HistoryOwnershipManifest,
    legacy: &SourceHistoryStore,
    before_activation: impl FnOnce() -> io::Result<()>,
) -> io::Result<(HistoryOwnershipManifest, SourceHistoryStore)> {
    ownership.validate_writer_lease(lease)?;
    let target =
        SourceHistoryStore::new_sqlite(legacy.state_root().to_owned(), legacy.profile_id().clone());
    let db = target
        .sqlite_database()
        .ok_or_else(|| io::Error::other("SQLite target has no database"))?;
    if expected.is_sqlite_backend() && expected.state() == HistoryOwnershipState::V2Active {
        validate_receipt(&db, expected)?;
        return Ok((expected.clone(), target));
    }
    if !(expected.state() == HistoryOwnershipState::V2Active
        || (expected.is_sqlite_backend() && expected.state() == HistoryOwnershipState::Migrating))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SQLite cutover requires complete source-aware history",
        ));
    }

    // The profile database contains both privacy namespaces. Fence every
    // already-v2 namespace before its contents become authoritative in SQL.
    // Use a nonblocking acquisition for the other namespace: opposite lock
    // acquisition orders may contend, but can never deadlock.
    let other_redaction = match ownership.redaction_profile() {
        RedactionProfile::Redacted => RedactionProfile::PreviewEnabled,
        RedactionProfile::PreviewEnabled => RedactionProfile::Redacted,
    };
    let other_store = HistoryOwnershipStore::new(
        legacy.state_root().to_owned(),
        legacy.profile_id().clone(),
        other_redaction,
    );
    let other_manifest = match other_store.load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest) if manifest.uses_source_history() => {
            Some(manifest)
        }
        _ => None,
    };
    let other_lease = if other_manifest.is_some() {
        match other_store.try_acquire_writer_lease()? {
            TryWriterLease::Acquired(lease) => Some(lease),
            TryWriterLease::Busy(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "SQLite cutover is waiting for the other privacy namespace writer",
                ));
            }
        }
    } else {
        None
    };

    let current = begin_if_needed(ownership, lease, expected)?;
    let other = match (&other_manifest, &other_lease) {
        (Some(manifest), Some(lease)) => Some(begin_if_needed(&other_store, lease, manifest)?),
        _ => None,
    };
    let mut participants = vec![current.clone()];
    if let Some(other) = &other {
        participants.push(other.clone());
    }
    let epochs = participants
        .iter()
        .map(|manifest| {
            let old = manifest
                .epoch()
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("invalid SQLite cutover epoch"))?;
            Ok((manifest.redaction_profile(), old, manifest.epoch()))
        })
        .collect::<io::Result<Vec<_>>>()?;
    // A crash may leave the securely published empty database (including its
    // creation link) before schema initialization. Both participant leases
    // are held here. A writable open can finish that pending initialization;
    // the database's active-owner guard still forbids recreating lost data.
    let receipts_complete = db.write(|connection| {
        let mut complete = true;
        for manifest in &participants {
            let receipt = database::state::<SqliteMigrationReceipt>(
                connection,
                &receipt_key(manifest.redaction_profile()),
            )?;
            let matches = receipt
                .as_ref()
                .is_some_and(|receipt| receipt_matches(receipt, manifest));
            if manifest.state() == HistoryOwnershipState::V2Active && !matches {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active privacy namespace has no matching SQLite migration receipt",
                ));
            }
            complete &= matches;
        }
        Ok(complete)
    })?;

    if !receipts_complete {
        db.write(|connection| {
            target.import_sqlite_history_core_and_facts(legacy)?;
            target.import_legacy_remote_sqlite_state(legacy)?;
            target.import_legacy_local_sqlite_state(legacy, &epochs)?;
            target.import_legacy_redaction_retirement_sqlite_state(legacy)?;
            target.import_legacy_source_purge_sqlite_state(legacy)?;
            validate_participants(
                ownership,
                lease,
                &current,
                &other_store,
                other_lease.as_ref(),
                other.as_ref(),
            )?;
            let (record_count, record_bytes, records_sha256) = record_identity(connection)?;
            let sqlite_version: String = connection
                .query_row("SELECT sqlite_version()", [], |row| row.get(0))
                .map_err(database::sql_error)?;
            let sqlite_source_id: String = connection
                .query_row("SELECT sqlite_source_id()", [], |row| row.get(0))
                .map_err(database::sql_error)?;
            for manifest in &participants {
                let receipt = SqliteMigrationReceipt {
                    format_version: 1,
                    profile_id: legacy.profile_id().clone(),
                    redaction_profile: manifest.redaction_profile(),
                    ownership_epoch: manifest.epoch(),
                    record_count,
                    record_bytes,
                    records_sha256: records_sha256.clone(),
                    sqlite_version: sqlite_version.clone(),
                    sqlite_source_id: sqlite_source_id.clone(),
                    completed_at: Utc::now(),
                };
                database::set_state(
                    connection,
                    &receipt_key(manifest.redaction_profile()),
                    &receipt,
                )?;
            }
            validate_participants(
                ownership,
                lease,
                &current,
                &other_store,
                other_lease.as_ref(),
                other.as_ref(),
            )
        })?;
    }
    before_activation()?;
    validate_participants(
        ownership,
        lease,
        &current,
        &other_store,
        other_lease.as_ref(),
        other.as_ref(),
    )?;
    // Both manifests already refuse old binaries. A crash between these
    // activations resumes from the committed receipts without a new import.
    if let (Some(manifest), Some(lease)) = (&other, &other_lease) {
        complete_if_needed(&other_store, lease, manifest)?;
    }
    let active = complete_if_needed(ownership, lease, &current)?;
    validate_receipt(&db, &active)?;
    Ok((active, target))
}

fn begin_if_needed(
    store: &HistoryOwnershipStore,
    lease: &HistoryWriterLease,
    manifest: &HistoryOwnershipManifest,
) -> io::Result<HistoryOwnershipManifest> {
    if manifest.is_sqlite_backend() {
        return Ok(manifest.clone());
    }
    match store.begin_sqlite_migration(lease, manifest)? {
        OwnershipCasOutcome::Applied(manifest) => Ok(manifest),
        OwnershipCasOutcome::Conflict(_) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "ownership changed before SQLite import",
        )),
    }
}

fn complete_if_needed(
    store: &HistoryOwnershipStore,
    lease: &HistoryWriterLease,
    manifest: &HistoryOwnershipManifest,
) -> io::Result<HistoryOwnershipManifest> {
    if manifest.state() == HistoryOwnershipState::V2Active {
        return Ok(manifest.clone());
    }
    match store.complete_sqlite_migration(lease, manifest)? {
        OwnershipCasOutcome::Applied(manifest) => Ok(manifest),
        OwnershipCasOutcome::Conflict(_) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "ownership changed before SQLite activation",
        )),
    }
}

fn validate_participants(
    current_store: &HistoryOwnershipStore,
    current_lease: &HistoryWriterLease,
    current: &HistoryOwnershipManifest,
    other_store: &HistoryOwnershipStore,
    other_lease: Option<&HistoryWriterLease>,
    other: Option<&HistoryOwnershipManifest>,
) -> io::Result<()> {
    current_store.validate_writer_lease(current_lease)?;
    if current_store.load_manifest()? != OwnershipManifestStatus::Initialized(current.clone()) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "ownership changed during SQLite import",
        ));
    }
    if let (Some(lease), Some(manifest)) = (other_lease, other) {
        other_store.validate_writer_lease(lease)?;
        if other_store.load_manifest()? != OwnershipManifestStatus::Initialized(manifest.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "privacy namespace changed during SQLite import",
            ));
        }
    }
    Ok(())
}

fn receipt_key(redaction: RedactionProfile) -> String {
    format!("database/migration/{}", redaction.directory_name())
}

fn receipt_matches(receipt: &SqliteMigrationReceipt, manifest: &HistoryOwnershipManifest) -> bool {
    receipt.format_version == 1
        && receipt.profile_id == *manifest.profile_id()
        && receipt.redaction_profile == manifest.redaction_profile()
        && receipt.ownership_epoch == manifest.epoch()
}

pub(crate) fn validate_receipt(
    database: &HistoryDatabase,
    manifest: &HistoryOwnershipManifest,
) -> io::Result<()> {
    database.read(|connection| {
        let receipt = database::state::<SqliteMigrationReceipt>(
            connection,
            &receipt_key(manifest.redaction_profile()),
        )?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SQLite history activation receipt is missing; no legacy fallback is allowed",
            )
        })?;
        if !receipt_matches(&receipt, manifest) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SQLite history activation receipt does not match ownership",
            ));
        }
        Ok(())
    })
}

fn record_identity(connection: &rusqlite::Connection) -> io::Result<(u64, u64, String)> {
    let mut statement = connection.prepare("SELECT namespace,record_key,sort_time,payload FROM history_records ORDER BY namespace,record_key").map_err(database::sql_error)?;
    let mut rows = statement.query([]).map_err(database::sql_error)?;
    let mut digest = Sha256::new();
    let mut record_count = 0u64;
    let mut record_bytes = 0u64;
    while let Some(row) = rows.next().map_err(database::sql_error)? {
        let namespace: String = row.get(0).map_err(database::sql_error)?;
        let key: String = row.get(1).map_err(database::sql_error)?;
        let time: i64 = row.get(2).map_err(database::sql_error)?;
        let payload = row
            .get_ref(3)
            .map_err(database::sql_error)?
            .as_blob()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        for part in [
            namespace.as_bytes(),
            key.as_bytes(),
            &time.to_be_bytes(),
            payload,
        ] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part);
        }
        record_count = record_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("migration record count overflowed"))?;
        record_bytes = record_bytes
            .checked_add(payload.len() as u64)
            .ok_or_else(|| io::Error::other("migration record bytes overflowed"))?;
    }
    Ok((
        record_count,
        record_bytes,
        format!("{:x}", digest.finalize()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_ownership::{
        InitializeV1Outcome, SQLITE_HISTORY_OWNERSHIP_MANIFEST_VERSION,
    };
    use crate::source_history::{SourceKind, SourceMetadata};

    fn owner(root: &std::path::Path, redaction: RedactionProfile) -> HistoryOwnershipStore {
        HistoryOwnershipStore::new(
            root.join("state"),
            "0123456789abcdef".parse().unwrap(),
            redaction,
        )
    }

    fn file_active(
        owner: &HistoryOwnershipStore,
        lease: &HistoryWriterLease,
    ) -> HistoryOwnershipManifest {
        let first = match owner.initialize_v1_active(lease).unwrap() {
            InitializeV1Outcome::Initialized(manifest)
            | InitializeV1Outcome::Existing(manifest) => manifest,
        };
        let OwnershipCasOutcome::Applied(migrating) = owner.begin_migration(lease, &first).unwrap()
        else {
            panic!("fixture ownership conflict")
        };
        let OwnershipCasOutcome::Applied(active) = owner
            .compare_and_transition(lease, &migrating, HistoryOwnershipState::V2Active)
            .unwrap()
        else {
            panic!("fixture activation conflict")
        };
        active
    }

    fn legacy(root: &std::path::Path) -> SourceHistoryStore {
        let store =
            SourceHistoryStore::new(root.join("state"), "0123456789abcdef".parse().unwrap());
        store
            .save_source_metadata(
                &SourceMetadata::new(
                    "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
                    SourceKind::Local,
                    "local",
                )
                .unwrap(),
            )
            .unwrap();
        store
    }

    #[test]
    fn sqlite_migration_recovers_committed_database_without_reimporting_changed_backup() {
        let directory = tempfile::tempdir().unwrap();
        let owner = owner(directory.path(), RedactionProfile::Redacted);
        let lease = owner.acquire_writer_lease().unwrap();
        let file_active = file_active(&owner, &lease);
        let legacy = legacy(directory.path());
        let error =
            activate_sqlite_history_with_hook(&owner, &lease, &file_active, &legacy, || {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "crash after SQL commit",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let OwnershipManifestStatus::Initialized(migrating) = owner.load_manifest().unwrap() else {
            panic!("lost ownership")
        };
        assert_eq!(
            migrating.version(),
            SQLITE_HISTORY_OWNERSHIP_MANIFEST_VERSION
        );
        assert_eq!(migrating.state(), HistoryOwnershipState::Migrating);
        // Deliberately make the old backup unreadable. Recovery must use the
        // committed receipt, not reread or rewrite that obsolete snapshot.
        let source_file = legacy
            .profile_directory()
            .join("sources/node-0123456789abcdef0123456789abcdef/source.json");
        std::fs::write(&source_file, b"corrupt old backup").unwrap();
        let (active, sql) = activate_sqlite_history(&owner, &lease, &migrating, &legacy).unwrap();
        assert!(active.is_sqlite_backend());
        assert_eq!(active.epoch(), file_active.epoch() + 1);
        assert_eq!(sql.list_source_metadata().unwrap().len(), 1);
        let authority = owner.authorize_v2_write(&lease, &active).unwrap();
        assert_eq!(
            legacy.writer(&authority).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            activate_sqlite_history(&owner, &lease, &active, &legacy)
                .unwrap()
                .0,
            active
        );
    }

    #[test]
    fn sqlite_migration_resumes_after_empty_database_publication() {
        #[cfg(unix)]
        let interrupted_links = [false, true].as_slice();
        #[cfg(not(unix))]
        let interrupted_links = [false].as_slice();
        for &interrupted_link in interrupted_links {
            let directory = tempfile::tempdir().unwrap();
            let owner = owner(directory.path(), RedactionProfile::Redacted);
            let lease = owner.acquire_writer_lease().unwrap();
            let active = file_active(&owner, &lease);
            let legacy = legacy(directory.path());
            let OwnershipCasOutcome::Applied(migrating) =
                owner.begin_sqlite_migration(&lease, &active).unwrap()
            else {
                panic!("fixture migration conflict")
            };
            let db = HistoryDatabase::new(legacy.state_root(), legacy.profile_id());
            legacy
                .prepare_private_directory(db.path().parent().unwrap())
                .unwrap();
            std::fs::write(db.path(), []).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(db.path(), std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            let creation_link = db.path().with_file_name(".history.sqlite3.1234.1.tmp");
            if interrupted_link {
                std::fs::hard_link(db.path(), &creation_link).unwrap();
            }
            let (active, sql) =
                activate_sqlite_history(&owner, &lease, &migrating, &legacy).unwrap();
            assert_eq!(active.state(), HistoryOwnershipState::V2Active);
            validate_receipt(&db, &active).unwrap();
            assert_eq!(sql.list_source_metadata().unwrap().len(), 1);
            assert!(!creation_link.exists());

            // The same empty publication is data loss after activation and
            // must never be initialized as a new authoritative database.
            std::fs::write(db.path(), []).unwrap();
            if interrupted_link {
                std::fs::hard_link(db.path(), &creation_link).unwrap();
            }
            assert!(activate_sqlite_history(&owner, &lease, &active, &legacy).is_err());
            assert_eq!(std::fs::metadata(db.path()).unwrap().len(), 0);
        }
    }

    #[test]
    fn sqlite_migration_fences_both_privacy_namespaces_and_rejects_missing_active_database() {
        let directory = tempfile::tempdir().unwrap();
        let redacted = owner(directory.path(), RedactionProfile::Redacted);
        let preview = owner(directory.path(), RedactionProfile::PreviewEnabled);
        let lease = redacted.acquire_writer_lease().unwrap();
        let active = file_active(&redacted, &lease);
        {
            let preview_lease = preview.acquire_writer_lease().unwrap();
            file_active(&preview, &preview_lease);
        }
        let legacy = legacy(directory.path());
        let (active, sql) = activate_sqlite_history(&redacted, &lease, &active, &legacy).unwrap();
        let OwnershipManifestStatus::Initialized(preview_active) = preview.load_manifest().unwrap()
        else {
            panic!("missing preview")
        };
        assert!(active.is_sqlite_backend() && preview_active.is_sqlite_backend());
        assert_eq!(preview_active.state(), HistoryOwnershipState::V2Active);
        validate_receipt(&sql.sqlite_database().unwrap(), &preview_active).unwrap();
        let path = sql.sqlite_database().unwrap().path().to_owned();
        std::fs::remove_file(&path).unwrap();
        assert!(activate_sqlite_history(&redacted, &lease, &active, &legacy).is_err());
        assert!(sql.sqlite_database().unwrap().write(|_| Ok(())).is_err());
        assert!(!path.exists());
        std::fs::write(&path, []).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(sql.sqlite_database().unwrap().write(|_| Ok(())).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn sqlite_migration_rolls_back_bad_input_and_refuses_corrupt_committed_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let owner = owner(directory.path(), RedactionProfile::Redacted);
        let lease = owner.acquire_writer_lease().unwrap();
        let active = file_active(&owner, &lease);
        let legacy = legacy(directory.path());
        let source_file = legacy
            .profile_directory()
            .join("sources/node-0123456789abcdef0123456789abcdef/source.json");
        let valid = std::fs::read(&source_file).unwrap();
        std::fs::write(&source_file, b"bad metadata").unwrap();
        assert!(activate_sqlite_history(&owner, &lease, &active, &legacy).is_err());
        let db = HistoryDatabase::new(legacy.state_root(), legacy.profile_id());
        db.read(|connection| {
            assert!(database::state_keys(connection, "database/migration/")?.is_empty());
            assert!(database::state_keys(connection, "sources/")?.is_empty());
            Ok(())
        })
        .unwrap();
        std::fs::write(&source_file, valid).unwrap();
        let OwnershipManifestStatus::Initialized(migrating) = owner.load_manifest().unwrap() else {
            panic!("missing ownership")
        };
        let (active, _) = activate_sqlite_history(&owner, &lease, &migrating, &legacy).unwrap();
        db.write(|connection| {
            database::set_state(
                connection,
                &receipt_key(RedactionProfile::Redacted),
                &"invalid receipt",
            )
        })
        .unwrap();
        assert_eq!(
            activate_sqlite_history(&owner, &lease, &active, &legacy)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
