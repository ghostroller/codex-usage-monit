//! Crash-safe destruction of one detached SSH source.
//! A durable SQL claim fences reattach while external state is cleaned up;
//! records, metadata and the claim are then removed in one transaction.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::*;

const SOURCE_PURGE_MARKER_FILE: &str = "source-purge.json";
const SOURCE_PURGE_MARKER_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceHistoryPurgeReport {
    resumed_claim: bool,
}

impl SourceHistoryPurgeReport {
    pub fn resumed_claim(self) -> bool {
        self.resumed_claim
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourcePurgeMarker {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    source_kind: SourceKind,
}

impl SourcePurgeMarker {
    fn ssh(profile_id: HistoryProfileId, source_id: NodeId) -> Self {
        Self {
            format_version: SOURCE_PURGE_MARKER_FORMAT_VERSION,
            profile_id,
            source_id,
            source_kind: SourceKind::Ssh,
        }
    }

    pub(crate) fn validate(
        &self,
        profile_id: &HistoryProfileId,
        source_id: &NodeId,
    ) -> io::Result<()> {
        if self.format_version != SOURCE_PURGE_MARKER_FORMAT_VERSION
            || &self.profile_id != profile_id
            || &self.source_id != source_id
            || self.source_kind != SourceKind::Ssh
        {
            return Err(invalid_data(
                "source purge marker does not match its source/profile namespace",
            ));
        }
        Ok(())
    }
}

impl SourceHistoryWriter<'_, '_, '_> {
    /// Durably claims one eligible detached source before any related ingest
    /// or project-mapping state is removed. Once this returns, ordinary
    /// metadata updates (including reattach) are fenced until purge finishes.
    pub(crate) fn prepare_detached_ssh_source_for_purge(
        &self,
        source_id: &NodeId,
    ) -> io::Result<()> {
        let database = self
            .store
            .sqlite_database()
            .expect("SQLite history backend");

        self.fenced(|store| {
            database.write(|_| {
                store.prepare_detached_ssh_source_for_purge_unfenced(source_id)?;
                self.validate()
            })
        })
    }

    /// Irreversibly removes one detached SSH source and no other history
    /// namespace. The caller must separately fence the remotes allowlist.
    pub(crate) fn purge_detached_ssh_source(
        &self,
        source_id: &NodeId,
    ) -> io::Result<SourceHistoryPurgeReport> {
        let database = self
            .store
            .sqlite_database()
            .expect("SQLite history backend");

        self.fenced(|store| {
            database.write(|_| {
                let report = store.purge_detached_ssh_source_unfenced(source_id)?;
                self.validate()?;
                Ok(report)
            })
        })
    }
}

impl SourceHistoryStore {
    /// Fences re-pairing against an irreversible purge that was durably
    /// claimed before a crash. Callers that mutate the allowlist must hold the
    /// remotes config lock before entering this source lock.
    pub(crate) fn ensure_source_not_pending_purge(&self, source_id: &NodeId) -> io::Result<()> {
        let database = self.sqlite_database().expect("SQLite history backend");

        if !database.exists()? {
            return Ok(());
        }
        database.read(|connection| {
            if sqlite_purge_marker(self, &database, connection, source_id)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "remote source cannot be paired while an irreversible purge is pending",
                ));
            }
            Ok(())
        })
    }

    fn prepare_detached_ssh_source_for_purge_unfenced(&self, source_id: &NodeId) -> io::Result<()> {
        let database = self.sqlite_database().expect("SQLite history backend");

        database.write(|connection| {
            let marker = sqlite_purge_marker(self, &database, connection, source_id)?;
            require_sqlite_purge_eligible(self, source_id, marker.is_some())?;
            if marker.is_none() {
                let key = database.namespace(
                    &self
                        .source_directory(source_id)
                        .join(SOURCE_PURGE_MARKER_FILE),
                )?;
                database::set_state(
                    connection,
                    &key,
                    &SourcePurgeMarker::ssh(self.profile_id.clone(), source_id.clone()),
                )?;
            }
            Ok(())
        })
    }

    fn purge_detached_ssh_source_unfenced(
        &self,
        source_id: &NodeId,
    ) -> io::Result<SourceHistoryPurgeReport> {
        let database = self.sqlite_database().expect("SQLite history backend");

        database.write(|connection| {
            // A pre-existing claim is validated even though the source
            // subtree and claim can now be removed in one transaction.
            let marker = sqlite_purge_marker(self, &database, connection, source_id)?;
            require_sqlite_purge_eligible(self, source_id, marker.is_some())?;
            let namespace = database.namespace(&self.source_directory(source_id))?;
            delete_sqlite_namespace_tree(connection, &namespace)?;
            Ok(SourceHistoryPurgeReport {
                resumed_claim: marker.is_some(),
            })
        })
    }
}

