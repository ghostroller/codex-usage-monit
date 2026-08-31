//! Cross-process ownership and cutover fencing for local history persistence.
//!
//! This module deliberately does not select a history backend or start a
//! migration. It only provides the durable state and operating-system locks a
//! future runtime cutover must use. In particular, process IDs and heartbeat
//! timestamps are diagnostic metadata; they never authorize lock stealing.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic_file::replace_file;
use crate::source_history::{HistoryProfileId, RedactionProfile};
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

pub const HISTORY_OWNERSHIP_MANIFEST_VERSION: u32 = 1;
pub const WRITER_LEASE_DIAGNOSTIC_VERSION: u32 = 1;

const OWNERSHIP_DIRECTORY: &str = "history-ownership";
const LEGACY_HISTORY_DIRECTORY: &str = "history-v1";
const WRITER_LOCK_FILE: &str = "writer.lock";
const TRANSITION_LOCK_FILE: &str = "transition.lock";
const INITIALIZATION_ANCHOR_FILE: &str = "initialization-anchor.json";
const MANIFEST_FILE: &str = "manifest.json";
const WRITER_DIAGNOSTIC_FILE: &str = "writer-diagnostic.json";
const INITIALIZATION_ANCHOR_VERSION: u32 = 1;
const MAX_INITIALIZATION_ANCHOR_FILE_BYTES: u64 = 8 * 1024;
const MAX_MANIFEST_FILE_BYTES: u64 = 16 * 1024;
const MAX_DIAGNOSTIC_FILE_BYTES: u64 = 8 * 1024;
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Durable backend-ownership phase for one profile and redaction namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryOwnershipState {
    V1Active,
    Migrating,
    V2Active,
}

/// Strict, durable ownership manifest.
///
/// Fields are intentionally private so callers can only obtain a manifest by
/// initializing or loading a validated store state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryOwnershipManifest {
    version: u32,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    epoch: u64,
    state: HistoryOwnershipState,
}

impl HistoryOwnershipManifest {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn profile_id(&self) -> &HistoryProfileId {
        &self.profile_id
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.redaction_profile
    }

