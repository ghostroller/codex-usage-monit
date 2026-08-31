use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_file::replace_file;
use crate::history::HistoryStore;
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

const SERVICE_LABEL: &str = "com.ghostroller.codex-usage-monit.recorder";
const SYSTEMD_UNIT: &str = "codex-usage-monit-recorder.service";
const WINDOWS_TASK_PREFIX: &str = r"\CodexUsageMonitRecorder";
const HRESULT_FILE_NOT_FOUND: i32 = 0x8007_0002_u32 as i32;
const WINDOWS_TASK_STATE_ENV: &str = "CODEX_USAGE_MONIT_TASK_NAME";
const WINDOWS_TASK_STATE_DISABLED: i32 = 1;
const WINDOWS_TASK_STATE_QUEUED: i32 = 2;
const WINDOWS_TASK_STATE_READY: i32 = 3;
const WINDOWS_TASK_STATE_RUNNING: i32 = 4;
const WINDOWS_TASK_STATE_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$folder = $service.GetFolder('\')
$task = $folder.GetTask($env:CODEX_USAGE_MONIT_TASK_NAME)
[Console]::Out.Write([int]$task.State)
"#;
const STATUS_SCHEMA_VERSION: u32 = 3;
const LEGACY_RECORDER_STALE_SECONDS: u64 = 12 * 60;
const RECORDER_STALE_GRACE_SECONDS: u64 = 2 * 60;
const RECORDER_INSTANCE_LOCK_FILE: &str = "recorder-instance.lock";
const SERVICE_CUTOVER_LOCK_FILE: &str = "service-cutover.lock";
const RECORDER_CUTOVER_BLOCKER_FILE: &str = "recorder-cutover-blocked.json";
const RECORDER_CUTOVER_BLOCKER_SCHEMA_VERSION: u32 = 1;
const CURRENT_SERVICE_DEFINITION_FILE: &str = "current-service-definition.json";
const CURRENT_SERVICE_DEFINITION_SCHEMA_VERSION: u32 = 1;
const SERVICE_TRUST_MARKER_MAX_BYTES: u64 = 8 * 1024;
const SERVICE_DEFINITION_MAX_BYTES: u64 = 256 * 1024;
const SERVICE_COORDINATION_DIRECTORY: &str = "service-registration-v1";
pub(crate) const SERVICE_CUTOVER_PROTOCOL: &str = "source-aware-v2";
pub(crate) const SERVICE_DEFINITION_ID_ARGUMENT: &str = "--service-definition-id";
const SERVICE_DEFINITION_ID_DOMAIN: &[u8] =
    b"codex-usage-monit/service-definition/source-aware-v2/platform-template-v1";
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static HELD_RECORDER_LOCKS: LazyLock<Mutex<HashSet<RecorderLockIdentity>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    NotInstalled,
    Installed,
    Running,
    Stopped,
    Unknown,
}

impl ServiceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not installed",
            Self::Installed => "installed",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub platform: String,
    pub state: ServiceState,
    pub installed: bool,
    pub running: bool,
    #[serde(default, with = "crate::exact_json::optional_pathbuf_lossy")]
    pub registration_path: Option<PathBuf>,
    pub last_history_heartbeat: Option<DateTime<Utc>>,
    /// Whether the latest recorder heartbeat is recent and belongs to the
    /// expected history namespace.
    #[serde(default)]
    pub heartbeat_recent: bool,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceOptions {
    pub executable: PathBuf,
    pub codex_home: PathBuf,
    pub codex_bin: Option<PathBuf>,
    pub history_dir: PathBuf,
    pub status_file: PathBuf,
    pub perf_log: Option<PathBuf>,
    /// Exact remotes.json selected when the service definition is installed.
    /// Background managers do not reliably inherit the installing shell's
    /// CODEX_USAGE_MONIT_CONFIG_DIR environment.
    pub remotes_config_file: Option<PathBuf>,
    /// Exact project-mappings.json selected at installation time.
    pub project_mapping_file: Option<PathBuf>,
    pub environment_path: Option<OsString>,
    pub lookback_days: i64,
    pub max_files: usize,
    pub active_grace_minutes: u64,
    pub offline: bool,
    pub redact_content: bool,
    pub no_rollout_cache: bool,
    #[cfg(test)]
    service_coordination_root_override: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecorderCutoverBlocker {
    schema_version: u32,
    blocked_at: DateTime<Utc>,
    platform: String,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentServiceDefinitionMarker {
    schema_version: u32,
    platform: String,
    fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServiceDefinitionObservation {
    Absent,
    Fingerprint(String),
    Unverifiable(String),
}

impl ServiceOptions {
    pub fn new(
        executable: PathBuf,
        codex_home: PathBuf,
        history_dir: PathBuf,
        status_file: PathBuf,
        perf_log: Option<PathBuf>,
    ) -> Self {
        Self {
            executable,
            codex_home,
            codex_bin: None,
            history_dir,
            status_file,
            perf_log,
            remotes_config_file: None,
            project_mapping_file: None,
            environment_path: env::var_os("PATH"),
            lookback_days: 7,
            max_files: 500,
            active_grace_minutes: 5,
            offline: false,
            redact_content: false,
            no_rollout_cache: false,
            #[cfg(test)]
            service_coordination_root_override: None,
        }
    }

    fn recorder_semantic_arguments(&self) -> Vec<OsString> {
        let mut arguments = vec![
            OsString::from("--codex-home"),
            self.codex_home.as_os_str().to_owned(),
        ];
        if let Some(codex_bin) = self.codex_bin.as_deref() {
            arguments.push(OsString::from("--codex-bin"));
            arguments.push(codex_bin.as_os_str().to_owned());
        }
        arguments.extend([
            OsString::from("--days"),
            OsString::from(self.lookback_days.to_string()),
            OsString::from("--max-files"),
            OsString::from(self.max_files.to_string()),
            OsString::from("--active-grace-minutes"),
            OsString::from(self.active_grace_minutes.to_string()),
        ]);
        if self.offline {
            arguments.push(OsString::from("--offline"));
        }
        if self.redact_content {
            arguments.push(OsString::from("--redact-content"));
        }
        if self.no_rollout_cache {
            arguments.push(OsString::from("--no-rollout-cache"));
        }
        if let Some(perf_log) = self.perf_log.as_deref() {
            arguments.push(OsString::from("--perf-log"));
            arguments.push(perf_log.as_os_str().to_owned());
        }
        if let Some(remotes_config_file) = self.remotes_config_file.as_deref() {
            arguments.push(OsString::from("--service-remotes-config"));
            arguments.push(remotes_config_file.as_os_str().to_owned());
        }
        arguments.extend([
            OsString::from("record"),
            OsString::from("--foreground"),
            OsString::from("--service-cutover-protocol"),
            OsString::from(SERVICE_CUTOVER_PROTOCOL),
        ]);
        if let Some(project_mapping_file) = self.project_mapping_file.as_deref() {
            arguments.push(OsString::from("--service-project-mapping-file"));
            arguments.push(project_mapping_file.as_os_str().to_owned());
        }
        arguments.extend([
            OsString::from("--history-dir"),
            self.history_dir.as_os_str().to_owned(),
            OsString::from("--status-file"),
            self.status_file.as_os_str().to_owned(),
        ]);
        arguments
    }

    pub(crate) fn service_definition_id(&self) -> String {
        let mut digest = Sha256::new();
        update_definition_hash_bytes(&mut digest, b"domain", SERVICE_DEFINITION_ID_DOMAIN);
        update_definition_hash_bytes(&mut digest, b"platform", std::env::consts::OS.as_bytes());
        update_definition_hash_os(&mut digest, b"executable", self.executable.as_os_str());
        match self.environment_path.as_deref() {
            Some(path) => {
                update_definition_hash_bytes(&mut digest, b"environment-present", b"1");
                update_definition_hash_os(&mut digest, b"environment-path", path);
            }
            None => {
                update_definition_hash_bytes(&mut digest, b"environment-present", b"0");
            }
        }
        let arguments = self.recorder_semantic_arguments();
        update_definition_hash_bytes(
            &mut digest,
            b"argument-count",
            &u64::try_from(arguments.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for argument in arguments {
            update_definition_hash_os(&mut digest, b"argument", &argument);
        }
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn recorder_arguments(&self) -> Vec<OsString> {
        let mut arguments = self.recorder_semantic_arguments();
        let protocol = arguments
            .iter()
            .position(|argument| argument == "--service-cutover-protocol")
            .expect("the service semantic argv always carries its cutover protocol");
        arguments.splice(
            protocol + 2..protocol + 2,
            [
                OsString::from(SERVICE_DEFINITION_ID_ARGUMENT),
                OsString::from(self.service_definition_id()),
            ],
        );
        arguments
    }
}

fn update_definition_hash_bytes(digest: &mut Sha256, label: &[u8], value: &[u8]) {
    digest.update(u64::try_from(label.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(label);
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

fn update_definition_hash_os(digest: &mut Sha256, label: &[u8], value: &OsStr) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        update_definition_hash_bytes(digest, label, value.as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units = value
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        update_definition_hash_bytes(digest, label, &units);
    }
    #[cfg(not(any(unix, windows)))]
    {
        update_definition_hash_bytes(digest, label, value.to_string_lossy().as_bytes());
    }
}

pub(crate) fn validate_service_definition_id(definition_id: &str) -> Result<()> {
    if definition_id.len() != 64
        || !definition_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!(
            "service definition identity must contain exactly 64 lowercase hexadecimal characters; reinstall the background service"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatusFile {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_namespace: Option<String>,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub last_attempt_at: DateTime<Utc>,
    pub last_history_heartbeat: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval_seconds: Option<u64>,
    /// Explicit persistence backend used by this recorder process. Missing on
    /// pre-v0.4 status files and therefore treated as a legacy v1 writer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_backend: Option<RecorderHistoryBackend>,
    /// Durable v2 ownership epoch held by a source-aware recorder. This must
    /// be non-zero and greater than the initial v1 epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_epoch: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderHistoryBackend {
    LegacyV1,
    SourceAwareV2,
}

impl RecorderStatusFile {
    pub fn started(now: DateTime<Utc>, history_namespace: String) -> Self {
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            history_namespace: Some(history_namespace),
            pid: std::process::id(),
            started_at: now,
            last_attempt_at: now,
            last_history_heartbeat: None,
            last_error: None,
            heartbeat_interval_seconds: None,
            history_backend: Some(RecorderHistoryBackend::LegacyV1),
            ownership_epoch: None,
        }
    }

    pub fn started_with_interval(
        now: DateTime<Utc>,
        history_namespace: String,
        heartbeat_interval_seconds: u64,
    ) -> Self {
        Self {
            heartbeat_interval_seconds: Some(heartbeat_interval_seconds.max(1)),
            ..Self::started(now, history_namespace)
        }
    }

    pub fn record_success(&mut self, now: DateTime<Utc>) {
        self.last_attempt_at = now;
        self.last_history_heartbeat = Some(now);
        self.last_error = None;
    }

    pub fn record_heartbeat(&mut self, now: DateTime<Utc>) {
        self.last_attempt_at = now;
        self.last_history_heartbeat = Some(now);
    }

    pub fn record_error(&mut self, now: DateTime<Utc>, error: impl Into<String>) {
        self.last_attempt_at = now;
        self.last_error = Some(error.into());
    }

    pub fn record_degraded(&mut self, now: DateTime<Utc>, error: impl Into<String>) {
        self.last_attempt_at = now;
        self.last_history_heartbeat = Some(now);
        self.last_error = Some(error.into());
    }

    pub fn heartbeat_is_recent(&self, now: DateTime<Utc>) -> bool {
        self.last_history_heartbeat
            .is_some_and(|heartbeat| self.activity_timestamp_is_recent(heartbeat, now))
    }

    /// Marks this process as a cooperating source-aware writer only after the
    /// v2 ownership activation has been durably observed.
    pub fn bind_source_aware_v2(&mut self, ownership_epoch: u64) -> io::Result<()> {
        if ownership_epoch <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source-aware recorder ownership epoch must be greater than one",
            ));
        }
        self.history_backend = Some(RecorderHistoryBackend::SourceAwareV2);
        self.ownership_epoch = Some(ownership_epoch);
        Ok(())
    }

    pub fn source_aware_v2_epoch(&self) -> Option<u64> {
        (self.schema_version >= 3
            && self.history_backend == Some(RecorderHistoryBackend::SourceAwareV2))
        .then_some(self.ownership_epoch)
        .flatten()
        .filter(|epoch| *epoch > 1)
    }

    /// Conservative quiescence check used before the one-way v1 -> v2
    /// cutover. Old binaries never acquire the ownership writer lease, so a
    /// recent legacy status must block migration until the service is stopped
    /// or restarted with a source-aware binary.
    pub fn incompatible_writer_may_be_active(&self, now: DateTime<Utc>) -> bool {
        self.source_aware_v2_epoch().is_none() && self.writer_may_be_active(now)
    }

    pub fn writer_may_be_active(&self, now: DateTime<Utc>) -> bool {
        self.activity_timestamp_is_recent(self.last_activity_at(), now)
    }

    fn last_activity_at(&self) -> DateTime<Utc> {
        self.last_history_heartbeat
            .map_or(self.last_attempt_at, |heartbeat| {
                heartbeat.max(self.last_attempt_at)
            })
    }

    fn activity_timestamp_is_recent(&self, timestamp: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        let age_seconds = now.signed_duration_since(timestamp).num_seconds();
        let stale_after_seconds =
            self.heartbeat_interval_seconds
                .map_or(LEGACY_RECORDER_STALE_SECONDS, |interval| {
                    interval
                        .saturating_add(RECORDER_STALE_GRACE_SECONDS)
                        .max(LEGACY_RECORDER_STALE_SECONDS)
                });
        let stale_after_seconds = i64::try_from(stale_after_seconds).unwrap_or(i64::MAX);
        // A heartbeat in the apparent future is ambiguous clock rollback,
        // not proof that the writer stopped. Fail closed until a cooperating
        // lifetime lock can prove quiescence.
        age_seconds <= stale_after_seconds
    }
}

/// Result of attempting to own the one cooperating recorder slot for a state
/// root.
///
/// The lock is intentionally shared by preview-enabled and redacted recorder
/// namespaces. `recorder-status.json` remains diagnostics only and must never
/// be used as lock authority.
#[must_use]
#[derive(Debug)]
pub enum TryRecorderInstanceLock {
    Acquired(RecorderInstanceLockGuard),
    Busy,
}

/// Process-lifetime authority for the single cooperating recorder associated
/// with one application state root.
///
/// Dropping the guard releases the operating-system lock. Callers performing a
/// v1 -> v2 cutover may use the same API as a short quiescence proof for
/// current versions, but this lock cannot fence a pre-v0.4 binary that does not
/// participate in the protocol. The legacy recorder-status check is still
/// required for that case.
pub struct RecorderInstanceLockGuard {
    file: File,
    path: PathBuf,
    identity: RecorderLockIdentity,
}

impl RecorderInstanceLockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for RecorderInstanceLockGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecorderInstanceLockGuard")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for RecorderInstanceLockGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
        held_recorder_locks().remove(&self.identity);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RecorderLockIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
}

/// Attempts to acquire the state-root-wide recorder lifetime lock.
///
/// `history_dir` is normally `<state>/history-v1`; its parent determines the
/// coordination root, so both content-redaction modes contend on the same
/// stable lock file. The process-local identity registry is deliberate: some
/// operating systems allow a process to reacquire a file lock through another
/// handle, which must not be mistaken for a second recorder slot.
pub fn try_acquire_recorder_instance_lock(
    history_dir: &Path,
) -> io::Result<TryRecorderInstanceLock> {
    let state_root = recorder_state_root(history_dir)?;
    try_acquire_named_private_root_lock(
        &state_root,
        RECORDER_INSTANCE_LOCK_FILE,
        CoordinationLockMode::Exclusive,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoordinationLockMode {
    Shared,
    Exclusive,
}

fn try_acquire_named_private_root_lock(
    state_root: &Path,
    file_name: &str,
    mode: CoordinationLockMode,
) -> io::Result<TryRecorderInstanceLock> {
    prepare_recorder_lock_state_root(state_root)?;
    let path = state_root.join(file_name);
    let file = open_recorder_lock_file(state_root, file_name)?;
    let identity = recorder_lock_identity(&file)?;

    {
        let mut held = held_recorder_locks();
        if !held.insert(identity) {
            drop(held);
            validate_opened_recorder_lock(state_root, file_name, &file, identity)?;
            return Ok(TryRecorderInstanceLock::Busy);
        }
    }

    let lock_result = match mode {
        CoordinationLockMode::Shared => fs2::FileExt::try_lock_shared(&file),
        CoordinationLockMode::Exclusive => fs2::FileExt::try_lock_exclusive(&file),
    };
    match lock_result {
        Ok(()) => {
            if let Err(error) =
                validate_opened_recorder_lock(state_root, file_name, &file, identity)
            {
                let _ = fs2::FileExt::unlock(&file);
                held_recorder_locks().remove(&identity);
                return Err(error);
            }
            Ok(TryRecorderInstanceLock::Acquired(
                RecorderInstanceLockGuard {
                    file,
                    path,
                    identity,
                },
            ))
        }
        Err(error) if recorder_lock_is_contended(&error) => {
            let validation = validate_opened_recorder_lock(state_root, file_name, &file, identity);
            held_recorder_locks().remove(&identity);
            validation?;
            Ok(TryRecorderInstanceLock::Busy)
        }
        Err(error) => {
            held_recorder_locks().remove(&identity);
            Err(error)
        }
    }
}

pub fn default_status_file(history_dir: &Path) -> PathBuf {
    history_dir
        .parent()
        .unwrap_or(history_dir)
        .join("recorder-status.json")
}

pub(crate) fn service_coordination_root() -> io::Result<PathBuf> {
    let root = match current_platform() {
        Platform::MacOs => stable_current_user_home()
            .map(|home| home.join("Library/Application Support/codex-usage-monit")),
        Platform::Linux | Platform::Unsupported => {
            stable_current_user_home().map(|home| home.join(".local/state/codex-usage-monit"))
        }
        Platform::Windows => stable_windows_local_app_data()
            .zip(windows_current_user_sid().ok())
            .map(|(local, sid)| local.join("codex-usage-monit").join(sid)),
    }
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the current-user service coordination directory is unavailable",
        )
    })?
    .join(SERVICE_COORDINATION_DIRECTORY);
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the current-user service coordination directory must be absolute",
        ));
    }
    Ok(root)
}

#[cfg(unix)]
fn stable_current_user_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    // SAFETY: geteuid has no preconditions. getpwuid_r writes only into the
    // supplied passwd/buffer storage and returns before either is released.
    let uid = unsafe { libc::geteuid() };
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut capacity = usize::try_from(suggested.max(16_384))
        .ok()?
        .min(1024 * 1024);
    let home_bytes = loop {
        let mut buffer = vec![0_u8; capacity];
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && capacity < 1024 * 1024 {
            capacity = capacity.saturating_mul(2).min(1024 * 1024);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let passwd = unsafe { passwd.assume_init() };
        if passwd.pw_dir.is_null() {
            return None;
        }
        break unsafe { std::ffi::CStr::from_ptr(passwd.pw_dir) }
            .to_bytes()
            .to_vec();
    };
    let home = PathBuf::from(OsString::from_vec(home_bytes));
    let metadata = fs::symlink_metadata(&home).ok()?;
    use std::os::unix::fs::MetadataExt;
    (home.is_absolute()
        && metadata.file_type().is_dir()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == uid)
        .then_some(home)
}

#[cfg(not(unix))]
fn stable_current_user_home() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn stable_windows_local_app_data() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::S_OK;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

    let mut path = std::ptr::null_mut();
    // SAFETY: SHGetKnownFolderPath initializes a COM-allocated NUL-terminated
    // string for the current process token; CoTaskMemFree releases it once.
    if unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut path) }
        != S_OK
        || path.is_null()
    {
        return None;
    }
    let mut length = 0;
    while unsafe { *path.add(length) } != 0 {
        length += 1;
    }
    let value = OsString::from_wide(unsafe { std::slice::from_raw_parts(path, length) });
    unsafe { CoTaskMemFree(path.cast()) };
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

#[cfg(not(windows))]
fn stable_windows_local_app_data() -> Option<PathBuf> {
    None
}

fn service_coordination_root_for_options(_options: &ServiceOptions) -> io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(root) = _options.service_coordination_root_override.as_ref() {
        return Ok(root.clone());
    }
    service_coordination_root()
}

pub(crate) fn try_acquire_service_cutover_shared_at(
    coordination_root: &Path,
) -> io::Result<TryRecorderInstanceLock> {
    try_acquire_named_private_root_lock(
        coordination_root,
        SERVICE_CUTOVER_LOCK_FILE,
        CoordinationLockMode::Shared,
    )
}

pub(crate) fn try_acquire_service_cutover_exclusive_at(
    coordination_root: &Path,
) -> io::Result<TryRecorderInstanceLock> {
    try_acquire_named_private_root_lock(
        coordination_root,
        SERVICE_CUTOVER_LOCK_FILE,
        CoordinationLockMode::Exclusive,
    )
}

/// Fails closed while a previous service replacement could not prove that an
/// automatic-start registration was removed. This current-user-global marker
/// has no freshness timeout and applies to every custom history directory.
pub(crate) fn ensure_no_recorder_cutover_blocker_at(coordination_root: &Path) -> io::Result<()> {
    // The shared/exclusive service gate validates this private root before the
    // check. `symlink_metadata` deliberately treats every leaf type (including
    // a malformed/symlink marker) as blocked.
    let path = coordination_root.join(RECORDER_CUTOVER_BLOCKER_FILE);
    match fs::symlink_metadata(&path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "source-aware history cutover is blocked by {}; run `codex-usage-monit service uninstall` or reinstall the service to verify that no legacy automatic-start registration remains",
                path.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
fn recorder_cutover_blocker_path(options: &ServiceOptions) -> io::Result<PathBuf> {
    Ok(service_coordination_root_for_options(options)?.join(RECORDER_CUTOVER_BLOCKER_FILE))
}

fn persist_recorder_cutover_blocker(options: &ServiceOptions, reason: &str) -> Result<()> {
    let coordination_root = service_coordination_root_for_options(options)?;
    validate_recorder_lock_state_root(
        &coordination_root,
        &fs::symlink_metadata(&coordination_root)?,
    )?;
    let path = coordination_root.join(RECORDER_CUTOVER_BLOCKER_FILE);
    let blocker = RecorderCutoverBlocker {
        schema_version: RECORDER_CUTOVER_BLOCKER_SCHEMA_VERSION,
        blocked_at: Utc::now(),
        platform: format!("{:?}", current_platform()).to_ascii_lowercase(),
        reason: reason.to_owned(),
    };
    let mut contents = serde_json::to_vec_pretty(&blocker)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');
    write_private_atomically(&path, &contents)
        .with_context(|| format!("could not persist cutover blocker {}", path.display()))
}

fn clear_recorder_cutover_blocker(options: &ServiceOptions) -> Result<()> {
    let coordination_root = service_coordination_root_for_options(options)?;
    validate_recorder_lock_state_root(
        &coordination_root,
        &fs::symlink_metadata(&coordination_root)?,
    )?;
    let path = coordination_root.join(RECORDER_CUTOVER_BLOCKER_FILE);
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent);
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("could not clear cutover blocker {}", path.display())),
    }
}

