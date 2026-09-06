//! Shared, security-sensitive filesystem primitives for small private stores.
//!
//! This module deliberately owns only the storage envelope: private directory
//! validation, no-follow opens, stable file identity checks, advisory locks,
//! bounded reads, and durable atomic replacement. Each caller retains its own
//! schema validation and mutation/CAS rules.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::atomic_file::replace_file;
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

const TEMP_FILE_ATTEMPTS: usize = 128;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LockFilePolicy {
    Create,
    Existing,
}

/// Names and limits that keep diagnostics specific to the owning store while
/// sharing the exact same hardened filesystem implementation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PrivateStoreLayout {
    pub(crate) store_name: &'static str,
    pub(crate) data_file_name: &'static str,
    pub(crate) data_path_name: &'static str,
    pub(crate) data_subject: &'static str,
    pub(crate) lock_file_name: &'static str,
    pub(crate) lock_subject: &'static str,
    pub(crate) temporary_subject: &'static str,
    pub(crate) maximum_file_bytes: u64,
}

impl PrivateStoreLayout {
    pub(crate) fn create_directory_beneath(&self, root: &Path, path: &Path) -> io::Result<()> {
        self.ensure_absolute_root(root)?;
        match self.validate_state_root(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.create_state_root(root)?;
            }
            Err(error) => return Err(error),
        }
        let relative = path.strip_prefix(root).map_err(|_| {
            invalid_input(format!(
                "{} path is outside its state root",
                self.store_name
            ))
        })?;
        let mut current = root.to_path_buf();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(invalid_input(format!(
                    "{} path contains a non-normal component",
                    self.store_name
                )));
            };
            current.push(name);
            match self.validate_private_directory(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.create_private_child_directory(&current)?;
                }
                Err(error) => return Err(error),
            }
        }
        self.validate_private_directory(path)
    }

    pub(crate) fn directory_exists_beneath(&self, root: &Path, path: &Path) -> io::Result<bool> {
        self.ensure_absolute_root(root)?;
        match self.validate_state_root(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        let relative = path.strip_prefix(root).map_err(|_| {
            invalid_input(format!(
                "{} path is outside its state root",
                self.store_name
            ))
        })?;
        let mut current = root.to_path_buf();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(invalid_input(format!(
                    "{} path contains a non-normal component",
                    self.store_name
                )));
            };
            current.push(name);
            match self.validate_private_directory(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    pub(crate) fn data_file_exists(&self, path: &Path) -> io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                validate_private_file_metadata(&metadata, self.data_subject)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Checks only for path presence. The subsequent bounded read performs
    /// all file-kind, permission, no-follow, and identity validation. Keeping
    /// this separate from `data_file_exists` preserves callers whose lock is
    /// intentionally acquired before validating the data file itself.
    pub(crate) fn data_path_exists(&self, path: &Path) -> io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn read_bounded(&self, path: &Path) -> io::Result<Vec<u8>> {
        read_private_bounded(path, self.maximum_file_bytes, self.data_subject)
    }

    pub(crate) fn open_lock(
        &self,
        directory: &Path,
        mode: LockMode,
        policy: LockFilePolicy,
    ) -> io::Result<File> {
        self.validate_private_directory(directory)?;
        let path = directory.join(self.lock_file_name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => validate_private_file_metadata(&metadata, self.lock_subject)?,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && matches!(policy, LockFilePolicy::Create) => {}
            Err(error) => return Err(error),
        }

        let mut options = OpenOptions::new();
        match policy {
            LockFilePolicy::Create => {
                options.read(true).write(true).create(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
            }
            LockFilePolicy::Existing => {
                options.read(true);
            }
        }
        add_nofollow_flags(&mut options);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(stable_lock_share_mode());
        }
        let file = options
            .open(&path)
            .map_err(|error| map_nofollow_error(error, self.lock_subject))?;
        validate_opened_private_file(&path, &file, self.lock_subject)?;

        match mode {
            LockMode::Shared => fs2::FileExt::lock_shared(&file)?,
            LockMode::Exclusive => fs2::FileExt::lock_exclusive(&file)?,
        }

        // Revalidate both the containing directory and the opened path after
        // lock acquisition. This ordering is intentional: a replacement that
        // raced the open must not be trusted merely because the old inode was
        // successfully locked.
        self.validate_private_directory(directory)?;
        validate_opened_private_file(&path, &file, self.lock_subject)?;
        Ok(file)
    }

    pub(crate) fn write_atomically(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid_input(format!("{} path has no parent", self.data_path_name)))?;
        self.validate_private_directory(parent)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) => validate_private_file_metadata(&metadata, self.data_subject)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let (temporary_path, mut temporary) = self.create_temporary_file(parent)?;
        let result = (|| {
            temporary.write_all(contents)?;
            temporary.sync_all()?;
            drop(temporary);
            replace_file(&temporary_path, path)?;
            validate_published_private_file(path, self.data_subject)?;
            sync_directory(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }

    pub(crate) fn validate_state_root(&self, path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let subject = format!("{} state root", self.store_name);
        if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
            return Err(invalid_data(format!("{subject} must be a real directory")));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            // SAFETY: geteuid has no preconditions and retains no pointers.
            let effective_uid = unsafe { libc::geteuid() };
            if metadata.uid() != effective_uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{subject} must be owned by the current user"),
                ));
            }
            let mode = metadata.permissions().mode() & 0o777;
            if mode != 0o700 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{subject} must have mode 0700 (found {mode:04o})"),
                ));
            }
        }
        #[cfg(windows)]
        validate_windows_private_directory(path, &subject)?;
        Ok(())
    }

    pub(crate) fn validate_private_directory(&self, path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let directory_subject = format!("{} directory", self.store_name);
        if metadata_is_link_or_reparse(&metadata) {
            return Err(invalid_data(format!(
                "{directory_subject} must not be a symbolic link or reparse point"
            )));
        }
        if !metadata.file_type().is_dir() {
            return Err(invalid_data(format!(
                "{} path must be a directory",
                self.store_name
            )));
        }
        ensure_private_path(&metadata, &directory_subject)?;
        #[cfg(windows)]
        validate_windows_private_directory(path, &directory_subject)?;
        Ok(())
    }

    fn ensure_absolute_root(&self, root: &Path) -> io::Result<()> {
        if !root.is_absolute() {
            return Err(invalid_input(format!(
                "{} state root must be absolute",
                self.store_name
            )));
        }
        Ok(())
    }

    fn create_state_root(&self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(path)?;
        self.validate_state_root(path)
    }

    fn create_private_child_directory(&self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        #[cfg(not(unix))]
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        self.validate_private_directory(path)
    }

    fn create_temporary_file(&self, parent: &Path) -> io::Result<(PathBuf, File)> {
        for _ in 0..TEMP_FILE_ATTEMPTS {
            let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".{}.{}.{}.tmp",
                self.data_file_name,
                std::process::id(),
                sequence
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            add_nofollow_flags(&mut options);
            match options.open(&path) {
                Ok(file) => {
                    validate_opened_private_file(&path, &file, self.temporary_subject)?;
                    return Ok((path, file));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("could not allocate a {} temporary file", self.store_name),
        ))
    }
}

