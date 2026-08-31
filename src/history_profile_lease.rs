//! Process-lifetime selection of one history redaction profile.
//!
//! A profile ID gets one stable operating-system lock and one replaceable,
//! durable active-profile marker. Cooperating processes may hold shared leases
//! for the same redaction profile concurrently. Changing the active redaction
//! profile requires the exclusive lock, so it cannot complete until every
//! shared lease has been dropped.
//!
//! All acquisition APIs are deliberately non-blocking. A caller that wants to
//! switch profiles retries after [`TryHistoryProfileLease::Busy`]. This
//! protocol only fences versions that participate in it; callers must retain
//! their recorder-status check when cutting over a pre-v0.4 process.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_file::replace_file;
use crate::source_history::{HistoryProfileId, RedactionProfile};
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

pub const HISTORY_PROFILE_LEASE_MARKER_VERSION: u32 = 1;
pub const HISTORY_PROFILE_LEASE_LOCK_VERSION: u32 = 1;

const LEASES_DIRECTORY: &str = "history-profile-leases";
const PROFILE_LOCK_FILE: &str = "profile.lock";
const ACTIVE_PROFILE_FILE: &str = "active-profile.json";
const MAX_MARKER_BYTES: u64 = 4 * 1024;
const LOCK_SLOT_BYTES: usize = 1024;
const LOCK_SLOT_COUNT: usize = 2;
const LOCK_FILE_BYTES: usize = LOCK_SLOT_BYTES * LOCK_SLOT_COUNT;
const LOCK_SLOT_LENGTH_BYTES: usize = 4;
const LOCK_SLOT_MAGIC: &str = "codex-usage-monit/history-profile-lease-lock";
const LOCK_CHECKSUM_DOMAIN: &[u8] = b"codex-usage-monit/history-profile-lease-lock/v1\0";
const SHA256_HEX_BYTES: usize = 64;
const LOCK_INITIALIZING_GENERATION: u64 = 1;
const LOCK_INITIALIZED_GENERATION: u64 = 2;
const TEMP_FILE_ATTEMPTS: usize = 128;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static LOCAL_LEASES: OnceLock<Mutex<HashMap<StableFileIdentity, LocalLeaseState>>> =
    OnceLock::new();

/// The durable selection protected by a history profile lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveHistoryProfile {
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
}

impl ActiveHistoryProfile {
    pub fn profile_id(&self) -> &HistoryProfileId {
        &self.profile_id
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.redaction_profile
    }
}

/// Result of one non-blocking history profile lease attempt.
#[must_use = "dropping an acquired result immediately releases the history profile lease"]
pub enum TryHistoryProfileLease {
    Acquired(HistoryProfileLeaseGuard),
    /// Another cooperating holder or profile transition currently owns the
    /// coordination domain. `active_profile` is `None` only during first-use
    /// publication, before an active marker exists.
    Busy {
        active_profile: Option<ActiveHistoryProfile>,
    },
}

impl fmt::Debug for TryHistoryProfileLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acquired(guard) => formatter.debug_tuple("Acquired").field(guard).finish(),
            Self::Busy { active_profile } => formatter
                .debug_struct("Busy")
                .field("active_profile", active_profile)
                .finish(),
        }
    }
}

/// Process-lifetime shared lease for one exact profile/redaction selection.
///
/// Its fields are private so callers cannot unlock the file behind the local
/// bookkeeping. Dropping the guard releases the operating-system shared lock.
#[must_use = "the guard must remain alive while this history profile is in use"]
pub struct HistoryProfileLeaseGuard {
    state_root: PathBuf,
    directory: PathBuf,
    lock_path: PathBuf,
    file: File,
    identity: StableFileIdentity,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
}

impl HistoryProfileLeaseGuard {
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn profile_id(&self) -> &HistoryProfileId {
        &self.profile_id
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.redaction_profile
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Revalidates the canonical private directory, stable lock identity,
    /// process-local shared ownership, and exact durable marker binding.
    pub fn validate(&self) -> io::Result<()> {
        validate_canonical_state_root(&self.state_root)?;
        validate_profile_directory(&self.state_root, &self.profile_id, &self.directory)?;
        validate_opened_lock(&self.directory, &self.file, self.identity)?;

        if read_lock_initialization(&self.file, &self.profile_id)?
            != LockInitialization::Initialized
        {
            return Err(invalid_data(
                "history profile stable lock is not durably initialized",
            ));
        }

        let leases = local_leases();
        let Some(state) = leases.get(&self.identity) else {
            return Err(invalid_data(
                "history profile lease is absent from process-local ownership state",
            ));
        };
        if state.shared_count == 0 || state.shared_profile != Some(self.redaction_profile) {
            return Err(invalid_data(
                "history profile lease does not match process-local ownership state",
            ));
        }
        drop(leases);

        let marker = read_active_profile(&self.directory, &self.profile_id)?.ok_or_else(|| {
            invalid_data("active history profile marker is missing after initialization")
        })?;
        if marker.redaction_profile != self.redaction_profile {
            return Err(invalid_data(
                "active history profile changed while a shared lease was held",
            ));
        }
        validate_opened_lock(&self.directory, &self.file, self.identity)
    }
}

impl fmt::Debug for HistoryProfileLeaseGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryProfileLeaseGuard")
            .field("state_root", &self.state_root)
            .field("profile_id", &self.profile_id)
            .field("redaction_profile", &self.redaction_profile)
            .finish_non_exhaustive()
    }
}

impl Drop for HistoryProfileLeaseGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
        release_local_shared(self.identity, self.redaction_profile);
    }
}

/// Attempts to hold the exact profile/redaction selection without waiting.
///
/// `state_root` must already be an absolute canonical private directory. The
/// profile-scoped coordination directories are created with mode 0700 when
/// missing; files are created with mode 0600.
pub fn try_acquire_history_profile_lease(
    state_root: &Path,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
) -> io::Result<TryHistoryProfileLease> {
    let state_root = validate_canonical_state_root(state_root)?;
    let directory = prepare_profile_directory(&state_root, &profile_id)?;
    let file = open_stable_lock(&directory)?;
    let identity = stable_file_identity(&file)?;

    if let Some(active_profile) = local_conflict(identity, &profile_id, redaction_profile) {
        validate_opened_lock(&directory, &file, identity)?;
        let active_profile = match active_profile {
            Some(active_profile) => Some(active_profile),
            None => read_active_profile(&directory, &profile_id)?,
        };
        return Ok(TryHistoryProfileLease::Busy { active_profile });
    }

    match fs2::FileExt::try_lock_shared(&file) {
        Ok(()) => {
            validate_opened_lock(&directory, &file, identity)?;
            let initialization = read_lock_initialization(&file, &profile_id)?;
            let active = read_active_profile(&directory, &profile_id)?;
            match (initialization, active) {
                (LockInitialization::Initialized, Some(active))
                    if active.redaction_profile == redaction_profile =>
                {
                    return finish_shared_acquisition(
                        state_root,
                        directory,
                        file,
                        identity,
                        profile_id,
                        redaction_profile,
                    );
                }
                (LockInitialization::Initialized, None) => {
                    fs2::FileExt::unlock(&file)?;
                    return Err(invalid_data(
                        "active history profile marker is missing after completed initialization; explicit repair is required",
                    ));
                }
                (LockInitialization::Uninitialized, Some(_)) => {
                    fs2::FileExt::unlock(&file)?;
                    return Err(invalid_data(
                        "active history profile marker exists without a stable lock initialization record",
                    ));
                }
                _ => fs2::FileExt::unlock(&file)?,
            }
        }
        Err(error) if lock_is_contended(&error) => {
            validate_opened_lock(&directory, &file, identity)?;
            return busy_result(&directory, &profile_id);
        }
        Err(error) => return Err(error),
    }

    try_switch_profile(
        state_root,
        directory,
        file,
        identity,
        profile_id,
        redaction_profile,
    )
}

