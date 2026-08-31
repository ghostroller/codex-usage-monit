//! Explicit, crash-recoverable destruction of one detached SSH source.
//!
//! The source directory is first claimed with a durable marker and then
//! renamed to a deterministic sibling trash directory. The rename is the
//! publication boundary: readers can no longer discover the source, while a
//! restart can validate the marker in trash and finish removing it. The
//! account namespace and every other source are outside the claimed subtree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::*;

const SOURCE_PURGE_MARKER_FILE: &str = "source-purge.json";
const SOURCE_PURGE_MARKER_FORMAT_VERSION: u32 = 1;
const SOURCE_PURGE_TRASH_PREFIX: &str = ".source-purge-";
const SOURCE_PURGE_TRASH_SUFFIX: &str = ".trash";
const MAX_SOURCE_PURGE_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceHistoryPurgeReport {
    resumed_from_trash: bool,
}

impl SourceHistoryPurgeReport {
    pub fn resumed_from_trash(self) -> bool {
        self.resumed_from_trash
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourcePurgeMarker {
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

    fn validate(&self, profile_id: &HistoryProfileId, source_id: &NodeId) -> io::Result<()> {
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
        self.fenced(|store| store.prepare_detached_ssh_source_for_purge_unfenced(source_id))
    }

    /// Irreversibly removes one detached SSH source and no other history
    /// namespace. The caller must separately fence the remotes allowlist.
    pub(crate) fn purge_detached_ssh_source(
        &self,
        source_id: &NodeId,
    ) -> io::Result<SourceHistoryPurgeReport> {
        self.fenced(|store| store.purge_detached_ssh_source_unfenced(source_id))
    }
}

impl SourceHistoryStore {
    /// Fences re-pairing against an irreversible purge that was durably
    /// claimed before a crash. Callers that mutate the allowlist must hold the
    /// remotes config lock before entering this source lock.
    pub(crate) fn ensure_source_not_pending_purge(&self, source_id: &NodeId) -> io::Result<()> {
        let source = self.source_directory(source_id);
        let trash = source_purge_trash_path(self, source_id);
        if checked_directory_exists(self, &trash, "source purge trash")? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote source cannot be paired while an irreversible purge is pending",
            ));
        }
        if !checked_directory_exists(self, &source, "source purge source")? {
            return Ok(());
        }
        let lock = open_lock_file(&source, SOURCE_LOCK_FILE)?;
        lock_shared(&lock, &source, SOURCE_LOCK_FILE)?;
        reject_source_metadata_update_during_purge(self, &source, source_id)
    }

    fn prepare_detached_ssh_source_for_purge_unfenced(&self, source_id: &NodeId) -> io::Result<()> {
        let source = self.source_directory(source_id);
        let trash = source_purge_trash_path(self, source_id);
        let source_exists = checked_directory_exists(self, &source, "source purge source")?;
        let trash_exists = checked_directory_exists(self, &trash, "source purge trash")?;
        match (source_exists, trash_exists) {
            (true, true) => Err(invalid_data(
                "source purge found both the live source and its recovery trash",
            )),
            (true, false) => {
                let lock = open_lock_file(&source, SOURCE_LOCK_FILE)?;
                lock_exclusive(&lock, &source, SOURCE_LOCK_FILE)?;
                let metadata = read_source_metadata_file(
                    &source.join(SOURCE_METADATA_FILE),
                    self.profile_id(),
                    source_id,
                )?;
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
                validate_live_source_layout(self, &source, source_id)?;
                validate_purge_tree(self, &source, 0)?;
                let marker_path = source.join(SOURCE_PURGE_MARKER_FILE);
                if marker_path.exists() {
                    validate_purge_marker(self, &source, source_id)?;
                } else {
                    let marker =
                        SourcePurgeMarker::ssh(self.profile_id().clone(), source_id.clone());
                    let contents = encode_pretty_bounded(&marker, MAX_METADATA_FILE_BYTES)?;
                    write_private_atomically(&marker_path, &contents)?;
                }
                validate_live_source_layout(self, &source, source_id)?;
                validate_purge_tree(self, &source, 0)
            }
            (false, true) if purge_trash_is_empty(self, &trash)? => Ok(()),
            (false, true) => {
                validate_purge_trash_layout(self, &trash, source_id)?;
                validate_purge_tree(self, &trash, 0)
            }
            (false, false) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("remote source {source_id} has no retained history"),
            )),
        }
    }

    fn purge_detached_ssh_source_unfenced(
        &self,
        source_id: &NodeId,
    ) -> io::Result<SourceHistoryPurgeReport> {
        let source = self.source_directory(source_id);
        let trash = source_purge_trash_path(self, source_id);
        let source_exists = checked_directory_exists(self, &source, "source purge source")?;
        let trash_exists = checked_directory_exists(self, &trash, "source purge trash")?;

        if source_exists && trash_exists {
            return Err(invalid_data(
                "source purge found both the live source and its recovery trash",
            ));
        }
        if trash_exists {
            if purge_trash_is_empty(self, &trash)? {
                fs::remove_dir(&trash)?;
                sync_directory(&self.sources_directory())?;
                return Ok(SourceHistoryPurgeReport {
                    resumed_from_trash: true,
                });
            }
            validate_purge_trash_layout(self, &trash, source_id)?;
            validate_purge_tree(self, &trash, 0)?;
            remove_purge_tree(self, &trash, 0)?;
            return Ok(SourceHistoryPurgeReport {
                resumed_from_trash: true,
            });
        }
        if !source_exists {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("remote source {source_id} has no retained history"),
            ));
        }

        let lock = open_lock_file(&source, SOURCE_LOCK_FILE)?;
        lock_exclusive(&lock, &source, SOURCE_LOCK_FILE)?;
        let metadata = read_source_metadata_file(
            &source.join(SOURCE_METADATA_FILE),
            self.profile_id(),
            source_id,
        )?;
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

        validate_live_source_layout(self, &source, source_id)?;
        validate_purge_tree(self, &source, 0)?;
        let marker = SourcePurgeMarker::ssh(self.profile_id().clone(), source_id.clone());
        let marker_path = source.join(SOURCE_PURGE_MARKER_FILE);
        if marker_path.exists() {
            validate_purge_marker(self, &source, source_id)?;
        } else {
            let contents = encode_pretty_bounded(&marker, MAX_METADATA_FILE_BYTES)?;
            write_private_atomically(&marker_path, &contents)?;
        }
        validate_live_source_layout(self, &source, source_id)?;
        validate_purge_tree(self, &source, 0)?;

        // Stable history locks do not share delete access on Windows. Release
        // the lock before the same-parent rename; config + writer fencing keeps
        // cooperative writers out, and a racing reader can only make rename
        // fail without deleting anything.
        fs2::FileExt::unlock(&lock)?;
        drop(lock);
        self.validate_private_path(&source)?;
        rename_purge_namespace(&source, &trash)?;
        sync_directory(&self.sources_directory())?;

        validate_purge_trash_layout(self, &trash, source_id)?;
        validate_purge_tree(self, &trash, 0)?;
        remove_purge_tree(self, &trash, 0)?;
        Ok(SourceHistoryPurgeReport {
            resumed_from_trash: false,
        })
    }
}