fn read_private_bounded(path: &Path, maximum: u64, subject: &str) -> io::Result<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    let metadata = file.metadata()?;
    validate_private_file_metadata(&metadata, subject)?;
    ensure_private_file(path, &file, &metadata, subject)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &metadata, subject)?;
    if metadata.len() > maximum {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    let initial_capacity = usize::try_from(metadata.len())
        .map_err(|_| invalid_data(format!("{subject} is too large for this platform")))?;
    let mut contents = Vec::new();
    contents
        .try_reserve_exact(initial_capacity)
        .map_err(|_| io::Error::other(format!("could not allocate memory for {subject}")))?;
    Read::by_ref(&mut file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > maximum {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    Ok(contents)
}

fn validate_opened_private_file(path: &Path, file: &File, subject: &str) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    let opened_metadata = file.metadata()?;
    validate_private_file_metadata(&opened_metadata, subject)?;
    ensure_private_file(path, file, &opened_metadata, subject)?;
    ensure_opened_file_matches_path(path, file, &path_metadata, &opened_metadata, subject)
}

fn validate_published_private_file(path: &Path, subject: &str) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    validate_opened_private_file(path, &file, subject)
}

fn validate_private_file_metadata(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_data(format!(
            "{subject} must not be a symbolic link or reparse point"
        )));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_data(format!("{subject} must be a regular file")));
    }
    ensure_private_path(metadata, subject)
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn ensure_private_path(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: geteuid has no preconditions and retains no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must be owned by the current user"),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must not be accessible by group or other users"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_path(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    Ok(())
}

fn ensure_private_file(
    path: &Path,
    file: &File,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    #[cfg(windows)]
    validate_windows_private_file(path, file, subject)?;
    #[cfg(not(windows))]
    let _ = (path, file, subject);
    Ok(())
}

#[cfg(unix)]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
    path_metadata: &fs::Metadata,
    opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if path_metadata.dev() == opened_metadata.dev() && path_metadata.ino() == opened_metadata.ino()
    {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn ensure_opened_file_matches_path(
    path: &Path,
    opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let current = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    if windows_file_identity(&current)? == windows_file_identity(opened_file)? {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the live handle and output pointer are valid for this call.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API reported that it initialized the output structure.
    let information = unsafe { information.assume_init() };
    Ok((
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

#[cfg(not(any(unix, windows)))]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Ok(())
}

fn add_nofollow_flags(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(any(windows, test))]
pub(crate) fn stable_lock_share_mode() -> u32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        FILE_SHARE_READ | FILE_SHARE_WRITE
    }
    #[cfg(not(windows))]
    {
        0x0000_0001 | 0x0000_0002
    }
}

fn map_nofollow_error(error: io::Error, subject: &str) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_data(format!("{subject} must not be a symbolic link"));
    }
    #[cfg(not(unix))]
    let _ = subject;
    error
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_LAYOUT: PrivateStoreLayout = PrivateStoreLayout {
        store_name: "test private store",
        data_file_name: "state.json",
        data_path_name: "test private state",
        data_subject: "test private state file",
        lock_file_name: "state.lock",
        lock_subject: "test private state lock",
        temporary_subject: "test private state temporary file",
        maximum_file_bytes: 64,
    };

    #[test]
    fn existing_lock_policy_never_creates_a_missing_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let directory = root.join("test-v1");
        TEST_LAYOUT
            .create_directory_beneath(&root, &directory)
            .unwrap();

        let error = TEST_LAYOUT
            .open_lock(&directory, LockMode::Shared, LockFilePolicy::Existing)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!directory.join("state.lock").exists());

        drop(
            TEST_LAYOUT
                .open_lock(&directory, LockMode::Exclusive, LockFilePolicy::Create)
                .unwrap(),
        );
        assert!(directory.join("state.lock").is_file());
    }

    #[test]
    fn atomic_write_round_trips_through_the_bounded_private_read() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let directory = root.join("test-v1");
        let path = directory.join("state.json");
        TEST_LAYOUT
            .create_directory_beneath(&root, &directory)
            .unwrap();
        let _lock = TEST_LAYOUT
            .open_lock(&directory, LockMode::Exclusive, LockFilePolicy::Create)
            .unwrap();

        TEST_LAYOUT.write_atomically(&path, b"{}\n").unwrap();

        assert!(TEST_LAYOUT.data_file_exists(&path).unwrap());
        assert_eq!(TEST_LAYOUT.read_bounded(&path).unwrap(), b"{}\n");
    }
}