fn remove_current_service_definition_marker(options: &ServiceOptions) -> Result<()> {
    let coordination_root = service_coordination_root_for_options(options)?;
    validate_recorder_lock_state_root(
        &coordination_root,
        &fs::symlink_metadata(&coordination_root)?,
    )?;
    let path = coordination_root.join(CURRENT_SERVICE_DEFINITION_FILE);
    match fs::remove_file(&path) {
        Ok(()) => {
            sync_directory(&coordination_root);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("could not remove service trust marker {}", path.display())),
    }
}

fn persist_current_service_definition_marker(options: &ServiceOptions) -> Result<()> {
    let coordination_root = service_coordination_root_for_options(options)?;
    let fingerprint = match verified_current_service_definition(options)? {
        ServiceDefinitionObservation::Fingerprint(fingerprint) => fingerprint,
        ServiceDefinitionObservation::Absent => {
            bail!("the newly registered current service definition could not be found")
        }
        ServiceDefinitionObservation::Unverifiable(detail) => {
            bail!("the newly registered current service definition could not be verified: {detail}")
        }
    };
    let marker = CurrentServiceDefinitionMarker {
        schema_version: CURRENT_SERVICE_DEFINITION_SCHEMA_VERSION,
        platform: format!("{:?}", current_platform()).to_ascii_lowercase(),
        fingerprint,
    };
    let mut contents = serde_json::to_vec_pretty(&marker)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');
    let path = coordination_root.join(CURRENT_SERVICE_DEFINITION_FILE);
    write_private_atomically(&path, &contents)
        .with_context(|| format!("could not persist service trust marker {}", path.display()))
}

fn verified_current_service_definition(
    options: &ServiceOptions,
) -> Result<ServiceDefinitionObservation> {
    let observation = current_user_service_definition_observation()?;
    match current_platform() {
        Platform::MacOs => verify_expected_definition_fingerprint(
            observation,
            service_definition_fingerprint(launchd_plist(options).as_bytes()),
        ),
        Platform::Linux => verify_expected_definition_fingerprint(
            observation,
            service_definition_fingerprint(systemd_unit(options).as_bytes()),
        ),
        Platform::Windows => {
            let user_sid = windows_current_user_sid()?;
            let task_name = windows_task_name(&user_sid);
            let (status, actual) = windows_task_xml_bounded(&task_name)?;
            if !status.success() || actual.is_empty() {
                return Ok(ServiceDefinitionObservation::Unverifiable(format!(
                    "Task Scheduler definition export failed with status {status}"
                )));
            }
            verify_windows_task_xml_matches_options(&actual, options, &user_sid)?;
            Ok(ServiceDefinitionObservation::Fingerprint(
                canonical_windows_task_fingerprint(&actual, &user_sid)?,
            ))
        }
        Platform::Unsupported => Ok(ServiceDefinitionObservation::Absent),
    }
}

fn verify_expected_definition_fingerprint(
    observation: ServiceDefinitionObservation,
    expected: String,
) -> Result<ServiceDefinitionObservation> {
    match observation {
        ServiceDefinitionObservation::Fingerprint(actual) if actual == expected => {
            Ok(ServiceDefinitionObservation::Fingerprint(actual))
        }
        ServiceDefinitionObservation::Fingerprint(_) => Ok(
            ServiceDefinitionObservation::Unverifiable(
                "the installed manager definition differs from the exact definition generated for this installation"
                    .to_string(),
            ),
        ),
        other => Ok(other),
    }
}

pub(crate) fn ensure_service_definition_is_trusted_at(
    coordination_root: &Path,
    observation: ServiceDefinitionObservation,
) -> io::Result<()> {
    let observed_fingerprint = match observation {
        ServiceDefinitionObservation::Absent => return Ok(()),
        ServiceDefinitionObservation::Fingerprint(fingerprint) => fingerprint,
        ServiceDefinitionObservation::Unverifiable(detail) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "source-aware history cutover cannot verify the current-user service registration: {detail}; reinstall or uninstall the service before retrying"
                ),
            ));
        }
    };
    let path = coordination_root.join(CURRENT_SERVICE_DEFINITION_FILE);
    let contents = match read_private_regular_file_bounded(
        &path,
        SERVICE_TRUST_MARKER_MAX_BYTES,
        "service trust marker",
    ) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "an automatic-start service exists without a trusted current-version marker at {}; reinstall or uninstall the service before source-aware history cutover",
                    path.display()
                ),
            ));
        }
        Err(error) => return Err(error),
    };
    let marker = serde_json::from_slice::<CurrentServiceDefinitionMarker>(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let expected_platform = format!("{:?}", current_platform()).to_ascii_lowercase();
    if marker.schema_version != CURRENT_SERVICE_DEFINITION_SCHEMA_VERSION
        || marker.platform != expected_platform
        || marker.fingerprint != observed_fingerprint
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the automatic-start service does not match trusted current-version marker {}; reinstall or uninstall the service before source-aware history cutover",
                path.display()
            ),
        ));
    }
    Ok(())
}

pub fn read_recorder_status(path: &Path) -> io::Result<Option<RecorderStatusFile>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let status = serde_json::from_slice::<RecorderStatusFile>(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if status.schema_version > STATUS_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recorder status uses future schema version {}",
                status.schema_version
            ),
        ));
    }
    Ok(Some(status))
}

/// Returns a recent recorder that is not a source-aware writer for the exact
/// requested namespace. A v2 writer for another redaction namespace is also
/// incompatible because source metadata is shared at the profile level.
/// Callers must check this before starting the one-way migration; the
/// ownership lease alone cannot fence binaries released before v0.4.
pub fn incompatible_recorder_for_cutover(
    path: &Path,
    expected_namespace: &str,
    now: DateTime<Utc>,
) -> io::Result<Option<RecorderStatusFile>> {
    let Some(status) = read_recorder_status(path)? else {
        return Ok(None);
    };
    let exact_namespace = status
        .history_namespace
        .as_deref()
        .is_some_and(|namespace| namespace == expected_namespace);
    let compatible_v2 = exact_namespace && status.source_aware_v2_epoch().is_some();
    Ok((status.writer_may_be_active(now) && !compatible_v2).then_some(status))
}

pub fn write_recorder_status(path: &Path, status: &RecorderStatusFile) -> io::Result<()> {
    let mut contents = serde_json::to_vec_pretty(status)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');
    write_private_atomically(path, &contents)
}

pub fn install(options: &ServiceOptions) -> Result<ServiceStatus> {
    validate_options(options)?;
    let platform = current_platform();
    if platform == Platform::Unsupported {
        bail!(
            "background service installation is unsupported on this platform; run `codex-usage-monit record --foreground` under a supervisor"
        );
    }
    match platform {
        Platform::MacOs => replace_service_after_quiescence_with_start(
            options,
            || quiesce_launchd_for_install(options),
            || install_launchd(options),
            cleanup_launchd_registration,
            || persist_current_service_definition_marker(options),
            start_launchd,
        )?,
        Platform::Linux => replace_service_after_quiescence_with_start(
            options,
            || quiesce_systemd_for_install(options),
            || install_systemd(options),
            cleanup_systemd_registration,
            || persist_current_service_definition_marker(options),
            start_systemd,
        )?,
        Platform::Windows => replace_service_after_quiescence_with_start(
            options,
            || quiesce_windows_task_for_install(options),
            || install_windows_task(options),
            cleanup_windows_task_registration,
            || persist_current_service_definition_marker(options),
            start_windows_task,
        )?,
        Platform::Unsupported => unreachable!("unsupported platforms returned before installation"),
    }
    status(options)
}

/// Replacing a pre-v0.4 recorder must keep its last status visible until its
/// activity fence expires. A platform registration is not tied to the status
/// PID, and old binaries do not acquire the source-aware writer lease, so
/// deleting their only cutover fence after stopping an unrelated registration
/// can let another process migrate v1 while the old recorder still appends.
fn replace_service_after_quiescence(
    options: &ServiceOptions,
    quiesce: impl FnOnce() -> Result<ManagedServiceQuiescence>,
    install: impl FnOnce() -> Result<()>,
    fail_safe_cleanup: impl FnOnce() -> Result<()>,
) -> Result<()> {
    replace_service_after_quiescence_with_start(
        options,
        quiesce,
        install,
        fail_safe_cleanup,
        || Ok(()),
        || Ok(()),
    )
}

fn replace_service_after_quiescence_with_start(
    options: &ServiceOptions,
    quiesce: impl FnOnce() -> Result<ManagedServiceQuiescence>,
    install: impl FnOnce() -> Result<()>,
    fail_safe_cleanup: impl FnOnce() -> Result<()>,
    mut publish_definition_trust: impl FnMut() -> Result<()>,
    start: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let coordination_root = service_coordination_root_for_options(options)
        .context("could not resolve the current-user service coordination scope")?;
    let mutation_guard = match try_acquire_service_cutover_exclusive_at(&coordination_root)
        .with_context(|| {
            format!(
                "could not acquire the current-user service cutover gate at {}",
                coordination_root.display()
            )
        })? {
        TryRecorderInstanceLock::Acquired(guard) => guard,
        TryRecorderInstanceLock::Busy => bail!(
            "another service install/uninstall or history cutover already owns the current-user service registration scope"
        ),
    };
    // Publish the non-expiring fence before touching the platform manager or
    // its definition. If this durable write fails, the old automatic-start
    // registration is left completely untouched. Registration and activation
    // are separate below, so the new recorder is not deliberately started
    // until this blocker has been cleared.
    persist_recorder_cutover_blocker(options, "service replacement is in progress")?;
    // A pre-v0.4 installer cannot update this marker. Removing it before the
    // first manager mutation makes any crash or later legacy overwrite fail
    // closed until a current definition is registered and fingerprinted.
    remove_current_service_definition_marker(options)?;
    let quiescence = match quiesce() {
        Ok(quiescence) => quiescence,
        Err(error) => {
            return finish_failed_service_replacement(
                options,
                error,
                fail_safe_cleanup,
                None,
                None,
            );
        }
    };
    let proof = match prove_no_unmanaged_recorder(options, quiescence) {
        Ok(proof) => proof,
        Err(error) => {
            return finish_failed_service_replacement(
                options,
                error,
                fail_safe_cleanup,
                None,
                None,
            );
        }
    };
    if let Err(error) = remove_status_file(&options.status_file) {
        let RecorderQuiescenceProof {
            guard,
            previous_status,
        } = proof;
        return finish_failed_service_replacement(
            options,
            error,
            fail_safe_cleanup,
            previous_status.as_ref(),
            Some(guard),
        );
    }
    match install() {
        Ok(()) => {
            // The registered definition is now guaranteed to reference this
            // binary, so it is safe to clear the blocker before starting it.
            // A start failure leaves the current definition installed and is
            // reported without restoring any pre-v0.4 registration.
            let activation_ready = (|| -> Result<()> {
                publish_definition_trust()?;
                // Reobserve immediately before clearing the durable fence. This
                // catches a non-cooperating legacy installer that replaced the
                // manager definition after registration or the first snapshot.
                publish_definition_trust()?;
                clear_recorder_cutover_blocker(options)
            })();
            if let Err(error) = activation_ready {
                let RecorderQuiescenceProof {
                    guard,
                    previous_status,
                } = proof;
                return finish_failed_service_replacement(
                    options,
                    error,
                    fail_safe_cleanup,
                    previous_status.as_ref(),
                    Some(guard),
                );
            }
            drop(proof.guard);
            // A service recorder must enter through the shared activation gate
            // before touching any history. Release the exclusive replacement
            // side before asking the manager to start it.
            drop(mutation_guard);
            start()
        }
        Err(error) => {
            let RecorderQuiescenceProof {
                guard,
                previous_status,
            } = proof;
            finish_failed_service_replacement(
                options,
                error,
                fail_safe_cleanup,
                previous_status.as_ref(),
                Some(guard),
            )
        }
    }
}

fn finish_failed_service_replacement(
    options: &ServiceOptions,
    primary_error: anyhow::Error,
    fail_safe_cleanup: impl FnOnce() -> Result<()>,
    previous_status: Option<&RecorderStatusFile>,
    recorder_guard: Option<RecorderInstanceLockGuard>,
) -> Result<()> {
    let cleanup_result = fail_safe_cleanup();
    let blocker_result = if cleanup_result.is_ok() {
        remove_current_service_definition_marker(options)
            .and_then(|()| clear_recorder_cutover_blocker(options))
    } else {
        // The blocker was durably published before any manager mutation and
        // deliberately remains in place when cleanup is ambiguous.
        Ok(())
    };
    // Keep a replacement recorder fenced throughout manager cleanup, then
    // release the singleton before the status-restore helper rechecks it.
    drop(recorder_guard);
    let status_result = restore_status_after_failed_replacement(options, previous_status);

    if cleanup_result.is_ok() && blocker_result.is_ok() && status_result.is_ok() {
        return Err(primary_error);
    }
    bail!(
        "service replacement failed: {primary_error:#}; fail-safe registration cleanup: {}; persistent cutover blocker: {}; prior recorder status diagnostic: {}",
        result_summary(cleanup_result),
        result_summary(blocker_result),
        result_summary(status_result),
    )
}

struct RecorderQuiescenceProof {
    guard: RecorderInstanceLockGuard,
    previous_status: Option<RecorderStatusFile>,
}

fn prove_no_unmanaged_recorder(
    options: &ServiceOptions,
    _quiescence: ManagedServiceQuiescence,
) -> Result<RecorderQuiescenceProof> {
    // The manager can prove that its own process stopped, but a plist, unit,
    // or scheduled task is not tied to the PID in recorder-status.json. Keep
    // the singleton lock while classifying that status so a cooperating v2
    // recorder cannot enter between the two checks. Legacy recorders predate
    // this lock, so every recent legacy status remains a hard fence regardless
    // of whether a manager definition happened to exist.
    let guard = match try_acquire_recorder_instance_lock(&options.history_dir).with_context(
        || {
            format!(
                "could not verify recorder quiescence for {}",
                options.history_dir.display()
            )
        },
    )? {
        TryRecorderInstanceLock::Acquired(guard) => guard,
        TryRecorderInstanceLock::Busy => bail!(
            "an independent foreground recorder still owns {}; stop it before installing or uninstalling the service",
            options.history_dir.display()
        ),
    };

    let previous_status = read_recorder_status(&options.status_file).with_context(|| {
        format!(
            "could not verify recorder status {} before service replacement",
            options.status_file.display()
        )
    })?;
    if let Some(status) = previous_status.as_ref()
        && status.source_aware_v2_epoch().is_none()
        && status.writer_may_be_active(Utc::now())
    {
        let last_activity = status.last_activity_at();
        bail!(
            "a legacy recorder may still be active (pid {}, last activity {}, status {}); stop it and wait for its legacy activity fence to expire before installing or uninstalling the service",
            status.pid,
            last_activity.to_rfc3339(),
            options.status_file.display(),
        );
    }
    Ok(RecorderQuiescenceProof {
        guard,
        previous_status,
    })
}

fn restore_status_after_failed_replacement(
    options: &ServiceOptions,
    previous_status: Option<&RecorderStatusFile>,
) -> Result<()> {
    let Some(previous_status) = previous_status else {
        return Ok(());
    };
    let guard =
        match try_acquire_recorder_instance_lock(&options.history_dir).with_context(|| {
            format!(
                "could not re-check recorder quiescence for {}",
                options.history_dir.display()
            )
        })? {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            // A replacement recorder acquired the authoritative v2 lock despite a
            // later manager error. Never overwrite the status it is about to write.
            TryRecorderInstanceLock::Busy => return Ok(()),
        };
    let status_exists = match fs::symlink_metadata(&options.status_file) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "could not inspect recorder status {}",
                    options.status_file.display()
                )
            });
        }
    };
    if !status_exists {
        write_recorder_status(&options.status_file, previous_status).with_context(|| {
            format!(
                "could not restore recorder status {}",
                options.status_file.display()
            )
        })?;
    }
    drop(guard);
    Ok(())
}

pub fn status(options: &ServiceOptions) -> Result<ServiceStatus> {
    let recorder = read_recorder_status(&options.status_file).with_context(|| {
        format!(
            "could not read recorder status {}",
            options.status_file.display()
        )
    })?;
    let heartbeat = recorder
        .as_ref()
        .and_then(|status| status.last_history_heartbeat);
    let expected_namespace = expected_history_namespace(options);
    let namespace_mismatch = recorder
        .as_ref()
        .and_then(|status| status.history_namespace.as_deref())
        .filter(|namespace| *namespace != expected_namespace)
        .map(str::to_string);
    let heartbeat_recent = recorder
        .as_ref()
        .is_some_and(|status| status.heartbeat_is_recent(Utc::now()))
        && namespace_mismatch.is_none();
    let mut service_status = match current_platform() {
        Platform::MacOs => launchd_status(options, heartbeat, heartbeat_recent),
        Platform::Linux => systemd_status(options, heartbeat, heartbeat_recent),
        Platform::Windows => windows_task_status(options, heartbeat, heartbeat_recent),
        Platform::Unsupported => Ok(ServiceStatus {
            platform: "unsupported".to_string(),
            state: ServiceState::Unknown,
            installed: false,
            running: false,
            registration_path: None,
            last_history_heartbeat: heartbeat,
            heartbeat_recent,
            detail: "service management is unsupported; use record --foreground".to_string(),
        }),
    }?;
    if let Some(namespace) = namespace_mismatch {
        service_status.detail.push_str(&format!(
            "; recorder targets history namespace {namespace}, expected {expected_namespace}"
        ));
    }
    if let Some(error) = recorder.and_then(|status| status.last_error) {
        service_status
            .detail
            .push_str(&format!("; last recorder error: {error}"));
    }
    Ok(service_status)
}

fn expected_history_namespace(options: &ServiceOptions) -> String {
    HistoryStore::new_with_redaction(
        options.history_dir.clone(),
        &options.codex_home,
        options.redact_content,
    )
    .namespace()
    .to_string()
}

pub fn uninstall(options: &ServiceOptions) -> Result<ServiceStatus> {
    match current_platform() {
        Platform::MacOs => replace_service_after_quiescence(
            options,
            || quiesce_launchd_for_install(options),
            cleanup_launchd_registration,
            cleanup_launchd_registration,
        )?,
        Platform::Linux => replace_service_after_quiescence(
            options,
            || quiesce_systemd_for_install(options),
            cleanup_systemd_registration,
            cleanup_systemd_registration,
        )?,
        Platform::Windows => replace_service_after_quiescence(
            options,
            || quiesce_windows_task_for_install(options),
            cleanup_windows_task_registration,
            cleanup_windows_task_registration,
        )?,
        Platform::Unsupported => bail!("background service management is unsupported"),
    }
    status(options)
}

fn remove_status_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not remove recorder status {}", path.display()));
        }
    }
    Ok(())
}

fn result_summary<T, E: fmt::Display>(result: std::result::Result<T, E>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) => error.to_string(),
    }
}

fn validate_options(options: &ServiceOptions) -> Result<()> {
    if !options.executable.is_absolute() {
        bail!("service executable path must be absolute");
    }
    if !is_executable_file(&options.executable) {
        bail!(
            "service executable is unavailable or not executable: {}",
            options.executable.display()
        );
    }
    if !options.codex_home.is_absolute() {
        bail!("Codex home must be absolute for background service registration");
    }
    if let Some(codex_bin) = options.codex_bin.as_deref() {
        if !codex_bin.is_absolute() {
            bail!("Codex executable path must be absolute for background service registration");
        }
        if !is_executable_file(codex_bin) {
            bail!(
                "Codex executable is unavailable or not executable: {}",
                codex_bin.display()
            );
        }
    } else if !options.offline {
        bail!("Codex executable must be resolved for an online background recorder");
    }
    if !options.history_dir.is_absolute() || !options.status_file.is_absolute() {
        bail!("service state paths must be absolute");
    }
    let mut definition_paths = vec![
        ("service executable", &options.executable),
        ("Codex home", &options.codex_home),
        ("history directory", &options.history_dir),
        ("recorder status", &options.status_file),
    ];
    if let Some(codex_bin) = options.codex_bin.as_ref() {
        definition_paths.push(("Codex executable", codex_bin));
    }
    for (label, path) in definition_paths {
        if path.to_str().is_none() {
            bail!("{label} cannot be represented in a service definition");
        }
    }
    if options
        .perf_log
        .as_deref()
        .is_some_and(|path| path.to_str().is_none())
    {
        bail!("performance log path cannot be represented in a service definition");
    }
    if let Some(path) = options.remotes_config_file.as_deref() {
        if !path.is_absolute() {
            bail!("remote configuration path must be absolute for background service registration");
        }
        if path.to_str().is_none() {
            bail!("remote configuration path cannot be represented in a service definition");
        }
    }
    if let Some(path) = options.project_mapping_file.as_deref() {
        if !path.is_absolute() {
            bail!("project mapping path must be absolute for background service registration");
        }
        if path.to_str().is_none() {
            bail!("project mapping path cannot be represented in a service definition");
        }
    }
    if options
        .environment_path
        .as_deref()
        .is_some_and(|path| path.to_str().is_none())
    {
        bail!("PATH cannot be represented in a service definition");
    }
    Ok(())
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
    Windows,
    Unsupported,
}

/// Evidence returned by the platform manager before recorder diagnostics may
/// be removed. A missing registration proves nothing about an independently
/// launched `record --foreground` process, including legacy binaries that do
/// not participate in the recorder instance lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedServiceQuiescence {
    StoppedManagedRegistration,
    NoRegistration,
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unsupported
    }
}