fn source_purge_trash_path(store: &SourceHistoryStore, source_id: &NodeId) -> PathBuf {
    store.sources_directory().join(format!(
        "{SOURCE_PURGE_TRASH_PREFIX}{source_id}{SOURCE_PURGE_TRASH_SUFFIX}"
    ))
}

#[cfg(not(windows))]
fn rename_purge_namespace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn rename_purge_namespace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows paths cannot contain NUL characters",
            ));
        }
        encoded.push(0);
        Ok(encoded)
    }

    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    // Omitting MOVEFILE_REPLACE_EXISTING is part of the fail-closed claim.
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

fn checked_directory_exists(
    store: &SourceHistoryStore,
    path: &Path,
    label: &str,
) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
                return Err(invalid_data(format!(
                    "{label} {} must be a real directory",
                    path.display()
                )));
            }
            store.validate_private_path(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn validate_live_source_layout(
    store: &SourceHistoryStore,
    directory: &Path,
    source_id: &NodeId,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    let mut metadata_seen = false;
    let mut lock_seen = false;
    let mut purge_marker_seen = false;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        let target = atomic_temporary_target_name(&name);
        let known_file = match name.to_str() {
            Some(SOURCE_METADATA_FILE) => {
                metadata_seen = true;
                true
            }
            Some(SOURCE_LOCK_FILE) => {
                lock_seen = true;
                true
            }
            Some("redaction-retirement.json") => true,
            Some(SOURCE_PURGE_MARKER_FILE) => {
                purge_marker_seen = true;
                true
            }
            _ if matches!(
                target,
                Some(SOURCE_METADATA_FILE | "redaction-retirement.json" | SOURCE_PURGE_MARKER_FILE)
            ) =>
            {
                true
            }
            _ => false,
        };
        let known_directory = matches!(
            name.to_str(),
            Some("redacted" | "preview-enabled" | "preview-enabled.retired-v1.trash")
        );
        let entry_metadata = fs::symlink_metadata(&path)?;
        if metadata_is_link_or_reparse(&entry_metadata) {
            return Err(invalid_data(format!(
                "source purge refuses link or reparse point {}",
                path.display()
            )));
        }
        if known_file {
            validate_data_file_metadata(&path, &entry_metadata)?;
        } else if known_directory {
            if !entry_metadata.file_type().is_dir() {
                return Err(invalid_data(format!(
                    "source purge expected a directory at {}",
                    path.display()
                )));
            }
            store.validate_private_path(&path)?;
        } else {
            return Err(invalid_data(format!(
                "source purge refuses unexpected source layout entry {}",
                path.display()
            )));
        }
    }
    if !metadata_seen || !lock_seen {
        return Err(invalid_data(
            "source purge source is missing source.json or source.lock",
        ));
    }
    if purge_marker_seen {
        validate_purge_marker(store, directory, source_id)?;
    }
    let metadata = read_source_metadata_file(
        &directory.join(SOURCE_METADATA_FILE),
        store.profile_id(),
        source_id,
    )?;
    if metadata.kind() != SourceKind::Ssh || !metadata.detached() {
        return Err(invalid_data(
            "source purge source metadata is no longer a detached SSH source",
        ));
    }
    Ok(())
}

