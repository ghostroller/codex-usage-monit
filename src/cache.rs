use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::atomic_file::replace_file;

const APP_DIRECTORY: &str = "codex-usage-monit";
const CACHE_DIRECTORY_ENV: &str = "CODEX_USAGE_MONIT_CACHE_DIR";
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Returns the platform-appropriate directory for persistent rollout cache data.
pub fn default_rollout_cache_dir() -> Option<PathBuf> {
    resolve_rollout_cache_dir(
        nonempty_env(CACHE_DIRECTORY_ENV).as_deref(),
        nonempty_env("XDG_CACHE_HOME").as_deref(),
        nonempty_env("HOME").as_deref(),
        nonempty_env("LOCALAPPDATA").as_deref(),
        nonempty_env("USERPROFILE").as_deref(),
        current_platform(),
    )
}

pub(crate) fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;

    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("cache"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.flush()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_private_cache_path(path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Proves that the private cache directory can create, flush, and remove a
/// file without retaining probe state. This is used only by an explicit
/// remote-agent health probe; normal startup never calls it.
pub(crate) fn probe_private_directory_writable(directory: &Path) -> io::Result<()> {
    create_private_directory(directory)?;
    let (temporary, mut file) = create_temporary_file(directory, OsStr::new("writable-probe"))?;
    let result = (|| {
        file.write_all(b"probe")?;
        file.sync_all()?;
        drop(file);
        fs::remove_file(&temporary)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Validates a cache directory before existing persistent entries are read or
/// removed. The final directory must be private, and user-controlled symbolic
/// link components are rejected instead of being followed.
pub(crate) fn validate_private_cache_directory(path: &Path) -> io::Result<()> {
    reject_untrusted_link_components(path, "rollout cache directory")?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_cache(
            "rollout cache directory must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_cache("rollout cache path must be a directory"));
    }
    ensure_private_directory(path, &metadata, "rollout cache directory")
}

/// Validates an already-open persistent cache entry against its current path.
pub(crate) fn validate_private_cache_file(path: &Path, file: &File) -> io::Result<()> {
    reject_untrusted_link_components(path, "rollout cache entry")?;
    let path_metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&path_metadata) || !path_metadata.file_type().is_file() {
        return Err(invalid_cache(
            "rollout cache entry must be a regular file, not a link or reparse point",
        ));
    }
    let opened_metadata = file.metadata()?;
    if metadata_is_link_or_reparse(&opened_metadata) || !opened_metadata.file_type().is_file() {
        return Err(invalid_cache(
            "opened rollout cache entry is not a regular file",
        ));
    }
    ensure_opened_file_matches_path(&path_metadata, &opened_metadata)?;
    ensure_private_file(path, file, &opened_metadata, "rollout cache entry")
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Windows,
    Unix,
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

fn resolve_rollout_cache_dir(
    override_directory: Option<&Path>,
    xdg_cache_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    user_profile: Option<&Path>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(directory) = nonempty_path(override_directory) {
        return Some(directory.to_path_buf());
    }
    if let Some(directory) = nonempty_path(xdg_cache_home) {
        return Some(directory.join(APP_DIRECTORY));
    }

    match platform {
        Platform::MacOs => nonempty_path(home)
            .map(|directory| directory.join("Library/Caches").join(APP_DIRECTORY)),
        Platform::Windows => nonempty_path(local_app_data)
            .map(Path::to_path_buf)
            .or_else(|| {
                nonempty_path(user_profile).map(|directory| directory.join("AppData").join("Local"))
            })
            .map(|directory| directory.join(APP_DIRECTORY).join("cache")),
        Platform::Unix => {
            nonempty_path(home).map(|directory| directory.join(".cache").join(APP_DIRECTORY))
        }
    }
}

fn nonempty_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

fn create_temporary_file(parent: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }

        match options.open(&temporary) {
            Ok(file) => match validate_private_cache_file(&temporary, &file) {
                Ok(()) => return Ok((temporary, file)),
                Err(error) => {
                    drop(file);
                    let _ = fs::remove_file(&temporary);
                    return Err(error);
                }
            },
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique cache temporary file",
    ))
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match validate_private_cache_directory(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
    }
    validate_private_cache_directory(path)
}

fn validate_private_cache_path(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
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
    let file = options.open(path)?;
    validate_private_cache_file(path, &file)
}

fn invalid_cache(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn reject_untrusted_link_components(path: &Path, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    for component in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() && metadata.uid() != 0 => {
                return Err(invalid_cache(format!(
                    "{subject} path must not traverse a user-controlled symbolic link ({})",
                    component.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn reject_untrusted_link_components(path: &Path, subject: &str) -> io::Result<()> {
    // The shared Windows validator checks every path component for reparse
    // points and validates the final directory DACL using a live handle.
    if path.is_dir() {
        crate::source_identity::validate_windows_private_directory(path, subject)
    } else if let Some(parent) = path.parent() {
        crate::source_identity::validate_windows_private_directory(parent, subject)
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn reject_untrusted_link_components(path: &Path, subject: &str) -> io::Result<()> {
    for component in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        if fs::symlink_metadata(component).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(invalid_cache(format!(
                "{subject} path must not traverse a symbolic link ({})",
                component.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_directory(
    _path: &Path,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_unix_metadata(metadata, subject)
}

#[cfg(unix)]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_unix_metadata(metadata, subject)
}

#[cfg(unix)]
fn ensure_private_unix_metadata(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

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

#[cfg(windows)]
fn ensure_private_directory(
    path: &Path,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_directory(path, subject)
}

#[cfg(windows)]
fn ensure_private_file(
    path: &Path,
    file: &File,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_file(path, file, subject)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(
    _path: &Path,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private rollout cache directories are unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private rollout cache files are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn ensure_opened_file_matches_path(
    path_metadata: &fs::Metadata,
    opened_metadata: &fs::Metadata,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if path_metadata.dev() == opened_metadata.dev() && path_metadata.ino() == opened_metadata.ino()
    {
        Ok(())
    } else {
        Err(invalid_cache(
            "rollout cache entry changed while it was being opened",
        ))
    }
}

#[cfg(not(unix))]
fn ensure_opened_file_matches_path(
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolver_honors_override_then_xdg_then_platform_fallbacks() {
        let override_directory = Path::new("/override-cache");
        let xdg = Path::new("/xdg-cache");
        let home = Path::new("/home/user");
        let local = Path::new("C:/Users/user/AppData/Local");

        assert_eq!(
            resolve_rollout_cache_dir(
                Some(override_directory),
                Some(xdg),
                Some(home),
                Some(local),
                None,
                Platform::Windows,
            ),
            Some(override_directory.to_path_buf())
        );
        assert_eq!(
            resolve_rollout_cache_dir(
                None,
                Some(xdg),
                Some(home),
                Some(local),
                None,
                Platform::MacOs,
            ),
            Some(xdg.join(APP_DIRECTORY))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, Some(home), None, None, Platform::MacOs),
            Some(home.join("Library/Caches").join(APP_DIRECTORY))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, Some(home), None, None, Platform::Unix),
            Some(home.join(".cache").join(APP_DIRECTORY))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, None, Some(local), None, Platform::Windows,),
            Some(local.join(APP_DIRECTORY).join("cache"))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, None, None, None, Platform::Unix),
            None
        );
    }

    #[test]
    fn windows_resolver_falls_back_to_user_profile_local_app_data() {
        let user_profile = Path::new("C:/Users/developer");
        assert_eq!(
            resolve_rollout_cache_dir(
                None,
                None,
                None,
                None,
                Some(user_profile),
                Platform::Windows,
            ),
            Some(
                user_profile
                    .join("AppData")
                    .join("Local")
                    .join(APP_DIRECTORY)
                    .join("cache")
            )
        );
    }

    #[test]
    fn resolver_ignores_empty_inputs() {
        let empty = Path::new("");
        let home = Path::new("/home/user");

        assert_eq!(
            resolve_rollout_cache_dir(
                Some(empty),
                Some(empty),
                Some(home),
                Some(empty),
                Some(empty),
                Platform::Unix,
            ),
            Some(home.join(".cache").join(APP_DIRECTORY))
        );
    }

    #[test]
    fn atomic_write_round_trips_and_replaces_without_leftovers() {
        let directory = tempdir().unwrap();
        let cache_directory = directory.path().join("nested/cache");
        let path = cache_directory.join("rollouts.json");

        write_private_atomically(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        write_private_atomically(&path, b"replacement").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(cache_directory).unwrap().count(), 1);
    }

    #[test]
    fn failed_replace_removes_the_temporary_file() {
        let directory = tempdir().unwrap();
        let cache_directory = directory.path().join("cache");
        let target_directory = cache_directory.join("target");
        fs::create_dir_all(&target_directory).unwrap();

        assert!(write_private_atomically(&target_directory, b"contents").is_err());
        assert_eq!(fs::read_dir(cache_directory).unwrap().count(), 1);
    }

    #[test]
    fn writable_probe_leaves_no_file_behind() {
        let directory = tempdir().unwrap();
        let cache_directory = directory.path().join("private/cache");
        probe_private_directory_writable(&cache_directory).unwrap();
        assert_eq!(fs::read_dir(cache_directory).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_cache_paths_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let cache_directory = directory.path().join("private/nested");
        let path = cache_directory.join("rollouts.json");
        write_private_atomically(&path, b"private").unwrap();

        assert_eq!(
            fs::metadata(&cache_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_public_cache_directory_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let cache_directory = directory.path().join("public-cache");
        fs::create_dir(&cache_directory).unwrap();
        fs::set_permissions(&cache_directory, fs::Permissions::from_mode(0o755)).unwrap();

        let error = probe_private_directory_writable(&cache_directory).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_dir(cache_directory).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn cache_directory_and_ancestor_symlinks_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();

        let final_link = directory.path().join("final-link");
        symlink(&outside, &final_link).unwrap();
        assert!(probe_private_directory_writable(&final_link).is_err());

        let nested = outside.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        let ancestor_link = directory.path().join("ancestor-link");
        symlink(&outside, &ancestor_link).unwrap();
        assert!(probe_private_directory_writable(&ancestor_link.join("nested")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn existing_public_cache_entry_is_rejected_on_read() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let cache_directory = directory.path().join("private-cache");
        fs::create_dir(&cache_directory).unwrap();
        fs::set_permissions(&cache_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = cache_directory.join("entry.json");
        fs::write(&path, b"cache").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let file = File::open(&path).unwrap();

        let error = validate_private_cache_file(&path, &file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