pub(crate) fn current_user_service_definition_observation() -> Result<ServiceDefinitionObservation>
{
    match current_platform() {
        Platform::MacOs => {
            let path = launchd_registration_path()?;
            match read_private_regular_file_bounded(
                &path,
                SERVICE_DEFINITION_MAX_BYTES,
                "launchd service definition",
            ) {
                Ok(contents) => {
                    let expected_arguments = launchd_definition_arguments(&contents)?;
                    let expected_path = launchd_definition_environment_path(&contents)?;
                    let definition_id = verify_service_contract_arguments(
                        &expected_arguments
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                        None,
                    )?
                    .to_string();
                    verify_loaded_manager_identity(
                        Command::new("launchctl")
                            .args(["print", &format!("{}/{SERVICE_LABEL}", launchd_domain())]),
                        "launchd",
                        &definition_id,
                        &expected_arguments,
                        expected_path.as_deref(),
                    )?;
                    Ok(ServiceDefinitionObservation::Fingerprint(
                        service_definition_fingerprint(&contents),
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let target = format!("{}/{SERVICE_LABEL}", launchd_domain());
                    let observation =
                        run_launchd_operation(LaunchdOperation::PrintService { target })?;
                    if observation.success {
                        Ok(ServiceDefinitionObservation::Unverifiable(
                            "launchd still has the job loaded, but its plist is absent".to_string(),
                        ))
                    } else if launchd_print_reports_missing(&observation.detail) {
                        Ok(ServiceDefinitionObservation::Absent)
                    } else {
                        Ok(ServiceDefinitionObservation::Unverifiable(format!(
                            "launchd registration query failed: {}",
                            observation.detail
                        )))
                    }
                }
                Err(error) => Err(error).with_context(|| {
                    format!("could not read launchd definition {}", path.display())
                }),
            }
        }
        Platform::Linux => {
            let path = systemd_registration_path()?;
            match read_private_regular_file_bounded(
                &path,
                SERVICE_DEFINITION_MAX_BYTES,
                "systemd service definition",
            ) {
                Ok(contents) => {
                    let expected_arguments = systemd_definition_arguments(&contents)?;
                    let expected_path = systemd_definition_environment_path(&contents)?;
                    let definition_id = verify_service_contract_arguments(
                        &expected_arguments
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                        None,
                    )?
                    .to_string();
                    verify_loaded_manager_identity(
                        Command::new("systemctl").args([
                            "--user",
                            "show",
                            "--property=ExecStart",
                            "--property=Environment",
                            SYSTEMD_UNIT,
                        ]),
                        "systemd",
                        &definition_id,
                        &expected_arguments,
                        expected_path.as_deref(),
                    )?;
                    Ok(ServiceDefinitionObservation::Fingerprint(
                        service_definition_fingerprint(&contents),
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let output = Command::new("systemctl")
                        .args([
                            "--user",
                            "show",
                            "--property=LoadState",
                            "--value",
                            SYSTEMD_UNIT,
                        ])
                        .output()
                        .context("could not query the systemd service definition")?;
                    let load_state = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if output.status.success() && load_state == "not-found" {
                        Ok(ServiceDefinitionObservation::Absent)
                    } else if output.status.success() {
                        Ok(ServiceDefinitionObservation::Unverifiable(format!(
                            "systemd reports LoadState={load_state:?}, but the unit file is absent"
                        )))
                    } else {
                        Ok(ServiceDefinitionObservation::Unverifiable(format!(
                            "systemd registration query failed: {}",
                            output_detail(&output)
                        )))
                    }
                }
                Err(error) => Err(error).with_context(|| {
                    format!("could not read systemd definition {}", path.display())
                }),
            }
        }
        Platform::Windows => {
            let user_sid = windows_current_user_sid()?;
            let task_name = windows_task_name(&user_sid);
            let mut run = run_windows_task_operation;
            if !windows_task_is_installed_with(&task_name, &mut run)? {
                return Ok(ServiceDefinitionObservation::Absent);
            }
            let (status, contents) = windows_task_xml_bounded(&task_name)?;
            if !status.success() || contents.is_empty() {
                return Ok(ServiceDefinitionObservation::Unverifiable(format!(
                    "Task Scheduler definition export failed with status {status}"
                )));
            }
            Ok(ServiceDefinitionObservation::Fingerprint(
                canonical_windows_task_fingerprint(&contents, &user_sid)?,
            ))
        }
        Platform::Unsupported => Ok(ServiceDefinitionObservation::Absent),
    }
}

#[cfg(windows)]
fn windows_task_xml_bounded(task_name: &str) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut child = Command::new("schtasks.exe")
        .args(["/Query", "/TN", task_name, "/XML", "/HRESULT"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not export the Task Scheduler definition")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Task Scheduler definition export had no stdout pipe"))?;
    let mut contents = Vec::new();
    Read::by_ref(&mut stdout)
        .take(SERVICE_DEFINITION_MAX_BYTES.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > SERVICE_DEFINITION_MAX_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "Task Scheduler definition exceeds the {}-byte limit",
            SERVICE_DEFINITION_MAX_BYTES
        );
    }
    let status = child.wait()?;
    Ok((status, contents))
}

#[cfg(not(windows))]
fn windows_task_xml_bounded(_task_name: &str) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    bail!("Task Scheduler definition export is only available on Windows")
}

fn verify_loaded_manager_identity(
    command: &mut Command,
    manager: &str,
    expected_definition_id: &str,
    expected_arguments: &[String],
    expected_environment_path: Option<&str>,
) -> Result<()> {
    let output = run_command_stdout_bounded(command, SERVICE_DEFINITION_MAX_BYTES)
        .with_context(|| format!("could not inspect the loaded {manager} definition"))?;
    if !output.0.success() {
        bail!("the loaded {manager} definition could not be inspected");
    }
    let text = String::from_utf8(output.1)
        .with_context(|| format!("the loaded {manager} definition is not UTF-8"))?;
    let arguments = if manager == "launchd" {
        launchd_loaded_arguments(&text)?
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        systemd_loaded_arguments(&text)?
    };
    if arguments != expected_arguments {
        bail!(
            "the loaded {manager} executable or argument list differs from its trusted on-disk definition"
        );
    }
    let loaded_environment_path = if manager == "launchd" {
        launchd_loaded_environment_path(&text)?
    } else {
        systemd_loaded_environment_path(&text)?
    };
    if loaded_environment_path.as_deref() != expected_environment_path {
        bail!("the loaded {manager} PATH differs from its trusted on-disk definition");
    }
    let references = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let definition_id =
        verify_service_contract_arguments(&references, Some(expected_definition_id))?;
    debug_assert_eq!(definition_id, expected_definition_id);
    Ok(())
}

#[cfg(test)]
fn launchd_definition_id(contents: &[u8]) -> Result<String> {
    let arguments = launchd_definition_arguments(contents)?;
    verify_service_contract_arguments(
        &arguments.iter().map(String::as_str).collect::<Vec<_>>(),
        None,
    )
    .map(str::to_string)
}

fn launchd_definition_arguments(contents: &[u8]) -> Result<Vec<String>> {
    let document = std::str::from_utf8(contents).context("launchd definition is not UTF-8")?;
    let program_arguments_key = "<key>ProgramArguments</key>";
    if document.matches(program_arguments_key).count() != 1 {
        bail!("launchd definition lacks one exact ProgramArguments key");
    }
    let remainder = document
        .split_once(program_arguments_key)
        .expect("the unique key was counted")
        .1;
    let array_start = remainder
        .find("<array>")
        .ok_or_else(|| anyhow!("launchd ProgramArguments lacks an array"))?;
    let array = &remainder[array_start + "<array>".len()..];
    let array_end = array
        .find("</array>")
        .ok_or_else(|| anyhow!("launchd ProgramArguments array is unterminated"))?;
    xml_text_values(&array[..array_end], "string")?
        .into_iter()
        .map(|value| {
            quick_xml::escape::unescape(value)
                .map(|value| value.into_owned())
                .map_err(Into::into)
        })
        .collect()
}

fn launchd_definition_environment_path(contents: &[u8]) -> Result<Option<String>> {
    let document = std::str::from_utf8(contents).context("launchd definition is not UTF-8")?;
    let marker = "<key>PATH</key>";
    if document.matches(marker).count() > 1 {
        bail!("launchd definition contains duplicate PATH keys");
    }
    let Some(remainder) = document.split_once(marker).map(|(_, rest)| rest) else {
        return Ok(None);
    };
    let values = xml_text_values(remainder, "string")?;
    let value = values
        .first()
        .ok_or_else(|| anyhow!("launchd PATH lacks a string value"))?;
    Ok(Some(quick_xml::escape::unescape(value)?.into_owned()))
}

#[cfg(test)]
fn systemd_definition_id(contents: &[u8]) -> Result<String> {
    let arguments = systemd_definition_arguments(contents)?;
    verify_service_contract_arguments(
        &arguments.iter().map(String::as_str).collect::<Vec<_>>(),
        None,
    )
    .map(str::to_string)
}

fn systemd_definition_arguments(contents: &[u8]) -> Result<Vec<String>> {
    let unit = std::str::from_utf8(contents).context("systemd definition is not UTF-8")?;
    let exec_starts = unit
        .lines()
        .filter_map(|line| line.strip_prefix("ExecStart="))
        .collect::<Vec<_>>();
    if exec_starts.len() != 1 {
        bail!("systemd definition lacks one exact ExecStart directive");
    }
    parse_systemd_words(exec_starts[0], true)
}

fn systemd_definition_environment_path(contents: &[u8]) -> Result<Option<String>> {
    let unit = std::str::from_utf8(contents).context("systemd definition is not UTF-8")?;
    let environments = unit
        .lines()
        .filter_map(|line| line.strip_prefix("Environment="))
        .collect::<Vec<_>>();
    if environments.len() > 1 {
        bail!("systemd definition contains duplicate Environment directives");
    }
    let Some(environment) = environments.first() else {
        return Ok(None);
    };
    let words = parse_systemd_words(environment, true)?;
    let paths = words
        .iter()
        .filter_map(|value| value.strip_prefix("PATH="))
        .collect::<Vec<_>>();
    match paths.as_slice() {
        [] => Ok(None),
        [path] => Ok(Some((*path).to_string())),
        _ => bail!("systemd definition contains duplicate PATH variables"),
    }
}

fn systemd_loaded_arguments(text: &str) -> Result<Vec<String>> {
    let argv = text
        .split_once("argv[]=")
        .map(|(_, rest)| rest.split_once(" ;").map_or(rest, |(value, _)| value))
        .unwrap_or(text)
        .trim();
    parse_systemd_words(argv, false)
}

fn systemd_loaded_environment_path(text: &str) -> Result<Option<String>> {
    let values = text
        .lines()
        .filter_map(|line| line.strip_prefix("Environment="))
        .collect::<Vec<_>>();
    if values.len() != 1 {
        bail!("loaded systemd definition lacks one exact Environment property");
    }
    if values[0].trim().is_empty() {
        return Ok(None);
    }
    let words = parse_systemd_words(values[0], false)?;
    let paths = words
        .iter()
        .filter_map(|value| value.strip_prefix("PATH="))
        .collect::<Vec<_>>();
    match paths.as_slice() {
        [] => Ok(None),
        [path] => Ok(Some((*path).to_string())),
        _ => bail!("loaded systemd definition contains duplicate PATH variables"),
    }
}

fn launchd_loaded_environment_path(text: &str) -> Result<Option<String>> {
    let lines = text.lines().collect::<Vec<_>>();
    let starts = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (line.trim() == "environment = {").then_some(index))
        .collect::<Vec<_>>();
    if starts.len() != 1 {
        bail!("loaded launchd definition lacks one exact environment block");
    }
    let mut paths = Vec::new();
    for line in &lines[starts[0] + 1..] {
        let line = line.trim();
        if line == "}" {
            break;
        }
        if let Some((key, value)) = line.split_once(" => ")
            && key.trim_matches('"') == "PATH"
        {
            paths.push(value.trim_matches('"').to_string());
        }
    }
    match paths.as_slice() {
        [] => Ok(None),
        [path] => Ok(Some(path.clone())),
        _ => bail!("loaded launchd definition contains duplicate PATH variables"),
    }
}

fn parse_systemd_words(text: &str, generated: bool) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' => quoted = !quoted,
            '\\' if quoted => match chars.next() {
                Some('n') => word.push('\n'),
                Some('r') => word.push('\r'),
                Some(value) => word.push(value),
                None => bail!("systemd argument list ends in an escape"),
            },
            value if value.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            '$' | '%' if generated && chars.peek() == Some(&character) => {
                word.push(character);
                chars.next();
            }
            value => word.push(value),
        }
    }
    if quoted {
        bail!("systemd argument list has an unterminated quote");
    }
    if !word.is_empty() {
        words.push(word);
    }
    if words.is_empty() {
        bail!("systemd definition has an empty executable and argument list");
    }
    Ok(words)
}

fn launchd_loaded_arguments(text: &str) -> Result<Vec<&str>> {
    let lines = text.lines().collect::<Vec<_>>();
    let starts = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (line.trim() == "arguments = {").then_some(index))
        .collect::<Vec<_>>();
    if starts.len() != 1 {
        bail!("loaded launchd definition lacks one exact arguments block");
    }
    let mut arguments = Vec::new();
    let mut terminated = false;
    for line in &lines[starts[0] + 1..] {
        let line = line.trim();
        if line == "}" {
            terminated = true;
            break;
        }
        if line.is_empty() {
            continue;
        }
        let value = line
            .split_once(" = ")
            .filter(|(index, _)| index.bytes().all(|byte| byte.is_ascii_digit()))
            .map_or(line, |(_, value)| value)
            .trim_matches('"');
        arguments.push(value);
    }
    if !terminated {
        bail!("loaded launchd arguments block is unterminated");
    }
    Ok(arguments)
}

fn definition_id_from_tokenized_contract<'a>(
    text: &'a str,
    expected_definition_id: Option<&str>,
) -> Result<&'a str> {
    let tokens = text
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    verify_service_contract_arguments(&tokens, expected_definition_id)
}

fn verify_service_contract_arguments<'a>(
    arguments: &[&'a str],
    expected_definition_id: Option<&str>,
) -> Result<&'a str> {
    let protocol_flag = unique_contract_argument(arguments, "--service-cutover-protocol")?;
    if arguments.get(protocol_flag + 1) != Some(&SERVICE_CUTOVER_PROTOCOL) {
        bail!("service cutover protocol is not the immediate value of its argument");
    }
    let identity_flag = unique_contract_argument(arguments, SERVICE_DEFINITION_ID_ARGUMENT)?;
    let definition_id = arguments
        .get(identity_flag + 1)
        .ok_or_else(|| anyhow!("service definition identity argument lacks a value"))?;
    validate_service_definition_id(definition_id)?;
    if arguments
        .iter()
        .filter(|argument| **argument == *definition_id)
        .count()
        != 1
    {
        bail!("service definition identity value is not unique");
    }
    if expected_definition_id.is_some_and(|expected| expected != *definition_id) {
        bail!("service definition identity differs from the expected identity");
    }
    Ok(definition_id)
}

fn unique_contract_argument(arguments: &[&str], expected: &str) -> Result<usize> {
    let positions = arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| (*argument == expected).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() != 1 {
        bail!("service definition must contain one exact {expected:?} argument");
    }
    Ok(positions[0])
}

fn run_command_stdout_bounded(
    command: &mut Command,
    limit: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("missing stdout pipe"))?;
    let mut contents = Vec::new();
    Read::by_ref(&mut stdout)
        .take(limit.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > limit {
        let _ = child.kill();
        let _ = child.wait();
        bail!("manager definition output exceeds the {limit}-byte limit");
    }
    Ok((child.wait()?, contents))
}

fn service_definition_fingerprint(contents: &[u8]) -> String {
    Sha256::digest(contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_private_regular_file_bounded(
    path: &Path,
    max_bytes: u64,
    description: &str,
) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if recorder_metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{description} {} must be a regular non-link file",
                path.display()
            ),
        ));
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} {} exceeds the {max_bytes}-byte limit",
                path.display()
            ),
        ));
    }
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
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path).map_err(map_recorder_nofollow_error)?;
    let opened_metadata = file.metadata()?;
    validate_private_regular_file_metadata(&opened_metadata, description)?;
    #[cfg(windows)]
    validate_windows_private_file(path, &file, description)?;
    if opened_metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} {} exceeds the {max_bytes}-byte limit",
                path.display()
            ),
        ));
    }
    let capacity = usize::try_from(opened_metadata.len().min(max_bytes)).unwrap_or(0);
    let mut contents = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} {} exceeds the {max_bytes}-byte limit",
                path.display()
            ),
        ));
    }
    Ok(contents)
}

fn validate_private_regular_file_metadata(
    metadata: &fs::Metadata,
    description: &str,
) -> io::Result<()> {
    if recorder_metadata_is_link_or_reparse(metadata) || !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{description} must be a regular non-link file"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and retains no pointers.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid
            || metadata.permissions().mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{description} must be private, current-user-owned, and single-linked"),
            ));
        }
    }
    Ok(())
}

fn install_launchd(options: &ServiceOptions) -> Result<()> {
    let path = launchd_registration_path()?;
    write_private_atomically(&path, launchd_plist(options).as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    let domain = launchd_domain();
    let target = format!("{domain}/{SERVICE_LABEL}");
    // Clear the disabled override left by quiescing the old registration before
    // bootstrap: launchd refuses to load a disabled service. The replacement
    // flow still owns the recorder singleton lock through definition trust and
    // blocker publication, so an eager RunAtLoad/KeepAlive attempt cannot
    // acquire the history writer slot or persist any data.
    run_checked(
        Command::new("launchctl").args(["enable", &target]),
        "launchctl enable",
    )?;
    run_checked(
        Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&path),
        "launchctl bootstrap",
    )?;
    Ok(())
}