fn try_switch_profile(
    state_root: PathBuf,
    directory: PathBuf,
    file: File,
    identity: StableFileIdentity,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
) -> io::Result<TryHistoryProfileLease> {
    let Some(mut transition) = LocalTransition::begin(identity) else {
        validate_opened_lock(&directory, &file, identity)?;
        return busy_result(&directory, &profile_id);
    };

    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(error) if lock_is_contended(&error) => {
            validate_opened_lock(&directory, &file, identity)?;
            return busy_result(&directory, &profile_id);
        }
        Err(error) => return Err(error),
    }

    let exclusive_result = (|| {
        validate_opened_lock(&directory, &file, identity)?;
        let initialization = read_lock_initialization(&file, &profile_id)?;
        let mut active = read_active_profile(&directory, &profile_id)?;

        match initialization {
            LockInitialization::Uninitialized => {
                if active.is_some() {
                    return Err(invalid_data(
                        "active history profile marker exists without a stable lock initialization record",
                    ));
                }
                write_lock_slot(
                    &file,
                    &StoredLockSlot::new(
                        profile_id.clone(),
                        LOCK_INITIALIZING_GENERATION,
                        LockState::Initializing,
                    )?,
                )?;
                write_active_profile(&directory, &profile_id, redaction_profile)?;
                active = Some(active_profile(profile_id.clone(), redaction_profile));
                write_lock_slot(
                    &file,
                    &StoredLockSlot::new(
                        profile_id.clone(),
                        LOCK_INITIALIZED_GENERATION,
                        LockState::Initialized,
                    )?,
                )?;
            }
            LockInitialization::Initializing => {
                if active.is_none() {
                    write_active_profile(&directory, &profile_id, redaction_profile)?;
                    active = Some(active_profile(profile_id.clone(), redaction_profile));
                }
                write_lock_slot(
                    &file,
                    &StoredLockSlot::new(
                        profile_id.clone(),
                        LOCK_INITIALIZED_GENERATION,
                        LockState::Initialized,
                    )?,
                )?;
            }
            LockInitialization::Initialized => {
                if active.is_none() {
                    return Err(invalid_data(
                        "active history profile marker is missing after completed initialization; explicit repair is required",
                    ));
                }
            }
        }

        if active.as_ref().map(ActiveHistoryProfile::redaction_profile) != Some(redaction_profile) {
            write_active_profile(&directory, &profile_id, redaction_profile)?;
        }
        let published = read_active_profile(&directory, &profile_id)?.ok_or_else(|| {
            invalid_data("active history profile marker was not durably published")
        })?;
        if published.redaction_profile != redaction_profile {
            return Err(invalid_data(
                "active history profile marker does not match the requested redaction profile",
            ));
        }
        if read_lock_initialization(&file, &profile_id)? != LockInitialization::Initialized {
            return Err(invalid_data(
                "history profile stable lock was not durably sealed after marker publication",
            ));
        }
        validate_opened_lock(&directory, &file, identity)
    })();
    if let Err(error) = exclusive_result {
        let _ = fs2::FileExt::unlock(&file);
        return Err(error);
    }
    fs2::FileExt::unlock(&file)?;

    match fs2::FileExt::try_lock_shared(&file) {
        Ok(()) => {}
        Err(error) if lock_is_contended(&error) => {
            validate_opened_lock(&directory, &file, identity)?;
            return busy_result(&directory, &profile_id);
        }
        Err(error) => return Err(error),
    }

    let shared_result = (|| {
        validate_opened_lock(&directory, &file, identity)?;
        if read_lock_initialization(&file, &profile_id)? != LockInitialization::Initialized {
            return Err(invalid_data(
                "history profile stable lock changed after profile selection",
            ));
        }
        let active = read_active_profile(&directory, &profile_id)?.ok_or_else(|| {
            invalid_data("active history profile marker disappeared during profile selection")
        })?;
        if active.redaction_profile != redaction_profile {
            return Ok(Some(active));
        }
        validate_opened_lock(&directory, &file, identity)?;
        Ok(None)
    })();
    match shared_result {
        Ok(None) => {
            transition.finish_with_shared(redaction_profile)?;
            Ok(TryHistoryProfileLease::Acquired(HistoryProfileLeaseGuard {
                state_root,
                lock_path: directory.join(PROFILE_LOCK_FILE),
                directory,
                file,
                identity,
                profile_id,
                redaction_profile,
            }))
        }
        Ok(Some(active_profile)) => {
            fs2::FileExt::unlock(&file)?;
            Ok(TryHistoryProfileLease::Busy {
                active_profile: Some(active_profile),
            })
        }
        Err(error) => {
            let _ = fs2::FileExt::unlock(&file);
            Err(error)
        }
    }
}

fn finish_shared_acquisition(
    state_root: PathBuf,
    directory: PathBuf,
    file: File,
    identity: StableFileIdentity,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
) -> io::Result<TryHistoryProfileLease> {
    {
        let mut leases = local_leases();
        let state = leases.entry(identity).or_default();
        if state.transition
            || (state.shared_count != 0 && state.shared_profile != Some(redaction_profile))
        {
            let active_profile = state
                .shared_profile
                .map(|active| active_profile(profile_id.clone(), active));
            drop(leases);
            fs2::FileExt::unlock(&file)?;
            return Ok(TryHistoryProfileLease::Busy { active_profile });
        }
        let next_count = state
            .shared_count
            .checked_add(1)
            .ok_or_else(|| invalid_data("process-local history profile lease count overflowed"))?;
        state.shared_profile = Some(redaction_profile);
        state.shared_count = next_count;
    }

    let guard = HistoryProfileLeaseGuard {
        state_root,
        lock_path: directory.join(PROFILE_LOCK_FILE),
        directory,
        file,
        identity,
        profile_id,
        redaction_profile,
    };
    if let Err(error) = guard.validate() {
        drop(guard);
        return Err(error);
    }
    Ok(TryHistoryProfileLease::Acquired(guard))
}