    /// Fencing generation for this cutover.
    ///
    /// Beginning a migration increments the epoch; activating v2 preserves
    /// that migration epoch so migration markers and the active owner bind to
    /// one generation.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn state(&self) -> HistoryOwnershipState {
        self.state
    }

    fn initial(profile_id: HistoryProfileId, redaction_profile: RedactionProfile) -> Self {
        Self {
            version: HISTORY_OWNERSHIP_MANIFEST_VERSION,
            profile_id,
            redaction_profile,
            epoch: 1,
            state: HistoryOwnershipState::V1Active,
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != HISTORY_OWNERSHIP_MANIFEST_VERSION {
            let relation = if self.version > HISTORY_OWNERSHIP_MANIFEST_VERSION {
                "future"
            } else {
                "unsupported"
            };
            return Err(invalid_data(format!(
                "{relation} history ownership manifest version {}; expected {}",
                self.version, HISTORY_OWNERSHIP_MANIFEST_VERSION
            )));
        }
        if self.epoch == 0 {
            return Err(invalid_data(
                "history ownership manifest epoch must be non-zero",
            ));
        }
        if self.state == HistoryOwnershipState::V1Active && self.epoch != 1 {
            return Err(invalid_data(
                "v1-active history ownership must use the initial epoch",
            ));
        }
        if self.state != HistoryOwnershipState::V1Active && self.epoch == 1 {
            return Err(invalid_data(
                "migrating or v2-active history ownership requires a cutover epoch",
            ));
        }
        Ok(())
    }

    fn validate_binding(&self, store: &HistoryOwnershipStore) -> io::Result<()> {
        self.validate()?;
        if &self.profile_id != store.profile_id()
            || self.redaction_profile != store.redaction_profile()
        {
            return Err(invalid_data(
                "history ownership manifest does not match the requested profile and redaction namespace",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredHistoryOwnershipManifest {
    version: u32,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    epoch: u64,
    state: HistoryOwnershipState,
}

impl From<&HistoryOwnershipManifest> for StoredHistoryOwnershipManifest {
    fn from(manifest: &HistoryOwnershipManifest) -> Self {
        Self {
            version: manifest.version,
            profile_id: manifest.profile_id.clone(),
            redaction_profile: manifest.redaction_profile,
            epoch: manifest.epoch,
            state: manifest.state,
        }
    }
}

impl From<StoredHistoryOwnershipManifest> for HistoryOwnershipManifest {
    fn from(manifest: StoredHistoryOwnershipManifest) -> Self {
        Self {
            version: manifest.version,
            profile_id: manifest.profile_id,
            redaction_profile: manifest.redaction_profile,
            epoch: manifest.epoch,
            state: manifest.state,
        }
    }
}

/// Immutable evidence that this namespace has crossed the initialization
/// boundary at least once. Unlike the replaceable manifest, this file is
/// created exactly once and must never be recreated after loss of state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HistoryInitializationAnchor {
    version: u32,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
}

impl HistoryInitializationAnchor {
    fn initial(store: &HistoryOwnershipStore) -> Self {
        Self {
            version: INITIALIZATION_ANCHOR_VERSION,
            profile_id: store.profile_id().clone(),
            redaction_profile: store.redaction_profile(),
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != INITIALIZATION_ANCHOR_VERSION {
            let relation = if self.version > INITIALIZATION_ANCHOR_VERSION {
                "future"
            } else {
                "unsupported"
            };
            return Err(invalid_data(format!(
                "{relation} history initialization anchor version {}; expected {}",
                self.version, INITIALIZATION_ANCHOR_VERSION
            )));
        }
        Ok(())
    }

    fn validate_binding(&self, store: &HistoryOwnershipStore) -> io::Result<()> {
        self.validate()?;
        if &self.profile_id != store.profile_id()
            || self.redaction_profile != store.redaction_profile()
        {
            return Err(invalid_data(
                "history initialization anchor does not match the requested profile and redaction namespace",
            ));
        }
        Ok(())
    }
}

/// Loading never initializes state implicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnershipManifestStatus {
    Uninitialized,
    Initialized(HistoryOwnershipManifest),
}

/// Result of the explicit first publication of `V1Active`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitializeV1Outcome {
    Initialized(HistoryOwnershipManifest),
    Existing(HistoryOwnershipManifest),
}

/// Compare-and-swap result. A conflict returns the complete current state,
/// including an explicitly uninitialized store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnershipCasOutcome {
    Applied(HistoryOwnershipManifest),
    Conflict(OwnershipManifestStatus),
}

/// Non-authoritative information about the process that last acquired the
/// writer lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterLeaseDiagnostic {
    version: u32,
    process_id: u32,
    heartbeat_at: DateTime<Utc>,
}

impl WriterLeaseDiagnostic {
    pub fn new(process_id: u32, heartbeat_at: DateTime<Utc>) -> io::Result<Self> {
        let diagnostic = Self {
            version: WRITER_LEASE_DIAGNOSTIC_VERSION,
            process_id,
            heartbeat_at,
        };
        diagnostic.validate()?;
        Ok(diagnostic)
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn heartbeat_at(&self) -> DateTime<Utc> {
        self.heartbeat_at
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != WRITER_LEASE_DIAGNOSTIC_VERSION || self.process_id == 0 {
            return Err(invalid_data("writer lease diagnostic is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredWriterLeaseDiagnostic {
    version: u32,
    process_id: u32,
    heartbeat_at: DateTime<Utc>,
}

impl From<&WriterLeaseDiagnostic> for StoredWriterLeaseDiagnostic {
    fn from(diagnostic: &WriterLeaseDiagnostic) -> Self {
        Self {
            version: diagnostic.version,
            process_id: diagnostic.process_id,
            heartbeat_at: diagnostic.heartbeat_at,
        }
    }
}

impl From<StoredWriterLeaseDiagnostic> for WriterLeaseDiagnostic {
    fn from(diagnostic: StoredWriterLeaseDiagnostic) -> Self {
        Self {
            version: diagnostic.version,
            process_id: diagnostic.process_id,
            heartbeat_at: diagnostic.heartbeat_at,
        }
    }
}

/// Explicit result when the stable writer lock is already held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterLeaseBusy {
    diagnostic: Option<WriterLeaseDiagnostic>,
    diagnostic_warning: Option<String>,
}

impl WriterLeaseBusy {
    pub fn diagnostic(&self) -> Option<&WriterLeaseDiagnostic> {
        self.diagnostic.as_ref()
    }

    /// A malformed diagnostic never changes the authoritative `Busy` result.
    pub fn diagnostic_warning(&self) -> Option<&str> {
        self.diagnostic_warning.as_deref()
    }
}

/// Non-blocking writer-lease acquisition result.
#[derive(Debug)]
pub enum TryWriterLease {
    Acquired(HistoryWriterLease),
    Busy(WriterLeaseBusy),
}

/// Lifetime guard for the exclusive local history writer.
///
/// The file and its identity are private and the guard is not clonable. Drop
/// releases the operating-system lock even after an unwind or process crash.
pub struct HistoryWriterLease {
    file: File,
    lock_path: PathBuf,
    identity: StableFileIdentity,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    // A lease may move to its owning worker, but sharing it across workers
    // would allow a v2 write to race migration's final verify -> CAS window.
    // `Cell` is Send but not Sync, enforcing one in-process lease user at a
    // time without weakening the cross-process OS lock.
    _not_sync: PhantomData<Cell<()>>,
}

/// History backend authorized by a writer-lease and durable ownership epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryWriteBackend {
    V1,
    V2,
}

/// Non-forgeable, short-lived authority for one history persistence backend.
///
/// Holding the OS writer lease alone is not enough: every write must also be
/// bound to the exact durable manifest that the caller observed. Writers
/// validate this authority immediately before and after publishing data so a
/// stale epoch or backend transition fails closed.
#[derive(Debug)]
pub struct HistoryWriteAuthority<'a> {
    store: &'a HistoryOwnershipStore,
    lease: &'a HistoryWriterLease,
    expected: &'a HistoryOwnershipManifest,
    backend: HistoryWriteBackend,
}

impl HistoryWriteAuthority<'_> {
    pub fn backend(&self) -> HistoryWriteBackend {
        self.backend
    }

    pub fn expected_manifest(&self) -> &HistoryOwnershipManifest {
        self.expected
    }

    /// Revalidates the stable writer-lock handle and exact durable epoch.
    pub fn validate(&self) -> io::Result<()> {
        self.expected.validate_binding(self.store)?;
        self.store.validate_writer_lease(self.lease)?;
        match (self.backend, self.expected.state()) {
            (HistoryWriteBackend::V1, HistoryOwnershipState::V1Active)
            | (
                HistoryWriteBackend::V2,
                HistoryOwnershipState::Migrating | HistoryOwnershipState::V2Active,
            ) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "history write authority does not match the active backend phase",
                ));
            }
        }
        match self.store.load_manifest()? {
            OwnershipManifestStatus::Initialized(current) if current == *self.expected => {}
            OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "history write authority is stale for the current ownership epoch",
                ));
            }
        }
        self.store.validate_writer_lease(self.lease)
    }

    /// Checks that a v2 store is the exact profile/redaction namespace fenced
    /// by this authority. Exact path equality intentionally rejects aliases;
    /// callers should construct both stores from one normalized state root.
    pub fn validate_v2_namespace(
        &self,
        state_root: &Path,
        profile_id: &HistoryProfileId,
        redaction_profile: RedactionProfile,
    ) -> io::Result<()> {
        if self.backend != HistoryWriteBackend::V2
            || self.store.state_root() != state_root
            || self.store.profile_id() != profile_id
            || self.store.redaction_profile() != redaction_profile
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "history write authority does not belong to this v2 namespace",
            ));
        }
        self.validate()
    }

    /// Checks that a legacy v1 store is the exact profile/redaction namespace
    /// fenced by this authority.
    ///
    /// The legacy root must be the literal `history-v1` child of the
    /// ownership state root. Exact lexical equality is deliberate: accepting
    /// aliases here could place the v1 shard lock and the ownership writer
    /// lease in different coordination domains.
    pub fn validate_v1_namespace(
        &self,
        legacy_root: &Path,
        namespace: &str,
        redact_content: bool,
    ) -> io::Result<()> {
        let expected_redaction = if redact_content {
            RedactionProfile::Redacted
        } else {
            RedactionProfile::PreviewEnabled
        };
        let expected_namespace = if redact_content {
            format!("{}-redacted", self.store.profile_id())
        } else {
            self.store.profile_id().as_str().to_owned()
        };
        if self.backend != HistoryWriteBackend::V1
            || legacy_root.file_name() != Some(OsStr::new(LEGACY_HISTORY_DIRECTORY))
            || legacy_root.parent() != Some(self.store.state_root())
            || self.store.redaction_profile() != expected_redaction
            || namespace != expected_namespace
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "history write authority does not belong to this v1 namespace",
            ));
        }
        self.validate()
    }
}

impl fmt::Debug for HistoryWriterLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryWriterLease")
            .field("lock_path", &self.lock_path)
            .field("profile_id", &self.profile_id)
            .field("redaction_profile", &self.redaction_profile)
            .finish_non_exhaustive()
    }
}

impl Drop for HistoryWriterLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Filesystem binding for one local profile/redaction ownership domain.
#[derive(Clone, Debug)]
pub struct HistoryOwnershipStore {
    state_root: PathBuf,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
}