fn start_launchd() -> Result<()> {
    let target = format!("{}/{SERVICE_LABEL}", launchd_domain());
    run_checked(
        Command::new("launchctl").args(["kickstart", "-k", &target]),
        "launchctl kickstart",
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LaunchdOperation {
    DisableService { target: String },
    PrintDisabled { domain: String },
    BootoutRegistration { domain: String, path: PathBuf },
    BootoutService { target: String },
    PrintService { target: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchdOperationResult {
    success: bool,
    detail: String,
}

fn run_launchd_operation(operation: LaunchdOperation) -> Result<LaunchdOperationResult> {
    let (mut command, description, preserve_stdout) = match operation {
        LaunchdOperation::DisableService { target } => {
            let mut command = Command::new("launchctl");
            command.args(["disable", &target]);
            (command, "launchctl disable service", false)
        }
        LaunchdOperation::PrintDisabled { domain } => {
            let mut command = Command::new("launchctl");
            command.args(["print-disabled", &domain]);
            (command, "launchctl print-disabled", true)
        }
        LaunchdOperation::BootoutRegistration { domain, path } => {
            let mut command = Command::new("launchctl");
            command.args(["bootout", &domain]).arg(path);
            (command, "launchctl bootout registration", false)
        }
        LaunchdOperation::BootoutService { target } => {
            let mut command = Command::new("launchctl");
            command.args(["bootout", &target]);
            (command, "launchctl bootout service", false)
        }
        LaunchdOperation::PrintService { target } => {
            let mut command = Command::new("launchctl");
            command.args(["print", &target]);
            (command, "launchctl print service", false)
        }
    };
    if preserve_stdout {
        let (status, stdout) =
            run_command_stdout_bounded(&mut command, SERVICE_DEFINITION_MAX_BYTES)
                .with_context(|| format!("could not run {description}"))?;
        return Ok(LaunchdOperationResult {
            success: status.success(),
            detail: String::from_utf8(stdout)
                .with_context(|| format!("{description} output is not UTF-8"))?
                .trim()
                .to_string(),
        });
    }
    let output = command
        .output()
        .with_context(|| format!("could not run {description}"))?;
    let success = output.status.success();
    Ok(LaunchdOperationResult {
        success,
        detail: if success {
            String::new()
        } else {
            output_detail(&output)
        },
    })
}

fn quiesce_launchd_for_install(_options: &ServiceOptions) -> Result<ManagedServiceQuiescence> {
    let path = launchd_registration_path()?;
    let domain = launchd_domain();
    quiesce_launchd_registration(&path, &domain, false, run_launchd_operation)
}

fn uninstall_launchd_registration(
    path: &Path,
    domain: &str,
    run: impl FnMut(LaunchdOperation) -> Result<LaunchdOperationResult>,
) -> Result<ManagedServiceQuiescence> {
    quiesce_launchd_registration(path, domain, true, run)
}

fn quiesce_launchd_registration(
    path: &Path,
    domain: &str,
    remove_registration: bool,
    mut run: impl FnMut(LaunchdOperation) -> Result<LaunchdOperationResult>,
) -> Result<ManagedServiceQuiescence> {
    let target = format!("{domain}/{SERVICE_LABEL}");
    let disabled = run(LaunchdOperation::DisableService {
        target: target.clone(),
    })?;
    if !disabled.success {
        bail!(
            "could not disable launchd service {target} before unloading it: {}",
            disabled.detail
        );
    }
    let disabled_state = run(LaunchdOperation::PrintDisabled {
        domain: domain.to_string(),
    })?;
    verify_launchd_service_disabled(&disabled_state)?;
    let registration_existed = path.exists();
    let mut failures = Vec::new();
    let registration_booted_out = if registration_existed {
        let result = run(LaunchdOperation::BootoutRegistration {
            domain: domain.to_string(),
            path: path.to_path_buf(),
        })?;
        if !result.success {
            failures.push(format!("registration bootout failed: {}", result.detail));
        }
        result.success
    } else {
        false
    };
    let mut loaded_service_booted_out = false;
    if !registration_booted_out {
        let result = run(LaunchdOperation::BootoutService {
            target: target.clone(),
        })?;
        if !result.success {
            failures.push(format!("service bootout failed: {}", result.detail));
        }
        loaded_service_booted_out = result.success;
    }

    let inspection = run(LaunchdOperation::PrintService {
        target: target.clone(),
    })?;
    if inspection.success {
        let detail = std::iter::once(
            "launchctl reported success, but the service remains loaded".to_string(),
        )
        .chain(failures)
        .collect::<Vec<_>>()
        .join("; ");
        bail!("could not unload launchd service {target}: {detail}");
    }
    if !launchd_print_reports_missing(&inspection.detail) {
        bail!(
            "could not verify that launchd service {target} was unloaded: {}",
            inspection.detail
        );
    }

    if remove_registration {
        match fs::symlink_metadata(path) {
            Ok(_) => fs::remove_file(path)
                .with_context(|| format!("could not remove {}", path.display()))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(
        if registration_existed || registration_booted_out || loaded_service_booted_out {
            ManagedServiceQuiescence::StoppedManagedRegistration
        } else {
            ManagedServiceQuiescence::NoRegistration
        },
    )
}

fn verify_launchd_service_disabled(result: &LaunchdOperationResult) -> Result<()> {
    if !result.success {
        bail!(
            "could not inspect launchd disabled services: {}",
            result.detail
        );
    }
    let values = result
        .detail
        .lines()
        .filter_map(|line| line.trim().split_once("=>"))
        .filter_map(|(label, value)| {
            (label.trim().trim_matches('"') == SERVICE_LABEL).then_some(value.trim())
        })
        .collect::<Vec<_>>();
    if values.as_slice() != ["disabled"] {
        bail!("launchd did not report the recorder service as uniquely disabled");
    }
    Ok(())
}

fn cleanup_launchd_registration() -> Result<()> {
    let path = launchd_registration_path()?;
    let domain = launchd_domain();
    let _ = uninstall_launchd_registration(&path, &domain, run_launchd_operation)?;
    Ok(())
}

fn launchd_print_reports_missing(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "could not find service",
        "service not found",
        "no such process",
        "service is not loaded",
    ]
    .iter()
    .any(|message| detail.contains(message))
}

fn launchd_status(
    _options: &ServiceOptions,
    heartbeat: Option<DateTime<Utc>>,
    heartbeat_running: bool,
) -> Result<ServiceStatus> {
    let path = launchd_registration_path()?;
    let installed = path.is_file();
    let domain = launchd_domain();
    let output = Command::new("launchctl")
        .args(["print", &format!("{domain}/{SERVICE_LABEL}")])
        .output()
        .context("could not run launchctl print")?;
    let manager_running = launchd_output_reports_running(&output);
    Ok(service_status(
        "macos-launchd",
        installed,
        installed && manager_running,
        Some(path),
        heartbeat,
        manager_running,
        heartbeat_running,
    ))
}

fn launchd_output_reports_running(output: &Output) -> bool {
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == "state = running")
}

fn launchd_registration_path() -> Result<PathBuf> {
    let home = stable_current_user_home()
        .ok_or_else(|| anyhow!("the stable current-user home is unavailable"))?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist")))
}

fn launchd_domain() -> String {
    #[cfg(unix)]
    {
        // SAFETY: geteuid takes no arguments and has no memory safety preconditions.
        format!("gui/{}", unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        "gui/0".to_string()
    }
}

fn launchd_plist(options: &ServiceOptions) -> String {
    let mut arguments = vec![options.executable.as_os_str().to_owned()];
    arguments.extend(options.recorder_arguments());
    let arguments = arguments
        .iter()
        .map(|argument| {
            format!(
                "        <string>{}</string>",
                xml_escape(&argument.to_string_lossy())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let environment = options.environment_path.as_ref().map_or_else(String::new, |path| {
        format!(
            "    <key>EnvironmentVariables</key>\n    <dict>\n        <key>PATH</key>\n        <string>{}</string>\n    </dict>\n",
            xml_escape(&path.to_string_lossy())
        )
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}
    </array>
{environment}    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>ThrottleInterval</key>
    <integer>30</integer>
</dict>
</plist>
"#
    )
}

fn install_systemd(options: &ServiceOptions) -> Result<()> {
    let path = systemd_registration_path()?;
    install_systemd_registration(&path, systemd_unit(options).as_bytes(), |operation| {
        run_systemd_install_operation(operation)
    })?;
    verify_systemd_registration_staged()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemdInstallOperation {
    DaemonReload,
    Enable,
}

fn run_systemd_install_operation(operation: SystemdInstallOperation) -> Result<()> {
    match operation {
        SystemdInstallOperation::DaemonReload => run_checked(
            Command::new("systemctl").args(["--user", "daemon-reload"]),
            "systemctl --user daemon-reload",
        ),
        SystemdInstallOperation::Enable => run_checked(
            Command::new("systemctl").args(["--user", "enable", SYSTEMD_UNIT]),
            "systemctl --user enable",
        ),
    }
}

fn start_systemd() -> Result<()> {
    run_systemd_install_operation(SystemdInstallOperation::Enable)?;
    run_checked(
        Command::new("systemctl").args(["--user", "restart", SYSTEMD_UNIT]),
        "systemctl --user restart",
    )
}

fn install_systemd_registration(
    path: &Path,
    unit: &[u8],
    mut run: impl FnMut(SystemdInstallOperation) -> Result<()>,
) -> Result<()> {
    write_private_atomically(path, unit)
        .with_context(|| format!("could not write {}", path.display()))?;
    run(SystemdInstallOperation::DaemonReload)
}

fn quiesce_systemd_for_install(_options: &ServiceOptions) -> Result<ManagedServiceQuiescence> {
    let registration_existed = systemd_registration_path()?.is_file();
    quiesce_systemd_registration(registration_existed, run_systemd_quiesce_operation)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemdQuiesceOperation {
    DisableNow,
    InspectDefinition,
    VerifyInactive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SystemdQuiesceOperationResult {
    success: bool,
    code: Option<i32>,
    stdout: String,
    detail: String,
}

fn run_systemd_quiesce_operation(
    operation: SystemdQuiesceOperation,
) -> Result<SystemdQuiesceOperationResult> {
    let mut command = Command::new("systemctl");
    match operation {
        SystemdQuiesceOperation::DisableNow => {
            command.args(["--user", "disable", "--now", SYSTEMD_UNIT]);
        }
        SystemdQuiesceOperation::InspectDefinition => {
            command.args([
                "--user",
                "show",
                "--property=LoadState",
                "--property=UnitFileState",
                SYSTEMD_UNIT,
            ]);
        }
        SystemdQuiesceOperation::VerifyInactive => {
            command.args(["--user", "is-active", "--quiet", SYSTEMD_UNIT]);
        }
    }
    let output = command
        .output()
        .with_context(|| format!("could not run systemd operation {operation:?}"))?;
    Ok(SystemdQuiesceOperationResult {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        detail: output_detail(&output),
    })
}

fn quiesce_systemd_registration(
    registration_existed: bool,
    mut run: impl FnMut(SystemdQuiesceOperation) -> Result<SystemdQuiesceOperationResult>,
) -> Result<ManagedServiceQuiescence> {
    // A missing unit commonly makes disable --now return non-zero. Its exit
    // status is only diagnostic: the two manager queries below are the stable,
    // machine-readable proof that no enabled or running registration remains.
    let disable = run(SystemdQuiesceOperation::DisableNow)?;
    let definition = run(SystemdQuiesceOperation::InspectDefinition)?;
    let manager_has_definition = verify_systemd_definition_disabled(&definition)?;
    let active = run(SystemdQuiesceOperation::VerifyInactive)?;
    verify_systemd_inactive(&active).with_context(|| {
        format!(
            "systemd disable --now result was {}; recorder quiescence was not proven",
            if disable.success {
                "successful".to_string()
            } else {
                disable.detail.clone()
            }
        )
    })?;
    Ok(
        if registration_existed || manager_has_definition || disable.success {
            ManagedServiceQuiescence::StoppedManagedRegistration
        } else {
            ManagedServiceQuiescence::NoRegistration
        },
    )
}

fn verify_systemd_registration_staged() -> Result<()> {
    let definition = run_systemd_quiesce_operation(SystemdQuiesceOperation::InspectDefinition)?;
    if !verify_systemd_definition_disabled(&definition)? {
        bail!("the newly installed systemd definition is unexpectedly absent");
    }
    let active = run_systemd_quiesce_operation(SystemdQuiesceOperation::VerifyInactive)?;
    verify_systemd_inactive(&active)
}

fn verify_systemd_definition_disabled(result: &SystemdQuiesceOperationResult) -> Result<bool> {
    if !result.success {
        bail!(
            "could not inspect the systemd unit state: {}",
            result.detail
        );
    }
    let mut load_state = None;
    let mut unit_file_state = None;
    for line in result.stdout.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("systemd returned malformed unit state"))?;
        match key {
            "LoadState" if load_state.replace(value).is_none() => {}
            "UnitFileState" if unit_file_state.replace(value).is_none() => {}
            "LoadState" | "UnitFileState" => {
                bail!("systemd returned a duplicate unit-state property")
            }
            _ => bail!("systemd returned an unexpected unit-state property"),
        }
    }
    let load_state = load_state.ok_or_else(|| anyhow!("systemd omitted LoadState"))?;
    let unit_file_state =
        unit_file_state.ok_or_else(|| anyhow!("systemd omitted UnitFileState"))?;
    if load_state == "not-found" {
        return match unit_file_state {
            "" | "not-found" | "disabled" => Ok(false),
            _ => bail!(
                "systemd unit is absent but retains an unsafe unit-file state {unit_file_state:?}"
            ),
        };
    }
    if unit_file_state == "disabled" {
        return Ok(true);
    }
    bail!(
        "systemd unit is not proven disabled (LoadState={load_state:?}, UnitFileState={unit_file_state:?})"
    )
}

fn verify_systemd_inactive(result: &SystemdQuiesceOperationResult) -> Result<()> {
    match result.code {
        Some(3 | 4) => Ok(()),
        Some(0) => bail!("systemd recorder remains active"),
        code => bail!(
            "systemd recorder inactivity is unknown (status {code:?}): {}",
            result.detail
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemdCleanupOperation {
    DisableNow,
    DaemonReload,
    VerifyDefinitionDisabled,
    VerifyInactive,
}

fn run_systemd_cleanup_operation(operation: SystemdCleanupOperation) -> Result<()> {
    match operation {
        SystemdCleanupOperation::DisableNow => run_checked(
            Command::new("systemctl").args(["--user", "disable", "--now", SYSTEMD_UNIT]),
            "systemctl --user disable --now",
        ),
        SystemdCleanupOperation::DaemonReload => run_checked(
            Command::new("systemctl").args(["--user", "daemon-reload"]),
            "systemctl --user daemon-reload",
        ),
        SystemdCleanupOperation::VerifyDefinitionDisabled => {
            let definition =
                run_systemd_quiesce_operation(SystemdQuiesceOperation::InspectDefinition)?;
            verify_systemd_definition_disabled(&definition).map(|_| ())
        }
        SystemdCleanupOperation::VerifyInactive => {
            let output = Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT])
                .output()
                .context("could not verify systemd recorder quiescence after cleanup")?;
            match output.status.code() {
                Some(3 | 4) => Ok(()),
                code => bail!(
                    "systemd recorder was not proven inactive after cleanup (status {code:?}): {}",
                    output_detail(&output)
                ),
            }
        }
    }
}

fn cleanup_systemd_registration() -> Result<()> {
    let path = systemd_registration_path()?;
    cleanup_systemd_registration_at(&path, run_systemd_cleanup_operation)
}

fn cleanup_systemd_registration_at(
    path: &Path,
    mut run: impl FnMut(SystemdCleanupOperation) -> Result<()>,
) -> Result<()> {
    let disable = run(SystemdCleanupOperation::DisableNow);
    let remove = match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    let reload = run(SystemdCleanupOperation::DaemonReload);
    let definition = run(SystemdCleanupOperation::VerifyDefinitionDisabled);
    let inactive = run(SystemdCleanupOperation::VerifyInactive);
    if remove.is_ok() && reload.is_ok() && definition.is_ok() && inactive.is_ok() {
        // A localized/non-zero disable result is harmless once the unit file
        // is absent, the manager has reloaded, and no process remains active.
        return Ok(());
    }
    bail!(
        "systemd fail-safe cleanup was not proven complete; disable --now: {}; definition removal: {}; daemon-reload: {}; disabled/not-found verification: {}; inactive verification: {}",
        result_summary(disable),
        result_summary(remove),
        result_summary(reload),
        result_summary(definition),
        result_summary(inactive),
    )
}

fn systemd_status(
    _options: &ServiceOptions,
    heartbeat: Option<DateTime<Utc>>,
    heartbeat_running: bool,
) -> Result<ServiceStatus> {
    let path = systemd_registration_path()?;
    let installed = path.is_file();
    let output = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT])
        .output()
        .context("could not run systemctl --user is-active")?;
    let manager_running = output.status.success();
    Ok(service_status(
        "linux-systemd-user",
        installed,
        installed && manager_running,
        Some(path),
        heartbeat,
        manager_running,
        heartbeat_running,
    ))
}

fn systemd_registration_path() -> Result<PathBuf> {
    let home = stable_current_user_home()
        .ok_or_else(|| anyhow!("the stable current-user home is unavailable"))?;
    Ok(home.join(".config/systemd/user").join(SYSTEMD_UNIT))
}

fn systemd_unit(options: &ServiceOptions) -> String {
    let command = std::iter::once(options.executable.as_os_str())
        .chain(options.recorder_arguments().iter().map(OsString::as_os_str))
        .map(systemd_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let environment = options
        .environment_path
        .as_ref()
        .map_or_else(String::new, |path| {
            format!(
                "Environment={}\n",
                systemd_environment_quote(OsStr::new(&format!("PATH={}", path.to_string_lossy())))
            )
        });
    format!(
        r#"[Unit]
Description=Codex usage history recorder
After=default.target

[Service]
Type=simple
{environment}
ExecStart={command}
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
"#
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WindowsTaskOperation {
    Query { task_name: String },
    State { task_name: String },
    Disable { task_name: String },
    End { task_name: String },
    Delete { task_name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowsTaskOperationResult {
    success: bool,
    code: Option<i32>,
    detail: String,
}

fn run_windows_task_operation(
    operation: WindowsTaskOperation,
) -> Result<WindowsTaskOperationResult> {
    let (mut command, description, preserve_stdout) = match operation {
        WindowsTaskOperation::Query { task_name } => {
            let mut command = Command::new("schtasks.exe");
            command.args(["/Query", "/TN", &task_name, "/HRESULT"]);
            (command, "schtasks /Query /HRESULT", false)
        }
        WindowsTaskOperation::State { task_name } => {
            let mut command = Command::new("powershell.exe");
            command
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    WINDOWS_TASK_STATE_SCRIPT,
                ])
                .env(WINDOWS_TASK_STATE_ENV, task_name);
            (command, "Task Scheduler state query", true)
        }
        WindowsTaskOperation::Disable { task_name } => {
            let mut command = Command::new("schtasks.exe");
            command.args(["/Change", "/TN", &task_name, "/DISABLE"]);
            (command, "schtasks /Change /DISABLE", false)
        }
        WindowsTaskOperation::End { task_name } => {
            let mut command = Command::new("schtasks.exe");
            command.args(["/End", "/TN", &task_name]);
            (command, "schtasks /End", false)
        }
        WindowsTaskOperation::Delete { task_name } => {
            let mut command = Command::new("schtasks.exe");
            command.args(["/Delete", "/TN", &task_name, "/F"]);
            (command, "schtasks /Delete", false)
        }
    };
    let output = command
        .output()
        .with_context(|| format!("could not run {description}"))?;
    let success = output.status.success();
    Ok(WindowsTaskOperationResult {
        success,
        code: output.status.code(),
        detail: if success && preserve_stdout {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else if success {
            String::new()
        } else {
            output_detail(&output)
        },
    })
}

fn install_windows_task(options: &ServiceOptions) -> Result<()> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    register_windows_task(options, &task_name, &user_sid)?;
    let mut run = run_windows_task_operation;
    verify_windows_task_staged_with(&task_name, &mut run)
}

fn verify_windows_task_staged_with(
    task_name: &str,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<()> {
    let state = windows_task_state_with(task_name, run)?;
    if state != WINDOWS_TASK_STATE_DISABLED {
        bail!("new Task Scheduler registration is not staged in Disabled state (state {state})");
    }
    Ok(())
}

fn start_windows_task() -> Result<()> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    run_checked(
        Command::new("schtasks.exe").args(["/Change", "/TN", &task_name, "/ENABLE"]),
        "schtasks /Change /ENABLE",
    )?;
    run_checked(
        Command::new("schtasks.exe").args(["/Run", "/TN", &task_name]),
        "schtasks /Run",
    )
}

fn quiesce_windows_task_for_install(_options: &ServiceOptions) -> Result<ManagedServiceQuiescence> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    let mut run = run_windows_task_operation;
    quiesce_windows_task_registration(&task_name, &mut run, || {
        std::thread::sleep(std::time::Duration::from_millis(50));
    })
}

fn cleanup_windows_task_registration() -> Result<()> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    let mut run = run_windows_task_operation;
    let quiescence = quiesce_windows_task_registration(&task_name, &mut run, || {
        std::thread::sleep(std::time::Duration::from_millis(50));
    })?;
    if quiescence == ManagedServiceQuiescence::NoRegistration {
        return Ok(());
    }
    delete_stopped_windows_task_registration(&task_name, &mut run)
}

fn windows_task_status(
    _options: &ServiceOptions,
    heartbeat: Option<DateTime<Utc>>,
    heartbeat_recent: bool,
) -> Result<ServiceStatus> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    let mut run = run_windows_task_operation;
    windows_task_status_with(&task_name, heartbeat, heartbeat_recent, &mut run)
}

fn windows_task_status_with(
    task_name: &str,
    heartbeat: Option<DateTime<Utc>>,
    heartbeat_recent: bool,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<ServiceStatus> {
    let installed = windows_task_is_installed_with(task_name, run)?;
    let manager_running = installed && windows_task_is_running_with(task_name, run)?;
    let mut status = service_status(
        "windows-task-scheduler",
        installed,
        manager_running,
        None,
        heartbeat,
        manager_running,
        heartbeat_recent,
    );
    if installed && !manager_running && heartbeat.is_none() {
        status.state = ServiceState::Installed;
        status.detail = "registered; waiting for the first recorder heartbeat".to_string();
    }
    Ok(status)
}

fn quiesce_windows_task_registration(
    task_name: &str,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
    mut wait: impl FnMut(),
) -> Result<ManagedServiceQuiescence> {
    if !windows_task_is_installed_with(task_name, run)? {
        return Ok(ManagedServiceQuiescence::NoRegistration);
    }
    let disabled = run(WindowsTaskOperation::Disable {
        task_name: task_name.to_string(),
    })?;
    if !disabled.success {
        bail!(
            "could not disable Task Scheduler registration {task_name} before quiescence: {}",
            windows_task_exit_failure_detail(&disabled)
        );
    }
    end_windows_task_with(task_name, run)?;
    for attempt in 0..=40 {
        if !windows_task_is_installed_with(task_name, run)? {
            return Ok(ManagedServiceQuiescence::StoppedManagedRegistration);
        }
        let state = windows_task_state_with(task_name, run)?;
        if state == WINDOWS_TASK_STATE_DISABLED {
            return Ok(ManagedServiceQuiescence::StoppedManagedRegistration);
        }
        if !matches!(
            state,
            WINDOWS_TASK_STATE_RUNNING | WINDOWS_TASK_STATE_QUEUED
        ) {
            bail!(
                "Task Scheduler recorder did not remain disabled after quiescence (state {state})"
            );
        }
        if attempt < 40 {
            wait();
        }
    }
    bail!("Task Scheduler recorder remains active after schtasks /End")
}

fn windows_task_is_installed_with(
    task_name: &str,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<bool> {
    let result = run(WindowsTaskOperation::Query {
        task_name: task_name.to_string(),
    })?;
    if result.success {
        Ok(true)
    } else if result.code == Some(HRESULT_FILE_NOT_FOUND) {
        Ok(false)
    } else {
        bail!(
            "schtasks /Query failed for {task_name}: {}",
            windows_task_query_failure_detail(&result)
        )
    }
}

fn end_windows_task_with(
    task_name: &str,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<()> {
    let running = match windows_task_is_running_with(task_name, run) {
        Ok(running) => running,
        Err(state_error) => match windows_task_is_installed_with(task_name, run) {
            Ok(false) => return Ok(()),
            Ok(true) => return Err(state_error),
            Err(query_error) => bail!(
                "could not read Task Scheduler state for {task_name}: {state_error}; \
                 could not verify whether the task remains registered: {query_error}"
            ),
        },
    };
    if !running {
        return Ok(());
    }
    let result = run(WindowsTaskOperation::End {
        task_name: task_name.to_string(),
    })?;
    if result.success {
        return Ok(());
    }
    let end_failure = windows_task_exit_failure_detail(&result);
    match windows_task_is_installed_with(task_name, run) {
        Ok(false) => Ok(()),
        Ok(true) => match windows_task_is_running_with(task_name, run) {
            Ok(false) => Ok(()),
            Ok(true) => bail!("schtasks /End failed for {task_name}: {end_failure}"),
            Err(error) => bail!(
                "schtasks /End failed for {task_name}: {end_failure}; \
                 could not verify whether the task stopped: {error}"
            ),
        },
        Err(error) => bail!(
            "schtasks /End failed for {task_name}: {end_failure}; \
             could not verify whether the task remains registered: {error}"
        ),
    }
}

fn windows_task_is_running_with(
    task_name: &str,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<bool> {
    let state = windows_task_state_with(task_name, run)?;
    match state {
        WINDOWS_TASK_STATE_QUEUED | WINDOWS_TASK_STATE_RUNNING => Ok(true),
        WINDOWS_TASK_STATE_DISABLED | WINDOWS_TASK_STATE_READY => Ok(false),
        _ => bail!("Task Scheduler returned unknown state {state} for {task_name}"),
    }
}

fn windows_task_state_with(
    task_name: &str,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<i32> {
    let result = run(WindowsTaskOperation::State {
        task_name: task_name.to_string(),
    })?;
    if !result.success {
        bail!(
            "could not read Task Scheduler state for {task_name}: {}",
            windows_task_exit_failure_detail(&result)
        );
    }
    result.detail.trim().parse::<i32>().with_context(|| {
        format!(
            "Task Scheduler returned invalid state {:?} for {task_name}",
            result.detail
        )
    })
}

#[cfg(test)]
fn uninstall_windows_task_registration(
    task_name: &str,
    mut run: impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<ManagedServiceQuiescence> {
    let quiescence = quiesce_windows_task_registration(task_name, &mut run, || {})?;
    if quiescence == ManagedServiceQuiescence::NoRegistration {
        return Ok(quiescence);
    }
    delete_stopped_windows_task_registration(task_name, &mut run)?;
    Ok(quiescence)
}

fn delete_stopped_windows_task_registration(
    task_name: &str,
    run: &mut impl FnMut(WindowsTaskOperation) -> Result<WindowsTaskOperationResult>,
) -> Result<()> {
    if !windows_task_is_installed_with(task_name, run)? {
        return Ok(());
    }
    if windows_task_is_running_with(task_name, run)? {
        bail!("Task Scheduler recorder became active again before deletion");
    }
    let result = run(WindowsTaskOperation::Delete {
        task_name: task_name.to_string(),
    })?;
    if result.success {
        return match windows_task_is_installed_with(task_name, run) {
            Ok(false) => Ok(()),
            Ok(true) => bail!(
                "schtasks /Delete reported success for {task_name}, but the task remains registered"
            ),
            Err(error) => bail!(
                "schtasks /Delete reported success for {task_name}, but removal could not be verified: {error}"
            ),
        };
    }
    let delete_failure = windows_task_exit_failure_detail(&result);
    match windows_task_is_installed_with(task_name, run) {
        Ok(false) => Ok(()),
        Ok(true) => bail!("schtasks /Delete failed for {task_name}: {delete_failure}"),
        Err(error) => bail!(
            "schtasks /Delete failed for {task_name}: {delete_failure}; \
             could not verify whether the task remains registered: {error}"
        ),
    }
}

fn windows_task_query_failure_detail(result: &WindowsTaskOperationResult) -> String {
    match (result.code, result.detail.trim()) {
        (Some(code), "") => format!("HRESULT 0x{:08X}", code as u32),
        (Some(code), detail) => format!("HRESULT 0x{:08X}: {detail}", code as u32),
        (None, "") => "process terminated without an exit code".to_string(),
        (None, detail) => detail.to_string(),
    }
}

fn windows_task_exit_failure_detail(result: &WindowsTaskOperationResult) -> String {
    match (result.code, result.detail.trim()) {
        (Some(code), "") => format!("exit status {code}"),
        (Some(code), detail) => format!("exit status {code}: {detail}"),
        (None, "") => "process terminated without an exit code".to_string(),
        (None, detail) => detail.to_string(),
    }
}

fn register_windows_task(options: &ServiceOptions, task_name: &str, user_sid: &str) -> Result<()> {
    let xml = windows_task_xml(options, user_sid);
    register_windows_task_xml(options, task_name, xml.as_bytes())
}

fn register_windows_task_xml(options: &ServiceOptions, task_name: &str, xml: &[u8]) -> Result<()> {
    let parent = options
        .status_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let (xml_path, mut file) = create_temporary_file(parent, OsStr::new("recorder-task.xml"))
        .context("could not create temporary Task Scheduler XML")?;
    let command_xml_path = xml_path.clone();
    let registration_result = (move || -> Result<()> {
        file.write_all(xml)
            .context("could not write temporary Task Scheduler XML")?;
        file.sync_all()
            .context("could not flush temporary Task Scheduler XML")?;
        drop(file);
        let mut command = Command::new("schtasks.exe");
        command
            .args(["/Create", "/TN", task_name, "/XML"])
            .arg(&command_xml_path)
            .arg("/F");
        run_checked(&mut command, "schtasks /Create /XML")
    })();
    let cleanup_result = fs::remove_file(&xml_path);
    registration_result?;
    if let Err(error) = cleanup_result
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error)
            .with_context(|| format!("could not remove temporary {}", xml_path.display()));
    }
    Ok(())
}

fn windows_task_xml(options: &ServiceOptions, user_sid: &str) -> String {
    let command = xml_escape(&options.executable.to_string_lossy());
    let arguments = xml_escape(&windows_recorder_arguments(options));
    let user_sid = xml_escape(user_sid);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Continuously record Codex usage history for the current user.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user_sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="RecorderUser">
      <UserId>{user_sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>false</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>255</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="RecorderUser">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn verify_windows_task_xml_matches_options(
    actual: &[u8],
    options: &ServiceOptions,
    user_sid: &str,
) -> Result<()> {
    let actual = decode_windows_task_xml(actual)?;
    let parsed = parse_windows_task_xml(&actual)?;
    let expected_command = options.executable.to_string_lossy();
    let expected_arguments = windows_recorder_arguments(options);
    verify_windows_task_structure(&parsed, user_sid, true)?;
    if parsed.text("Task/Actions/Exec/Command")? != expected_command
        || parsed.text("Task/Actions/Exec/Arguments")? != expected_arguments
    {
        bail!("Task Scheduler definition has an unexpected recorder command or argument list");
    }
    let definition_id = definition_id_from_tokenized_contract(&expected_arguments, None)?;
    if definition_id != options.service_definition_id() {
        bail!("generated Task Scheduler definition has an unexpected service identity");
    }
    Ok(())
}

fn canonical_windows_task_fingerprint(actual: &[u8], user_sid: &str) -> Result<String> {
    let document = decode_windows_task_xml(actual)?;
    let parsed = parse_windows_task_xml(&document)?;
    verify_windows_task_structure(&parsed, user_sid, false)?;
    let command = parsed.text("Task/Actions/Exec/Command")?;
    let arguments = parsed.text("Task/Actions/Exec/Arguments")?;
    let trigger_user = parsed.text("Task/Triggers/LogonTrigger/UserId")?;
    let principal_user = parsed.text("Task/Principals/Principal/UserId")?;
    let definition_id = definition_id_from_tokenized_contract(arguments, None)?;
    let canonical = format!(
        "task-contract=source-aware-v2/windows-task-v1\ncommand={}\narguments={}\ndefinition-id={}\nuser-trigger={}\nuser-principal={}\ntrigger-enabled=true\nsettings-enabled=normalized\nlogon-type=InteractiveToken\nrun-level=LeastPrivilege\nmultiple-instances=IgnoreNew\nbattery-start=false\nbattery-stop=false\nhard-terminate=true\nstart-when-available=true\nnetwork-required=false\non-demand=true\nhidden=false\nidle=false\nwake=false\nexecution-limit=PT0S\npriority=7\nrestart-interval=PT1M\nrestart-count=255\n",
        command, arguments, definition_id, trigger_user, principal_user
    );
    Ok(service_definition_fingerprint(canonical.as_bytes()))
}

fn verify_windows_task_structure(
    task: &ParsedWindowsTaskXml,
    user_sid: &str,
    require_staged_disabled: bool,
) -> Result<()> {
    task.verify_critical_paths()?;
    for path in [
        "Task",
        "Task/Triggers",
        "Task/Triggers/LogonTrigger",
        "Task/Principals",
        "Task/Principals/Principal",
        "Task/Settings",
        "Task/Settings/RestartOnFailure",
        "Task/Actions",
        "Task/Actions/Exec",
    ] {
        task.require_single_node(path)?;
    }
    task.require_attribute("Task", "version", "1.2")?;
    task.require_attribute(
        "Task",
        "xmlns",
        "http://schemas.microsoft.com/windows/2004/02/mit/task",
    )?;
    task.require_attribute("Task/Principals/Principal", "id", "RecorderUser")?;
    task.require_attribute("Task/Actions", "Context", "RecorderUser")?;
    for path in ["Task", "Task/Principals/Principal", "Task/Actions"] {
        task.require_no_extra_attributes(path)?;
    }
    for (path, expected) in [
        ("Task/Triggers/LogonTrigger/UserId", user_sid),
        ("Task/Principals/Principal/UserId", user_sid),
        ("Task/Principals/Principal/LogonType", "InteractiveToken"),
        ("Task/Principals/Principal/RunLevel", "LeastPrivilege"),
        ("Task/Settings/RestartOnFailure/Interval", "PT1M"),
        ("Task/Settings/RestartOnFailure/Count", "255"),
    ] {
        if task.text(path)? != expected {
            bail!("Task Scheduler definition has an unexpected value at {path}");
        }
    }
    for (path, schema_default, expected) in [
        (
            "Task/Settings/MultipleInstancesPolicy",
            "IgnoreNew",
            "IgnoreNew",
        ),
        ("Task/Settings/DisallowStartIfOnBatteries", "true", "false"),
        ("Task/Settings/StopIfGoingOnBatteries", "true", "false"),
        ("Task/Settings/AllowHardTerminate", "true", "true"),
        ("Task/Settings/StartWhenAvailable", "false", "true"),
        ("Task/Settings/RunOnlyIfNetworkAvailable", "false", "false"),
        ("Task/Settings/AllowStartOnDemand", "true", "true"),
        ("Task/Settings/Hidden", "false", "false"),
        ("Task/Settings/RunOnlyIfIdle", "false", "false"),
        ("Task/Settings/WakeToRun", "false", "false"),
        ("Task/Settings/ExecutionTimeLimit", "PT72H", "PT0S"),
        ("Task/Settings/Priority", "7", "7"),
        ("Task/Triggers/LogonTrigger/Enabled", "true", "true"),
    ] {
        if task.text_or_default(path, schema_default)? != expected {
            bail!("Task Scheduler definition has an unexpected value at {path}");
        }
    }
    let settings_enabled = task.text_or_default("Task/Settings/Enabled", "true")?;
    if !matches!(settings_enabled, "true" | "false")
        || (require_staged_disabled && settings_enabled != "false")
    {
        bail!("Task Scheduler definition has an unexpected Settings/Enabled state");
    }
    definition_id_from_tokenized_contract(task.text("Task/Actions/Exec/Arguments")?, None)?;
    Ok(())
}

#[derive(Debug)]
struct ParsedWindowsTaskXml {
    nodes: HashMap<String, usize>,
    attributes: HashMap<String, HashMap<String, String>>,
    texts: HashMap<String, String>,
}

impl ParsedWindowsTaskXml {
    fn verify_critical_paths(&self) -> Result<()> {
        const ACTIONS: &[&str] = &[
            "Task/Actions",
            "Task/Actions/Exec",
            "Task/Actions/Exec/Command",
            "Task/Actions/Exec/Arguments",
        ];
        const TRIGGERS: &[&str] = &[
            "Task/Triggers",
            "Task/Triggers/LogonTrigger",
            "Task/Triggers/LogonTrigger/Enabled",
            "Task/Triggers/LogonTrigger/UserId",
        ];
        const PRINCIPALS: &[&str] = &[
            "Task/Principals",
            "Task/Principals/Principal",
            "Task/Principals/Principal/UserId",
            "Task/Principals/Principal/LogonType",
            "Task/Principals/Principal/RunLevel",
        ];
        const SETTINGS: &[&str] = &[
            "Task/Settings",
            "Task/Settings/MultipleInstancesPolicy",
            "Task/Settings/DisallowStartIfOnBatteries",
            "Task/Settings/StopIfGoingOnBatteries",
            "Task/Settings/AllowHardTerminate",
            "Task/Settings/StartWhenAvailable",
            "Task/Settings/RunOnlyIfNetworkAvailable",
            "Task/Settings/AllowStartOnDemand",
            "Task/Settings/Enabled",
            "Task/Settings/Hidden",
            "Task/Settings/RunOnlyIfIdle",
            "Task/Settings/WakeToRun",
            "Task/Settings/ExecutionTimeLimit",
            "Task/Settings/Priority",
            "Task/Settings/RestartOnFailure",
            "Task/Settings/RestartOnFailure/Interval",
            "Task/Settings/RestartOnFailure/Count",
            "Task/Settings/UseUnifiedSchedulingEngine",
            "Task/Settings/DisallowStartOnRemoteAppSession",
            "Task/Settings/DeleteExpiredTaskAfter",
        ];
        for path in self.nodes.keys() {
            let path = path.as_str();
            if (path.starts_with("Task/Actions") && !ACTIONS.contains(&path))
                || (path.starts_with("Task/Triggers") && !TRIGGERS.contains(&path))
                || (path.starts_with("Task/Principals") && !PRINCIPALS.contains(&path))
                || (path.starts_with("Task/Settings") && !SETTINGS.contains(&path))
            {
                bail!("Task Scheduler definition contains unsupported critical node {path}");
            }
        }
        for (path, default) in [
            ("Task/Settings/UseUnifiedSchedulingEngine", "false"),
            ("Task/Settings/DisallowStartOnRemoteAppSession", "false"),
            ("Task/Settings/DeleteExpiredTaskAfter", "PT0S"),
        ] {
            if self.nodes.contains_key(path) && self.text(path)? != default {
                bail!("Task Scheduler definition has a non-default optional value at {path}");
            }
        }
        Ok(())
    }

    fn require_single_node(&self, path: &str) -> Result<()> {
        if self.nodes.get(path) != Some(&1) {
            bail!("Task Scheduler definition must contain one exact {path} node");
        }
        Ok(())
    }

    fn text(&self, path: &str) -> Result<&str> {
        self.require_single_node(path)?;
        self.texts
            .get(path)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("Task Scheduler definition lacks text at {path}"))
    }

    fn text_or_default<'a>(&'a self, path: &str, default: &'a str) -> Result<&'a str> {
        match self.nodes.get(path).copied().unwrap_or_default() {
            0 => Ok(default),
            1 => self
                .texts
                .get(path)
                .map(String::as_str)
                .ok_or_else(|| anyhow!("Task Scheduler definition lacks text at {path}")),
            _ => bail!("Task Scheduler definition must contain at most one {path} node"),
        }
    }

    fn require_attribute(&self, path: &str, name: &str, expected: &str) -> Result<()> {
        if self
            .attributes
            .get(path)
            .and_then(|attributes| attributes.get(name))
            .is_none_or(|value| value != expected)
        {
            bail!("Task Scheduler definition has an unexpected {name} attribute at {path}");
        }
        Ok(())
    }

    fn require_no_extra_attributes(&self, path: &str) -> Result<()> {
        let expected = match path {
            "Task" => &["version", "xmlns"][..],
            "Task/Principals/Principal" => &["id"][..],
            "Task/Actions" => &["Context"][..],
            _ => &[],
        };
        let actual = self.attributes.get(path).map_or(0, HashMap::len);
        if actual != expected.len() {
            bail!("Task Scheduler definition has unexpected attributes at {path}");
        }
        Ok(())
    }
}

fn parse_windows_task_xml(document: &str) -> Result<ParsedWindowsTaskXml> {
    let mut reader = Reader::from_str(document);
    let mut stack = Vec::<String>::new();
    let mut nodes = HashMap::<String, usize>::new();
    let mut attributes = HashMap::<String, HashMap<String, String>>::new();
    let mut texts = HashMap::<String, String>::new();
    let mut declaration_seen = false;
    loop {
        match reader
            .read_event()
            .context("could not parse Task Scheduler XML")?
        {
            Event::Decl(_) if stack.is_empty() && !declaration_seen => declaration_seen = true,
            Event::Start(element) => {
                let name = std::str::from_utf8(element.name().as_ref())
                    .context("Task Scheduler XML element name is not UTF-8")?
                    .to_string();
                if name.contains(':') {
                    bail!("Task Scheduler XML namespace prefixes are not accepted");
                }
                if stack.is_empty() && name != "Task" {
                    bail!("Task Scheduler XML contains a non-Task top-level element");
                }
                stack.push(name);
                let path = stack.join("/");
                *nodes.entry(path.clone()).or_default() += 1;
                if nodes[&path] > 1 {
                    bail!("Task Scheduler XML contains a duplicate {path} node");
                }
                let mut element_attributes = HashMap::new();
                for attribute in element.attributes().with_checks(true) {
                    let attribute = attribute.context("invalid Task Scheduler XML attribute")?;
                    let name = std::str::from_utf8(attribute.key.as_ref())
                        .context("Task Scheduler XML attribute name is not UTF-8")?
                        .to_string();
                    let value = attribute
                        .decode_and_unescape_value(reader.decoder())
                        .context("invalid Task Scheduler XML attribute value")?
                        .into_owned();
                    if element_attributes.insert(name.clone(), value).is_some() {
                        bail!("Task Scheduler XML contains duplicate attribute {name}");
                    }
                }
                if !element_attributes.is_empty() {
                    attributes.insert(path, element_attributes);
                }
            }
            Event::Text(text) => {
                let decoded = text
                    .xml10_content()
                    .context("invalid Task Scheduler XML text encoding")?;
                let value = unescape(decoded.as_ref())
                    .context("invalid Task Scheduler XML entity")?
                    .into_owned();
                if value.bytes().all(|byte| byte.is_ascii_whitespace()) {
                    continue;
                }
                if stack.is_empty() {
                    bail!("Task Scheduler XML contains text outside its Task root");
                }
                let path = stack.join("/");
                texts.entry(path).or_default().push_str(&value);
            }
            Event::GeneralRef(reference) => {
                if stack.is_empty() {
                    bail!("Task Scheduler XML contains an entity outside its Task root");
                }
                let value = if let Some(character) = reference
                    .resolve_char_ref()
                    .context("invalid Task Scheduler XML character reference")?
                {
                    character.to_string()
                } else {
                    match reference
                        .decode()
                        .context("invalid Task Scheduler XML entity name")?
                        .as_ref()
                    {
                        "amp" => "&".to_string(),
                        "lt" => "<".to_string(),
                        "gt" => ">".to_string(),
                        "quot" => "\"".to_string(),
                        "apos" => "'".to_string(),
                        entity => bail!("unsupported Task Scheduler XML entity &{entity};"),
                    }
                };
                texts.entry(stack.join("/")).or_default().push_str(&value);
            }
            Event::End(element) => {
                let closing = std::str::from_utf8(element.name().as_ref())
                    .context("Task Scheduler XML closing name is not UTF-8")?
                    .to_string();
                if stack
                    .last()
                    .is_none_or(|open| open.as_str() != closing.as_str())
                {
                    bail!("Task Scheduler XML has a mismatched closing tag {closing}");
                }
                stack
                    .pop()
                    .ok_or_else(|| anyhow!("Task Scheduler XML has an unmatched closing tag"))?;
            }
            Event::Eof if stack.is_empty() => break,
            Event::Eof => bail!("Task Scheduler XML ended inside an element"),
            Event::Comment(_) | Event::CData(_) | Event::DocType(_) | Event::PI(_) => {
                bail!("Task Scheduler XML contains a disallowed non-structural event")
            }
            Event::Empty(_) | Event::Decl(_) => {
                bail!("Task Scheduler XML contains an unsupported event")
            }
        }
    }
    if nodes.get("Task") != Some(&1) {
        bail!("Task Scheduler XML must have exactly one Task root");
    }
    Ok(ParsedWindowsTaskXml {
        nodes,
        attributes,
        texts,
    })
}

fn decode_windows_task_xml(contents: &[u8]) -> Result<String> {
    if let Some(bytes) = contents.strip_prefix(&[0xff, 0xfe]) {
        if bytes.len() % 2 != 0 {
            bail!("Task Scheduler returned truncated UTF-16LE XML");
        }
        let units = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).context("Task Scheduler returned invalid UTF-16LE XML");
    }
    if let Some(bytes) = contents.strip_prefix(&[0xfe, 0xff]) {
        if bytes.len() % 2 != 0 {
            bail!("Task Scheduler returned truncated UTF-16BE XML");
        }
        let units = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).context("Task Scheduler returned invalid UTF-16BE XML");
    }
    let contents = contents
        .strip_prefix(&[0xef, 0xbb, 0xbf])
        .unwrap_or(contents);
    String::from_utf8(contents.to_vec()).context("Task Scheduler returned invalid UTF-8 XML")
}

fn xml_text_values<'a>(document: &'a str, element: &str) -> Result<Vec<&'a str>> {
    let open = format!("<{element}>");
    let close = format!("</{element}>");
    let mut values = Vec::new();
    let mut remainder = document;
    while let Some(start) = remainder.find(&open) {
        remainder = &remainder[start + open.len()..];
        let end = remainder
            .find(&close)
            .ok_or_else(|| anyhow!("Task Scheduler definition has an unterminated <{element}>"))?;
        values.push(&remainder[..end]);
        remainder = &remainder[end + close.len()..];
    }
    Ok(values)
}

fn windows_recorder_arguments(options: &ServiceOptions) -> String {
    let mut arguments = Vec::new();
    if let Some(path) = options.environment_path.as_ref() {
        let mut service_path = OsString::from("--service-path=");
        service_path.push(path);
        arguments.push(service_path);
    }
    arguments.extend(options.recorder_arguments());
    arguments
        .iter()
        .map(OsString::as_os_str)
        .map(quote_windows_argument)
        .collect::<Vec<_>>()
        .join(" ")
}

fn windows_task_name(user_sid: &str) -> String {
    format!(
        "{WINDOWS_TASK_PREFIX}-{:016x}",
        stable_hash(user_sid.as_bytes())
    )
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(windows)]
fn windows_current_user_sid() -> Result<String> {
    use std::mem;
    use std::ptr;
    use std::slice;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::core::PWSTR;

    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token points to writable storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error()).context("could not open the current process token");
    }
    let result = (|| -> Result<String> {
        let mut required = 0_u32;
        // SAFETY: a zero-length query with a null buffer asks Windows for the required size.
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(io::Error::last_os_error())
                .context("could not size the current user token");
        }
        let word_size = mem::size_of::<usize>();
        let words = usize::try_from(required)
            .unwrap_or(usize::MAX)
            .saturating_add(word_size - 1)
            / word_size;
        let mut buffer = vec![0_usize; words];
        // SAFETY: buffer is aligned for TOKEN_USER and has at least `required` writable bytes.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error())
                .context("could not read the current user token");
        }
        // SAFETY: GetTokenInformation(TokenUser) initialized the buffer as TOKEN_USER.
        let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut string_sid: PWSTR = ptr::null_mut();
        // SAFETY: token_user contains the SID owned by the token buffer and string_sid is writable.
        if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut string_sid) } == 0 {
            return Err(io::Error::last_os_error())
                .context("could not format the current user SID");
        }
        let mut length = 0_usize;
        // SAFETY: ConvertSidToStringSidW returns a valid null-terminated UTF-16 allocation.
        unsafe {
            while *string_sid.add(length) != 0 {
                length += 1;
            }
        }
        // SAFETY: the allocation contains `length` initialized UTF-16 code units.
        let sid = String::from_utf16_lossy(unsafe { slice::from_raw_parts(string_sid, length) });
        // SAFETY: ConvertSidToStringSidW allocated string_sid with LocalAlloc.
        unsafe {
            LocalFree(string_sid.cast());
        }
        Ok(sid)
    })();
    // SAFETY: token was returned by OpenProcessToken and has not yet been closed.
    unsafe {
        CloseHandle(token);
    }
    result
}

#[cfg(not(windows))]
fn windows_current_user_sid() -> Result<String> {
    bail!("the current Windows user SID is unavailable on this platform")
}

fn service_status(
    platform: &'static str,
    installed: bool,
    running: bool,
    registration_path: Option<PathBuf>,
    heartbeat: Option<DateTime<Utc>>,
    manager_reports_running: bool,
    heartbeat_recent: bool,
) -> ServiceStatus {
    let state = if !installed {
        ServiceState::NotInstalled
    } else if running {
        ServiceState::Running
    } else {
        ServiceState::Stopped
    };
    let detail = if manager_reports_running && heartbeat_recent {
        "service manager reports the recorder active; recent history heartbeat observed".to_string()
    } else if manager_reports_running {
        "service manager reports the recorder active, but no recent history heartbeat was observed"
            .to_string()
    } else if running {
        "recent recorder heartbeat observed".to_string()
    } else if installed && heartbeat_recent {
        "registered manager is not active; the last heartbeat is still recent".to_string()
    } else if installed {
        "registered, but no recent recorder heartbeat was observed".to_string()
    } else {
        "no recorder registration found".to_string()
    };
    ServiceStatus {
        platform: platform.to_string(),
        state,
        installed,
        running,
        registration_path,
        last_history_heartbeat: heartbeat,
        heartbeat_recent,
        detail,
    }
}

fn run_checked(command: &mut Command, description: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("could not run {description}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("{description} failed: {}", output_detail(&output))
    }
}

fn output_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stdout
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote(value: &OsStr) -> String {
    systemd_quote_inner(value, true)
}

fn systemd_environment_quote(value: &OsStr) -> String {
    systemd_quote_inner(value, false)
}

fn systemd_quote_inner(value: &OsStr, escape_dollar: bool) -> String {
    let value = value.to_string_lossy();
    let mut escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('%', "%%");
    if escape_dollar {
        escaped = escaped.replace('$', "$$");
    }
    format!("\"{escaped}\"")
}

/// Quotes one argv element according to the CommandLineToArgvW/CRT rules.
fn quote_windows_argument(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return value.into_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0_usize;
    for character in value.chars() {
        if character == '\\' {
            backslashes += 1;
            continue;
        }
        if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2).saturating_add(1)));
            quoted.push('"');
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(character);
        }
        backslashes = 0;
    }
    quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2)));
    quoted.push('"');
    quoted
}

