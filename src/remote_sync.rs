//! Bounded center-side orchestration for one explicitly selected remote host.
//!
//! This module deliberately has no scheduler or CLI entry point.  A caller
//! must first select one exact, paired [`RemoteHostConfig`] from a loaded
//! allowlist and pass the resulting [`RemoteSyncHostSnapshot`] here.  Local
//! recovery/commit phases are short and release both the history writer lease
//! and the ingest lock before any SSH exchange starts.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

use crate::history_ownership::{
    HistoryOwnershipState, HistoryOwnershipStore, OwnershipManifestStatus, TryWriterLease,
};
use crate::project_mapping::{
    PreparedProjectMappingBatch, ProjectMappingStore, ProjectObservation,
    PublishedProjectMappingBatch,
};
use crate::remote_agent::{current_accepted_revisions, current_revisions};
use crate::remote_ingest_state::{
    RemoteDeltaIngestBinding, RemoteDeltaIngestStateStore, RemoteDeltaNextRequestPosition,
    RemoteDeltaRangePolicy, activate_remote_delta_bootstrap, apply_and_commit_remote_delta_page,
    retry_deferred_remote_generation_cleanup, sweep_unreferenced_remote_history_generations,
};
use crate::remote_protocol::{
    DeltaPayload, DeltaRequest, EmptyRemotePayload, ExportRange, MAX_REMOTE_FRAME_ENCODED_BYTES,
    MIN_REMOTE_RESPONSE_ENCODED_BYTES, REMOTE_PROTOCOL_VERSION, RemoteDeltaResponse,
    RemoteExportRequest, RemoteExportRequestBody, RemoteExportResponseBody, RemoteFailure,
    RemoteFailureKind, RemoteProtocolError,
};
use crate::remote_transport::{
    RemoteExchangeReport, RemoteTransportError, SshCommandEnvironment,
    exchange_remote_with_agent_executable_and_environment, exchange_remote_with_environment,
};
use crate::remotes_config::{RemoteHostConfig, RemotesConfig, RemotesConfigStore, TryCurrentHost};
use crate::source_history::{HistoryProfileId, RedactionProfile, SourceHistoryStore};
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

const REMOTE_SYNC_WINDOW_MINUTES: u32 = 31 * 24 * 60;
const REMOTE_SYNC_OVERLAP_MINUTES: u16 = 60;
const REMOTE_SYNC_INCLUDE_LIVE: bool = true;
const REMOTE_FRAME_HEADER_BYTES: usize = 20;
const DEFAULT_MAX_PAGES_PER_RUN: usize = 4;
const DEFAULT_MAX_RESPONSE_BYTES_PER_RUN: usize =
    DEFAULT_MAX_PAGES_PER_RUN * (MAX_REMOTE_FRAME_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES);
const MAX_CONFIGURED_PAGES_PER_RUN: usize = 32;
const MAX_CONFIGURED_RESPONSE_BYTES_PER_RUN: usize = 128 * 1024 * 1024;
const MAX_CONFIGURED_EXCHANGE_TIMEOUT: StdDuration = StdDuration::from_secs(120);
const MAX_REMOTE_SYNC_RUN_TIME: StdDuration = StdDuration::from_secs(300);
const REMOTE_HOST_SYNC_LOCK_DIRECTORY: &str = "remote-host-sync-locks-v1";
const REMOTE_HOST_SYNC_LOCK_DOMAIN: &[u8] = b"codex-usage-monit/remote-host-sync-lock/v1\0";
const MAX_REMOTE_HOST_ID_BYTES: usize = 64;

/// Smallest aggregate response budget that can admit one framed page.
///
/// Bandwidth admission uses this before opening SSH so a near-exhausted
/// rolling budget cannot create a reservation that is too small for the
/// protocol to use.
pub(crate) const MIN_REMOTE_SYNC_RESPONSE_BYTES: usize =
    MIN_REMOTE_RESPONSE_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES;

/// Exact allowlist entry and revision selected by the caller.
///
/// Pairing is required, but the host does not have to be enabled for automatic
/// scheduling: this type is also suitable for a future explicit one-host CLI
/// sync.  Constructing it never connects to SSH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSyncHostSnapshot {
    config_revision: u64,
    host: RemoteHostConfig,
}

impl RemoteSyncHostSnapshot {
    /// Captures one explicitly requested host for a manual sync. Per-host and
    /// global automatic opt-ins are deliberately irrelevant to this path.
    pub fn capture_manual(
        config: &RemotesConfig,
        explicitly_selected_host: &RemoteHostConfig,
    ) -> Result<Self, RemoteSyncError> {
        Self::capture_inner(config, explicitly_selected_host)
    }

    /// Captures a host selected by the automatic scheduler. Both opt-ins must
    /// be true at capture time; the exact config revision is fenced again
    /// around every later local mutation.
    pub fn capture_for_automatic(
        config: &RemotesConfig,
        selected_host: &RemoteHostConfig,
    ) -> Result<Self, RemoteSyncError> {
        if !config.auto_sync_enabled() || !selected_host.sync_enabled() {
            return Err(RemoteSyncError::HostNotEnabledForAutomaticSync {
                host_id: selected_host.id().to_owned(),
            });
        }
        Self::capture_inner(config, selected_host)
    }

    fn capture_inner(
        config: &RemotesConfig,
        explicitly_selected_host: &RemoteHostConfig,
    ) -> Result<Self, RemoteSyncError> {
        config.validate().map_err(RemoteSyncError::Local)?;
        if config.host(explicitly_selected_host.id()) != Some(explicitly_selected_host) {
            return Err(RemoteSyncError::StaleHostSelection {
                host_id: explicitly_selected_host.id().to_owned(),
            });
        }
        if !explicitly_selected_host.is_paired() {
            return Err(RemoteSyncError::HostNotPaired {
                host_id: explicitly_selected_host.id().to_owned(),
            });
        }
        Ok(Self {
            config_revision: config.config_revision(),
            host: explicitly_selected_host.clone(),
        })
    }

    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn host(&self) -> &RemoteHostConfig {
        &self.host
    }
}

/// Cross-process, per-configured-host guard held for one complete aggregate +
/// fact attempt. It is deliberately separate from the short history/config
/// locks, which must never remain held across SSH.
pub struct RemoteHostSyncLease {
    file: File,
    path: PathBuf,
}

impl fmt::Debug for RemoteHostSyncLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteHostSyncLease")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for RemoteHostSyncLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[derive(Debug)]
pub enum TryRemoteHostSyncLease {
    Acquired(RemoteHostSyncLease),
    Busy,
}

/// Attempts one nonblocking host lease. The state root must be the canonical,
/// private root already validated by [`crate::history_runtime::HistoryRuntime`].
pub fn try_acquire_remote_host_sync_lease(
    state_root: &Path,
    host_id: &str,
) -> io::Result<TryRemoteHostSyncLease> {
    validate_remote_host_sync_state_root(state_root)?;
    if host_id.is_empty()
        || host_id.len() > MAX_REMOTE_HOST_ID_BYTES
        || !host_id.is_ascii()
        || host_id.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote host sync lease received an invalid host ID",
        ));
    }
    let directory = state_root.join(REMOTE_HOST_SYNC_LOCK_DIRECTORY);
    create_remote_host_sync_lock_directory(&directory)?;
    validate_remote_host_sync_directory(&directory)?;
    let path = directory.join(remote_host_sync_lock_name(host_id));
    let file = open_remote_host_sync_lock(&path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            validate_opened_remote_host_sync_lock(&path, &file)?;
            Ok(TryRemoteHostSyncLease::Acquired(RemoteHostSyncLease {
                file,
                path,
            }))
        }
        Err(error) if remote_host_sync_lock_is_contended(&error) => {
            validate_opened_remote_host_sync_lock(&path, &file)?;
            Ok(TryRemoteHostSyncLease::Busy)
        }
        Err(error) => Err(error),
    }
}

fn remote_host_sync_lock_name(host_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(REMOTE_HOST_SYNC_LOCK_DOMAIN);
    hasher.update((host_id.len() as u64).to_le_bytes());
    hasher.update(host_id.as_bytes());
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity("host-sha256-.lock".len() + digest.len() * 2);
    encoded.push_str("host-sha256-");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded.push_str(".lock");
    encoded
}

fn create_remote_host_sync_lock_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_remote_host_sync_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options.open(path)?;
    validate_opened_remote_host_sync_lock(path, &file)?;
    Ok(file)
}

fn validate_remote_host_sync_state_root(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || fs::canonicalize(path)? != path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote host sync state root must be an absolute canonical directory",
        ));
    }
    validate_remote_host_sync_directory(path)
}

fn validate_remote_host_sync_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if remote_host_sync_metadata_is_link(&metadata) || !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote host sync directory must be a real directory",
        ));
    }
    validate_remote_host_sync_private_metadata(&metadata, "remote host sync directory")?;
    #[cfg(windows)]
    validate_windows_private_directory(path, "remote host sync directory")?;
    Ok(())
}

fn validate_opened_remote_host_sync_lock(path: &Path, file: &File) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    if remote_host_sync_metadata_is_link(&path_metadata)
        || !path_metadata.file_type().is_file()
        || !file_metadata.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote host sync lock must be a real regular file",
        ));
    }
    validate_remote_host_sync_private_metadata(&path_metadata, "remote host sync lock")?;
    validate_remote_host_sync_private_metadata(&file_metadata, "remote host sync lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote host sync lock path changed while opening",
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_private_file(path, file, "remote host sync lock")?;
    Ok(())
}

#[cfg(unix)]
fn validate_remote_host_sync_private_metadata(
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: geteuid has no preconditions and retains no pointers.
    let expected_uid = unsafe { libc::geteuid() };
    if metadata.uid() != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must be owned by the current user"),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    let expected = if metadata.file_type().is_dir() {
        0o700
    } else {
        0o600
    };
    if mode != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must have mode {expected:04o} (found {mode:04o})"),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_remote_host_sync_private_metadata(
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_remote_host_sync_private_metadata(
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn remote_host_sync_metadata_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn remote_host_sync_metadata_is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn remote_host_sync_metadata_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn remote_host_sync_lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
}

/// Per-invocation network bounds.  Durable ingest state makes a continuation
/// safe to resume in a later invocation without extending these bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteSyncLimits {
    pub max_pages: NonZeroUsize,
    pub max_response_bytes: usize,
    pub exchange_timeout: StdDuration,
}

impl Default for RemoteSyncLimits {
    fn default() -> Self {
        Self {
            max_pages: NonZeroUsize::new(DEFAULT_MAX_PAGES_PER_RUN)
                .expect("the default page limit is non-zero"),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES_PER_RUN,
            exchange_timeout: StdDuration::from_secs(60),
        }
    }
}