fn busy_result(
    directory: &Path,
    profile_id: &HistoryProfileId,
) -> io::Result<TryHistoryProfileLease> {
    Ok(TryHistoryProfileLease::Busy {
        active_profile: read_active_profile(directory, profile_id)?,
    })
}

fn active_profile(
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
) -> ActiveHistoryProfile {
    ActiveHistoryProfile {
        profile_id,
        redaction_profile,
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct LocalLeaseState {
    shared_profile: Option<RedactionProfile>,
    shared_count: usize,
    transition: bool,
}

fn local_lease_map() -> &'static Mutex<HashMap<StableFileIdentity, LocalLeaseState>> {
    LOCAL_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn local_leases() -> MutexGuard<'static, HashMap<StableFileIdentity, LocalLeaseState>> {
    local_lease_map()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn local_conflict(
    identity: StableFileIdentity,
    profile_id: &HistoryProfileId,
    requested: RedactionProfile,
) -> Option<Option<ActiveHistoryProfile>> {
    let leases = local_leases();
    let state = leases.get(&identity)?;
    if state.transition {
        return Some(
            state
                .shared_profile
                .map(|active| active_profile(profile_id.clone(), active)),
        );
    }
    if state.shared_count != 0 && state.shared_profile != Some(requested) {
        return Some(
            state
                .shared_profile
                .map(|active| active_profile(profile_id.clone(), active)),
        );
    }
    None
}

fn release_local_shared(identity: StableFileIdentity, profile: RedactionProfile) {
    let mut leases = local_leases();
    let mut remove = false;
    if let Some(state) = leases.get_mut(&identity) {
        if state.shared_profile == Some(profile) && state.shared_count != 0 {
            state.shared_count -= 1;
            if state.shared_count == 0 {
                state.shared_profile = None;
            }
        }
        remove = state.shared_count == 0 && !state.transition;
    }
    if remove {
        leases.remove(&identity);
    }
}

struct LocalTransition {
    identity: StableFileIdentity,
    finished: bool,
}

impl LocalTransition {
    fn begin(identity: StableFileIdentity) -> Option<Self> {
        let mut leases = local_leases();
        let state = leases.entry(identity).or_default();
        if state.transition || state.shared_count != 0 {
            return None;
        }
        state.transition = true;
        Some(Self {
            identity,
            finished: false,
        })
    }

    fn finish_with_shared(&mut self, redaction_profile: RedactionProfile) -> io::Result<()> {
        let mut leases = local_leases();
        let state = leases.get_mut(&self.identity).ok_or_else(|| {
            invalid_data("history profile transition lost process-local ownership state")
        })?;
        if !state.transition || state.shared_count != 0 || state.shared_profile.is_some() {
            return Err(invalid_data(
                "history profile transition process-local state is inconsistent",
            ));
        }
        state.transition = false;
        state.shared_profile = Some(redaction_profile);
        state.shared_count = 1;
        self.finished = true;
        Ok(())
    }
}

impl Drop for LocalTransition {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut leases = local_leases();
        let mut remove = false;
        if let Some(state) = leases.get_mut(&self.identity) {
            state.transition = false;
            remove = state.shared_count == 0;
        }
        if remove {
            leases.remove(&self.identity);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockInitialization {
    Uninitialized,
    Initializing,
    Initialized,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LockState {
    Initializing,
    Initialized,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredLockSlot {
    magic: String,
    schema_version: u32,
    generation: u64,
    history_profile_id: HistoryProfileId,
    state: LockState,
    checksum: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LockChecksumPayload<'a> {
    magic: &'a str,
    schema_version: u32,
    generation: u64,
    history_profile_id: &'a HistoryProfileId,
    state: LockState,
}

impl StoredLockSlot {
    fn new(
        history_profile_id: HistoryProfileId,
        generation: u64,
        state: LockState,
    ) -> io::Result<Self> {
        let mut stored = Self {
            magic: LOCK_SLOT_MAGIC.to_owned(),
            schema_version: HISTORY_PROFILE_LEASE_LOCK_VERSION,
            generation,
            history_profile_id,
            state,
            checksum: String::new(),
        };
        stored.checksum = stored.expected_checksum()?;
        Ok(stored)
    }

    fn expected_checksum(&self) -> io::Result<String> {
        let payload = LockChecksumPayload {
            magic: &self.magic,
            schema_version: self.schema_version,
            generation: self.generation,
            history_profile_id: &self.history_profile_id,
            state: self.state,
        };
        let encoded = serde_json::to_vec(&payload).map_err(|error| {
            invalid_data(format!(
                "could not encode history profile stable lock checksum payload: {error}"
            ))
        })?;
        let mut hasher = Sha256::new();
        hasher.update(LOCK_CHECKSUM_DOMAIN);
        hasher.update(encoded);
        Ok(lower_hex(&hasher.finalize()))
    }

    fn validate(&self, expected_profile_id: &HistoryProfileId) -> io::Result<()> {
        if self.magic != LOCK_SLOT_MAGIC {
            return Err(invalid_data(
                "history profile stable lock has an invalid format binding",
            ));
        }
        if self.schema_version != HISTORY_PROFILE_LEASE_LOCK_VERSION {
            let relation = if self.schema_version > HISTORY_PROFILE_LEASE_LOCK_VERSION {
                "future"
            } else {
                "unsupported"
            };
            return Err(invalid_data(format!(
                "history profile stable lock uses {relation} schema version {}; expected {}",
                self.schema_version, HISTORY_PROFILE_LEASE_LOCK_VERSION
            )));
        }
        if &self.history_profile_id != expected_profile_id {
            return Err(invalid_data(
                "history profile stable lock does not match its profile-scoped directory",
            ));
        }
        if self.checksum.len() != SHA256_HEX_BYTES
            || !self
                .checksum
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.checksum != self.expected_checksum()?
        {
            return Err(invalid_data(
                "history profile stable lock checksum does not match its contents",
            ));
        }
        Ok(())
    }
}

enum DecodedLockSlot {
    EmptyOrTorn,
    Valid(StoredLockSlot),
}

fn read_lock_initialization(
    file: &File,
    expected_profile_id: &HistoryProfileId,
) -> io::Result<LockInitialization> {
    let length = usize::try_from(file.metadata()?.len())
        .map_err(|_| invalid_data("history profile stable lock is too large"))?;
    if length > LOCK_FILE_BYTES {
        return Err(invalid_data("history profile stable lock is too large"));
    }
    if length == 0 {
        return Ok(LockInitialization::Uninitialized);
    }

    let mut contents = vec![0_u8; length];
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    reader.read_exact(&mut contents)?;

    let first = decode_lock_slot(&contents[..length.min(LOCK_SLOT_BYTES)])?;
    let first = match first {
        DecodedLockSlot::Valid(slot) => slot,
        DecodedLockSlot::EmptyOrTorn if length <= LOCK_SLOT_BYTES => {
            return Ok(LockInitialization::Uninitialized);
        }
        DecodedLockSlot::EmptyOrTorn => {
            return Err(invalid_data(
                "history profile stable lock initialization record is corrupt",
            ));
        }
    };
    first.validate(expected_profile_id)?;
    if first.generation != LOCK_INITIALIZING_GENERATION || first.state != LockState::Initializing {
        return Err(invalid_data(
            "history profile stable lock has an invalid initializing record",
        ));
    }

    if length <= LOCK_SLOT_BYTES {
        return Ok(LockInitialization::Initializing);
    }
    match decode_lock_slot(&contents[LOCK_SLOT_BYTES..])? {
        DecodedLockSlot::EmptyOrTorn => Ok(LockInitialization::Initializing),
        DecodedLockSlot::Valid(second) => {
            second.validate(expected_profile_id)?;
            if second.generation != LOCK_INITIALIZED_GENERATION
                || second.state != LockState::Initialized
            {
                return Err(invalid_data(
                    "history profile stable lock has an invalid initialized record",
                ));
            }
            Ok(LockInitialization::Initialized)
        }
    }
}

fn decode_lock_slot(contents: &[u8]) -> io::Result<DecodedLockSlot> {
    if contents.is_empty() || contents.iter().all(|byte| *byte == 0) {
        return Ok(DecodedLockSlot::EmptyOrTorn);
    }
    if contents.len() < LOCK_SLOT_LENGTH_BYTES {
        return Ok(DecodedLockSlot::EmptyOrTorn);
    }
    let encoded_length = u32::from_le_bytes(
        contents[..LOCK_SLOT_LENGTH_BYTES]
            .try_into()
            .expect("lock slot length prefix is exactly four bytes"),
    ) as usize;
    if encoded_length == 0 || encoded_length > LOCK_SLOT_BYTES - LOCK_SLOT_LENGTH_BYTES {
        return Ok(DecodedLockSlot::EmptyOrTorn);
    }
    let encoded_end = LOCK_SLOT_LENGTH_BYTES + encoded_length;
    if contents.len() < LOCK_SLOT_BYTES || encoded_end > contents.len() {
        return Ok(DecodedLockSlot::EmptyOrTorn);
    }
    if contents[encoded_end..LOCK_SLOT_BYTES]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(invalid_data(
            "history profile stable lock slot has nonzero trailing data",
        ));
    }

    let value = match serde_json::from_slice::<serde_json::Value>(
        &contents[LOCK_SLOT_LENGTH_BYTES..encoded_end],
    ) {
        Ok(value) => value,
        Err(_) => return Ok(DecodedLockSlot::EmptyOrTorn),
    };
    if value
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| version > u64::from(HISTORY_PROFILE_LEASE_LOCK_VERSION))
    {
        return Err(invalid_data(
            "history profile stable lock uses a future schema version",
        ));
    }
    let stored = serde_json::from_value::<StoredLockSlot>(value).map_err(|error| {
        invalid_data(format!(
            "could not decode history profile stable lock slot: {error}"
        ))
    })?;
    Ok(DecodedLockSlot::Valid(stored))
}

fn write_lock_slot(file: &File, stored: &StoredLockSlot) -> io::Result<()> {
    let slot_index = match (stored.generation, stored.state) {
        (LOCK_INITIALIZING_GENERATION, LockState::Initializing) => 0,
        (LOCK_INITIALIZED_GENERATION, LockState::Initialized) => 1,
        _ => {
            return Err(invalid_data(
                "history profile stable lock write requested an invalid state transition",
            ));
        }
    };
    let encoded = serde_json::to_vec(stored).map_err(|error| {
        invalid_data(format!(
            "could not encode history profile stable lock slot: {error}"
        ))
    })?;
    let encoded_length = u32::try_from(encoded.len())
        .map_err(|_| invalid_data("history profile stable lock slot is too large"))?;
    if encoded.len() > LOCK_SLOT_BYTES - LOCK_SLOT_LENGTH_BYTES {
        return Err(invalid_data(
            "history profile stable lock slot is too large",
        ));
    }

    let mut slot = [0_u8; LOCK_SLOT_BYTES];
    slot[..LOCK_SLOT_LENGTH_BYTES].copy_from_slice(&encoded_length.to_le_bytes());
    slot[LOCK_SLOT_LENGTH_BYTES..LOCK_SLOT_LENGTH_BYTES + encoded.len()].copy_from_slice(&encoded);
    let mut writer = file.try_clone()?;
    writer.seek(SeekFrom::Start((slot_index * LOCK_SLOT_BYTES) as u64))?;
    writer.write_all(&slot)?;
    writer.sync_all()
}

fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredActiveHistoryProfile {
    schema_version: u32,
    history_profile_id: HistoryProfileId,
    active_redaction_profile: RedactionProfile,
}

impl StoredActiveHistoryProfile {
    fn new(profile_id: HistoryProfileId, redaction_profile: RedactionProfile) -> Self {
        Self {
            schema_version: HISTORY_PROFILE_LEASE_MARKER_VERSION,
            history_profile_id: profile_id,
            active_redaction_profile: redaction_profile,
        }
    }

    fn validate_binding(&self, expected: &HistoryProfileId) -> io::Result<()> {
        if self.schema_version != HISTORY_PROFILE_LEASE_MARKER_VERSION {
            let relation = if self.schema_version > HISTORY_PROFILE_LEASE_MARKER_VERSION {
                "future"
            } else {
                "unsupported"
            };
            return Err(invalid_data(format!(
                "active history profile marker uses {relation} schema version {}; expected {}",
                self.schema_version, HISTORY_PROFILE_LEASE_MARKER_VERSION
            )));
        }
        if &self.history_profile_id != expected {
            return Err(invalid_data(
                "active history profile marker does not match its profile-scoped directory",
            ));
        }
        Ok(())
    }

    fn into_active(self) -> ActiveHistoryProfile {
        active_profile(self.history_profile_id, self.active_redaction_profile)
    }
}

fn read_active_profile(
    directory: &Path,
    expected_profile_id: &HistoryProfileId,
) -> io::Result<Option<ActiveHistoryProfile>> {
    validate_private_directory(directory, "history profile lease directory")?;
    let path = directory.join(ACTIVE_PROFILE_FILE);
    let path_metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_private_file_metadata(&path_metadata, "active history profile marker")?;
    let mut file = open_nofollow(&path, false, false)?;
    validate_private_file_metadata(&file.metadata()?, "active history profile marker")?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        stable_file_identity(&file)?,
        false,
        "active history profile marker",
    )?;
    let length = file.metadata()?.len();
    if length > MAX_MARKER_BYTES {
        return Err(invalid_data("active history profile marker is too large"));
    }
    let mut contents = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_MARKER_BYTES {
        return Err(invalid_data("active history profile marker is too large"));
    }
    let stored =
        serde_json::from_slice::<StoredActiveHistoryProfile>(&contents).map_err(|error| {
            invalid_data(format!(
                "could not decode active history profile marker: {error}"
            ))
        })?;
    stored.validate_binding(expected_profile_id)?;
    Ok(Some(stored.into_active()))
}

fn write_active_profile(
    directory: &Path,
    profile_id: &HistoryProfileId,
    redaction_profile: RedactionProfile,
) -> io::Result<()> {
    validate_private_directory(directory, "history profile lease directory")?;
    let path = directory.join(ACTIVE_PROFILE_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, "active history profile marker")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let marker = StoredActiveHistoryProfile::new(profile_id.clone(), redaction_profile);
    let mut contents = serde_json::to_vec_pretty(&marker).map_err(|error| {
        invalid_data(format!(
            "could not encode active history profile marker: {error}"
        ))
    })?;
    contents.push(b'\n');
    if contents.len() as u64 > MAX_MARKER_BYTES {
        return Err(invalid_data("active history profile marker is too large"));
    }

    let (temporary_path, mut temporary) = create_temporary_file(directory)?;
    let result = (|| {
        temporary.write_all(&contents)?;
        temporary.sync_all()?;
        drop(temporary);
        replace_file(&temporary_path, &path)?;
        validate_published_marker(&path)?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn validate_published_marker(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&metadata, "active history profile marker")?;
    let file = open_nofollow(path, false, false)?;
    validate_private_file_metadata(&file.metadata()?, "active history profile marker")?;
    ensure_opened_file_matches_path(
        path,
        &file,
        stable_file_identity(&file)?,
        false,
        "active history profile marker",
    )
}

fn create_temporary_file(directory: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            ".{ACTIVE_PROFILE_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        configure_private_open(&mut options, false);
        match options.open(&path) {
            Ok(file) => {
                if let Err(error) = validate_private_file_metadata(
                    &file.metadata()?,
                    "active history profile temporary file",
                ) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                #[cfg(windows)]
                validate_windows_private_file(
                    &path,
                    &file,
                    "active history profile temporary file",
                )?;
                // On Windows this additionally rejects a hard-link alias via
                // FILE_STANDARD_INFO; on Unix the metadata check above has
                // already enforced nlink == 1.
                let _ = stable_file_identity(&file)?;
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate an active history profile temporary file",
    ))
}

fn validate_canonical_state_root(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history profile lease state root must be absolute and canonical",
        ));
    }
    let canonical = fs::canonicalize(path)?;
    if canonical != path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "history profile lease state root {} is not canonical (expected {})",
                path.display(),
                canonical.display()
            ),
        ));
    }
    validate_private_directory(&canonical, "history profile lease state root")?;
    Ok(canonical)
}