fn require_sqlite_purge_eligible(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    claimed: bool,
) -> io::Result<()> {
    let metadata = match store.load_source_metadata(source_id) {
        Ok(metadata) => metadata,
        // A validated SQL claim can outlive its source metadata. The
        // irreversible SSH claim still authorizes finishing its removal.
        Err(error) if claimed && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.kind() != SourceKind::Ssh {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only retained SSH sources can be purged",
        ));
    }
    if !metadata.detached() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "an attached SSH source cannot be purged; remove its configured host first",
        ));
    }
    Ok(())
}

fn sqlite_purge_marker(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
) -> io::Result<Option<SourcePurgeMarker>> {
    let key = database.namespace(
        &store
            .source_directory(source_id)
            .join(SOURCE_PURGE_MARKER_FILE),
    )?;
    let marker =
        sqlite_state_bounded::<SourcePurgeMarker>(connection, &key, MAX_METADATA_FILE_BYTES, None)?;
    if let Some(marker) = &marker {
        marker.validate(store.profile_id(), source_id)?;
    }
    Ok(marker)
}

/// Removes exactly one logical subtree. The separating slash prevents an
/// adjacent source or profile whose name shares a prefix from being selected.
pub(super) fn delete_sqlite_namespace_tree(
    connection: &rusqlite::Connection,
    namespace: &str,
) -> io::Result<()> {
    if namespace.is_empty() {
        return Err(invalid_data("cannot purge an empty history namespace"));
    }
    let prefix = format!("{namespace}/");
    connection
        .execute(
            "DELETE FROM history_records WHERE namespace=?1 OR substr(namespace,1,length(?2))=?2",
            rusqlite::params![namespace, prefix],
        )
        .map_err(database::sql_error)?;
    connection
        .execute(
            "DELETE FROM history_state WHERE state_key=?1 OR substr(state_key,1,length(?2))=?2",
            rusqlite::params![namespace, prefix],
        )
        .map_err(database::sql_error)?;
    Ok(())
}