impl RemoteSyncLimits {
    fn validate(self) -> Result<Self, RemoteSyncError> {
        if self.max_pages.get() > MAX_CONFIGURED_PAGES_PER_RUN {
            return Err(RemoteSyncError::InvalidLimits(
                "remote sync page limit exceeds 32",
            ));
        }
        if self.max_response_bytes < MIN_REMOTE_SYNC_RESPONSE_BYTES
            || self.max_response_bytes > MAX_CONFIGURED_RESPONSE_BYTES_PER_RUN
        {
            return Err(RemoteSyncError::InvalidLimits(
                "remote sync response-byte limit is outside the supported range",
            ));
        }
        if self.exchange_timeout.is_zero()
            || self.exchange_timeout > MAX_CONFIGURED_EXCHANGE_TIMEOUT
        {
            return Err(RemoteSyncError::InvalidLimits(
                "remote sync exchange timeout must be between 1ns and 120s",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSyncCompletion {
    /// The exporter returned a final page and the cursor is committed.
    Complete,
    /// More pages remain, but this invocation reached its page bound.
    Continuation(RemoteDeltaNextRequestPosition),
    /// A fully validated cursor-expiry failure started a fresh bootstrap.  A
    /// later invocation will issue the cursorless bootstrap request.
    BootstrapRestarted(RemoteDeltaNextRequestPosition),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSyncReport {
    pub pages_committed: usize,
    /// Typed bucket/session journal changes contained in committed pages.
    /// A terminal delta page can advance/confirm its cursor with zero changes;
    /// schedulers must treat that case as idle rather than using page count as
    /// an activity signal.
    pub changes_committed: usize,
    /// At least one committed page replaced the center's known live snapshot
    /// with a full, different semantic revision. A revision-only confirmation
    /// is deliberately not activity.
    pub live_state_changed: bool,
    pub response_bytes: usize,
    pub completion: RemoteSyncCompletion,
}

impl RemoteSyncReport {
    pub fn has_activity(&self) -> bool {
        self.changes_committed > 0 || self.live_state_changed
    }
}

/// Network boundary injected for deterministic tests.  The production
/// implementation below delegates to the one-shot OpenSSH transport.
pub trait RemoteDeltaTransport {
    fn exchange(
        &mut self,
        ssh_host: &str,
        request: &RemoteExportRequest,
        timeout: StdDuration,
    ) -> Result<RemoteExchangeReport<DeltaPayload, EmptyRemotePayload>, RemoteTransportError>;

    fn exchange_host(
        &mut self,
        host: &RemoteHostConfig,
        request: &RemoteExportRequest,
        timeout: StdDuration,
    ) -> Result<RemoteExchangeReport<DeltaPayload, EmptyRemotePayload>, RemoteTransportError> {
        self.exchange(host.ssh_host(), request, timeout)
    }
}

#[derive(Default)]
pub struct SshRemoteDeltaTransport {
    environment: SshCommandEnvironment,
}

impl SshRemoteDeltaTransport {
    pub fn new(environment: SshCommandEnvironment) -> Self {
        Self { environment }
    }

    pub(crate) fn environment(&self) -> &SshCommandEnvironment {
        &self.environment
    }
}

impl RemoteDeltaTransport for SshRemoteDeltaTransport {
    fn exchange(
        &mut self,
        ssh_host: &str,
        request: &RemoteExportRequest,
        timeout: StdDuration,
    ) -> Result<RemoteExchangeReport<DeltaPayload, EmptyRemotePayload>, RemoteTransportError> {
        exchange_remote_with_environment(ssh_host, request, timeout, &self.environment)
    }

    fn exchange_host(
        &mut self,
        host: &RemoteHostConfig,
        request: &RemoteExportRequest,
        timeout: StdDuration,
    ) -> Result<RemoteExchangeReport<DeltaPayload, EmptyRemotePayload>, RemoteTransportError> {
        exchange_remote_with_agent_executable_and_environment(
            host.ssh_host(),
            host.agent_executable(),
            request,
            timeout,
            &self.environment,
        )
    }
}

/// Short local phases used by the orchestrator.  Implementations must acquire
/// the history writer before the ingest lock and release both before returning.
pub trait RemoteDeltaLocalPhases {
    type PreparedPage;

    /// Replays a pending WAL page, finishes a pending activation, and starts an
    /// initial bootstrap when no active/staging generation exists.
    fn recover_and_position(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        observed_at: DateTime<Utc>,
    ) -> io::Result<RemoteDeltaNextRequestPosition>;

    /// Starts a fresh bootstrap after a validated cursor expiry, but only if
    /// `expired_request` still matches the locked durable ingest position.
    fn restart_bootstrap(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        expired_request: &RemoteExportRequest,
        observed_at: DateTime<Utc>,
    ) -> io::Result<RemoteDeltaNextRequestPosition>;

    /// Performs every potentially expensive page-local preparation without a
    /// remotes config lock. The returned value must not make the page visible;
    /// it is consumed only by the exact-host fenced commit below.
    fn prepare_page(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        request: &RemoteExportRequest,
        response: &RemoteDeltaResponse,
        received_at: DateTime<Utc>,
    ) -> io::Result<Self::PreparedPage>;

    /// Performs only the short, nonblocking publication CAS while the exact
    /// host fence is held. Any post-rename durability work is deliberately
    /// deferred until after the remotes lock is released.
    fn publish_prepared_page(&mut self, _prepared: &mut Self::PreparedPage) -> io::Result<()> {
        Ok(())
    }

    /// Makes a successful page-local publication durable before history can
    /// advance. A second exact-host fence is acquired for the history commit,
    /// so a config change during this unfenced fsync leaves at most an unused
    /// mapping observation.
    fn finish_page_publication(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// WAL-persist, apply, and cursor-commit exactly one validated page.
    fn commit_page(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        request: &RemoteExportRequest,
        response: &RemoteDeltaResponse,
        prepared: Self::PreparedPage,
        observed_at: DateTime<Utc>,
    ) -> io::Result<RemoteDeltaNextRequestPosition>;
}

pub struct FilesystemRemoteDeltaPreparedPage {
    project_mapping: Option<PreparedProjectMappingBatch>,
    received_at: DateTime<Utc>,
}

/// Filesystem-backed local phases used by manual and automatic synchronization.
/// It refuses to write until ownership is V2Active and the source has already
/// been registered as an SSH [`SourceMetadata`](crate::source_history::SourceMetadata)
/// by the runtime integration.
pub struct FilesystemRemoteDeltaLocalPhases<'a> {
    ownership_store: &'a HistoryOwnershipStore,
    history_store: &'a SourceHistoryStore,
    project_mapping_store: ProjectMappingStore,
    published_project_mapping: Option<PublishedProjectMappingBatch>,
}

impl<'a> FilesystemRemoteDeltaLocalPhases<'a> {
    pub fn new(
        ownership_store: &'a HistoryOwnershipStore,
        history_store: &'a SourceHistoryStore,
    ) -> Self {
        #[cfg(test)]
        let project_mapping_store = ProjectMappingStore::new(
            history_store
                .state_root()
                .join("test-config/project-mappings.json"),
        );
        #[cfg(not(test))]
        let project_mapping_store = ProjectMappingStore::discover();
        Self::new_with_project_mapping_store(ownership_store, history_store, project_mapping_store)
    }

    /// Explicit mapping-store injection for isolated runtimes and tests.
    /// Production construction uses the discovered user configuration store.
    pub fn new_with_project_mapping_store(
        ownership_store: &'a HistoryOwnershipStore,
        history_store: &'a SourceHistoryStore,
        project_mapping_store: ProjectMappingStore,
    ) -> Self {
        Self {
            ownership_store,
            history_store,
            project_mapping_store,
            published_project_mapping: None,
        }
    }

    fn prepare_project_descriptors(
        &self,
        binding: &RemoteDeltaIngestBinding,
        request: &RemoteExportRequest,
        response: &RemoteDeltaResponse,
    ) -> io::Result<Option<PreparedProjectMappingBatch>> {
        response.validate_for_request(request).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("remote delta exchange is invalid: {error}"),
            )
        })?;
        if request.expected_source.as_ref() != Some(binding.source())
            || response.source != *binding.source()
            || response.redaction_profile != binding.redaction_profile()
            || response.revisions != *binding.revisions()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote project descriptors do not match the exact ingest binding",
            ));
        }
        let RemoteExportResponseBody::Delta { payload, .. } = &response.result else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote project descriptors require a delta response",
            ));
        };
        validate_center_descriptor_references(payload)?;
        if payload.project_descriptors.is_empty() {
            return Ok(None);
        }
        let observations = payload
            .project_descriptors
            .iter()
            .map(|descriptor| {
                ProjectObservation::from_remote_descriptor(
                    binding.source().node_id.clone(),
                    descriptor,
                )
            })
            .collect::<io::Result<Vec<_>>>()?;

        self.project_mapping_store
            .prepare_resolve_or_create_batch(observations)
            .map(Some)
    }

    fn with_session<T>(
        &self,
        binding: &RemoteDeltaIngestBinding,
        operation: impl FnOnce(
            &mut crate::remote_ingest_state::RemoteDeltaIngestSession<'_>,
            &crate::source_history::SourceHistoryWriter<'_, '_, '_>,
        ) -> io::Result<T>,
    ) -> io::Result<T> {
        if self.history_store.profile_id() != binding.profile_id()
            || self.ownership_store.profile_id() != binding.profile_id()
            || self.ownership_store.redaction_profile() != binding.redaction_profile()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote sync local stores do not match the ingest binding",
            ));
        }

        // Global writer first; source ingest second.  All guards are scoped to
        // this function and therefore cannot escape into the transport phase.
        let lease = match self.ownership_store.try_acquire_writer_lease()? {
            TryWriterLease::Acquired(lease) => lease,
            TryWriterLease::Busy(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "history writer is busy; resume remote sync later",
                ));
            }
        };
        let manifest = match self.ownership_store.load_manifest()? {
            OwnershipManifestStatus::Initialized(manifest)
                if manifest.state() == HistoryOwnershipState::V2Active =>
            {
                manifest
            }
            OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "remote sync requires active v2 history ownership",
                ));
            }
        };
        let authority = self.ownership_store.authorize_v2_write(&lease, &manifest)?;
        let writer = self.history_store.writer(&authority)?;
        let ingest_store =
            RemoteDeltaIngestStateStore::new(self.history_store.clone(), binding.clone())?;
        let mut session = ingest_store.try_begin()?;
        operation(&mut session, &writer)
    }

    fn recover_locked(
        session: &mut crate::remote_ingest_state::RemoteDeltaIngestSession<'_>,
        writer: &crate::source_history::SourceHistoryWriter<'_, '_, '_>,
        observed_at: DateTime<Utc>,
    ) -> io::Result<()> {
        if let Some(page) = session.pending_page()? {
            apply_and_commit_remote_delta_page(session, writer, &page, observed_at)?;
        }
        let _ = activate_remote_delta_bootstrap(session, writer, observed_at)?;
        let _ = retry_deferred_remote_generation_cleanup(session, writer)?;
        let _ = sweep_unreferenced_remote_history_generations(session, writer)?;
        Ok(())
    }
}

