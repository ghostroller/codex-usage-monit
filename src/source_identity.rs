use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Deserializer, Serialize};

use crate::atomic_file::replace_file;

pub const SOURCE_IDENTITY_VERSION: u32 = 2;
const LEGACY_SOURCE_IDENTITY_VERSION: u32 = 1;

const APP_DIRECTORY: &str = "codex-usage-monit";
const STATE_DIRECTORY_ENV: &str = "CODEX_USAGE_MONIT_STATE_DIR";
const IDENTITY_FILE: &str = "source-identity.json";
const IDENTITY_ANCHOR_FILE: &str = "source-identity.anchor";
const LOCK_FILE: &str = "source-identity.lock";
const NODE_ID_PREFIX: &str = "node-";
const NODE_ID_RANDOM_BYTES: usize = 16;
const NODE_ID_HEX_LEN: usize = NODE_ID_RANDOM_BYTES * 2;
const NODE_ID_LEN: usize = NODE_ID_PREFIX.len() + NODE_ID_HEX_LEN;
const PROJECT_KEY_SECRET_BYTES: usize = 32;
const PROJECT_KEY_SECRET_HEX_LEN: usize = PROJECT_KEY_SECRET_BYTES * 2;
const MAX_IDENTITY_FILE_BYTES: u64 = 4 * 1024;
const IDENTITY_ANCHOR_VERSION: u32 = 1;
const MAX_IDENTITY_ANCHOR_BYTES: u64 = 1024;
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Stable, opaque identity for one local Codex usage source.
///
/// The textual form is deliberately strict so a malformed or hand-edited
/// identity cannot silently create a second source namespace.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn generate_distinct(previous: Option<&Self>) -> io::Result<Self> {
        // Repeating protects both against an all-zero provider result and the
        // astronomically unlikely case where rotate produces the old ID.
        for _ in 0..8 {
            let mut random = [0_u8; NODE_ID_RANDOM_BYTES];
            getrandom::fill(&mut random).map_err(|error| {
                io::Error::other(format!("could not generate node ID: {error}"))
            })?;
            if random.iter().all(|byte| *byte == 0) {
                continue;
            }

            let mut value = String::with_capacity(NODE_ID_LEN);
            value.push_str(NODE_ID_PREFIX);
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for byte in random {
                value.push(HEX[usize::from(byte >> 4)] as char);
                value.push(HEX[usize::from(byte & 0x0f)] as char);
            }
            let candidate = Self(value);
            if previous != Some(&candidate) {
                return Ok(candidate);
            }
        }

        Err(io::Error::other(
            "secure random provider repeatedly returned an unusable node ID",
        ))
    }
}

