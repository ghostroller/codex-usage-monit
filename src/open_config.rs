use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub(crate) const OPEN_CONFIG_VERSION: u32 = 1;

const APP_DIRECTORY: &str = "codex-usage-monit";
const CONFIG_DIRECTORY_ENV: &str = "CODEX_USAGE_MONIT_CONFIG_DIR";
const CONFIG_FILE: &str = "open.json";
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OpenBackend {
    #[default]
    Zellij,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ZellijOpenConfig {
    pub(crate) floating: bool,
    pub(crate) width_percent: u8,
    pub(crate) height_percent: u8,
    pub(crate) close_on_exit: bool,
}

impl Default for ZellijOpenConfig {
    fn default() -> Self {
        Self {
            floating: true,
            width_percent: 90,
            height_percent: 90,
            close_on_exit: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OpenConfig {
    pub(crate) version: u32,
    pub(crate) enabled: bool,
    pub(crate) backend: OpenBackend,
    pub(crate) codex_bin: Option<PathBuf>,
    pub(crate) zellij: ZellijOpenConfig,
}

impl Default for OpenConfig {
    fn default() -> Self {
        Self {
            version: OPEN_CONFIG_VERSION,
            enabled: true,
            backend: OpenBackend::Zellij,
            codex_bin: None,
            zellij: ZellijOpenConfig::default(),
        }
    }
}

impl OpenConfig {
    /// Conservative runtime fallback for an unavailable or invalid config.
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != OPEN_CONFIG_VERSION {
            return Err(invalid_config(format!(
                "unsupported Open config version {}; expected {}",
                self.version, OPEN_CONFIG_VERSION
            )));
        }
        validate_percentage("zellij.widthPercent", self.zellij.width_percent)?;
        validate_percentage("zellij.heightPercent", self.zellij.height_percent)?;

        if let Some(path) = self.codex_bin.as_deref() {
            if path.as_os_str().is_empty() {
                return Err(invalid_config("codexBin must not be empty"));
            }
            if path.to_string_lossy().contains('\0') {
                return Err(invalid_config("codexBin must not contain a NUL byte"));
            }
        }
        Ok(())
    }
}

/// Loads the user-level Open configuration and creates a default file when it
/// does not exist. Errors are deliberately surfaced so the caller can disable
/// terminal launching instead of silently enabling it with defaults.
#[derive(Debug)]
pub(crate) struct OpenConfigStore {
    path: Option<PathBuf>,
}

impl Default for OpenConfigStore {
    fn default() -> Self {
        Self::discover()
    }
}

impl OpenConfigStore {
    pub(crate) fn discover() -> Self {
        Self {
            path: default_open_config_path(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn load_or_create(&self) -> io::Result<OpenConfig> {
        let path = self.path.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no user-level configuration directory is available",
            )
        })?;

        match fs::read(path) {
            Ok(contents) => deserialize_config(&contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let default = OpenConfig::default();
                let contents = serialize_config(&default)?;
                match create_private_atomically(path, &contents) {
                    Ok(true) => Ok(default),
                    Ok(false) => deserialize_config(&fs::read(path)?),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn default_open_config_path() -> Option<PathBuf> {
    resolve_open_config_path(
        nonempty_env(CONFIG_DIRECTORY_ENV).as_deref(),
        nonempty_env("XDG_CONFIG_HOME").as_deref(),
        nonempty_env("HOME").as_deref(),
        nonempty_env("LOCALAPPDATA").as_deref(),
        current_platform(),
    )
}

fn serialize_config(config: &OpenConfig) -> io::Result<Vec<u8>> {
    let mut contents = serde_json::to_vec_pretty(config)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');
    Ok(contents)
}

fn deserialize_config(contents: &[u8]) -> io::Result<OpenConfig> {
    let version = serde_json::from_slice::<VersionProbe>(contents)
        .map_err(|error| invalid_config(format!("invalid Open config: {error}")))?;
    let Some(version) = version.version else {
        return Err(invalid_config("Open config is missing version"));
    };
    if version != OPEN_CONFIG_VERSION {
        return Err(invalid_config(format!(
            "unsupported Open config version {version}; expected {OPEN_CONFIG_VERSION}"
        )));
    }

    let config: OpenConfig = serde_json::from_slice(contents)
        .map_err(|error| invalid_config(format!("invalid Open config: {error}")))?;
    config.validate()?;
    Ok(config)
}

#[derive(Debug, Deserialize)]
struct VersionProbe {
    version: Option<u32>,
}

fn validate_percentage(name: &str, value: u8) -> io::Result<()> {
    if (1..=100).contains(&value) {
        Ok(())
    } else {
        Err(invalid_config(format!("{name} must be between 1 and 100")))
    }
}

fn invalid_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
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

fn resolve_open_config_path(
    override_directory: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(directory) = nonempty_path(override_directory) {
        return Some(directory.join(CONFIG_FILE));
    }
    if let Some(directory) = nonempty_path(xdg_config_home) {
        return Some(directory.join(APP_DIRECTORY).join(CONFIG_FILE));
    }

    let directory = match platform {
        Platform::MacOs => nonempty_path(home).map(|path| path.join("Library/Application Support")),
        Platform::Windows => nonempty_path(local_app_data).map(Path::to_path_buf),
        Platform::Unix => nonempty_path(home).map(|path| path.join(".config")),
    }?;
    Some(directory.join(APP_DIRECTORY).join(CONFIG_FILE))
}

fn nonempty_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

/// Publishes via a hard link so a concurrent user-created config wins instead
/// of being overwritten between the missing-file check and the atomic create.
/// `Ok(false)` means another writer created the destination first.
fn create_private_atomically(path: &Path, contents: &[u8]) -> io::Result<bool> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;

    let file_name = path.file_name().unwrap_or_else(|| OsStr::new(CONFIG_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                fs::remove_file(&temporary)?;
                sync_directory(parent);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
        "could not allocate a unique Open config temporary file",
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

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_enable_zellij_with_a_large_held_floating_pane() {
        assert_eq!(
            OpenConfig::default(),
            OpenConfig {
                version: OPEN_CONFIG_VERSION,
                enabled: true,
                backend: OpenBackend::Zellij,
                codex_bin: None,
                zellij: ZellijOpenConfig {
                    floating: true,
                    width_percent: 90,
                    height_percent: 90,
                    close_on_exit: false,
                },
            }
        );
        assert!(!OpenConfig::disabled().enabled);
        assert_eq!(OpenConfig::disabled().zellij, ZellijOpenConfig::default());
    }

    #[test]
    fn missing_file_is_created_with_default_pretty_json() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/open.json");
        let store = OpenConfigStore::new(path.clone());

        assert_eq!(store.path(), Some(path.as_path()));
        assert_eq!(store.load_or_create().unwrap(), OpenConfig::default());
        assert_eq!(store.load_or_create().unwrap(), OpenConfig::default());

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.ends_with('\n'));
        assert!(contents.contains("\"backend\": \"zellij\""));
        assert!(contents.contains("\"widthPercent\": 90"));
        assert!(contents.contains("\"closeOnExit\": false"));
        assert!(contents.contains("\"codexBin\": null"));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn complete_and_partial_version_one_files_load_with_defaults() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        fs::write(
            &path,
            br#"{
                "version": 1,
                "enabled": false,
                "codexBin": "/opt/codex",
                "zellij": {"widthPercent": 75, "closeOnExit": true}
            }"#,
        )
        .unwrap();

        let config = OpenConfigStore::new(path).load_or_create().unwrap();
        assert!(!config.enabled);
        assert_eq!(config.codex_bin, Some(PathBuf::from("/opt/codex")));
        assert_eq!(config.backend, OpenBackend::Zellij);
        assert_eq!(config.zellij.width_percent, 75);
        assert_eq!(config.zellij.height_percent, 90);
        assert!(config.zellij.close_on_exit);
        assert!(config.zellij.floating);
    }

    #[test]
    fn malformed_missing_version_and_future_version_are_not_overwritten() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        let store = OpenConfigStore::new(path.clone());

        for contents in [
            "not json",
            r#"{"enabled":false}"#,
            r#"{"version":2,"enabled":false}"#,
        ] {
            fs::write(&path, contents).unwrap();
            let error = store.load_or_create().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[test]
    fn invalid_dimensions_and_empty_codex_bin_are_rejected() {
        for contents in [
            r#"{"version":1,"zellij":{"widthPercent":0}}"#,
            r#"{"version":1,"zellij":{"heightPercent":101}}"#,
            r#"{"version":1,"codexBin":""}"#,
            r#"{"version":1,"enable":false}"#,
            r#"{"version":1,"zellij":{"closeOnExist":true}}"#,
        ] {
            let error = deserialize_config(contents.as_bytes()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn unreadable_path_and_failed_initial_create_return_errors() {
        let directory = tempdir().unwrap();
        let unreadable = directory.path().join("directory-instead-of-json");
        fs::create_dir(&unreadable).unwrap();
        assert!(OpenConfigStore::new(unreadable).load_or_create().is_err());

        let blocker = directory.path().join("not-a-directory");
        fs::write(&blocker, "file").unwrap();
        assert!(
            OpenConfigStore::new(blocker.join(CONFIG_FILE))
                .load_or_create()
                .is_err()
        );
    }

    #[test]
    fn path_resolution_honors_override_then_xdg_then_platform_defaults() {
        let override_directory = Path::new("/override");
        let xdg = Path::new("/xdg-config");
        let home = Path::new("/home/user");
        let local = Path::new("C:/Users/user/AppData/Local");

        assert_eq!(
            resolve_open_config_path(
                Some(override_directory),
                Some(xdg),
                Some(home),
                Some(local),
                Platform::MacOs,
            ),
            Some(override_directory.join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_open_config_path(None, Some(xdg), Some(home), None, Platform::Unix),
            Some(xdg.join(APP_DIRECTORY).join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_open_config_path(None, None, Some(home), None, Platform::MacOs),
            Some(
                home.join("Library/Application Support")
                    .join(APP_DIRECTORY)
                    .join(CONFIG_FILE)
            )
        );
        assert_eq!(
            resolve_open_config_path(None, None, Some(home), None, Platform::Unix),
            Some(home.join(".config").join(APP_DIRECTORY).join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_open_config_path(None, None, None, Some(local), Platform::Windows),
            Some(local.join(APP_DIRECTORY).join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_open_config_path(None, None, None, None, Platform::Unix),
            None
        );
    }

    #[test]
    fn path_resolution_ignores_empty_inputs() {
        let empty = Path::new("");
        let home = Path::new("/home/user");
        assert_eq!(
            resolve_open_config_path(
                Some(empty),
                Some(empty),
                Some(home),
                Some(empty),
                Platform::Unix,
            ),
            Some(home.join(".config").join(APP_DIRECTORY).join(CONFIG_FILE))
        );
    }

    #[test]
    fn concurrent_create_does_not_replace_the_winning_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        fs::write(&path, br#"{"version":1,"enabled":false}"#).unwrap();

        assert!(!create_private_atomically(&path, b"replacement").unwrap());
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            r#"{"version":1,"enabled":false}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_config_paths_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let config_directory = directory.path().join("private/nested");
        let path = config_directory.join(CONFIG_FILE);
        OpenConfigStore::new(path.clone()).load_or_create().unwrap();

        assert_eq!(
            fs::metadata(config_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