fn prepare_profile_directory(
    state_root: &Path,
    profile_id: &HistoryProfileId,
) -> io::Result<PathBuf> {
    validate_canonical_state_root(state_root)?;
    let leases = create_private_child(state_root, LEASES_DIRECTORY)?;
    let profile = create_private_child(&leases, profile_id.as_str())?;
    validate_profile_directory(state_root, profile_id, &profile)?;
    Ok(profile)
}

fn validate_profile_directory(
    state_root: &Path,
    profile_id: &HistoryProfileId,
    directory: &Path,
) -> io::Result<()> {
    validate_canonical_state_root(state_root)?;
    let leases = state_root.join(LEASES_DIRECTORY);
    validate_private_directory(&leases, "history profile leases directory")?;
    let expected = leases.join(profile_id.as_str());
    if directory != expected {
        return Err(invalid_data(
            "history profile lease directory does not match its profile ID",
        ));
    }
    validate_private_directory(directory, "history profile lease directory")
}

fn create_private_child(parent: &Path, name: &str) -> io::Result<PathBuf> {
    validate_private_directory(parent, "history profile lease parent directory")?;
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_private_directory_metadata(
            &path,
            &metadata,
            "history profile lease directory",
        )?,
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
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    validate_private_directory(&path, "history profile lease directory")?;
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    validate_private_directory(parent, "history profile lease parent directory")?;
    validate_private_directory(&path, "history profile lease directory")?;
    Ok(path)
}