impl HistoryOwnershipStore {
    pub fn new(
        state_root: PathBuf,
        profile_id: HistoryProfileId,
        redaction_profile: RedactionProfile,
    ) -> Self {
        Self {
            state_root,
            profile_id,
            redaction_profile,
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn profile_id(&self) -> &HistoryProfileId {
        &self.profile_id
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.redaction_profile
    }

    pub fn ownership_directory(&self) -> PathBuf {
        self.state_root
            .join(OWNERSHIP_DIRECTORY)
            .join(self.profile_id.as_str())
            .join(self.redaction_profile.directory_name())
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.ownership_directory().join(MANIFEST_FILE)
    }

    pub fn initialization_anchor_path(&self) -> PathBuf {
        self.ownership_directory().join(INITIALIZATION_ANCHOR_FILE)
    }

    pub fn writer_lock_path(&self) -> PathBuf {
        self.ownership_directory().join(WRITER_LOCK_FILE)
    }

    pub fn transition_lock_path(&self) -> PathBuf {
        self.ownership_directory().join(TRANSITION_LOCK_FILE)
    }

    /// Authorizes one v1 write only while the exact initial manifest remains
    /// active. Once migration begins, old v1 writers cannot obtain or reuse a
    /// valid authority.
    pub fn authorize_v1_write<'a>(
        &'a self,
        lease: &'a HistoryWriterLease,
        expected: &'a HistoryOwnershipManifest,
    ) -> io::Result<HistoryWriteAuthority<'a>> {
        self.authorize_write(lease, expected, HistoryWriteBackend::V1)
    }

