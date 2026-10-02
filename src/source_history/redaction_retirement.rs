//! Crash-safe retirement of no-longer-visible SSH preview history.
//!
//! SQLite publishes the metadata change and deletes the preview subtree in
//! one transaction. Migration retains a queued marker while preview is still
//! visible. The legacy migration/fixture transition is deliberately ordered as:
//!
//! 1. persist a source-scoped retirement marker;
//! 2. atomically publish `redacted` in `source.json`;
//! 3. rename the old preview namespace to deterministic private trash;
//! 4. remove a bounded number of entries and clear the marker only after the
//!    live and trash namespaces are both absent.
//!
//! A crash at any boundary is therefore retryable without deleting the
//! profile that metadata still exposes. The caller additionally holds the
//! exact remotes-config fence and v2 writer authority; this module holds the
//! stable source lock across publication and namespace isolation.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::*;

const RETIREMENT_MARKER_FILE: &str = "redaction-retirement.json";
const PREVIEW_RETIREMENT_TRASH_DIRECTORY: &str = "preview-enabled.retired-v1.trash";
const RETIREMENT_MARKER_FORMAT_VERSION: u32 = 1;
const MAX_RETIREMENT_TREE_VISITS: usize = 512;
const MAX_RETIREMENT_TREE_REMOVALS: usize = 128;
const MAX_RETIREMENT_TREE_DEPTH: usize = 24;

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
        if let Some(database) = self.store.sqlite_database() {
            return self.fenced(|store| {
                database.write(|_| {
                    let result = store.publish_remote_source_redaction_profile_unfenced(
                        source_id,
                        target_profile,
                    )?;
                    self.validate()?;
                    Ok(result)
                })
            });
        }
        self.fenced(|store| {
            store.publish_remote_source_redaction_profile_unfenced(source_id, target_profile)
        })
    }

    /// Makes one bounded recovery pass for a retirement left by a crash or a
    /// previous sharing/IO failure. It never changes aggregate visibility.
    pub(crate) fn retry_remote_source_redaction_retirement(
        &self,
        source_id: &NodeId,
    ) -> io::Result<SourceRedactionRetirementStatus> {
        if let Some(database) = self.store.sqlite_database() {
            return self.fenced(|store| {
                database.write(|_| {
                    let status = store.retry_remote_source_redaction_retirement_unfenced(
                        source_id,
                        self.redaction_profile(),
                    )?;
                    self.validate()?;
                    Ok(status)
                })
            });
        }
        self.fenced(|store| {
            store.retry_remote_source_redaction_retirement_unfenced(
                source_id,
                self.redaction_profile(),
            )
        })
    }
}

impl SourceHistoryStore {
    /// Carries pending privacy transitions into the same import transaction
    /// as their data. Already-hidden SQL preview rows can be retired there;
    /// the original filesystem namespace remains an explicit backup.
    pub(crate) fn import_legacy_redaction_retirement_sqlite_state(
        &self,
        legacy: &SourceHistoryStore,
    ) -> io::Result<()> {
        if legacy.sqlite_database().is_some() || legacy.profile_id() != self.profile_id() {
            return Err(invalid_data(
                "redaction retirement import backend/profile mismatch",
            ));
        }
        let database = self
            .sqlite_database()
            .ok_or_else(|| invalid_data("redaction retirement import requires SQLite"))?;
        database.write(|connection| {
            let already_imported =
                !database::state_keys(connection, "database/migration/")?.is_empty();
            for source in legacy.list_source_metadata()? {
                if source.kind() != SourceKind::Ssh {
                    continue;
                }
                let directory = legacy.source_directory(source.source_id());
                let marker = read_optional_json_file::<SourceRedactionRetirementMarker>(
                    &retirement_marker_path(&directory),
                    MAX_METADATA_FILE_BYTES,
                )?;
                if let Some(marker) = &marker {
                    marker.validate(legacy.profile_id(), source.source_id())?;
                }
                if already_imported {
                    continue;
                }
                if marker.is_none()
                    && (source.aggregate_redaction_profile() != RedactionProfile::Redacted
                        || !retirement_artifacts_exist(legacy, source.source_id())?)
                {
                    continue;
                }
                let marker = marker.unwrap_or_else(|| {
                    SourceRedactionRetirementMarker::preview_to_redacted(
                        self.profile_id.clone(),
                        source.source_id().clone(),
                    )
                });
                let key = database.namespace(&retirement_marker_path(
                    &self.source_directory(source.source_id()),
                ))?;
                if let Some(existing) =
                    sqlite_retirement_marker(self, connection, source.source_id(), &key)?
                    && existing != marker
                {
                    return Err(invalid_data("redaction retirement import marker conflict"));
                }
                database::set_state(connection, &key, &marker)?;
                if source.aggregate_redaction_profile() == RedactionProfile::Redacted {
                    self.retry_remote_source_redaction_retirement_unfenced(
                        source.source_id(),
                        RedactionProfile::Redacted,
                    )?;
                }
            }
            Ok(())
        })
    }