fn open_stable_lock(directory: &Path) -> io::Result<File> {
    validate_private_directory(directory, "history profile lease directory")?;
    let path = directory.join(PROFILE_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, "history profile stable lock")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut create_options = OpenOptions::new();
    create_options.read(true).write(true).create_new(true);
    configure_private_open(&mut create_options, true);
    let file = match create_options.open(&path) {
        Ok(file) => {
            sync_directory(directory)?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            configure_private_open(&mut options, true);
            options.open(&path).map_err(map_nofollow_error)?
        }
        Err(error) => return Err(map_nofollow_error(error)),
    };

    validate_private_file_metadata(&file.metadata()?, "history profile stable lock")?;
    #[cfg(windows)]
    validate_windows_private_file(&path, &file, "history profile stable lock")?;
    let identity = stable_file_identity(&file)?;
    validate_opened_lock(directory, &file, identity)?;
    Ok(file)
}

fn validate_opened_lock(
    directory: &Path,
    opened: &File,
    expected_identity: StableFileIdentity,
) -> io::Result<()> {
    validate_private_directory(directory, "history profile lease directory")?;
    validate_private_file_metadata(&opened.metadata()?, "history profile stable lock")?;
    if stable_file_identity(opened)? != expected_identity {
        return Err(invalid_data("history profile stable lock identity changed"));
    }
    let path = directory.join(PROFILE_LOCK_FILE);
    ensure_opened_file_matches_path(
        &path,
        opened,
        expected_identity,
        true,
        "history profile stable lock",
    )
}

fn ensure_opened_file_matches_path(
    path: &Path,
    opened: &File,
    expected_identity: StableFileIdentity,
    stable_lock: bool,
    subject: &str,
) -> io::Result<()> {
    validate_private_file_metadata(&opened.metadata()?, subject)?;
    let path_metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    let current = open_nofollow(path, false, stable_lock)?;
    validate_private_file_metadata(&current.metadata()?, subject)?;
    #[cfg(windows)]
    {
        validate_windows_private_file(path, opened, subject)?;
        validate_windows_private_file(path, &current, subject)?;
    }
    if stable_file_identity(&current)? != expected_identity {
        return Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )));
    }
    Ok(())
}

fn open_nofollow(path: &Path, write: bool, stable_lock: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    configure_private_open(&mut options, stable_lock);
    options.open(path).map_err(map_nofollow_error)
}

