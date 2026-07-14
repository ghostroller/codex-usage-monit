use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temporary, target)
    }
    #[cfg(windows)]
    {
        match fs::rename(temporary, target) {
            Ok(()) => Ok(()),
            Err(error)
                if target.is_file()
                    && matches!(
                        error.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                    ) =>
            {
                fs::remove_file(target)?;
                fs::rename(temporary, target)
            }
            Err(error) => Err(error),
        }
    }
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
            options.mode(0o600);
        }

        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
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
    if path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
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
                Platform::Windows,
            ),
            Some(override_directory.to_path_buf())
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, Some(xdg), Some(home), Some(local), Platform::MacOs,),
            Some(xdg.join(APP_DIRECTORY))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, Some(home), None, Platform::MacOs),
            Some(home.join("Library/Caches").join(APP_DIRECTORY))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, Some(home), None, Platform::Unix),
            Some(home.join(".cache").join(APP_DIRECTORY))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, None, Some(local), Platform::Windows),
            Some(local.join(APP_DIRECTORY).join("cache"))
        );
        assert_eq!(
            resolve_rollout_cache_dir(None, None, None, None, Platform::Unix),
            None
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
}
