use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::digest::Update;
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

use crate::source_identity::{NodeId, SourceIdentity};

const OBSERVED_PROJECT_KEY_PREFIX: &str = "opk-hmac-sha256-v1-";
const OBSERVED_PROJECT_KEY_DOMAIN: &[u8] = b"codex-usage-monit/observed-project-key/hmac/v1\0";
const THREAD_SHARD_KEY_PREFIX: &str = "tsk-sha256-v1-";
const THREAD_SHARD_KEY_DOMAIN: &[u8] = b"codex-usage-monit/thread-shard-key/v1\0";
const SHA256_HEX_LEN: usize = 64;

const PROJECT_INSTANCE_ID_PREFIX: &str = "project-instance-";
const LOGICAL_PROJECT_ID_PREFIX: &str = "logical-project-";
const RANDOM_ID_BYTES: usize = 16;
const RANDOM_ID_HEX_LEN: usize = RANDOM_ID_BYTES * 2;

const MAX_THREAD_ID_BYTES: usize = 128;
const MAX_PROJECT_LABEL_BYTES: usize = 512;
const MAX_PROJECT_LABEL_CHARS: usize = 160;

macro_rules! impl_string_identity {
    ($type:ty) => {
        impl AsRef<str> for $type {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

macro_rules! impl_validated_deserialize {
    ($type:ty) => {
        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

/// A source-scoped fingerprint of a canonical project path.
///
/// Only the SHA-256 digest is retained. The canonical path is never stored in
/// this value and therefore cannot leak through its `Debug` or serde forms.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ObservedProjectKey(String);

impl ObservedProjectKey {
    /// Derives the stable observed key for a source and canonical absolute path.
    ///
    /// Callers remain responsible for resolving filesystem aliases with
    /// `canonicalize` before calling this function. Requiring an absolute path
    /// here prevents accidental source-relative fingerprints.
    pub fn from_canonical_path(
        source_identity: &SourceIdentity,
        canonical_project_path: &Path,
    ) -> Result<Self, SourceModelError> {
        if !canonical_project_path.is_absolute() {
            return Err(SourceModelError(
                "observed project paths must be canonical absolute paths",
            ));
        }
        let normalized = canonical_project_path.components().collect::<PathBuf>();
        if normalized.as_os_str() != canonical_project_path.as_os_str()
            || canonical_project_path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(SourceModelError(
                "observed project paths must not contain dot components",
            ));
        }

        let secret = source_identity.project_key_secret();
        let mut hasher = Hmac::<Sha256>::new_from_slice(&secret)
            .expect("a 256-bit project-key secret is a valid HMAC key");
        Update::update(&mut hasher, OBSERVED_PROJECT_KEY_DOMAIN);
        update_length_prefixed(&mut hasher, source_identity.node_id().as_str().as_bytes());

        update_canonical_path(&mut hasher, canonical_project_path)?;
        let digest = hasher.finalize().into_bytes();

        let mut value = String::with_capacity(OBSERVED_PROJECT_KEY_PREFIX.len() + SHA256_HEX_LEN);
        value.push_str(OBSERVED_PROJECT_KEY_PREFIX);
        append_lower_hex(&mut value, &digest);
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl_string_identity!(ObservedProjectKey);

impl FromStr for ObservedProjectKey {
    type Err = SourceModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_hex(value, OBSERVED_PROJECT_KEY_PREFIX, SHA256_HEX_LEN, false)?;
        Ok(Self(value.to_owned()))
    }
}

impl_validated_deserialize!(ObservedProjectKey);

/// Center-generated stable ID for one physical project instance.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ProjectInstanceId(String);

impl ProjectInstanceId {
    /// Generates a new 128-bit opaque ID using the operating system CSPRNG.
    pub fn generate() -> io::Result<Self> {
        generate_random_id(PROJECT_INSTANCE_ID_PREFIX).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl_string_identity!(ProjectInstanceId);

impl FromStr for ProjectInstanceId {
    type Err = SourceModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_hex(value, PROJECT_INSTANCE_ID_PREFIX, RANDOM_ID_HEX_LEN, true)?;
        Ok(Self(value.to_owned()))
    }
}

impl_validated_deserialize!(ProjectInstanceId);

/// Center-generated stable ID for a user-controlled logical project mapping.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct LogicalProjectId(String);

impl LogicalProjectId {
    /// Generates a new 128-bit opaque ID using the operating system CSPRNG.
    pub fn generate() -> io::Result<Self> {
        generate_random_id(LOGICAL_PROJECT_ID_PREFIX).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl_string_identity!(LogicalProjectId);

impl FromStr for LogicalProjectId {
    type Err = SourceModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_hex(value, LOGICAL_PROJECT_ID_PREFIX, RANDOM_ID_HEX_LEN, true)?;
        Ok(Self(value.to_owned()))
    }
}

impl_validated_deserialize!(LogicalProjectId);

/// Bounded, wire-safe opaque Codex thread ID used in remote protocol keys.
///
/// The current Codex UUID/ULID-like IDs fit this alphabet. Keeping this type
/// narrower than arbitrary text keeps protocol values terminal-safe. Filesystem
/// layouts must use [`ThreadShardKey`] rather than a raw thread ID.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl_string_identity!(ThreadId);

impl FromStr for ThreadId {
    type Err = SourceModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > MAX_THREAD_ID_BYTES {
            return Err(SourceModelError("thread ID has an invalid length"));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(SourceModelError(
                "thread ID contains unsafe protocol characters",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl_validated_deserialize!(ThreadId);

/// Canonical, source-scoped filesystem key for one physical thread replica.
///
/// Raw protocol thread IDs are never used as path components, avoiding Windows
/// reserved-name and case-folding collisions without narrowing their wire form.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ThreadShardKey(String);

impl ThreadShardKey {
    pub fn from_replica(replica: &SessionReplicaKey) -> Self {
        let mut hasher = Sha256::new();
        Digest::update(&mut hasher, THREAD_SHARD_KEY_DOMAIN);
        update_length_prefixed(&mut hasher, replica.source_id().as_str().as_bytes());
        update_length_prefixed(&mut hasher, replica.thread_id().as_str().as_bytes());
        let digest = hasher.finalize();
        let mut value = String::with_capacity(THREAD_SHARD_KEY_PREFIX.len() + SHA256_HEX_LEN);
        value.push_str(THREAD_SHARD_KEY_PREFIX);
        append_lower_hex(&mut value, &digest);
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl_string_identity!(ThreadShardKey);

impl FromStr for ThreadShardKey {
    type Err = SourceModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_hex(value, THREAD_SHARD_KEY_PREFIX, SHA256_HEX_LEN, false)?;
        Ok(Self(value.to_owned()))
    }
}

impl_validated_deserialize!(ThreadShardKey);

/// Physical session identity. Identical thread IDs on different sources remain
/// distinct replicas until the logical-session deduper evaluates their facts.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionReplicaKey {
    source_id: NodeId,
    thread_id: ThreadId,
}

impl SessionReplicaKey {
    pub fn new(source_id: NodeId, thread_id: ThreadId) -> Self {
        Self {
            source_id,
            thread_id,
        }
    }

    pub fn source_id(&self) -> &NodeId {
        &self.source_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }
}

/// Sanitized display-only project label suitable for remote descriptors.
///
/// Labels are deliberately never accepted as path components or identity.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ProjectDisplayLabel(String);

impl ProjectDisplayLabel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl_string_identity!(ProjectDisplayLabel);

impl FromStr for ProjectDisplayLabel {
    type Err = SourceModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.trim() != value
            || value.len() > MAX_PROJECT_LABEL_BYTES
            || value.chars().count() > MAX_PROJECT_LABEL_CHARS
            || matches!(value, "." | "..")
            || value.chars().any(|character| {
                character.is_control()
                    || is_bidi_control(character)
                    || matches!(character, '\u{2028}' | '\u{2029}' | '/' | '\\')
            })
        {
            return Err(SourceModelError("project display label is invalid"));
        }
        Ok(Self(value.to_owned()))
    }
}

impl_validated_deserialize!(ProjectDisplayLabel);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceModelError(&'static str);

impl fmt::Display for SourceModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for SourceModelError {}

fn update_length_prefixed(hasher: &mut impl Update, bytes: &[u8]) {
    Update::update(hasher, &(bytes.len() as u64).to_be_bytes());
    Update::update(hasher, bytes);
}

#[cfg(unix)]
fn update_canonical_path(hasher: &mut impl Update, path: &Path) -> Result<(), SourceModelError> {
    // Unix paths are arbitrary bytes. This is an explicit durable encoding;
    // unlike OsStr::as_encoded_bytes it is stable across Rust versions.
    Update::update(hasher, b"unix-raw\0");
    update_length_prefixed(hasher, path.as_os_str().as_bytes());
    Ok(())
}

#[cfg(windows)]
fn update_canonical_path(hasher: &mut impl Update, path: &Path) -> Result<(), SourceModelError> {
    // UTF-16LE is explicit and preserves Windows' potentially unpaired WTF-16
    // code units. Canonicalization/case resolution remains the caller's job.
    Update::update(hasher, b"windows-utf16le\0");
    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    Update::update(hasher, &((units.len() as u64) * 2).to_be_bytes());
    for unit in units {
        Update::update(hasher, &unit.to_le_bytes());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn update_canonical_path(hasher: &mut impl Update, path: &Path) -> Result<(), SourceModelError> {
    // Other targets have no stable public OsStr byte contract, so fail closed
    // for non-UTF-8 paths instead of persisting compiler-specific bytes.
    let path = path.to_str().ok_or(SourceModelError(
        "canonical project path encoding is unsupported on this platform",
    ))?;
    Update::update(hasher, b"portable-utf8\0");
    update_length_prefixed(hasher, path.as_bytes());
    Ok(())
}

fn generate_random_id(prefix: &str) -> io::Result<String> {
    for _ in 0..8 {
        let mut random = [0_u8; RANDOM_ID_BYTES];
        getrandom::fill(&mut random)
            .map_err(|error| io::Error::other(format!("could not generate opaque ID: {error}")))?;
        if random.iter().all(|byte| *byte == 0) {
            continue;
        }

        let mut value = String::with_capacity(prefix.len() + RANDOM_ID_HEX_LEN);
        value.push_str(prefix);
        append_lower_hex(&mut value, &random);
        return Ok(value);
    }

    Err(io::Error::other(
        "secure random provider repeatedly returned an unusable opaque ID",
    ))
}

fn validate_prefixed_hex(
    value: &str,
    prefix: &str,
    hex_len: usize,
    reject_zero: bool,
) -> Result<(), SourceModelError> {
    if value.len() != prefix.len() + hex_len {
        return Err(SourceModelError("opaque ID has the wrong length"));
    }
    let Some(hex) = value.strip_prefix(prefix) else {
        return Err(SourceModelError("opaque ID has the wrong prefix"));
    };
    if !hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SourceModelError(
            "opaque ID must use lowercase hexadecimal characters",
        ));
    }
    if reject_zero && hex.bytes().all(|byte| byte == b'0') {
        return Err(SourceModelError("opaque ID must not be all zeroes"));
    }
    Ok(())
}

fn append_lower_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    #[cfg(unix)]
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn node(hex: char) -> NodeId {
        format!("node-{}", hex.to_string().repeat(32))
            .parse()
            .unwrap()
    }

    fn identity(hex: char) -> SourceIdentity {
        SourceIdentity::from_test_parts(node(hex), &"ab".repeat(32))
    }

    #[test]
    fn observed_project_key_is_stable_source_scoped_and_opaque() {
        let path = if cfg!(windows) {
            Path::new(r"C:\Users\alice\secret-project")
        } else {
            Path::new("/home/alice/secret-project")
        };
        let first_identity = identity('1');
        let second_identity = identity('2');
        let first = ObservedProjectKey::from_canonical_path(&first_identity, path).unwrap();
        let repeated = ObservedProjectKey::from_canonical_path(&first_identity, path).unwrap();
        let another_source =
            ObservedProjectKey::from_canonical_path(&second_identity, path).unwrap();
        let another_path = if cfg!(windows) {
            Path::new(r"C:\Users\alice\another-project")
        } else {
            Path::new("/home/alice/another-project")
        };
        let another_project =
            ObservedProjectKey::from_canonical_path(&first_identity, another_path).unwrap();

        assert_eq!(first, repeated);
        assert_ne!(first, another_source);
        assert_ne!(first, another_project);
        assert!(first.as_str().starts_with(OBSERVED_PROJECT_KEY_PREFIX));
        assert!(!first.as_str().contains("alice"));
        assert!(
            !serde_json::to_string(&first)
                .unwrap()
                .contains("secret-project")
        );
        #[cfg(unix)]
        assert_eq!(
            first.as_str(),
            "opk-hmac-sha256-v1-952fa16d7461b14fa301471a36bb42f4b697af6660e305714002a5308f58a51e"
        );
        #[cfg(windows)]
        assert_eq!(
            first.as_str(),
            "opk-hmac-sha256-v1-62d41455ea8ea7fcb52215264f104ea9fe131496be242e8625fc49a4d2d32744"
        );
        assert_eq!(first, first.as_str().parse().unwrap());
    }

    #[test]
    fn observed_project_key_rejects_noncanonical_relative_input() {
        let identity = identity('1');
        assert!(ObservedProjectKey::from_canonical_path(&identity, Path::new("project")).is_err());

        let path = if cfg!(windows) {
            Path::new(r"C:\safe\..\project")
        } else {
            Path::new("/safe/../project")
        };
        assert!(ObservedProjectKey::from_canonical_path(&identity, path).is_err());

        let path = if cfg!(windows) {
            Path::new(r"C:\safe\.\project")
        } else {
            Path::new("/safe/./project")
        };
        assert!(ObservedProjectKey::from_canonical_path(&identity, path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn observed_project_key_preserves_non_utf8_unix_path_bytes() {
        let first_path = Path::new(OsStr::from_bytes(b"/srv/project-\xff"));
        let second_path = Path::new(OsStr::from_bytes(b"/srv/project-\xfe"));
        let identity = identity('1');
        let first = ObservedProjectKey::from_canonical_path(&identity, first_path).unwrap();
        let repeated = ObservedProjectKey::from_canonical_path(&identity, first_path).unwrap();
        let second = ObservedProjectKey::from_canonical_path(&identity, second_path).unwrap();

        assert_eq!(first, repeated);
        assert_ne!(first, second);
    }

    #[cfg(windows)]
    #[test]
    fn observed_project_key_preserves_windows_utf16_and_exact_case() {
        let mixed_case = Path::new(r"C:\Users\Alice\项目");
        let lower_case = Path::new(r"C:\users\alice\项目");
        let identity = identity('1');
        let first = ObservedProjectKey::from_canonical_path(&identity, mixed_case).unwrap();
        let repeated = ObservedProjectKey::from_canonical_path(&identity, mixed_case).unwrap();
        let differently_encoded =
            ObservedProjectKey::from_canonical_path(&identity, lower_case).unwrap();

        assert_eq!(first, repeated);
        assert_ne!(first, differently_encoded);
    }

    #[test]
    fn opaque_project_ids_are_random_canonical_and_path_safe() {
        let first_instance = ProjectInstanceId::generate().unwrap();
        let second_instance = ProjectInstanceId::generate().unwrap();
        let logical = LogicalProjectId::generate().unwrap();

        assert_ne!(first_instance, second_instance);
        assert_eq!(first_instance, first_instance.as_str().parse().unwrap());
        assert_eq!(logical, logical.as_str().parse().unwrap());
        for value in [first_instance.as_str(), logical.as_str()] {
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            );
            assert!(!matches!(value, "." | ".."));
            assert!(!value.contains('/') && !value.contains('\\'));
        }
    }

    #[test]
    fn opaque_project_ids_reject_noncanonical_forms() {
        let valid_instance = format!("{PROJECT_INSTANCE_ID_PREFIX}{}", "a".repeat(32));
        let valid_logical = format!("{LOGICAL_PROJECT_ID_PREFIX}{}", "b".repeat(32));
        assert!(valid_instance.parse::<ProjectInstanceId>().is_ok());
        assert!(valid_logical.parse::<LogicalProjectId>().is_ok());

        for invalid in [
            "project-instance-abc".to_string(),
            format!("{PROJECT_INSTANCE_ID_PREFIX}{}", "A".repeat(32)),
            format!("{PROJECT_INSTANCE_ID_PREFIX}{}", "0".repeat(32)),
            format!("logical-project-{}", "g".repeat(32)),
            format!("logical-project/{}", "a".repeat(32)),
        ] {
            assert!(invalid.parse::<ProjectInstanceId>().is_err());
            assert!(invalid.parse::<LogicalProjectId>().is_err());
        }
    }

    #[test]
    fn validated_serde_rejects_malformed_identifiers() {
        assert!(serde_json::from_value::<ObservedProjectKey>(json!("../../secret")).is_err());
        assert!(
            serde_json::from_value::<ProjectInstanceId>(json!(format!(
                "{PROJECT_INSTANCE_ID_PREFIX}{}",
                "A".repeat(32)
            )))
            .is_err()
        );
        assert!(serde_json::from_value::<ThreadId>(json!("../thread")).is_err());
    }

    #[test]
    fn thread_id_accepts_current_ids_and_rejects_path_or_terminal_controls() {
        let current = "01a00b37-eb69-7f23-9c43-03cba436f012"
            .parse::<ThreadId>()
            .unwrap();
        assert_eq!(current.as_str(), "01a00b37-eb69-7f23-9c43-03cba436f012");

        for invalid in [
            "",
            ".",
            "..",
            "../thread",
            r"..\thread",
            "thread id",
            "thread\nspoof",
            "thread\u{202e}spoof",
            "é",
        ] {
            assert!(invalid.parse::<ThreadId>().is_err(), "accepted {invalid:?}");
        }
        assert!("a".repeat(MAX_THREAD_ID_BYTES).parse::<ThreadId>().is_ok());
        assert!(
            "a".repeat(MAX_THREAD_ID_BYTES + 1)
                .parse::<ThreadId>()
                .is_err()
        );
    }

    #[test]
    fn thread_shard_keys_are_source_scoped_and_windows_path_safe() {
        let lower = SessionReplicaKey::new(node('1'), "con".parse().unwrap());
        let upper = SessionReplicaKey::new(node('1'), "CON".parse().unwrap());
        let another_source = SessionReplicaKey::new(node('2'), "con".parse().unwrap());

        let lower_key = ThreadShardKey::from_replica(&lower);
        let upper_key = ThreadShardKey::from_replica(&upper);
        let another_source_key = ThreadShardKey::from_replica(&another_source);

        assert_ne!(lower_key, upper_key);
        assert_ne!(lower_key, another_source_key);
        assert_eq!(lower_key, lower_key.as_str().parse().unwrap());
        for key in [lower_key, upper_key, another_source_key] {
            assert!(key.as_str().starts_with(THREAD_SHARD_KEY_PREFIX));
            assert!(key.as_str().bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
            }));
            assert!(!matches!(key.as_str(), "CON" | "NUL" | "COM1"));
        }
    }

    #[test]
    fn session_replica_key_keeps_same_thread_separate_by_source() {
        let thread_id = "01a00b37-eb69-7f23-9c43-03cba436f012"
            .parse::<ThreadId>()
            .unwrap();
        let local = SessionReplicaKey::new(node('1'), thread_id.clone());
        let remote = SessionReplicaKey::new(node('2'), thread_id);

        assert_ne!(local, remote);
        assert_eq!(local.thread_id(), remote.thread_id());
        assert_ne!(local.source_id(), remote.source_id());

        let encoded = serde_json::to_value(&local).unwrap();
        assert_eq!(encoded["sourceId"], local.source_id().as_str());
        assert_eq!(encoded["threadId"], local.thread_id().as_str());
        assert_eq!(
            serde_json::from_value::<SessionReplicaKey>(encoded).unwrap(),
            local
        );
    }

    #[test]
    fn session_replica_key_rejects_unknown_or_malformed_fields() {
        let valid_source = node('1').to_string();
        assert!(
            serde_json::from_value::<SessionReplicaKey>(json!({
                "sourceId": valid_source,
                "threadId": "../thread"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SessionReplicaKey>(json!({
                "sourceId": node('1').to_string(),
                "threadId": "thread",
                "path": "/secret"
            }))
            .is_err()
        );
    }

    #[test]
    fn project_display_label_is_bounded_and_terminal_safe() {
        let label = "用量监控 project".parse::<ProjectDisplayLabel>().unwrap();
        assert_eq!(label.as_str(), "用量监控 project");
        assert_eq!(
            serde_json::from_value::<ProjectDisplayLabel>(serde_json::to_value(&label).unwrap())
                .unwrap(),
            label
        );

        for invalid in [
            "",
            " ",
            " leading",
            "trailing ",
            ".",
            "..",
            "project/name",
            r"project\name",
            "project\nname",
            "project\u{202e}name",
        ] {
            assert!(
                invalid.parse::<ProjectDisplayLabel>().is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(
            "a".repeat(MAX_PROJECT_LABEL_CHARS)
                .parse::<ProjectDisplayLabel>()
                .is_ok()
        );
        assert!(
            "a".repeat(MAX_PROJECT_LABEL_CHARS + 1)
                .parse::<ProjectDisplayLabel>()
                .is_err()
        );
    }
}