fn configure_private_open(options: &mut OpenOptions, stable_lock: bool) {
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
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        if stable_lock {
            // Never grant delete sharing for the stable coordination inode.
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
    }
    #[cfg(not(windows))]
    let _ = stable_lock;
}

fn validate_private_directory(path: &Path, subject: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_private_directory_metadata(path, &metadata, subject)
}

fn validate_private_directory_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_data(format!(
            "{subject} {} must not be a symbolic link or reparse point",
            path.display()
        )));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_data(format!(
            "{subject} {} must be a directory",
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
                format!("{subject} must be owned by the current user"),
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{subject} {} must have mode 0700", path.display()),
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_private_directory(path, subject)?;
    Ok(())
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
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{subject} must have mode 0600"),
            ));
        }
        if metadata.nlink() != 1 {
            return Err(invalid_data(format!(
                "{subject} must not have hard-link aliases"
            )));
        }
    }
    #[cfg(windows)]
    validate_windows_link_count(metadata, subject)?;
    Ok(())
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

#[cfg(windows)]
fn validate_windows_link_count(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    // The handle-based FILE_STANDARD_INFO check is performed by
    // `stable_file_identity`/`validate_windows_private_file` after open. Path
    // metadata alone does not expose a Windows hard-link count.
    Ok(())
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
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

#[cfg(unix)]
fn stable_file_identity(file: &File) -> io::Result<StableFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(StableFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn stable_file_identity(file: &File) -> io::Result<StableFileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FILE_STANDARD_INFO, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx,
    };

    let mut id = FILE_ID_INFO::default();
    // SAFETY: `file` owns a live handle and `id` is exact writable storage.
    let id_success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if id_success == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut standard = FILE_STANDARD_INFO::default();
    // SAFETY: `file` owns a live handle and `standard` is exact writable storage.
    let standard_success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&mut standard as *mut FILE_STANDARD_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_STANDARD_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if standard_success == 0 {
        return Err(io::Error::last_os_error());
    }
    if standard.NumberOfLinks != 1 {
        return Err(invalid_data(
            "history profile lease file must not have hard-link aliases",
        ));
    }

    Ok(StableFileIdentity {
        volume_serial_number: id.VolumeSerialNumber,
        file_id: id.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
fn stable_file_identity(_file: &File) -> io::Result<StableFileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "history profile leases require stable file identity support",
    ))
}

fn lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
}

fn map_nofollow_error(error: io::Error) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_data("history profile lease path must not be a symbolic link");
    }
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

#[cfg(test)]
fn stable_lock_share_mode_for_test() -> u32 {
    0x0000_0001 | 0x0000_0002
}