impl AsRef<str> for NodeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NodeId {
    type Err = NodeIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != NODE_ID_LEN {
            return Err(NodeIdParseError("node ID has the wrong length"));
        }
        let Some(hex) = value.strip_prefix(NODE_ID_PREFIX) else {
            return Err(NodeIdParseError("node ID has the wrong prefix"));
        };
        if !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(NodeIdParseError(
                "node ID must use lowercase hexadecimal characters",
            ));
        }
        if hex.bytes().all(|byte| byte == b'0') {
            return Err(NodeIdParseError("node ID must not be all zeroes"));
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeIdParseError(&'static str);

impl fmt::Display for NodeIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for NodeIdParseError {}

/// Persistent machine identity, exporter generation, and private project-key
/// material. A rotate changes the node ID and project-key secret while
/// incrementing the generation, invalidating prior cursors and observed keys.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    version: u32,
    node_id: NodeId,
    generation: u64,
    project_key_secret: ProjectKeySecret,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSourceIdentityV2 {
    version: u32,
    node_id: NodeId,
    generation: u64,
    project_key_secret: ProjectKeySecret,
}

impl From<&SourceIdentity> for StoredSourceIdentityV2 {
    fn from(identity: &SourceIdentity) -> Self {
        Self {
            version: identity.version,
            node_id: identity.node_id.clone(),
            generation: identity.generation,
            project_key_secret: identity.project_key_secret.clone(),
        }
    }
}

impl From<StoredSourceIdentityV2> for SourceIdentity {
    fn from(identity: StoredSourceIdentityV2) -> Self {
        Self {
            version: identity.version,
            node_id: identity.node_id,
            generation: identity.generation,
            project_key_secret: identity.project_key_secret,
        }
    }
}

impl fmt::Debug for SourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceIdentity")
            .field("version", &self.version)
            .field("node_id", &self.node_id)
            .field("generation", &self.generation)
            .field("project_key_secret", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct ProjectKeySecret(String);

impl fmt::Debug for ProjectKeySecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl ProjectKeySecret {
    fn generate() -> io::Result<Self> {
        for _ in 0..8 {
            let mut random = [0_u8; PROJECT_KEY_SECRET_BYTES];
            getrandom::fill(&mut random).map_err(|error| {
                io::Error::other(format!("could not generate project-key secret: {error}"))
            })?;
            if random.iter().all(|byte| *byte == 0) {
                continue;
            }
            let mut value = String::with_capacity(PROJECT_KEY_SECRET_HEX_LEN);
            append_lower_hex(&mut value, &random);
            return Ok(Self(value));
        }
        Err(io::Error::other(
            "secure random provider repeatedly returned an unusable project-key secret",
        ))
    }

    fn validate(&self) -> io::Result<()> {
        if self.0.len() != PROJECT_KEY_SECRET_HEX_LEN
            || !self
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.0.bytes().all(|byte| byte == b'0')
        {
            return Err(invalid_identity("project-key secret is invalid"));
        }
        Ok(())
    }

    fn decode(&self) -> [u8; PROJECT_KEY_SECRET_BYTES] {
        let mut bytes = [0_u8; PROJECT_KEY_SECRET_BYTES];
        for (index, pair) in self.0.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        bytes
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySourceIdentityV1 {
    version: u32,
    node_id: NodeId,
    generation: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredSourceIdentity {
    Current(StoredSourceIdentityV2),
    Legacy(LegacySourceIdentityV1),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredIdentityAnchor {
    version: u32,
}

impl SourceIdentity {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn project_key_secret(&self) -> [u8; PROJECT_KEY_SECRET_BYTES] {
        self.project_key_secret.decode()
    }

    #[cfg(test)]
    pub(crate) fn from_test_parts(node_id: NodeId, secret_hex: &str) -> Self {
        Self {
            version: SOURCE_IDENTITY_VERSION,
            node_id,
            generation: 1,
            project_key_secret: ProjectKeySecret(secret_hex.to_owned()),
        }
    }

    fn generate(generation: u64, previous: Option<&NodeId>) -> io::Result<Self> {
        Ok(Self {
            version: SOURCE_IDENTITY_VERSION,
            node_id: NodeId::generate_distinct(previous)?,
            generation,
            project_key_secret: ProjectKeySecret::generate()?,
        })
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != SOURCE_IDENTITY_VERSION {
            return Err(invalid_identity(format!(
                "unsupported source identity version {}; expected {}",
                self.version, SOURCE_IDENTITY_VERSION
            )));
        }
        if self.generation == 0 {
            return Err(invalid_identity(
                "source identity generation must be greater than zero",
            ));
        }
        self.project_key_secret.validate()?;
        Ok(())
    }
}

/// Store for the process-independent identity shared by local collection and
/// the short-lived remote exporter.
#[derive(Clone, Debug)]
pub struct SourceIdentityStore {
    path: Option<PathBuf>,
}

impl Default for SourceIdentityStore {
    fn default() -> Self {
        Self::discover()
    }
}

impl SourceIdentityStore {
    pub fn discover() -> Self {
        Self {
            path: default_source_identity_path(),
        }
    }

    /// Creates a store at an explicit path. This is useful for embedding and
    /// tests; normal application code should use [`Self::discover`].
    pub fn at_path(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Strictly loads an existing identity. Missing, malformed, insecure, or
    /// unsupported files are errors and are never repaired implicitly.
    pub fn load(&self) -> io::Result<SourceIdentity> {
        let path = self.required_path()?;
        validate_private_directory(identity_parent(path))?;
        read_identity(path)
    }

    /// Loads the stable identity, creating it once when it is genuinely absent.
    /// A corrupt existing file fails closed instead of being replaced.
    pub fn load_or_create(&self) -> io::Result<SourceIdentity> {
        let path = self.required_path()?;
        let parent = identity_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent)?;

        let stored_identity = match read_stored_identity(path) {
            Ok(identity) => {
                validate_stored_identity(&identity)?;
                Some(identity)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let anchor_path = parent.join(IDENTITY_ANCHOR_FILE);
        let anchor_exists = match read_identity_anchor(&anchor_path) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };

        if let Some(identity) = stored_identity {
            // v0.4 bootstrap: an already-valid pre-anchor identity is the
            // continuity authority. Publish the create-once sentinel before
            // performing a legacy schema upgrade or returning it.
            if !anchor_exists {
                ensure_identity_anchor(&anchor_path)?;
            }
            return materialize_stored_identity(path, identity);
        }
        if anchor_exists {
            return Err(missing_initialized_identity());
        }

        // Generate the candidate before committing the sentinel so a random
        // provider failure does not manufacture an anchor-only crash state.
        // The sentinel is nevertheless durably published before the identity
        // path, so a crash can never be mistaken for another first install.
        let identity = SourceIdentity::generate(1, None)?;
        if !create_identity_anchor_atomically(&anchor_path)? {
            // A non-cooperating initializer raced us. Its complete pair wins;
            // an anchor-only outcome is treated exactly like a prior crash.
            read_identity_anchor(&anchor_path)?;
            return load_existing_identity_after_anchor_race(path);
        }
        if create_identity_atomically(path, &identity)? {
            Ok(identity)
        } else {
            // A non-cooperating writer may have published an identity after
            // the absence check. Its file wins and is validated strictly.
            load_existing_identity_after_anchor_race(path)
        }
    }

    /// Explicitly replaces the current node ID and increments its generation.
    /// The new identity is returned only after the atomic write completes.
    pub fn rotate(&self) -> io::Result<SourceIdentity> {
        let path = self.required_path()?;
        let parent = identity_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent)?;

        let current = read_stored_identity(path)?;
        validate_stored_identity(&current)?;
        let (current_node_id, current_generation) = match &current {
            StoredSourceIdentity::Current(identity) => {
                let identity = SourceIdentity::from(identity.clone());
                (identity.node_id.clone(), identity.generation())
            }
            StoredSourceIdentity::Legacy(identity) => {
                (identity.node_id.clone(), identity.generation)
            }
        };
        ensure_identity_anchor(&parent.join(IDENTITY_ANCHOR_FILE))?;
        let generation = current_generation
            .checked_add(1)
            .ok_or_else(|| invalid_identity("source identity generation cannot be incremented"))?;
        let rotated = SourceIdentity::generate(generation, Some(&current_node_id))?;
        write_identity_atomically(path, &rotated)?;
        Ok(rotated)
    }

    /// Verifies that the exporter state directory remains writable without
    /// changing the durable source identity or leaving probe state behind.
    pub(crate) fn probe_state_directory_writable(&self) -> io::Result<()> {
        let path = self.required_path()?;
        let parent = identity_parent(path);
        validate_private_directory(parent)?;
        let (temporary, mut file) = create_temporary_file(parent, OsStr::new("writable-probe"))?;
        let result = (|| {
            file.write_all(b"probe")?;
            file.sync_all()?;
            drop(file);
            fs::remove_file(&temporary)?;
            sync_directory(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn required_path(&self) -> io::Result<&Path> {
        self.path.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no user-level state directory is available for source identity",
            )
        })
    }
}

pub fn default_source_identity_path() -> Option<PathBuf> {
    resolve_source_identity_path(
        nonempty_env(STATE_DIRECTORY_ENV).as_deref(),
        nonempty_env("XDG_STATE_HOME").as_deref(),
        nonempty_env("HOME").as_deref(),
        nonempty_env("LOCALAPPDATA").as_deref(),
        nonempty_env("USERPROFILE").as_deref(),
        current_platform(),
    )
}

fn read_identity(path: &Path) -> io::Result<SourceIdentity> {
    match read_stored_identity(path)? {
        StoredSourceIdentity::Current(identity) => {
            let identity = SourceIdentity::from(identity);
            identity.validate()?;
            Ok(identity)
        }
        StoredSourceIdentity::Legacy(_) => Err(invalid_identity(
            "source identity schema v1 requires a locked load_or_create upgrade",
        )),
    }
}

fn read_stored_identity(path: &Path) -> io::Result<StoredSourceIdentity> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_identity_file_metadata(&path_metadata)?;

    let file = open_identity_file(path)?;
    let metadata = file.metadata()?;
    validate_identity_file_metadata(&metadata)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &metadata, "source identity")?;
    if metadata.len() > MAX_IDENTITY_FILE_BYTES {
        return Err(invalid_identity("source identity file is too large"));
    }
    ensure_private_file(path, &file, &metadata, "source identity file")?;

    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_IDENTITY_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_IDENTITY_FILE_BYTES {
        return Err(invalid_identity("source identity file is too large"));
    }

    let identity: StoredSourceIdentity = serde_json::from_slice(&contents)
        .map_err(|error| invalid_identity(format!("invalid source identity: {error}")))?;
    Ok(identity)
}

fn validate_stored_identity(identity: &StoredSourceIdentity) -> io::Result<()> {
    match identity {
        StoredSourceIdentity::Current(identity) => {
            SourceIdentity::from(identity.clone()).validate()
        }
        StoredSourceIdentity::Legacy(identity) => validate_legacy_identity(identity),
    }
}

fn materialize_stored_identity(
    path: &Path,
    identity: StoredSourceIdentity,
) -> io::Result<SourceIdentity> {
    match identity {
        StoredSourceIdentity::Current(identity) => {
            let identity = SourceIdentity::from(identity);
            identity.validate()?;
            Ok(identity)
        }
        StoredSourceIdentity::Legacy(legacy) => {
            let identity = upgrade_legacy_identity(legacy)?;
            write_identity_atomically(path, &identity)?;
            Ok(identity)
        }
    }
}

fn load_or_upgrade_identity(path: &Path) -> io::Result<SourceIdentity> {
    materialize_stored_identity(path, read_stored_identity(path)?)
}

fn load_existing_identity_after_anchor_race(path: &Path) -> io::Result<SourceIdentity> {
    load_or_upgrade_identity(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            missing_initialized_identity()
        } else {
            error
        }
    })
}

fn missing_initialized_identity() -> io::Error {
    invalid_identity(
        "source identity is missing after this state directory was initialized; refusing implicit generation=1 replacement (restore the original identity or use an explicit identity-repair workflow; remove the anchor only when accepting continuity and data loss)",
    )
}

fn validate_legacy_identity(identity: &LegacySourceIdentityV1) -> io::Result<()> {
    if identity.version != LEGACY_SOURCE_IDENTITY_VERSION || identity.generation == 0 {
        return Err(invalid_identity("invalid legacy source identity"));
    }
    Ok(())
}

fn upgrade_legacy_identity(identity: LegacySourceIdentityV1) -> io::Result<SourceIdentity> {
    validate_legacy_identity(&identity)?;
    Ok(SourceIdentity {
        version: SOURCE_IDENTITY_VERSION,
        node_id: identity.node_id,
        generation: identity.generation,
        project_key_secret: ProjectKeySecret::generate()?,
    })
}

fn append_lower_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("validated project-key secret contains only lowercase hex"),
    }
}

fn open_identity_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    options
        .open(path)
        .map_err(|error| map_nofollow_error(error, "source identity path"))
}

fn read_identity_anchor(path: &Path) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_identity_file_metadata(&path_metadata)?;

    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, "source identity anchor"))?;
    let metadata = file.metadata()?;
    validate_identity_file_metadata(&metadata)?;
    ensure_opened_file_matches_path(
        path,
        &file,
        &path_metadata,
        &metadata,
        "source identity anchor",
    )?;
    if metadata.len() > MAX_IDENTITY_ANCHOR_BYTES {
        return Err(invalid_identity("source identity anchor is too large"));
    }
    ensure_private_file(path, &file, &metadata, "source identity anchor")?;

    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_IDENTITY_ANCHOR_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_IDENTITY_ANCHOR_BYTES {
        return Err(invalid_identity("source identity anchor is too large"));
    }
    let anchor: StoredIdentityAnchor = serde_json::from_slice(&contents)
        .map_err(|error| invalid_identity(format!("invalid source identity anchor: {error}")))?;
    if anchor.version != IDENTITY_ANCHOR_VERSION {
        return Err(invalid_identity(format!(
            "unsupported source identity anchor version {}; expected {}",
            anchor.version, IDENTITY_ANCHOR_VERSION
        )));
    }
    Ok(())
}

