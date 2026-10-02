//! Atomic retirement of no-longer-visible SSH preview history.
//!
//! Metadata policy publication and preview namespace deletion share one SQL
//! transaction, under the exact remotes-config and ownership writer fences.

#[cfg(test)]
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::*;

const RETIREMENT_MARKER_FILE: &str = "redaction-retirement.json";
const RETIREMENT_MARKER_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceRedactionRetirementStatus {
    NotRequired,
    Complete,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceRedactionRetirementMarker {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    retiring_profile: RedactionProfile,
    replacement_profile: RedactionProfile,
}

impl SourceRedactionRetirementMarker {
    fn preview_to_redacted(profile_id: HistoryProfileId, source_id: NodeId) -> Self {
        Self {
            format_version: RETIREMENT_MARKER_FORMAT_VERSION,
            profile_id,
            source_id,
            retiring_profile: RedactionProfile::PreviewEnabled,
            replacement_profile: RedactionProfile::Redacted,
        }
    }

    fn validate(&self, profile_id: &HistoryProfileId, source_id: &NodeId) -> io::Result<()> {
        if self.format_version != RETIREMENT_MARKER_FORMAT_VERSION
            || &self.profile_id != profile_id
            || &self.source_id != source_id
            || self.retiring_profile != RedactionProfile::PreviewEnabled
            || self.replacement_profile != RedactionProfile::Redacted
        {
            return Err(invalid_data(
                "source redaction retirement marker does not match its namespace",
            ));
        }
        Ok(())
    }
}

impl SourceHistoryWriter<'_, '_, '_> {
    /// Publishes one aggregate redaction profile for an SSH source.
    ///
    /// Preview-to-redacted publication durably queues old-profile retirement
    /// before changing reader-visible metadata. Other profile transitions do
    /// not remove either namespace.
    pub(crate) fn publish_remote_source_redaction_profile(
        &self,
        source_id: &NodeId,
        target_profile: RedactionProfile,
    ) -> io::Result<(SourceMetadata, SourceRedactionRetirementStatus)> {
        if target_profile != self.redaction_profile() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "v2 history writer authority does not match the published remote redaction profile",
            ));
        }
        let database = self
            .store
            .sqlite_database()
            .expect("SQLite history backend");

        self.fenced(|store| {
            database.write(|_| {
                let result = store
                    .publish_remote_source_redaction_profile_unfenced(source_id, target_profile)?;
                self.validate()?;
                Ok(result)
            })
        })
    }

    /// Makes one bounded recovery pass for a retirement left by a crash or a
    /// previous sharing/IO failure. It never changes aggregate visibility.
    pub(crate) fn retry_remote_source_redaction_retirement(
        &self,
        source_id: &NodeId,
    ) -> io::Result<SourceRedactionRetirementStatus> {
        let database = self
            .store
            .sqlite_database()
            .expect("SQLite history backend");

        self.fenced(|store| {
            database.write(|_| {
                let status = store.retry_remote_source_redaction_retirement_unfenced(
                    source_id,
                    self.redaction_profile(),
                )?;
                self.validate()?;
                Ok(status)
            })
        })
    }
}

impl SourceHistoryStore {
    fn publish_remote_source_redaction_profile_unfenced(
        &self,
        source_id: &NodeId,
        target_profile: RedactionProfile,
    ) -> io::Result<(SourceMetadata, SourceRedactionRetirementStatus)> {
        let database = self.sqlite_database().expect("SQLite history backend");

        database.write(|connection| {
            let mut metadata = self.load_source_metadata(source_id)?;
            require_remote_source(&metadata)?;
            let marker_key =
                database.namespace(&retirement_marker_path(&self.source_directory(source_id)))?;
            let marker = sqlite_retirement_marker(self, connection, source_id, &marker_key)?;
            let retiring = target_profile == RedactionProfile::Redacted
                && (metadata.aggregate_redaction_profile() == RedactionProfile::PreviewEnabled
                    || marker.is_some()
                    || sqlite_preview_exists(self, &database, connection, source_id)?);
            if retiring && marker.is_none() {
                database::set_state(
                    connection,
                    &marker_key,
                    &SourceRedactionRetirementMarker::preview_to_redacted(
                        self.profile_id.clone(),
                        source_id.clone(),
                    ),
                )?;
            }
            if metadata.aggregate_redaction_profile() != target_profile {
                metadata = self.update_source_metadata_unfenced(source_id, |metadata| {
                    metadata.set_aggregate_redaction_profile(target_profile);
                    Ok(())
                })?;
            }
            let status = if retiring {
                let namespace = database.namespace(
                    &self
                        .source_directory(source_id)
                        .join(RedactionProfile::PreviewEnabled.directory_name()),
                )?;
                source_purge::delete_sqlite_namespace_tree(connection, &namespace)?;
                database::delete_state(connection, &marker_key)?;
                SourceRedactionRetirementStatus::Complete
            } else {
                SourceRedactionRetirementStatus::NotRequired
            };
            Ok((metadata, status))
        })
    }