fn recorder_state_root(history_dir: &Path) -> io::Result<PathBuf> {
    history_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "history directory must have a state-root parent",
            )
        })
}

fn held_recorder_locks() -> std::sync::MutexGuard<'static, HashSet<RecorderLockIdentity>> {
    HELD_RECORDER_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn prepare_recorder_lock_state_root(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => return validate_recorder_lock_state_root(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(not(unix))]
    {
        #[cfg(windows)]
        reject_windows_recorder_lock_reparse_components(path)?;
        fs::create_dir_all(path)?;
    }

    let metadata = fs::symlink_metadata(path)?;
    validate_recorder_lock_state_root(path, &metadata)
}

fn validate_recorder_lock_state_root(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if recorder_metadata_is_link_or_reparse(metadata) {
        return Err(invalid_recorder_lock_data(format!(
            "recorder state root {} must not be a symbolic link or reparse point",
            path.display()
        )));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_recorder_lock_data(format!(
            "recorder state root {} must be a directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and retains no pointers.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recorder state root must be owned by the current user",
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("recorder state root {} must have mode 0700", path.display()),
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_private_directory(path, "recorder state root")?;
    Ok(())
}

fn open_recorder_lock_file(state_root: &Path, file_name: &str) -> io::Result<File> {
    let path = state_root.join(file_name);
    validate_recorder_lock_state_root(state_root, &fs::symlink_metadata(state_root)?)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_recorder_lock_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
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
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        // Denying delete sharing prevents another process from displacing the
        // coordination inode while a guard exists.
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(&path).map_err(map_recorder_nofollow_error)?;
    validate_recorder_lock_metadata(&file.metadata()?)?;
    #[cfg(windows)]
    validate_windows_private_file(&path, &file, "recorder instance lock")?;
    let identity = recorder_lock_identity(&file)?;
    validate_opened_recorder_lock(state_root, file_name, &file, identity)?;
    Ok(file)
}

fn validate_opened_recorder_lock(
    state_root: &Path,
    file_name: &str,
    opened: &File,
    expected_identity: RecorderLockIdentity,
) -> io::Result<()> {
    validate_recorder_lock_state_root(state_root, &fs::symlink_metadata(state_root)?)?;
    validate_recorder_lock_metadata(&opened.metadata()?)?;
    if recorder_lock_identity(opened)? != expected_identity {
        return Err(invalid_recorder_lock_data(
            "recorder instance lock identity changed",
        ));
    }

    let path = state_root.join(file_name);
    validate_recorder_lock_metadata(&fs::symlink_metadata(&path)?)?;
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
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let current = options.open(&path).map_err(map_recorder_nofollow_error)?;
    validate_recorder_lock_metadata(&current.metadata()?)?;
    #[cfg(windows)]
    {
        validate_windows_private_file(&path, opened, "recorder instance lock")?;
        validate_windows_private_file(&path, &current, "recorder instance lock")?;
    }
    if recorder_lock_identity(&current)? != expected_identity {
        return Err(invalid_recorder_lock_data(
            "recorder instance lock changed while it was being opened",
        ));
    }
    Ok(())
}

fn validate_recorder_lock_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if recorder_metadata_is_link_or_reparse(metadata) {
        return Err(invalid_recorder_lock_data(
            "recorder instance lock must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_recorder_lock_data(
            "recorder instance lock must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and retains no pointers.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recorder instance lock must be owned by the current user",
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recorder instance lock must have mode 0600",
            ));
        }
        if metadata.nlink() != 1 {
            return Err(invalid_recorder_lock_data(
                "recorder instance lock must not have hard-link aliases",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn recorder_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn recorder_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    recorder_windows_attributes_are_reparse(
        metadata.file_attributes(),
        FILE_ATTRIBUTE_REPARSE_POINT,
    )
}

#[cfg(not(any(unix, windows)))]
fn recorder_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(any(windows, test))]
fn recorder_windows_attributes_are_reparse(attributes: u32, reparse_flag: u32) -> bool {
    attributes & reparse_flag != 0
}

#[cfg(unix)]
fn recorder_lock_identity(file: &File) -> io::Result<RecorderLockIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(RecorderLockIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn recorder_lock_identity(file: &File) -> io::Result<RecorderLockIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut information = FILE_ID_INFO::default();
    // SAFETY: the live file owns the handle for this call and `information`
    // points to writable storage of the exact size supplied to the API.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RecorderLockIdentity {
        volume_serial_number: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
fn recorder_lock_identity(_file: &File) -> io::Result<RecorderLockIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "recorder locking requires stable file identity support",
    ))
}

fn recorder_lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
}

fn map_recorder_nofollow_error(error: io::Error) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_recorder_lock_data("recorder instance lock must not be a symbolic link");
    }
    error
}