fn ensure_identity_anchor(path: &Path) -> io::Result<()> {
    match read_identity_anchor(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if create_identity_anchor_atomically(path)? {
                Ok(())
            } else {
                read_identity_anchor(path)
            }
        }
        Err(error) => Err(error),
    }
}

fn create_identity_anchor_atomically(path: &Path) -> io::Result<bool> {
    let anchor = StoredIdentityAnchor {
        version: IDENTITY_ANCHOR_VERSION,
    };
    let mut contents = serde_json::to_vec_pretty(&anchor)
        .map_err(|error| invalid_identity(format!("invalid source identity anchor: {error}")))?;
    contents.push(b'\n');
    create_private_atomically(path, &contents, "source identity anchor")
}

fn write_identity_atomically(path: &Path, identity: &SourceIdentity) -> io::Result<()> {
    let contents = serialize_identity(identity)?;
    write_private_atomically(path, &contents)
}

/// Publishes a first identity without replacing a path that appeared after the
/// caller's absence check. `Ok(false)` means that path won the race.
fn create_identity_atomically(path: &Path, identity: &SourceIdentity) -> io::Result<bool> {
    let contents = serialize_identity(identity)?;
    create_private_atomically(path, &contents, "source identity file")
}

fn serialize_identity(identity: &SourceIdentity) -> io::Result<Vec<u8>> {
    identity.validate()?;
    let mut contents = serde_json::to_vec_pretty(&StoredSourceIdentityV2::from(identity))
        .map_err(|error| invalid_identity(format!("invalid source identity: {error}")))?;
    contents.push(b'\n');
    Ok(contents)
}

fn invalid_identity(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn identity_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = identity_parent(path);
    create_private_directory(parent)?;
    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new(IDENTITY_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_published_private_file(path, "source identity file")?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Publishes an initial file via a hard link so an independently created path
/// wins instead of being replaced between an absence check and publication.
fn create_private_atomically(path: &Path, contents: &[u8], subject: &str) -> io::Result<bool> {
    let parent = identity_parent(path);
    create_private_directory(parent)?;
    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new(IDENTITY_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                validate_published_private_file(path, subject)?;
                fs::remove_file(&temporary)?;
                sync_directory(parent)?;
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
            Ok(file) => {
                let validation = (|| {
                    let metadata = file.metadata()?;
                    validate_identity_file_metadata(&metadata)?;
                    ensure_private_file(
                        &temporary,
                        &file,
                        &metadata,
                        "source identity temporary file",
                    )
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
        "could not allocate a unique source identity temporary file",
    ))
}

fn open_lock_file(directory: &Path) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_lock_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    add_nofollow_flags(&mut options);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // Keep the stable sidecar name bound to this inode while any process
        // may be coordinating through it. Readers and writers can still open
        // the lock, but replacement/unlink requires FILE_SHARE_DELETE.
        options.share_mode(stable_lock_share_mode());
    }
    let file = options
        .open(&path)
        .map_err(|error| map_nofollow_error(error, "source identity lock"))?;
    let metadata = file.metadata()?;
    validate_lock_metadata(&metadata)?;
    ensure_private_file(&path, &file, &metadata, "source identity lock")?;

    // Re-check the directory entry after open. Together with O_NOFOLLOW and
    // the inode comparison on Unix, this prevents a swapped lock path from
    // making cooperating processes lock different files.
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &metadata,
        "source identity lock",
    )?;
    Ok(file)
}

/// Opens and locks the stable lock-file inode, then verifies that the directory
/// entry still names that inode. Keeping the returned file alive holds the
/// process lock for the caller's complete read/modify/write operation.
fn open_locked_lock_file(directory: &Path) -> io::Result<File> {
    let file = open_lock_file(directory)?;
    lock_opened_lock_file(directory, file)
}

fn lock_opened_lock_file(directory: &Path, file: File) -> io::Result<File> {
    fs2::FileExt::lock_exclusive(&file)?;

    // A lock path replaced between open and lock acquisition would otherwise
    // let two processes lock different inodes. Re-check after the blocking
    // lock call so such a race fails closed.
    validate_private_directory(directory)?;
    let path = directory.join(LOCK_FILE);
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    let opened_metadata = file.metadata()?;
    validate_lock_metadata(&opened_metadata)?;
    ensure_private_file(&path, &file, &opened_metadata, "source identity lock")?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &opened_metadata,
        "source identity lock",
    )?;
    Ok(file)
}

fn add_nofollow_flags(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // O_NOFOLLOW closes the final-component lstat/open race. O_NONBLOCK
        // ensures a FIFO swapped into place cannot hang before fstat rejects
        // it as non-regular.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        // Open the reparse point itself so a path swap cannot redirect an
        // identity or make cooperating processes lock different targets.
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(any(test, windows))]
fn stable_lock_share_mode() -> u32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        FILE_SHARE_READ | FILE_SHARE_WRITE
    }
    #[cfg(not(windows))]
    {
        // Windows SDK values, kept here so the policy has a host-independent
        // regression test without exposing it in non-test Unix builds.
        0x0000_0001 | 0x0000_0002
    }
}

fn map_nofollow_error(error: io::Error, subject: &str) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_identity(format!("{subject} must not be a symbolic link"));
    }
    #[cfg(not(unix))]
    let _ = subject;
    error
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match validate_private_directory(path) {
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
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_identity(
            "source identity state directory must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_identity(
            "source identity state path must be a directory",
        ));
    }
    ensure_private_directory(path, &metadata)
}