    fn publish_remote_source_redaction_profile_unfenced(
        &self,
        source_id: &NodeId,
        target_profile: RedactionProfile,
    ) -> io::Result<(SourceMetadata, SourceRedactionRetirementStatus)> {
        if let Some(database) = self.sqlite_database() {
            return database.write(|connection| {
                let mut metadata = self.load_source_metadata(source_id)?;
                require_remote_source(&metadata)?;
                let marker_key = database
                    .namespace(&retirement_marker_path(&self.source_directory(source_id)))?;
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
            });
        }
        let source_directory = self.source_directory(source_id);
        self.validate_private_path(&source_directory)?;
        let lock = open_lock_file(&source_directory, SOURCE_LOCK_FILE)?;
        let _lock = lock_exclusive(lock, &source_directory, SOURCE_LOCK_FILE)?;
        cleanup_retirement_marker_temporaries(self, &source_directory)?;

        let metadata_path = source_directory.join(SOURCE_METADATA_FILE);
        let mut metadata = read_source_metadata_file(&metadata_path, &self.profile_id, source_id)?;
        require_remote_source(&metadata)?;

        if target_profile == RedactionProfile::Redacted {
            let retirement_exists = retirement_artifacts_exist(self, source_id)?;
            if metadata.aggregate_redaction_profile() == RedactionProfile::PreviewEnabled
                || retirement_exists
            {
                ensure_retirement_marker_locked(self, source_id, &source_directory)?;
            }
        }

        if metadata.aggregate_redaction_profile() != target_profile {
            metadata.set_aggregate_redaction_profile(target_profile);
            write_source_metadata_locked(self, &metadata_path, &metadata)?;
        }

        let status = if target_profile == RedactionProfile::Redacted {
            retire_preview_namespace_locked(self, source_id, &metadata, &source_directory)?
        } else {
            SourceRedactionRetirementStatus::NotRequired
        };
        Ok((metadata, status))
    }

