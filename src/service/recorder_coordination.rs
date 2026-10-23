//! Recorder lifetime, status, and source-aware cutover coordination.
//!
//! Platform service registration stays in the parent module. This module owns
//! the platform-independent contract used by recorders and v1-to-v2 history
//! migration: one process-lifetime recorder lease, a shared/exclusive service
//! cutover gate, conservative legacy status interpretation, and the durable
//! fail-closed cutover marker check.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    Platform, ServiceOptions, current_platform, stable_current_user_home,
    stable_windows_local_app_data, windows_current_user_sid, write_private_atomically,
};
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

const STATUS_SCHEMA_VERSION: u32 = 3;
const LEGACY_RECORDER_STALE_SECONDS: u64 = 12 * 60;
const RECORDER_STALE_GRACE_SECONDS: u64 = 2 * 60;
pub(super) const RECORDER_INSTANCE_LOCK_FILE: &str = "recorder-instance.lock";
const SERVICE_CUTOVER_LOCK_FILE: &str = "service-cutover.lock";
pub(super) const RECORDER_CUTOVER_BLOCKER_FILE: &str = "recorder-cutover-blocked.json";
const SERVICE_COORDINATION_DIRECTORY: &str = "service-registration-v1";
static HELD_RECORDER_LOCKS: LazyLock<Mutex<HashSet<RecorderLockIdentity>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[cfg(test)]
thread_local! {
    static RECORDER_ACQUISITION_VALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_recorder_acquisition_validation_hook() {
    let hook = RECORDER_ACQUISITION_VALIDATION_HOOK.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatusFile {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_namespace: Option<String>,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_definition_id: Option<String>,
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
            build_id: Some(env!("MONIT_BUILD_ID").to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            service_definition_id: None,
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

    pub fn incompatible_writer_may_be_active(&self, now: DateTime<Utc>) -> bool {
        self.source_aware_v2_epoch().is_none() && self.writer_may_be_active(now)
    }

    pub fn writer_may_be_active(&self, now: DateTime<Utc>) -> bool {
        self.activity_timestamp_is_recent(self.last_activity_at(), now)
    }

    pub(super) fn last_activity_at(&self) -> DateTime<Utc> {
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
        age_seconds <= stale_after_seconds
    }
}

#[must_use]
#[derive(Debug)]
pub enum TryRecorderInstanceLock {
    Acquired(RecorderInstanceLockGuard),
    Busy,
}

pub struct RecorderInstanceLockGuard {
    pub(super) file: File,
    pub(super) path: PathBuf,
    pub(super) identity: RecorderLockIdentity,
    // False only while the private acquisition candidate owns a reservation
    // but has not acquired the OS lock. Published guards always hold both.
    os_lock_acquired: bool,
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
        if self.os_lock_acquired {
            let _ = std::fs::File::unlock(&self.file);
        }
        release_held_recorder_lock(self.identity);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct RecorderLockIdentity {
    #[cfg(unix)]
    pub(super) device: u64,
    #[cfg(unix)]
    pub(super) inode: u64,
    #[cfg(windows)]
    pub(super) volume_serial_number: u64,
    #[cfg(windows)]
    pub(super) file_id: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CoordinationLockMode {
    Shared,
    Exclusive,
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

fn held_recorder_locks() -> io::Result<std::sync::MutexGuard<'static, HashSet<RecorderLockIdentity>>>
{
    HELD_RECORDER_LOCKS.lock().map_err(|_| {
        invalid_recorder_lock_data(
            "process-local recorder ownership was interrupted; restart required",
        )
    })
}

fn release_held_recorder_lock(identity: RecorderLockIdentity) {
    // Cleanup must still release OS resources, but cannot trust or repair
    // interrupted process-local ownership. Future acquisitions fail closed.
    if let Ok(mut held) = held_recorder_locks() {
        held.remove(&identity);
    }
}

pub(super) fn prepare_recorder_lock_state_root(path: &Path) -> io::Result<()> {
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
    #[cfg(windows)]
    crate::windows_private_directory::create_dir_all(path)?;
    #[cfg(not(any(unix, windows)))]
    fs::create_dir_all(path)?;

    let metadata = fs::symlink_metadata(path)?;
    validate_recorder_lock_state_root(path, &metadata)
}

pub(super) fn validate_recorder_lock_state_root(
    path: &Path,
    metadata: &fs::Metadata,
) -> io::Result<()> {
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
pub(super) fn recorder_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(super) fn recorder_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    recorder_windows_attributes_are_reparse(
        metadata.file_attributes(),
        FILE_ATTRIBUTE_REPARSE_POINT,
    )
}

#[cfg(not(any(unix, windows)))]
pub(super) fn recorder_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(any(windows, test))]
pub(super) fn recorder_windows_attributes_are_reparse(attributes: u32, reparse_flag: u32) -> bool {
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

pub(super) fn map_recorder_nofollow_error(error: io::Error) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_recorder_lock_data("recorder instance lock must not be a symbolic link");
    }
    error
}

fn invalid_recorder_lock_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
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

    // Reserve this identity without serializing unrelated roots' filesystem
    // and ACL validation behind the process-wide registry mutex.
    let reserved = held_recorder_locks()?.insert(identity);
    if !reserved {
        #[cfg(test)]
        run_recorder_acquisition_validation_hook();
        validate_opened_recorder_lock(state_root, file_name, &file, identity)?;
        // The current owner may have finished while validation ran. Busy is
        // still conservative, but poisoned ownership must be reported.
        let _held = held_recorder_locks()?;
        return Ok(TryRecorderInstanceLock::Busy);
    }

    // Keep this guard private until OS acquisition and the final registry
    // fence succeed. Its Drop releases both resources on every early return,
    // including validation failure, poison and an ordinary busy OS lock.
    let mut candidate = RecorderInstanceLockGuard {
        file,
        path,
        identity,
        os_lock_acquired: false,
    };
    let lock_result = match mode {
        CoordinationLockMode::Shared => std::fs::File::try_lock_shared(&candidate.file),
        CoordinationLockMode::Exclusive => std::fs::File::try_lock(&candidate.file),
    };
    candidate.os_lock_acquired = match lock_result {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    };
    #[cfg(test)]
    run_recorder_acquisition_validation_hook();
    validate_opened_recorder_lock(state_root, file_name, &candidate.file, identity)?;
    {
        // Another owner could poison the registry during I/O. Only hand out a
        // guard while the reservation is still present in trusted bookkeeping.
        // End this scope before candidate Drop can re-enter registry cleanup.
        let held = held_recorder_locks()?;
        if !held.contains(&identity) {
            return Err(invalid_recorder_lock_data(
                "recorder lock reservation disappeared during validation",
            ));
        }
    }
    if candidate.os_lock_acquired {
        Ok(TryRecorderInstanceLock::Acquired(candidate))
    } else {
        Ok(TryRecorderInstanceLock::Busy)
    }
}

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

pub(super) fn service_coordination_root_for_options(
    _options: &ServiceOptions,
) -> io::Result<PathBuf> {
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

pub(crate) fn ensure_no_recorder_cutover_blocker_at(coordination_root: &Path) -> io::Result<()> {
    let path = coordination_root.join(RECORDER_CUTOVER_BLOCKER_FILE);
    match std::fs::symlink_metadata(&path) {
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

pub fn read_recorder_status(path: &Path) -> io::Result<Option<RecorderStatusFile>> {
    let contents = match std::fs::read(path) {
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

#[cfg(test)]
mod poison_tests {
    use super::*;

    #[test]
    fn recorder_validation_releases_registry_and_rejects_concurrent_poison() {
        use std::process::Command;

        const CHILD: &str = "CODEX_USAGE_MONIT_RECORDER_VALIDATION_POISON_CHILD";
        let Ok(mode) = std::env::var(CHILD) else {
            // Each branch poisons process-wide state permanently. Isolate all
            // three rather than clearing poison or relying on test ordering.
            for mode in ["acquired", "local_busy", "os_busy"] {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "service::recorder_coordination::poison_tests::recorder_validation_releases_registry_and_rejects_concurrent_poison",
                        "--nocapture",
                    ])
                    .env(CHILD, mode)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{mode} child failed: {output:?}");
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            }
            return;
        };

        let directory = tempfile::tempdir().unwrap();
        let history = directory.path().join("state/history-v1");
        let TryRecorderInstanceLock::Acquired(guard) =
            try_acquire_recorder_instance_lock(&history).unwrap()
        else {
            panic!("isolated recorder must acquire its lock");
        };
        let path = guard.path().to_owned();
        let local_owner = if mode == "local_busy" {
            Some(guard)
        } else {
            drop(guard);
            None
        };
        let os_owner = if mode == "os_busy" {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            std::fs::File::try_lock(&file).unwrap();
            Some(file)
        } else {
            None
        };

        RECORDER_ACQUISITION_VALIDATION_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| {
                // This must succeed synchronously: a regression fails here
                // instead of blocking a helper thread behind the registry.
                let held = HELD_RECORDER_LOCKS
                    .try_lock()
                    .expect("filesystem validation must not hold the global registry");
                assert_eq!(held.len(), 1, "the identity remains reserved during I/O");
                drop(held);
                let poisoner = std::thread::spawn(|| {
                    let mut held = HELD_RECORDER_LOCKS.lock().unwrap();
                    held.clear();
                    panic!("another owner interrupted the registry during validation");
                });
                assert!(poisoner.join().is_err());
            }));
        });

        assert_eq!(
            try_acquire_recorder_instance_lock(&history)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData,
            "the final fence must reject poisoned ownership in {mode}",
        );
        assert!(HELD_RECORDER_LOCKS.is_poisoned());
        if mode != "acquired" {
            let probe = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            assert!(
                matches!(
                    std::fs::File::try_lock(&probe),
                    Err(std::fs::TryLockError::WouldBlock)
                ),
                "rejecting a busy acquisition must not unlock its existing owner",
            );
        }
        drop(local_owner);
        drop(os_owner);
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        std::fs::File::try_lock(&probe)
            .expect("failed acquisition and guard cleanup must release every OS lock");
        std::fs::File::unlock(&probe).unwrap();
    }

    #[test]
    fn poisoned_recorder_ownership_rejects_acquisition_and_releases_os_lock() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::process::Command;

        const CHILD: &str = "CODEX_USAGE_MONIT_POISONED_RECORDER_OWNER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "service::recorder_coordination::poison_tests::poisoned_recorder_ownership_rejects_acquisition_and_releases_os_lock",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "child failed: {output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let history = directory.path().join("state/history-v1");
        let TryRecorderInstanceLock::Acquired(guard) =
            try_acquire_recorder_instance_lock(&history).unwrap()
        else {
            panic!("isolated recorder must acquire its lock");
        };
        let path = guard.path().to_owned();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let mut held = HELD_RECORDER_LOCKS.lock().unwrap();
                held.clear();
                panic!("interrupted recorder ownership update");
            }))
            .is_err()
        );
        assert!(catch_unwind(AssertUnwindSafe(|| drop(guard))).is_ok());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        std::fs::File::try_lock(&file).expect("dropping the guard must still release its OS lock");
        std::fs::File::unlock(&file).unwrap();
        assert_eq!(
            try_acquire_recorder_instance_lock(&history)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData,
        );
        assert!(HELD_RECORDER_LOCKS.is_poisoned());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::ptr;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    fn public_parent(path: &Path, trustee: &str) {
        struct Descriptor(PSECURITY_DESCRIPTOR);
        impl Drop for Descriptor {
            fn drop(&mut self) {
                // SAFETY: the SDDL conversion returns a LocalAlloc allocation.
                unsafe { LocalFree(self.0) };
            }
        }

        let user = windows_current_user_sid().unwrap();
        let sddl = format!(
            "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GR;;;{trustee})"
        )
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
        let mut descriptor = ptr::null_mut();
        // SAFETY: SDDL is terminated and descriptor is a writable output.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            },
            0
        );
        let descriptor = Descriptor(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let path = crate::atomic_file::windows_wide_path(path).unwrap();
        // SAFETY: the path and descriptor remain alive throughout creation.
        assert_ne!(unsafe { CreateDirectoryW(path.as_ptr(), &attributes) }, 0);
    }

    #[test]
    fn windows_recorder_and_cutover_locks_initialize_beneath_public_parent() {
        for trustee in ["WD", "BU"] {
            let temporary = tempfile::tempdir().unwrap();
            let parent = temporary.path().join("public");
            public_parent(&parent, trustee);
            let state = parent.join("missing/recorder");
            let recorder = match try_acquire_recorder_instance_lock(&state.join("history-v1"))
                .expect("first recorder must create a private state root")
            {
                TryRecorderInstanceLock::Acquired(guard) => guard,
                TryRecorderInstanceLock::Busy => panic!("isolated recorder must acquire its lock"),
            };
            validate_windows_private_directory(&state, "recorder state root").unwrap();
            validate_windows_private_file(recorder.path(), &recorder.file, "recorder lock")
                .unwrap();
            assert!(matches!(
                try_acquire_recorder_instance_lock(&state.join("history-v1")).unwrap(),
                TryRecorderInstanceLock::Busy
            ));
            drop(recorder);

            let coordination = parent.join("missing/service-registration");
            for mode in [
                CoordinationLockMode::Shared,
                CoordinationLockMode::Exclusive,
            ] {
                let gate = match try_acquire_named_private_root_lock(
                    &coordination,
                    SERVICE_CUTOVER_LOCK_FILE,
                    mode,
                )
                .expect("first service gate must create a private coordination root")
                {
                    TryRecorderInstanceLock::Acquired(guard) => guard,
                    TryRecorderInstanceLock::Busy => panic!("isolated gate must acquire its lock"),
                };
                validate_windows_private_directory(&coordination, "service coordination root")
                    .unwrap();
                validate_windows_private_file(gate.path(), &gate.file, "service gate").unwrap();
                drop(gate);
            }
            assert_eq!(
                validate_windows_private_directory(&parent, "public parent")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied,
                "creating private children must not change the public parent's ACL"
            );
        }
    }

    #[test]
    fn windows_recorder_lock_rejects_existing_public_state_without_repairing_it() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("public-state");
        public_parent(&state, "WD");
        fs::write(state.join("sentinel"), b"keep existing data").unwrap();
        assert_eq!(
            try_acquire_recorder_instance_lock(&state.join("history-v1"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::read(state.join("sentinel")).unwrap(),
            b"keep existing data"
        );
        assert!(!state.join(RECORDER_INSTANCE_LOCK_FILE).exists());
        assert!(validate_windows_private_directory(&state, "existing state").is_err());
    }
}