fn validate_lock_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_identity(
            "source identity lock must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_identity(
            "source identity lock must be a regular file",
        ));
    }
    ensure_private_path(metadata, "source identity lock")
}

fn validate_identity_file_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_identity(
            "source identity path must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_identity(
            "source identity path must be a regular file",
        ));
    }
    Ok(())
}

fn validate_published_private_file(path: &Path, subject: &str) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_identity_file_metadata(&path_metadata)?;
    let file = open_identity_file(path)?;
    let opened_metadata = file.metadata()?;
    validate_identity_file_metadata(&opened_metadata)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &opened_metadata, subject)?;
    ensure_private_file(path, &file, &opened_metadata, subject)
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    windows_attributes_are_reparse(metadata.file_attributes())
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_path(metadata, subject)
}

#[cfg(unix)]
fn ensure_private_directory(_path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    ensure_private_path(metadata, "source identity state directory")
}

#[cfg(unix)]
fn ensure_private_path(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // SAFETY: geteuid has no preconditions and does not retain pointers.
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
fn ensure_private_file(
    path: &Path,
    file: &File,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    validate_windows_private_file(path, file, subject)
}

#[cfg(windows)]
fn ensure_private_directory(path: &Path, _metadata: &fs::Metadata) -> io::Result<()> {
    validate_windows_private_directory(path, "source identity state directory")
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
        "private source identity files are unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(_path: &Path, _metadata: &fs::Metadata) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private source identity directories are unsupported on this platform",
    ))
}

#[cfg(not(unix))]
fn ensure_private_path(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    // Windows validates DACLs against live handles instead of metadata. This
    // helper remains only for the shared type validation path.
    Ok(())
}

#[cfg(any(windows, test))]
const WINDOWS_ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_DENIED_ACE_TYPE: u8 = 1;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_ALLOWED_COMPOUND_ACE_TYPE: u8 = 4;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
#[cfg(any(windows, test))]
const WINDOWS_ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;
#[cfg(any(windows, test))]
const WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowsAclTrustee {
    CurrentUser,
    LocalSystem,
    Administrators,
    Other,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowsAclEntryPolicy {
    ace_type: u8,
    mask: u32,
    trustee: Option<WindowsAclTrustee>,
}

#[cfg(any(windows, test))]
fn validate_windows_private_acl_policy(
    owner_is_current_user: bool,
    dacl_present: bool,
    entries: &[WindowsAclEntryPolicy],
    subject: &str,
) -> io::Result<()> {
    if !owner_is_current_user {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must be owned by the current Windows user"),
        ));
    }
    if !dacl_present {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must have a non-null private DACL"),
        ));
    }

    for entry in entries {
        match entry.ace_type {
            WINDOWS_ACCESS_ALLOWED_ACE_TYPE => {
                if entry.mask == 0 {
                    continue;
                }
                match entry.trustee {
                    Some(
                        WindowsAclTrustee::CurrentUser
                        | WindowsAclTrustee::LocalSystem
                        | WindowsAclTrustee::Administrators,
                    ) => {}
                    Some(WindowsAclTrustee::Other) | None => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "{subject} DACL grants access to a principal other than the current user or trusted Windows system administrators"
                            ),
                        ));
                    }
                }
            }
            WINDOWS_ACCESS_DENIED_ACE_TYPE
            | WINDOWS_ACCESS_DENIED_OBJECT_ACE_TYPE
            | WINDOWS_ACCESS_DENIED_CALLBACK_ACE_TYPE
            | WINDOWS_ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {}
            WINDOWS_ACCESS_ALLOWED_COMPOUND_ACE_TYPE
            | WINDOWS_ACCESS_ALLOWED_OBJECT_ACE_TYPE
            | WINDOWS_ACCESS_ALLOWED_CALLBACK_ACE_TYPE
            | WINDOWS_ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{subject} DACL contains an unsupported access-granting ACE"),
                ));
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{subject} DACL contains an unsupported ACE type"),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_attributes_are_reparse(attributes: u32) -> bool {
    attributes & WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
pub(crate) fn validate_windows_private_file(
    path: &Path,
    file: &File,
    subject: &str,
) -> io::Result<()> {
    reject_windows_reparse_components(path, subject)?;
    let metadata = file.metadata()?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_identity(format!(
            "{subject} must not be a reparse point"
        )));
    }
    validate_windows_private_handle(file, subject)?;
    reject_windows_reparse_components(path, subject)
}

#[cfg(windows)]
pub(crate) fn validate_windows_private_directory(path: &Path, subject: &str) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    reject_windows_reparse_components(path, subject)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(path)?;
    let metadata = directory.metadata()?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_identity(format!(
            "{subject} must not be a reparse point"
        )));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_identity(format!("{subject} must be a directory")));
    }
    validate_windows_private_handle(&directory, subject)?;
    reject_windows_reparse_components(path, subject)
}

