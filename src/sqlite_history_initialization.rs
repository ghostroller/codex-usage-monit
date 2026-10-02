//! Fenced SQLite initialization retaining settings, purge intent, revision floors and quota.
//! Usage, facts, ingest progress and backfill state are rebuilt on demand.

use std::io;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::history_ownership::{
    HistoryOwnershipManifest, HistoryOwnershipState, HistoryOwnershipStore, HistoryWriterLease,
    OwnershipCasOutcome, OwnershipManifestStatus, TryWriterLease,
};
use crate::source_history::database::{self, HistoryDatabase};
use crate::source_history::{HistoryProfileId, RedactionProfile, SourceHistoryStore};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SqliteInitializationReceipt {
    format_version: u32,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    ownership_epoch: u64,
    derived_history_imported: bool,
    sqlite_version: String,
    sqlite_source_id: String,
    completed_at: DateTime<Utc>,
}

pub(crate) fn activate_sqlite_history(
    ownership: &HistoryOwnershipStore,
    lease: &HistoryWriterLease,
    expected: &HistoryOwnershipManifest,
    store: &SourceHistoryStore,
) -> io::Result<(HistoryOwnershipManifest, SourceHistoryStore)> {
    activate_sqlite_history_with_hook(ownership, lease, expected, store, || Ok(()))
}

fn activate_sqlite_history_with_hook(
    ownership: &HistoryOwnershipStore,
    lease: &HistoryWriterLease,
    expected: &HistoryOwnershipManifest,
    store: &SourceHistoryStore,
    before_activation: impl FnOnce() -> io::Result<()>,
) -> io::Result<(HistoryOwnershipManifest, SourceHistoryStore)> {
    ownership.validate_writer_lease(lease)?;
    let target =
        SourceHistoryStore::new_sqlite(store.state_root().to_owned(), store.profile_id().clone());
    let db = target
        .sqlite_database()
        .ok_or_else(|| io::Error::other("SQLite target has no database"))?;
    if expected.is_sqlite_backend() && expected.state() == HistoryOwnershipState::V2Active {
        validate_receipt(&db, expected)?;
        return Ok((expected.clone(), target));
    }
    // The profile database contains both privacy namespaces. Fence every
    // privacy namespace before its contents become authoritative in SQL.
    // Use a nonblocking acquisition for the other namespace: opposite lock
    // acquisition orders may contend, but can never deadlock.
    let other_redaction = match ownership.redaction_profile() {
        RedactionProfile::Redacted => RedactionProfile::PreviewEnabled,
        RedactionProfile::PreviewEnabled => RedactionProfile::Redacted,
    };
    let other_store = HistoryOwnershipStore::new(
        store.state_root().to_owned(),
        store.profile_id().clone(),
        other_redaction,
    );
    let other_lease = match other_store.try_acquire_writer_lease()? {
        TryWriterLease::Acquired(lease) => Some(lease),
        TryWriterLease::Busy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "SQLite initialization is waiting for the other privacy namespace writer",
            ));
        }
    };
    let other_manifest = Some(match other_store.load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest) => manifest,
        OwnershipManifestStatus::Uninitialized => {
            // Both ownership namespaces are published before the shared database.
            // Recreating either one could reuse an epoch from an existing receipt.
            if db.exists()? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "history ownership manifest is missing for the other privacy namespace of an existing SQLite database",
                ));
            }
            match other_store
                .initialize_v1_active(other_lease.as_ref().expect("other lease acquired"))?
            {
                crate::history_ownership::InitializeV1Outcome::Initialized(manifest)
                | crate::history_ownership::InitializeV1Outcome::Existing(manifest) => manifest,
            }
        }
    });

    let current = begin_if_needed(ownership, lease, expected)?;
    let other = match (&other_manifest, &other_lease) {
        (Some(manifest), Some(lease)) => Some(begin_if_needed(&other_store, lease, manifest)?),
        _ => None,
    };
    let mut participants = vec![current.clone()];
    if let Some(other) = &other {
        participants.push(other.clone());
    }
    // A crash may leave the securely published empty database (including its
    // creation link) before schema initialization. Both participant leases
    // are held here. A writable open can finish that pending initialization;
    // the database's active-owner guard still forbids recreating lost data.
    let receipts_complete = db.write(|connection| {
        let mut complete = true;
        for manifest in &participants {
            let receipt = database::state::<SqliteInitializationReceipt>(
                connection,
                &receipt_key(manifest.redaction_profile()),
            )?;
            let matches = receipt
                .as_ref()
                .is_some_and(|receipt| receipt_matches(receipt, manifest));
            if manifest.state() == HistoryOwnershipState::V2Active && !matches {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active privacy namespace has no matching SQLite initialization receipt",
                ));
            }
            complete &= matches;
        }
        Ok(complete)
    })?;

    if !receipts_complete {
        db.write(|connection| {
            let retained_key = "database/retained-state-completed";
            match database::state::<bool>(connection, retained_key)? {
                None => {
                    crate::sqlite_retained_state::retain_required_state(&target)?;
                    database::set_state(connection, retained_key, &true)?;
                }
                Some(true) => {}
                Some(false) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid SQLite retention completion marker",
                    ));
                }
            }
            validate_participants(
                ownership,
                lease,
                &current,
                &other_store,
                other_lease.as_ref(),
                other.as_ref(),
            )?;
            let sqlite_version: String = connection
                .query_row("SELECT sqlite_version()", [], |row| row.get(0))
                .map_err(database::sql_error)?;
            let sqlite_source_id: String = connection
                .query_row("SELECT sqlite_source_id()", [], |row| row.get(0))
                .map_err(database::sql_error)?;
            for manifest in &participants {
                let receipt = SqliteInitializationReceipt {
                    format_version: 1,
                    profile_id: store.profile_id().clone(),
                    redaction_profile: manifest.redaction_profile(),
                    ownership_epoch: manifest.epoch(),
                    derived_history_imported: false,
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
    // activations resumes from the committed receipts without reading the old inputs again.
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
    match store.begin_sqlite_initialization(lease, manifest)? {
        OwnershipCasOutcome::Applied(manifest) => Ok(manifest),
        OwnershipCasOutcome::Conflict(_) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "ownership changed before SQLite initialization",
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
    match store.complete_sqlite_initialization(lease, manifest)? {
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
            "ownership changed during SQLite initialization",
        ));
    }
    if let (Some(lease), Some(manifest)) = (other_lease, other) {
        other_store.validate_writer_lease(lease)?;
        if other_store.load_manifest()? != OwnershipManifestStatus::Initialized(manifest.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "privacy namespace changed during SQLite initialization",
            ));
        }
    }
    Ok(())
}