pub(super) fn reject_source_metadata_update_during_purge(
    store: &SourceHistoryStore,
    _directory: &Path,
    source_id: &NodeId,
) -> io::Result<()> {
    let database = store.sqlite_database().expect("SQLite history backend");

    if !database.exists()? {
        return Ok(());
    }
    database.read(|connection| {
        if sqlite_purge_marker(store, &database, connection, source_id)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "source metadata cannot change while an irreversible purge is pending",
            ));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const REMOTE: &str = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "node-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn fixture() -> (tempfile::TempDir, SourceHistoryStore, NodeId) {
        let directory = tempdir().unwrap();
        let store = SourceHistoryStore::new(
            directory.path().join("state"),
            "profile-one".parse().unwrap(),
        );
        let source = REMOTE.parse().unwrap();
        (directory, store, source)
    }

    fn save_source(store: &SourceHistoryStore, source: &NodeId, kind: SourceKind, detached: bool) {
        let mut metadata = SourceMetadata::new(source.clone(), kind, "test source").unwrap();
        metadata.set_detached(detached);
        store.save_source_metadata(&metadata).unwrap();
    }

    #[test]
    fn purge_refuses_attached_and_local_sources() {
        let (_directory, store, remote) = fixture();
        save_source(&store, &remote, SourceKind::Ssh, false);
        let error = store
            .purge_detached_ssh_source_unfenced(&remote)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(store.load_source_metadata(&remote).is_ok());

        let local: NodeId = OTHER.parse().unwrap();
        save_source(&store, &local, SourceKind::Local, true);
        let error = store
            .purge_detached_ssh_source_unfenced(&local)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(store.load_source_metadata(&local).is_ok());
    }

    #[test]
    fn durable_purge_claim_blocks_metadata_reattach_before_related_cleanup() {
        let (_directory, store, remote) = fixture();
        save_source(&store, &remote, SourceKind::Ssh, true);

        store
            .prepare_detached_ssh_source_for_purge_unfenced(&remote)
            .unwrap();
        assert!(store.ensure_source_not_pending_purge(&remote).is_err());
        let error = store
            .update_source_metadata(&remote, |metadata| {
                metadata.set_detached(false);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(store.load_source_metadata(&remote).unwrap().detached());

        store.purge_detached_ssh_source_unfenced(&remote).unwrap();
        assert_eq!(
            store.load_source_metadata(&remote).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn sqlite_purge_claim_survives_restart_and_removal_is_atomic_and_source_scoped() {
        let (_directory, legacy, remote) = fixture();
        let store = SourceHistoryStore::new_sqlite(
            legacy.state_root().to_owned(),
            legacy.profile_id().clone(),
        );
        let other: NodeId = OTHER.parse().unwrap();
        save_source(&store, &remote, SourceKind::Ssh, true);
        save_source(&store, &other, SourceKind::Ssh, true);
        let database = store.sqlite_database().unwrap();
        let removed = database
            .namespace(
                &store
                    .source_directory(&remote)
                    .join("redacted/facts/active"),
            )
            .unwrap();
        let retained = database
            .namespace(&store.source_directory(&other).join("redacted/facts/active"))
            .unwrap();
        let account = database.namespace(&store.account_directory()).unwrap();
        database
            .write(|connection| {
                database::put_record(connection, &removed, "event", 1, &1u64)?;
                database::put_record(connection, &retained, "event", 1, &2u64)?;
                database::put_record(connection, &account, "quota", 1, &3u64)
            })
            .unwrap();
        store
            .prepare_detached_ssh_source_for_purge_unfenced(&remote)
            .unwrap();
        let restarted = SourceHistoryStore::new_sqlite(
            store.state_root().to_owned(),
            store.profile_id().clone(),
        );
        assert_eq!(
            restarted
                .ensure_source_not_pending_purge(&remote)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            restarted
                .update_source_metadata(&remote, |metadata| {
                    metadata.set_detached(false);
                    Ok(())
                })
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let error = database
            .write(|_| {
                restarted.purge_detached_ssh_source_unfenced(&remote)?;
                Err::<(), _>(io::Error::other("interrupt before purge commit"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "interrupt before purge commit");
        assert!(restarted.load_source_metadata(&remote).unwrap().detached());
        assert!(restarted.ensure_source_not_pending_purge(&remote).is_err());
        restarted
            .purge_detached_ssh_source_unfenced(&remote)
            .unwrap();
        assert_eq!(
            restarted.load_source_metadata(&remote).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(restarted.load_source_metadata(&other).is_ok());
        restarted.ensure_source_not_pending_purge(&remote).unwrap();
        database
            .read(|connection| {
                let load = |namespace: &str| {
                    database::records::<u64>(
                        connection,
                        namespace,
                        0,
                        &mut SourceHistoryReadBudget::for_query(),
                    )
                };
                assert!(load(&removed)?.is_empty());
                assert_eq!(load(&retained)?, vec![2]);
                assert_eq!(load(&account)?, vec![3]);
                Ok(())
            })
            .unwrap();
    }
}