fn invalid_recorder_lock_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(windows)]
fn reject_windows_recorder_lock_reparse_components(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                return Err(invalid_recorder_lock_data(format!(
                    "recorder state root must not traverse a reparse point ({})",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("service"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        sync_directory(parent);
        Ok(())
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
        "could not allocate a unique service temporary file",
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
    use std::process::{Command as ProcessCommand, Stdio};
    use std::thread;
    use std::time::{Duration as StdDuration, Instant};

    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;

    const RECORDER_LOCK_CHILD_HISTORY_ENV: &str = "CODEX_USAGE_MONIT_RECORDER_LOCK_TEST_HISTORY";
    const RECORDER_LOCK_CHILD_READY_ENV: &str = "CODEX_USAGE_MONIT_RECORDER_LOCK_TEST_READY";

    fn options(root: &Path) -> ServiceOptions {
        let mut options = ServiceOptions::new(
            root.join("bin/codex usage monit.exe"),
            root.join("Codex Home"),
            root.join("State Dir/history-v1"),
            root.join("State Dir/recorder-status.json"),
            Some(root.join("State Dir/perf log.jsonl")),
        );
        options.codex_bin = Some(root.join("Codex & $% tools/codex.cmd"));
        options.environment_path = Some(OsString::from("/opt/codex & tools/bin:/usr/bin"));
        options.remotes_config_file = Some(root.join("Config Dir/remotes.json"));
        options.project_mapping_file = Some(root.join("Config Dir/project-mappings.json"));
        options.lookback_days = 11;
        options.max_files = 777;
        options.active_grace_minutes = 9;
        options.redact_content = true;
        options.no_rollout_cache = true;
        options.service_coordination_root_override =
            Some(root.join("Current User Service Registration Scope"));
        options
    }

    fn windows_options() -> ServiceOptions {
        let mut options = ServiceOptions::new(
            PathBuf::from(r"C:\Users\A B\bin\codex usage monit.exe"),
            PathBuf::from(r"C:\Users\A B\Codex Home"),
            PathBuf::from(r"C:\Users\A B\State Dir\history-v1"),
            PathBuf::from(r"C:\Users\A B\State Dir\recorder-status.json"),
            Some(PathBuf::from(r"C:\Users\A B\State Dir\perf log.jsonl")),
        );
        options.codex_bin = Some(PathBuf::from(r"C:\Users\A B\Codex & $% tools\codex.cmd"));
        options.environment_path = Some(OsString::from("/opt/codex & tools/bin:/usr/bin"));
        options.remotes_config_file = Some(PathBuf::from(r"C:\Users\A B\Config Dir\remotes.json"));
        options.project_mapping_file = Some(PathBuf::from(
            r"C:\Users\A B\Config Dir\project-mappings.json",
        ));
        options.lookback_days = 11;
        options.max_files = 777;
        options.active_grace_minutes = 9;
        options.redact_content = true;
        options.no_rollout_cache = true;
        options
    }

    fn valid_options(root: &Path) -> ServiceOptions {
        let options = options(root);
        for executable in [
            options.executable.as_path(),
            options
                .codex_bin
                .as_deref()
                .expect("test service options include a Codex executable"),
        ] {
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(executable, b"test executable\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        options
    }

    #[test]
    fn recorder_arguments_preserve_paths_as_distinct_argv_elements() {
        let root = Path::new("/tmp/root with spaces");
        let arguments = options(root).recorder_arguments();
        assert_eq!(arguments[0], "--codex-home");
        assert_eq!(arguments[1], root.join("Codex Home").as_os_str());
        assert!(arguments.contains(&OsString::from("--codex-bin")));
        assert!(arguments.contains(&root.join("Codex & $% tools/codex.cmd").into_os_string()));
        assert!(arguments.windows(2).any(|pair| pair == ["--days", "11"]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--max-files", "777"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--active-grace-minutes", "9"])
        );
        assert!(arguments.contains(&OsString::from("--redact-content")));
        assert!(arguments.contains(&OsString::from("--no-rollout-cache")));
        assert!(arguments.contains(&OsString::from("record")));
        assert!(arguments.windows(2).any(|pair| {
            pair == [
                OsString::from("--service-cutover-protocol"),
                OsString::from(SERVICE_CUTOVER_PROTOCOL),
            ]
        }));
        assert!(arguments.contains(&root.join("State Dir/history-v1").into_os_string()));
        assert!(arguments.contains(&root.join("State Dir/perf log.jsonl").into_os_string()));
        assert!(arguments.contains(&OsString::from("--service-remotes-config")));
        assert!(arguments.contains(&root.join("Config Dir/remotes.json").into_os_string()));
        assert!(arguments.contains(&OsString::from("--service-project-mapping-file")));
        assert!(
            arguments.contains(
                &root
                    .join("Config Dir/project-mappings.json")
                    .into_os_string()
            )
        );
    }

    #[test]
    fn service_definition_identity_binds_semantic_argv_executable_and_environment() {
        let root = Path::new("/tmp/root with spaces");
        let options = options(root);
        let definition_id = options.service_definition_id();
        validate_service_definition_id(&definition_id).unwrap();
        let arguments = options.recorder_arguments();
        let identity_flag = arguments
            .iter()
            .position(|argument| argument == SERVICE_DEFINITION_ID_ARGUMENT)
            .unwrap();
        assert_eq!(arguments[identity_flag + 1], definition_id.as_str());

        let mut changed_executable = options.clone();
        changed_executable.executable = root.join("bin/another executable");
        assert_ne!(
            changed_executable.service_definition_id(),
            options.service_definition_id()
        );
        let mut changed_argument = options.clone();
        changed_argument.max_files += 1;
        assert_ne!(
            changed_argument.service_definition_id(),
            options.service_definition_id()
        );
        let mut changed_environment = options.clone();
        changed_environment.environment_path = Some(OsString::from("/another/bin"));
        assert_ne!(
            changed_environment.service_definition_id(),
            options.service_definition_id()
        );
        let mut changed_remotes = options.clone();
        changed_remotes.remotes_config_file = Some(root.join("Other Config/remotes.json"));
        assert_ne!(
            changed_remotes.service_definition_id(),
            options.service_definition_id()
        );
        let mut changed_mapping = options.clone();
        changed_mapping.project_mapping_file =
            Some(root.join("Other Config/project-mappings.json"));
        assert_ne!(
            changed_mapping.service_definition_id(),
            options.service_definition_id()
        );

        assert_eq!(
            launchd_definition_id(launchd_plist(&options).as_bytes()).unwrap(),
            options.service_definition_id()
        );
        assert_eq!(
            systemd_definition_id(systemd_unit(&options).as_bytes()).unwrap(),
            options.service_definition_id()
        );
    }

    #[test]
    fn project_mapping_path_must_be_absolute_and_service_representable() {
        let directory = tempdir().unwrap();
        let options = valid_options(directory.path());
        validate_options(&options).unwrap();

        let mut relative = options.clone();
        relative.project_mapping_file = Some(PathBuf::from("config/project-mappings.json"));
        let error = validate_options(&relative).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("project mapping path must be absolute")
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let mut unrepresentable = options;
            unrepresentable.project_mapping_file = Some(
                directory
                    .path()
                    .join(OsString::from_vec(b"project-mappings-\xff.json".to_vec())),
            );
            let error = validate_options(&unrepresentable).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("project mapping path cannot be represented")
            );
        }
    }

    #[test]
    fn loaded_service_contract_requires_immediate_unique_identity_values() {
        let definition_id = "a".repeat(64);
        let extra_sha = "b".repeat(64);
        let arguments = [
            extra_sha.as_str(),
            "--service-cutover-protocol",
            SERVICE_CUTOVER_PROTOCOL,
            SERVICE_DEFINITION_ID_ARGUMENT,
            definition_id.as_str(),
        ];
        assert_eq!(
            verify_service_contract_arguments(&arguments, Some(&definition_id)).unwrap(),
            definition_id
        );
        let separated = [
            "--service-cutover-protocol",
            "unrelated",
            SERVICE_CUTOVER_PROTOCOL,
            SERVICE_DEFINITION_ID_ARGUMENT,
            definition_id.as_str(),
        ];
        assert!(verify_service_contract_arguments(&separated, Some(&definition_id)).is_err());
        let duplicate_identity = [
            "--service-cutover-protocol",
            SERVICE_CUTOVER_PROTOCOL,
            SERVICE_DEFINITION_ID_ARGUMENT,
            definition_id.as_str(),
            definition_id.as_str(),
        ];
        assert!(
            verify_service_contract_arguments(&duplicate_identity, Some(&definition_id)).is_err()
        );

        let launchd = format!(
            "arguments = {{\n  0 = /bin/recorder\n  1 = --service-cutover-protocol\n  2 = {SERVICE_CUTOVER_PROTOCOL}\n  3 = {SERVICE_DEFINITION_ID_ARGUMENT}\n  4 = {definition_id}\n}}"
        );
        let loaded = launchd_loaded_arguments(&launchd).unwrap();
        assert_eq!(
            verify_service_contract_arguments(&loaded, Some(&definition_id)).unwrap(),
            definition_id
        );
    }

    #[test]
    fn recorder_status_namespace_follows_content_redaction_mode() {
        let root = Path::new("/tmp/root");
        let mut options = options(root);
        options.redact_content = false;
        let visible_namespace = expected_history_namespace(&options);
        options.redact_content = true;
        let redacted_namespace = expected_history_namespace(&options);

        assert_ne!(redacted_namespace, visible_namespace);
        assert_eq!(redacted_namespace, format!("{visible_namespace}-redacted"));
    }

    #[test]
    fn recorder_instance_lock_is_state_root_wide_and_released_by_drop() {
        let directory = tempdir().unwrap();
        let preview_history = directory.path().join("state/history-v1");
        // Redaction is a namespace property, not part of the history root;
        // two foreground modes therefore derive this same state-root lock.
        let redacted_history = directory.path().join("state/history-v1");

        let first = match try_acquire_recorder_instance_lock(&preview_history).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("first recorder lock was unexpectedly busy"),
        };
        assert_eq!(
            first.path(),
            directory.path().join("state/recorder-instance.lock")
        );
        assert!(matches!(
            try_acquire_recorder_instance_lock(&redacted_history).unwrap(),
            TryRecorderInstanceLock::Busy
        ));

        drop(first);
        assert!(matches!(
            try_acquire_recorder_instance_lock(&redacted_history).unwrap(),
            TryRecorderInstanceLock::Acquired(_)
        ));
    }

    #[test]
    fn concurrent_same_process_attempts_cannot_reopen_the_lock_inode() {
        let directory = tempdir().unwrap();
        let history = directory.path().join("state/history-v1");
        let holder = match try_acquire_recorder_instance_lock(&history).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("first recorder lock was unexpectedly busy"),
        };

        let attempts = (0..8)
            .map(|_| {
                let history = history.clone();
                thread::spawn(move || {
                    matches!(
                        try_acquire_recorder_instance_lock(&history).unwrap(),
                        TryRecorderInstanceLock::Busy
                    )
                })
            })
            .collect::<Vec<_>>();
        assert!(attempts.into_iter().all(|attempt| attempt.join().unwrap()));
        drop(holder);
    }

    #[test]
    fn recorder_instance_locks_for_different_state_roots_are_independent() {
        let directory = tempdir().unwrap();
        let left = directory.path().join("left/history-v1");
        let right = directory.path().join("right/history-v1");

        let left = match try_acquire_recorder_instance_lock(&left).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("left recorder lock was unexpectedly busy"),
        };
        let right = match try_acquire_recorder_instance_lock(&right).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("right recorder lock was unexpectedly busy"),
        };
        assert_ne!(left.path(), right.path());
    }

    #[test]
    fn killed_process_releases_recorder_instance_lock() {
        let directory = tempdir().unwrap();
        let history = directory.path().join("state/history-v1");
        let ready = directory.path().join("child-ready");
        let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service::tests::recorder_instance_lock_child_process_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(RECORDER_LOCK_CHILD_HISTORY_ENV, &history)
            .env(RECORDER_LOCK_CHILD_READY_ENV, &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + StdDuration::from_secs(10);
        while !ready.is_file() {
            assert!(
                Instant::now() < deadline,
                "child did not acquire the recorder instance lock in time"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before holding the recorder instance lock"
            );
            thread::sleep(StdDuration::from_millis(20));
        }
        assert!(matches!(
            try_acquire_recorder_instance_lock(&history).unwrap(),
            TryRecorderInstanceLock::Busy
        ));

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(matches!(
            try_acquire_recorder_instance_lock(&history).unwrap(),
            TryRecorderInstanceLock::Acquired(_)
        ));
    }

    #[test]
    fn recorder_instance_lock_child_process_helper() {
        let Some(history) = std::env::var_os(RECORDER_LOCK_CHILD_HISTORY_ENV).map(PathBuf::from)
        else {
            return;
        };
        let ready = PathBuf::from(std::env::var_os(RECORDER_LOCK_CHILD_READY_ENV).unwrap());
        let _guard = match try_acquire_recorder_instance_lock(&history).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("child recorder lock was unexpectedly busy"),
        };
        fs::write(ready, b"ready\n").unwrap();
        loop {
            thread::sleep(StdDuration::from_secs(1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn recorder_instance_lock_rejects_symlinks_and_non_private_modes() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let real_state = directory.path().join("real-state");
        fs::create_dir(&real_state).unwrap();
        fs::set_permissions(&real_state, fs::Permissions::from_mode(0o700)).unwrap();
        let linked_state = directory.path().join("linked-state");
        symlink(&real_state, &linked_state).unwrap();
        let error =
            try_acquire_recorder_instance_lock(&linked_state.join("history-v1")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let state = directory.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.path().join("lock-target");
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, state.join(RECORDER_INSTANCE_LOCK_FILE)).unwrap();
        let error = try_acquire_recorder_instance_lock(&state.join("history-v1")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        fs::remove_file(state.join(RECORDER_INSTANCE_LOCK_FILE)).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        let error = try_acquire_recorder_instance_lock(&state.join("history-v1")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let guard = match try_acquire_recorder_instance_lock(&state.join("history-v1")).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("recorder lock was unexpectedly busy"),
        };
        drop(guard);
        fs::set_permissions(
            state.join(RECORDER_INSTANCE_LOCK_FILE),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let error = try_acquire_recorder_instance_lock(&state.join("history-v1")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn windows_reparse_attribute_policy_fails_closed() {
        const REPARSE: u32 = 0x0000_0400;
        assert!(recorder_windows_attributes_are_reparse(REPARSE, REPARSE));
        assert!(!recorder_windows_attributes_are_reparse(0, REPARSE));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn launchd_and_systemd_render_escaped_argument_arrays() {
        let root = Path::new("/tmp/a & b");
        let options = options(root);
        let plist = launchd_plist(&options);
        assert!(plist.contains("/tmp/a &amp; b"));
        assert!(plist.contains("Codex &amp; $% tools/codex.cmd"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("/opt/codex &amp; tools/bin:/usr/bin"));
        let unit = systemd_unit(&options);
        assert!(unit.contains("ExecStart=\"/tmp/a & b/bin/codex usage monit.exe\""));
        assert!(unit.contains("Codex & $$%% tools/codex.cmd"));
        assert!(unit.contains("Environment=\"PATH=/opt/codex & tools/bin:/usr/bin\""));
        assert!(unit.contains("Restart=on-failure"));
        assert!(!unit.contains("/bin/sh"));
        assert_eq!(
            systemd_quote(OsStr::new("/tmp/$cash/%profile")),
            r#""/tmp/$$cash/%%profile""#
        );
    }

    #[test]
    fn systemd_registration_is_reloaded_but_not_enabled_before_trust() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("codex-usage-monit-recorder.service");
        fs::write(&path, b"old unit\n").unwrap();
        let mut operations = Vec::new();

        install_systemd_registration(&path, b"new unit\n", |operation| {
            operations.push(operation);
            Ok(())
        })
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new unit\n");
        assert_eq!(operations, vec![SystemdInstallOperation::DaemonReload]);
    }

    #[test]
    fn systemd_quiescence_accepts_fresh_missing_unit_only_after_state_proof() {
        let mut operations = Vec::new();
        let quiescence = quiesce_systemd_registration(false, |operation| {
            operations.push(operation);
            Ok(match operation {
                SystemdQuiesceOperation::DisableNow => SystemdQuiesceOperationResult {
                    success: false,
                    code: Some(1),
                    stdout: String::new(),
                    detail: "unit missing".to_string(),
                },
                SystemdQuiesceOperation::InspectDefinition => SystemdQuiesceOperationResult {
                    success: true,
                    code: Some(0),
                    stdout: "LoadState=not-found\nUnitFileState=".to_string(),
                    detail: String::new(),
                },
                SystemdQuiesceOperation::VerifyInactive => SystemdQuiesceOperationResult {
                    success: false,
                    code: Some(4),
                    stdout: String::new(),
                    detail: "not found".to_string(),
                },
            })
        })
        .unwrap();

        assert_eq!(quiescence, ManagedServiceQuiescence::NoRegistration);
        assert_eq!(
            operations,
            vec![
                SystemdQuiesceOperation::DisableNow,
                SystemdQuiesceOperation::InspectDefinition,
                SystemdQuiesceOperation::VerifyInactive,
            ]
        );
    }

    #[test]
    fn systemd_quiescence_fails_closed_for_enabled_or_active_units() {
        let enabled = SystemdQuiesceOperationResult {
            success: true,
            code: Some(0),
            stdout: "LoadState=loaded\nUnitFileState=enabled".to_string(),
            detail: String::new(),
        };
        assert!(verify_systemd_definition_disabled(&enabled).is_err());
        let unknown = SystemdQuiesceOperationResult {
            success: false,
            code: Some(1),
            stdout: String::new(),
            detail: "manager unavailable".to_string(),
        };
        assert!(verify_systemd_definition_disabled(&unknown).is_err());
        let active = SystemdQuiesceOperationResult {
            success: true,
            code: Some(0),
            stdout: String::new(),
            detail: String::new(),
        };
        assert!(verify_systemd_inactive(&active).is_err());
    }

    #[test]
    fn failed_systemd_replacement_removes_the_unit_before_a_future_manager_restart() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let path = directory.path().join("codex-usage-monit-recorder.service");
        fs::write(&path, b"old pre-v0.4 unit\n").unwrap();

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::StoppedManagedRegistration),
            || {
                install_systemd_registration(&path, b"new unit\n", |operation| match operation {
                    SystemdInstallOperation::DaemonReload => bail!("daemon reload failed"),
                    SystemdInstallOperation::Enable => unreachable!(),
                })
            },
            || cleanup_systemd_registration_at(&path, |_| Ok(())),
        )
        .unwrap_err();

        assert!(error.to_string().contains("daemon reload failed"));
        assert!(
            !path.exists(),
            "a future systemd reload must not find a unit"
        );
        assert!(
            ensure_no_recorder_cutover_blocker_at(
                &service_coordination_root_for_options(&service_options).unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn windows_command_line_quotes_spaces_quotes_and_trailing_slashes() {
        assert_eq!(
            quote_windows_argument(OsStr::new(r"C:\Program Files\monit.exe")),
            r#""C:\Program Files\monit.exe""#
        );
        assert_eq!(
            quote_windows_argument(OsStr::new(r#"a "quoted" value"#)),
            r#""a \"quoted\" value""#
        );
        assert_eq!(
            quote_windows_argument(OsStr::new(r"C:\path with space\")),
            r#""C:\path with space\\""#
        );
        let options = windows_options();
        let arguments = windows_recorder_arguments(&options);
        assert!(arguments.starts_with(
            r#""--service-path=/opt/codex & tools/bin:/usr/bin" --codex-home "C:\Users\A B\Codex Home""#
        ));
        assert!(arguments.contains(r#"--codex-bin "C:\Users\A B\Codex & $% tools\codex.cmd""#));
        assert!(arguments.contains("--days 11 --max-files 777 --active-grace-minutes 9"));
        assert!(arguments.contains("--redact-content --no-rollout-cache"));
        assert!(arguments.contains("record --foreground"));
        let xml = windows_task_xml(&options, "S-1-5-21-1234");
        assert!(xml.contains(r#"<Command>C:\Users\A B\bin\codex usage monit.exe</Command>"#));
        assert!(xml.contains(r#"&quot;--service-path=/opt/codex &amp; tools/bin:/usr/bin&quot;"#));
        assert!(xml.contains(r"Codex &amp; $% tools\codex.cmd"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert_eq!(
            windows_task_name("S-1-5-21-1234"),
            windows_task_name("S-1-5-21-1234")
        );
        assert_ne!(
            windows_task_name("S-1-5-21-1234"),
            windows_task_name("S-1-5-21-5678")
        );

        let mut inherited_path = options;
        inherited_path.environment_path = None;
        assert!(!windows_recorder_arguments(&inherited_path).contains("--service-path"));
    }

    #[test]
    fn windows_recorder_path_value_can_begin_with_a_hyphen() {
        let mut options = windows_options();
        options.environment_path = Some(OsString::from(
            r"-C:\Portable Node & Tools;C:\Windows\System32",
        ));

        let arguments = windows_recorder_arguments(&options);
        assert!(arguments.starts_with(
            r#""--service-path=-C:\Portable Node & Tools;C:\Windows\System32" --codex-home "C:\Users\A B\Codex Home""#
        ));
        let xml = windows_task_xml(&options, "S-1-5-21-1234");
        assert!(xml.contains(
            r#"&quot;--service-path=-C:\Portable Node &amp; Tools;C:\Windows\System32&quot;"#
        ));
    }

    #[test]
    fn failed_service_install_uses_cleanup_instead_of_definition_rollback() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let old_definition = directory.path().join("old-automatic-start-definition");
        fs::write(&old_definition, b"old definition").unwrap();

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || bail!("new registration failed"),
            || {
                fs::remove_file(&old_definition)?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("new registration failed"));
        assert!(!old_definition.exists());
    }

    #[test]
    fn failed_windows_replacement_deletes_the_task_before_a_future_logon() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let task_name = r"\CodexUsageMonitRecorder-test";
        let registered = std::cell::Cell::new(true);

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::StoppedManagedRegistration),
            || bail!("schtasks /Run failed"),
            || {
                let _ = uninstall_windows_task_registration(task_name, |operation| {
                    Ok(match operation {
                        WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                            success: true,
                            code: Some(0),
                            detail: String::new(),
                        },
                        WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                            success: registered.get(),
                            code: Some(if registered.get() {
                                0
                            } else {
                                HRESULT_FILE_NOT_FOUND
                            }),
                            detail: String::new(),
                        },
                        WindowsTaskOperation::State { .. } => WindowsTaskOperationResult {
                            success: true,
                            code: Some(0),
                            detail: WINDOWS_TASK_STATE_DISABLED.to_string(),
                        },
                        WindowsTaskOperation::Delete { .. } => {
                            registered.set(false);
                            WindowsTaskOperationResult {
                                success: true,
                                code: Some(0),
                                detail: String::new(),
                            }
                        }
                        WindowsTaskOperation::End { .. } => {
                            panic!("an idle task must not be ended")
                        }
                    })
                })?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("schtasks /Run failed"));
        assert!(
            !registered.get(),
            "a future logon must not find the old task"
        );
    }

    #[test]
    fn windows_task_query_distinguishes_missing_from_other_failures() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let mut missing = |_| {
            Ok(WindowsTaskOperationResult {
                success: false,
                code: Some(HRESULT_FILE_NOT_FOUND),
                detail: "localized task-not-found diagnostic".to_string(),
            })
        };
        assert!(!windows_task_is_installed_with(task_name, &mut missing).unwrap());

        let mut installed = |_| {
            Ok(WindowsTaskOperationResult {
                success: true,
                code: Some(0),
                detail: String::new(),
            })
        };
        assert!(windows_task_is_installed_with(task_name, &mut installed).unwrap());

        let mut denied = |_| {
            Ok(WindowsTaskOperationResult {
                success: false,
                code: Some(0x8007_0005_u32 as i32),
                detail: "localized access-denied diagnostic".to_string(),
            })
        };
        let error = windows_task_is_installed_with(task_name, &mut denied).unwrap_err();
        assert!(error.to_string().contains("schtasks /Query failed"));
        assert!(error.to_string().contains("HRESULT 0x80070005"));
        assert!(error.to_string().contains("localized access-denied"));
    }

    #[test]
    fn windows_service_status_uses_scheduler_state_separately_from_heartbeat() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let heartbeat = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        let mut ready = |operation| {
            Ok(match operation {
                WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::State { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: WINDOWS_TASK_STATE_DISABLED.to_string(),
                },
                operation => panic!("unexpected status operation: {operation:?}"),
            })
        };
        let stopped =
            windows_task_status_with(task_name, Some(heartbeat), true, &mut ready).unwrap();
        assert_eq!(stopped.state, ServiceState::Stopped);
        assert!(!stopped.running);
        assert!(stopped.heartbeat_recent);

        let mut running = |operation| {
            Ok(match operation {
                WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::State { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: WINDOWS_TASK_STATE_RUNNING.to_string(),
                },
                operation => panic!("unexpected status operation: {operation:?}"),
            })
        };
        let awaiting_heartbeat =
            windows_task_status_with(task_name, None, false, &mut running).unwrap();
        assert_eq!(awaiting_heartbeat.state, ServiceState::Running);
        assert!(awaiting_heartbeat.running);
        assert!(!awaiting_heartbeat.heartbeat_recent);
    }

    #[test]
    fn windows_new_registration_must_be_observed_disabled_before_trust() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let mut disabled = |_| {
            Ok(WindowsTaskOperationResult {
                success: true,
                code: Some(0),
                detail: WINDOWS_TASK_STATE_DISABLED.to_string(),
            })
        };
        verify_windows_task_staged_with(task_name, &mut disabled).unwrap();

        let mut ready = |_| {
            Ok(WindowsTaskOperationResult {
                success: true,
                code: Some(0),
                detail: WINDOWS_TASK_STATE_READY.to_string(),
            })
        };
        assert!(verify_windows_task_staged_with(task_name, &mut ready).is_err());
    }

    #[test]
    fn windows_task_uninstall_skips_end_when_not_running_but_still_deletes() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let mut operations = Vec::new();
        let mut deleted = false;

        let quiescence = uninstall_windows_task_registration(task_name, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                    success: !deleted,
                    code: Some(if deleted { HRESULT_FILE_NOT_FOUND } else { 0 }),
                    detail: String::new(),
                },
                WindowsTaskOperation::Delete { .. } => {
                    deleted = true;
                    WindowsTaskOperationResult {
                        success: true,
                        code: Some(0),
                        detail: String::new(),
                    }
                }
                WindowsTaskOperation::State { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: WINDOWS_TASK_STATE_DISABLED.to_string(),
                },
                WindowsTaskOperation::End { .. } => panic!("an idle task must not be ended"),
            })
        })
        .unwrap();

        assert_eq!(
            quiescence,
            ManagedServiceQuiescence::StoppedManagedRegistration
        );

        assert!(matches!(
            operations.as_slice(),
            [
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::Disable { .. },
                WindowsTaskOperation::State { .. },
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::State { .. },
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::State { .. },
                WindowsTaskOperation::Delete { .. },
                WindowsTaskOperation::Query { .. }
            ]
        ));
    }

    #[test]
    fn windows_task_uninstall_waits_for_successful_end_to_reach_disabled_before_delete() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let mut state_queries = 0;
        let mut operations = Vec::new();
        let mut deleted = false;

        uninstall_windows_task_registration(task_name, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                    success: !deleted,
                    code: Some(if deleted { HRESULT_FILE_NOT_FOUND } else { 0 }),
                    detail: String::new(),
                },
                WindowsTaskOperation::End { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Delete { .. } => {
                    deleted = true;
                    WindowsTaskOperationResult {
                        success: true,
                        code: Some(0),
                        detail: String::new(),
                    }
                }
                WindowsTaskOperation::State { .. } => {
                    state_queries += 1;
                    WindowsTaskOperationResult {
                        success: true,
                        code: Some(0),
                        detail: if state_queries <= 2 {
                            WINDOWS_TASK_STATE_RUNNING
                        } else {
                            WINDOWS_TASK_STATE_DISABLED
                        }
                        .to_string(),
                    }
                }
            })
        })
        .unwrap();

        let end = operations
            .iter()
            .position(|operation| matches!(operation, WindowsTaskOperation::End { .. }))
            .unwrap();
        let delete = operations
            .iter()
            .position(|operation| matches!(operation, WindowsTaskOperation::Delete { .. }))
            .unwrap();
        assert!(delete > end);
        assert!(
            operations[end + 1..delete]
                .iter()
                .any(|operation| matches!(operation, WindowsTaskOperation::State { .. }))
        );
        assert_eq!(state_queries, 4);
    }

    #[test]
    fn windows_task_uninstall_never_deletes_while_end_remains_running() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let mut operations = Vec::new();

        let error = uninstall_windows_task_registration(task_name, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                WindowsTaskOperation::Query { .. }
                | WindowsTaskOperation::Disable { .. }
                | WindowsTaskOperation::End { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::State { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: WINDOWS_TASK_STATE_RUNNING.to_string(),
                },
                WindowsTaskOperation::Delete { .. } => {
                    panic!("delete must not run while the task remains active")
                }
            })
        })
        .unwrap_err();

        assert!(error.to_string().contains("remains active"));
        assert!(
            !operations
                .iter()
                .any(|operation| matches!(operation, WindowsTaskOperation::Delete { .. }))
        );
    }

    #[test]
    fn windows_task_uninstall_stops_on_query_or_end_errors() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let mut query_operations = Vec::new();
        let query_error = uninstall_windows_task_registration(task_name, |operation| {
            query_operations.push(operation);
            Ok(WindowsTaskOperationResult {
                success: false,
                code: Some(0x8004_1322_u32 as i32),
                detail: "localized scheduler-unavailable diagnostic".to_string(),
            })
        })
        .unwrap_err();
        assert!(query_error.to_string().contains("schtasks /Query failed"));
        assert!(matches!(
            query_operations.as_slice(),
            [WindowsTaskOperation::Query { .. }]
        ));

        let mut disable_operations = Vec::new();
        let disable_error = uninstall_windows_task_registration(task_name, |operation| {
            disable_operations.push(operation.clone());
            Ok(match operation {
                WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                    success: false,
                    code: Some(5),
                    detail: "localized access-denied diagnostic".to_string(),
                },
                operation => panic!("operation {operation:?} must not follow failed disable"),
            })
        })
        .unwrap_err();
        assert!(disable_error.to_string().contains("disable"));
        assert!(matches!(
            disable_operations.as_slice(),
            [
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::Disable { .. }
            ]
        ));

        let mut end_operations = Vec::new();
        let end_error = uninstall_windows_task_registration(task_name, |operation| {
            end_operations.push(operation.clone());
            Ok(match operation {
                WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::State { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: WINDOWS_TASK_STATE_RUNNING.to_string(),
                },
                WindowsTaskOperation::End { .. } => WindowsTaskOperationResult {
                    success: false,
                    code: Some(5),
                    detail: "localized access-denied diagnostic".to_string(),
                },
                WindowsTaskOperation::Delete { .. } => {
                    panic!("delete must not run after an End failure")
                }
            })
        })
        .unwrap_err();
        assert!(end_error.to_string().contains("schtasks /End failed"));
        assert!(end_error.to_string().contains("exit status 5"));
        assert!(!end_error.to_string().contains("HRESULT"));
        assert!(matches!(
            end_operations.as_slice(),
            [
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::Disable { .. },
                WindowsTaskOperation::State { .. },
                WindowsTaskOperation::End { .. },
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::State { .. }
            ]
        ));
    }

    #[test]
    fn windows_task_uninstall_verifies_state_after_delete_failure() {
        let task_name = r"\CodexUsageMonitRecorder-test";
        let mut delete_attempted = false;
        let mut operations = Vec::new();

        uninstall_windows_task_registration(task_name, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                WindowsTaskOperation::Disable { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: String::new(),
                },
                WindowsTaskOperation::Query { .. } => WindowsTaskOperationResult {
                    success: !delete_attempted,
                    code: if delete_attempted {
                        Some(HRESULT_FILE_NOT_FOUND)
                    } else {
                        Some(0)
                    },
                    detail: "localized query diagnostic".to_string(),
                },
                WindowsTaskOperation::State { .. } => WindowsTaskOperationResult {
                    success: true,
                    code: Some(0),
                    detail: WINDOWS_TASK_STATE_DISABLED.to_string(),
                },
                WindowsTaskOperation::End { .. } => panic!("an idle task must not be ended"),
                WindowsTaskOperation::Delete { .. } => {
                    delete_attempted = true;
                    WindowsTaskOperationResult {
                        success: false,
                        code: Some(1),
                        detail: "localized delete diagnostic".to_string(),
                    }
                }
            })
        })
        .unwrap();

        assert!(matches!(
            operations.as_slice(),
            [
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::Disable { .. },
                WindowsTaskOperation::State { .. },
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::State { .. },
                WindowsTaskOperation::Query { .. },
                WindowsTaskOperation::State { .. },
                WindowsTaskOperation::Delete { .. },
                WindowsTaskOperation::Query { .. }
            ]
        ));
    }

    #[test]
    fn offline_recorder_preserves_offline_mode_without_requiring_codex() {
        let root = Path::new("/tmp/offline");
        let mut options = options(root);
        options.offline = true;
        options.codex_bin = None;
        let arguments = options.recorder_arguments();
        assert!(arguments.contains(&OsString::from("--offline")));
        assert!(!arguments.contains(&OsString::from("--codex-bin")));
    }

    #[test]
    fn status_file_round_trips_atomically_and_tracks_freshness() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/recorder-status.json");
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        let mut status = RecorderStatusFile::started(now, "test-history".to_string());
        status.record_success(now + Duration::minutes(1));
        write_recorder_status(&path, &status).unwrap();
        assert_eq!(read_recorder_status(&path).unwrap(), Some(status.clone()));
        assert!(status.heartbeat_is_recent(now + Duration::minutes(12)));
        assert!(!status.heartbeat_is_recent(now + Duration::minutes(14)));
        assert!(status.heartbeat_is_recent(now - Duration::minutes(2)));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        status.record_degraded(now + Duration::minutes(2), "account collection is stale");
        assert_eq!(
            status.last_history_heartbeat,
            Some(now + Duration::minutes(2))
        );
        assert_eq!(
            status.last_error.as_deref(),
            Some("account collection is stale")
        );
        status.record_heartbeat(now + Duration::minutes(3));
        assert_eq!(
            status.last_error.as_deref(),
            Some("account collection is stale")
        );
        status.record_success(now + Duration::minutes(4));
        assert!(status.last_error.is_none());

        status.record_error(now + Duration::minutes(17), "history persistence failed");
        assert!(!status.heartbeat_is_recent(now + Duration::minutes(17)));
        assert!(status.writer_may_be_active(now + Duration::minutes(17)));
        assert!(status.incompatible_writer_may_be_active(now + Duration::minutes(17)));
        assert_eq!(status.last_activity_at(), now + Duration::minutes(17));
    }

    #[test]
    fn install_keeps_recent_legacy_status_after_a_managed_registration_stops() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let status_file = service_options.status_file.clone();
        write_recorder_status(
            &status_file,
            &RecorderStatusFile::started(Utc::now(), "legacy-writer-fence".to_owned()),
        )
        .unwrap();
        let install_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::StoppedManagedRegistration),
            || {
                install_called.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("legacy recorder"));
        assert!(status_file.is_file());
        assert!(!install_called.get());
    }

    #[test]
    fn uninstall_keeps_recent_legacy_status_after_a_managed_registration_stops() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let registration = directory.path().join("managed-service-definition");
        fs::write(&registration, b"old definition").unwrap();
        write_recorder_status(
            &service_options.status_file,
            &RecorderStatusFile::started(Utc::now(), "legacy-writer-fence".to_owned()),
        )
        .unwrap();
        let uninstall_completed = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || {
                assert!(registration.is_file());
                Ok(ManagedServiceQuiescence::StoppedManagedRegistration)
            },
            || {
                uninstall_completed.set(true);
                fs::remove_file(&registration)?;
                Ok(())
            },
            || {
                fs::remove_file(&registration)?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("legacy recorder"));
        assert!(service_options.status_file.is_file());
        assert!(!registration.exists());
        assert!(!uninstall_completed.get());
    }

    #[test]
    fn failed_service_quiescence_preserves_the_legacy_writer_fence() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let status_file = service_options.status_file.clone();
        prepare_recorder_lock_state_root(status_file.parent().unwrap()).unwrap();
        fs::write(&status_file, b"legacy-writer-fence").unwrap();
        let install_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || bail!("manager still active"),
            || {
                install_called.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("manager still active"));
        assert!(status_file.is_file());
        assert!(!install_called.get());
    }

    #[test]
    fn blocker_publish_failure_does_not_touch_the_service_manager() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();
        prepare_recorder_lock_state_root(&coordination_root).unwrap();
        fs::create_dir(coordination_root.join(RECORDER_CUTOVER_BLOCKER_FILE)).unwrap();
        let quiesce_called = std::cell::Cell::new(false);
        let install_called = std::cell::Cell::new(false);
        let cleanup_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || {
                quiesce_called.set(true);
                Ok(ManagedServiceQuiescence::NoRegistration)
            },
            || {
                install_called.set(true);
                Ok(())
            },
            || {
                cleanup_called.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("cutover blocker"));
        assert!(!quiesce_called.get());
        assert!(!install_called.get());
        assert!(!cleanup_called.get());
    }

    #[test]
    fn concurrent_service_mutation_never_enters_manager_operations() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();
        let _first = match try_acquire_service_cutover_exclusive_at(&coordination_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("first service mutation lock was busy"),
        };
        let quiesce_called = std::cell::Cell::new(false);
        let install_called = std::cell::Cell::new(false);
        let cleanup_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || {
                quiesce_called.set(true);
                Ok(ManagedServiceQuiescence::NoRegistration)
            },
            || {
                install_called.set(true);
                Ok(())
            },
            || {
                cleanup_called.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("another service install/uninstall")
        );
        assert!(!quiesce_called.get());
        assert!(!install_called.get());
        assert!(!cleanup_called.get());
    }

    #[test]
    fn different_history_roots_share_one_current_user_service_mutation_gate() {
        let directory = tempdir().unwrap();
        let coordination_root = directory.path().join("current-user-service-scope");
        let mut first_options = options(&directory.path().join("history-a"));
        first_options.service_coordination_root_override = Some(coordination_root.clone());
        let mut second_options = options(&directory.path().join("history-b"));
        second_options.service_coordination_root_override = Some(coordination_root.clone());
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first_entered = entered.clone();
        let first_resume = resume.clone();

        let first = std::thread::spawn(move || {
            replace_service_after_quiescence(
                &first_options,
                || {
                    first_entered.wait();
                    first_resume.wait();
                    Ok(ManagedServiceQuiescence::NoRegistration)
                },
                || Ok(()),
                || panic!("cleanup must not run after a successful registration"),
            )
        });
        entered.wait();

        let quiesce_called = std::cell::Cell::new(false);
        let install_called = std::cell::Cell::new(false);
        let cleanup_called = std::cell::Cell::new(false);
        let second_error = replace_service_after_quiescence(
            &second_options,
            || {
                quiesce_called.set(true);
                Ok(ManagedServiceQuiescence::NoRegistration)
            },
            || {
                install_called.set(true);
                Ok(())
            },
            || {
                cleanup_called.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            second_error
                .to_string()
                .contains("current-user service registration scope")
        );
        assert!(!quiesce_called.get());
        assert!(!install_called.get());
        assert!(!cleanup_called.get());

        resume.wait();
        first.join().unwrap().unwrap();
        assert!(
            !coordination_root
                .join(RECORDER_CUTOVER_BLOCKER_FILE)
                .exists()
        );
    }

    #[test]
    fn ambiguous_cleanup_keeps_a_non_expiring_cutover_blocker() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || bail!("registration failed"),
            || bail!("automatic-start removal could not be verified"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("registration failed"));
        assert!(error.to_string().contains("automatic-start removal"));
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();
        let blocker = coordination_root.join(RECORDER_CUTOVER_BLOCKER_FILE);
        assert!(blocker.is_file());
        assert_eq!(
            serde_json::from_slice::<RecorderCutoverBlocker>(&fs::read(&blocker).unwrap())
                .unwrap()
                .schema_version,
            RECORDER_CUTOVER_BLOCKER_SCHEMA_VERSION
        );
        let cutover_error = ensure_no_recorder_cutover_blocker_at(&coordination_root).unwrap_err();
        assert_eq!(cutover_error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn service_definition_marker_is_bound_to_the_exact_registered_definition() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();
        let guard = match try_acquire_service_cutover_exclusive_at(&coordination_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test service gate was unexpectedly busy"),
        };
        let marker = CurrentServiceDefinitionMarker {
            schema_version: CURRENT_SERVICE_DEFINITION_SCHEMA_VERSION,
            platform: format!("{:?}", current_platform()).to_ascii_lowercase(),
            fingerprint: "current-definition".to_string(),
        };
        write_private_atomically(
            &coordination_root.join(CURRENT_SERVICE_DEFINITION_FILE),
            &serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();

        ensure_service_definition_is_trusted_at(
            &coordination_root,
            ServiceDefinitionObservation::Fingerprint("current-definition".to_string()),
        )
        .unwrap();
        let error = ensure_service_definition_is_trusted_at(
            &coordination_root,
            ServiceDefinitionObservation::Fingerprint("legacy-definition".to_string()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        drop(guard);
    }

    #[test]
    fn actual_definition_mismatch_is_never_trusted() {
        let observation = verify_expected_definition_fingerprint(
            ServiceDefinitionObservation::Fingerprint("legacy".to_string()),
            "current".to_string(),
        )
        .unwrap();
        assert!(matches!(
            observation,
            ServiceDefinitionObservation::Unverifiable(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn service_coordination_scope_ignores_shell_home() {
        const CHILD_OUTPUT: &str = "CODEX_USAGE_MONIT_SERVICE_SCOPE_TEST_OUTPUT";
        if let Some(output) = env::var_os(CHILD_OUTPUT) {
            fs::write(
                output,
                service_coordination_root()
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes(),
            )
            .unwrap();
            return;
        }
        let directory = tempdir().unwrap();
        let run = |name: &str, fake_home: &str| {
            let output = directory.path().join(name);
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "service::tests::service_coordination_scope_ignores_shell_home",
                    "--nocapture",
                ])
                .env("HOME", fake_home)
                .env(CHILD_OUTPUT, &output)
                .status()
                .unwrap();
            assert!(status.success());
            fs::read(output).unwrap()
        };

        assert_eq!(run("a", "/tmp/fake-home-a"), run("b", "/tmp/fake-home-b"));
    }

    #[test]
    fn windows_definition_validation_binds_command_arguments_protocol_and_principal() {
        let options = windows_options();
        let sid = "S-1-5-21-1234";
        let xml = windows_task_xml(&options, sid);
        verify_windows_task_xml_matches_options(xml.as_bytes(), &options, sid).unwrap();

        let wrong_command = xml.replacen(
            &xml_escape(&options.executable.to_string_lossy()),
            r"C:\legacy\codex-usage-monit.exe",
            1,
        );
        assert!(
            verify_windows_task_xml_matches_options(wrong_command.as_bytes(), &options, sid)
                .is_err()
        );
        let wrong_protocol = xml.replacen(SERVICE_CUTOVER_PROTOCOL, "legacy-v1", 1);
        assert!(
            verify_windows_task_xml_matches_options(wrong_protocol.as_bytes(), &options, sid)
                .is_err()
        );
        let wrong_principal = xml.replacen(sid, "S-1-5-21-9999", 1);
        assert!(
            verify_windows_task_xml_matches_options(wrong_principal.as_bytes(), &options, sid)
                .is_err()
        );
        let prematurely_enabled =
            xml.replacen("<Settings>", "<Settings>\n    <Enabled>true</Enabled>", 1);
        assert!(
            verify_windows_task_xml_matches_options(prematurely_enabled.as_bytes(), &options, sid)
                .is_err()
        );
        let extra_action = xml.replacen(
            "</Actions>",
            "  <ComHandler><ClassId>00000000-0000-0000-0000-000000000000</ClassId></ComHandler>\n  </Actions>",
            1,
        );
        assert!(
            verify_windows_task_xml_matches_options(extra_action.as_bytes(), &options, sid)
                .is_err()
        );
        let wrong_command_parent = xml.replacen("<Command>", "<Wrapper><Command>", 1).replacen(
            "</Command>",
            "</Command></Wrapper>",
            1,
        );
        assert!(
            verify_windows_task_xml_matches_options(wrong_command_parent.as_bytes(), &options, sid)
                .is_err()
        );

        let enabled = xml.replacen("<Enabled>false</Enabled>", "<Enabled>true</Enabled>", 1);
        assert_eq!(
            canonical_windows_task_fingerprint(xml.as_bytes(), sid).unwrap(),
            canonical_windows_task_fingerprint(enabled.as_bytes(), sid).unwrap()
        );
        let normalized_defaults = xml.replacen(
            "</Settings>",
            "    <UseUnifiedSchedulingEngine>false</UseUnifiedSchedulingEngine>\n  </Settings>",
            1,
        );
        assert_eq!(
            canonical_windows_task_fingerprint(xml.as_bytes(), sid).unwrap(),
            canonical_windows_task_fingerprint(normalized_defaults.as_bytes(), sid).unwrap()
        );
        let omitted_defaults = [
            "    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n",
            "    <AllowHardTerminate>true</AllowHardTerminate>\n",
            "    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n",
            "    <AllowStartOnDemand>true</AllowStartOnDemand>\n",
            "    <Hidden>false</Hidden>\n",
            "    <RunOnlyIfIdle>false</RunOnlyIfIdle>\n",
            "    <WakeToRun>false</WakeToRun>\n",
            "    <Priority>7</Priority>\n",
            "      <Enabled>true</Enabled>\n",
        ]
        .into_iter()
        .fold(enabled.clone(), |document, node| document.replace(node, ""));
        assert_eq!(
            canonical_windows_task_fingerprint(enabled.as_bytes(), sid).unwrap(),
            canonical_windows_task_fingerprint(omitted_defaults.as_bytes(), sid).unwrap()
        );
        let unknown_setting = xml.replacen(
            "</Settings>",
            "    <UnknownSetting>false</UnknownSetting>\n  </Settings>",
            1,
        );
        assert!(canonical_windows_task_fingerprint(unknown_setting.as_bytes(), sid).is_err());
        let non_default_optional = xml.replacen(
            "</Settings>",
            "    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>\n  </Settings>",
            1,
        );
        assert!(canonical_windows_task_fingerprint(non_default_optional.as_bytes(), sid).is_err());

        let mut utf16 = vec![0xff, 0xfe];
        for unit in xml.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        verify_windows_task_xml_matches_options(&utf16, &options, sid).unwrap();
    }

    #[test]
    fn unverifiable_service_definition_fails_closed() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();

        let error = ensure_service_definition_is_trusted_at(
            &coordination_root,
            ServiceDefinitionObservation::Unverifiable("manager query failed".to_string()),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn malformed_and_oversized_service_definition_markers_fail_closed() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();
        let marker_path = coordination_root.join(CURRENT_SERVICE_DEFINITION_FILE);
        let observation = || ServiceDefinitionObservation::Fingerprint("definition".to_string());

        write_private_atomically(&marker_path, b"not-json").unwrap();
        assert_eq!(
            ensure_service_definition_is_trusted_at(&coordination_root, observation())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        write_private_atomically(
            &marker_path,
            &vec![b'x'; usize::try_from(SERVICE_TRUST_MARKER_MAX_BYTES + 1).unwrap()],
        )
        .unwrap();
        assert_eq!(
            ensure_service_definition_is_trusted_at(&coordination_root, observation())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_service_definition_marker_fails_closed() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();
        let target = coordination_root.join("marker-target.json");
        write_private_atomically(&target, b"{}").unwrap();
        symlink(
            &target,
            coordination_root.join(CURRENT_SERVICE_DEFINITION_FILE),
        )
        .unwrap();

        let error = ensure_service_definition_is_trusted_at(
            &coordination_root,
            ServiceDefinitionObservation::Fingerprint("definition".to_string()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn successful_install_reports_and_retains_a_blocker_that_cannot_be_cleared() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let blocker = recorder_cutover_blocker_path(&service_options).unwrap();
        let cleanup_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || {
                fs::remove_file(&blocker)?;
                fs::create_dir(&blocker)?;
                Ok(())
            },
            || {
                cleanup_called.set(true);
                bail!("manager cleanup could not be proven")
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("could not clear cutover blocker")
        );
        assert!(cleanup_called.get());
        assert!(blocker.is_dir());
    }

    #[test]
    fn trust_failure_runs_cleanup_while_the_recorder_writer_is_still_fenced() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let cleanup_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence_with_start(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || Ok(()),
            || {
                assert!(matches!(
                    try_acquire_recorder_instance_lock(&service_options.history_dir)?,
                    TryRecorderInstanceLock::Busy
                ));
                cleanup_called.set(true);
                Ok(())
            },
            || bail!("loaded definition identity mismatch"),
            || panic!("an untrusted service must never enter the start phase"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("identity mismatch"));
        assert!(cleanup_called.get());
        assert!(
            ensure_no_recorder_cutover_blocker_at(
                &service_coordination_root_for_options(&service_options).unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn replacement_starts_the_new_recorder_only_after_the_blocker_is_cleared() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let coordination_root = service_coordination_root_for_options(&service_options).unwrap();
        let blocker = coordination_root.join(RECORDER_CUTOVER_BLOCKER_FILE);
        let started = std::cell::Cell::new(false);

        replace_service_after_quiescence_with_start(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || {
                assert!(blocker.is_file());
                Ok(())
            },
            || panic!("cleanup must not run after a successful registration"),
            || Ok(()),
            || {
                assert!(!blocker.exists());
                started.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(started.get());
    }

    #[test]
    fn failed_registration_restores_the_previous_status_fence() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let status = RecorderStatusFile::started(
            Utc::now() - Duration::minutes(13),
            "legacy-recorder".to_owned(),
        );
        write_recorder_status(&service_options.status_file, &status).unwrap();

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || bail!("new registration failed"),
            || Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("new registration failed"));
        assert_eq!(
            read_recorder_status(&service_options.status_file).unwrap(),
            Some(status)
        );
    }

    #[test]
    fn registration_keeps_the_recorder_lock_until_the_explicit_start_phase() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let mut status =
            RecorderStatusFile::started(Utc::now(), "source-aware-recorder".to_owned());
        status.bind_source_aware_v2(2).unwrap();
        write_recorder_status(&service_options.status_file, &status).unwrap();
        let replacement_guard = std::cell::RefCell::new(None);

        let error = replace_service_after_quiescence_with_start(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || {
                assert!(matches!(
                    try_acquire_recorder_instance_lock(&service_options.history_dir)?,
                    TryRecorderInstanceLock::Busy
                ));
                Ok(())
            },
            || panic!("cleanup must not run after a successful registration"),
            || Ok(()),
            || {
                let guard = match try_acquire_recorder_instance_lock(&service_options.history_dir)?
                {
                    TryRecorderInstanceLock::Acquired(guard) => guard,
                    TryRecorderInstanceLock::Busy => {
                        bail!("replacement lock was unexpectedly busy")
                    }
                };
                let _ = replacement_guard.replace(Some(guard));
                bail!("manager failed after starting replacement")
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("manager failed after starting replacement")
        );
        assert!(!service_options.status_file.exists());
        drop(replacement_guard.into_inner());
    }

    #[test]
    fn missing_registration_preserves_a_recent_foreground_recorder_fence() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let status = RecorderStatusFile::started(Utc::now(), "foreground-recorder".to_owned());
        write_recorder_status(&service_options.status_file, &status).unwrap();
        let install_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || {
                install_called.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("legacy recorder"));
        assert!(service_options.status_file.is_file());
        assert!(!install_called.get());
    }

    #[test]
    fn recent_v2_status_does_not_override_a_busy_recorder_lock() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let mut status =
            RecorderStatusFile::started(Utc::now(), "source-aware-recorder".to_owned());
        status.bind_source_aware_v2(2).unwrap();
        write_recorder_status(&service_options.status_file, &status).unwrap();
        let _foreground =
            match try_acquire_recorder_instance_lock(&service_options.history_dir).unwrap() {
                TryRecorderInstanceLock::Acquired(guard) => guard,
                TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
            };
        let install_called = std::cell::Cell::new(false);

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::StoppedManagedRegistration),
            || {
                install_called.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("foreground recorder"));
        assert!(service_options.status_file.is_file());
        assert!(!install_called.get());
    }

    #[test]
    fn install_ignores_recent_v2_residual_status_when_the_lock_is_free() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let mut status =
            RecorderStatusFile::started(Utc::now(), "source-aware-recorder".to_owned());
        status.bind_source_aware_v2(2).unwrap();
        write_recorder_status(&service_options.status_file, &status).unwrap();
        let install_called = std::cell::Cell::new(false);

        replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || {
                install_called.set(true);
                Ok(())
            },
            || panic!("cleanup must not run after a successful install"),
        )
        .unwrap();

        assert!(install_called.get());
        assert!(!service_options.status_file.exists());
    }

    #[test]
    fn uninstall_ignores_recent_v2_residual_status_when_the_lock_is_free() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let mut status =
            RecorderStatusFile::started(Utc::now(), "source-aware-recorder".to_owned());
        status.bind_source_aware_v2(2).unwrap();
        write_recorder_status(&service_options.status_file, &status).unwrap();
        let uninstall_completed = std::cell::Cell::new(false);

        replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::NoRegistration),
            || {
                uninstall_completed.set(true);
                Ok(())
            },
            || panic!("cleanup must not run after a successful uninstall"),
        )
        .unwrap();

        assert!(uninstall_completed.get());
        assert!(!service_options.status_file.exists());
    }

    #[test]
    fn status_file_v3_tracks_backend_quiescence_and_reads_v1_files() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recorder-status.json");
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        let mut status =
            RecorderStatusFile::started_with_interval(now, "test-history".to_string(), 3_600);
        status.record_success(now);
        write_recorder_status(&path, &status).unwrap();

        let loaded = read_recorder_status(&path).unwrap().unwrap();
        assert_eq!(loaded.schema_version, 3);
        assert_eq!(loaded.heartbeat_interval_seconds, Some(3_600));
        assert_eq!(
            loaded.history_backend,
            Some(RecorderHistoryBackend::LegacyV1)
        );
        assert!(loaded.incompatible_writer_may_be_active(now));
        assert!(loaded.incompatible_writer_may_be_active(now - Duration::days(1)));
        assert!(loaded.heartbeat_is_recent(now + Duration::minutes(62)));
        assert!(!loaded.heartbeat_is_recent(now + Duration::seconds(3_721)));

        let mut v2 = loaded.clone();
        assert!(v2.bind_source_aware_v2(1).is_err());
        v2.bind_source_aware_v2(2).unwrap();
        assert_eq!(v2.source_aware_v2_epoch(), Some(2));
        assert!(!v2.incompatible_writer_may_be_active(now));
        write_recorder_status(&path, &v2).unwrap();
        assert!(
            incompatible_recorder_for_cutover(&path, "test-history", now)
                .unwrap()
                .is_none()
        );
        assert!(
            incompatible_recorder_for_cutover(&path, "other-history", now)
                .unwrap()
                .is_some()
        );

        let v1 = serde_json::json!({
            "schemaVersion": 1,
            "historyNamespace": "test-history",
            "pid": 42,
            "startedAt": now,
            "lastAttemptAt": now,
            "lastHistoryHeartbeat": now,
            "lastError": null
        });
        fs::write(&path, serde_json::to_vec(&v1).unwrap()).unwrap();
        let loaded = read_recorder_status(&path).unwrap().unwrap();
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.heartbeat_interval_seconds, None);
        assert_eq!(loaded.history_backend, None);
        assert_eq!(loaded.ownership_epoch, None);
        assert!(loaded.incompatible_writer_may_be_active(now));
        assert!(loaded.heartbeat_is_recent(now + Duration::minutes(12)));
        assert!(!loaded.heartbeat_is_recent(now + Duration::seconds(721)));
        assert!(
            incompatible_recorder_for_cutover(&path, "test-history", now)
                .unwrap()
                .is_some()
        );
        assert!(
            incompatible_recorder_for_cutover(&path, "other-history", now)
                .unwrap()
                .is_some()
        );
        assert!(
            incompatible_recorder_for_cutover(&path, "test-history", now + Duration::seconds(721),)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn launchd_uninstall_falls_back_to_the_service_target_and_is_idempotent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("missing.plist");
        let domain = "gui/501";
        let target = format!("{domain}/{SERVICE_LABEL}");
        let mut operations = Vec::new();

        let quiescence = uninstall_launchd_registration(&path, domain, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                LaunchdOperation::DisableService { .. } => LaunchdOperationResult {
                    success: true,
                    detail: String::new(),
                },
                LaunchdOperation::PrintDisabled { .. } => LaunchdOperationResult {
                    success: true,
                    detail: format!("\"{SERVICE_LABEL}\" => disabled"),
                },
                LaunchdOperation::BootoutService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "service is not loaded".to_string(),
                },
                LaunchdOperation::PrintService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "Could not find service in domain".to_string(),
                },
                LaunchdOperation::BootoutRegistration { .. } => {
                    panic!("a missing registration must not be booted out by path")
                }
            })
        })
        .unwrap();

        assert_eq!(quiescence, ManagedServiceQuiescence::NoRegistration);

        assert_eq!(
            operations,
            vec![
                LaunchdOperation::DisableService {
                    target: target.clone(),
                },
                LaunchdOperation::PrintDisabled {
                    domain: domain.to_string(),
                },
                LaunchdOperation::BootoutService {
                    target: target.clone(),
                },
                LaunchdOperation::PrintService { target },
            ]
        );
    }

    #[test]
    fn launchd_disabled_state_is_machine_verified_and_unambiguous() {
        let valid = LaunchdOperationResult {
            success: true,
            detail: format!(
                "disabled services = {{\n  \"other.service\" => enabled\n  \"{SERVICE_LABEL}\" => disabled\n}}"
            ),
        };
        verify_launchd_service_disabled(&valid).unwrap();

        for detail in [
            format!("\"{SERVICE_LABEL}\" => enabled"),
            format!("\"{SERVICE_LABEL}\" => disabled\n\"{SERVICE_LABEL}\" => disabled"),
            "unrelated output".to_string(),
        ] {
            assert!(
                verify_launchd_service_disabled(&LaunchdOperationResult {
                    success: true,
                    detail,
                })
                .is_err()
            );
        }
    }

    #[test]
    fn launchd_quiescence_stops_before_bootout_when_disable_is_unproven() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recorder.plist");
        fs::write(&path, "registration").unwrap();
        let mut operations = Vec::new();
        let error = uninstall_launchd_registration(&path, "gui/501", |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                LaunchdOperation::DisableService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "permission denied".to_string(),
                },
                operation => panic!("operation {operation:?} must not follow failed disable"),
            })
        })
        .unwrap_err();
        assert!(error.to_string().contains("could not disable"));
        assert!(matches!(
            operations.as_slice(),
            [LaunchdOperation::DisableService { .. }]
        ));
        assert!(path.is_file());
    }

    #[test]
    fn failed_launchd_replacement_removes_the_plist_before_a_future_login() {
        let directory = tempdir().unwrap();
        let service_options = options(directory.path());
        let path = directory.path().join("recorder.plist");
        fs::write(&path, b"old pre-v0.4 plist").unwrap();
        let domain = "gui/501";

        let error = replace_service_after_quiescence(
            &service_options,
            || Ok(ManagedServiceQuiescence::StoppedManagedRegistration),
            || bail!("launchctl bootstrap failed"),
            || {
                let _ = uninstall_launchd_registration(&path, domain, |operation| {
                    Ok(match operation {
                        LaunchdOperation::DisableService { .. } => LaunchdOperationResult {
                            success: true,
                            detail: String::new(),
                        },
                        LaunchdOperation::PrintDisabled { .. } => LaunchdOperationResult {
                            success: true,
                            detail: format!("\"{SERVICE_LABEL}\" => disabled"),
                        },
                        LaunchdOperation::BootoutRegistration { .. } => LaunchdOperationResult {
                            success: true,
                            detail: String::new(),
                        },
                        LaunchdOperation::PrintService { .. } => LaunchdOperationResult {
                            success: false,
                            detail: "Could not find service in domain".to_string(),
                        },
                        LaunchdOperation::BootoutService { .. } => {
                            panic!("a successful registration bootout needs no fallback")
                        }
                    })
                })?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("launchctl bootstrap failed"));
        assert!(!path.exists(), "a future login must not find the old plist");
    }

    #[test]
    fn launchd_uninstall_preserves_registration_when_the_job_remains_loaded() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recorder.plist");
        fs::write(&path, "registration").unwrap();
        let domain = "gui/501";
        let mut operations = Vec::new();

        let error = uninstall_launchd_registration(&path, domain, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                LaunchdOperation::DisableService { .. } => LaunchdOperationResult {
                    success: true,
                    detail: String::new(),
                },
                LaunchdOperation::PrintDisabled { .. } => LaunchdOperationResult {
                    success: true,
                    detail: format!("\"{SERVICE_LABEL}\" => disabled"),
                },
                LaunchdOperation::PrintService { .. } => LaunchdOperationResult {
                    success: true,
                    detail: String::new(),
                },
                LaunchdOperation::BootoutRegistration { .. }
                | LaunchdOperation::BootoutService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "permission denied".to_string(),
                },
            })
        })
        .unwrap_err();

        assert!(error.to_string().contains("service remains loaded"));
        assert!(error.to_string().contains("permission denied"));
        assert!(path.exists());
        assert!(matches!(
            operations.as_slice(),
            [
                LaunchdOperation::DisableService { .. },
                LaunchdOperation::PrintDisabled { .. },
                LaunchdOperation::BootoutRegistration { .. },
                LaunchdOperation::BootoutService { .. },
                LaunchdOperation::PrintService { .. }
            ]
        ));
    }

    #[test]
    fn launchd_uninstall_preserves_registration_when_inspection_fails() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recorder.plist");
        fs::write(&path, "registration").unwrap();

        let error = uninstall_launchd_registration(&path, "gui/501", |operation| {
            Ok(match operation {
                LaunchdOperation::DisableService { .. } => LaunchdOperationResult {
                    success: true,
                    detail: String::new(),
                },
                LaunchdOperation::PrintDisabled { .. } => LaunchdOperationResult {
                    success: true,
                    detail: format!("\"{SERVICE_LABEL}\" => disabled"),
                },
                LaunchdOperation::BootoutRegistration { .. } => LaunchdOperationResult {
                    success: true,
                    detail: String::new(),
                },
                LaunchdOperation::PrintService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "permission denied".to_string(),
                },
                LaunchdOperation::BootoutService { .. } => {
                    panic!("a successful registration bootout needs no target fallback")
                }
            })
        })
        .unwrap_err();

        assert!(error.to_string().contains("could not verify"));
        assert!(error.to_string().contains("permission denied"));
        assert!(path.exists());
    }

    #[test]
    fn service_status_has_a_stable_round_trip_json_shape() {
        let heartbeat = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        let status = ServiceStatus {
            platform: "linux-systemd-user".to_string(),
            state: ServiceState::NotInstalled,
            installed: false,
            running: false,
            registration_path: Some(PathBuf::from("/tmp/recorder.service")),
            last_history_heartbeat: Some(heartbeat),
            heartbeat_recent: false,
            detail: "no registration".to_string(),
        };

        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "platform": "linux-systemd-user",
                "state": "not_installed",
                "installed": false,
                "running": false,
                "registrationPath": "/tmp/recorder.service",
                "lastHistoryHeartbeat": "2026-08-30T12:00:00Z",
                "heartbeatRecent": false,
                "detail": "no registration"
            })
        );
        assert_eq!(
            serde_json::from_value::<ServiceStatus>(serde_json::to_value(&status).unwrap())
                .unwrap(),
            status
        );
        assert_eq!(
            [
                ServiceState::NotInstalled,
                ServiceState::Installed,
                ServiceState::Running,
                ServiceState::Stopped,
                ServiceState::Unknown,
            ]
            .map(|state| serde_json::to_value(state).unwrap()),
            [
                serde_json::json!("not_installed"),
                serde_json::json!("installed"),
                serde_json::json!("running"),
                serde_json::json!("stopped"),
                serde_json::json!("unknown"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn service_status_json_lossily_encodes_non_unicode_registration_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let status = ServiceStatus {
            platform: "test".to_string(),
            state: ServiceState::Stopped,
            installed: true,
            running: false,
            registration_path: Some(PathBuf::from(OsString::from_vec(vec![
                b'/', b't', b'm', b'p', b'/', 0xff,
            ]))),
            last_history_heartbeat: None,
            heartbeat_recent: false,
            detail: "non-Unicode path".to_string(),
        };

        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["registrationPath"], "/tmp/\u{fffd}");
    }

    #[test]
    fn service_status_distinguishes_registration_and_heartbeat() {
        let now = Utc::now();
        let stopped = service_status("test", true, false, None, Some(now), false, true);
        assert_eq!(stopped.state, ServiceState::Stopped);
        assert!(stopped.detail.contains("manager is not active"));
        let running = service_status("test", true, true, None, Some(now), false, true);
        assert_eq!(running.state, ServiceState::Running);
        let missing = service_status("test", false, false, None, None, false, false);
        assert_eq!(missing.state, ServiceState::NotInstalled);
    }
}