fn receipt_key(redaction: RedactionProfile) -> String {
    format!("database/initialization/{}", redaction.directory_name())
}

fn receipt_matches(
    receipt: &SqliteInitializationReceipt,
    manifest: &HistoryOwnershipManifest,
) -> bool {
    receipt.format_version == 1
        && !receipt.derived_history_imported
        && receipt.profile_id == *manifest.profile_id()
        && receipt.redaction_profile == manifest.redaction_profile()
        && receipt.ownership_epoch == manifest.epoch()
}

pub(crate) fn validate_receipt(
    database: &HistoryDatabase,
    manifest: &HistoryOwnershipManifest,
) -> io::Result<()> {
    database.read(|connection| {
        let receipt = database::state::<SqliteInitializationReceipt>(
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

#[cfg(test)]
pub(crate) fn initialize_for_test(
    ownership: &HistoryOwnershipStore,
) -> io::Result<(HistoryOwnershipManifest, SourceHistoryStore)> {
    crate::source_identity::SourceIdentityStore::at_path(
        ownership.state_root().join("source-identity.json"),
    )
    .load_or_create()?;
    let lease = ownership.acquire_writer_lease()?;
    let manifest = match ownership.load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest) => manifest,
        OwnershipManifestStatus::Uninitialized => match ownership.initialize_v1_active(&lease)? {
            crate::history_ownership::InitializeV1Outcome::Initialized(manifest)
            | crate::history_ownership::InitializeV1Outcome::Existing(manifest) => manifest,
        },
    };
    let store = SourceHistoryStore::new_sqlite(
        ownership.state_root().to_owned(),
        ownership.profile_id().clone(),
    );
    activate_sqlite_history(ownership, &lease, &manifest, &store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use chrono::{Duration, TimeZone};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::domain::Provenance;
    use crate::history::{HistoryObservation, QuotaPoint};
    use crate::source_history::{LocalObservationMode, SourceKind, SourceMetadata};
    use crate::source_identity::{NodeId, SourceIdentity, SourceIdentityStore};

    const PROFILE: &str = "0123456789abcdef";
    const REMOTE: &str = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn fixture() -> (
        TempDir,
        HistoryOwnershipStore,
        SourceHistoryStore,
        SourceIdentity,
    ) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("state");
        let identity = SourceIdentityStore::at_path(root.join("source-identity.json"))
            .load_or_create()
            .unwrap();
        let profile: HistoryProfileId = PROFILE.parse().unwrap();
        let ownership = HistoryOwnershipStore::new(
            root.clone(),
            profile.clone(),
            RedactionProfile::PreviewEnabled,
        );
        let store = SourceHistoryStore::new_sqlite(root, profile);
        (temp, ownership, store, identity)
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 2, 10, 0, 0)
            .single()
            .unwrap()
    }

    fn point() -> QuotaPoint {
        QuotaPoint {
            observed_at: at(),
            limit_id: "codex".to_owned(),
            duration_mins: 300,
            resets_at: at() + Duration::hours(2),
            used_percent: 25.0,
            remaining_percent: 75.0,
            provenance: Provenance::ServerSnapshot,
        }
    }

    fn write_old(store: &SourceHistoryStore, path: &Path, value: &Value) {
        write_bytes(store, path, &serde_json::to_vec(value).unwrap());
    }

    fn write_bytes(store: &SourceHistoryStore, path: &Path, bytes: &[u8]) {
        store
            .prepare_private_directory(path.parent().unwrap())
            .unwrap();
        #[cfg(windows)]
        let mut file = crate::windows_private_directory::open_private_file(path).unwrap();
        #[cfg(not(windows))]
        let mut file = {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path).unwrap()
        };
        file.set_len(0).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn old_v1_quota(store: &SourceHistoryStore) -> std::path::PathBuf {
        let path = store
            .state_root()
            .join("history-v1")
            .join(PROFILE)
            .join("2026-10-02.json");
        // Usage fields have deliberately incompatible shapes. They must be
        // skipped, rather than parsed by a legacy usage adapter.
        write_old(
            store,
            &path,
            &json!({
                "formatVersion": 2, "namespace": PROFILE, "utcDay": "2026-10-02",
                "quotaPoints": [point()], "halfHourBuckets": {"obsolete": "shape"},
                "weeklyLocalPoints": "unsupported-old-usage"
            }),
        );
        path
    }

    fn begin(
        ownership: &HistoryOwnershipStore,
        lease: &HistoryWriterLease,
    ) -> HistoryOwnershipManifest {
        match ownership.initialize_v1_active(lease).unwrap() {
            crate::history_ownership::InitializeV1Outcome::Initialized(manifest)
            | crate::history_ownership::InitializeV1Outcome::Existing(manifest) => manifest,
        }
    }

    #[test]
    fn initialization_retains_only_quota_policies_and_revision_floor() {
        let (_temp, ownership, store, identity) = fixture();
        old_v1_quota(&store);
        // The same sample may exist in both old file layouts; retention must
        // preserve it once without bringing across either usage payload.
        write_old(
            &store,
            &store.account_directory().join("2026-10-02.json"),
            &json!({
                "formatVersion": 1, "quotaRevision": 1, "profileId": PROFILE,
                "utcDay": "2026-10-02", "quotaPoints": [point()]
            }),
        );
        let local = identity.node_id();
        let mut metadata = SourceMetadata::new_with_redaction_profile(
            local.clone(),
            SourceKind::Local,
            "my device",
            RedactionProfile::PreviewEnabled,
        )
        .unwrap();
        metadata.set_include_in_aggregates(false);
        write_old(
            &store,
            &store.source_directory(local).join("source.json"),
            &json!({"formatVersion":1,"profileId":PROFILE,"source":metadata}),
        );
        write_old(
            &store,
            &store
                .source_directory(local)
                .join("preview-enabled/local-observation-state.json"),
            &json!({
                "formatVersion":1,"profileId":PROFILE,"sourceId":local,"sourceGeneration":identity.generation(),
                "redactionProfile":"preview-enabled","lastReservedRevision":41
            }),
        );
        // No old usage, digest, facts, pending state or sync progress is read.
        for name in [
            "preview-enabled/buckets/2026-10-02.json",
            "preview-enabled/weekly/2026-10-02.json",
            "preview-enabled/local-observation-pending.json",
            "preview-enabled/session-digests/2026-10-02.json",
        ] {
            write_bytes(
                &store,
                &store.source_directory(local).join(name),
                b"broken discarded payload",
            );
        }
        write_bytes(
            &store,
            &store
                .profile_directory()
                .join("remote-ingest-v1/broken.json"),
            b"broken cursor",
        );

        let (active, sql) = initialize_for_test(&ownership).unwrap();
        assert!(active.is_sqlite_backend());
        assert_eq!(active.state(), HistoryOwnershipState::V2Active);
        assert_eq!(
            sql.load_account_since(at() - Duration::days(1))
                .unwrap()
                .quota_points,
            vec![point()]
        );
        assert_eq!(sql.load_source_metadata(local).unwrap(), metadata);
        assert_eq!(
            sql.load_local_observation_revision(&identity, RedactionProfile::PreviewEnabled)
                .unwrap(),
            41
        );
        assert_eq!(
            sql.load_local_observation_projection_revision(
                &identity,
                RedactionProfile::PreviewEnabled
            )
            .unwrap(),
            0
        );
        assert!(
            sql.load_source_since(
                local,
                RedactionProfile::PreviewEnabled,
                at() - Duration::days(1)
            )
            .unwrap()
            .buckets
            .is_empty()
        );
        assert!(
            sql.load_v2_summary_backfill_attempt(RedactionProfile::PreviewEnabled, active.epoch())
                .unwrap()
                .is_none()
        );
        let lease = ownership.acquire_writer_lease().unwrap();
        let authority = ownership.authorize_v2_write(&lease, &active).unwrap();
        let writer = sql.writer(&authority).unwrap();
        let report = writer
            .record_local_observation(
                &identity,
                "my device",
                RedactionProfile::PreviewEnabled,
                &HistoryObservation {
                    observed_at: at(),
                    ..Default::default()
                },
                LocalObservationMode::Incremental,
            )
            .unwrap();
        assert_eq!(
            report.revision, 42,
            "new rebuilt publications cannot reuse old revisions"
        );
        assert!(
            !sql.load_source_metadata(local)
                .unwrap()
                .include_in_aggregates()
        );
    }

    #[test]
    fn committed_retention_resumes_without_old_files_and_never_reimports_them() {
        let (_temp, ownership, store, _identity) = fixture();
        let old_quota = old_v1_quota(&store);
        let lease = ownership.acquire_writer_lease().unwrap();
        let expected = begin(&ownership, &lease);
        let error =
            activate_sqlite_history_with_hook(&ownership, &lease, &expected, &store, || {
                Err(io::Error::other("crash before activation"))
            })
            .unwrap_err();
        assert!(error.to_string().contains("crash before activation"));
        let pending = match ownership.load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            _ => unreachable!(),
        };
        assert_eq!(pending.state(), HistoryOwnershipState::Migrating);
        assert!(pending.is_sqlite_backend());
        fs::remove_file(&old_quota).unwrap();
        let (active, sql) = activate_sqlite_history(&ownership, &lease, &pending, &store).unwrap();
        validate_receipt(&sql.sqlite_database().unwrap(), &active).unwrap();
        assert_eq!(
            sql.load_account_since(at() - Duration::days(1))
                .unwrap()
                .quota_points,
            vec![point()]
        );
        write_bytes(
            &store,
            &old_quota,
            b"invalid old copy must never be reopened",
        );
        activate_sqlite_history(&ownership, &lease, &active, &sql).unwrap();
        drop(lease);
        let other = HistoryOwnershipStore::new(
            store.state_root().to_owned(),
            store.profile_id().clone(),
            RedactionProfile::Redacted,
        );
        let (other_active, _) = initialize_for_test(&other).unwrap();
        assert!(other_active.is_sqlite_backend());
    }

    #[test]
    fn committed_receipts_refuse_lost_other_privacy_ownership() {
        for remove_anchor in [false, true] {
            let (_temp, ownership, store, _identity) = fixture();
            old_v1_quota(&store);
            let lease = ownership.acquire_writer_lease().unwrap();
            let expected = begin(&ownership, &lease);
            let error =
                activate_sqlite_history_with_hook(&ownership, &lease, &expected, &store, || {
                    Err(io::Error::other("crash after receipts committed"))
                })
                .unwrap_err();
            assert!(error.to_string().contains("crash after receipts committed"));
            let OwnershipManifestStatus::Initialized(pending) = ownership.load_manifest().unwrap()
            else {
                panic!("current privacy namespace must remain initialized");
            };
            assert_eq!(pending.state(), HistoryOwnershipState::Migrating);

            let other = HistoryOwnershipStore::new(
                store.state_root().to_owned(),
                store.profile_id().clone(),
                RedactionProfile::Redacted,
            );
            let OwnershipManifestStatus::Initialized(other_pending) =
                other.load_manifest().unwrap()
            else {
                panic!("other privacy namespace must have been fenced");
            };
            let database = store.sqlite_database().unwrap();
            validate_receipt(&database, &pending).unwrap();
            validate_receipt(&database, &other_pending).unwrap();
            let receipts = database
                .read(|connection| {
                    [
                        pending.redaction_profile(),
                        other_pending.redaction_profile(),
                    ]
                    .map(|redaction| database::state::<Value>(connection, &receipt_key(redaction)))
                    .into_iter()
                    .collect::<io::Result<Vec<_>>>()
                })
                .unwrap();

            fs::remove_file(other.manifest_path()).unwrap();
            if remove_anchor {
                fs::remove_file(other.initialization_anchor_path()).unwrap();
            }
            let error = activate_sqlite_history(&ownership, &lease, &pending, &store).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("ownership manifest is missing"));
            assert!(!other.manifest_path().exists());
            assert_eq!(other.initialization_anchor_path().exists(), !remove_anchor);
            assert_eq!(
                ownership.load_manifest().unwrap(),
                OwnershipManifestStatus::Initialized(pending.clone())
            );
            let after = database
                .read(|connection| {
                    [
                        pending.redaction_profile(),
                        other_pending.redaction_profile(),
                    ]
                    .map(|redaction| database::state::<Value>(connection, &receipt_key(redaction)))
                    .into_iter()
                    .collect::<io::Result<Vec<_>>>()
                })
                .unwrap();
            assert_eq!(after, receipts);
            assert_eq!(
                store
                    .load_account_since(at() - Duration::days(1))
                    .unwrap()
                    .quota_points,
                vec![point()]
            );
        }
    }

    #[test]
    fn invalid_quota_rolls_back_all_retained_state_and_retry_is_fenced() {
        let (_temp, ownership, store, _identity) = fixture();
        let remote: NodeId = REMOTE.parse().unwrap();
        let metadata = SourceMetadata::new(remote.clone(), SourceKind::Ssh, "remote").unwrap();
        write_old(
            &store,
            &store.source_directory(&remote).join("source.json"),
            &json!({"formatVersion":1,"profileId":PROFILE,"source":metadata}),
        );
        let path = old_v1_quota(&store);
        let mut bad = point();
        bad.observed_at += Duration::days(1);
        write_old(
            &store,
            &path,
            &json!({"formatVersion":2,"namespace":PROFILE,"utcDay":"2026-10-02","quotaPoints":[bad]}),
        );
        let error = initialize_for_test(&ownership).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            store.load_source_metadata(&remote).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(
            store
                .load_account_since(at() - Duration::days(1))
                .unwrap()
                .quota_points
                .is_empty()
        );
        let pending = match ownership.load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            _ => unreachable!(),
        };
        assert!(pending.is_sqlite_backend());
        assert_eq!(pending.state(), HistoryOwnershipState::Migrating);
        old_v1_quota(&store);
        initialize_for_test(&ownership).unwrap();
        assert_eq!(store.load_source_metadata(&remote).unwrap(), metadata);
    }

    #[test]
    fn remote_quota_is_retained_without_old_generation_or_cursor() {
        let (_temp, ownership, store, _identity) = fixture();
        let remote: NodeId = REMOTE.parse().unwrap();
        let metadata = SourceMetadata::new_with_redaction_profile(
            remote.clone(),
            SourceKind::Ssh,
            "remote",
            RedactionProfile::PreviewEnabled,
        )
        .unwrap();
        write_old(
            &store,
            &store.source_directory(&remote).join("source.json"),
            &json!({"formatVersion":1,"profileId":PROFILE,"source":metadata}),
        );
        let root = store
            .source_directory(&remote)
            .join("preview-enabled/remote-history-v1");
        let generation = "ingest-gen-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        write_old(
            &store,
            &root.join("active.json"),
            &json!({"formatVersion":2,"profileId":PROFILE,"sourceId":remote,"redactionProfile":"preview-enabled","activeGeneration":generation,"binding":"discarded","activatedAt":"discarded"}),
        );
        let quota = crate::remote_quota::RemoteQuotaPoint::from_local(&point()).unwrap();
        write_old(
            &store,
            &root.join("generations").join(generation).join("quota.json"),
            &json!({"version":1,"profileId":PROFILE,"sourceId":remote,"redactionProfile":"preview-enabled","lastSequence":"discarded","days":{"2026-10-02":{"day":"2026-10-02","points":[quota]}}}),
        );
        let (_, sql) = initialize_for_test(&ownership).unwrap();
        assert_eq!(
            sql.load_remote_quota_since_with_budget(
                &remote,
                RedactionProfile::PreviewEnabled,
                at() - Duration::days(1),
                &mut crate::source_history::SourceHistoryReadBudget::for_query()
            )
            .unwrap(),
            vec![point()]
        );
        assert!(
            sql.active_remote_history_generation(&remote, RedactionProfile::PreviewEnabled)
                .unwrap()
                .is_none()
        );
        assert!(
            sql.load_source_since(
                &remote,
                RedactionProfile::PreviewEnabled,
                at() - Duration::days(1)
            )
            .unwrap()
            .buckets
            .is_empty()
        );
    }

    #[test]
    fn initialization_preserves_irreversible_purge_intent_with_or_without_source_metadata() {
        for metadata_present in [true, false] {
            let (_temp, ownership, store, _identity) = fixture();
            old_v1_quota(&store);
            let claimed: NodeId = REMOTE.parse().unwrap();
            let other: NodeId = "node-cccccccccccccccccccccccccccccccc".parse().unwrap();
            let mut claimed_metadata = SourceMetadata::new_with_redaction_profile(
                claimed.clone(),
                SourceKind::Ssh,
                "claimed",
                RedactionProfile::PreviewEnabled,
            )
            .unwrap();
            claimed_metadata.set_detached(true);
            let other_metadata = SourceMetadata::new_with_redaction_profile(
                other.clone(),
                SourceKind::Ssh,
                "other",
                RedactionProfile::PreviewEnabled,
            )
            .unwrap();
            for metadata in [&claimed_metadata, &other_metadata] {
                write_old(
                    &store,
                    &store
                        .source_directory(metadata.source_id())
                        .join("source.json"),
                    &json!({"formatVersion":1,"profileId":PROFILE,"source":metadata}),
                );
                let root = store
                    .source_directory(metadata.source_id())
                    .join("preview-enabled/remote-history-v1");
                let generation = "ingest-gen-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
                write_old(
                    &store,
                    &root.join("active.json"),
                    &json!({"formatVersion":2,"profileId":PROFILE,"sourceId":metadata.source_id(),"redactionProfile":"preview-enabled","activeGeneration":generation}),
                );
                let quota = crate::remote_quota::RemoteQuotaPoint::from_local(&point()).unwrap();
                write_old(
                    &store,
                    &root.join("generations").join(generation).join("quota.json"),
                    &json!({"version":1,"profileId":PROFILE,"sourceId":metadata.source_id(),"redactionProfile":"preview-enabled","days":{"2026-10-02":{"day":"2026-10-02","points":[quota]}}}),
                );
            }
            let purge_path = store.source_directory(&claimed).join("source-purge.json");
            write_old(
                &store,
                &purge_path,
                &json!({"formatVersion":1,"profileId":PROFILE,"sourceId":claimed,"sourceKind":"ssh"}),
            );
            if !metadata_present {
                // The durable claim remains after a partial old removal.
                fs::remove_file(store.source_directory(&claimed).join("source.json")).unwrap();
            }
            let (active, sql) = initialize_for_test(&ownership).unwrap();
            assert_eq!(
                sql.ensure_source_not_pending_purge(&claimed)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            {
                let lease = ownership.acquire_writer_lease().unwrap();
                let authority = ownership.authorize_v2_write(&lease, &active).unwrap();
                let writer = sql.writer(&authority).unwrap();
                assert_eq!(
                    writer
                        .update_source_metadata(&claimed, |metadata| {
                            metadata.set_detached(false);
                            Ok(())
                        })
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    writer
                        .save_source_metadata(&claimed_metadata)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::PermissionDenied
                );
                let report = writer.purge_detached_ssh_source(&claimed).unwrap();
                assert!(report.resumed_claim());
                sql.ensure_source_not_pending_purge(&claimed).unwrap();
                assert_eq!(
                    sql.load_source_metadata(&claimed).unwrap_err().kind(),
                    io::ErrorKind::NotFound
                );
                assert_eq!(sql.load_source_metadata(&other).unwrap(), other_metadata);
                assert_eq!(
                    sql.load_remote_quota_since(
                        &other,
                        RedactionProfile::PreviewEnabled,
                        at() - Duration::days(1)
                    )
                    .unwrap(),
                    vec![point()]
                );
                assert_eq!(
                    sql.load_account_since(at() - Duration::days(1))
                        .unwrap()
                        .quota_points,
                    vec![point()]
                );
            }
            let (_, restarted) = initialize_for_test(&ownership).unwrap();
            restarted.ensure_source_not_pending_purge(&claimed).unwrap();
            assert_eq!(
                restarted.load_source_metadata(&claimed).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
            assert!(
                purge_path.is_file(),
                "the old input remains a backup, not the SQL authority"
            );
        }
    }

    #[test]
    fn other_privacy_writer_blocks_initialization_without_publishing_database() {
        let (_temp, ownership, store, _identity) = fixture();
        let other = HistoryOwnershipStore::new(
            store.state_root().to_owned(),
            store.profile_id().clone(),
            RedactionProfile::Redacted,
        );
        let _busy = other.acquire_writer_lease().unwrap();
        let error = initialize_for_test(&ownership).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(!store.sqlite_database().unwrap().exists().unwrap());
    }

    #[test]
    fn active_database_loss_never_recreates_or_reimports_legacy_quota() {
        let (_temp, ownership, store, _identity) = fixture();
        old_v1_quota(&store);
        initialize_for_test(&ownership).unwrap();
        let db = store.sqlite_database().unwrap();
        fs::remove_file(db.path()).unwrap();
        let error = initialize_for_test(&ownership).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::InvalidData
        ));
        assert!(!db.path().exists());
    }
}