    /// Authorizes v2 migration/import writes and post-cutover v2 runtime
    /// writes. The exact manifest/epoch remains pinned for the guard lifetime.
    pub fn authorize_v2_write<'a>(
        &'a self,
        lease: &'a HistoryWriterLease,
        expected: &'a HistoryOwnershipManifest,
    ) -> io::Result<HistoryWriteAuthority<'a>> {
        self.authorize_write(lease, expected, HistoryWriteBackend::V2)
    }

    fn authorize_write<'a>(
        &'a self,
        lease: &'a HistoryWriterLease,
        expected: &'a HistoryOwnershipManifest,
        backend: HistoryWriteBackend,
    ) -> io::Result<HistoryWriteAuthority<'a>> {
        let authority = HistoryWriteAuthority {
            store: self,
            lease,
            expected,
            backend,
        };
        authority.validate()?;
        Ok(authority)
    }

    /// Loads strict durable state without creating directories or a manifest.
    pub fn load_manifest(&self) -> io::Result<OwnershipManifestStatus> {
        if !self.validate_namespace_if_present()? {
            return Ok(OwnershipManifestStatus::Uninitialized);
        }

        let anchor_path = self.initialization_anchor_path();
        let manifest_path = self.manifest_path();
        let anchor_exists = path_exists(&anchor_path)?;
        let manifest_exists = path_exists(&manifest_path)?;
        match (anchor_exists, manifest_exists) {
            (false, false) => Ok(OwnershipManifestStatus::Uninitialized),
            (true, true) => {
                read_initialization_anchor(&anchor_path, self)?;
                read_manifest(&manifest_path, self).map(OwnershipManifestStatus::Initialized)
            }
            (true, false) => {
                // Validate the anchor as well so a link, corrupt payload, or
                // namespace mismatch cannot be hidden behind the missing file.
                read_initialization_anchor(&anchor_path, self)?;
                Err(invalid_data(
                    "history ownership manifest is missing after namespace initialization; explicit repair is required",
                ))
            }
            (false, true) => Err(invalid_data(
                "history initialization anchor is missing for an existing ownership manifest; explicit repair is required",
            )),
        }
    }

    /// Attempts to become the only cooperating local history writer.
    pub fn try_acquire_writer_lease(&self) -> io::Result<TryWriterLease> {
        self.try_acquire_writer_lease_with_diagnostic(None)
    }

    /// Attempts to acquire the writer lease and, on success, publishes
    /// non-authoritative process diagnostics.
    pub fn try_acquire_writer_lease_with_diagnostic(
        &self,
        diagnostic: Option<&WriterLeaseDiagnostic>,
    ) -> io::Result<TryWriterLease> {
        if let Some(diagnostic) = diagnostic {
            diagnostic.validate()?;
        }
        let directory = self.prepare_namespace_directory()?;
        let path = directory.join(WRITER_LOCK_FILE);
        let file = open_stable_lock_file(&directory, WRITER_LOCK_FILE, "history writer lock")?;
        let identity = stable_file_identity(&file, "history writer lock")?;

        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                validate_locked_file(
                    &directory,
                    WRITER_LOCK_FILE,
                    &file,
                    identity,
                    "history writer lock",
                )?;
                self.finish_writer_lease_acquisition(&directory, path, file, identity, diagnostic)
                    .map(TryWriterLease::Acquired)
            }
            Err(error) if lock_is_contended(&error) => {
                // Returning Busy for a displaced inode would let a caller
                // trust the wrong coordination domain, so revalidate even on
                // the non-owning path.
                validate_opened_file(
                    &directory,
                    WRITER_LOCK_FILE,
                    &file,
                    identity,
                    "history writer lock",
                )?;
                let (diagnostic, diagnostic_warning) =
                    match read_diagnostic(&directory.join(WRITER_DIAGNOSTIC_FILE)) {
                        Ok(diagnostic) => (diagnostic, None),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (None, None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                Ok(TryWriterLease::Busy(WriterLeaseBusy {
                    diagnostic,
                    diagnostic_warning,
                }))
            }
            Err(error) => Err(error),
        }
    }

    /// Waits until this process becomes the only cooperating local history
    /// writer. Use this for short persistence critical sections that must not
    /// turn temporary lease contention into a dropped observation.
    pub fn acquire_writer_lease(&self) -> io::Result<HistoryWriterLease> {
        self.acquire_writer_lease_with_diagnostic(None)
    }

    /// Blocking writer-lease acquisition with optional non-authoritative
    /// process diagnostics. The stable lock path and file identity are
    /// revalidated after the wait before authority is returned.
    pub fn acquire_writer_lease_with_diagnostic(
        &self,
        diagnostic: Option<&WriterLeaseDiagnostic>,
    ) -> io::Result<HistoryWriterLease> {
        if let Some(diagnostic) = diagnostic {
            diagnostic.validate()?;
        }
        let directory = self.prepare_namespace_directory()?;
        let path = directory.join(WRITER_LOCK_FILE);
        let file = open_stable_lock_file(&directory, WRITER_LOCK_FILE, "history writer lock")?;
        let identity = stable_file_identity(&file, "history writer lock")?;
        fs2::FileExt::lock_exclusive(&file)?;
        validate_locked_file(
            &directory,
            WRITER_LOCK_FILE,
            &file,
            identity,
            "history writer lock",
        )?;
        self.finish_writer_lease_acquisition(&directory, path, file, identity, diagnostic)
    }

    fn finish_writer_lease_acquisition(
        &self,
        directory: &Path,
        path: PathBuf,
        file: File,
        identity: StableFileIdentity,
        diagnostic: Option<&WriterLeaseDiagnostic>,
    ) -> io::Result<HistoryWriterLease> {
        // A corrupt, future, or mismatched manifest refuses ownership even
        // though the caller won the OS lock. Missing state is allowed only so
        // initialize_v1_active can publish it.
        self.load_manifest()?;
        if let Some(diagnostic) = diagnostic {
            write_diagnostic_atomically(&directory.join(WRITER_DIAGNOSTIC_FILE), diagnostic)?;
        } else {
            remove_stale_diagnostic(directory)?;
        }
        validate_locked_file(
            directory,
            WRITER_LOCK_FILE,
            &file,
            identity,
            "history writer lock",
        )?;
        Ok(HistoryWriterLease {
            file,
            lock_path: path,
            identity,
            profile_id: self.profile_id.clone(),
            redaction_profile: self.redaction_profile,
            _not_sync: PhantomData,
        })
    }

    /// Revalidates that this guard still names the store's stable writer-lock
    /// inode. Runtime writers should call this immediately before and after a
    /// lease-protected persistence operation.
    pub fn validate_writer_lease(&self, lease: &HistoryWriterLease) -> io::Result<()> {
        if lease.lock_path != self.writer_lock_path()
            || lease.profile_id != self.profile_id
            || lease.redaction_profile != self.redaction_profile
        {
            return Err(invalid_data(
                "writer lease does not belong to this history ownership namespace",
            ));
        }
        let directory = self.prepare_namespace_directory()?;
        validate_locked_file(
            &directory,
            WRITER_LOCK_FILE,
            &lease.file,
            lease.identity,
            "history writer lock",
        )
    }

    /// Refreshes diagnostics while preserving the OS lock as the sole source
    /// of writer authority.
    pub fn update_writer_diagnostic(
        &self,
        lease: &HistoryWriterLease,
        diagnostic: &WriterLeaseDiagnostic,
    ) -> io::Result<()> {
        self.validate_writer_lease(lease)?;
        diagnostic.validate()?;
        write_diagnostic_atomically(
            &self.ownership_directory().join(WRITER_DIAGNOSTIC_FILE),
            diagnostic,
        )?;
        self.validate_writer_lease(lease)
    }

    /// Explicitly publishes the first `V1Active` manifest.
    ///
    /// This method acquires only the transition lock; the caller must already
    /// hold the writer lease.
    pub fn initialize_v1_active(
        &self,
        lease: &HistoryWriterLease,
    ) -> io::Result<InitializeV1Outcome> {
        self.validate_writer_lease(lease)?;
        let transition = self.lock_transition()?;
        self.validate_writer_lease(lease)?;
        let outcome = match self.load_manifest()? {
            OwnershipManifestStatus::Initialized(existing) => {
                InitializeV1Outcome::Existing(existing)
            }
            OwnershipManifestStatus::Uninitialized => {
                let anchor = HistoryInitializationAnchor::initial(self);
                if !write_initialization_anchor_once(&self.initialization_anchor_path(), &anchor)? {
                    return Err(invalid_data(
                        "history initialization anchor appeared during initialization; refusing to recreate ownership state",
                    ));
                }
                let published_anchor =
                    read_initialization_anchor(&self.initialization_anchor_path(), self)?;
                if published_anchor != anchor {
                    return Err(invalid_data(
                        "history initialization anchor changed during initialization",
                    ));
                }

                let manifest = HistoryOwnershipManifest::initial(
                    self.profile_id.clone(),
                    self.redaction_profile,
                );
                write_manifest_atomically(&self.manifest_path(), &manifest)?;
                let OwnershipManifestStatus::Initialized(published) = self.load_manifest()? else {
                    return Err(invalid_data(
                        "history ownership state disappeared during initialization",
                    ));
                };
                if published != manifest {
                    return Err(invalid_data(
                        "history ownership manifest changed during initialization",
                    ));
                }
                InitializeV1Outcome::Initialized(published)
            }
        };
        drop(transition);
        self.validate_writer_lease(lease)?;
        Ok(outcome)
    }

    /// Begins a cutover with compare-and-swap semantics. Activating v2 is
    /// deliberately not exposed here because it requires epoch-bound,
    /// lease-protected migration verification; use the local migration
    /// activation API for that final transition.
    pub fn begin_migration(
        &self,
        lease: &HistoryWriterLease,
        expected_v1: &HistoryOwnershipManifest,
    ) -> io::Result<OwnershipCasOutcome> {
        if expected_v1.state() != HistoryOwnershipState::V1Active {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "beginning migration requires a v1-active ownership manifest",
            ));
        }
        self.compare_and_transition(lease, expected_v1, HistoryOwnershipState::Migrating)
    }

    /// Internal state-machine primitive. `Migrating -> V2Active` must only be
    /// called by the migration module while holding its verified frozen-v1
    /// activation critical section.
    pub(crate) fn compare_and_transition(
        &self,
        lease: &HistoryWriterLease,
        expected: &HistoryOwnershipManifest,
        next_state: HistoryOwnershipState,
    ) -> io::Result<OwnershipCasOutcome> {
        expected.validate_binding(self)?;
        self.validate_writer_lease(lease)?;
        let transition = self.lock_transition()?;
        self.validate_writer_lease(lease)?;

        let current = self.load_manifest()?;
        if current != OwnershipManifestStatus::Initialized(expected.clone()) {
            drop(transition);
            self.validate_writer_lease(lease)?;
            return Ok(OwnershipCasOutcome::Conflict(current));
        }

        let next_epoch = match (expected.state, next_state) {
            (HistoryOwnershipState::V1Active, HistoryOwnershipState::Migrating) => expected
                .epoch
                .checked_add(1)
                .ok_or_else(|| invalid_data("history ownership epoch overflowed"))?,
            (HistoryOwnershipState::Migrating, HistoryOwnershipState::V2Active) => expected.epoch,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "illegal history ownership transition from {:?} to {:?}",
                        expected.state, next_state
                    ),
                ));
            }
        };
        let next = HistoryOwnershipManifest {
            version: HISTORY_OWNERSHIP_MANIFEST_VERSION,
            profile_id: self.profile_id.clone(),
            redaction_profile: self.redaction_profile,
            epoch: next_epoch,
            state: next_state,
        };
        next.validate_binding(self)?;
        write_manifest_atomically(&self.manifest_path(), &next)?;
        let published = read_manifest(&self.manifest_path(), self)?;
        if published != next {
            return Err(invalid_data(
                "history ownership manifest changed during transition",
            ));
        }
        drop(transition);
        self.validate_writer_lease(lease)?;
        Ok(OwnershipCasOutcome::Applied(published))
    }

    fn lock_transition(&self) -> io::Result<LockedFile> {
        let directory = self.prepare_namespace_directory()?;
        let file =
            open_stable_lock_file(&directory, TRANSITION_LOCK_FILE, "history transition lock")?;
        let identity = stable_file_identity(&file, "history transition lock")?;
        fs2::FileExt::lock_exclusive(&file)?;
        validate_locked_file(
            &directory,
            TRANSITION_LOCK_FILE,
            &file,
            identity,
            "history transition lock",
        )?;
        Ok(LockedFile { file })
    }

    fn prepare_namespace_directory(&self) -> io::Result<PathBuf> {
        create_private_state_root(&self.state_root)?;
        let ownership = create_private_child_directory(&self.state_root, OWNERSHIP_DIRECTORY)?;
        let profile = create_private_child_directory(&ownership, self.profile_id.as_str())?;
        create_private_child_directory(&profile, self.redaction_profile.directory_name())
    }

    fn validate_namespace_if_present(&self) -> io::Result<bool> {
        let paths = [
            self.state_root.clone(),
            self.state_root.join(OWNERSHIP_DIRECTORY),
            self.state_root
                .join(OWNERSHIP_DIRECTORY)
                .join(self.profile_id.as_str()),
            self.ownership_directory(),
        ];
        for (index, path) in paths.into_iter().enumerate() {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if index == 0 => validate_private_state_root(&metadata, &path)?,
                Ok(metadata) => validate_private_directory_metadata(&metadata, &path)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }
}