#[cfg(test)]
fn windows_attributes_are_reparse_for_test(attributes: u32, reparse_flag: u32) -> bool {
    attributes & reparse_flag != 0
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::*;

    const PROFILE: &str = "0123456789abcdef";
    const OTHER_PROFILE: &str = "fedcba9876543210";
    const CHILD_STATE_ROOT_ENV: &str = "CODEX_USAGE_MONIT_PROFILE_LEASE_CHILD_STATE_ROOT";
    const CHILD_READY_ENV: &str = "CODEX_USAGE_MONIT_PROFILE_LEASE_CHILD_READY";
    const CHILD_ARRIVED_ENV: &str = "CODEX_USAGE_MONIT_PROFILE_LEASE_CHILD_ARRIVED";
    const CHILD_START_ENV: &str = "CODEX_USAGE_MONIT_PROFILE_LEASE_CHILD_START";
    const CHILD_RELEASE_ENV: &str = "CODEX_USAGE_MONIT_PROFILE_LEASE_CHILD_RELEASE";
    const CHILD_CRASH_POINT_ENV: &str = "CODEX_USAGE_MONIT_PROFILE_LEASE_CHILD_CRASH_POINT";

    fn private_state_root(parent: &Path) -> PathBuf {
        let state = parent.join("state");
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
        }
        #[cfg(not(unix))]
        fs::create_dir(&state).unwrap();
        fs::canonicalize(state).unwrap()
    }

    fn profile() -> HistoryProfileId {
        PROFILE.parse().unwrap()
    }

    fn acquire(
        state: &Path,
        profile_id: HistoryProfileId,
        redaction: RedactionProfile,
    ) -> HistoryProfileLeaseGuard {
        match try_acquire_history_profile_lease(state, profile_id, redaction).unwrap() {
            TryHistoryProfileLease::Acquired(guard) => guard,
            TryHistoryProfileLease::Busy { active_profile } => {
                panic!("history profile lease unexpectedly busy: {active_profile:?}")
            }
        }
    }

    fn marker_path(state: &Path, profile_id: &HistoryProfileId) -> PathBuf {
        state
            .join(LEASES_DIRECTORY)
            .join(profile_id.as_str())
            .join(ACTIVE_PROFILE_FILE)
    }

    fn wait_for_child_file(child: &mut Child, path: &Path, subject: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.is_file() {
            assert!(Instant::now() < deadline, "{subject} timed out");
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before {subject}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn spawn_exact_test(test: &str, state: &Path, ready: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(CHILD_STATE_ROOT_ENV, state)
            .env(CHILD_READY_ENV, ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn initial_selection_is_persisted_and_validated() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let guard = acquire(&state, profile(), RedactionProfile::Redacted);
        guard.validate().unwrap();

        let marker = read_active_profile(
            marker_path(&state, &profile()).parent().unwrap(),
            &profile(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(marker.profile_id(), &profile());
        assert_eq!(marker.redaction_profile(), RedactionProfile::Redacted);
    }

    #[test]
    fn same_profile_supports_multiple_concurrent_shared_guards() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let first = acquire(&state, profile(), RedactionProfile::PreviewEnabled);
        let second = acquire(&state, profile(), RedactionProfile::PreviewEnabled);
        first.validate().unwrap();
        second.validate().unwrap();
    }

    #[test]
    fn different_redaction_profile_is_busy_until_every_shared_guard_drops() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let first = acquire(&state, profile(), RedactionProfile::Redacted);
        let second = acquire(&state, profile(), RedactionProfile::Redacted);

        let busy =
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::PreviewEnabled)
                .unwrap();
        let TryHistoryProfileLease::Busy {
            active_profile: Some(active),
        } = busy
        else {
            panic!("different redaction profile should be busy")
        };
        assert_eq!(active.redaction_profile(), RedactionProfile::Redacted);

        drop(first);
        assert!(matches!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::PreviewEnabled)
                .unwrap(),
            TryHistoryProfileLease::Busy { .. }
        ));
        drop(second);
        let switched = acquire(&state, profile(), RedactionProfile::PreviewEnabled);
        switched.validate().unwrap();
    }

    #[test]
    fn different_history_profile_ids_are_independent() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let first = acquire(&state, profile(), RedactionProfile::Redacted);
        let second = acquire(
            &state,
            OTHER_PROFILE.parse().unwrap(),
            RedactionProfile::PreviewEnabled,
        );
        first.validate().unwrap();
        second.validate().unwrap();
        assert_ne!(first.lock_path(), second.lock_path());
    }

    #[test]
    fn contended_try_is_non_blocking() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let _guard = acquire(&state, profile(), RedactionProfile::Redacted);
        let started = Instant::now();
        assert!(matches!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::PreviewEnabled)
                .unwrap(),
            TryHistoryProfileLease::Busy { .. }
        ));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn competing_profile_switches_never_hold_different_profiles_together() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        drop(acquire(&state, profile(), RedactionProfile::Redacted));

        let barrier = Arc::new(Barrier::new(3));
        let (result_tx, result_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let mut threads = Vec::new();
        for redaction in [RedactionProfile::Redacted, RedactionProfile::PreviewEnabled] {
            let state = state.clone();
            let barrier = Arc::clone(&barrier);
            let result_tx = result_tx.clone();
            let release_rx = Arc::clone(&release_rx);
            threads.push(thread::spawn(move || {
                barrier.wait();
                let result =
                    try_acquire_history_profile_lease(&state, profile(), redaction).unwrap();
                match result {
                    TryHistoryProfileLease::Acquired(guard) => {
                        result_tx.send(Some(redaction)).unwrap();
                        release_rx.lock().unwrap().recv().unwrap();
                        drop(guard);
                    }
                    TryHistoryProfileLease::Busy { .. } => {
                        result_tx.send(None).unwrap();
                    }
                }
            }));
        }
        barrier.wait();
        let results = [
            result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        let acquired = results.iter().filter(|result| result.is_some()).count();
        assert!(acquired <= 1);
        if acquired == 1 {
            release_tx.send(()).unwrap();
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let active = read_active_profile(
            marker_path(&state, &profile()).parent().unwrap(),
            &profile(),
        )
        .unwrap()
        .unwrap();
        let guard = acquire(&state, profile(), active.redaction_profile());
        guard.validate().unwrap();
    }

    #[test]
    fn corrupt_future_and_mismatched_markers_fail_closed() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        drop(acquire(&state, profile(), RedactionProfile::Redacted));
        let path = marker_path(&state, &profile());

        fs::write(&path, b"not-json\n").unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let future = serde_json::json!({
            "schemaVersion": HISTORY_PROFILE_LEASE_MARKER_VERSION + 1,
            "historyProfileId": PROFILE,
            "activeRedactionProfile": "redacted"
        });
        fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .to_string()
                .contains("future")
        );

        let mismatched = serde_json::json!({
            "schemaVersion": HISTORY_PROFILE_LEASE_MARKER_VERSION,
            "historyProfileId": OTHER_PROFILE,
            "activeRedactionProfile": "redacted"
        });
        fs::write(&path, serde_json::to_vec(&mismatched).unwrap()).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn future_and_mismatched_stable_lock_bindings_fail_closed() {
        for failure in ["future", "mismatched"] {
            let directory = tempdir().unwrap();
            let state = private_state_root(directory.path());
            drop(acquire(&state, profile(), RedactionProfile::Redacted));
            let profile_directory = marker_path(&state, &profile())
                .parent()
                .unwrap()
                .to_path_buf();
            let file = open_stable_lock(&profile_directory).unwrap();
            fs2::FileExt::try_lock_exclusive(&file).unwrap();
            let mut stored = StoredLockSlot::new(
                if failure == "mismatched" {
                    OTHER_PROFILE.parse().unwrap()
                } else {
                    profile()
                },
                LOCK_INITIALIZING_GENERATION,
                LockState::Initializing,
            )
            .unwrap();
            if failure == "future" {
                stored.schema_version = HISTORY_PROFILE_LEASE_LOCK_VERSION + 1;
                stored.checksum = stored.expected_checksum().unwrap();
            }
            write_lock_slot(&file, &stored).unwrap();
            fs2::FileExt::unlock(&file).unwrap();

            let error =
                try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                    .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            if failure == "future" {
                assert!(error.to_string().contains("future"));
            } else {
                assert!(error.to_string().contains("profile-scoped"));
            }
        }
    }

    #[test]
    fn removed_marker_after_initialization_fails_closed() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        drop(acquire(&state, profile(), RedactionProfile::Redacted));
        fs::remove_file(marker_path(&state, &profile())).unwrap();
        let error =
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn creator_exit_during_initial_publication_is_recovered_across_processes() {
        for crash_point in ["empty-lock", "initializing", "marker-published"] {
            let directory = tempdir().unwrap();
            let state = private_state_root(directory.path());
            let ready = directory.path().join("creator-ready");
            let mut child = spawn_exact_test(
                "history_profile_lease::tests::profile_lease_creator_exit_helper",
                &state,
                &ready,
            )
            .env(CHILD_CRASH_POINT_ENV, crash_point)
            .spawn()
            .unwrap();
            wait_for_child_file(&mut child, &ready, "creator crash point");
            assert!(child.wait().unwrap().success());
            assert_eq!(
                marker_path(&state, &profile()).exists(),
                crash_point == "marker-published"
            );

            let requested = if crash_point == "marker-published" {
                RedactionProfile::PreviewEnabled
            } else {
                RedactionProfile::Redacted
            };
            let guard = acquire(&state, profile(), requested);
            guard.validate().unwrap();
            let lock = open_stable_lock(guard.lock_path().parent().unwrap()).unwrap();
            assert_eq!(
                read_lock_initialization(&lock, &profile()).unwrap(),
                LockInitialization::Initialized
            );
        }
    }

    #[test]
    fn profile_lease_creator_exit_helper() {
        let Some(state) = std::env::var_os(CHILD_STATE_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let ready = PathBuf::from(std::env::var_os(CHILD_READY_ENV).unwrap());
        let crash_point = std::env::var(CHILD_CRASH_POINT_ENV).unwrap();
        let profile_id = profile();
        let directory = prepare_profile_directory(&state, &profile_id).unwrap();
        let file = open_stable_lock(&directory).unwrap();
        if matches!(crash_point.as_str(), "initializing" | "marker-published") {
            fs2::FileExt::try_lock_exclusive(&file).unwrap();
            write_lock_slot(
                &file,
                &StoredLockSlot::new(
                    profile_id,
                    LOCK_INITIALIZING_GENERATION,
                    LockState::Initializing,
                )
                .unwrap(),
            )
            .unwrap();
            if crash_point == "marker-published" {
                write_active_profile(&directory, &profile(), RedactionProfile::Redacted).unwrap();
            }
        } else {
            assert_eq!(crash_point, "empty-lock");
            assert_eq!(file.metadata().unwrap().len(), 0);
        }
        fs::write(ready, b"ready\n").unwrap();
    }

    #[test]
    fn simultaneous_cross_process_first_acquisition_recovers_and_shares() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let start = directory.path().join("start");
        let release = directory.path().join("release");
        let mut children = Vec::new();
        for index in 0..2 {
            let ready = directory.path().join(format!("ready-{index}"));
            let arrived = directory.path().join(format!("arrived-{index}"));
            let child = spawn_exact_test(
                "history_profile_lease::tests::profile_lease_first_acquisition_child_helper",
                &state,
                &ready,
            )
            .env(CHILD_ARRIVED_ENV, &arrived)
            .env(CHILD_START_ENV, &start)
            .env(CHILD_RELEASE_ENV, &release)
            .spawn()
            .unwrap();
            children.push((child, arrived, ready));
        }
        for (child, arrived, _) in &mut children {
            wait_for_child_file(child, arrived, "first-acquisition barrier");
        }
        fs::write(&start, b"start\n").unwrap();
        for (child, _, ready) in &mut children {
            wait_for_child_file(child, ready, "shared first acquisition");
        }

        let lock = open_stable_lock(
            marker_path(&state, &profile())
                .parent()
                .expect("marker has a profile directory"),
        )
        .unwrap();
        assert_eq!(
            read_lock_initialization(&lock, &profile()).unwrap(),
            LockInitialization::Initialized
        );
        fs::write(&release, b"release\n").unwrap();
        for (mut child, _, _) in children {
            assert!(child.wait().unwrap().success());
        }
        let guard = acquire(&state, profile(), RedactionProfile::Redacted);
        guard.validate().unwrap();
    }

    #[test]
    fn profile_lease_first_acquisition_child_helper() {
        let Some(state) = std::env::var_os(CHILD_STATE_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let arrived = PathBuf::from(std::env::var_os(CHILD_ARRIVED_ENV).unwrap());
        let ready = PathBuf::from(std::env::var_os(CHILD_READY_ENV).unwrap());
        let start = PathBuf::from(std::env::var_os(CHILD_START_ENV).unwrap());
        let release = PathBuf::from(std::env::var_os(CHILD_RELEASE_ENV).unwrap());
        fs::write(arrived, b"arrived\n").unwrap();
        while !start.is_file() {
            thread::sleep(Duration::from_millis(5));
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let guard = loop {
            match try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap()
            {
                TryHistoryProfileLease::Acquired(guard) => break guard,
                TryHistoryProfileLease::Busy { .. } => {
                    assert!(Instant::now() < deadline, "first acquisition remained busy");
                    thread::sleep(Duration::from_millis(5));
                }
            }
        };
        guard.validate().unwrap();
        fs::write(ready, b"ready\n").unwrap();
        while !release.is_file() {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn child_process_shares_same_profile_and_excludes_other_profile() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let ready = directory.path().join("child-ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "history_profile_lease::tests::profile_lease_child_process_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_STATE_ROOT_ENV, &state)
            .env(CHILD_READY_ENV, &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.is_file() {
            assert!(Instant::now() < deadline, "child lease timed out");
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before holding its lease"
            );
            thread::sleep(Duration::from_millis(20));
        }

        let local = acquire(&state, profile(), RedactionProfile::PreviewEnabled);
        assert!(matches!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap(),
            TryHistoryProfileLease::Busy { .. }
        ));
        drop(local);
        child.kill().unwrap();
        child.wait().unwrap();
        let switched = acquire(&state, profile(), RedactionProfile::Redacted);
        switched.validate().unwrap();
    }

    #[test]
    fn profile_lease_child_process_helper() {
        let Some(state) = std::env::var_os(CHILD_STATE_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let ready = PathBuf::from(std::env::var_os(CHILD_READY_ENV).unwrap());
        let _guard = acquire(&state, profile(), RedactionProfile::PreviewEnabled);
        fs::write(ready, b"ready\n").unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_paths_reject_symlinks_hardlinks_and_non_private_modes() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        drop(acquire(&state, profile(), RedactionProfile::Redacted));
        let profile_directory = marker_path(&state, &profile())
            .parent()
            .unwrap()
            .to_path_buf();
        let marker = profile_directory.join(ACTIVE_PROFILE_FILE);
        let marker_alias = profile_directory.join("marker-alias");
        fs::hard_link(&marker, &marker_alias).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(marker_alias).unwrap();

        let lock = profile_directory.join(PROFILE_LOCK_FILE);
        let lock_alias = profile_directory.join("lock-alias");
        fs::hard_link(&lock, &lock_alias).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(lock_alias).unwrap();

        let marker_target = directory.path().join("marker-target");
        fs::rename(&marker, &marker_target).unwrap();
        symlink(&marker_target, &marker).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(&marker).unwrap();
        fs::rename(marker_target, &marker).unwrap();

        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        let other_directory = tempdir().unwrap();
        let non_private_state = private_state_root(other_directory.path());
        fs::set_permissions(&non_private_state, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(
                &non_private_state,
                profile(),
                RedactionProfile::Redacted
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn displaced_stable_lock_is_detected_by_a_live_guard() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let guard = acquire(&state, profile(), RedactionProfile::Redacted);
        let lock = guard.lock_path().to_path_buf();
        fs::remove_file(&lock).unwrap();
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            guard.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn noncanonical_state_root_is_rejected() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        let noncanonical = state.join("..").join("state");
        let error =
            try_acquire_history_profile_lease(&noncanonical, profile(), RedactionProfile::Redacted)
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn windows_stable_lock_share_mode_excludes_delete() {
        assert_eq!(stable_lock_share_mode_for_test(), 0x1 | 0x2);
        assert_eq!(stable_lock_share_mode_for_test() & 0x4, 0);
    }

    #[test]
    fn windows_reparse_attribute_policy_is_fail_closed() {
        assert!(windows_attributes_are_reparse_for_test(0x400, 0x400));
        assert!(!windows_attributes_are_reparse_for_test(0x20, 0x400));
    }

    #[cfg(windows)]
    #[test]
    fn windows_files_reject_hard_link_aliases() {
        let directory = tempdir().unwrap();
        let state = private_state_root(directory.path());
        drop(acquire(&state, profile(), RedactionProfile::Redacted));
        let profile_directory = marker_path(&state, &profile())
            .parent()
            .unwrap()
            .to_path_buf();

        let marker = profile_directory.join(ACTIVE_PROFILE_FILE);
        let marker_alias = profile_directory.join("marker-alias");
        fs::hard_link(&marker, &marker_alias).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(marker_alias).unwrap();

        let lock = profile_directory.join(PROFILE_LOCK_FILE);
        let lock_alias = profile_directory.join("lock-alias");
        fs::hard_link(&lock, &lock_alias).unwrap();
        assert_eq!(
            try_acquire_history_profile_lease(&state, profile(), RedactionProfile::Redacted)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