/// Defense-in-depth for the center-side trust boundary. Protocol validation
/// applies the same rule, but mapping persistence must independently reject an
/// implementation regression or an alternate in-process transport that hands
/// it a payload with capacity-consuming, unreferenced descriptors.
fn validate_center_descriptor_references(payload: &DeltaPayload) -> io::Result<()> {
    let described = payload
        .project_descriptors
        .iter()
        .map(|descriptor| descriptor.observed_project_key.as_str())
        .collect::<BTreeSet<_>>();
    let referenced = payload
        .bucket_changes
        .iter()
        .filter_map(|change| match &change.mutation {
            crate::remote_protocol::RemoteUsageBucketMutation::Upsert(bucket) => {
                Some(bucket.as_ref())
            }
            crate::remote_protocol::RemoteUsageBucketMutation::Tombstone => None,
        })
        .flat_map(|bucket| {
            bucket
                .project_groups
                .iter()
                .filter_map(|group| group.observed_project_key.as_ref())
        })
        .chain(
            payload
                .session_digest_changes
                .iter()
                .filter_map(|change| match &change.mutation {
                    crate::remote_protocol::RemoteSessionDigestMutation::Upsert(digest) => {
                        Some(digest.as_ref())
                    }
                    crate::remote_protocol::RemoteSessionDigestMutation::Tombstone => None,
                })
                .flat_map(|digest| digest.observed_project_keys.iter()),
        )
        .chain(
            payload
                .live
                .iter()
                .filter_map(|live| live.snapshot.as_ref())
                .flat_map(|snapshot| snapshot.tasks.iter())
                .filter_map(|task| task.observed_project_key.as_ref()),
        )
        .map(|key| key.as_str())
        .collect::<BTreeSet<_>>();
    if described != referenced {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote project descriptors do not exactly match page-local project references",
        ));
    }
    Ok(())
}

impl RemoteDeltaLocalPhases for FilesystemRemoteDeltaLocalPhases<'_> {
    type PreparedPage = FilesystemRemoteDeltaPreparedPage;

    fn recover_and_position(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        observed_at: DateTime<Utc>,
    ) -> io::Result<RemoteDeltaNextRequestPosition> {
        self.with_session(binding, |session, writer| {
            Self::recover_locked(session, writer, observed_at)?;
            let status = session.status();
            if status.active_generation.is_none() && status.bootstrap_generation.is_none() {
                session.start_bootstrap(writer)?;
            }
            session.next_request_position()
        })
    }

    fn restart_bootstrap(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        expired_request: &RemoteExportRequest,
        observed_at: DateTime<Utc>,
    ) -> io::Result<RemoteDeltaNextRequestPosition> {
        self.with_session(binding, |session, writer| {
            Self::recover_locked(session, writer, observed_at)?;
            ensure_request_position(session.next_request_position()?, expired_request)?;
            session.restart_bootstrap_after_cursor_expiry(writer)?;
            session.next_request_position()
        })
    }

    fn prepare_page(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        request: &RemoteExportRequest,
        response: &RemoteDeltaResponse,
        received_at: DateTime<Utc>,
    ) -> io::Result<Self::PreparedPage> {
        if self.published_project_mapping.is_some() {
            return Err(io::Error::other(
                "a prior remote mapping publication was not finalized",
            ));
        }
        Ok(FilesystemRemoteDeltaPreparedPage {
            project_mapping: self.prepare_project_descriptors(binding, request, response)?,
            received_at,
        })
    }

    fn commit_page(
        &mut self,
        binding: &RemoteDeltaIngestBinding,
        request: &RemoteExportRequest,
        response: &RemoteDeltaResponse,
        prepared: Self::PreparedPage,
        observed_at: DateTime<Utc>,
    ) -> io::Result<RemoteDeltaNextRequestPosition> {
        debug_assert!(prepared.project_mapping.is_none());
        let received_at = prepared.received_at;
        self.with_session(binding, |session, writer| {
            Self::recover_locked(session, writer, observed_at)?;
            ensure_request_position(session.next_request_position()?, request)?;
            let page = session.prepare_page(request, response, received_at)?;
            apply_and_commit_remote_delta_page(session, writer, &page, observed_at)?;
            let _ = activate_remote_delta_bootstrap(session, writer, observed_at)?;
            session.next_request_position()
        })
    }

    fn publish_prepared_page(&mut self, prepared: &mut Self::PreparedPage) -> io::Result<()> {
        // Mapping may safely become visible before history. A crash or config
        // change after this CAS leaves only an unused observation; history is
        // fenced and committed separately after publication is durable.
        if let Some(mapping) = prepared.project_mapping.take() {
            self.published_project_mapping = Some(mapping.publish()?);
        }
        Ok(())
    }

    fn finish_page_publication(&mut self) -> io::Result<()> {
        if let Some(publication) = self.published_project_mapping.take() {
            publication.finish()?;
        }
        Ok(())
    }
}

/// Runs a bounded number of pages for one explicitly selected paired host.
///
/// `started_at` fixes the rolling 31-day range for the first page.  Continued
/// pages use the exact durable range returned by local ingest, including when
/// the run is resumed in a later process with a different wall clock.
pub fn sync_remote_delta_bounded(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    profile_id: HistoryProfileId,
    local: &mut impl RemoteDeltaLocalPhases,
    transport: &mut impl RemoteDeltaTransport,
    started_at: DateTime<Utc>,
    limits: RemoteSyncLimits,
) -> Result<RemoteSyncReport, RemoteSyncError> {
    let limits = limits.validate()?;
    let host = selected.host();
    let binding = build_remote_delta_ingest_binding(selected, profile_id)?;
    let mut position =
        preflight_remote_delta_position(config_store, selected, &binding, local, started_at)?;
    let mut report = RemoteSyncReport {
        pages_committed: 0,
        changes_committed: 0,
        live_state_changed: false,
        response_bytes: 0,
        completion: RemoteSyncCompletion::Continuation(position.clone()),
    };
    let run_deadline = Instant::now() + MAX_REMOTE_SYNC_RUN_TIME;

    for _ in 0..limits.max_pages.get() {
        let remaining_run_time = run_deadline.saturating_duration_since(Instant::now());
        if remaining_run_time.is_zero() {
            break;
        }
        let remaining_response_bytes = limits
            .max_response_bytes
            .saturating_sub(report.response_bytes);
        if remaining_response_bytes < MIN_REMOTE_RESPONSE_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES
        {
            break;
        }
        let max_page_bytes = remaining_response_bytes
            .saturating_sub(REMOTE_FRAME_HEADER_BYTES)
            .min(MAX_REMOTE_FRAME_ENCODED_BYTES) as u32;
        let range = position
            .exact_range
            .clone()
            .map(Ok)
            .unwrap_or_else(|| rolling_range(started_at))?;
        let request = build_delta_request(
            &binding,
            position.delta_cursor,
            position.known_live_revision,
            range,
            max_page_bytes,
        )?;

        // No local guard exists here: recover_and_position/commit_page own all
        // writer and ingest guards and drop them before returning.
        let exchange_timeout = limits.exchange_timeout.min(remaining_run_time);
        let exchange = transport.exchange_host(host, &request, exchange_timeout)?;
        let received_at = Utc::now();
        let next_total = report
            .response_bytes
            .checked_add(exchange.response_bytes)
            .ok_or(RemoteSyncError::ResponseBudgetExceeded)?;
        if next_total > limits.max_response_bytes {
            return Err(RemoteSyncError::ResponseBudgetExceeded);
        }
        report.response_bytes = next_total;

        exchange
            .response
            .validate_for_request(&request)
            .map_err(RemoteSyncError::Protocol)?;

        match &exchange.response.result {
            RemoteExportResponseBody::Delta { page, payload } => {
                validate_bound_envelope(&binding, &request, &exchange.response)?;
                let known_live_revision = delta_request(&request)?.known_live_revision;
                let mut prepared = local
                    .prepare_page(&binding, &request, &exchange.response, received_at)
                    .map_err(RemoteSyncError::Local)?;
                try_with_current_config(config_store, selected, || {
                    local.publish_prepared_page(&mut prepared)
                })?;
                local
                    .finish_page_publication()
                    .map_err(RemoteSyncError::Local)?;
                let commit = try_with_current_config(config_store, selected, || {
                    local.commit_page(&binding, &request, &exchange.response, prepared, Utc::now())
                });
                position = commit?;
                ensure_committed_position(&position, &request, page, payload)?;
                report.pages_committed += 1;
                report.changes_committed +=
                    payload.bucket_changes.len() + payload.session_digest_changes.len();
                report.live_state_changed |= payload.live.as_ref().is_some_and(|live| {
                    live.snapshot.is_some() && Some(live.live_revision) != known_live_revision
                });
                if !page.has_more {
                    report.completion = RemoteSyncCompletion::Complete;
                    return Ok(report);
                }
                report.completion = RemoteSyncCompletion::Continuation(position.clone());
            }
            RemoteExportResponseBody::Failure(failure) => {
                if failure.kind == RemoteFailureKind::CursorExpired
                    && delta_request(&request)?.delta_cursor.is_some()
                {
                    // Failure validation intentionally permits identity/version
                    // diagnostics. Cursor reset is state-changing, so require
                    // the complete exact bound envelope before acting on it.
                    validate_bound_envelope(&binding, &request, &exchange.response)?;
                    ensure_config_is_current(config_store, selected)?;
                    position = with_current_config(config_store, selected, || {
                        local.restart_bootstrap(&binding, &request, Utc::now())
                    })?;
                    ensure_cursorless_bootstrap_position(&position)?;
                    report.completion = RemoteSyncCompletion::BootstrapRestarted(position.clone());
                    return Ok(report);
                }
                return Err(RemoteSyncError::Remote(failure.clone()));
            }
            RemoteExportResponseBody::Probe(_)
            | RemoteExportResponseBody::FactSnapshot { .. }
            | RemoteExportResponseBody::FactDelta { .. } => {
                return Err(RemoteSyncError::UnexpectedResponse);
            }
        }
    }

    Ok(report)
}

/// Builds the exact durable binding without opening transport. Callers that
/// need to classify an existing bootstrap/incremental position may safely run
/// [`preflight_remote_delta_position`] before bandwidth admission, then let
/// the orchestrator revalidate it immediately before its first exchange.
pub(crate) fn build_remote_delta_ingest_binding(
    selected: &RemoteSyncHostSnapshot,
    profile_id: HistoryProfileId,
) -> Result<RemoteDeltaIngestBinding, RemoteSyncError> {
    let host = selected.host();
    let source = host
        .expected_source()
        .cloned()
        .ok_or_else(|| RemoteSyncError::HostNotPaired {
            host_id: host.id().to_owned(),
        })?;
    let redaction_profile = if host.redact_content() {
        RedactionProfile::Redacted
    } else {
        RedactionProfile::PreviewEnabled
    };
    Ok(RemoteDeltaIngestBinding::new(
        profile_id,
        source,
        redaction_profile,
        current_revisions(),
        RemoteDeltaRangePolicy::new(
            NonZeroU32::new(REMOTE_SYNC_WINDOW_MINUTES).expect("the fixed sync window is non-zero"),
            REMOTE_SYNC_OVERLAP_MINUTES,
            REMOTE_SYNC_INCLUDE_LIVE,
        )?,
    )?)
}

pub(crate) fn preflight_remote_delta_position(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    binding: &RemoteDeltaIngestBinding,
    local: &mut impl RemoteDeltaLocalPhases,
    observed_at: DateTime<Utc>,
) -> Result<RemoteDeltaNextRequestPosition, RemoteSyncError> {
    // A captured selection is not itself an authorization to mutate forever.
    // Fence the short recovery phase with the exact current host snapshot.
    ensure_config_is_current(config_store, selected).map_err(mark_pre_transport_error)?;
    with_current_config(config_store, selected, || {
        local.recover_and_position(binding, observed_at)
    })
    .map_err(mark_pre_transport_error)
}