    fn retry_remote_source_redaction_retirement_unfenced(
        &self,
        source_id: &NodeId,
        writer_profile: RedactionProfile,
    ) -> io::Result<SourceRedactionRetirementStatus> {
        if writer_profile != RedactionProfile::Redacted {
            return Ok(SourceRedactionRetirementStatus::NotRequired);
        }
        if let Some(database) = self.sqlite_database() {
            return database.write(|connection| {
                let metadata = self.load_source_metadata(source_id)?;
                require_remote_source(&metadata)?;
                let marker_key = database
                    .namespace(&retirement_marker_path(&self.source_directory(source_id)))?;
                let marker = sqlite_retirement_marker(self, connection, source_id, &marker_key)?;
                if metadata.aggregate_redaction_profile() != RedactionProfile::Redacted {
                    return Ok(SourceRedactionRetirementStatus::Pending);
                }
                if marker.is_none()
                    && !sqlite_preview_exists(self, &database, connection, source_id)?
                {
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
            });
        }
        let source_directory = self.source_directory(source_id);
        self.validate_private_path(&source_directory)?;
        let lock = open_lock_file(&source_directory, SOURCE_LOCK_FILE)?;
        let _lock = lock_exclusive(lock, &source_directory, SOURCE_LOCK_FILE)?;
        cleanup_retirement_marker_temporaries(self, &source_directory)?;
        let metadata = read_source_metadata_file(
            &source_directory.join(SOURCE_METADATA_FILE),
            &self.profile_id,
            source_id,
        )?;
        require_remote_source(&metadata)?;
        if metadata.aggregate_redaction_profile() != RedactionProfile::Redacted {
            // A marker can precede the metadata publish when the process dies
            // at the first crash boundary. The still-visible preview profile
            // must remain untouched until a later successful finalize.
            return Ok(SourceRedactionRetirementStatus::Pending);
        }
        if retirement_artifacts_exist(self, source_id)? {
            ensure_retirement_marker_locked(self, source_id, &source_directory)?;
        }
        retire_preview_namespace_locked(self, source_id, &metadata, &source_directory)
    }

    #[cfg(test)]
    pub(crate) fn queue_preview_retirement_for_test(&self, source_id: &NodeId) -> io::Result<()> {
        if let Some(database) = self.sqlite_database() {
            let key =
                database.namespace(&retirement_marker_path(&self.source_directory(source_id)))?;
            return database.write(|connection| {
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
            });
        }
        let source_directory = self.source_directory(source_id);
        self.validate_private_path(&source_directory)?;
        let lock = open_lock_file(&source_directory, SOURCE_LOCK_FILE)?;
        let _lock = lock_exclusive(lock, &source_directory, SOURCE_LOCK_FILE)?;
        ensure_retirement_marker_locked(self, source_id, &source_directory)
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

fn write_source_metadata_locked(
    store: &SourceHistoryStore,
    path: &Path,
    metadata: &SourceMetadata,
) -> io::Result<()> {
    metadata.validate()?;
    let envelope = SourceMetadataEnvelope::new(store.profile_id.clone(), metadata.clone());
    let contents = encode_pretty_bounded(&envelope, MAX_METADATA_FILE_BYTES)?;
    write_private_atomically(path, &contents)
}

fn retirement_marker_path(source_directory: &Path) -> PathBuf {
    source_directory.join(RETIREMENT_MARKER_FILE)
}

fn retirement_trash_path(source_directory: &Path) -> PathBuf {
    source_directory.join(PREVIEW_RETIREMENT_TRASH_DIRECTORY)
}

fn retirement_artifacts_exist(store: &SourceHistoryStore, source_id: &NodeId) -> io::Result<bool> {
    let source_directory = store.source_directory(source_id);
    let marker_exists = match fs::symlink_metadata(retirement_marker_path(&source_directory)) {
        Ok(metadata) => {
            validate_data_file_metadata(&retirement_marker_path(&source_directory), &metadata)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    Ok(marker_exists
        || store.private_directory_exists(
            &source_directory.join(RedactionProfile::PreviewEnabled.directory_name()),
        )?
        || store.private_directory_exists(&retirement_trash_path(&source_directory))?)
}

fn ensure_retirement_marker_locked(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    source_directory: &Path,
) -> io::Result<()> {
    let marker_path = retirement_marker_path(source_directory);
    let expected = SourceRedactionRetirementMarker::preview_to_redacted(
        store.profile_id.clone(),
        source_id.clone(),
    );
    match read_optional_json_file::<SourceRedactionRetirementMarker>(
        &marker_path,
        MAX_METADATA_FILE_BYTES,
    )? {
        Some(marker) => marker.validate(&store.profile_id, source_id),
        None => write_private_atomically(
            &marker_path,
            &encode_pretty_bounded(&expected, MAX_METADATA_FILE_BYTES)?,
        ),
    }
}

fn retire_preview_namespace_locked(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    metadata: &SourceMetadata,
    source_directory: &Path,
) -> io::Result<SourceRedactionRetirementStatus> {
    if metadata.aggregate_redaction_profile() != RedactionProfile::Redacted {
        return Ok(SourceRedactionRetirementStatus::Pending);
    }
    let marker_path = retirement_marker_path(source_directory);
    let live = source_directory.join(RedactionProfile::PreviewEnabled.directory_name());
    let trash = retirement_trash_path(source_directory);
    let marker = read_optional_json_file::<SourceRedactionRetirementMarker>(
        &marker_path,
        MAX_METADATA_FILE_BYTES,
    )?;
    if let Some(marker) = &marker {
        marker.validate(&store.profile_id, source_id)?;
    }
    if marker.is_none()
        && !store.private_directory_exists(&live)?
        && !store.private_directory_exists(&trash)?
    {
        return Ok(SourceRedactionRetirementStatus::NotRequired);
    }
    if marker.is_none() {
        ensure_retirement_marker_locked(store, source_id, source_directory)?;
    }

    let mut budget = RetirementWorkBudget::default();
    if store.private_directory_exists(&trash)?
        && !remove_retirement_tree_bounded(store, &trash, &mut budget, 0)?
    {
        return Ok(SourceRedactionRetirementStatus::Pending);
    }

    if store.private_directory_exists(&live)? {
        store.validate_private_path(&live)?;
        rename_preview_namespace_to_trash(&live, &trash)?;
        sync_directory(source_directory)?;
    }

    if store.private_directory_exists(&trash)?
        && !remove_retirement_tree_bounded(store, &trash, &mut budget, 0)?
    {
        return Ok(SourceRedactionRetirementStatus::Pending);
    }

    if store.private_directory_exists(&live)? || store.private_directory_exists(&trash)? {
        return Ok(SourceRedactionRetirementStatus::Pending);
    }
    remove_retirement_marker_locked(store, &marker_path, source_directory)?;
    Ok(SourceRedactionRetirementStatus::Complete)
}

#[derive(Default)]
struct RetirementWorkBudget {
    visits: usize,
    removals: usize,
}

impl RetirementWorkBudget {
    fn can_visit(&self) -> bool {
        self.visits < MAX_RETIREMENT_TREE_VISITS
    }

    fn can_remove(&self) -> bool {
        self.removals < MAX_RETIREMENT_TREE_REMOVALS
    }

    fn visit(&mut self) {
        self.visits += 1;
    }

    fn removed(&mut self) {
        self.removals += 1;
    }
}

/// Removes at most a fixed number of validated entries. `true` means the
/// supplied root itself is gone; `false` is ordinary durable backpressure and
/// is retried on a later preparation/finalization pass.
fn remove_retirement_tree_bounded(
    store: &SourceHistoryStore,
    directory: &Path,
    budget: &mut RetirementWorkBudget,
    depth: usize,
) -> io::Result<bool> {
    if depth > MAX_RETIREMENT_TREE_DEPTH {
        return Err(invalid_data(
            "retired preview namespace exceeds the cleanup depth bound",
        ));
    }
    store.validate_private_path(directory)?;
    let mut modified = false;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        if !budget.can_visit() {
            if modified {
                sync_directory(directory)?;
            }
            return Ok(false);
        }
        budget.visit();
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err(invalid_data(format!(
                "retired preview cleanup refuses link or reparse point {}",
                child.display()
            )));
        }
        if metadata.file_type().is_dir() {
            ensure_private_directory(&metadata)?;
            if !remove_retirement_tree_bounded(store, &child, budget, depth + 1)? {
                if modified {
                    sync_directory(directory)?;
                }
                return Ok(false);
            }
            modified = true;
            continue;
        }
        validate_data_file_metadata(&child, &metadata)?;
        if !budget.can_remove() {
            if modified {
                sync_directory(directory)?;
            }
            return Ok(false);
        }
        store.validate_private_path(directory)?;
        fs::remove_file(&child)?;
        budget.removed();
        modified = true;
    }

    if modified {
        sync_directory(directory)?;
    }
    if !budget.can_remove() {
        return Ok(false);
    }
    let parent = directory
        .parent()
        .ok_or_else(|| invalid_data("retired preview namespace has no parent"))?;
    store.validate_private_path(parent)?;
    store.validate_private_path(directory)?;
    fs::remove_dir(directory)?;
    budget.removed();
    sync_directory(parent)?;
    Ok(true)
}

fn remove_retirement_marker_locked(
    store: &SourceHistoryStore,
    marker_path: &Path,
    source_directory: &Path,
) -> io::Result<()> {
    match fs::symlink_metadata(marker_path) {
        Ok(metadata) => {
            validate_data_file_metadata(marker_path, &metadata)?;
            store.validate_private_path(source_directory)?;
            fs::remove_file(marker_path)?;
            sync_directory(source_directory)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn cleanup_retirement_marker_temporaries(
    store: &SourceHistoryStore,
    source_directory: &Path,
) -> io::Result<()> {
    store.validate_private_path(source_directory)?;
    let mut removed = false;
    for entry in fs::read_dir(source_directory)? {
        store.validate_private_path(source_directory)?;
        let entry = entry?;
        if atomic_temporary_target_name(&entry.file_name()) != Some(RETIREMENT_MARKER_FILE) {
            continue;
        }
        validate_published_private_file(&entry.path())?;
        fs::remove_file(entry.path())?;
        removed = true;
    }
    if removed {
        sync_directory(source_directory)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn rename_preview_namespace_to_trash(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn rename_preview_namespace_to_trash(source: &Path, destination: &Path) -> io::Result<()> {
    use crate::atomic_file::windows_wide_path;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source = windows_wide_path(source)?;
    let destination = windows_wide_path(destination)?;
    // SAFETY: both buffers are NUL-terminated and remain alive for the call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

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
        store_backend(false)
    }

    fn store_backend(sqlite: bool) -> (tempfile::TempDir, SourceHistoryStore, NodeId) {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let profile = "profile".parse().unwrap();
        let store = if sqlite {
            SourceHistoryStore::new_sqlite(root, profile)
        } else {
            SourceHistoryStore::new(root, profile)
        };
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

        let status = store
            .retry_remote_source_redaction_retirement_unfenced(&source, RedactionProfile::Redacted)
            .unwrap();

        assert_eq!(status, SourceRedactionRetirementStatus::Pending);
        assert_eq!(
            store
                .load_source_metadata(&source)
                .unwrap()
                .aggregate_redaction_profile(),
            RedactionProfile::PreviewEnabled
        );
        assert!(
            store
                .source_directory(&source)
                .join(RedactionProfile::PreviewEnabled.directory_name())
                .is_dir()
        );
    }

    #[test]
    fn redacted_publish_adopts_and_removes_legacy_preview_namespace() {
        let (_directory, store, source) = store();
        install_preview_generation(&store, &source);

        let (metadata, status) = store
            .publish_remote_source_redaction_profile_unfenced(&source, RedactionProfile::Redacted)
            .unwrap();

        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::Redacted
        );
        assert_eq!(status, SourceRedactionRetirementStatus::Complete);
        let source_directory = store.source_directory(&source);
        assert!(!source_directory.join("preview-enabled").exists());
        assert!(!retirement_trash_path(&source_directory).exists());
        assert!(!retirement_marker_path(&source_directory).exists());
    }

    #[test]
    fn bounded_cleanup_is_resumable_and_eventually_clears_marker() {
        let (_directory, store, source) = store();
        let preview = store
            .source_directory(&source)
            .join(RedactionProfile::PreviewEnabled.directory_name());
        store.prepare_private_directory(&preview).unwrap();
        for index in 0..(MAX_RETIREMENT_TREE_REMOVALS + 5) {
            let shard = preview.join(format!("shard-{index}.json"));
            fs::write(&shard, b"{}\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&shard, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }

        let (_, first) = store
            .publish_remote_source_redaction_profile_unfenced(&source, RedactionProfile::Redacted)
            .unwrap();
        assert_eq!(first, SourceRedactionRetirementStatus::Pending);
        assert!(
            retirement_marker_path(&store.source_directory(&source)).is_file(),
            "a bounded partial pass must retain its durable marker"
        );

        let second = store
            .retry_remote_source_redaction_retirement_unfenced(&source, RedactionProfile::Redacted)
            .unwrap();
        assert_eq!(second, SourceRedactionRetirementStatus::Complete);
        assert!(!retirement_marker_path(&store.source_directory(&source)).exists());
    }

    #[test]
    fn retirement_waits_for_metadata_dependent_source_reader() {
        let (_directory, store, source) = store();
        install_preview_generation(&store, &source);
        let reader_store = store.clone();
        let reader_source = source.clone();
        let (reader_locked_tx, reader_locked_rx) = mpsc::channel();
        let (release_reader_tx, release_reader_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            reader_store
                .with_source_metadata_shared(&reader_source, |_| {
                    reader_locked_tx.send(()).unwrap();
                    release_reader_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });
        reader_locked_rx.recv().unwrap();

        let retiring_store = store.clone();
        let retiring_source = source.clone();
        let (retired_tx, retired_rx) = mpsc::channel();
        let retire = thread::spawn(move || {
            let result = retiring_store.publish_remote_source_redaction_profile_unfenced(
                &retiring_source,
                RedactionProfile::Redacted,
            );
            retired_tx.send(result).unwrap();
        });
        assert!(
            retired_rx
                .recv_timeout(StdDuration::from_millis(100))
                .is_err(),
            "an in-flight metadata-dependent reader must retain the preview namespace"
        );
        release_reader_tx.send(()).unwrap();
        reader.join().unwrap();
        let (_, status) = retired_rx.recv().unwrap().unwrap();
        assert_eq!(status, SourceRedactionRetirementStatus::Complete);
        retire.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retirement_refuses_symlink_inside_isolated_trash() {
        use std::os::unix::fs::symlink;

        let (_directory, store, source) = store();
        let preview = store
            .source_directory(&source)
            .join(RedactionProfile::PreviewEnabled.directory_name());
        store.prepare_private_directory(&preview).unwrap();
        symlink(store.state_root(), preview.join("escape")).unwrap();

        let error = store
            .publish_remote_source_redaction_profile_unfenced(&source, RedactionProfile::Redacted)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            store
                .load_source_metadata(&source)
                .unwrap()
                .aggregate_redaction_profile(),
            RedactionProfile::Redacted,
            "visibility changes before best-effort physical retirement"
        );
        assert!(retirement_marker_path(&store.source_directory(&source)).exists());
    }

    #[test]
    fn sqlite_retirement_preserves_visible_preview_until_metadata_and_data_commit_together() {
        let (_directory, store, source) = store_backend(true);
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

    #[test]
    fn sqlite_migration_carries_retirement_intent_without_deleting_still_visible_preview() {
        for profile in [RedactionProfile::PreviewEnabled, RedactionProfile::Redacted] {
            let (_directory, legacy, source) = store();
            legacy.queue_preview_retirement_for_test(&source).unwrap();
            legacy
                .update_source_metadata(&source, |metadata| {
                    metadata.set_aggregate_redaction_profile(profile);
                    Ok(())
                })
                .unwrap();
            let marker_path = retirement_marker_path(&legacy.source_directory(&source));
            let marker_bytes = fs::read(&marker_path).unwrap();
            let target = SourceHistoryStore::new_sqlite(
                legacy.state_root().to_owned(),
                legacy.profile_id().clone(),
            );
            target
                .save_source_metadata(&legacy.load_source_metadata(&source).unwrap())
                .unwrap();
            install_preview_generation(&target, &source);
            target
                .import_legacy_redaction_retirement_sqlite_state(&legacy)
                .unwrap();
            assert_eq!(
                target
                    .load_source_metadata(&source)
                    .unwrap()
                    .aggregate_redaction_profile(),
                profile
            );
            assert_eq!(
                target
                    .active_remote_history_generation(&source, RedactionProfile::PreviewEnabled)
                    .unwrap()
                    .is_some(),
                profile == RedactionProfile::PreviewEnabled
            );
            assert_eq!(
                target
                    .retry_remote_source_redaction_retirement_unfenced(
                        &source,
                        RedactionProfile::Redacted
                    )
                    .unwrap(),
                if profile == RedactionProfile::PreviewEnabled {
                    SourceRedactionRetirementStatus::Pending
                } else {
                    SourceRedactionRetirementStatus::NotRequired
                }
            );
            if profile == RedactionProfile::Redacted {
                target
                    .sqlite_database()
                    .unwrap()
                    .write(|connection| {
                        database::set_state(connection, "database/migration/test-receipt", &true)
                    })
                    .unwrap();
                target
                    .update_source_metadata(&source, |metadata| {
                        metadata.set_aggregate_redaction_profile(RedactionProfile::PreviewEnabled);
                        Ok(())
                    })
                    .unwrap();
                install_preview_generation(&target, &source);
                target
                    .import_legacy_redaction_retirement_sqlite_state(&legacy)
                    .unwrap();
                assert_eq!(
                    target
                        .load_source_metadata(&source)
                        .unwrap()
                        .aggregate_redaction_profile(),
                    RedactionProfile::PreviewEnabled
                );
                assert!(
                    target
                        .active_remote_history_generation(&source, RedactionProfile::PreviewEnabled)
                        .unwrap()
                        .is_some()
                );
            }
            assert_eq!(fs::read(marker_path).unwrap(), marker_bytes);
        }
    }
}