struct LockedFile {
    file: File,
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StableFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
}

fn path_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_initialization_anchor(
    path: &Path,
    store: &HistoryOwnershipStore,
) -> io::Result<HistoryInitializationAnchor> {
    let contents = read_private_bounded(
        path,
        MAX_INITIALIZATION_ANCHOR_FILE_BYTES,
        "history initialization anchor",
    )?;
    let anchor: HistoryInitializationAnchor = serde_json::from_slice(&contents)
        .map_err(|error| invalid_data(format!("invalid history initialization anchor: {error}")))?;
    anchor.validate_binding(store)?;
    Ok(anchor)
}

fn write_initialization_anchor_once(
    path: &Path,
    anchor: &HistoryInitializationAnchor,
) -> io::Result<bool> {
    anchor.validate()?;
    let mut contents = serde_json::to_vec_pretty(anchor)
        .map_err(|error| invalid_data(format!("invalid history initialization anchor: {error}")))?;
    contents.push(b'\n');
    write_private_once_atomically(path, &contents, "history initialization anchor")
}

fn read_manifest(
    path: &Path,
    store: &HistoryOwnershipStore,
) -> io::Result<HistoryOwnershipManifest> {
    let contents =
        read_private_bounded(path, MAX_MANIFEST_FILE_BYTES, "history ownership manifest")?;
    let stored: StoredHistoryOwnershipManifest = serde_json::from_slice(&contents)
        .map_err(|error| invalid_data(format!("invalid history ownership manifest: {error}")))?;
    let manifest = HistoryOwnershipManifest::from(stored);
    manifest.validate_binding(store)?;
    Ok(manifest)
}

fn write_manifest_atomically(path: &Path, manifest: &HistoryOwnershipManifest) -> io::Result<()> {
    manifest.validate()?;
    let mut contents = serde_json::to_vec_pretty(&StoredHistoryOwnershipManifest::from(manifest))
        .map_err(|error| {
        invalid_data(format!("invalid history ownership manifest: {error}"))
    })?;
    contents.push(b'\n');
    write_private_atomically(path, &contents, "history ownership manifest")
}

fn read_diagnostic(path: &Path) -> io::Result<Option<WriterLeaseDiagnostic>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let contents =
        read_private_bounded(path, MAX_DIAGNOSTIC_FILE_BYTES, "history writer diagnostic")?;
    let stored: StoredWriterLeaseDiagnostic = serde_json::from_slice(&contents)
        .map_err(|error| invalid_data(format!("invalid history writer diagnostic: {error}")))?;
    let diagnostic = WriterLeaseDiagnostic::from(stored);
    diagnostic.validate()?;
    Ok(Some(diagnostic))
}

fn write_diagnostic_atomically(path: &Path, diagnostic: &WriterLeaseDiagnostic) -> io::Result<()> {
    diagnostic.validate()?;
    let mut contents = serde_json::to_vec_pretty(&StoredWriterLeaseDiagnostic::from(diagnostic))
        .map_err(|error| invalid_data(format!("invalid history writer diagnostic: {error}")))?;
    contents.push(b'\n');
    write_private_atomically(path, &contents, "history writer diagnostic")
}