fn build_delta_request(
    binding: &RemoteDeltaIngestBinding,
    cursor: Option<crate::remote_protocol::DeltaCursor>,
    known_live_revision: Option<std::num::NonZeroU64>,
    range: ExportRange,
    max_page_bytes: u32,
) -> Result<RemoteExportRequest, RemoteSyncError> {
    Ok(RemoteExportRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        client_version: env!("CARGO_PKG_VERSION")
            .parse()
            .map_err(RemoteSyncError::Protocol)?,
        expected_source: Some(binding.source().clone()),
        redaction_profile: binding.redaction_profile(),
        max_page_bytes,
        accepted_revisions: current_accepted_revisions(),
        request: RemoteExportRequestBody::Delta(DeltaRequest {
            delta_cursor: cursor,
            range,
            overlap_minutes: REMOTE_SYNC_OVERLAP_MINUTES,
            include_live: REMOTE_SYNC_INCLUDE_LIVE,
            known_live_revision,
        }),
    })
}

fn rolling_range(to: DateTime<Utc>) -> Result<ExportRange, RemoteSyncError> {
    let from = to
        .checked_sub_signed(Duration::minutes(i64::from(REMOTE_SYNC_WINDOW_MINUTES)))
        .ok_or(RemoteSyncError::InvalidStartedAt)?;
    Ok(ExportRange { from, to })
}

fn delta_request(request: &RemoteExportRequest) -> Result<&DeltaRequest, RemoteSyncError> {
    match &request.request {
        RemoteExportRequestBody::Delta(delta) => Ok(delta),
        RemoteExportRequestBody::Probe(_) | RemoteExportRequestBody::SessionFacts(_) => {
            Err(RemoteSyncError::UnexpectedResponse)
        }
    }
}

fn validate_bound_envelope(
    binding: &RemoteDeltaIngestBinding,
    request: &RemoteExportRequest,
    response: &RemoteDeltaResponse,
) -> Result<(), RemoteSyncError> {
    if request.expected_source.as_ref() != Some(binding.source())
        || request.redaction_profile != binding.redaction_profile()
        || response.source != *binding.source()
        || response.redaction_profile != binding.redaction_profile()
        || response.revisions != *binding.revisions()
    {
        return Err(RemoteSyncError::UnboundResponseEnvelope);
    }
    Ok(())
}

fn ensure_config_is_current(
    store: &RemotesConfigStore,
    expected: &RemoteSyncHostSnapshot,
) -> Result<(), RemoteSyncError> {
    let current = store.load().map_err(RemoteSyncError::Local)?;
    if current.config_revision() != expected.config_revision
        || current.host(expected.host.id()) != Some(&expected.host)
    {
        return Err(RemoteSyncError::ConfigurationChanged {
            host_id: expected.host.id().to_owned(),
        });
    }
    Ok(())
}

fn with_current_config<T>(
    store: &RemotesConfigStore,
    expected: &RemoteSyncHostSnapshot,
    operation: impl FnOnce() -> io::Result<T>,
) -> Result<T, RemoteSyncError> {
    store
        .with_current_host(expected.config_revision, &expected.host, operation)
        .map_err(RemoteSyncError::Local)
}

fn try_with_current_config<T>(
    store: &RemotesConfigStore,
    expected: &RemoteSyncHostSnapshot,
    operation: impl FnOnce() -> io::Result<T>,
) -> Result<T, RemoteSyncError> {
    match store
        .try_with_current_host(expected.config_revision, &expected.host, operation)
        .map_err(RemoteSyncError::Local)?
    {
        TryCurrentHost::Current(value) => Ok(value),
        TryCurrentHost::Busy => Err(RemoteSyncError::Local(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote configuration is busy; retry the staged page",
        ))),
        TryCurrentHost::Changed => Err(RemoteSyncError::ConfigurationChanged {
            host_id: expected.host.id().to_owned(),
        }),
    }
}

fn mark_pre_transport_error(error: RemoteSyncError) -> RemoteSyncError {
    match error {
        RemoteSyncError::Local(error) => RemoteSyncError::PreTransportLocal(error),
        error => error,
    }
}

fn ensure_request_position(
    position: RemoteDeltaNextRequestPosition,
    request: &RemoteExportRequest,
) -> io::Result<()> {
    let RemoteExportRequestBody::Delta(delta) = &request.request else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync local phase accepts only delta requests",
        ));
    };
    if position.delta_cursor != delta.delta_cursor
        || position.known_live_revision != delta.known_live_revision
        || position
            .exact_range
            .as_ref()
            .is_some_and(|range| range != &delta.range)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote ingest position changed before page commit",
        ));
    }
    Ok(())
}

fn ensure_committed_position(
    position: &RemoteDeltaNextRequestPosition,
    request: &RemoteExportRequest,
    page: &crate::remote_protocol::DeltaPage,
    payload: &DeltaPayload,
) -> Result<(), RemoteSyncError> {
    let RemoteExportRequestBody::Delta(delta) = &request.request else {
        return Err(RemoteSyncError::Local(io::Error::new(
            io::ErrorKind::InvalidData,
            "committed remote sync request is not delta",
        )));
    };
    let expected_range = page.has_more.then_some(&delta.range);
    let expected_live_revision = payload.live.as_ref().map(|live| live.live_revision);
    if position.delta_cursor != Some(page.next_delta_cursor)
        || position.exact_range.as_ref() != expected_range
        || position.known_live_revision != expected_live_revision
    {
        return Err(RemoteSyncError::Local(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote ingest committed an inconsistent continuation position",
        )));
    }
    Ok(())
}