#[cfg(windows)]
fn reject_windows_reparse_components(path: &Path, subject: &str) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;

    for component in path.ancestors() {
        if component.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(component) {
            Ok(metadata) => {
                if windows_attributes_are_reparse(metadata.file_attributes()) {
                    return Err(invalid_identity(format!(
                        "{subject} path must not traverse a reparse point ({})",
                        component.display()
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_private_handle(file: &File, subject: &str) -> io::Result<()> {
    use std::ffi::c_void;
    use std::mem;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, IsValidSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);
    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: GetSecurityInfo returns a single LocalAlloc-backed
                // descriptor which is owned by this guard.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the live file owns the handle; all requested output pointers are
    // valid for writes and the returned descriptor is guarded by LocalFree.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = SecurityDescriptor(descriptor);
    if descriptor.is_null() || owner.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} has no Windows owner SID"),
        ));
    }
    // SAFETY: the owner pointer belongs to the live security descriptor.
    if unsafe { IsValidSid(owner) } == 0 {
        return Err(invalid_identity(format!(
            "{subject} has an invalid Windows owner SID"
        )));
    }

    let current_user = windows_current_user_sid()?;
    // SAFETY: both pointers reference validated SIDs kept alive for the call.
    let owner_is_current_user = unsafe { EqualSid(owner, current_user.as_psid()) } != 0;
    if dacl.is_null() {
        return validate_windows_private_acl_policy(owner_is_current_user, false, &[], subject);
    }

    let mut acl_information = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl belongs to the live security descriptor and the output
    // buffer matches the information class and supplied size.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut acl_information as *mut ACL_SIZE_INFORMATION).cast(),
            u32::try_from(mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(u32::MAX),
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let acl_start = dacl as usize;
    let acl_end = acl_start
        .checked_add(usize::try_from(acl_information.AclBytesInUse).unwrap_or(usize::MAX))
        .ok_or_else(|| invalid_identity(format!("{subject} DACL size overflow")))?;
    let mut entries = Vec::with_capacity(acl_information.AceCount as usize);
    for index in 0..acl_information.AceCount {
        let mut raw_ace: *mut c_void = ptr::null_mut();
        // SAFETY: dacl is valid and index is below the count returned by
        // GetAclInformation; raw_ace points to writable pointer storage.
        if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if raw_ace.is_null() {
            return Err(invalid_identity(format!(
                "{subject} DACL contains a null ACE"
            )));
        }
        let ace_start = raw_ace as usize;
        if ace_start < acl_start || acl_end.saturating_sub(ace_start) < mem::size_of::<ACE_HEADER>()
        {
            return Err(invalid_identity(format!(
                "{subject} DACL contains an out-of-bounds ACE"
            )));
        }
        // SAFETY: GetAce returned a pointer inside the validated ACL. Every
        // ACE begins with ACE_HEADER.
        let header = unsafe { &*raw_ace.cast::<ACE_HEADER>() };
        let ace_end = ace_start
            .checked_add(usize::from(header.AceSize))
            .ok_or_else(|| invalid_identity(format!("{subject} ACE size overflow")))?;
        if usize::from(header.AceSize) < mem::size_of::<ACE_HEADER>() || ace_end > acl_end {
            return Err(invalid_identity(format!(
                "{subject} DACL contains a truncated ACE"
            )));
        }

        let ace_type = header.AceType;
        if ace_type == WINDOWS_ACCESS_ALLOWED_ACE_TYPE {
            let sid_offset = mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
            const MINIMUM_SID_BYTES: usize = 8;
            if usize::from(header.AceSize) < sid_offset + MINIMUM_SID_BYTES {
                return Err(invalid_identity(format!(
                    "{subject} DACL contains a truncated allowed ACE"
                )));
            }
            // SAFETY: the size check above covers the fixed fields and minimum
            // SID header; GetAce guarantees the ACE storage for AceSize bytes.
            let allowed = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
            // SAFETY: sid_offset + MINIMUM_SID_BYTES was checked above.
            let sid_bytes = unsafe { raw_ace.cast::<u8>().add(sid_offset) };
            // SAFETY: every SID header stores SubAuthorityCount at byte 1.
            let sub_authority_count = usize::from(unsafe { *sid_bytes.add(1) });
            let required_sid_bytes = MINIMUM_SID_BYTES
                .checked_add(sub_authority_count.saturating_mul(mem::size_of::<u32>()))
                .ok_or_else(|| invalid_identity(format!("{subject} SID size overflow")))?;
            if usize::from(header.AceSize) < sid_offset + required_sid_bytes {
                return Err(invalid_identity(format!(
                    "{subject} DACL contains a truncated trustee SID"
                )));
            }
            let sid: PSID = sid_bytes.cast();
            // SAFETY: the SID header is within the ACE buffer.
            if unsafe { IsValidSid(sid) } == 0 {
                return Err(invalid_identity(format!(
                    "{subject} DACL contains an invalid trustee SID"
                )));
            }
            let trustee = windows_acl_trustee(sid, current_user.as_psid());
            entries.push(WindowsAclEntryPolicy {
                ace_type,
                mask: allowed.Mask,
                trustee: Some(trustee),
            });
        } else {
            entries.push(WindowsAclEntryPolicy {
                ace_type,
                mask: 0,
                trustee: None,
            });
        }
    }

    validate_windows_private_acl_policy(owner_is_current_user, true, &entries, subject)
}

#[cfg(windows)]
fn windows_acl_trustee(
    sid: windows_sys::Win32::Security::PSID,
    current_user: windows_sys::Win32::Security::PSID,
) -> WindowsAclTrustee {
    use windows_sys::Win32::Security::{
        EqualSid, IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };

    // SAFETY: callers pass validated SIDs that outlive these comparisons.
    if unsafe { EqualSid(sid, current_user) } != 0 {
        WindowsAclTrustee::CurrentUser
    } else if unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0 {
        WindowsAclTrustee::LocalSystem
    } else if unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) } != 0 {
        WindowsAclTrustee::Administrators
    } else {
        WindowsAclTrustee::Other
    }
}

#[cfg(windows)]
struct WindowsSid {
    storage: Vec<usize>,
}

#[cfg(windows)]
impl WindowsSid {
    fn as_psid(&self) -> windows_sys::Win32::Security::PSID {
        self.storage.as_ptr().cast_mut().cast()
    }
}

#[cfg(windows)]
fn windows_current_user_sid() -> io::Result<WindowsSid> {
    use std::mem;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        CopySid, GetLengthSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this guard uniquely owns the token handle.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token points
    // to writable handle storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _token = Token(token);

    let mut required = 0_u32;
    // SAFETY: a zero-length query asks Windows for the required buffer size.
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let word_size = mem::size_of::<usize>();
    let word_count = usize::try_from(required)
        .unwrap_or(usize::MAX)
        .saturating_add(word_size - 1)
        / word_size;
    let mut token_buffer = vec![0_usize; word_count];
    // SAFETY: token_buffer is pointer-aligned and contains at least required
    // writable bytes.
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            token_buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful TokenUser query initialized TOKEN_USER at the
    // start of the aligned buffer.
    let token_user = unsafe { &*token_buffer.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: TOKEN_USER supplies a valid SID pointer on a successful query.
    if token_user.User.Sid.is_null() || unsafe { IsValidSid(token_user.User.Sid) } == 0 {
        return Err(invalid_identity(
            "current Windows access token has no valid user SID",
        ));
    }
    // SAFETY: the SID was validated immediately above.
    let sid_bytes = unsafe { GetLengthSid(token_user.User.Sid) };
    if sid_bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_word_count = usize::try_from(sid_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(word_size - 1)
        / word_size;
    let mut storage = vec![0_usize; sid_word_count];
    // SAFETY: storage is aligned and at least sid_bytes long; the source SID
    // remains alive in token_buffer for this call.
    if unsafe { CopySid(sid_bytes, storage.as_mut_ptr().cast(), token_user.User.Sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(WindowsSid { storage })
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
        Err(invalid_identity(format!(
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
    let expected = windows_file_identity(opened_file, subject)?;

    // Re-open the current directory entry without following a reparse point,
    // then compare that handle's stable identity with the original handle.
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let current = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    if windows_file_identity(&current, subject)? == expected {
        Ok(())
    } else {
        Err(invalid_identity(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File, subject: &str) -> io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the raw handle remains owned and live for this call;
    // GetFileInformationByHandle initializes the output on success and does
    // not retain either pointer.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API reported success, so the complete structure is
    // initialized.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);

    require_stable_file_identity(
        Some(information.dwVolumeSerialNumber),
        Some(file_index),
        subject,
    )
}

#[cfg(any(windows, test))]
fn require_stable_file_identity(
    volume_serial_number: Option<u32>,
    file_index: Option<u64>,
    subject: &str,
) -> io::Result<(u32, u64)> {
    match (volume_serial_number, file_index) {
        (Some(volume_serial_number), Some(file_index)) => Ok((volume_serial_number, file_index)),
        _ => Err(invalid_identity(format!(
            "{subject} does not expose a stable file identity"
        ))),
    }
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // replace_file uses MOVEFILE_WRITE_THROUGH on Windows. Rust does not offer
    // a portable directory-handle fsync for the create-only hard-link path, so
    // that path guarantees runtime atomicity but not a stronger crash-durability
    // promise on every Windows filesystem.
    Ok(())
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

fn resolve_source_identity_path(
    state_directory: Option<&Path>,
    xdg_state_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    user_profile: Option<&Path>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(directory) = nonempty_path(state_directory) {
        return Some(directory.join(IDENTITY_FILE));
    }
    if let Some(directory) = nonempty_path(xdg_state_home) {
        return Some(directory.join(APP_DIRECTORY).join(IDENTITY_FILE));
    }

    let directory = match platform {
        Platform::MacOs => nonempty_path(home).map(|path| path.join("Library/Application Support")),
        Platform::Windows => nonempty_path(local_app_data)
            .map(Path::to_path_buf)
            .or_else(|| nonempty_path(user_profile).map(|path| path.join("AppData").join("Local"))),
        Platform::Unix => nonempty_path(home).map(|path| path.join(".local/state")),
    }?;
    Some(directory.join(APP_DIRECTORY).join(IDENTITY_FILE))
}

fn nonempty_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use tempfile::tempdir;

    const VALID_NODE_ID: &str = "node-0123456789abcdef0123456789abcdef";

    #[test]
    fn resolver_matches_existing_cross_platform_state_layout() {
        let override_directory = Path::new("/override-state");
        let xdg = Path::new("/xdg-state");
        let home = Path::new("/home/user");
        let local = Path::new("C:/Users/user/AppData/Local");

        assert_eq!(
            resolve_source_identity_path(
                Some(override_directory),
                Some(xdg),
                Some(home),
                Some(local),
                None,
                Platform::Windows,
            ),
            Some(override_directory.join(IDENTITY_FILE))
        );
        assert_eq!(
            resolve_source_identity_path(None, Some(xdg), Some(home), None, None, Platform::MacOs,),
            Some(xdg.join(APP_DIRECTORY).join(IDENTITY_FILE))
        );
        assert_eq!(
            resolve_source_identity_path(None, None, Some(home), None, None, Platform::MacOs),
            Some(
                home.join("Library/Application Support")
                    .join(APP_DIRECTORY)
                    .join(IDENTITY_FILE)
            )
        );
        assert_eq!(
            resolve_source_identity_path(None, None, Some(home), None, None, Platform::Unix),
            Some(
                home.join(".local/state")
                    .join(APP_DIRECTORY)
                    .join(IDENTITY_FILE)
            )
        );
        assert_eq!(
            resolve_source_identity_path(None, None, None, Some(local), None, Platform::Windows,),
            Some(local.join(APP_DIRECTORY).join(IDENTITY_FILE))
        );
        assert_eq!(
            resolve_source_identity_path(None, None, None, None, None, Platform::Unix),
            None
        );
    }

    #[test]
    fn windows_resolver_falls_back_to_user_profile_local_app_data() {
        let user_profile = Path::new("C:/Users/developer");
        assert_eq!(
            resolve_source_identity_path(
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
                    .join(IDENTITY_FILE)
            )
        );
    }

    #[test]
    fn resolver_ignores_empty_inputs() {
        let empty = Path::new("");
        let home = Path::new("/home/user");
        assert_eq!(
            resolve_source_identity_path(
                Some(empty),
                Some(empty),
                Some(home),
                Some(empty),
                Some(empty),
                Platform::Unix,
            ),
            Some(
                home.join(".local/state")
                    .join(APP_DIRECTORY)
                    .join(IDENTITY_FILE)
            )
        );
    }

    #[test]
    fn missing_identity_is_created_once_and_reused() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/source-identity.json");
        let anchor_path = path.parent().unwrap().join(IDENTITY_ANCHOR_FILE);
        let store = SourceIdentityStore::at_path(path.clone());

        let first = store.load_or_create().unwrap();
        let second = store.load_or_create().unwrap();

        assert_eq!(first, second);
        assert_eq!(first.version(), SOURCE_IDENTITY_VERSION);
        assert_eq!(first.generation(), 1);
        assert!(first.node_id().as_str().parse::<NodeId>().is_ok());
        assert_eq!(store.load().unwrap(), first);
        assert!(fs::read_to_string(path).unwrap().ends_with('\n'));
        read_identity_anchor(&anchor_path).unwrap();
        assert!(fs::read_to_string(anchor_path).unwrap().ends_with('\n'));
        let debug = format!("{first:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&first.project_key_secret.0));
    }

    #[test]
    fn existing_pre_anchor_identity_bootstraps_without_changing_identity() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state").join(IDENTITY_FILE);
        let anchor_path = path.parent().unwrap().join(IDENTITY_ANCHOR_FILE);
        let original = SourceIdentity::generate(7, None).unwrap();
        write_private_test_file(&path, &serialize_identity(&original).unwrap());
        assert!(!anchor_path.exists());

        let loaded = SourceIdentityStore::at_path(path.clone())
            .load_or_create()
            .unwrap();

        assert_eq!(loaded, original);
        assert_eq!(read_identity(&path).unwrap(), original);
        read_identity_anchor(&anchor_path).unwrap();
    }

    #[test]
    fn deleting_an_initialized_identity_never_recreates_generation_one() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state").join(IDENTITY_FILE);
        let anchor_path = path.parent().unwrap().join(IDENTITY_ANCHOR_FILE);
        let store = SourceIdentityStore::at_path(path.clone());
        store.load_or_create().unwrap();
        fs::remove_file(&path).unwrap();

        let error = store.load_or_create().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("explicit identity-repair workflow")
        );
        assert!(!path.exists());
        read_identity_anchor(&anchor_path).unwrap();
    }

    #[test]
    fn anchor_only_crash_window_fails_closed_on_the_next_start() {
        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        create_private_directory(&state_directory).unwrap();
        let path = state_directory.join(IDENTITY_FILE);
        let anchor_path = state_directory.join(IDENTITY_ANCHOR_FILE);
        assert!(create_identity_anchor_atomically(&anchor_path).unwrap());

        let error = SourceIdentityStore::at_path(path.clone())
            .load_or_create()
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("explicit identity-repair workflow")
        );
        assert!(!path.exists());
        read_identity_anchor(&anchor_path).unwrap();
    }

    #[test]
    fn load_or_create_atomically_upgrades_only_a_strict_v1_identity() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(IDENTITY_FILE);
        let legacy = format!(r#"{{"version":1,"nodeId":"{VALID_NODE_ID}","generation":7}}"#);
        write_private_test_file(&path, legacy.as_bytes());
        let store = SourceIdentityStore::at_path(path.clone());

        assert_eq!(store.load().unwrap_err().kind(), io::ErrorKind::InvalidData);
        let upgraded = store.load_or_create().unwrap();
        assert_eq!(upgraded.version(), SOURCE_IDENTITY_VERSION);
        assert_eq!(upgraded.node_id().as_str(), VALID_NODE_ID);
        assert_eq!(upgraded.generation(), 7);
        assert_eq!(store.load().unwrap(), upgraded);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted["version"], SOURCE_IDENTITY_VERSION);
        assert_eq!(persisted["projectKeySecret"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn concurrent_first_use_converges_on_one_identity() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state/source-identity.json");
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();

        for _ in 0..8 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                SourceIdentityStore::at_path(path).load_or_create().unwrap()
            }));
        }

        let identities = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(identities.iter().all(|identity| identity == &identities[0]));
    }

    #[test]
    fn create_only_publish_never_overwrites_a_racing_winner() {
        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        create_private_directory(&state_directory).unwrap();
        let path = state_directory.join(IDENTITY_FILE);
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();

        for generation in 1..=8 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                let candidate = SourceIdentity::generate(generation, None).unwrap();
                barrier.wait();
                let published = create_identity_atomically(&path, &candidate).unwrap();
                (candidate, published)
            }));
        }

        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        let winners = outcomes
            .iter()
            .filter(|(_, published)| *published)
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1);
        assert_eq!(read_identity(&path).unwrap(), winners[0].0);
    }

    #[test]
    fn rotate_changes_node_id_and_increments_generation_atomically() {
        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        let path = state_directory.join(IDENTITY_FILE);
        let store = SourceIdentityStore::at_path(path.clone());
        let original = store.load_or_create().unwrap();

        let rotated = store.rotate().unwrap();

        assert_ne!(rotated.node_id(), original.node_id());
        assert_ne!(rotated.project_key_secret(), original.project_key_secret());
        assert_eq!(rotated.generation(), original.generation() + 1);
        assert_eq!(store.load().unwrap(), rotated);
        assert!(fs::read_dir(state_directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn rotate_bootstraps_anchor_for_an_existing_pre_anchor_identity() {
        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        let path = state_directory.join(IDENTITY_FILE);
        let anchor_path = state_directory.join(IDENTITY_ANCHOR_FILE);
        let original = SourceIdentity::generate(9, None).unwrap();
        write_private_test_file(&path, &serialize_identity(&original).unwrap());

        let rotated = SourceIdentityStore::at_path(path.clone()).rotate().unwrap();

        assert_eq!(rotated.generation(), 10);
        assert_ne!(rotated.node_id(), original.node_id());
        read_identity_anchor(&anchor_path).unwrap();
        assert_eq!(read_identity(&path).unwrap(), rotated);
    }

    #[test]
    fn rotate_requires_an_existing_valid_identity() {
        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        let path = state_directory.join(IDENTITY_FILE);
        let error = SourceIdentityStore::at_path(path.clone())
            .rotate()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!path.exists());
        assert!(!state_directory.join(IDENTITY_ANCHOR_FILE).exists());
    }

    #[test]
    fn rotate_does_not_rebuild_an_identity_from_an_anchor_only_state() {
        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        create_private_directory(&state_directory).unwrap();
        let path = state_directory.join(IDENTITY_FILE);
        let anchor_path = state_directory.join(IDENTITY_ANCHOR_FILE);
        assert!(create_identity_anchor_atomically(&anchor_path).unwrap());

        let error = SourceIdentityStore::at_path(path.clone())
            .rotate()
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!path.exists());
        read_identity_anchor(&anchor_path).unwrap();
    }

    #[test]
    fn generation_overflow_fails_closed_without_replacement() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(IDENTITY_FILE);
        let identity = SourceIdentity {
            version: SOURCE_IDENTITY_VERSION,
            node_id: VALID_NODE_ID.parse().unwrap(),
            generation: u64::MAX,
            project_key_secret: ProjectKeySecret("ab".repeat(32)),
        };
        write_private_test_file(&path, &serialize_identity(&identity).unwrap());
        let original = fs::read(&path).unwrap();

        assert_eq!(
            SourceIdentityStore::at_path(path.clone())
                .rotate()
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn corrupt_identity_fails_closed_without_replacement() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(IDENTITY_FILE);
        write_private_test_file(&path, b"{ definitely-not-json\n");
        let original = fs::read(&path).unwrap();
        let store = SourceIdentityStore::at_path(path.clone());

        assert_eq!(
            store.load_or_create().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.rotate().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn document_schema_is_strict() {
        for contents in [
            format!(r#"{{"version":2,"nodeId":"{VALID_NODE_ID}","generation":1}}"#),
            format!(r#"{{"version":1,"nodeId":"{VALID_NODE_ID}","generation":0}}"#),
            format!(r#"{{"version":1,"nodeId":"{VALID_NODE_ID}","generation":1,"extra":true}}"#),
            r#"{"version":1,"nodeId":"node-ABCDEFABCDEFABCDEFABCDEFABCDEFAB","generation":1}"#
                .to_string(),
        ] {
            let directory = tempdir().unwrap();
            let path = directory.path().join(IDENTITY_FILE);
            write_private_test_file(&path, contents.as_bytes());
            assert_eq!(
                SourceIdentityStore::at_path(path)
                    .load()
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn identity_anchor_schema_is_strict_and_bounded() {
        for contents in [
            br#"{"version":2}"#.as_slice(),
            br#"{"version":1,"extra":true}"#.as_slice(),
            b"not-json".as_slice(),
        ] {
            let directory = tempdir().unwrap();
            let state_directory = directory.path().join("state");
            let path = state_directory.join(IDENTITY_FILE);
            let identity = SourceIdentity::generate(1, None).unwrap();
            write_private_test_file(&path, &serialize_identity(&identity).unwrap());
            write_private_test_file(&state_directory.join(IDENTITY_ANCHOR_FILE), contents);

            let error = SourceIdentityStore::at_path(path.clone())
                .load_or_create()
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(read_identity(&path).unwrap(), identity);
        }

        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        let path = state_directory.join(IDENTITY_FILE);
        let identity = SourceIdentity::generate(1, None).unwrap();
        write_private_test_file(&path, &serialize_identity(&identity).unwrap());
        let oversized = [b' '; (MAX_IDENTITY_ANCHOR_BYTES + 1) as usize];
        write_private_test_file(&state_directory.join(IDENTITY_ANCHOR_FILE), &oversized);
        let error = SourceIdentityStore::at_path(path)
            .load_or_create()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn node_id_parser_accepts_only_the_canonical_form() {
        assert_eq!(
            VALID_NODE_ID.parse::<NodeId>().unwrap().as_str(),
            VALID_NODE_ID
        );
        for invalid in [
            "0123456789abcdef0123456789abcdef",
            "node-0123456789ABCDEF0123456789abcdef",
            "node-0123456789abcdef0123456789abcdeg",
            "node-00000000000000000000000000000000",
            "node-0123",
        ] {
            assert!(invalid.parse::<NodeId>().is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn stable_file_identity_requires_volume_and_index() {
        assert_eq!(
            require_stable_file_identity(Some(7), Some(11), "test file").unwrap(),
            (7, 11)
        );
        for (volume, index) in [(None, Some(11)), (Some(7), None), (None, None)] {
            let error = require_stable_file_identity(volume, index, "test file").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("stable file identity"));
        }
    }

    #[test]
    fn windows_private_acl_policy_accepts_only_user_and_system_grants() {
        let entries = [
            WindowsAclEntryPolicy {
                ace_type: WINDOWS_ACCESS_ALLOWED_ACE_TYPE,
                mask: u32::MAX,
                trustee: Some(WindowsAclTrustee::CurrentUser),
            },
            WindowsAclEntryPolicy {
                ace_type: WINDOWS_ACCESS_ALLOWED_ACE_TYPE,
                mask: u32::MAX,
                trustee: Some(WindowsAclTrustee::LocalSystem),
            },
            WindowsAclEntryPolicy {
                ace_type: WINDOWS_ACCESS_ALLOWED_ACE_TYPE,
                mask: u32::MAX,
                trustee: Some(WindowsAclTrustee::Administrators),
            },
            WindowsAclEntryPolicy {
                ace_type: WINDOWS_ACCESS_DENIED_ACE_TYPE,
                mask: u32::MAX,
                trustee: Some(WindowsAclTrustee::Other),
            },
        ];
        validate_windows_private_acl_policy(true, true, &entries, "test path").unwrap();
    }

    #[test]
    fn windows_private_acl_policy_fails_closed() {
        for (owner_is_current, dacl_present, entries) in [
            (false, true, Vec::new()),
            (true, false, Vec::new()),
            (
                true,
                true,
                vec![WindowsAclEntryPolicy {
                    ace_type: WINDOWS_ACCESS_ALLOWED_ACE_TYPE,
                    mask: 1,
                    trustee: Some(WindowsAclTrustee::Other),
                }],
            ),
            (
                true,
                true,
                vec![WindowsAclEntryPolicy {
                    ace_type: WINDOWS_ACCESS_ALLOWED_OBJECT_ACE_TYPE,
                    mask: 0,
                    trustee: None,
                }],
            ),
            (
                true,
                true,
                vec![WindowsAclEntryPolicy {
                    ace_type: u8::MAX,
                    mask: 0,
                    trustee: None,
                }],
            ),
        ] {
            let error = validate_windows_private_acl_policy(
                owner_is_current,
                dacl_present,
                &entries,
                "test path",
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn windows_reparse_attribute_is_rejected_independently_of_symlink_type() {
        assert!(!windows_attributes_are_reparse(0));
        assert!(windows_attributes_are_reparse(
            WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(windows_attributes_are_reparse(
            WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT | 0x10
        ));
    }

    #[test]
    fn windows_stable_lock_share_mode_excludes_delete() {
        const FILE_SHARE_READ_FOR_TEST: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE_FOR_TEST: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE_FOR_TEST: u32 = 0x0000_0004;

        let mode = stable_lock_share_mode();
        assert_eq!(
            mode & (FILE_SHARE_READ_FOR_TEST | FILE_SHARE_WRITE_FOR_TEST),
            FILE_SHARE_READ_FOR_TEST | FILE_SHARE_WRITE_FOR_TEST
        );
        assert_eq!(mode & FILE_SHARE_DELETE_FOR_TEST, 0);
    }

    #[cfg(unix)]
    #[test]
    fn waiter_rejects_a_lock_inode_replaced_while_it_was_blocked() {
        use std::sync::mpsc;
        use std::time::Duration;

        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("private");
        let store = SourceIdentityStore::at_path(state_directory.join(IDENTITY_FILE));
        store.load_or_create().unwrap();

        let holder = open_lock_file(&state_directory).unwrap();
        fs2::FileExt::lock_exclusive(&holder).unwrap();

        let (opened_sender, opened_receiver) = mpsc::channel();
        let waiter_directory = state_directory.clone();
        let waiter = thread::spawn(move || {
            let opened = open_lock_file(&waiter_directory).unwrap();
            opened_sender.send(()).unwrap();
            lock_opened_lock_file(&waiter_directory, opened)
        });
        opened_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter did not open the original lock inode");

        let lock_path = state_directory.join(LOCK_FILE);
        fs::rename(
            &lock_path,
            state_directory.join("displaced-source-identity.lock"),
        )
        .unwrap();
        write_private_test_file(&lock_path, b"");
        drop(holder);

        let error = waiter.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed while"));

        // A fresh operation coordinates through the replacement and remains
        // usable; only the waiter holding the displaced inode is rejected.
        assert_eq!(store.load_or_create().unwrap(), store.load().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_identity_and_lock_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("private/nested");
        let path = state_directory.join(IDENTITY_FILE);
        SourceIdentityStore::at_path(path.clone())
            .load_or_create()
            .unwrap();

        assert_eq!(
            fs::metadata(&state_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(state_directory.join(LOCK_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(state_directory.join(IDENTITY_ANCHOR_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn permissive_or_non_regular_identity_anchor_fails_closed() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{PermissionsExt, symlink};

        for anchor_kind in ["permissive", "symlink", "fifo"] {
            let directory = tempdir().unwrap();
            let state_directory = directory.path().join("state");
            let path = state_directory.join(IDENTITY_FILE);
            let identity = SourceIdentity::generate(1, None).unwrap();
            write_private_test_file(&path, &serialize_identity(&identity).unwrap());
            let anchor_path = state_directory.join(IDENTITY_ANCHOR_FILE);
            match anchor_kind {
                "permissive" => {
                    write_private_test_file(&anchor_path, br#"{"version":1}"#);
                    fs::set_permissions(&anchor_path, fs::Permissions::from_mode(0o644)).unwrap();
                }
                "symlink" => {
                    let target = state_directory.join("anchor-target");
                    write_private_test_file(&target, br#"{"version":1}"#);
                    symlink(&target, &anchor_path).unwrap();
                }
                "fifo" => {
                    let path_bytes = CString::new(anchor_path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: path_bytes is a valid NUL-terminated path and
                    // mkfifo retains no pointer after returning.
                    assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);
                }
                _ => unreachable!(),
            }

            let error = SourceIdentityStore::at_path(path)
                .load_or_create()
                .unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::PermissionDenied
                ),
                "anchor_kind={anchor_kind}: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn permissive_identity_permissions_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let path = directory.path().join(IDENTITY_FILE);
        write_private_test_file(
            &path,
            format!(r#"{{"version":1,"nodeId":"{VALID_NODE_ID}","generation":1}}"#).as_bytes(),
        );
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();

        assert_eq!(
            SourceIdentityStore::at_path(path)
                .load_or_create()
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn permissive_state_directory_and_lock_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let state_directory = directory.path().join("state");
        fs::create_dir(&state_directory).unwrap();
        fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o755)).unwrap();
        let path = state_directory.join(IDENTITY_FILE);
        assert_eq!(
            SourceIdentityStore::at_path(path.clone())
                .load_or_create()
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!path.exists());

        fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let store = SourceIdentityStore::at_path(path);
        store.load_or_create().unwrap();
        let lock_path = state_directory.join(LOCK_FILE);
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            store.load_or_create().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_symlink_and_fifo_fail_without_following_or_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target.json");
        write_private_test_file(
            &target,
            format!(r#"{{"version":1,"nodeId":"{VALID_NODE_ID}","generation":1}}"#).as_bytes(),
        );
        let symlink_path = directory.path().join("identity-link.json");
        symlink(&target, &symlink_path).unwrap();
        assert_eq!(
            SourceIdentityStore::at_path(symlink_path)
                .load()
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let fifo_path = directory.path().join("identity.fifo");
        let fifo_path_bytes = CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_path_bytes is a live NUL-terminated path and the mode is valid.
        assert_eq!(unsafe { libc::mkfifo(fifo_path_bytes.as_ptr(), 0o600) }, 0);
        assert_eq!(
            SourceIdentityStore::at_path(fifo_path)
                .load()
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_symlink_and_fifo_fail_without_following_or_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        for use_fifo in [false, true] {
            let directory = tempdir().unwrap();
            let state_directory = directory.path().join("state");
            create_private_directory(&state_directory).unwrap();
            let lock_path = state_directory.join(LOCK_FILE);
            if use_fifo {
                let lock_path_bytes = CString::new(lock_path.as_os_str().as_bytes()).unwrap();
                // SAFETY: lock_path_bytes is a live NUL-terminated path and the mode is valid.
                assert_eq!(unsafe { libc::mkfifo(lock_path_bytes.as_ptr(), 0o600) }, 0);
            } else {
                let target = state_directory.join("lock-target");
                write_private_test_file(&target, b"");
                symlink(target, &lock_path).unwrap();
            }

            assert_eq!(
                SourceIdentityStore::at_path(state_directory.join(IDENTITY_FILE))
                    .load_or_create()
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    fn write_private_test_file(path: &Path, contents: &[u8]) {
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut directory_permissions = fs::metadata(parent).unwrap().permissions();
            directory_permissions.set_mode(0o700);
            fs::set_permissions(parent, directory_permissions).unwrap();
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(path, permissions).unwrap();
        }
    }
}