fn remove_stale_diagnostic(directory: &Path) -> io::Result<()> {
    let path = directory.join(WRITER_DIAGNOSTIC_FILE);
    match fs::remove_file(&path) {
        Ok(()) => sync_directory(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_private_bounded(path: &Path, maximum: u64, subject: &str) -> io::Result<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    let file = open_nofollow(path, false, false, false, subject)?;
    let metadata = file.metadata()?;
    validate_private_file_metadata(&metadata, subject)?;
    ensure_opened_file_matches_path(path, &file, false, subject)?;
    if metadata.len() > maximum {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(maximum + 1).read_to_end(&mut contents)?;
    if contents.len() as u64 > maximum {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    Ok(contents)
}

fn write_private_atomically(path: &Path, contents: &[u8], subject: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    validate_private_directory(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, subject)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("state"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_published_private_file(path, subject)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Publishes fully-synced contents without ever replacing an existing path.
/// A hard link gives both Unix and Windows an atomic no-clobber publication
/// step while keeping the temporary file in the same private directory.
fn write_private_once_atomically(path: &Path, contents: &[u8], subject: &str) -> io::Result<bool> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    validate_private_directory(parent)?;
    match fs::symlink_metadata(path) {
        Ok(_) => {
            validate_published_private_file(path, subject)?;
            return Ok(false);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("state"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                validate_published_private_file(path, subject)?;
                sync_directory(parent)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_published_private_file(path, subject)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return result;
    }
    fs::remove_file(&temporary)?;
    sync_directory(parent)?;
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
            Ok(file) => {
                let validation = (|| {
                    let metadata = file.metadata()?;
                    validate_private_file_metadata(&metadata, "history ownership temporary file")?;
                    #[cfg(windows)]
                    validate_windows_private_file(
                        &temporary,
                        &file,
                        "history ownership temporary file",
                    )?;
                    Ok(())
                })();
                match validation {
                    Ok(()) => return Ok((temporary, file)),
                    Err(error) => {
                        drop(file);
                        let _ = fs::remove_file(&temporary);
                        return Err(error);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique history ownership temporary file",
    ))
}

fn open_stable_lock_file(directory: &Path, name: &str, subject: &str) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, subject)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = open_nofollow(&path, true, true, true, subject)?;
    let metadata = file.metadata()?;
    validate_private_file_metadata(&metadata, subject)?;
    ensure_opened_file_matches_path(&path, &file, true, subject)?;
    Ok(file)
}

fn open_nofollow(
    path: &Path,
    write: bool,
    create: bool,
    stable_lock: bool,
    subject: &str,
) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(write).create(create);
    #[cfg(unix)]
    if create {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    if stable_lock {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        // Stable lock files must not be renamed or deleted while any lease
        // handle is alive. Manifest/diagnostic readers intentionally retain
        // Rust's default delete sharing so atomic replacement stays possible.
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    #[cfg(not(windows))]
    let _ = stable_lock;
    add_nofollow_flags(&mut options);
    options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))
}

fn validate_opened_file(
    directory: &Path,
    name: &str,
    file: &File,
    expected_identity: StableFileIdentity,
    subject: &str,
) -> io::Result<()> {
    validate_private_directory(directory)?;
    let metadata = file.metadata()?;
    validate_private_file_metadata(&metadata, subject)?;
    if stable_file_identity(file, subject)? != expected_identity {
        return Err(invalid_data(format!("{subject} file identity changed")));
    }
    let path = directory.join(name);
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    ensure_opened_file_matches_path(&path, file, true, subject)
}

fn validate_locked_file(
    directory: &Path,
    name: &str,
    file: &File,
    expected_identity: StableFileIdentity,
    subject: &str,
) -> io::Result<()> {
    // Lock ownership is represented by the private guard. This function's
    // job is to fence displaced lock inodes before protected work proceeds.
    validate_opened_file(directory, name, file, expected_identity, subject)
}

fn ensure_opened_file_matches_path(
    path: &Path,
    opened: &File,
    stable_lock: bool,
    subject: &str,
) -> io::Result<()> {
    #[cfg(windows)]
    validate_windows_private_file(path, opened, subject)?;
    let current = open_nofollow(path, false, false, stable_lock, subject)?;
    let metadata = current.metadata()?;
    validate_private_file_metadata(&metadata, subject)?;
    #[cfg(windows)]
    validate_windows_private_file(path, &current, subject)?;
    if stable_file_identity(&current, subject)? == stable_file_identity(opened, subject)? {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(unix)]
fn stable_file_identity(file: &File, _subject: &str) -> io::Result<StableFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(StableFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn stable_file_identity(file: &File, subject: &str) -> io::Result<StableFileIdentity> {
    let (volume_serial_number, file_id) = windows_file_identity(file, subject)?;
    Ok(StableFileIdentity {
        volume_serial_number,
        file_id,
    })
}

#[cfg(windows)]
fn windows_file_identity(file: &File, subject: &str) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut information = FILE_ID_INFO::default();
    // SAFETY: the live file owns the handle for this call and `information`
    // points to writable storage of the size supplied to the API.
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
    require_stable_windows_file_identity(
        Some(information.VolumeSerialNumber),
        Some(information.FileId.Identifier),
        subject,
    )
}

#[cfg(any(windows, test))]
fn require_stable_windows_file_identity(
    volume_serial_number: Option<u64>,
    file_id: Option<[u8; 16]>,
    subject: &str,
) -> io::Result<(u64, [u8; 16])> {
    match (volume_serial_number, file_id) {
        (Some(volume), Some(file_id)) => Ok((volume, file_id)),
        _ => Err(invalid_data(format!(
            "{subject} does not expose a stable Windows file identity"
        ))),
    }
}

#[cfg(not(any(unix, windows)))]
fn stable_file_identity(_file: &File, subject: &str) -> io::Result<StableFileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{subject} locking requires stable file identity support"),
    ))
}

fn lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
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

fn map_nofollow_error(error: io::Error, subject: &str) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_data(format!("{subject} must not be a symbolic link"));
    }
    #[cfg(not(unix))]
    let _ = subject;
    error
}

fn create_private_state_root(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => return validate_private_state_root(&metadata, path),
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
        reject_windows_reparse_components_before_create(path, "history ownership state root")?;
        fs::create_dir_all(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    validate_private_state_root(&metadata, path)
}

fn create_private_child_directory(parent: &Path, name: &str) -> io::Result<PathBuf> {
    validate_private_directory(parent)?;
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_private_directory_metadata(&metadata, &path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            let create_result = {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(&path)
            };
            #[cfg(not(unix))]
            let create_result = fs::create_dir(&path);
            match create_result {
                Ok(()) => {}
                // Two first-use callers can both observe the missing child.
                // The winner's directory is acceptable only after the loser
                // re-reads and validates the published filesystem object;
                // never treat EEXIST alone as proof of a safe directory.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&path)?;
                    validate_private_directory_metadata(&metadata, &path)?;
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    validate_private_directory(parent)?;
    validate_private_directory(&path)?;
    Ok(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_private_directory_metadata(&metadata, path)
}

fn validate_private_directory_metadata(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_data(format!(
            "history ownership directory {} must not be a symbolic link or reparse point",
            path.display()
        )));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_data(format!(
            "history ownership path {} must be a directory",
            path.display()
        )));
    }
    ensure_private_path(metadata, "history ownership directory")?;
    #[cfg(windows)]
    validate_windows_private_directory(path, "history ownership directory")?;
    Ok(())
}

fn validate_private_state_root(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    validate_private_directory_metadata(metadata, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "history ownership state root {} must have mode 0700",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn validate_published_private_file(path: &Path, subject: &str) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    let file = open_nofollow(path, false, false, false, subject)?;
    let metadata = file.metadata()?;
    validate_private_file_metadata(&metadata, subject)?;
    ensure_opened_file_matches_path(path, &file, false, subject)
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
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
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
    // Windows std metadata does not expose a portable DACL check. The layout
    // still rejects reparse points and inherits the state root's per-user ACL.
    Ok(())
}

#[cfg(windows)]
fn reject_windows_reparse_components_before_create(path: &Path, subject: &str) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                return Err(invalid_data(format!(
                    "{subject} path must not traverse a reparse point ({})",
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // atomic_file uses MOVEFILE_WRITE_THROUGH on Windows.
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration as StdDuration, Instant};

    use chrono::Duration;
    use tempfile::tempdir;

    const PROFILE: &str = "0123456789abcdef";
    const CHILD_STATE_ROOT_ENV: &str = "CODEX_USAGE_MONIT_OWNERSHIP_TEST_STATE_ROOT";
    const CHILD_READY_FILE_ENV: &str = "CODEX_USAGE_MONIT_OWNERSHIP_TEST_READY_FILE";

    fn store(root: &Path) -> HistoryOwnershipStore {
        HistoryOwnershipStore::new(
            root.join("state"),
            PROFILE.parse().unwrap(),
            RedactionProfile::PreviewEnabled,
        )
    }

    fn acquire(store: &HistoryOwnershipStore) -> HistoryWriterLease {
        match store.try_acquire_writer_lease().unwrap() {
            TryWriterLease::Acquired(lease) => lease,
            TryWriterLease::Busy(_) => panic!("writer lease unexpectedly busy"),
        }
    }

    fn initialize(
        store: &HistoryOwnershipStore,
        lease: &HistoryWriterLease,
    ) -> HistoryOwnershipManifest {
        match store.initialize_v1_active(lease).unwrap() {
            InitializeV1Outcome::Initialized(manifest)
            | InitializeV1Outcome::Existing(manifest) => manifest,
        }
    }

    #[test]
    fn missing_manifest_requires_explicit_v1_initialization() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        assert_eq!(
            store.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
        assert!(!store.state_root().exists());
        assert!(!store.initialization_anchor_path().exists());

        let lease = acquire(&store);
        assert_eq!(
            store.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
        let manifest = initialize(&store, &lease);
        assert_eq!(manifest.version(), HISTORY_OWNERSHIP_MANIFEST_VERSION);
        assert_eq!(manifest.profile_id().as_str(), PROFILE);
        assert_eq!(
            manifest.redaction_profile(),
            RedactionProfile::PreviewEnabled
        );
        assert_eq!(manifest.epoch(), 1);
        assert_eq!(manifest.state(), HistoryOwnershipState::V1Active);
        assert_eq!(
            read_initialization_anchor(&store.initialization_anchor_path(), &store).unwrap(),
            HistoryInitializationAnchor::initial(&store)
        );
        assert_eq!(
            store.initialize_v1_active(&lease).unwrap(),
            InitializeV1Outcome::Existing(manifest)
        );
    }

    #[test]
    fn deleting_manifest_after_initialization_fails_closed() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        let initialized = initialize(&store, &lease);
        assert_eq!(initialized.epoch(), 1);

        fs::remove_file(store.manifest_path()).unwrap();
        sync_directory(&store.ownership_directory()).unwrap();

        let error = store.load_manifest().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("explicit repair"));
        let error = store.initialize_v1_active(&lease).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("explicit repair"));

        drop(lease);
        assert!(store.try_acquire_writer_lease().is_err());
        assert!(!store.manifest_path().exists());
        assert!(store.initialization_anchor_path().exists());
    }

    #[test]
    fn anchor_only_initialization_crash_fails_closed() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        let transition = store.lock_transition().unwrap();
        let anchor = HistoryInitializationAnchor::initial(&store);
        assert!(
            write_initialization_anchor_once(&store.initialization_anchor_path(), &anchor).unwrap()
        );
        drop(transition);
        drop(lease);

        assert!(!store.manifest_path().exists());
        assert_eq!(
            read_initialization_anchor(&store.initialization_anchor_path(), &store).unwrap(),
            anchor
        );
        let error = store.load_manifest().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("explicit repair"));
        assert!(store.try_acquire_writer_lease().is_err());
    }

    #[test]
    fn write_authority_is_backend_and_epoch_bound() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        let v1 = initialize(&store, &lease);

        let v1_authority = store.authorize_v1_write(&lease, &v1).unwrap();
        assert_eq!(v1_authority.backend(), HistoryWriteBackend::V1);
        v1_authority.validate().unwrap();
        v1_authority
            .validate_v1_namespace(
                &store.state_root().join(LEGACY_HISTORY_DIRECTORY),
                PROFILE,
                false,
            )
            .unwrap();
        assert!(
            v1_authority
                .validate_v1_namespace(
                    &store.state_root().join("history-v1-alias"),
                    PROFILE,
                    false,
                )
                .is_err()
        );
        assert!(
            v1_authority
                .validate_v1_namespace(
                    &store.state_root().join(LEGACY_HISTORY_DIRECTORY),
                    "different-profile",
                    false,
                )
                .is_err()
        );
        assert!(
            v1_authority
                .validate_v1_namespace(
                    &store.state_root().join(LEGACY_HISTORY_DIRECTORY),
                    PROFILE,
                    true,
                )
                .is_err()
        );
        assert!(store.authorize_v2_write(&lease, &v1).is_err());

        let migrating = match store.begin_migration(&lease, &v1).unwrap() {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected migration conflict"),
        };
        assert_eq!(
            v1_authority.validate().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(store.authorize_v1_write(&lease, &migrating).is_err());

        let v2_authority = store.authorize_v2_write(&lease, &migrating).unwrap();
        v2_authority
            .validate_v2_namespace(
                store.state_root(),
                store.profile_id(),
                RedactionProfile::PreviewEnabled,
            )
            .unwrap();
        assert!(
            v2_authority
                .validate_v2_namespace(
                    &store.state_root().join("alias"),
                    store.profile_id(),
                    RedactionProfile::PreviewEnabled,
                )
                .is_err()
        );

        let active = match store
            .compare_and_transition(&lease, &migrating, HistoryOwnershipState::V2Active)
            .unwrap()
        {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected activation conflict"),
        };
        assert_eq!(
            v2_authority.validate().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        store
            .authorize_v2_write(&lease, &active)
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn redacted_v1_authority_requires_the_legacy_namespace_suffix() {
        let directory = tempdir().unwrap();
        let store = HistoryOwnershipStore::new(
            directory.path().join("state"),
            PROFILE.parse().unwrap(),
            RedactionProfile::Redacted,
        );
        let lease = acquire(&store);
        let manifest = initialize(&store, &lease);
        let authority = store.authorize_v1_write(&lease, &manifest).unwrap();
        let legacy_root = store.state_root().join(LEGACY_HISTORY_DIRECTORY);

        authority
            .validate_v1_namespace(&legacy_root, &format!("{PROFILE}-redacted"), true)
            .unwrap();
        assert!(
            authority
                .validate_v1_namespace(&legacy_root, PROFILE, true)
                .is_err()
        );
        assert!(
            authority
                .validate_v1_namespace(&legacy_root, &format!("{PROFILE}-redacted"), false)
                .is_err()
        );
    }

    #[test]
    fn only_one_writer_lease_is_granted_and_drop_releases_it() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let first = acquire(&store);
        assert!(matches!(
            store.try_acquire_writer_lease().unwrap(),
            TryWriterLease::Busy(_)
        ));
        drop(first);
        assert!(matches!(
            store.try_acquire_writer_lease().unwrap(),
            TryWriterLease::Acquired(_)
        ));
    }

    #[test]
    fn blocking_writer_lease_waits_then_serializes_the_next_writer() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let first = acquire(&store);
        let waiting_store = store.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _lease = waiting_store.acquire_writer_lease().unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();
        assert!(
            acquired_rx
                .recv_timeout(StdDuration::from_millis(50))
                .is_err()
        );
        drop(first);
        acquired_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn killed_child_process_releases_writer_lease() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("child-state");
        let ready_file = directory.path().join("child-ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "history_ownership::tests::writer_lease_child_process_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_STATE_ROOT_ENV, &state_root)
            .env(CHILD_READY_FILE_ENV, &ready_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + StdDuration::from_secs(10);
        while !ready_file.is_file() {
            assert!(
                Instant::now() < deadline,
                "child did not acquire the writer lease in time"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before holding the writer lease"
            );
            thread::sleep(StdDuration::from_millis(20));
        }

        let store = HistoryOwnershipStore::new(
            state_root,
            PROFILE.parse().unwrap(),
            RedactionProfile::PreviewEnabled,
        );
        assert!(matches!(
            store.try_acquire_writer_lease().unwrap(),
            TryWriterLease::Busy(_)
        ));

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(matches!(
            store.try_acquire_writer_lease().unwrap(),
            TryWriterLease::Acquired(_)
        ));
    }

    #[test]
    fn writer_lease_child_process_helper() {
        let Some(state_root) = std::env::var_os(CHILD_STATE_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let ready_file = PathBuf::from(std::env::var_os(CHILD_READY_FILE_ENV).unwrap());
        let store = HistoryOwnershipStore::new(
            state_root,
            PROFILE.parse().unwrap(),
            RedactionProfile::PreviewEnabled,
        );
        let _lease = acquire(&store);
        fs::write(ready_file, b"ready\n").unwrap();
        loop {
            thread::sleep(StdDuration::from_secs(1));
        }
    }

    #[test]
    fn stale_pid_and_heartbeat_never_steal_a_live_os_lock() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let stale = WriterLeaseDiagnostic::new(7, Utc::now() - Duration::days(365)).unwrap();
        let first = match store
            .try_acquire_writer_lease_with_diagnostic(Some(&stale))
            .unwrap()
        {
            TryWriterLease::Acquired(lease) => lease,
            TryWriterLease::Busy(_) => panic!("writer lease unexpectedly busy"),
        };

        let TryWriterLease::Busy(busy) = store.try_acquire_writer_lease().unwrap() else {
            panic!("stale diagnostics must not permit lock stealing");
        };
        assert_eq!(busy.diagnostic(), Some(&stale));
        assert!(busy.diagnostic_warning().is_none());

        drop(first);
        assert!(matches!(
            store.try_acquire_writer_lease().unwrap(),
            TryWriterLease::Acquired(_)
        ));
    }

    #[test]
    fn acquiring_without_diagnostics_clears_the_previous_owner_record() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let previous = WriterLeaseDiagnostic::new(7, Utc::now()).unwrap();
        let first = match store
            .try_acquire_writer_lease_with_diagnostic(Some(&previous))
            .unwrap()
        {
            TryWriterLease::Acquired(lease) => lease,
            TryWriterLease::Busy(_) => panic!("writer lease unexpectedly busy"),
        };
        drop(first);

        let second = acquire(&store);
        assert!(
            !store
                .ownership_directory()
                .join(WRITER_DIAGNOSTIC_FILE)
                .exists()
        );
        let TryWriterLease::Busy(busy) = store.try_acquire_writer_lease().unwrap() else {
            panic!("writer lease unexpectedly available");
        };
        assert!(busy.diagnostic().is_none());
        assert!(busy.diagnostic_warning().is_none());
        drop(second);
    }

    #[test]
    fn legal_state_machine_preserves_one_cutover_epoch_and_forbids_rollback() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        let v1 = initialize(&store, &lease);
        let initialization_anchor =
            read_initialization_anchor(&store.initialization_anchor_path(), &store).unwrap();

        let migrating = match store.begin_migration(&lease, &v1).unwrap() {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected CAS conflict"),
        };
        assert_eq!(migrating.epoch(), 2);
        assert_eq!(migrating.state(), HistoryOwnershipState::Migrating);

        let v2 = match store
            .compare_and_transition(&lease, &migrating, HistoryOwnershipState::V2Active)
            .unwrap()
        {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected CAS conflict"),
        };
        assert_eq!(v2.epoch(), migrating.epoch());
        assert_eq!(v2.state(), HistoryOwnershipState::V2Active);

        for forbidden in [
            HistoryOwnershipState::V1Active,
            HistoryOwnershipState::Migrating,
            HistoryOwnershipState::V2Active,
        ] {
            let error = store
                .compare_and_transition(&lease, &v2, forbidden)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(
            store.load_manifest().unwrap(),
            OwnershipManifestStatus::Initialized(v2)
        );
        assert_eq!(
            read_initialization_anchor(&store.initialization_anchor_path(), &store).unwrap(),
            initialization_anchor
        );
    }

    #[test]
    fn stale_compare_and_swap_cannot_repeat_a_transition() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        let v1 = initialize(&store, &lease);

        let first = store.begin_migration(&lease, &v1).unwrap();
        assert!(matches!(first, OwnershipCasOutcome::Applied(_)));
        let stale = store.begin_migration(&lease, &v1).unwrap();
        assert!(matches!(stale, OwnershipCasOutcome::Conflict(_)));
    }

    #[test]
    fn corrupt_future_and_binding_mismatched_manifests_fail_closed() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        let manifest = initialize(&store, &lease);
        drop(lease);
        let path = store.manifest_path();

        write_private_test_file(&path, b"{ definitely not json\n");
        assert_eq!(
            store.load_manifest().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(store.try_acquire_writer_lease().is_err());

        write_private_test_file(
            &path,
            format!(
                "{{\"version\":{},\"profileId\":\"{}\",\"redactionProfile\":\"preview-enabled\",\"epoch\":1,\"state\":\"v1_active\"}}",
                HISTORY_OWNERSHIP_MANIFEST_VERSION + 1,
                PROFILE
            )
            .as_bytes(),
        );
        let error = store.load_manifest().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("future"));

        write_private_test_file(
            &path,
            br#"{"version":1,"profileId":"different","redactionProfile":"preview-enabled","epoch":1,"state":"v1_active"}"#,
        );
        assert_eq!(
            store.load_manifest().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        write_manifest_atomically(&path, &manifest).unwrap();
        assert_eq!(
            store.load_manifest().unwrap(),
            OwnershipManifestStatus::Initialized(manifest)
        );
    }

    #[cfg(unix)]
    #[test]
    fn displaced_writer_lock_is_rejected_before_transition() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        let v1 = initialize(&store, &lease);
        let lock_path = store.writer_lock_path();
        fs::rename(
            &lock_path,
            store.ownership_directory().join("displaced-writer.lock"),
        )
        .unwrap();
        write_private_test_file(&lock_path, b"");

        let error = store.validate_writer_lease(&lease).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed"));
        assert!(store.begin_migration(&lease, &v1).is_err());
        assert_eq!(
            store.load_manifest().unwrap(),
            OwnershipManifestStatus::Initialized(v1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_namespace_files_and_permissive_permissions_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let lease = acquire(&store);
        initialize(&store, &lease);
        drop(lease);

        let manifest = store.manifest_path();
        let target = store.ownership_directory().join("manifest-target");
        fs::rename(&manifest, &target).unwrap();
        symlink(&target, &manifest).unwrap();
        assert_eq!(
            store.load_manifest().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        fs::remove_file(&manifest).unwrap();
        fs::rename(&target, &manifest).unwrap();
        let mut permissions = fs::metadata(&manifest).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&manifest, permissions).unwrap();
        assert_eq!(
            store.load_manifest().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn windows_identity_helper_requires_both_stable_components() {
        let file_id = [11_u8; 16];
        assert_eq!(
            require_stable_windows_file_identity(Some(7), Some(file_id), "test lock").unwrap(),
            (7, file_id)
        );
        for (volume, id) in [(None, Some(file_id)), (Some(7), None), (None, None)] {
            let error = require_stable_windows_file_identity(volume, id, "test lock").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("stable Windows file identity"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn ownership_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let diagnostic = WriterLeaseDiagnostic::new(42, Utc::now()).unwrap();
        let lease = match store
            .try_acquire_writer_lease_with_diagnostic(Some(&diagnostic))
            .unwrap()
        {
            TryWriterLease::Acquired(lease) => lease,
            TryWriterLease::Busy(_) => panic!("writer lease unexpectedly busy"),
        };
        initialize(&store, &lease);

        for path in [
            store.state_root().to_path_buf(),
            store.state_root().join(OWNERSHIP_DIRECTORY),
            store.state_root().join(OWNERSHIP_DIRECTORY).join(PROFILE),
            store.ownership_directory(),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for path in [
            store.writer_lock_path(),
            store.transition_lock_path(),
            store.initialization_anchor_path(),
            store.manifest_path(),
            store.ownership_directory().join(WRITER_DIAGNOSTIC_FILE),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    fn write_private_test_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(path, permissions).unwrap();
        }
    }
}