fn ensure_cursorless_bootstrap_position(
    position: &RemoteDeltaNextRequestPosition,
) -> Result<(), RemoteSyncError> {
    if position.delta_cursor.is_some() || position.exact_range.is_some() {
        return Err(RemoteSyncError::Local(io::Error::new(
            io::ErrorKind::InvalidData,
            "cursor-expiry recovery did not create a fresh cursorless bootstrap",
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub enum RemoteSyncError {
    HostNotPaired {
        host_id: String,
    },
    HostNotEnabledForAutomaticSync {
        host_id: String,
    },
    StaleHostSelection {
        host_id: String,
    },
    ConfigurationChanged {
        host_id: String,
    },
    InvalidLimits(&'static str),
    InvalidStartedAt,
    ResponseBudgetExceeded,
    UnboundResponseEnvelope,
    UnexpectedResponse,
    /// A local/config preflight failure that is structurally guaranteed to
    /// precede the first transport exchange.
    PreTransportLocal(io::Error),
    Local(io::Error),
    Protocol(RemoteProtocolError),
    Transport(RemoteTransportError),
    /// A completed transport path left an SSH helper outside the owned
    /// process tree. This content-free promotion is used when a secondary
    /// automatic exchange (for example session facts or a hard-cap probe)
    /// cannot retain the original transport error but must stop scheduling.
    ProcessContainment,
    Remote(RemoteFailure),
}

impl fmt::Display for RemoteSyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostNotPaired { host_id } => {
                write!(formatter, "remote host {host_id:?} is not paired")
            }
            Self::HostNotEnabledForAutomaticSync { host_id } => write!(
                formatter,
                "remote host {host_id:?} is not enabled for automatic synchronization"
            ),
            Self::StaleHostSelection { host_id } => write!(
                formatter,
                "remote host selection {host_id:?} is not the exact loaded allowlist entry"
            ),
            Self::ConfigurationChanged { host_id } => write!(
                formatter,
                "remote host {host_id:?} changed while its sync request was in flight"
            ),
            Self::InvalidLimits(message) => formatter.write_str(message),
            Self::InvalidStartedAt => {
                formatter.write_str("remote sync start time cannot represent the 31-day window")
            }
            Self::ResponseBudgetExceeded => {
                formatter.write_str("remote sync response-byte budget was exceeded")
            }
            Self::UnboundResponseEnvelope => formatter
                .write_str("remote response envelope does not match the selected durable binding"),
            Self::UnexpectedResponse => {
                formatter.write_str("remote returned a non-delta response to a delta request")
            }
            Self::PreTransportLocal(error) => {
                write!(
                    formatter,
                    "remote sync pre-transport local phase failed: {error}"
                )
            }
            Self::Local(error) => write!(formatter, "remote sync local phase failed: {error}"),
            Self::Protocol(error) => write!(formatter, "remote sync protocol failed: {error}"),
            Self::Transport(error) => write!(formatter, "remote sync transport failed: {error}"),
            Self::ProcessContainment => formatter.write_str(
                "remote sync paused because the SSH process tree could not be fully reclaimed",
            ),
            Self::Remote(failure) => write!(
                formatter,
                "remote sync failed ({:?}): {}",
                failure.kind, failure.message
            ),
        }
    }
}

impl std::error::Error for RemoteSyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PreTransportLocal(error) | Self::Local(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for RemoteSyncError {
    fn from(error: io::Error) -> Self {
        Self::Local(error)
    }
}

impl From<RemoteTransportError> for RemoteSyncError {
    fn from(error: RemoteTransportError) -> Self {
        Self::Transport(error)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::num::NonZeroU64;
    use std::rc::Rc;
    use std::str::FromStr;
    use std::sync::mpsc;
    use std::thread;

    use chrono::TimeZone;
    use tempfile::TempDir;

    use super::*;
    use crate::history_ownership::{InitializeV1Outcome, OwnershipCasOutcome};
    use crate::project_mapping::SourceObservedProject;
    use crate::remote_protocol::{
        BinaryVersion, DeltaCursor, DeltaPage, ProtocolRevisions, RemoteApiCostAmount,
        RemoteDeltaCoverage, RemoteDeltaStats, RemoteExportResponse, RemoteLiveSnapshot,
        RemoteLiveState, RemoteProjectDescriptor, RemoteProjectUsageGroup, RemoteTiming,
        RemoteTokenUsage, RemoteU128, RemoteUsageBucket, RemoteUsageBucketChange,
        RemoteUsageBucketMutation, SourceGeneration,
    };
    use crate::remotes_config::{
        RemoteHostConfig, RemoteHostEdit, RemotesConfigMutation, RemotesConfigStore,
    };
    use crate::source_history::{SourceKind, SourceMetadata};

    const PROFILE: &str = "0123456789abcdef";
    const SOURCE: &str = "node-0123456789abcdef0123456789abcdef";
    const OTHER_SOURCE: &str = "node-fedcba9876543210fedcba9876543210";

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn source(node_id: &str) -> SourceGeneration {
        SourceGeneration {
            node_id: node_id.parse().unwrap(),
            generation: NonZeroU64::new(7).unwrap(),
        }
    }

    fn paired_config(
        temp: &TempDir,
    ) -> (
        RemotesConfigStore,
        RemotesConfig,
        RemoteHostConfig,
        RemoteSyncHostSnapshot,
    ) {
        let store = RemotesConfigStore::new(temp.path().join("config").join("remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        let paired = store
            .update(
                configured.config_revision(),
                RemotesConfigMutation::pair_pin("dev", source(SOURCE)),
            )
            .unwrap();
        let host = paired.host("dev").unwrap().clone();
        let selected = RemoteSyncHostSnapshot::capture_manual(&paired, &host).unwrap();
        (store, paired, host, selected)
    }

    #[derive(Clone, Copy, Debug)]
    enum FakeReply {
        Delta { sequence: u64, has_more: bool },
        EmptyDelta,
        CursorExpired,
        CursorExpiredFromWrongSource,
    }

    struct FakeTransport {
        local_guard: Rc<Cell<bool>>,
        replies: VecDeque<FakeReply>,
        requests: Vec<RemoteExportRequest>,
        response_bytes: usize,
        on_exchange: Option<Box<dyn FnMut()>>,
    }

    impl FakeTransport {
        fn new(local_guard: Rc<Cell<bool>>, replies: impl IntoIterator<Item = FakeReply>) -> Self {
            Self {
                local_guard,
                replies: replies.into_iter().collect(),
                requests: Vec::new(),
                response_bytes: 512,
                on_exchange: None,
            }
        }
    }

    impl RemoteDeltaTransport for FakeTransport {
        fn exchange(
            &mut self,
            ssh_host: &str,
            request: &RemoteExportRequest,
            _timeout: StdDuration,
        ) -> Result<RemoteExchangeReport<DeltaPayload, EmptyRemotePayload>, RemoteTransportError>
        {
            assert_eq!(ssh_host, "dev-alias");
            assert!(
                !self.local_guard.get(),
                "transport ran while a local phase guard was held"
            );
            if let Some(hook) = self.on_exchange.as_mut() {
                hook();
            }
            let reply = self.replies.pop_front().expect("a queued fake response");
            self.requests.push(request.clone());
            let response = fake_response(request, reply);
            Ok(RemoteExchangeReport {
                response,
                elapsed: StdDuration::from_millis(5),
                request_bytes: 128,
                response_bytes: self.response_bytes,
                response_decoded_bytes: self.response_bytes,
                stderr_bytes: 0,
            })
        }
    }

    struct FakeLocal {
        guard: Rc<Cell<bool>>,
        position: RemoteDeltaNextRequestPosition,
        recoveries: usize,
        commits: usize,
        restarts: usize,
        bootstrap_generation: u64,
        preserve_expired_position: bool,
        advance_before_restart: bool,
    }

    impl FakeLocal {
        fn new(guard: Rc<Cell<bool>>) -> Self {
            Self {
                guard,
                position: RemoteDeltaNextRequestPosition {
                    delta_cursor: None,
                    exact_range: None,
                    known_live_revision: None,
                },
                recoveries: 0,
                commits: 0,
                restarts: 0,
                bootstrap_generation: 1,
                preserve_expired_position: false,
                advance_before_restart: false,
            }
        }

        fn local<T>(&self, operation: impl FnOnce() -> T) -> T {
            assert!(!self.guard.replace(true));
            let result = operation();
            assert!(self.guard.replace(false));
            result
        }
    }

    impl RemoteDeltaLocalPhases for FakeLocal {
        type PreparedPage = ();

        fn recover_and_position(
            &mut self,
            binding: &RemoteDeltaIngestBinding,
            _observed_at: DateTime<Utc>,
        ) -> io::Result<RemoteDeltaNextRequestPosition> {
            assert_eq!(
                binding.range_policy().window_minutes().get(),
                REMOTE_SYNC_WINDOW_MINUTES
            );
            assert_eq!(
                binding.range_policy().overlap_minutes(),
                REMOTE_SYNC_OVERLAP_MINUTES
            );
            assert!(binding.range_policy().include_live());
            let position = self.local(|| self.position.clone());
            self.recoveries += 1;
            Ok(position)
        }

        fn restart_bootstrap(
            &mut self,
            _binding: &RemoteDeltaIngestBinding,
            expired_request: &RemoteExportRequest,
            _observed_at: DateTime<Utc>,
        ) -> io::Result<RemoteDeltaNextRequestPosition> {
            self.local(|| ());
            if self.advance_before_restart {
                let cursor = self.position.delta_cursor.as_mut().unwrap();
                cursor.sequence += 1;
            }
            ensure_request_position(self.position.clone(), expired_request)?;
            self.restarts += 1;
            self.bootstrap_generation += 1;
            if !self.preserve_expired_position {
                self.position = RemoteDeltaNextRequestPosition {
                    delta_cursor: None,
                    exact_range: None,
                    known_live_revision: None,
                };
            }
            Ok(self.position.clone())
        }

        fn prepare_page(
            &mut self,
            _binding: &RemoteDeltaIngestBinding,
            _request: &RemoteExportRequest,
            _response: &RemoteDeltaResponse,
            _received_at: DateTime<Utc>,
        ) -> io::Result<Self::PreparedPage> {
            self.local(|| ());
            Ok(())
        }

        fn commit_page(
            &mut self,
            _binding: &RemoteDeltaIngestBinding,
            request: &RemoteExportRequest,
            response: &RemoteDeltaResponse,
            (): Self::PreparedPage,
            _observed_at: DateTime<Utc>,
        ) -> io::Result<RemoteDeltaNextRequestPosition> {
            self.local(|| ());
            let RemoteExportRequestBody::Delta(delta) = &request.request else {
                panic!("sync request must be delta")
            };
            let RemoteExportResponseBody::Delta { page, payload } = &response.result else {
                panic!("committed response must be delta")
            };
            self.commits += 1;
            self.position = RemoteDeltaNextRequestPosition {
                delta_cursor: Some(page.next_delta_cursor),
                exact_range: page.has_more.then(|| delta.range.clone()),
                known_live_revision: payload.live.as_ref().map(|live| live.live_revision),
            };
            Ok(self.position.clone())
        }
    }

    fn fake_response(request: &RemoteExportRequest, reply: FakeReply) -> RemoteDeltaResponse {
        let RemoteExportRequestBody::Delta(delta) = &request.request else {
            panic!("fake transport accepts only delta requests")
        };
        let revisions = current_revisions();
        let live = || {
            delta.include_live.then(|| {
                let revision = delta
                    .known_live_revision
                    .unwrap_or_else(|| NonZeroU64::new(1).unwrap());
                RemoteLiveState {
                    live_revision: revision,
                    snapshot: (delta.known_live_revision != Some(revision)).then(|| {
                        RemoteLiveSnapshot {
                            captured_at: delta.range.to,
                            tasks: Vec::new(),
                            turns: Vec::new(),
                        }
                    }),
                }
            })
        };
        let result = match reply {
            FakeReply::Delta { sequence, has_more } => {
                let generation = delta
                    .delta_cursor
                    .map_or_else(|| NonZeroU64::new(11).unwrap(), |cursor| cursor.generation);
                let from_sequence = delta.delta_cursor.map_or(sequence, |cursor| {
                    assert_eq!(sequence, cursor.sequence + 1);
                    sequence
                });
                RemoteExportResponseBody::Delta {
                    page: DeltaPage {
                        generation,
                        from_sequence,
                        through_sequence: sequence,
                        next_delta_cursor: DeltaCursor {
                            generation,
                            sequence,
                        },
                        has_more,
                    },
                    payload: DeltaPayload {
                        coverage: RemoteDeltaCoverage {
                            requested_range: delta.range.clone(),
                            covered_range: Some(delta.range.clone()),
                            range_complete: true,
                            partial_reasons: Vec::new(),
                        },
                        project_descriptors: Vec::new(),
                        bucket_changes: vec![RemoteUsageBucketChange {
                            sequence: NonZeroU64::new(sequence).unwrap(),
                            starts_at: delta.range.from,
                            revision: NonZeroU64::new(1).unwrap(),
                            mutation: RemoteUsageBucketMutation::Tombstone,
                        }],
                        session_digest_changes: Vec::new(),
                        live: live(),
                        stats: RemoteDeltaStats {
                            journal_records_scanned: 1,
                            bucket_changes_emitted: 1,
                            ..RemoteDeltaStats::default()
                        },
                        warnings: Vec::new(),
                    },
                }
            }
            FakeReply::EmptyDelta => {
                let generation = delta
                    .delta_cursor
                    .map_or_else(|| NonZeroU64::new(11).unwrap(), |cursor| cursor.generation);
                let sequence = delta.delta_cursor.map_or(0, |cursor| cursor.sequence);
                RemoteExportResponseBody::Delta {
                    page: DeltaPage {
                        generation,
                        from_sequence: sequence,
                        through_sequence: sequence,
                        next_delta_cursor: DeltaCursor {
                            generation,
                            sequence,
                        },
                        has_more: false,
                    },
                    payload: DeltaPayload {
                        coverage: RemoteDeltaCoverage {
                            requested_range: delta.range.clone(),
                            covered_range: Some(delta.range.clone()),
                            range_complete: true,
                            partial_reasons: Vec::new(),
                        },
                        project_descriptors: Vec::new(),
                        bucket_changes: Vec::new(),
                        session_digest_changes: Vec::new(),
                        live: live(),
                        stats: RemoteDeltaStats::default(),
                        warnings: Vec::new(),
                    },
                }
            }
            FakeReply::CursorExpired | FakeReply::CursorExpiredFromWrongSource => {
                RemoteExportResponseBody::Failure(RemoteFailure {
                    kind: RemoteFailureKind::CursorExpired,
                    message: "remote delta cursor expired".to_owned(),
                    retry_after_seconds: None,
                })
            }
        };
        let response_source = if matches!(reply, FakeReply::CursorExpiredFromWrongSource) {
            source(OTHER_SOURCE)
        } else {
            request.expected_source.clone().unwrap()
        };
        let received_at = delta.range.to + Duration::seconds(1);
        RemoteExportResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: BinaryVersion::from_str("0.4.0-test").unwrap(),
            source: response_source,
            redaction_profile: request.redaction_profile,
            revisions,
            observed_at: received_at,
            timing: RemoteTiming {
                remote_received_at: received_at,
                remote_sent_at: received_at + Duration::milliseconds(1),
            },
            result,
        }
    }

    fn project_response(
        request: &RemoteExportRequest,
        key: crate::source_model::ObservedProjectKey,
        label: &str,
    ) -> RemoteDeltaResponse {
        let mut response = fake_response(
            request,
            FakeReply::Delta {
                sequence: 1,
                has_more: false,
            },
        );
        let RemoteExportRequestBody::Delta(delta) = &request.request else {
            unreachable!()
        };
        let revisions = response.revisions.clone();
        let RemoteExportResponseBody::Delta { payload, .. } = &mut response.result else {
            unreachable!()
        };
        let tokens = RemoteTokenUsage {
            input_tokens: 1,
            total_tokens: 1,
            ..RemoteTokenUsage::default()
        };
        payload.project_descriptors = vec![RemoteProjectDescriptor {
            observed_project_key: key.clone(),
            display_label: label.parse().unwrap(),
            git_evidence: crate::remote_protocol::RemoteGitRepositoryEvidence::Unavailable,
        }];
        payload.stats.project_descriptors_emitted = 1;
        payload.bucket_changes[0].mutation =
            RemoteUsageBucketMutation::Upsert(Box::new(RemoteUsageBucket {
                starts_at: delta.range.from,
                ends_at: delta.range.from + Duration::minutes(15),
                sampled_at: delta.range.from + Duration::minutes(15),
                token_usage: tokens,
                estimated_cost_units: RemoteU128::new(1),
                api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
                long_context_usage_unknown: false,
                api_equivalent_cost: RemoteApiCostAmount::default(),
                call_count: 1,
                metric_revision: revisions.metric,
                estimator_revision: revisions.estimator,
                project_breakdown_revision: revisions.project_breakdown,
                api_pricing_catalog_revision: revisions.api_pricing_catalog,
                model_groups: Vec::new(),
                project_groups: vec![RemoteProjectUsageGroup {
                    observed_project_key: Some(key),
                    emitting_thread_id: "thread-1".parse().unwrap(),
                    emitting_turn_id: Some("turn-1".to_owned()),
                    parent_thread_id: None,
                    root_session_thread_id: Some("thread-1".parse().unwrap()),
                    root_session_turn_id: Some("turn-1".to_owned()),
                    title_preview: None,
                    message_preview: None,
                    token_usage: tokens,
                    estimated_cost_units: RemoteU128::new(1),
                    api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
                    api_equivalent_cost: RemoteApiCostAmount::default(),
                    call_count: 1,
                }],
                partial_reasons: Vec::new(),
            }));
        response
    }

    fn limits(max_pages: usize) -> RemoteSyncLimits {
        RemoteSyncLimits {
            max_pages: NonZeroUsize::new(max_pages).unwrap(),
            ..RemoteSyncLimits::default()
        }
    }

    #[test]
    fn fixed_request_policy_and_transport_has_no_local_guard() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        let mut transport = FakeTransport::new(
            guard,
            [FakeReply::Delta {
                sequence: 1,
                has_more: false,
            }],
        );
        let started_at = at(30, 12);

        let report = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            started_at,
            RemoteSyncLimits::default(),
        )
        .unwrap();

        assert_eq!(report.pages_committed, 1);
        assert_eq!(report.changes_committed, 1);
        assert_eq!(report.completion, RemoteSyncCompletion::Complete);
        assert_eq!(local.recoveries, 1);
        assert_eq!(local.commits, 1);
        let RemoteExportRequestBody::Delta(delta) = &transport.requests[0].request else {
            panic!("request must be delta")
        };
        assert_eq!(delta.delta_cursor, None);
        assert_eq!(delta.range.to, started_at);
        assert_eq!(delta.range.from, started_at - Duration::days(31));
        assert_eq!(delta.overlap_minutes, 60);
        assert!(delta.include_live);
        assert_eq!(delta.known_live_revision, None);
        assert_eq!(transport.requests[0].expected_source, Some(source(SOURCE)));
        assert_eq!(
            transport.requests[0].redaction_profile,
            RedactionProfile::Redacted
        );
        assert_eq!(
            transport.requests[0].accepted_revisions,
            current_accepted_revisions()
        );
    }

    #[test]
    fn terminal_empty_delta_page_reports_full_live_replacement_activity() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        let mut transport = FakeTransport::new(guard, [FakeReply::EmptyDelta]);

        let report = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            RemoteSyncLimits::default(),
        )
        .unwrap();

        assert_eq!(report.pages_committed, 1);
        assert_eq!(report.changes_committed, 0);
        assert!(report.live_state_changed);
        assert!(report.has_activity());
        assert_eq!(report.completion, RemoteSyncCompletion::Complete);
    }

    #[test]
    fn revision_only_live_confirmation_is_not_reported_as_activity() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        local.position.known_live_revision = NonZeroU64::new(1);
        let mut transport = FakeTransport::new(guard, [FakeReply::EmptyDelta]);

        let report = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            RemoteSyncLimits::default(),
        )
        .unwrap();

        assert_eq!(report.changes_committed, 0);
        assert!(!report.live_state_changed);
        assert!(!report.has_activity());
    }

    #[test]
    fn delayed_pending_wal_recovery_preserves_original_center_receive_time() {
        let temp = TempDir::new().unwrap();
        let (_, _, _, selected) = paired_config(&temp);
        let profile: HistoryProfileId = PROFILE.parse().unwrap();
        let state_root = temp.path().join("state");
        let ownership = HistoryOwnershipStore::new(
            state_root.clone(),
            profile.clone(),
            RedactionProfile::Redacted,
        );
        let history = SourceHistoryStore::new(state_root, profile.clone());

        let lease = ownership.acquire_writer_lease().unwrap();
        let v1 = match ownership.initialize_v1_active(&lease).unwrap() {
            InitializeV1Outcome::Initialized(manifest)
            | InitializeV1Outcome::Existing(manifest) => manifest,
        };
        let migrating = match ownership.begin_migration(&lease, &v1).unwrap() {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected ownership conflict"),
        };
        let authority = ownership.authorize_v2_write(&lease, &migrating).unwrap();
        let writer = history.writer(&authority).unwrap();
        writer
            .save_source_metadata(
                &SourceMetadata::new(
                    selected.host().expected_source().unwrap().node_id.clone(),
                    SourceKind::Ssh,
                    "remote",
                )
                .unwrap(),
            )
            .unwrap();
        let active = match ownership
            .compare_and_transition(&lease, &migrating, HistoryOwnershipState::V2Active)
            .unwrap()
        {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected ownership conflict"),
        };
        assert_eq!(active.state(), HistoryOwnershipState::V2Active);
        drop(lease);

        let mappings =
            ProjectMappingStore::new(temp.path().join("mapping-config/project-mappings.json"));
        let binding = build_remote_delta_ingest_binding(&selected, profile).unwrap();
        let mut initial = FilesystemRemoteDeltaLocalPhases::new_with_project_mapping_store(
            &ownership,
            &history,
            mappings.clone(),
        );
        assert_eq!(
            initial.recover_and_position(&binding, at(30, 12)).unwrap(),
            RemoteDeltaNextRequestPosition {
                delta_cursor: None,
                exact_range: None,
                known_live_revision: None,
            }
        );
        drop(initial);

        let range = rolling_range(at(30, 12)).unwrap();
        let request = build_delta_request(
            &binding,
            None,
            None,
            range,
            MAX_REMOTE_FRAME_ENCODED_BYTES as u32,
        )
        .unwrap();
        let response = fake_response(&request, FakeReply::EmptyDelta);
        let received_at = response.observed_at + Duration::minutes(2);
        let recovered_at = received_at + Duration::hours(24);
        let ingest = RemoteDeltaIngestStateStore::new(history.clone(), binding.clone()).unwrap();
        let mut pending = ingest.try_begin().unwrap();
        pending
            .prepare_page(&request, &response, received_at)
            .unwrap();
        drop(pending); // Crash after the WAL publish and before local apply.

        let mut recovered = FilesystemRemoteDeltaLocalPhases::new_with_project_mapping_store(
            &ownership, &history, mappings,
        );
        recovered
            .recover_and_position(&binding, recovered_at)
            .unwrap();
        let live = history
            .load_remote_live_state(&binding.source().node_id)
            .unwrap()
            .unwrap();
        assert_eq!(live.remote_observed_at, response.observed_at);
        assert_eq!(live.received_at, received_at);
        assert_ne!(live.received_at, recovered_at);
        assert!(recovered_at.signed_duration_since(live.received_at) > Duration::minutes(15));
    }

    #[test]
    fn filesystem_phase_registers_exact_source_descriptors_and_refreshes_metadata() {
        let temp = TempDir::new().unwrap();
        let (_, _, _, selected) = paired_config(&temp);
        let profile: HistoryProfileId = PROFILE.parse().unwrap();
        let state_root = temp.path().join("state");
        let ownership = HistoryOwnershipStore::new(
            state_root.clone(),
            profile.clone(),
            RedactionProfile::Redacted,
        );
        let history = SourceHistoryStore::new(state_root, profile.clone());
        let mappings =
            ProjectMappingStore::new(temp.path().join("mapping-config/project-mappings.json"));
        let phases = FilesystemRemoteDeltaLocalPhases::new_with_project_mapping_store(
            &ownership,
            &history,
            mappings.clone(),
        );
        let binding = build_remote_delta_ingest_binding(&selected, profile).unwrap();
        let range = rolling_range(at(30, 12)).unwrap();
        let request = build_delta_request(
            &binding,
            None,
            None,
            range,
            MAX_REMOTE_FRAME_ENCODED_BYTES as u32,
        )
        .unwrap();
        let key: crate::source_model::ObservedProjectKey =
            format!("opk-hmac-sha256-v1-{}", "a".repeat(64))
                .parse()
                .unwrap();

        let first = project_response(&request, key.clone(), "initial label");
        phases
            .prepare_project_descriptors(&binding, &request, &first)
            .unwrap()
            .unwrap()
            .publish()
            .unwrap()
            .finish()
            .unwrap();
        let initial = mappings.load().unwrap();
        let initial_projection = initial
            .projection()
            .resolve(&binding.source().node_id, &key)
            .unwrap()
            .clone();

        let refreshed = project_response(&request, key.clone(), "refreshed label");
        phases
            .prepare_project_descriptors(&binding, &request, &refreshed)
            .unwrap()
            .unwrap()
            .publish()
            .unwrap()
            .finish()
            .unwrap();
        let after_refresh = mappings.load().unwrap();
        let refreshed_projection = after_refresh
            .projection()
            .resolve(&binding.source().node_id, &key)
            .unwrap()
            .clone();
        assert_eq!(
            initial_projection.instance_id(),
            refreshed_projection.instance_id()
        );
        assert_eq!(
            refreshed_projection.display_label().unwrap().as_str(),
            "refreshed label"
        );

        let mut wrong_source = project_response(&request, key.clone(), "must not publish");
        wrong_source.source = source(OTHER_SOURCE);
        let before_rejected = mappings.load().unwrap();
        let error = phases
            .prepare_project_descriptors(&binding, &request, &wrong_source)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(mappings.load().unwrap(), before_rejected);

        let blocked_parent = temp.path().join("mapping-parent-is-a-file");
        std::fs::write(&blocked_parent, b"blocked").unwrap();
        let blocked = FilesystemRemoteDeltaLocalPhases::new_with_project_mapping_store(
            &ownership,
            &history,
            ProjectMappingStore::new(blocked_parent.join("project-mappings.json")),
        );
        blocked
            .prepare_project_descriptors(&binding, &request, &refreshed)
            .unwrap_err();
        assert_eq!(
            ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
    }

    #[test]
    fn center_rejects_unreferenced_descriptors_before_mapping_staging() {
        let temp = TempDir::new().unwrap();
        let (_, _, _, selected) = paired_config(&temp);
        let profile: HistoryProfileId = PROFILE.parse().unwrap();
        let ownership = HistoryOwnershipStore::new(
            temp.path().join("state"),
            profile.clone(),
            RedactionProfile::Redacted,
        );
        let history = SourceHistoryStore::new(temp.path().join("state"), profile.clone());
        let mappings =
            ProjectMappingStore::new(temp.path().join("mapping-config/project-mappings.json"));
        let phases = FilesystemRemoteDeltaLocalPhases::new_with_project_mapping_store(
            &ownership,
            &history,
            mappings.clone(),
        );
        let binding = build_remote_delta_ingest_binding(&selected, profile).unwrap();
        let request = build_delta_request(
            &binding,
            None,
            None,
            rolling_range(at(30, 12)).unwrap(),
            MAX_REMOTE_FRAME_ENCODED_BYTES as u32,
        )
        .unwrap();
        let referenced_key: crate::source_model::ObservedProjectKey =
            format!("opk-hmac-sha256-v1-{}", "a".repeat(64))
                .parse()
                .unwrap();
        let unused_key: crate::source_model::ObservedProjectKey =
            format!("opk-hmac-sha256-v1-{}", "b".repeat(64))
                .parse()
                .unwrap();
        let mut response = project_response(&request, referenced_key, "referenced");
        let RemoteExportResponseBody::Delta { payload, .. } = &mut response.result else {
            unreachable!()
        };
        payload.project_descriptors.push(RemoteProjectDescriptor {
            observed_project_key: unused_key,
            display_label: "unused".parse().unwrap(),
            git_evidence: crate::remote_protocol::RemoteGitRepositoryEvidence::Unavailable,
        });
        payload.stats.project_descriptors_emitted = 2;

        let center_error = validate_center_descriptor_references(payload).unwrap_err();
        assert_eq!(center_error.kind(), io::ErrorKind::InvalidData);

        let error = phases
            .prepare_project_descriptors(&binding, &request, &response)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            mappings
                .load()
                .is_err_and(|error| { error.kind() == io::ErrorKind::NotFound })
        );
        assert_eq!(
            ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
    }

    #[test]
    fn unchanged_descriptor_batch_cas_failure_cannot_commit_history() {
        let temp = TempDir::new().unwrap();
        let (_, _, _, selected) = paired_config(&temp);
        let profile: HistoryProfileId = PROFILE.parse().unwrap();
        let state_root = temp.path().join("state");
        let ownership = HistoryOwnershipStore::new(
            state_root.clone(),
            profile.clone(),
            RedactionProfile::Redacted,
        );
        let history = SourceHistoryStore::new(state_root, profile.clone());
        let mappings =
            ProjectMappingStore::new(temp.path().join("mapping-config/project-mappings.json"));
        let mut phases = FilesystemRemoteDeltaLocalPhases::new_with_project_mapping_store(
            &ownership,
            &history,
            mappings.clone(),
        );
        let binding = build_remote_delta_ingest_binding(&selected, profile).unwrap();
        let request = build_delta_request(
            &binding,
            None,
            None,
            rolling_range(at(30, 12)).unwrap(),
            MAX_REMOTE_FRAME_ENCODED_BYTES as u32,
        )
        .unwrap();
        let key: crate::source_model::ObservedProjectKey =
            format!("opk-hmac-sha256-v1-{}", "f".repeat(64))
                .parse()
                .unwrap();
        let response = project_response(&request, key.clone(), "stable");
        phases
            .prepare_project_descriptors(&binding, &request, &response)
            .unwrap()
            .unwrap()
            .publish()
            .unwrap()
            .finish()
            .unwrap();

        // The response now resolves without changing any mapping metadata, but
        // it must still be fenced against a later purge or mapping rewrite.
        let mut prepared = phases
            .prepare_page(&binding, &request, &response, at(30, 12))
            .unwrap();
        let before_change = mappings.load().unwrap();
        mappings
            .resolve_or_create(
                before_change.revision(),
                ProjectObservation::new(SourceObservedProject::new(
                    binding.source().node_id.clone(),
                    format!("opk-hmac-sha256-v1-{}", "1".repeat(64))
                        .parse()
                        .unwrap(),
                )),
            )
            .unwrap();
        let error = phases.publish_prepared_page(&mut prepared).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        phases.finish_page_publication().unwrap();
        assert_eq!(
            ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized,
            "mapping CAS failure must occur before history initialization or page commit"
        );
    }

    fn assert_config_mutation_overtakes_blocked_mapping_prepare(
        mutation: impl FnOnce() -> RemotesConfigMutation,
    ) {
        let temp = TempDir::new().unwrap();
        let (store, paired, _, _) = paired_config(&temp);
        let current = store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        let selected =
            RemoteSyncHostSnapshot::capture_manual(&current, current.host("dev").unwrap()).unwrap();
        let profile: HistoryProfileId = PROFILE.parse().unwrap();
        let state_root = temp.path().join("state");
        let mapping_path = temp
            .path()
            .join("mapping-config")
            .join("project-mappings.json");
        let mappings = ProjectMappingStore::new(mapping_path.clone());
        mappings.load_or_create().unwrap();

        // Hold the real filesystem mapping lock before staging starts. If
        // staging retained a remotes-config fence while waiting here, the
        // disable/remove transaction below would deadlock behind it.
        let mapping_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(mapping_path.parent().unwrap().join("project-mappings.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&mapping_lock).unwrap();

        let binding = build_remote_delta_ingest_binding(&selected, profile.clone()).unwrap();
        let request = build_delta_request(
            &binding,
            None,
            None,
            rolling_range(at(30, 12)).unwrap(),
            MAX_REMOTE_FRAME_ENCODED_BYTES as u32,
        )
        .unwrap();
        let key: crate::source_model::ObservedProjectKey =
            format!("opk-hmac-sha256-v1-{}", "c".repeat(64))
                .parse()
                .unwrap();
        let response = project_response(&request, key.clone(), "late page");
        let (preparing_tx, preparing_rx) = mpsc::channel();
        let (sync_result_tx, sync_result_rx) = mpsc::channel();
        let sync_store = store.clone();
        let sync_selected = selected.clone();
        let sync_state_root = state_root.clone();
        let sync_mapping_path = mapping_path.clone();
        let sync_binding = binding.clone();
        let sync_request = request.clone();
        let sync_response = response.clone();
        let sync_thread = thread::spawn(move || {
            let ownership = HistoryOwnershipStore::new(
                sync_state_root.clone(),
                profile.clone(),
                RedactionProfile::Redacted,
            );
            let history = SourceHistoryStore::new(sync_state_root, profile);
            let mut phases = FilesystemRemoteDeltaLocalPhases::new_with_project_mapping_store(
                &ownership,
                &history,
                ProjectMappingStore::new(sync_mapping_path),
            );
            preparing_tx.send(()).unwrap();
            let mut prepared = phases
                .prepare_page(&sync_binding, &sync_request, &sync_response, at(30, 12))
                .unwrap();
            let result = match try_with_current_config(&sync_store, &sync_selected, || {
                phases.publish_prepared_page(&mut prepared)
            }) {
                Ok(()) => match phases.finish_page_publication() {
                    Ok(()) => try_with_current_config(&sync_store, &sync_selected, || {
                        phases.commit_page(
                            &sync_binding,
                            &sync_request,
                            &sync_response,
                            prepared,
                            at(30, 12),
                        )
                    }),
                    Err(error) => Err(RemoteSyncError::Local(error)),
                },
                Err(error) => Err(error),
            };
            sync_result_tx.send(result).unwrap();
        });
        preparing_rx
            .recv_timeout(StdDuration::from_secs(1))
            .unwrap();
        thread::sleep(StdDuration::from_millis(50));
        assert!(matches!(
            sync_result_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        let (mutation_tx, mutation_rx) = mpsc::channel();
        let mutation_store = store.clone();
        let expected_revision = current.config_revision();
        let mutation = mutation();
        let mutation_thread = thread::spawn(move || {
            mutation_tx
                .send(mutation_store.update(expected_revision, mutation))
                .unwrap();
        });
        let changed = mutation_rx
            .recv_timeout(StdDuration::from_secs(1))
            .expect("disable/remove must not wait for project mapping preparation")
            .unwrap();
        mutation_thread.join().unwrap();
        assert!(changed.config_revision() > expected_revision);

        fs2::FileExt::unlock(&mapping_lock).unwrap();
        drop(mapping_lock);
        let commit = sync_result_rx
            .recv_timeout(StdDuration::from_secs(2))
            .expect("staged sync must terminate after the mapping lock is released");
        assert!(matches!(
            commit,
            Err(RemoteSyncError::ConfigurationChanged { ref host_id }) if host_id == "dev"
        ));
        sync_thread.join().unwrap();

        assert!(
            mappings
                .load()
                .unwrap()
                .projection()
                .resolve(&binding.source().node_id, &key)
                .is_none(),
            "a page staged under an obsolete host revision must not publish mapping state"
        );
        let ownership = HistoryOwnershipStore::new(
            state_root,
            binding.profile_id().clone(),
            RedactionProfile::Redacted,
        );
        assert_eq!(
            ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized,
            "a page staged under an obsolete host revision must not activate history"
        );
    }

    #[test]
    fn mapping_lock_does_not_block_disable_and_late_page_cannot_publish() {
        assert_config_mutation_overtakes_blocked_mapping_prepare(|| {
            RemotesConfigMutation::disable_host("dev")
        });
    }

    #[test]
    fn mapping_lock_does_not_block_remove_and_late_page_cannot_publish() {
        assert_config_mutation_overtakes_blocked_mapping_prepare(|| {
            RemotesConfigMutation::remove_host("dev")
        });
    }

    #[test]
    fn detached_source_purge_and_staged_sync_do_not_invert_config_mapping_locks() {
        let temp = TempDir::new().unwrap();
        let (store, paired, host, _) = paired_config(&temp);
        let mappings =
            ProjectMappingStore::new(temp.path().join("mapping-config/project-mappings.json"));
        let initial = mappings.load_or_create().unwrap();
        mappings
            .resolve_or_create(
                initial.revision(),
                ProjectObservation::new(SourceObservedProject::new(
                    OTHER_SOURCE.parse().unwrap(),
                    format!("opk-hmac-sha256-v1-{}", "d".repeat(64))
                        .parse()
                        .unwrap(),
                )),
            )
            .unwrap();
        let staged = mappings
            .prepare_resolve_or_create_batch(vec![ProjectObservation::new(
                SourceObservedProject::new(
                    SOURCE.parse().unwrap(),
                    format!("opk-hmac-sha256-v1-{}", "e".repeat(64))
                        .parse()
                        .unwrap(),
                ),
            )])
            .unwrap();

        let (purge_entered_tx, purge_entered_rx) = mpsc::channel();
        let (allow_purge_tx, allow_purge_rx) = mpsc::channel();
        let (purge_result_tx, purge_result_rx) = mpsc::channel();
        let purge_store = store.clone();
        let purge_mappings = mappings.clone();
        let detached_source: crate::source_identity::NodeId = OTHER_SOURCE.parse().unwrap();
        let purge_thread = thread::spawn(move || {
            let result = purge_store.with_unattached_source(&detached_source, || {
                purge_entered_tx.send(()).unwrap();
                allow_purge_rx.recv().unwrap();
                purge_mappings.purge_source_observations(&detached_source)
            });
            purge_result_tx.send(result).unwrap();
        });
        purge_entered_rx
            .recv_timeout(StdDuration::from_secs(1))
            .expect("purge must acquire the remotes fence");

        // The purge holds remotes exclusive and will later acquire mapping.
        // The staged sync must not wait for remotes while retaining mapping:
        // its exact-host fence is a try-lock and immediately abandons the
        // candidate instead.
        let started = std::time::Instant::now();
        let fence = store
            .try_with_current_host(paired.config_revision(), &host, || {
                staged.publish()?.finish()
            })
            .unwrap();
        let elapsed = started.elapsed();
        assert!(matches!(fence, TryCurrentHost::Busy));
        assert!(
            elapsed < StdDuration::from_millis(250),
            "staged sync waited on a purge fence for {elapsed:?}"
        );

        allow_purge_tx.send(()).unwrap();
        assert_eq!(
            purge_result_rx
                .recv_timeout(StdDuration::from_secs(2))
                .expect("purge must finish after staged sync abandons its candidate")
                .unwrap(),
            1
        );
        purge_thread.join().unwrap();
        assert!(mappings.load().unwrap().projection().is_empty());
    }

    #[test]
    fn exact_multi_page_range_survives_a_bounded_run_boundary() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        let mut transport = FakeTransport::new(
            guard,
            [
                FakeReply::Delta {
                    sequence: 1,
                    has_more: true,
                },
                FakeReply::Delta {
                    sequence: 2,
                    has_more: true,
                },
                FakeReply::Delta {
                    sequence: 3,
                    has_more: false,
                },
            ],
        );

        let first = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(29, 10),
            limits(2),
        )
        .unwrap();
        assert!(matches!(
            first.completion,
            RemoteSyncCompletion::Continuation(_)
        ));
        assert_eq!(first.pages_committed, 2);
        assert_eq!(first.changes_committed, 2);

        let second = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 22),
            limits(2),
        )
        .unwrap();
        assert_eq!(second.completion, RemoteSyncCompletion::Complete);
        assert_eq!(second.pages_committed, 1);
        assert_eq!(second.changes_committed, 1);
        let ranges = transport
            .requests
            .iter()
            .map(|request| match &request.request {
                RemoteExportRequestBody::Delta(delta) => delta.range.clone(),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(ranges[0], ranges[1]);
        assert_eq!(ranges[1], ranges[2]);
    }

    #[test]
    fn config_change_during_transport_rejects_response_before_commit() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        let mut transport = FakeTransport::new(
            guard,
            [FakeReply::Delta {
                sequence: 1,
                has_more: false,
            }],
        );
        let mutating_store = store.clone();
        let revision = selected.config_revision();
        transport.on_exchange = Some(Box::new(move || {
            mutating_store
                .update(
                    revision,
                    RemotesConfigMutation::edit_host(
                        "dev",
                        RemoteHostEdit {
                            ssh_host: Some("changed-alias".to_owned()),
                            agent_executable: None,
                            redact_content: None,
                        },
                    ),
                )
                .unwrap();
        }));

        let error = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            limits(1),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RemoteSyncError::ConfigurationChanged { .. }
        ));
        assert_eq!(local.commits, 0);
    }

    #[test]
    fn current_host_guard_linearizes_local_phase_against_config_updates() {
        let temp = TempDir::new().unwrap();
        let (store, _, host, selected) = paired_config(&temp);
        let guarded_store = store.clone();
        let guarded_host = host.clone();
        let revision = selected.config_revision();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let guarded = thread::spawn(move || {
            guarded_store.with_current_host(revision, &guarded_host, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        entered_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();

        let updating_store = store.clone();
        let (attempting_tx, attempting_rx) = mpsc::channel();
        let (updated_tx, updated_rx) = mpsc::channel();
        let updater = thread::spawn(move || {
            attempting_tx.send(()).unwrap();
            let result = updating_store.update(
                revision,
                RemotesConfigMutation::edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: Some("changed-alias".to_owned()),
                        agent_executable: None,
                        redact_content: None,
                    },
                ),
            );
            updated_tx.send(result).unwrap();
        });
        attempting_rx
            .recv_timeout(StdDuration::from_secs(1))
            .unwrap();
        assert!(
            updated_rx
                .recv_timeout(StdDuration::from_millis(100))
                .is_err(),
            "config update completed while the guarded local phase was active"
        );

        release_tx.send(()).unwrap();
        guarded.join().unwrap().unwrap();
        updated_rx
            .recv_timeout(StdDuration::from_secs(1))
            .unwrap()
            .unwrap();
        updater.join().unwrap();
    }

    #[test]
    fn cursor_expiry_requires_an_exact_bound_failure_envelope() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);

        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        local.bootstrap_generation = 41;
        local.position.delta_cursor = Some(DeltaCursor {
            generation: NonZeroU64::new(11).unwrap(),
            sequence: 9,
        });
        let mut transport = FakeTransport::new(guard, [FakeReply::CursorExpired]);
        let report = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            limits(1),
        )
        .unwrap();
        let RemoteSyncCompletion::BootstrapRestarted(restarted) = report.completion else {
            panic!("cursor expiry must restart bootstrap")
        };
        assert_eq!(
            restarted,
            RemoteDeltaNextRequestPosition {
                delta_cursor: None,
                exact_range: None,
                known_live_revision: None,
            }
        );
        assert_eq!(local.restarts, 1);
        assert_eq!(local.bootstrap_generation, 42);

        local.position.delta_cursor = Some(DeltaCursor {
            generation: NonZeroU64::new(11).unwrap(),
            sequence: 9,
        });
        let guard = local.guard.clone();
        let mut transport = FakeTransport::new(guard, [FakeReply::CursorExpiredFromWrongSource]);
        let error = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            limits(1),
        )
        .unwrap_err();
        assert!(matches!(error, RemoteSyncError::UnboundResponseEnvelope));
        assert_eq!(local.restarts, 1);
        assert_eq!(local.bootstrap_generation, 42);
    }

    #[test]
    fn total_response_budget_is_checked_before_local_commit() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        let mut transport = FakeTransport::new(
            guard,
            [FakeReply::Delta {
                sequence: 1,
                has_more: false,
            }],
        );
        transport.response_bytes =
            MIN_REMOTE_RESPONSE_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES + 1;
        let error = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            RemoteSyncLimits {
                max_pages: NonZeroUsize::new(1).unwrap(),
                max_response_bytes: MIN_REMOTE_RESPONSE_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES,
                exchange_timeout: StdDuration::from_secs(1),
            },
        )
        .unwrap_err();
        assert!(matches!(error, RemoteSyncError::ResponseBudgetExceeded));
        assert_eq!(local.commits, 0);
    }

    #[test]
    fn cursor_expiry_refuses_a_local_phase_that_reuses_the_expired_continuation() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        local.position = RemoteDeltaNextRequestPosition {
            delta_cursor: Some(DeltaCursor {
                generation: NonZeroU64::new(11).unwrap(),
                sequence: 9,
            }),
            exact_range: Some(ExportRange {
                from: at(1, 0),
                to: at(30, 0),
            }),
            known_live_revision: None,
        };
        local.preserve_expired_position = true;
        let mut transport = FakeTransport::new(guard, [FakeReply::CursorExpired]);

        let error = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            limits(1),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RemoteSyncError::Local(ref error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(local.restarts, 1);
    }

    #[test]
    fn cursorless_bootstrap_does_not_rotate_on_cursor_expired() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        let mut transport = FakeTransport::new(guard, [FakeReply::CursorExpired]);

        let error = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            limits(1),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RemoteSyncError::Remote(ref failure)
                if failure.kind == RemoteFailureKind::CursorExpired
        ));
        assert_eq!(local.restarts, 0);
        assert_eq!(local.bootstrap_generation, 1);
    }

    #[test]
    fn limits_and_rolling_window_have_hard_time_bounds() {
        let error = RemoteSyncLimits {
            exchange_timeout: StdDuration::from_secs(121),
            ..RemoteSyncLimits::default()
        }
        .validate()
        .unwrap_err();
        assert!(matches!(error, RemoteSyncError::InvalidLimits(_)));

        assert!(matches!(
            rolling_range(DateTime::<Utc>::MIN_UTC).unwrap_err(),
            RemoteSyncError::InvalidStartedAt
        ));
    }

    #[test]
    fn automatic_snapshot_requires_both_global_and_host_opt_in() {
        let temp = TempDir::new().unwrap();
        let (store, paired, host, _) = paired_config(&temp);
        assert!(matches!(
            RemoteSyncHostSnapshot::capture_for_automatic(&paired, &host).unwrap_err(),
            RemoteSyncError::HostNotEnabledForAutomaticSync { .. }
        ));

        let host_enabled = store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();
        let host = host_enabled.host("dev").unwrap();
        assert!(matches!(
            RemoteSyncHostSnapshot::capture_for_automatic(&host_enabled, host).unwrap_err(),
            RemoteSyncError::HostNotEnabledForAutomaticSync { .. }
        ));

        let automatic = store
            .update(
                host_enabled.config_revision(),
                RemotesConfigMutation::set_auto_sync_enabled(true),
            )
            .unwrap();
        let host = automatic.host("dev").unwrap();
        RemoteSyncHostSnapshot::capture_for_automatic(&automatic, host).unwrap();
    }

    #[test]
    fn delayed_cursor_expiry_cannot_replace_a_position_advanced_by_another_run() {
        let temp = TempDir::new().unwrap();
        let (store, _, _, selected) = paired_config(&temp);
        let guard = Rc::new(Cell::new(false));
        let mut local = FakeLocal::new(guard.clone());
        local.position.delta_cursor = Some(DeltaCursor {
            generation: NonZeroU64::new(11).unwrap(),
            sequence: 9,
        });
        local.advance_before_restart = true;
        let mut transport = FakeTransport::new(guard, [FakeReply::CursorExpired]);

        let error = sync_remote_delta_bounded(
            &store,
            &selected,
            PROFILE.parse().unwrap(),
            &mut local,
            &mut transport,
            at(30, 12),
            limits(1),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RemoteSyncError::Local(ref error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        assert_eq!(local.restarts, 0);
        assert_eq!(local.bootstrap_generation, 1);
    }

    #[test]
    fn capture_rejects_an_unpaired_explicit_host() {
        let temp = TempDir::new().unwrap();
        let store = RemotesConfigStore::new(temp.path().join("config").join("remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        let host = configured.host("dev").unwrap();
        let error = RemoteSyncHostSnapshot::capture_manual(&configured, host).unwrap_err();
        assert!(matches!(error, RemoteSyncError::HostNotPaired { .. }));
    }

    #[test]
    fn current_revision_binding_is_exact() {
        let revisions: ProtocolRevisions = current_revisions();
        let accepted = current_accepted_revisions();
        assert!(accepted.accepts(&revisions));
    }

    #[test]
    fn host_attempt_lease_is_per_host_nonblocking_and_released_on_drop() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        #[cfg(unix)]
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let state_root = fs::canonicalize(temp.path()).unwrap();
        let first = match try_acquire_remote_host_sync_lease(&state_root, "dev").unwrap() {
            TryRemoteHostSyncLease::Acquired(lease) => lease,
            TryRemoteHostSyncLease::Busy => panic!("first host lease was unexpectedly busy"),
        };
        assert!(matches!(
            try_acquire_remote_host_sync_lease(&state_root, "dev").unwrap(),
            TryRemoteHostSyncLease::Busy
        ));
        let other = match try_acquire_remote_host_sync_lease(&state_root, "staging").unwrap() {
            TryRemoteHostSyncLease::Acquired(lease) => lease,
            TryRemoteHostSyncLease::Busy => panic!("another host shared the same lease"),
        };
        drop(first);
        assert!(matches!(
            try_acquire_remote_host_sync_lease(&state_root, "dev").unwrap(),
            TryRemoteHostSyncLease::Acquired(_)
        ));
        drop(other);
    }
}
