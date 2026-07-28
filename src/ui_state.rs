use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::atomic_file::replace_file;

pub const UI_STATE_VERSION: u32 = 1;

const APP_DIRECTORY: &str = "codex-usage-monit";
const STATE_FILE: &str = "tui-state.json";
const STATE_DIRECTORY_ENV: &str = "CODEX_USAGE_MONIT_STATE_DIR";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiTheme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiView {
    #[default]
    Overview,
    Health,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiWindowScope {
    #[default]
    FiveHours,
    Week,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiTaskListMode {
    #[default]
    Flat,
    Tree,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiTaskSourceFilter {
    #[default]
    All,
    Desktop,
    Subagent,
    Cli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiState {
    pub version: u32,
    pub theme: UiTheme,
    pub view: UiView,
    pub window_scope: UiWindowScope,
    pub turns_visible: bool,
    pub models_visible: bool,
    pub task_list_mode: UiTaskListMode,
    pub task_source_filter: UiTaskSourceFilter,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            version: UI_STATE_VERSION,
            theme: UiTheme::Dark,
            view: UiView::Overview,
            window_scope: UiWindowScope::FiveHours,
            turns_visible: true,
            models_visible: true,
            task_list_mode: UiTaskListMode::Flat,
            task_source_filter: UiTaskSourceFilter::All,
        }
    }
}

#[derive(Debug)]
pub struct UiStateStore {
    path: Option<PathBuf>,
    writes_allowed: bool,
}

impl Default for UiStateStore {
    fn default() -> Self {
        Self::discover()
    }
}

impl UiStateStore {
    pub fn discover() -> Self {
        Self {
            path: default_ui_state_path(),
            writes_allowed: true,
        }
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            writes_allowed: true,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn writes_allowed(&self) -> bool {
        self.writes_allowed && self.path.is_some()
    }

    /// Loads saved preferences. Missing, unreadable, or malformed files use defaults.
    /// A future schema version also uses defaults and disables writes for this store.
    pub fn load(&mut self) -> UiState {
        self.writes_allowed = true;
        let Some(path) = self.path.as_deref() else {
            return UiState::default();
        };
        let Ok(contents) = fs::read(path) else {
            return UiState::default();
        };
        let Ok(version) = serde_json::from_slice::<VersionProbe>(&contents) else {
            return UiState::default();
        };
        if version.version > UI_STATE_VERSION {
            self.writes_allowed = false;
            return UiState::default();
        }
        serde_json::from_slice(&contents).unwrap_or_default()
    }

    /// Atomically saves preferences where the platform supports replacing a file
    /// with `rename`. `false` means persistence is unavailable or was disabled by
    /// a future-version file; I/O and serialization failures are returned.
    pub fn save(&self, state: &UiState) -> io::Result<bool> {
        if !self.writes_allowed {
            return Ok(false);
        }
        let Some(path) = self.path.as_deref() else {
            return Ok(false);
        };

        let mut current = state.clone();
        current.version = UI_STATE_VERSION;
        let mut contents = serde_json::to_vec_pretty(&current)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        contents.push(b'\n');
        write_atomically(path, &contents)?;
        Ok(true)
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct VersionProbe {
    version: u32,
}

impl Default for VersionProbe {
    fn default() -> Self {
        Self {
            version: UI_STATE_VERSION,
        }
    }
}

pub fn default_ui_state_path() -> Option<PathBuf> {
    resolve_ui_state_path(
        nonempty_env(STATE_DIRECTORY_ENV).as_deref(),
        nonempty_env("XDG_STATE_HOME").as_deref(),
        nonempty_env("HOME").as_deref(),
        nonempty_env("LOCALAPPDATA").as_deref(),
        current_platform(),
    )
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

fn resolve_ui_state_path(
    state_directory: Option<&Path>,
    xdg_state_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(directory) = state_directory.filter(|path| !path.as_os_str().is_empty()) {
        return Some(directory.join(STATE_FILE));
    }
    if let Some(directory) = xdg_state_home.filter(|path| !path.as_os_str().is_empty()) {
        return Some(directory.join(APP_DIRECTORY).join(STATE_FILE));
    }

    let directory = match platform {
        Platform::MacOs => home.map(|path| path.join("Library/Application Support")),
        Platform::Windows => local_app_data.map(Path::to_path_buf),
        Platform::Unix => home.map(|path| path.join(".local/state")),
    }?;
    Some(directory.join(APP_DIRECTORY).join(STATE_FILE))
}

fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;

    let file_name = path.file_name().unwrap_or_else(|| OsStr::new(STATE_FILE));
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        sequence
    ));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        sync_directory(parent);
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
    fn defaults_match_the_initial_tui_menu() {
        assert_eq!(
            UiState::default(),
            UiState {
                version: UI_STATE_VERSION,
                theme: UiTheme::Dark,
                view: UiView::Overview,
                window_scope: UiWindowScope::FiveHours,
                turns_visible: true,
                models_visible: true,
                task_list_mode: UiTaskListMode::Flat,
                task_source_filter: UiTaskSourceFilter::All,
            }
        );
    }

    #[test]
    fn round_trip_uses_camel_case_and_replaces_existing_state() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/tui-state.json");
        let mut store = UiStateStore::new(path.clone());
        let first = UiState::default();
        assert!(store.save(&first).unwrap());

        let expected = UiState {
            version: UI_STATE_VERSION,
            theme: UiTheme::Light,
            view: UiView::Health,
            window_scope: UiWindowScope::Week,
            turns_visible: false,
            models_visible: false,
            task_list_mode: UiTaskListMode::Tree,
            task_source_filter: UiTaskSourceFilter::Subagent,
        };
        assert!(store.save(&expected).unwrap());
        assert_eq!(store.load(), expected);

        let json = fs::read_to_string(path).unwrap();
        assert!(json.contains("\"windowScope\": \"week\""));
        assert!(json.contains("\"taskListMode\": \"tree\""));
        assert!(!json.contains("window_scope"));
        assert_eq!(
            fs::read_dir(directory.path().join("nested"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn missing_malformed_and_partial_state_fall_back_safely() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("tui-state.json");
        let mut store = UiStateStore::new(path.clone());
        assert_eq!(store.load(), UiState::default());

        fs::write(&path, b"not json").unwrap();
        assert_eq!(store.load(), UiState::default());

        fs::write(
            &path,
            br#"{"version":1,"windowScope":"week","unknownSetting":true}"#,
        )
        .unwrap();
        assert_eq!(
            store.load(),
            UiState {
                window_scope: UiWindowScope::Week,
                ..UiState::default()
            }
        );
    }

    #[test]
    fn future_versions_use_defaults_without_being_overwritten() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("tui-state.json");
        let original = format!(
            "{{\"version\":{},\"windowScope\":\"week\"}}",
            UI_STATE_VERSION + 1
        );
        fs::write(&path, &original).unwrap();

        let mut store = UiStateStore::new(path.clone());
        assert_eq!(store.load(), UiState::default());
        assert!(!store.writes_allowed());
        assert!(!store.save(&UiState::default()).unwrap());
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn path_resolution_honors_overrides_and_platform_fallbacks() {
        let override_directory = Path::new("/override");
        let xdg = Path::new("/xdg-state");
        let home = Path::new("/home/user");
        let local = Path::new("C:/Users/user/AppData/Local");

        assert_eq!(
            resolve_ui_state_path(
                Some(override_directory),
                Some(xdg),
                Some(home),
                Some(local),
                Platform::MacOs,
            ),
            Some(override_directory.join(STATE_FILE))
        );
        assert_eq!(
            resolve_ui_state_path(None, Some(xdg), Some(home), None, Platform::MacOs),
            Some(xdg.join(APP_DIRECTORY).join(STATE_FILE))
        );
        assert_eq!(
            resolve_ui_state_path(None, None, Some(home), None, Platform::MacOs),
            Some(
                home.join("Library/Application Support")
                    .join(APP_DIRECTORY)
                    .join(STATE_FILE)
            )
        );
        assert_eq!(
            resolve_ui_state_path(None, None, Some(home), None, Platform::Unix),
            Some(
                home.join(".local/state")
                    .join(APP_DIRECTORY)
                    .join(STATE_FILE)
            )
        );
        assert_eq!(
            resolve_ui_state_path(None, None, None, Some(local), Platform::Windows),
            Some(local.join(APP_DIRECTORY).join(STATE_FILE))
        );
        assert_eq!(
            resolve_ui_state_path(None, None, None, None, Platform::Unix),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_state_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("private");
        let path = state_directory.join(STATE_FILE);
        let store = UiStateStore::new(path.clone());
        store.save(&UiState::default()).unwrap();

        assert_eq!(
            fs::metadata(state_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