fn validate_purge_trash_layout(
    store: &SourceHistoryStore,
    directory: &Path,
    source_id: &NodeId,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    let mut marker_seen = false;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        let target = atomic_temporary_target_name(&name);
        let known_file = match name.to_str() {
            Some(SOURCE_METADATA_FILE | SOURCE_LOCK_FILE | "redaction-retirement.json") => true,
            Some(SOURCE_PURGE_MARKER_FILE) => {
                marker_seen = true;
                true
            }
            _ if matches!(
                target,
                Some(SOURCE_METADATA_FILE | "redaction-retirement.json" | SOURCE_PURGE_MARKER_FILE)
            ) =>
            {
                true
            }
            _ => false,
        };
        let known_directory = matches!(
            name.to_str(),
            Some("redacted" | "preview-enabled" | "preview-enabled.retired-v1.trash")
        );
        let entry_metadata = fs::symlink_metadata(&path)?;
        if metadata_is_link_or_reparse(&entry_metadata) {
            return Err(invalid_data(format!(
                "source purge refuses link or reparse point {}",
                path.display()
            )));
        }
        if known_file {
            validate_data_file_metadata(&path, &entry_metadata)?;
        } else if known_directory {
            if !entry_metadata.file_type().is_dir() {
                return Err(invalid_data(format!(
                    "source purge expected a directory at {}",
                    path.display()
                )));
            }
            store.validate_private_path(&path)?;
        } else {
            return Err(invalid_data(format!(
                "source purge refuses unexpected recovery layout entry {}",
                path.display()
            )));
        }
    }
    if !marker_seen {
        return Err(invalid_data(
            "source purge recovery trash is missing its durable marker",
        ));
    }
    validate_purge_marker(store, directory, source_id)?;
    match read_source_metadata_file(
        &directory.join(SOURCE_METADATA_FILE),
        store.profile_id(),
        source_id,
    ) {
        Ok(metadata) if metadata.kind() == SourceKind::Ssh && metadata.detached() => Ok(()),
        Ok(_) => Err(invalid_data(
            "source purge recovery metadata is not a detached SSH source",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn purge_trash_is_empty(store: &SourceHistoryStore, directory: &Path) -> io::Result<bool> {
    store.validate_private_path(directory)?;
    Ok(fs::read_dir(directory)?.next().transpose()?.is_none())
}

fn validate_purge_marker(
    store: &SourceHistoryStore,
    directory: &Path,
    source_id: &NodeId,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    let marker: SourcePurgeMarker = read_json_file(
        &directory.join(SOURCE_PURGE_MARKER_FILE),
        MAX_METADATA_FILE_BYTES,
    )?;
    marker.validate(store.profile_id(), source_id)
}

pub(super) fn reject_source_metadata_update_during_purge(
    store: &SourceHistoryStore,
    directory: &Path,
    source_id: &NodeId,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let name = entry.file_name();
        let marker = name == OsStr::new(SOURCE_PURGE_MARKER_FILE);
        let marker_temporary = atomic_temporary_target_name(&name)
            .is_some_and(|target| target == SOURCE_PURGE_MARKER_FILE);
        if !marker && !marker_temporary {
            continue;
        }
        let path = entry.path();
        validate_data_file_metadata(&path, &fs::symlink_metadata(&path)?)?;
        if marker {
            validate_purge_marker(store, directory, source_id)?;
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "source metadata cannot change while an irreversible purge is pending",
        ));
    }
    Ok(())
}

fn validate_purge_tree(
    store: &SourceHistoryStore,
    directory: &Path,
    depth: usize,
) -> io::Result<()> {
    if depth > MAX_SOURCE_PURGE_DEPTH {
        return Err(invalid_data("source purge tree exceeds its depth bound"));
    }
    store.validate_private_path(directory)?;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err(invalid_data(format!(
                "source purge refuses link or reparse point {}",
                path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            ensure_private_directory(&metadata)?;
            validate_purge_tree(store, &path, depth + 1)?;
        } else {
            validate_data_file_metadata(&path, &metadata)?;
        }
    }
    Ok(())
}

fn remove_purge_tree(store: &SourceHistoryStore, directory: &Path, depth: usize) -> io::Result<()> {
    if depth > MAX_SOURCE_PURGE_DEPTH {
        return Err(invalid_data("source purge tree exceeds its depth bound"));
    }
    store.validate_private_path(directory)?;
    let mut root_marker = None;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        if depth == 0 && entry.file_name() == OsStr::new(SOURCE_PURGE_MARKER_FILE) {
            root_marker = Some(path);
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err(invalid_data(format!(
                "source purge refuses link or reparse point {}",
                path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            ensure_private_directory(&metadata)?;
            remove_purge_tree(store, &path, depth + 1)?;
        } else {
            validate_data_file_metadata(&path, &metadata)?;
            fs::remove_file(&path)?;
        }
    }
    if let Some(marker) = root_marker {
        // Persist removal of every ordinary entry while the durable recovery
        // marker still exists. After a power loss, the namespace is therefore
        // either recoverable from that marker or already empty except for it.
        sync_directory(directory)?;
        validate_data_file_metadata(&marker, &fs::symlink_metadata(&marker)?)?;
        fs::remove_file(marker)?;
        // Persist the marker unlink as a separate phase before removing the
        // now-empty root namespace.
        sync_directory(directory)?;
    } else {
        sync_directory(directory)?;
    }
    let parent = directory
        .parent()
        .ok_or_else(|| invalid_data("source purge directory has no parent"))?;
    store.validate_private_path(parent)?;
    fs::remove_dir(directory)?;
    sync_directory(parent)
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
        assert!(store.source_directory(&remote).is_dir());

        let local: NodeId = OTHER.parse().unwrap();
        save_source(&store, &local, SourceKind::Local, true);
        let error = store
            .purge_detached_ssh_source_unfenced(&local)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(store.source_directory(&local).is_dir());
    }

    #[test]
    fn purge_refuses_unknown_source_layout() {
        let (_directory, store, remote) = fixture();
        save_source(&store, &remote, SourceKind::Ssh, true);
        let unexpected = store.source_directory(&remote).join("unexpected");
        fs::write(&unexpected, b"do not delete").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&unexpected, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = store
            .purge_detached_ssh_source_unfenced(&remote)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(unexpected.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn purge_refuses_symlink_anywhere_in_source_tree() {
        use std::os::unix::fs::symlink;

        let (directory, store, remote) = fixture();
        save_source(&store, &remote, SourceKind::Ssh, true);
        let redacted = store
            .source_directory(&remote)
            .join(RedactionProfile::Redacted.directory_name());
        store.prepare_private_directory(&redacted).unwrap();
        symlink(directory.path(), redacted.join("escape")).unwrap();
        let error = store
            .purge_detached_ssh_source_unfenced(&remote)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(store.source_directory(&remote).is_dir());
        assert!(
            !store
                .source_directory(&remote)
                .join(SOURCE_PURGE_MARKER_FILE)
                .exists()
        );
    }

    #[test]
    fn purge_removes_only_selected_source_and_stays_gone_after_restart() {
        let (_directory, store, remote) = fixture();
        save_source(&store, &remote, SourceKind::Ssh, true);
        let other: NodeId = OTHER.parse().unwrap();
        save_source(&store, &other, SourceKind::Ssh, true);
        store
            .prepare_private_directory(&store.account_directory())
            .unwrap();
        let account_sentinel = store.account_directory().join("keep");
        fs::write(&account_sentinel, b"account").unwrap();

        let report = store.purge_detached_ssh_source_unfenced(&remote).unwrap();
        assert!(!report.resumed_from_trash());
        assert!(!store.source_directory(&remote).exists());
        assert!(store.source_directory(&other).is_dir());
        assert!(account_sentinel.is_file());

        let restarted =
            SourceHistoryStore::new(store.state_root().to_path_buf(), store.profile_id().clone());
        assert_eq!(
            restarted
                .list_source_metadata()
                .unwrap()
                .into_iter()
                .map(|source| source.source_id().clone())
                .collect::<Vec<_>>(),
            vec![other]
        );
        assert!(account_sentinel.is_file());
    }

    #[test]
    fn purge_resumes_a_claimed_trash_namespace_after_restart() {
        let (_directory, store, remote) = fixture();
        save_source(&store, &remote, SourceKind::Ssh, true);
        let source = store.source_directory(&remote);
        let marker = SourcePurgeMarker::ssh(store.profile_id().clone(), remote.clone());
        let contents = encode_pretty_bounded(&marker, MAX_METADATA_FILE_BYTES).unwrap();
        write_private_atomically(&source.join(SOURCE_PURGE_MARKER_FILE), &contents).unwrap();
        let trash = source_purge_trash_path(&store, &remote);
        fs::rename(&source, &trash).unwrap();
        fs::remove_file(trash.join(SOURCE_METADATA_FILE)).unwrap();

        let restarted =
            SourceHistoryStore::new(store.state_root().to_path_buf(), store.profile_id().clone());
        let report = restarted
            .purge_detached_ssh_source_unfenced(&remote)
            .unwrap();
        assert!(report.resumed_from_trash());
        assert!(!trash.exists());
        assert!(!source.exists());
    }

    #[test]
    fn durable_purge_claim_blocks_metadata_reattach_before_related_cleanup() {
        let (_directory, store, remote) = fixture();
        save_source(&store, &remote, SourceKind::Ssh, true);

        store
            .prepare_detached_ssh_source_for_purge_unfenced(&remote)
            .unwrap();
        assert!(
            store
                .source_directory(&remote)
                .join(SOURCE_PURGE_MARKER_FILE)
                .is_file()
        );
        let error = store
            .update_source_metadata(&remote, |metadata| {
                metadata.set_detached(false);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(store.load_source_metadata(&remote).unwrap().detached());

        store.purge_detached_ssh_source_unfenced(&remote).unwrap();
        assert!(!store.source_directory(&remote).exists());
    }
}