    fn retry_remote_source_redaction_retirement_unfenced(
        &self,
        source_id: &NodeId,
        writer_profile: RedactionProfile,
    ) -> io::Result<SourceRedactionRetirementStatus> {
        if writer_profile != RedactionProfile::Redacted {
            return Ok(SourceRedactionRetirementStatus::NotRequired);
        }
        let database = self.sqlite_database().expect("SQLite history backend");

        database.write(|connection| {
            let metadata = self.load_source_metadata(source_id)?;
            require_remote_source(&metadata)?;
            let marker_key =
                database.namespace(&retirement_marker_path(&self.source_directory(source_id)))?;
            let marker = sqlite_retirement_marker(self, connection, source_id, &marker_key)?;
            if metadata.aggregate_redaction_profile() != RedactionProfile::Redacted {
                return Ok(SourceRedactionRetirementStatus::Pending);
            }
            if marker.is_none() && !sqlite_preview_exists(self, &database, connection, source_id)? {
                return Ok(SourceRedactionRetirementStatus::NotRequired);
            }
            let namespace = database.namespace(
                &self
                    .source_directory(source_id)
                    .join(RedactionProfile::PreviewEnabled.directory_name()),
            )?;
            source_purge::delete_sqlite_namespace_tree(connection, &namespace)?;
            database::delete_state(connection, &marker_key)?;
            Ok(SourceRedactionRetirementStatus::Complete)
        })
    }

    #[cfg(test)]
    pub(crate) fn queue_preview_retirement_for_test(&self, source_id: &NodeId) -> io::Result<()> {
        let database = self.sqlite_database().expect("SQLite history backend");

        let key = database.namespace(&retirement_marker_path(&self.source_directory(source_id)))?;
        database.write(|connection| {
            if sqlite_retirement_marker(self, connection, source_id, &key)?.is_none() {
                database::set_state(
                    connection,
                    &key,
                    &SourceRedactionRetirementMarker::preview_to_redacted(
                        self.profile_id.clone(),
                        source_id.clone(),
                    ),
                )?;
            }
            Ok(())
        })
    }
}

fn sqlite_retirement_marker(
    store: &SourceHistoryStore,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
    key: &str,
) -> io::Result<Option<SourceRedactionRetirementMarker>> {
    let marker = sqlite_state_bounded::<SourceRedactionRetirementMarker>(
        connection,
        key,
        MAX_METADATA_FILE_BYTES,
        None,
    )?;
    if let Some(marker) = &marker {
        marker.validate(store.profile_id(), source_id)?;
    }
    Ok(marker)
}

fn sqlite_preview_exists(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
) -> io::Result<bool> {
    let namespace = database.namespace(
        &store
            .source_directory(source_id)
            .join(RedactionProfile::PreviewEnabled.directory_name()),
    )?;
    let prefix = format!("{namespace}/");
    connection.query_row("SELECT EXISTS(SELECT 1 FROM history_records WHERE namespace=?1 OR substr(namespace,1,length(?2))=?2) OR EXISTS(SELECT 1 FROM history_state WHERE state_key=?1 OR substr(state_key,1,length(?2))=?2)", rusqlite::params![namespace, prefix], |row| row.get(0)).map_err(database::sql_error)
}

fn require_remote_source(metadata: &SourceMetadata) -> io::Result<()> {
    if metadata.kind() != SourceKind::Ssh {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "redaction namespace retirement is only valid for SSH sources",
        ));
    }
    Ok(())
}

fn retirement_marker_path(source_directory: &Path) -> PathBuf {
    source_directory.join(RETIREMENT_MARKER_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU32, NonZeroU64};

    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    use crate::remote_protocol::{ProtocolRevisions, SourceGeneration};

    const SOURCE: &str = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 31, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn store() -> (tempfile::TempDir, SourceHistoryStore, NodeId) {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let profile = "profile".parse().unwrap();
        let store = SourceHistoryStore::new(root, profile);
        let source: NodeId = SOURCE.parse().unwrap();
        store
            .save_source_metadata(
                &SourceMetadata::new_with_redaction_profile(
                    source.clone(),
                    SourceKind::Ssh,
                    "remote",
                    RedactionProfile::PreviewEnabled,
                )
                .unwrap(),
            )
            .unwrap();
        (directory, store, source)
    }

    fn install_preview_generation(store: &SourceHistoryStore, source: &NodeId) {
        let generation = "ingest-gen-11111111111111111111111111111111"
            .parse()
            .unwrap();
        let binding = SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: source.clone(),
                generation: NonZeroU64::new(1).unwrap(),
            },
            ProtocolRevisions {
                history_format: NonZeroU32::new(1).unwrap(),
                metric: NonZeroU32::new(1).unwrap(),
                estimator: NonZeroU32::new(1).unwrap(),
                project_breakdown: NonZeroU32::new(1).unwrap(),
                api_pricing_catalog: NonZeroU32::new(1).unwrap(),
                model_catalog_fingerprint: crate::remote_protocol::test_model_catalog_fingerprint(
                    1,
                ),
            },
        )
        .unwrap();
        store
            .ensure_remote_history_generation(
                source,
                RedactionProfile::PreviewEnabled,
                &generation,
                &binding,
            )
            .unwrap();
        store
            .activate_remote_history_generation(
                source,
                RedactionProfile::PreviewEnabled,
                None,
                &generation,
                &binding,
                at(1),
            )
            .unwrap();
    }

    #[test]
    fn queued_marker_never_removes_still_visible_preview_namespace() {
        let (_directory, store, source) = store();
        install_preview_generation(&store, &source);
        store.queue_preview_retirement_for_test(&source).unwrap();
        for _ in 0..2 {
            assert_eq!(
                store
                    .retry_remote_source_redaction_retirement_unfenced(
                        &source,
                        RedactionProfile::Redacted
                    )
                    .unwrap(),
                SourceRedactionRetirementStatus::Pending
            );
            assert_eq!(
                store
                    .load_source_metadata(&source)
                    .unwrap()
                    .aggregate_redaction_profile(),
                RedactionProfile::PreviewEnabled
            );
            assert!(
                store
                    .active_remote_history_ref(&source, RedactionProfile::PreviewEnabled)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn sqlite_retirement_preserves_visible_preview_until_metadata_and_data_commit_together() {
        let (_directory, store, source) = store();
        install_preview_generation(&store, &source);
        store
            .update_source_metadata(&source, |metadata| {
                metadata.set_detached(true);
                metadata.set_include_in_aggregates(false);
                Ok(())
            })
            .unwrap();
        let database = store.sqlite_database().unwrap();
        let preview = database
            .namespace(
                &store
                    .source_directory(&source)
                    .join("preview-enabled/facts/active"),
            )
            .unwrap();
        let redacted = database
            .namespace(
                &store
                    .source_directory(&source)
                    .join("redacted/facts/active"),
            )
            .unwrap();
        let other = database
            .namespace(
                &store
                    .sources_directory()
                    .join("node-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/preview-enabled/facts/active"),
            )
            .unwrap();
        database
            .write(|connection| {
                database::put_record(connection, &preview, "event", 1, &1u64)?;
                database::put_record(connection, &redacted, "event", 1, &2u64)?;
                database::put_record(connection, &other, "event", 1, &3u64)
            })
            .unwrap();
        store.queue_preview_retirement_for_test(&source).unwrap();
        assert_eq!(
            store
                .retry_remote_source_redaction_retirement_unfenced(
                    &source,
                    RedactionProfile::Redacted
                )
                .unwrap(),
            SourceRedactionRetirementStatus::Pending
        );
        assert!(
            store
                .active_remote_history_generation(&source, RedactionProfile::PreviewEnabled)
                .unwrap()
                .is_some()
        );
        database
            .write(|_| {
                store.publish_remote_source_redaction_profile_unfenced(
                    &source,
                    RedactionProfile::Redacted,
                )?;
                Err::<(), _>(io::Error::other("interrupt before privacy commit"))
            })
            .unwrap_err();
        assert_eq!(
            store
                .load_source_metadata(&source)
                .unwrap()
                .aggregate_redaction_profile(),
            RedactionProfile::PreviewEnabled
        );
        assert!(
            store
                .active_remote_history_generation(&source, RedactionProfile::PreviewEnabled)
                .unwrap()
                .is_some()
        );
        let (metadata, status) = store
            .publish_remote_source_redaction_profile_unfenced(&source, RedactionProfile::Redacted)
            .unwrap();
        assert_eq!(status, SourceRedactionRetirementStatus::Complete);
        assert!(metadata.detached());
        assert!(!metadata.include_in_aggregates());
        assert!(
            store
                .active_remote_history_generation(&source, RedactionProfile::PreviewEnabled)
                .unwrap()
                .is_none()
        );
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
                assert!(load(&preview)?.is_empty());
                assert_eq!(load(&redacted)?, vec![2]);
                assert_eq!(load(&other)?, vec![3]);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .retry_remote_source_redaction_retirement_unfenced(
                    &source,
                    RedactionProfile::Redacted
                )
                .unwrap(),
            SourceRedactionRetirementStatus::NotRequired
        );
    }
}
