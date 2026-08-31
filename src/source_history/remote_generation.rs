//! Generation-scoped bucket and session-digest persistence for SSH sources.
//!
//! A bootstrap is written into an explicit, initially invisible generation.
//! Readers resolve both data families through one active manifest, so a
//! multi-page bootstrap can never expose a half-populated replacement or keep
//! stale rows from the preceding generation alive after activation.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use super::session_evidence::{
    DIGESTS_DIRECTORY, DIGESTS_LOCK_FILE, MAX_COMPRESSED_EVIDENCE_SHARD_BYTES,
    validate_digest_shard_for_remote_clone,
};
use super::*;
use crate::remote_protocol::{ProtocolRevisions, SourceGeneration};

const REMOTE_HISTORY_DIRECTORY: &str = "remote-history-v1";
const REMOTE_GENERATIONS_DIRECTORY: &str = "generations";
const REMOTE_GENERATION_METADATA_FILE: &str = "generation.json";
const REMOTE_ACTIVE_MANIFEST_FILE: &str = "active.json";
const REMOTE_HISTORY_LOCK_FILE: &str = "remote-history.lock";
const REMOTE_GC_TRASH_DIRECTORY: &str = "gc-trash";
const REMOTE_GC_TRASH_PREFIX: &str = "retired-";
const REMOTE_GC_TRASH_SUFFIX: &str = ".trash";
const REMOTE_GENERATION_FORMAT_VERSION: u32 = 3;
const REMOTE_ACTIVE_MANIFEST_FORMAT_VERSION: u32 = 2;
const MAX_REMOTE_GENERATION_FILE_BYTES: u64 = 64 * 1024;
const MAX_REMOTE_BINDING_BYTES: u64 = 4 * 1024;
const MAX_REMOTE_COW_FINGERPRINT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_REMOTE_CLONE_SHARDS_PER_FAMILY: usize = 64;
const MAX_REMOTE_CLONE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_REMOTE_HISTORY_GENERATIONS: usize = 32;
const REMOTE_GENERATION_PREFIX: &str = "ingest-gen-";
const REMOTE_GENERATION_HEX_LEN: usize = 32;
const REMOTE_PAGE_FINGERPRINT_PREFIX: &str = "remote-page-sha256-v1-";
const MAX_REMOTE_GC_TREE_ENTRIES: u64 = 256;
const MAX_REMOTE_GC_TREE_BYTES: u64 = 1024 * 1024 * 1024;

/// Opaque, path-safe center-owned identity for one SSH history generation.
///
/// This intentionally uses the same wire representation as the center ingest
/// state without making source-history persistence depend on that higher-level
/// state machine. Callers can bridge the two using their validated string form.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SourceHistoryRemoteGenerationId(String);

impl SourceHistoryRemoteGenerationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> io::Result<()> {
        self.as_str()
            .parse::<Self>()
            .map(|_| ())
            .map_err(|error| invalid_data(error.to_string()))
    }
}

impl fmt::Display for SourceHistoryRemoteGenerationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SourceHistoryRemoteGenerationId {
    type Err = SourceHistoryRemoteGenerationIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(hex) = value.strip_prefix(REMOTE_GENERATION_PREFIX) else {
            return Err(SourceHistoryRemoteGenerationIdParseError);
        };
        if hex.len() != REMOTE_GENERATION_HEX_LEN
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            || hex.bytes().all(|byte| byte == b'0')
        {
            return Err(SourceHistoryRemoteGenerationIdParseError);
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for SourceHistoryRemoteGenerationId {
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
pub struct SourceHistoryRemoteGenerationIdParseError;

impl fmt::Display for SourceHistoryRemoteGenerationIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote history generation ID is invalid")
    }
}

impl std::error::Error for SourceHistoryRemoteGenerationIdParseError {}

/// Exact exporter identity and data-domain revisions represented by one
/// source-history generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceHistoryRemoteBinding {
    source: SourceGeneration,
    revisions: ProtocolRevisions,
}

impl SourceHistoryRemoteBinding {
    pub fn new(source: SourceGeneration, revisions: ProtocolRevisions) -> io::Result<Self> {
        let binding = Self { source, revisions };
        binding.validate()?;
        Ok(binding)
    }

    pub fn source(&self) -> &SourceGeneration {
        &self.source
    }

    pub fn revisions(&self) -> &ProtocolRevisions {
        &self.revisions
    }

    fn validate(&self) -> io::Result<()> {
        // NonZero fields and NodeId's validated deserializer enforce the wire
        // value bounds. A serialization bound keeps future fields from
        // bypassing the persistence acceptance gate.
        encode_pretty_bounded(self, MAX_REMOTE_BINDING_BYTES).map(|_| ())
    }

    pub(super) fn validate_namespace(&self, source_id: &NodeId) -> io::Result<()> {
        self.validate()?;
        if &self.source.node_id != source_id {
            return Err(invalid_data(
                "remote history binding source does not match its namespace",
            ));
        }
        Ok(())
    }
}

/// Atomic reader-visible generation selection, including its exact exporter
/// binding. Cursors and WAL pages must compare this whole value, never the
/// center-owned generation ID alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceHistoryRemoteActiveRef {
    generation: SourceHistoryRemoteGenerationId,
    binding: SourceHistoryRemoteBinding,
}

impl SourceHistoryRemoteActiveRef {
    pub fn new(
        generation: SourceHistoryRemoteGenerationId,
        binding: SourceHistoryRemoteBinding,
    ) -> io::Result<Self> {
        generation.validate()?;
        binding.validate()?;
        Ok(Self {
            generation,
            binding,
        })
    }

    pub fn generation(&self) -> &SourceHistoryRemoteGenerationId {
        &self.generation
    }

    pub fn binding(&self) -> &SourceHistoryRemoteBinding {
        &self.binding
    }

    fn validate_namespace(&self, source_id: &NodeId) -> io::Result<()> {
        self.generation.validate()?;
        self.binding.validate_namespace(source_id)
    }
}

/// One reader-consistent view of both history families for an SSH source.
///
/// The active manifest and both record families are resolved while the same
/// shared remote-history root lock is held. This prevents a manifest switch
/// from combining bucket records from one generation with session digests
/// from another generation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceHistoryRemoteSnapshot {
    pub active_ref: Option<SourceHistoryRemoteActiveRef>,
    pub bucket_records: Vec<SourceBucketRecord>,
    pub session_digest_records: Vec<SourceSessionDigestRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteHistoryPageWriteReport {
    pub bucket_history: SourceHistoryWriteReport,
    pub session_digests: SourceHistoryWriteReport,
}

/// Result of an explicitly authorized remote-generation cleanup attempt.
///
/// This primitive does not decide retention policy. Its caller must provide
/// the exact candidate and the complete generation protection set obtained
/// from ingest state while holding the higher-level source ingest lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteHistoryGenerationGcOutcome {
    Deleted,
    RecoveredTrash,
    SkippedActive,
    SkippedProtected,
    NotFound,
}

/// Observable result of one bounded tracing sweep.
///
/// `deleted` counts generations moved out of the live namespace and fully
/// removed in this call. `recovered` counts deterministic trash trees left by
/// an interrupted earlier deletion and completed in this call. `skipped`
/// counts active or caller-protected roots, while `remaining` counts otherwise
/// collectible entries deferred solely by `max_work`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoteHistoryGenerationSweepReport {
    pub deleted: usize,
    pub recovered: usize,
    pub skipped: usize,
    pub remaining: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteGenerationMetadata {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    generation: SourceHistoryRemoteGenerationId,
    binding: SourceHistoryRemoteBinding,
    origin: RemoteGenerationOrigin,
    clone_complete: bool,
}

impl RemoteGenerationMetadata {
    fn bootstrap(
        profile_id: HistoryProfileId,
        source_id: NodeId,
        redaction_profile: RedactionProfile,
        generation: SourceHistoryRemoteGenerationId,
        binding: SourceHistoryRemoteBinding,
    ) -> Self {
        Self {
            format_version: REMOTE_GENERATION_FORMAT_VERSION,
            profile_id,
            source_id,
            redaction_profile,
            generation,
            binding,
            origin: RemoteGenerationOrigin::Bootstrap,
            clone_complete: true,
        }
    }

    fn active_replacement(
        profile_id: HistoryProfileId,
        source_id: NodeId,
        redaction_profile: RedactionProfile,
        generation: SourceHistoryRemoteGenerationId,
        binding: SourceHistoryRemoteBinding,
        expected_active_generation: SourceHistoryRemoteGenerationId,
        page_fingerprint: String,
    ) -> Self {
        Self {
            format_version: REMOTE_GENERATION_FORMAT_VERSION,
            profile_id,
            source_id,
            redaction_profile,
            generation,
            binding,
            origin: RemoteGenerationOrigin::ActiveReplacement {
                expected_active_generation,
                page_fingerprint,
            },
            clone_complete: false,
        }
    }

    fn validate(
        &self,
        profile_id: &HistoryProfileId,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<()> {
        self.generation.validate()?;
        self.binding.validate_namespace(source_id)?;
        self.origin.validate()?;
        if self.format_version != REMOTE_GENERATION_FORMAT_VERSION
            || &self.profile_id != profile_id
            || &self.source_id != source_id
            || self.redaction_profile != redaction_profile
            || &self.generation != generation
        {
            return Err(invalid_data(
                "remote history generation metadata does not match its namespace",
            ));
        }
        if matches!(self.origin, RemoteGenerationOrigin::Bootstrap) && !self.clone_complete {
            return Err(invalid_data(
                "bootstrap remote history generation must be ready",
            ));
        }
        Ok(())
    }

    fn validate_ready(
        &self,
        profile_id: &HistoryProfileId,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<()> {
        self.validate(profile_id, source_id, redaction_profile, generation)?;
        if !self.clone_complete {
            return Err(invalid_data(
                "remote history generation baseline clone is incomplete",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RemoteGenerationOrigin {
    Bootstrap,
    ActiveReplacement {
        expected_active_generation: SourceHistoryRemoteGenerationId,
        page_fingerprint: String,
    },
}

impl RemoteGenerationOrigin {
    fn validate(&self) -> io::Result<()> {
        match self {
            Self::Bootstrap => Ok(()),
            Self::ActiveReplacement {
                expected_active_generation,
                page_fingerprint,
            } => {
                expected_active_generation.validate()?;
                validate_page_fingerprint(page_fingerprint)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteActiveManifest {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    active_generation: SourceHistoryRemoteGenerationId,
    binding: SourceHistoryRemoteBinding,
    activated_at: DateTime<Utc>,
}

impl RemoteActiveManifest {
    fn validate(
        &self,
        profile_id: &HistoryProfileId,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> io::Result<()> {
        self.active_generation.validate()?;
        self.binding.validate_namespace(source_id)?;
        if self.format_version != REMOTE_ACTIVE_MANIFEST_FORMAT_VERSION
            || &self.profile_id != profile_id
            || &self.source_id != source_id
            || self.redaction_profile != redaction_profile
        {
            return Err(invalid_data(
                "remote active history manifest does not match its namespace",
            ));
        }
        Ok(())
    }
}

impl SourceHistoryStore {
    pub fn source_remote_history_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source_id)
            .join(redaction_profile.directory_name())
            .join(REMOTE_HISTORY_DIRECTORY)
    }

    pub fn source_remote_history_generation_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> PathBuf {
        self.source_remote_history_directory(source_id, redaction_profile)
            .join(REMOTE_GENERATIONS_DIRECTORY)
            .join(generation.as_str())
    }

    /// Returns the active SSH history generation, if one has been activated.
    /// A corrupt or mismatched manifest fails closed instead of falling back to
    /// a legacy/direct namespace.
    pub fn active_remote_history_generation(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> io::Result<Option<SourceHistoryRemoteGenerationId>> {
        Ok(self
            .active_remote_history_ref(source_id, redaction_profile)?
            .map(|active| active.generation))
    }

    pub fn active_remote_history_ref(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> io::Result<Option<SourceHistoryRemoteActiveRef>> {
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return Ok(None);
        }
        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_shared(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        let Some(manifest) =
            self.read_remote_active_manifest_locked(source_id, redaction_profile, &root)?
        else {
            return Ok(None);
        };
        self.validate_remote_generation_binding_locked(
            source_id,
            redaction_profile,
            &manifest.active_generation,
            &manifest.binding,
        )?;
        Ok(Some(SourceHistoryRemoteActiveRef {
            generation: manifest.active_generation,
            binding: manifest.binding,
        }))
    }

    /// Loads both revisioned history families from one active SSH generation.
    ///
    /// Unlike calling the bucket and digest query methods separately, this
    /// method keeps the shared remote-history root lock across both reads, so
    /// the active manifest cannot switch between the two families.
    pub fn load_remote_history_snapshot_since(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<SourceHistoryRemoteSnapshot> {
        self.load_remote_history_snapshot_since_with_between_families(
            source_id,
            redaction_profile,
            since,
            || {},
        )
    }

    fn load_remote_history_snapshot_since_with_between_families(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        between_families: impl FnOnce(),
    ) -> io::Result<SourceHistoryRemoteSnapshot> {
        self.with_source_metadata_shared(source_id, |source| {
            require_ssh_source(source)?;
            let root = self.source_remote_history_directory(source_id, redaction_profile);
            if !self.private_directory_exists(&root)? {
                return Ok(SourceHistoryRemoteSnapshot::default());
            }
            let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
            lock_shared(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
            let Some(manifest) =
                self.read_remote_active_manifest_locked(source_id, redaction_profile, &root)?
            else {
                return Ok(SourceHistoryRemoteSnapshot::default());
            };
            let generation_directory = self.validate_remote_generation_binding_locked(
                source_id,
                redaction_profile,
                &manifest.active_generation,
                &manifest.binding,
            )?;
            let active_ref = SourceHistoryRemoteActiveRef {
                generation: manifest.active_generation,
                binding: manifest.binding,
            };
            let bucket_records = self.load_source_bucket_records_from_directory(
                source_id,
                redaction_profile,
                since,
                &generation_directory.join(BUCKETS_DIRECTORY),
            )?;
            between_families();
            let session_digest_records = self.load_source_session_digest_records_from_directory(
                source_id,
                redaction_profile,
                since,
                &generation_directory.join(DIGESTS_DIRECTORY),
            )?;
            Ok(SourceHistoryRemoteSnapshot {
                active_ref: Some(active_ref),
                bucket_records,
                session_digest_records,
            })
        })
    }

    pub(super) fn with_active_remote_history_generation<T>(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        operation: impl FnOnce(Option<&Path>) -> io::Result<T>,
    ) -> io::Result<T> {
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return operation(None);
        }
        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_shared(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        let manifest =
            self.read_remote_active_manifest_locked(source_id, redaction_profile, &root)?;
        let generation_directory = manifest
            .as_ref()
            .map(|manifest| {
                self.validate_remote_generation_binding_locked(
                    source_id,
                    redaction_profile,
                    &manifest.active_generation,
                    &manifest.binding,
                )
            })
            .transpose()?;
        operation(generation_directory.as_deref())
    }

    fn ensure_remote_history_generation_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
    ) -> io::Result<()> {
        generation.validate()?;
        binding.validate_namespace(source_id)?;
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        self.prepare_private_directory(&root)?;
        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_exclusive(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        cleanup_remote_atomic_temporary_files(self, &root, REMOTE_ACTIVE_MANIFEST_FILE)?;
        validate_remote_generation_capacity_locked(
            self,
            source_id,
            redaction_profile,
            &root,
            generation,
        )?;
        let directory = self.source_remote_history_generation_directory(
            source_id,
            redaction_profile,
            generation,
        );
        self.prepare_private_directory(&directory)?;
        cleanup_remote_atomic_temporary_files(self, &directory, REMOTE_GENERATION_METADATA_FILE)?;
        let metadata_path = directory.join(REMOTE_GENERATION_METADATA_FILE);
        let expected = RemoteGenerationMetadata::bootstrap(
            self.profile_id.clone(),
            source_id.clone(),
            redaction_profile,
            generation.clone(),
            binding.clone(),
        );
        match read_optional_json_file::<RemoteGenerationMetadata>(
            &metadata_path,
            MAX_REMOTE_GENERATION_FILE_BYTES,
        )? {
            Some(existing) => {
                existing.validate_ready(
                    &self.profile_id,
                    source_id,
                    redaction_profile,
                    generation,
                )?;
                if existing.origin != RemoteGenerationOrigin::Bootstrap {
                    return Err(invalid_data(
                        "remote history generation ID is already bound to a replacement",
                    ));
                }
                if &existing.binding != binding {
                    return Err(invalid_data(
                        "remote history generation is bound to another exporter revision",
                    ));
                }
            }
            None => write_private_atomically(
                &metadata_path,
                &encode_pretty_bounded(&expected, MAX_REMOTE_GENERATION_FILE_BYTES)?,
            )?,
        }
        self.prepare_private_directory(&directory.join(BUCKETS_DIRECTORY))?;
        self.prepare_private_directory(&directory.join(DIGESTS_DIRECTORY))?;
        self.validate_remote_generation_root_entries(&directory)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_remote_history_generation_page_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
        require_active: bool,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        generation.validate()?;
        binding.validate_namespace(source_id)?;
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "remote history generation has not been staged",
            ));
        }
        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_exclusive(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        cleanup_remote_atomic_temporary_files(self, &root, REMOTE_ACTIVE_MANIFEST_FILE)?;
        let candidate_directory = self.source_remote_history_generation_directory(
            source_id,
            redaction_profile,
            generation,
        );
        if self.private_directory_exists(&candidate_directory)? {
            cleanup_remote_atomic_temporary_files(
                self,
                &candidate_directory,
                REMOTE_GENERATION_METADATA_FILE,
            )?;
        }
        let active = self
            .read_remote_active_manifest_locked(source_id, redaction_profile, &root)?
            .map(|manifest| manifest.active_generation);
        if active.as_ref() == Some(generation) || require_active {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote active history generations are immutable; use a COW replacement",
            ));
        }
        let directory = self.validate_remote_generation_binding_locked(
            source_id,
            redaction_profile,
            generation,
            binding,
        )?;
        let bucket_history = self.record_source_bucket_changes_in_directory_unfenced(
            source_id,
            redaction_profile,
            &directory.join(BUCKETS_DIRECTORY),
            bucket_records,
        )?;
        let session_digests = self.record_source_session_digest_changes_in_directory_unfenced(
            source_id,
            redaction_profile,
            &directory.join(DIGESTS_DIRECTORY),
            digest_records,
        )?;
        Ok(RemoteHistoryPageWriteReport {
            bucket_history,
            session_digests,
        })
    }

    /// Crash-safe active incremental apply. The active generation is never
    /// edited in place: its immutable shards are cloned into the caller's
    /// deterministic replacement, the complete page is applied there, and a
    /// single manifest replace publishes both data families.
    #[allow(clippy::too_many_arguments)]
    fn apply_remote_history_active_page_cow_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: &SourceHistoryRemoteActiveRef,
        replacement_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        activated_at: DateTime<Utc>,
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        expected_active.validate_namespace(source_id)?;
        replacement_generation.validate()?;
        candidate_binding.validate_namespace(source_id)?;
        if candidate_binding != expected_active.binding() {
            return Err(invalid_data(
                "active COW replacement binding must match the active generation",
            ));
        }
        if expected_active.generation() == replacement_generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote active replacement must use a distinct generation",
            ));
        }
        let page_fingerprint = remote_page_fingerprint(bucket_records, digest_records)?;
        let expected_origin = RemoteGenerationOrigin::ActiveReplacement {
            expected_active_generation: expected_active.generation().clone(),
            page_fingerprint,
        };
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "remote history has no active generation",
            ));
        }
        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_exclusive(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        cleanup_remote_atomic_temporary_files(self, &root, REMOTE_ACTIVE_MANIFEST_FILE)?;
        let active_manifest = self
            .read_remote_active_manifest_locked(source_id, redaction_profile, &root)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "remote history has no active generation",
                )
            })?;
        let active = SourceHistoryRemoteActiveRef {
            generation: active_manifest.active_generation,
            binding: active_manifest.binding,
        };

        if active.generation() == replacement_generation && active.binding() == candidate_binding {
            self.validate_remote_replacement_locked(
                source_id,
                redaction_profile,
                replacement_generation,
                candidate_binding,
                &expected_origin,
            )?;
            return Ok(RemoteHistoryPageWriteReport::default());
        }
        if &active != expected_active {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote active history generation changed before COW apply",
            ));
        }

        let expected_directory = self.validate_remote_generation_binding_locked(
            source_id,
            redaction_profile,
            expected_active.generation(),
            expected_active.binding(),
        )?;
        let replacement_directory = self.prepare_remote_replacement_locked(
            source_id,
            redaction_profile,
            replacement_generation,
            candidate_binding,
            &expected_origin,
            &expected_directory,
        )?;

        // Per-family writes may fail independently, but the replacement is
        // invisible until both finish and the manifest is switched below.
        let bucket_history = self.record_source_bucket_changes_in_directory_unfenced(
            source_id,
            redaction_profile,
            &replacement_directory.join(BUCKETS_DIRECTORY),
            bucket_records,
        )?;
        let session_digests = self.record_source_session_digest_changes_in_directory_unfenced(
            source_id,
            redaction_profile,
            &replacement_directory.join(DIGESTS_DIRECTORY),
            digest_records,
        )?;

        // Revalidate the exact replacement binding after nested shard writes.
        // The root lock prevents another cooperative activation, while this
        // catches namespace corruption before publication.
        self.validate_remote_replacement_locked(
            source_id,
            redaction_profile,
            replacement_generation,
            candidate_binding,
            &expected_origin,
        )?;
        let manifest = RemoteActiveManifest {
            format_version: REMOTE_ACTIVE_MANIFEST_FORMAT_VERSION,
            profile_id: self.profile_id.clone(),
            source_id: source_id.clone(),
            redaction_profile,
            active_generation: replacement_generation.clone(),
            binding: candidate_binding.clone(),
            activated_at,
        };
        write_private_atomically(
            &root.join(REMOTE_ACTIVE_MANIFEST_FILE),
            &encode_pretty_bounded(&manifest, MAX_REMOTE_GENERATION_FILE_BYTES)?,
        )?;
        Ok(RemoteHistoryPageWriteReport {
            bucket_history,
            session_digests,
        })
    }

    fn activate_remote_history_generation_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: Option<&SourceHistoryRemoteActiveRef>,
        candidate_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        activated_at: DateTime<Utc>,
    ) -> io::Result<()> {
        if let Some(expected_active) = expected_active {
            expected_active.validate_namespace(source_id)?;
        }
        candidate_generation.validate()?;
        candidate_binding.validate_namespace(source_id)?;
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "remote history generation has not been staged",
            ));
        }
        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_exclusive(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        cleanup_remote_atomic_temporary_files(self, &root, REMOTE_ACTIVE_MANIFEST_FILE)?;
        let candidate_directory = self.source_remote_history_generation_directory(
            source_id,
            redaction_profile,
            candidate_generation,
        );
        if self.private_directory_exists(&candidate_directory)? {
            cleanup_remote_atomic_temporary_files(
                self,
                &candidate_directory,
                REMOTE_GENERATION_METADATA_FILE,
            )?;
        }
        let metadata = self.read_remote_generation_metadata_locked(
            source_id,
            redaction_profile,
            candidate_generation,
        )?;
        metadata.validate_ready(
            &self.profile_id,
            source_id,
            redaction_profile,
            candidate_generation,
        )?;
        if &metadata.binding != candidate_binding {
            return Err(invalid_data(
                "bootstrap candidate binding does not match its generation metadata",
            ));
        }
        if metadata.origin != RemoteGenerationOrigin::Bootstrap {
            return Err(invalid_data(
                "active replacement generations require the COW CAS activation path",
            ));
        }
        self.validate_remote_generation_binding_locked(
            source_id,
            redaction_profile,
            candidate_generation,
            candidate_binding,
        )?;
        let actual_manifest =
            self.read_remote_active_manifest_locked(source_id, redaction_profile, &root)?;
        let actual = if let Some(manifest) = actual_manifest {
            let active = SourceHistoryRemoteActiveRef {
                generation: manifest.active_generation,
                binding: manifest.binding,
            };
            self.validate_remote_generation_binding_locked(
                source_id,
                redaction_profile,
                active.generation(),
                active.binding(),
            )?;
            Some(active)
        } else {
            None
        };
        let candidate = SourceHistoryRemoteActiveRef {
            generation: candidate_generation.clone(),
            binding: candidate_binding.clone(),
        };
        if actual.as_ref() == Some(&candidate) {
            return Ok(());
        }
        if actual.as_ref() != expected_active {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote active history generation changed before bootstrap activation",
            ));
        }
        if let Some(active) = &actual {
            validate_binding_does_not_roll_back(active.binding(), candidate_binding)?;
        }
        let manifest = RemoteActiveManifest {
            format_version: REMOTE_ACTIVE_MANIFEST_FORMAT_VERSION,
            profile_id: self.profile_id.clone(),
            source_id: source_id.clone(),
            redaction_profile,
            active_generation: candidate_generation.clone(),
            binding: candidate_binding.clone(),
            activated_at,
        };
        write_private_atomically(
            &root.join(REMOTE_ACTIVE_MANIFEST_FILE),
            &encode_pretty_bounded(&manifest, MAX_REMOTE_GENERATION_FILE_BYTES)?,
        )
    }

    #[cfg(test)]
    fn validate_active_remote_history_generation_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<()> {
        generation.validate()?;
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "remote history has no active generation",
            ));
        }
        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_shared(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        let manifest = self
            .read_remote_active_manifest_locked(source_id, redaction_profile, &root)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "remote history has no active generation",
                )
            })?;
        if &manifest.active_generation != generation {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote active history generation does not match",
            ));
        }
        self.validate_remote_generation_binding_locked(
            source_id,
            redaction_profile,
            generation,
            &manifest.binding,
        )?;
        Ok(())
    }

    fn garbage_collect_remote_history_generation_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        candidate: &SourceHistoryRemoteGenerationId,
        protected: &BTreeSet<SourceHistoryRemoteGenerationId>,
    ) -> io::Result<RemoteHistoryGenerationGcOutcome> {
        candidate.validate()?;
        for generation in protected {
            generation.validate()?;
        }
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return Ok(RemoteHistoryGenerationGcOutcome::NotFound);
        }

        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_exclusive(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        cleanup_remote_atomic_temporary_files(self, &root, REMOTE_ACTIVE_MANIFEST_FILE)?;
        validate_remote_gc_root_namespace(self, &root)?;

        let active_manifest =
            self.read_remote_active_manifest_locked(source_id, redaction_profile, &root)?;
        if let Some(active) = &active_manifest {
            // Do not trust only the generation string in the manifest. Its
            // complete binding to generation metadata must still be valid
            // while the exclusive root lock is held.
            self.validate_remote_generation_binding_locked(
                source_id,
                redaction_profile,
                &active.active_generation,
                &active.binding,
            )?;
        }

        if active_manifest
            .as_ref()
            .is_some_and(|active| &active.active_generation == candidate)
        {
            return Ok(RemoteHistoryGenerationGcOutcome::SkippedActive);
        }
        if protected.contains(candidate) {
            return Ok(RemoteHistoryGenerationGcOutcome::SkippedProtected);
        }

        let generations = root.join(REMOTE_GENERATIONS_DIRECTORY);
        let trash = root.join(REMOTE_GC_TRASH_DIRECTORY);
        validate_remote_gc_generation_parent(self, &generations)?;
        if self.private_directory_exists(&trash)? {
            validate_remote_gc_trash_parent(self, &trash)?;
        }

        let candidate_directory = generations.join(candidate.as_str());
        let trash_directory = trash.join(remote_gc_trash_name(candidate));
        let candidate_exists = self.private_directory_exists(&candidate_directory)?;
        let trash_exists = self.private_directory_exists(&trash_directory)?;
        if candidate_exists && trash_exists {
            return Err(invalid_data(
                "remote history generation exists in both active and GC trash namespaces",
            ));
        }

        if trash_exists {
            validate_remote_gc_trash_generation_directory(
                self,
                source_id,
                redaction_profile,
                candidate,
                &trash_directory,
            )?;
            remove_remote_gc_generation_tree(self, &trash_directory)?;
            sync_directory(&trash)?;
            return Ok(RemoteHistoryGenerationGcOutcome::RecoveredTrash);
        }
        if !candidate_exists {
            return Ok(RemoteHistoryGenerationGcOutcome::NotFound);
        }

        validate_remote_gc_generation_directory(
            self,
            source_id,
            redaction_profile,
            candidate,
            &candidate_directory,
        )?;
        self.prepare_private_directory(&trash)?;
        validate_remote_gc_trash_parent(self, &trash)?;
        rename_remote_generation_to_trash(&candidate_directory, &trash_directory)?;
        sync_directory(&generations)?;
        sync_directory(&trash)?;

        // Revalidate the moved tree before any pathname-based removal. A
        // crash from here leaves a deterministic, recoverable trash name.
        validate_remote_gc_trash_generation_directory(
            self,
            source_id,
            redaction_profile,
            candidate,
            &trash_directory,
        )?;
        remove_remote_gc_generation_tree(self, &trash_directory)?;
        sync_directory(&trash)?;
        Ok(RemoteHistoryGenerationGcOutcome::Deleted)
    }

    /// Sweeps every generation not reachable from the active manifest or the
    /// caller's complete protected set. The caller owns the higher-level
    /// source-ingest lock; this method additionally holds the remote-history
    /// root lock for one consistent trace-and-sweep pass.
    fn sweep_remote_history_generations_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        protected: &BTreeSet<SourceHistoryRemoteGenerationId>,
        max_work: usize,
    ) -> io::Result<RemoteHistoryGenerationSweepReport> {
        for generation in protected {
            generation.validate()?;
        }
        let source = self.load_source_metadata(source_id)?;
        require_ssh_source(&source)?;
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&root)? {
            return Ok(RemoteHistoryGenerationSweepReport::default());
        }

        let lock = open_lock_file(&root, REMOTE_HISTORY_LOCK_FILE)?;
        lock_exclusive(&lock, &root, REMOTE_HISTORY_LOCK_FILE)?;
        cleanup_remote_atomic_temporary_files(self, &root, REMOTE_ACTIVE_MANIFEST_FILE)?;
        let active_manifest =
            self.read_remote_active_manifest_locked(source_id, redaction_profile, &root)?;
        if let Some(active) = &active_manifest {
            self.validate_remote_generation_binding_locked(
                source_id,
                redaction_profile,
                &active.active_generation,
                &active.binding,
            )?;
        }
        let active = active_manifest
            .as_ref()
            .map(|manifest| &manifest.active_generation);
        let catalog = load_remote_history_generation_catalog_locked(
            self,
            source_id,
            redaction_profile,
            &root,
            true,
        )?;

        let generations = root.join(REMOTE_GENERATIONS_DIRECTORY);
        let trash = root.join(REMOTE_GC_TRASH_DIRECTORY);
        let mut report = RemoteHistoryGenerationSweepReport::default();

        // Complete interrupted deterministic removals before starting new
        // ones. Each completed tree consumes one work unit.
        for (generation, directory) in &catalog.trash {
            if active == Some(generation) || protected.contains(generation) {
                report.skipped += 1;
                continue;
            }
            if report.deleted + report.recovered >= max_work {
                report.remaining += 1;
                continue;
            }
            validate_remote_gc_trash_generation_directory(
                self,
                source_id,
                redaction_profile,
                generation,
                directory,
            )?;
            remove_remote_gc_generation_tree(self, directory)?;
            sync_directory(&trash)?;
            report.recovered += 1;
        }

        for (generation, directory) in &catalog.generations {
            if active == Some(generation) || protected.contains(generation) {
                report.skipped += 1;
                continue;
            }
            if report.deleted + report.recovered >= max_work {
                report.remaining += 1;
                continue;
            }

            validate_remote_gc_generation_directory_mode(
                self,
                source_id,
                redaction_profile,
                generation,
                directory,
                false,
            )?;
            self.prepare_private_directory(&trash)?;
            validate_remote_gc_trash_parent(self, &trash)?;
            let trash_directory = trash.join(remote_gc_trash_name(generation));
            if self.private_directory_exists(&trash_directory)? {
                return Err(invalid_data(
                    "remote history generation appeared in GC trash during sweep",
                ));
            }
            rename_remote_generation_to_trash(directory, &trash_directory)?;
            sync_directory(&generations)?;
            sync_directory(&trash)?;
            validate_remote_gc_trash_generation_directory(
                self,
                source_id,
                redaction_profile,
                generation,
                &trash_directory,
            )?;
            remove_remote_gc_generation_tree(self, &trash_directory)?;
            sync_directory(&trash)?;
            report.deleted += 1;
        }
        Ok(report)
    }

    fn prepare_remote_replacement_locked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        replacement_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        expected_origin: &RemoteGenerationOrigin,
        expected_directory: &Path,
    ) -> io::Result<PathBuf> {
        let directory = self.source_remote_history_generation_directory(
            source_id,
            redaction_profile,
            replacement_generation,
        );
        let root = self.source_remote_history_directory(source_id, redaction_profile);
        let directory_exists = validate_remote_generation_capacity_locked(
            self,
            source_id,
            redaction_profile,
            &root,
            replacement_generation,
        )?;
        if directory_exists {
            cleanup_remote_atomic_temporary_files(
                self,
                &directory,
                REMOTE_GENERATION_METADATA_FILE,
            )?;
        }
        let metadata_path = directory.join(REMOTE_GENERATION_METADATA_FILE);
        let mut metadata = if directory_exists {
            match read_optional_json_file::<RemoteGenerationMetadata>(
                &metadata_path,
                MAX_REMOTE_GENERATION_FILE_BYTES,
            )? {
                Some(metadata) => metadata,
                None => {
                    ensure_recoverable_empty_generation_directory(self, &directory)?;
                    let RemoteGenerationOrigin::ActiveReplacement {
                        expected_active_generation,
                        page_fingerprint,
                    } = expected_origin
                    else {
                        return Err(invalid_data("replacement origin is not active COW"));
                    };
                    let metadata = RemoteGenerationMetadata::active_replacement(
                        self.profile_id.clone(),
                        source_id.clone(),
                        redaction_profile,
                        replacement_generation.clone(),
                        candidate_binding.clone(),
                        expected_active_generation.clone(),
                        page_fingerprint.clone(),
                    );
                    write_private_atomically(
                        &metadata_path,
                        &encode_pretty_bounded(&metadata, MAX_REMOTE_GENERATION_FILE_BYTES)?,
                    )?;
                    metadata
                }
            }
        } else {
            self.prepare_private_directory(&directory)?;
            let RemoteGenerationOrigin::ActiveReplacement {
                expected_active_generation,
                page_fingerprint,
            } = expected_origin
            else {
                return Err(invalid_data("replacement origin is not active COW"));
            };
            let metadata = RemoteGenerationMetadata::active_replacement(
                self.profile_id.clone(),
                source_id.clone(),
                redaction_profile,
                replacement_generation.clone(),
                candidate_binding.clone(),
                expected_active_generation.clone(),
                page_fingerprint.clone(),
            );
            write_private_atomically(
                &metadata_path,
                &encode_pretty_bounded(&metadata, MAX_REMOTE_GENERATION_FILE_BYTES)?,
            )?;
            metadata
        };
        metadata.validate(
            &self.profile_id,
            source_id,
            redaction_profile,
            replacement_generation,
        )?;
        if &metadata.origin != expected_origin {
            return Err(invalid_data(
                "remote replacement generation is bound to another active page",
            ));
        }
        if &metadata.binding != candidate_binding {
            return Err(invalid_data(
                "remote replacement generation is bound to another exporter revision",
            ));
        }

        let replacement_buckets = directory.join(BUCKETS_DIRECTORY);
        let replacement_digests = directory.join(DIGESTS_DIRECTORY);
        if metadata.clone_complete {
            self.validate_remote_replacement_locked(
                source_id,
                redaction_profile,
                replacement_generation,
                candidate_binding,
                expected_origin,
            )?;
            return Ok(directory);
        }
        self.prepare_private_directory(&replacement_buckets)?;
        self.prepare_private_directory(&replacement_digests)?;
        self.validate_remote_generation_root_entries(&directory)?;

        clone_remote_generation_family(
            self,
            source_id,
            redaction_profile,
            &expected_directory.join(BUCKETS_DIRECTORY),
            &replacement_buckets,
            RemoteCloneFamily::Buckets,
        )?;
        clone_remote_generation_family(
            self,
            source_id,
            redaction_profile,
            &expected_directory.join(DIGESTS_DIRECTORY),
            &replacement_digests,
            RemoteCloneFamily::Digests,
        )?;

        metadata.clone_complete = true;
        write_private_atomically(
            &metadata_path,
            &encode_pretty_bounded(&metadata, MAX_REMOTE_GENERATION_FILE_BYTES)?,
        )?;
        self.validate_remote_replacement_locked(
            source_id,
            redaction_profile,
            replacement_generation,
            candidate_binding,
            expected_origin,
        )?;
        Ok(directory)
    }

    fn validate_remote_replacement_locked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        replacement_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        expected_origin: &RemoteGenerationOrigin,
    ) -> io::Result<PathBuf> {
        let metadata = self.read_remote_generation_metadata_locked(
            source_id,
            redaction_profile,
            replacement_generation,
        )?;
        metadata.validate_ready(
            &self.profile_id,
            source_id,
            redaction_profile,
            replacement_generation,
        )?;
        if &metadata.origin != expected_origin {
            return Err(invalid_data(
                "remote replacement generation is bound to another active page",
            ));
        }
        if &metadata.binding != candidate_binding {
            return Err(invalid_data(
                "remote replacement generation is bound to another exporter revision",
            ));
        }
        self.validate_remote_generation_binding_locked(
            source_id,
            redaction_profile,
            replacement_generation,
            candidate_binding,
        )
    }

    fn read_remote_active_manifest_locked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        root: &Path,
    ) -> io::Result<Option<RemoteActiveManifest>> {
        let manifest = read_optional_json_file::<RemoteActiveManifest>(
            &root.join(REMOTE_ACTIVE_MANIFEST_FILE),
            MAX_REMOTE_GENERATION_FILE_BYTES,
        )?;
        if let Some(manifest) = &manifest {
            manifest.validate(&self.profile_id, source_id, redaction_profile)?;
        }
        Ok(manifest)
    }

    fn read_remote_generation_metadata_locked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<RemoteGenerationMetadata> {
        let directory = self.source_remote_history_generation_directory(
            source_id,
            redaction_profile,
            generation,
        );
        self.validate_private_path(&directory)?;
        let metadata: RemoteGenerationMetadata = read_json_file(
            &directory.join(REMOTE_GENERATION_METADATA_FILE),
            MAX_REMOTE_GENERATION_FILE_BYTES,
        )?;
        metadata.validate(&self.profile_id, source_id, redaction_profile, generation)?;
        Ok(metadata)
    }

    fn validate_remote_generation_root_entries(&self, directory: &Path) -> io::Result<()> {
        self.validate_private_path(directory)?;
        let mut metadata = false;
        let mut buckets = false;
        let mut digests = false;
        for entry in fs::read_dir(directory)? {
            self.validate_private_path(directory)?;
            let entry = entry?;
            if is_remote_atomic_temporary_file(&entry.file_name(), REMOTE_GENERATION_METADATA_FILE)
            {
                validate_published_private_file(&entry.path())?;
                continue;
            }
            match entry.file_name().to_str() {
                Some(REMOTE_GENERATION_METADATA_FILE) if !metadata => {
                    validate_published_private_file(&entry.path())?;
                    metadata = true;
                }
                Some(BUCKETS_DIRECTORY) if !buckets => {
                    self.validate_private_path(&entry.path())?;
                    buckets = true;
                }
                Some(DIGESTS_DIRECTORY) if !digests => {
                    self.validate_private_path(&entry.path())?;
                    digests = true;
                }
                _ => {
                    return Err(invalid_data(format!(
                        "unexpected path in remote history generation {}",
                        entry.path().display()
                    )));
                }
            }
        }
        if !metadata || !buckets || !digests {
            return Err(invalid_data(
                "remote history generation namespace is incomplete",
            ));
        }
        Ok(())
    }

    fn validate_remote_generation_locked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<PathBuf> {
        let directory = self.source_remote_history_generation_directory(
            source_id,
            redaction_profile,
            generation,
        );
        self.validate_remote_generation_root_entries(&directory)?;
        let metadata =
            self.read_remote_generation_metadata_locked(source_id, redaction_profile, generation)?;
        metadata.validate_ready(&self.profile_id, source_id, redaction_profile, generation)?;
        self.validate_private_path(&directory.join(BUCKETS_DIRECTORY))?;
        self.validate_private_path(&directory.join(DIGESTS_DIRECTORY))?;
        Ok(directory)
    }

    fn validate_remote_generation_binding_locked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
    ) -> io::Result<PathBuf> {
        binding.validate_namespace(source_id)?;
        let directory =
            self.validate_remote_generation_locked(source_id, redaction_profile, generation)?;
        let metadata =
            self.read_remote_generation_metadata_locked(source_id, redaction_profile, generation)?;
        if &metadata.binding != binding {
            return Err(invalid_data(
                "remote history manifest binding does not match generation metadata",
            ));
        }
        Ok(directory)
    }

    #[cfg(test)]
    pub(crate) fn ensure_remote_history_generation(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
    ) -> io::Result<()> {
        self.ensure_remote_history_generation_unfenced(
            source_id,
            redaction_profile,
            generation,
            binding,
        )
    }

    #[cfg(test)]
    pub(crate) fn apply_remote_history_generation_page(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.apply_remote_history_generation_page_unfenced(
            source_id,
            redaction_profile,
            generation,
            binding,
            false,
            bucket_records,
            digest_records,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_remote_history_active_page_cow(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: &SourceHistoryRemoteActiveRef,
        replacement_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        activated_at: DateTime<Utc>,
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.apply_remote_history_active_page_cow_unfenced(
            source_id,
            redaction_profile,
            expected_active,
            replacement_generation,
            candidate_binding,
            bucket_records,
            digest_records,
            activated_at,
        )
    }

    #[cfg(test)]
    pub(crate) fn activate_remote_history_generation(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: Option<&SourceHistoryRemoteActiveRef>,
        candidate_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        activated_at: DateTime<Utc>,
    ) -> io::Result<()> {
        self.activate_remote_history_generation_unfenced(
            source_id,
            redaction_profile,
            expected_active,
            candidate_generation,
            candidate_binding,
            activated_at,
        )
    }

    #[cfg(test)]
    pub(crate) fn garbage_collect_remote_history_generation(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        candidate: &SourceHistoryRemoteGenerationId,
        protected: &BTreeSet<SourceHistoryRemoteGenerationId>,
    ) -> io::Result<RemoteHistoryGenerationGcOutcome> {
        self.garbage_collect_remote_history_generation_unfenced(
            source_id,
            redaction_profile,
            candidate,
            protected,
        )
    }

    #[cfg(test)]
    pub(crate) fn sweep_remote_history_generations(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        protected: &BTreeSet<SourceHistoryRemoteGenerationId>,
        max_work: usize,
    ) -> io::Result<RemoteHistoryGenerationSweepReport> {
        self.sweep_remote_history_generations_unfenced(
            source_id,
            redaction_profile,
            protected,
            max_work,
        )
    }
}

impl SourceHistoryWriter<'_, '_, '_> {
    pub(crate) fn ensure_remote_history_generation(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
    ) -> io::Result<()> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.ensure_remote_history_generation_unfenced(
                source_id,
                redaction_profile,
                generation,
                binding,
            )
        })
    }

    pub(crate) fn apply_remote_history_generation_page(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.apply_remote_history_generation_page_unfenced(
                source_id,
                redaction_profile,
                generation,
                binding,
                false,
                bucket_records,
                digest_records,
            )
        })
    }

    /// Applies one active incremental page through an immutable, deterministic
    /// replacement generation and atomically publishes it on success.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_remote_history_active_page_cow(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: &SourceHistoryRemoteActiveRef,
        replacement_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        activated_at: DateTime<Utc>,
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.apply_remote_history_active_page_cow_unfenced(
                source_id,
                redaction_profile,
                expected_active,
                replacement_generation,
                candidate_binding,
                bucket_records,
                digest_records,
                activated_at,
            )
        })
    }

    pub(crate) fn activate_remote_history_generation(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: Option<&SourceHistoryRemoteActiveRef>,
        candidate_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        activated_at: DateTime<Utc>,
    ) -> io::Result<()> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.activate_remote_history_generation_unfenced(
                source_id,
                redaction_profile,
                expected_active,
                candidate_generation,
                candidate_binding,
                activated_at,
            )
        })
    }

    /// Deletes one explicitly selected, unreferenced remote history
    /// generation. Retention policy and the protected set are owned by the
    /// caller; this method rechecks the active manifest under the exclusive
    /// remote-history root lock and fails closed on namespace ambiguity.
    pub(crate) fn garbage_collect_remote_history_generation(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        candidate: &SourceHistoryRemoteGenerationId,
        protected: &BTreeSet<SourceHistoryRemoteGenerationId>,
    ) -> io::Result<RemoteHistoryGenerationGcOutcome> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.garbage_collect_remote_history_generation_unfenced(
                source_id,
                redaction_profile,
                candidate,
                protected,
            )
        })
    }

    /// Performs one bounded trace-and-sweep pass. The caller must hold the
    /// source-wide ingest lock and supply the complete generation protection
    /// set from every binding namespace for this source/redaction pair.
    #[allow(dead_code)] // Called by the ingest bridge once the v0.4 runtime is wired.
    pub(crate) fn sweep_remote_history_generations(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        protected: &BTreeSet<SourceHistoryRemoteGenerationId>,
        max_work: usize,
    ) -> io::Result<RemoteHistoryGenerationSweepReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.sweep_remote_history_generations_unfenced(
                source_id,
                redaction_profile,
                protected,
                max_work,
            )
        })
    }
}

#[derive(Clone, Copy)]
enum RemoteCloneFamily {
    Buckets,
    Digests,
}

fn cleanup_remote_atomic_temporary_files(
    store: &SourceHistoryStore,
    directory: &Path,
    target: &str,
) -> io::Result<usize> {
    store.validate_private_path(directory)?;
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        if !is_remote_atomic_temporary_file(&entry.file_name(), target) {
            continue;
        }
        validate_published_private_file(&entry.path())?;
        store.validate_private_path(directory)?;
        fs::remove_file(entry.path())?;
        removed += 1;
    }
    if removed > 0 {
        sync_directory(directory)?;
    }
    Ok(removed)
}

fn is_remote_atomic_temporary_file(name: &OsStr, target: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(body) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((body, sequence)) = body.rsplit_once('.') else {
        return false;
    };
    let Some((actual_target, process_id)) = body.rsplit_once('.') else {
        return false;
    };
    actual_target == target
        && !process_id.is_empty()
        && process_id.bytes().all(|byte| byte.is_ascii_digit())
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

fn remote_atomic_temporary_target(name: &OsStr) -> Option<&str> {
    let name = name.to_str()?;
    let body = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let (body, sequence) = body.rsplit_once('.')?;
    let (target, process_id) = body.rsplit_once('.')?;
    if target.is_empty()
        || process_id.is_empty()
        || !process_id.bytes().all(|byte| byte.is_ascii_digit())
        || sequence.is_empty()
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(target)
}

fn remote_gc_trash_name(generation: &SourceHistoryRemoteGenerationId) -> String {
    format!(
        "{REMOTE_GC_TRASH_PREFIX}{}{REMOTE_GC_TRASH_SUFFIX}",
        generation.as_str()
    )
}

fn parse_remote_gc_trash_name(name: &OsStr) -> Option<SourceHistoryRemoteGenerationId> {
    name.to_str()?
        .strip_prefix(REMOTE_GC_TRASH_PREFIX)?
        .strip_suffix(REMOTE_GC_TRASH_SUFFIX)?
        .parse()
        .ok()
}

#[derive(Debug, Default)]
struct RemoteHistoryGenerationCatalog {
    generations: BTreeMap<SourceHistoryRemoteGenerationId, PathBuf>,
    trash: BTreeMap<SourceHistoryRemoteGenerationId, PathBuf>,
}

impl RemoteHistoryGenerationCatalog {
    fn len(&self) -> usize {
        self.generations.len() + self.trash.len()
    }
}

/// Strictly catalogs both live and deterministic-trash namespaces. A
/// generation may occur in exactly one of them: counting the union keeps a
/// crash after rename from freeing capacity before its tree is removed.
fn load_remote_history_generation_catalog_locked(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    root: &Path,
    validate_generation_trees: bool,
) -> io::Result<RemoteHistoryGenerationCatalog> {
    validate_remote_gc_root_namespace(store, root)?;
    let mut catalog = RemoteHistoryGenerationCatalog::default();

    let generations = root.join(REMOTE_GENERATIONS_DIRECTORY);
    if store.private_directory_exists(&generations)? {
        store.validate_private_path(&generations)?;
        for entry in fs::read_dir(&generations)? {
            store.validate_private_path(&generations)?;
            let entry = entry?;
            let generation = entry
                .file_name()
                .to_str()
                .ok_or_else(|| invalid_data("remote history generation name is not UTF-8"))?
                .parse::<SourceHistoryRemoteGenerationId>()
                .map_err(|_| {
                    invalid_data(format!(
                        "unexpected path in remote history generation parent {}",
                        entry.path().display()
                    ))
                })?;
            let directory = entry.path();
            validate_remote_generation_capacity_entry_locked(
                store,
                source_id,
                redaction_profile,
                &generation,
                &directory,
            )?;
            if validate_generation_trees {
                validate_remote_gc_generation_directory_mode(
                    store,
                    source_id,
                    redaction_profile,
                    &generation,
                    &directory,
                    false,
                )?;
            }
            if catalog.generations.insert(generation, directory).is_some() {
                return Err(invalid_data(
                    "duplicate remote history generation namespace entry",
                ));
            }
        }
    }

    let trash = root.join(REMOTE_GC_TRASH_DIRECTORY);
    if store.private_directory_exists(&trash)? {
        store.validate_private_path(&trash)?;
        for entry in fs::read_dir(&trash)? {
            store.validate_private_path(&trash)?;
            let entry = entry?;
            let generation = parse_remote_gc_trash_name(&entry.file_name()).ok_or_else(|| {
                invalid_data(format!(
                    "unexpected path in remote history GC trash {}",
                    entry.path().display()
                ))
            })?;
            let directory = entry.path();
            validate_remote_gc_trash_generation_directory(
                store,
                source_id,
                redaction_profile,
                &generation,
                &directory,
            )?;
            if catalog.generations.contains_key(&generation)
                || catalog.trash.insert(generation, directory).is_some()
            {
                return Err(invalid_data(
                    "remote history generation exists in multiple live or GC trash entries",
                ));
            }
        }
    }
    Ok(catalog)
}

/// Reserves capacity for `candidate` while the caller holds the exclusive
/// remote-history root lock. Returning `true` means the candidate's live
/// directory already exists, so a crash replay remains legal at the cap.
fn validate_remote_generation_capacity_locked(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    root: &Path,
    candidate: &SourceHistoryRemoteGenerationId,
) -> io::Result<bool> {
    candidate.validate()?;
    if let Some(active) =
        store.read_remote_active_manifest_locked(source_id, redaction_profile, root)?
    {
        store.validate_remote_generation_binding_locked(
            source_id,
            redaction_profile,
            &active.active_generation,
            &active.binding,
        )?;
    }
    let catalog = load_remote_history_generation_catalog_locked(
        store,
        source_id,
        redaction_profile,
        root,
        false,
    )?;
    if catalog.generations.contains_key(candidate) {
        return Ok(true);
    }
    if catalog.trash.contains_key(candidate) {
        return Err(invalid_data(
            "remote history generation ID is still being collected",
        ));
    }
    if catalog.len() >= MAX_REMOTE_HISTORY_GENERATIONS {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!("remote history generation limit of {MAX_REMOTE_HISTORY_GENERATIONS} reached"),
        ));
    }
    Ok(false)
}

/// Accepts the bounded set of generation-root states that our atomic create
/// and COW flows can leave behind. It intentionally does not require a ready
/// generation: an empty directory, a metadata temporary, or a partially
/// cloned replacement are recoverable crash states and must still consume a
/// slot. Unknown entries or metadata bound to another namespace fail closed.
fn validate_remote_generation_capacity_entry_locked(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    generation: &SourceHistoryRemoteGenerationId,
    directory: &Path,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    let mut metadata = None;
    let mut buckets = false;
    let mut digests = false;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        match entry.file_name().to_str() {
            Some(REMOTE_GENERATION_METADATA_FILE) if metadata.is_none() => {
                validate_remote_gc_regular_file(&path, MAX_REMOTE_GENERATION_FILE_BYTES, None)?;
                let stored: RemoteGenerationMetadata =
                    read_json_file(&path, MAX_REMOTE_GENERATION_FILE_BYTES)?;
                stored.validate(&store.profile_id, source_id, redaction_profile, generation)?;
                metadata = Some(stored);
            }
            Some(BUCKETS_DIRECTORY) if !buckets => {
                store.validate_private_path(&path)?;
                buckets = true;
            }
            Some(DIGESTS_DIRECTORY) if !digests => {
                store.validate_private_path(&path)?;
                digests = true;
            }
            _ if is_remote_atomic_temporary_file(
                &entry.file_name(),
                REMOTE_GENERATION_METADATA_FILE,
            ) =>
            {
                validate_remote_gc_regular_file(&path, MAX_REMOTE_GENERATION_FILE_BYTES, None)?;
            }
            _ => {
                return Err(invalid_data(format!(
                    "unexpected path in remote history generation {}",
                    path.display()
                )));
            }
        }
    }

    if metadata.is_none() && (buckets || digests) {
        return Err(invalid_data(
            "remote history generation families exist without generation metadata",
        ));
    }
    if digests && !buckets {
        return Err(invalid_data(
            "remote history generation digest family exists before its bucket family",
        ));
    }
    if metadata.as_ref().is_some_and(|metadata| {
        matches!(
            metadata.origin,
            RemoteGenerationOrigin::ActiveReplacement { .. }
        ) && metadata.clone_complete
            && (!buckets || !digests)
    }) {
        return Err(invalid_data(
            "completed remote replacement generation is missing a history family",
        ));
    }
    Ok(())
}

fn validate_remote_gc_root_namespace(store: &SourceHistoryStore, root: &Path) -> io::Result<()> {
    store.validate_private_path(root)?;
    for entry in fs::read_dir(root)? {
        store.validate_private_path(root)?;
        let entry = entry?;
        let path = entry.path();
        match entry.file_name().to_str() {
            Some(REMOTE_HISTORY_LOCK_FILE) => {
                validate_lock_metadata(&path, &fs::symlink_metadata(&path)?)?;
            }
            Some(REMOTE_ACTIVE_MANIFEST_FILE) => {
                validate_remote_gc_regular_file(&path, MAX_REMOTE_GENERATION_FILE_BYTES, None)?;
            }
            Some(REMOTE_GENERATIONS_DIRECTORY) | Some(REMOTE_GC_TRASH_DIRECTORY) => {
                store.validate_private_path(&path)?;
            }
            _ => {
                return Err(invalid_data(format!(
                    "unexpected path in remote history GC root {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_remote_gc_generation_parent(
    store: &SourceHistoryStore,
    generations: &Path,
) -> io::Result<()> {
    if !store.private_directory_exists(generations)? {
        return Ok(());
    }
    store.validate_private_path(generations)?;
    for entry in fs::read_dir(generations)? {
        store.validate_private_path(generations)?;
        let entry = entry?;
        entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("remote history generation name is not UTF-8"))?
            .parse::<SourceHistoryRemoteGenerationId>()
            .map_err(|_| {
                invalid_data(format!(
                    "unexpected path in remote history generation parent {}",
                    entry.path().display()
                ))
            })?;
        store.validate_private_path(&entry.path())?;
    }
    Ok(())
}

fn validate_remote_gc_trash_parent(store: &SourceHistoryStore, trash: &Path) -> io::Result<()> {
    store.validate_private_path(trash)?;
    for entry in fs::read_dir(trash)? {
        store.validate_private_path(trash)?;
        let entry = entry?;
        parse_remote_gc_trash_name(&entry.file_name()).ok_or_else(|| {
            invalid_data(format!(
                "unexpected path in remote history GC trash {}",
                entry.path().display()
            ))
        })?;
        store.validate_private_path(&entry.path())?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct RemoteGcTreeUsage {
    entries: u64,
    bytes: u64,
}

impl RemoteGcTreeUsage {
    fn add_directory(&mut self) -> io::Result<()> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| invalid_data("remote history GC entry count overflowed"))?;
        self.validate()
    }

    fn add_file(&mut self, bytes: u64) -> io::Result<()> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| invalid_data("remote history GC entry count overflowed"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid_data("remote history GC byte count overflowed"))?;
        self.validate()
    }

    fn validate(self) -> io::Result<()> {
        if self.entries > MAX_REMOTE_GC_TREE_ENTRIES {
            return Err(invalid_data(
                "remote history generation exceeds the GC entry bound",
            ));
        }
        if self.bytes > MAX_REMOTE_GC_TREE_BYTES {
            return Err(invalid_data(
                "remote history generation exceeds the GC byte bound",
            ));
        }
        Ok(())
    }
}

fn validate_remote_gc_generation_directory(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    generation: &SourceHistoryRemoteGenerationId,
    directory: &Path,
) -> io::Result<()> {
    validate_remote_gc_generation_directory_mode(
        store,
        source_id,
        redaction_profile,
        generation,
        directory,
        true,
    )
}

fn validate_remote_gc_trash_generation_directory(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    generation: &SourceHistoryRemoteGenerationId,
    directory: &Path,
) -> io::Result<()> {
    validate_remote_gc_generation_directory_mode(
        store,
        source_id,
        redaction_profile,
        generation,
        directory,
        false,
    )
}

fn validate_remote_gc_generation_directory_mode(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    generation: &SourceHistoryRemoteGenerationId,
    directory: &Path,
    require_complete: bool,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    let mut usage = RemoteGcTreeUsage::default();
    usage.add_directory()?;
    let mut metadata = false;
    let mut buckets = false;
    let mut digests = false;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        match entry.file_name().to_str() {
            Some(REMOTE_GENERATION_METADATA_FILE) if !metadata => {
                validate_remote_gc_regular_file(
                    &path,
                    MAX_REMOTE_GENERATION_FILE_BYTES,
                    Some(&mut usage),
                )?;
                metadata = true;
            }
            Some(BUCKETS_DIRECTORY) if !buckets => {
                store.validate_private_path(&path)?;
                usage.add_directory()?;
                validate_remote_gc_family(store, &path, RemoteCloneFamily::Buckets, &mut usage)?;
                buckets = true;
            }
            Some(DIGESTS_DIRECTORY) if !digests => {
                store.validate_private_path(&path)?;
                usage.add_directory()?;
                validate_remote_gc_family(store, &path, RemoteCloneFamily::Digests, &mut usage)?;
                digests = true;
            }
            _ if is_remote_atomic_temporary_file(
                &entry.file_name(),
                REMOTE_GENERATION_METADATA_FILE,
            ) =>
            {
                validate_remote_gc_regular_file(
                    &path,
                    MAX_REMOTE_GENERATION_FILE_BYTES,
                    Some(&mut usage),
                )?;
            }
            _ => {
                return Err(invalid_data(format!(
                    "unexpected path in remote history GC generation {}",
                    path.display()
                )));
            }
        }
    }

    if require_complete && (!metadata || !buckets || !digests) {
        return Err(invalid_data(
            "remote history GC candidate namespace is incomplete",
        ));
    }
    if !metadata && (buckets || digests) {
        return Err(invalid_data(
            "partially deleted remote history GC trash lost its metadata out of order",
        ));
    }
    if metadata {
        let stored: RemoteGenerationMetadata = read_json_file(
            &directory.join(REMOTE_GENERATION_METADATA_FILE),
            MAX_REMOTE_GENERATION_FILE_BYTES,
        )?;
        if require_complete {
            stored.validate_ready(&store.profile_id, source_id, redaction_profile, generation)?;
        } else {
            stored.validate(&store.profile_id, source_id, redaction_profile, generation)?;
        }
    }
    Ok(())
}

fn validate_remote_gc_family(
    store: &SourceHistoryStore,
    directory: &Path,
    family: RemoteCloneFamily,
    usage: &mut RemoteGcTreeUsage,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == family.lock_name() {
            let metadata = fs::symlink_metadata(&path)?;
            validate_lock_metadata(&path, &metadata)?;
            if metadata.len() > MAX_REMOTE_GENERATION_FILE_BYTES {
                return Err(invalid_data("remote history GC lock file is too large"));
            }
            usage.add_file(metadata.len())?;
            continue;
        }
        let published = family.shard_day(&path).is_some();
        let temporary = remote_atomic_temporary_target(&entry.file_name())
            .is_some_and(|target| family.shard_day(Path::new(target)).is_some());
        if !published && !temporary {
            return Err(invalid_data(format!(
                "unexpected path in remote history GC family {}",
                path.display()
            )));
        }
        validate_remote_gc_regular_file(&path, family.maximum_shard_bytes(), Some(usage))?;
    }
    Ok(())
}

fn validate_remote_gc_regular_file(
    path: &Path,
    maximum: u64,
    usage: Option<&mut RemoteGcTreeUsage>,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_published_private_file(path)?;
    if metadata.len() > maximum {
        return Err(invalid_data(
            "remote history GC file exceeds its family byte bound",
        ));
    }
    if let Some(usage) = usage {
        usage.add_file(metadata.len())?;
    }
    Ok(())
}

fn remove_remote_gc_generation_tree(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    for family_name in [BUCKETS_DIRECTORY, DIGESTS_DIRECTORY] {
        let family = directory.join(family_name);
        match fs::symlink_metadata(&family) {
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
                    return Err(invalid_data(format!(
                        "remote history GC family {} is not a real directory",
                        family.display()
                    )));
                }
                store.validate_private_path(&family)?;
                for entry in fs::read_dir(&family)? {
                    store.validate_private_path(&family)?;
                    let path = entry?.path();
                    let metadata = fs::symlink_metadata(&path)?;
                    validate_data_file_metadata(&path, &metadata)?;
                    fs::remove_file(&path)?;
                }
                store.validate_private_path(&family)?;
                fs::remove_dir(&family)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    store.validate_private_path(directory)?;
    let mut metadata_temps = Vec::new();
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        if is_remote_atomic_temporary_file(&entry.file_name(), REMOTE_GENERATION_METADATA_FILE) {
            metadata_temps.push(entry.path());
        }
    }
    for path in metadata_temps {
        validate_published_private_file(&path)?;
        fs::remove_file(path)?;
    }
    let metadata_path = directory.join(REMOTE_GENERATION_METADATA_FILE);
    match fs::symlink_metadata(&metadata_path) {
        Ok(metadata) => {
            validate_data_file_metadata(&metadata_path, &metadata)?;
            fs::remove_file(&metadata_path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    store.validate_private_path(directory)?;
    if fs::read_dir(directory)?.next().transpose()?.is_some() {
        return Err(invalid_data(
            "remote history GC trash contains unexpected remaining entries",
        ));
    }
    fs::remove_dir(directory)
}

#[cfg(not(windows))]
fn rename_remote_generation_to_trash(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn rename_remote_generation_to_trash(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows paths cannot contain NUL characters",
            ));
        }
        encoded.push(0);
        Ok(encoded)
    }

    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    // SAFETY: both buffers are NUL-terminated and remain alive for the call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            remote_gc_windows_move_flags(),
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(any(windows, test))]
fn remote_gc_windows_move_flags() -> u32 {
    // MOVEFILE_WRITE_THROUGH. Deliberately omit MOVEFILE_REPLACE_EXISTING so a
    // pre-existing trash namespace can never be overwritten.
    0x0000_0008
}

impl RemoteCloneFamily {
    fn lock_name(self) -> &'static OsStr {
        match self {
            Self::Buckets => OsStr::new(BUCKETS_LOCK_FILE),
            Self::Digests => OsStr::new(DIGESTS_LOCK_FILE),
        }
    }

    fn maximum_shard_bytes(self) -> u64 {
        match self {
            Self::Buckets => MAX_SHARD_FILE_BYTES,
            Self::Digests => MAX_COMPRESSED_EVIDENCE_SHARD_BYTES,
        }
    }

    fn atomic_shard_kind(self) -> AtomicShardFileKind {
        match self {
            Self::Buckets => AtomicShardFileKind::Json,
            Self::Digests => AtomicShardFileKind::GzipJson,
        }
    }

    fn shard_day(self, path: &Path) -> Option<NaiveDate> {
        match self {
            Self::Buckets => shard_day_from_path(path),
            Self::Digests => {
                let name = path.file_name()?.to_str()?;
                NaiveDate::parse_from_str(name.strip_suffix(".json.gz")?, "%Y-%m-%d").ok()
            }
        }
    }

    fn validate_shard(
        self,
        path: &Path,
        profile_id: &HistoryProfileId,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        day: NaiveDate,
    ) -> io::Result<()> {
        match self {
            Self::Buckets => {
                read_source_bucket_shard(path, profile_id, source_id, redaction_profile, day)?
                    .ok_or_else(|| invalid_data("remote clone bucket shard disappeared"))?;
                Ok(())
            }
            Self::Digests => validate_digest_shard_for_remote_clone(
                path,
                profile_id,
                source_id,
                redaction_profile,
                day,
            ),
        }
    }
}

fn clone_remote_generation_family(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    source_directory: &Path,
    destination_directory: &Path,
    family: RemoteCloneFamily,
) -> io::Result<()> {
    if source_directory == destination_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote history clone source and destination must differ",
        ));
    }
    store.validate_private_path(source_directory)?;
    store.validate_private_path(destination_directory)?;

    // An interrupted atomic shard write leaves a target-bound temporary in
    // the family directory. Recover it before enumerating the immutable
    // baseline, and keep both family locks for the complete clone so a writer
    // cannot introduce a new temporary between cleanup and enumeration.
    let source_lock_name = family
        .lock_name()
        .to_str()
        .expect("remote family lock names are ASCII");
    let source_lock = open_lock_file(source_directory, source_lock_name)?;
    lock_exclusive(&source_lock, source_directory, source_lock_name)?;
    cleanup_atomic_shard_temporary_files(store, source_directory, family.atomic_shard_kind())?;
    let destination_lock = open_lock_file(destination_directory, source_lock_name)?;
    lock_exclusive(&destination_lock, destination_directory, source_lock_name)?;
    cleanup_atomic_shard_temporary_files(store, destination_directory, family.atomic_shard_kind())?;

    let source_entries = remote_clone_shard_entries(store, source_directory, family, true)?;
    let destination_entries =
        remote_clone_shard_entries(store, destination_directory, family, true)?;
    if source_entries.len() > MAX_REMOTE_CLONE_SHARDS_PER_FAMILY
        || destination_entries.len() > MAX_REMOTE_CLONE_SHARDS_PER_FAMILY
    {
        return Err(invalid_data(
            "remote history generation has too many shards to clone",
        ));
    }
    if destination_entries
        .keys()
        .any(|name| !source_entries.contains_key(name))
    {
        return Err(invalid_data(
            "remote replacement contains a shard outside its expected baseline",
        ));
    }

    let mut total_bytes = 0_u64;
    for (name, (day, source_path, source_bytes)) in source_entries {
        total_bytes = total_bytes
            .checked_add(source_bytes)
            .ok_or_else(|| invalid_data("remote history generation clone byte count overflowed"))?;
        if total_bytes > MAX_REMOTE_CLONE_TOTAL_BYTES {
            return Err(invalid_data(
                "remote history generation is too large to clone",
            ));
        }
        let destination_path = destination_directory.join(&name);
        if destination_entries.contains_key(&name) {
            ensure_private_files_equal(
                &source_path,
                &destination_path,
                family.maximum_shard_bytes(),
            )?;
        } else if fs::hard_link(&source_path, &destination_path).is_err() {
            let contents = read_private_file_bounded(&source_path, family.maximum_shard_bytes())?;
            write_private_atomically(&destination_path, &contents)?;
        }
        validate_published_private_file(&destination_path)?;
        family.validate_shard(
            &destination_path,
            &store.profile_id,
            source_id,
            redaction_profile,
            day,
        )?;
    }
    sync_directory(destination_directory)
}

fn remote_clone_shard_entries(
    store: &SourceHistoryStore,
    directory: &Path,
    family: RemoteCloneFamily,
    allow_lock: bool,
) -> io::Result<BTreeMap<OsString, (NaiveDate, PathBuf, u64)>> {
    store.validate_private_path(directory)?;
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let name = entry.file_name();
        if allow_lock && name == family.lock_name() {
            validate_lock_metadata(&entry.path(), &fs::symlink_metadata(entry.path())?)?;
            continue;
        }
        let path = entry.path();
        let day = family.shard_day(&path).ok_or_else(|| {
            invalid_data(format!(
                "unexpected path in remote history clone source {}",
                path.display()
            ))
        })?;
        validate_published_private_file(&path)?;
        let bytes = fs::symlink_metadata(&path)?.len();
        if bytes > family.maximum_shard_bytes() {
            return Err(invalid_data(
                "remote history clone shard exceeds its byte bound",
            ));
        }
        if entries.insert(name, (day, path, bytes)).is_some() {
            return Err(invalid_data(
                "duplicate path in remote history clone namespace",
            ));
        }
    }
    Ok(entries)
}

fn ensure_recoverable_empty_generation_directory(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<()> {
    store.validate_private_path(directory)?;
    if fs::read_dir(directory)?.next().transpose()?.is_some() {
        return Err(invalid_data(
            "remote replacement generation is missing metadata but is not empty",
        ));
    }
    Ok(())
}

fn ensure_private_files_equal(left: &Path, right: &Path, maximum: u64) -> io::Result<()> {
    let left = read_private_file_bounded(left, maximum)?;
    let right = read_private_file_bounded(right, maximum)?;
    if left != right {
        return Err(invalid_data(
            "remote replacement baseline shard conflicts with its active origin",
        ));
    }
    Ok(())
}

fn read_private_file_bounded(path: &Path, maximum: u64) -> io::Result<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_data_file_metadata(path, &path_metadata)?;
    if path_metadata.len() > maximum {
        return Err(invalid_data("remote history clone shard is too large"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let subject = format!("remote history clone shard {}", path.display());
    let mut file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, &subject))?;
    let metadata = file.metadata()?;
    validate_data_file_metadata(path, &metadata)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &metadata, &subject)?;
    if metadata.len() > maximum {
        return Err(invalid_data("remote history clone shard is too large"));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > maximum {
        return Err(invalid_data("remote history clone shard is too large"));
    }
    Ok(contents)
}

fn remote_page_fingerprint(
    bucket_records: &[SourceBucketRecord],
    digest_records: &[SourceSessionDigestRecord],
) -> io::Result<String> {
    let mut writer = BoundedHashWriter::new(MAX_REMOTE_COW_FINGERPRINT_BYTES);
    writer.write_all(b"codex-usage-monit/remote-active-page/v1\0")?;
    serde_json::to_writer(&mut writer, &(bucket_records, digest_records)).map_err(|error| {
        invalid_data(format!(
            "could not fingerprint remote active history page: {error}"
        ))
    })?;
    let digest = writer.finish();
    let mut result = String::with_capacity(REMOTE_PAGE_FINGERPRINT_PREFIX.len() + 64);
    result.push_str(REMOTE_PAGE_FINGERPRINT_PREFIX);
    append_lower_hex(&mut result, &digest);
    Ok(result)
}

fn validate_page_fingerprint(value: &str) -> io::Result<()> {
    let Some(hex) = value.strip_prefix(REMOTE_PAGE_FINGERPRINT_PREFIX) else {
        return Err(invalid_data("remote active page fingerprint is invalid"));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_data("remote active page fingerprint is invalid"));
    }
    Ok(())
}

fn append_lower_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

struct BoundedHashWriter {
    hasher: Sha256,
    written: u64,
    maximum: u64,
}

impl BoundedHashWriter {
    fn new(maximum: u64) -> Self {
        Self {
            hasher: Sha256::new(),
            written: 0,
            maximum,
        }
    }

    fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

impl Write for BoundedHashWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buffer.len())
            .map_err(|_| invalid_data("remote active page fingerprint size overflowed"))?;
        self.written = self
            .written
            .checked_add(length)
            .ok_or_else(|| invalid_data("remote active page fingerprint size overflowed"))?;
        if self.written > self.maximum {
            return Err(invalid_data(
                "remote active history page exceeds its fingerprint byte bound",
            ));
        }
        Digest::update(&mut self.hasher, buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn require_ssh_source(source: &SourceMetadata) -> io::Result<()> {
    if source.kind() != SourceKind::Ssh {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote history generations require an SSH source",
        ));
    }
    Ok(())
}

fn validate_binding_does_not_roll_back(
    active: &SourceHistoryRemoteBinding,
    candidate: &SourceHistoryRemoteBinding,
) -> io::Result<()> {
    active.validate()?;
    candidate.validate()?;
    if candidate.source.node_id != active.source.node_id {
        return Err(invalid_data(
            "remote history activation cannot change source identity",
        ));
    }
    if candidate.source.generation < active.source.generation
        || candidate.revisions.history_format < active.revisions.history_format
        || candidate.revisions.metric < active.revisions.metric
        || candidate.revisions.estimator < active.revisions.estimator
        || candidate.revisions.project_breakdown < active.revisions.project_breakdown
        || candidate.revisions.api_pricing_catalog < active.revisions.api_pricing_catalog
    {
        return Err(invalid_data(
            "remote history activation would roll back source or protocol revisions",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;
    use crate::domain::{ApiCostAmount, TokenUsage};
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, LocalHalfHourBucket,
    };
    use crate::source_model::{SessionReplicaKey, ThreadId};

    const PROFILE: &str = "0123456789abcdef";
    const SOURCE: &str = "node-0123456789abcdef0123456789abcdef";
    const GENERATION_A: &str = "ingest-gen-11111111111111111111111111111111";
    const GENERATION_B: &str = "ingest-gen-22222222222222222222222222222222";
    const GENERATION_C: &str = "ingest-gen-33333333333333333333333333333333";

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn profile() -> HistoryProfileId {
        PROFILE.parse().unwrap()
    }

    fn source_id() -> NodeId {
        SOURCE.parse().unwrap()
    }

    fn generation(value: &str) -> SourceHistoryRemoteGenerationId {
        value.parse().unwrap()
    }

    fn numbered_generation(value: usize) -> SourceHistoryRemoteGenerationId {
        format!("{REMOTE_GENERATION_PREFIX}{value:032x}")
            .parse()
            .unwrap()
    }

    fn binding(source_generation: u64, revisions: [u32; 5]) -> SourceHistoryRemoteBinding {
        SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: source_id(),
                generation: NonZeroU64::new(source_generation).unwrap(),
            },
            ProtocolRevisions {
                history_format: NonZeroU32::new(revisions[0]).unwrap(),
                metric: NonZeroU32::new(revisions[1]).unwrap(),
                estimator: NonZeroU32::new(revisions[2]).unwrap(),
                project_breakdown: NonZeroU32::new(revisions[3]).unwrap(),
                api_pricing_catalog: NonZeroU32::new(revisions[4]).unwrap(),
            },
        )
        .unwrap()
    }

    fn default_binding() -> SourceHistoryRemoteBinding {
        binding(7, [1; 5])
    }

    fn store(state_root: PathBuf, kind: SourceKind) -> SourceHistoryStore {
        let store = SourceHistoryStore::new(state_root, profile());
        store
            .save_source_metadata(
                &SourceMetadata::new(source_id(), kind, "generation-test").unwrap(),
            )
            .unwrap();
        store
    }

    fn bucket(starts_at: DateTime<Utc>, total: u64) -> SourceBucketRecord {
        bucket_revision(1, starts_at, total)
    }

    fn bucket_revision(revision: u64, starts_at: DateTime<Utc>, total: u64) -> SourceBucketRecord {
        SourceBucketRecord::upsert(
            revision,
            LocalHalfHourBucket {
                starts_at,
                ends_at: starts_at + Duration::minutes(15),
                sampled_at: starts_at + Duration::minutes(15),
                token_usage: TokenUsage {
                    input_tokens: total,
                    total_tokens: total,
                    ..TokenUsage::default()
                },
                estimated_cost_units: u128::from(total),
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
                api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
                call_count: 1,
                groups: Vec::new(),
                project_groups: Vec::new(),
                partial_reasons: Vec::new(),
            },
        )
        .unwrap()
    }

    fn digest(thread: &str, starts_at: DateTime<Utc>, total: u64) -> SourceSessionDigestRecord {
        digest_revision(1, thread, starts_at, total)
    }

    fn digest_revision(
        revision: u64,
        thread: &str,
        starts_at: DateTime<Utc>,
        total: u64,
    ) -> SourceSessionDigestRecord {
        let ends_at = starts_at + Duration::minutes(15);
        let digest = SourceSessionDigest::new(
            SessionReplicaKey::new(source_id(), thread.parse::<ThreadId>().unwrap()),
            starts_at,
            ends_at,
            ends_at,
            format!("session-digest-sha256-v1-{}", "a".repeat(64))
                .parse()
                .unwrap(),
            format!("session-digest-sha256-v1-{}", "b".repeat(64))
                .parse()
                .unwrap(),
            1,
            true,
            true,
            Vec::new(),
            SessionUsageMetrics {
                token_usage: TokenUsage {
                    input_tokens: total,
                    total_tokens: total,
                    ..TokenUsage::default()
                },
                estimated_cost_units: u128::from(total),
                api_long_context_extra_cost_units: Some(0),
                api_equivalent_cost: ApiCostAmount::default(),
                call_count: 1,
                metric_revision: 1,
                estimator_revision: 1,
                project_breakdown_revision: 1,
                api_pricing_catalog_revision: 1,
                partial_reasons: Vec::new(),
            },
        )
        .unwrap();
        SourceSessionDigestRecord::upsert(revision, digest).unwrap()
    }

    fn bucket_total(record: &SourceBucketRecord) -> u64 {
        match record.change() {
            SourceBucketChange::Upsert(bucket) => bucket.token_usage.total_tokens,
            SourceBucketChange::Tombstone => 0,
        }
    }

    fn digest_thread(record: &SourceSessionDigestRecord) -> &str {
        record.thread_id().as_str()
    }

    fn digest_total(record: &SourceSessionDigestRecord) -> u64 {
        match record.change() {
            SourceSessionDigestChange::Upsert(digest) => digest.metrics().token_usage.total_tokens,
            SourceSessionDigestChange::Tombstone => 0,
        }
    }

    fn activate_initial_generation(
        history: &SourceHistoryStore,
        source: &NodeId,
        redaction: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
    ) -> SourceHistoryRemoteActiveRef {
        history
            .ensure_remote_history_generation(source, redaction, generation, binding)
            .unwrap();
        history
            .apply_remote_history_generation_page(
                source,
                redaction,
                generation,
                binding,
                &[bucket(at(10, 0), 10)],
                &[digest("old-thread", at(10, 0), 10)],
            )
            .unwrap();
        history
            .activate_remote_history_generation(
                source,
                redaction,
                None,
                generation,
                binding,
                at(11, 0),
            )
            .unwrap();
        SourceHistoryRemoteActiveRef::new(generation.clone(), binding.clone()).unwrap()
    }

    #[test]
    fn generation_ids_are_fixed_lowercase_path_components() {
        assert_eq!(generation(GENERATION_A).as_str(), GENERATION_A);
        for invalid in [
            "ingest-gen-00000000000000000000000000000000",
            "ingest-gen-1111",
            "ingest-gen-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "../ingest-gen-11111111111111111111111111111111",
            "ingest-gen-1111111111111111111111111111111/",
            "CON",
        ] {
            assert!(invalid.parse::<SourceHistoryRemoteGenerationId>().is_err());
        }
    }

    #[test]
    fn remote_generation_cap_allows_existing_replays_and_rejects_new_bootstrap_and_cow() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let binding = default_binding();
        let active = numbered_generation(1);
        let active_ref =
            activate_initial_generation(&history, &source, redaction, &active, &binding);
        let replacement = numbered_generation(2);
        let bucket_page = vec![bucket(at(10, 15), 20)];
        let digest_page = vec![digest("new-thread", at(10, 15), 20)];
        history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &active_ref,
                &replacement,
                &binding,
                &bucket_page,
                &digest_page,
                at(11, 15),
            )
            .unwrap();

        // These structural root entries are deliberately present before
        // filling the cap. Only controlled generation identities beneath
        // generations/ or gc-trash/ count toward the bound.
        let root = history.source_remote_history_directory(&source, redaction);
        history
            .prepare_private_directory(&root.join(REMOTE_GC_TRASH_DIRECTORY))
            .unwrap();
        assert!(root.join(REMOTE_HISTORY_LOCK_FILE).is_file());
        assert!(root.join(REMOTE_ACTIVE_MANIFEST_FILE).is_file());

        for value in 3..=MAX_REMOTE_HISTORY_GENERATIONS {
            history
                .ensure_remote_history_generation(
                    &source,
                    redaction,
                    &numbered_generation(value),
                    &binding,
                )
                .unwrap();
        }

        // An exact generation replay must remain usable at capacity.
        history
            .ensure_remote_history_generation(&source, redaction, &active, &binding)
            .unwrap();
        let replay = history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &active_ref,
                &replacement,
                &binding,
                &bucket_page,
                &digest_page,
                at(11, 15),
            )
            .unwrap();
        assert_eq!(replay, RemoteHistoryPageWriteReport::default());

        let bootstrap_overflow = numbered_generation(MAX_REMOTE_HISTORY_GENERATIONS + 1);
        let error = history
            .ensure_remote_history_generation(&source, redaction, &bootstrap_overflow, &binding)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(
            !history
                .source_remote_history_generation_directory(
                    &source,
                    redaction,
                    &bootstrap_overflow,
                )
                .exists()
        );

        let cow_overflow = numbered_generation(MAX_REMOTE_HISTORY_GENERATIONS + 2);
        let replacement_active =
            SourceHistoryRemoteActiveRef::new(replacement.clone(), binding.clone()).unwrap();
        let error = history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &replacement_active,
                &cow_overflow,
                &binding,
                &[bucket(at(10, 30), 30)],
                &[digest("newer-thread", at(10, 30), 30)],
                at(11, 30),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(
            !history
                .source_remote_history_generation_directory(&source, redaction, &cow_overflow)
                .exists()
        );
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(replacement)
        );
    }

    #[test]
    fn bounded_sweep_reclaims_thirty_two_orphans_and_restores_capacity() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let binding = default_binding();
        for value in 1..=MAX_REMOTE_HISTORY_GENERATIONS {
            history
                .ensure_remote_history_generation(
                    &source,
                    redaction,
                    &numbered_generation(value),
                    &binding,
                )
                .unwrap();
        }

        assert_eq!(
            history
                .sweep_remote_history_generations(&source, redaction, &BTreeSet::new(), 0)
                .unwrap(),
            RemoteHistoryGenerationSweepReport {
                deleted: 0,
                recovered: 0,
                skipped: 0,
                remaining: MAX_REMOTE_HISTORY_GENERATIONS,
            }
        );

        for expected_remaining in [24, 16, 8, 0] {
            let report = history
                .sweep_remote_history_generations(&source, redaction, &BTreeSet::new(), 8)
                .unwrap();
            assert_eq!(report.deleted, 8);
            assert_eq!(report.recovered, 0);
            assert_eq!(report.skipped, 0);
            assert_eq!(report.remaining, expected_remaining);
            assert!(report.deleted + report.recovered <= 8);
        }

        history
            .ensure_remote_history_generation(
                &source,
                redaction,
                &numbered_generation(MAX_REMOTE_HISTORY_GENERATIONS + 1),
                &binding,
            )
            .unwrap();
    }

    #[test]
    fn sweep_keeps_all_traced_roots_and_capacity_remains_full() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let binding = default_binding();
        let mut protected = BTreeSet::new();
        for value in 1..=MAX_REMOTE_HISTORY_GENERATIONS {
            let generation = numbered_generation(value);
            history
                .ensure_remote_history_generation(&source, redaction, &generation, &binding)
                .unwrap();
            protected.insert(generation);
        }
        let active = numbered_generation(1);
        history
            .activate_remote_history_generation(
                &source,
                redaction,
                None,
                &active,
                &binding,
                at(11, 0),
            )
            .unwrap();
        protected.remove(&active);

        assert_eq!(
            history
                .sweep_remote_history_generations(&source, redaction, &protected, 8)
                .unwrap(),
            RemoteHistoryGenerationSweepReport {
                deleted: 0,
                recovered: 0,
                skipped: MAX_REMOTE_HISTORY_GENERATIONS,
                remaining: 0,
            }
        );
        assert_eq!(
            history
                .ensure_remote_history_generation(
                    &source,
                    redaction,
                    &numbered_generation(MAX_REMOTE_HISTORY_GENERATIONS + 1),
                    &binding,
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
    }

    #[test]
    fn deterministic_trash_consumes_capacity_and_sweep_recovers_it_first() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let binding = default_binding();
        for value in 1..=MAX_REMOTE_HISTORY_GENERATIONS {
            history
                .ensure_remote_history_generation(
                    &source,
                    redaction,
                    &numbered_generation(value),
                    &binding,
                )
                .unwrap();
        }

        let retired = numbered_generation(1);
        let root = history.source_remote_history_directory(&source, redaction);
        let generations = root.join(REMOTE_GENERATIONS_DIRECTORY);
        let trash = root.join(REMOTE_GC_TRASH_DIRECTORY);
        history.prepare_private_directory(&trash).unwrap();
        let original =
            history.source_remote_history_generation_directory(&source, redaction, &retired);
        let trash_generation = trash.join(remote_gc_trash_name(&retired));
        rename_remote_generation_to_trash(&original, &trash_generation).unwrap();
        sync_directory(&generations).unwrap();
        sync_directory(&trash).unwrap();

        let next = numbered_generation(MAX_REMOTE_HISTORY_GENERATIONS + 1);
        assert_eq!(
            history
                .ensure_remote_history_generation(&source, redaction, &next, &binding)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(
            history
                .ensure_remote_history_generation(&source, redaction, &retired, &binding)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        assert_eq!(
            history
                .sweep_remote_history_generations(&source, redaction, &BTreeSet::new(), 1)
                .unwrap(),
            RemoteHistoryGenerationSweepReport {
                deleted: 0,
                recovered: 1,
                skipped: 0,
                remaining: MAX_REMOTE_HISTORY_GENERATIONS - 1,
            }
        );
        assert!(!trash_generation.exists());
        history
            .ensure_remote_history_generation(&source, redaction, &next, &binding)
            .unwrap();
    }

    #[test]
    fn remote_generation_capacity_fails_closed_on_unknown_or_non_directory_entries() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let binding = default_binding();
        let first = numbered_generation(1);
        history
            .ensure_remote_history_generation(&source, redaction, &first, &binding)
            .unwrap();
        let generations = history
            .source_remote_history_directory(&source, redaction)
            .join(REMOTE_GENERATIONS_DIRECTORY);

        let unknown = generations.join("unknown-generation");
        history.prepare_private_directory(&unknown).unwrap();
        let error = history
            .ensure_remote_history_generation(&source, redaction, &numbered_generation(2), &binding)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir(&unknown).unwrap();

        let malformed = generations.join(numbered_generation(4).as_str());
        history.prepare_private_directory(&malformed).unwrap();
        let unexpected = malformed.join("unexpected-entry");
        write_private_atomically(&unexpected, b"not a generation root entry\n").unwrap();
        let error = history
            .ensure_remote_history_generation(&source, redaction, &numbered_generation(2), &binding)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_file(unexpected).unwrap();
        fs::remove_dir(malformed).unwrap();

        // A syntactically valid generation name must still be a real private
        // directory. This rejects regular files, Unix links, and Windows
        // reparse points through validate_private_path.
        let non_directory = generations.join(numbered_generation(3).as_str());
        write_private_atomically(&non_directory, b"not a generation directory\n").unwrap();
        let error = history
            .ensure_remote_history_generation(&source, redaction, &numbered_generation(2), &binding)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(non_directory.is_file());
    }

    #[test]
    fn remote_generation_gc_never_deletes_active_or_explicitly_protected_roots() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let active = generation(GENERATION_A);
        let protected_generation = generation(GENERATION_B);
        let binding = default_binding();
        activate_initial_generation(&history, &source, redaction, &active, &binding);
        history
            .ensure_remote_history_generation(&source, redaction, &protected_generation, &binding)
            .unwrap();

        assert_eq!(
            history
                .garbage_collect_remote_history_generation(
                    &source,
                    redaction,
                    &active,
                    &BTreeSet::new(),
                )
                .unwrap(),
            RemoteHistoryGenerationGcOutcome::SkippedActive
        );
        assert!(
            history
                .source_remote_history_generation_directory(&source, redaction, &active)
                .is_dir()
        );

        let protected = BTreeSet::from([protected_generation.clone()]);
        assert_eq!(
            history
                .garbage_collect_remote_history_generation(
                    &source,
                    redaction,
                    &protected_generation,
                    &protected,
                )
                .unwrap(),
            RemoteHistoryGenerationGcOutcome::SkippedProtected
        );
        assert!(
            history
                .source_remote_history_generation_directory(
                    &source,
                    redaction,
                    &protected_generation,
                )
                .is_dir()
        );
    }

    #[test]
    fn remote_generation_gc_deletes_a_valid_retired_generation() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let retired = generation(GENERATION_A);
        let active = generation(GENERATION_B);
        let binding = default_binding();
        let retired_ref =
            activate_initial_generation(&history, &source, redaction, &retired, &binding);
        history
            .ensure_remote_history_generation(&source, redaction, &active, &binding)
            .unwrap();
        history
            .activate_remote_history_generation(
                &source,
                redaction,
                Some(&retired_ref),
                &active,
                &binding,
                at(11, 15),
            )
            .unwrap();

        assert_eq!(
            history
                .garbage_collect_remote_history_generation(
                    &source,
                    redaction,
                    &retired,
                    &BTreeSet::from([active.clone()]),
                )
                .unwrap(),
            RemoteHistoryGenerationGcOutcome::Deleted
        );
        assert!(
            !history
                .source_remote_history_generation_directory(&source, redaction, &retired)
                .exists()
        );
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(active)
        );
    }

    #[test]
    fn remote_generation_gc_resumes_its_exact_partially_deleted_trash() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let candidate = generation(GENERATION_A);
        let binding = default_binding();
        history
            .ensure_remote_history_generation(&source, redaction, &candidate, &binding)
            .unwrap();
        history
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &candidate,
                &binding,
                &[bucket(at(10, 0), 5)],
                &[digest("trash-thread", at(10, 0), 5)],
            )
            .unwrap();

        let root = history.source_remote_history_directory(&source, redaction);
        let generations = root.join(REMOTE_GENERATIONS_DIRECTORY);
        let trash = root.join(REMOTE_GC_TRASH_DIRECTORY);
        history.prepare_private_directory(&trash).unwrap();
        let original =
            history.source_remote_history_generation_directory(&source, redaction, &candidate);
        let trash_generation = trash.join(remote_gc_trash_name(&candidate));
        rename_remote_generation_to_trash(&original, &trash_generation).unwrap();
        // Model a crash halfway through deletion: one family is already gone,
        // but generation metadata remains as the recovery authority.
        let buckets = trash_generation.join(BUCKETS_DIRECTORY);
        for entry in fs::read_dir(&buckets).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        fs::remove_dir(&buckets).unwrap();
        sync_directory(&generations).unwrap();
        sync_directory(&trash).unwrap();

        assert_eq!(
            history
                .garbage_collect_remote_history_generation(
                    &source,
                    redaction,
                    &candidate,
                    &BTreeSet::new(),
                )
                .unwrap(),
            RemoteHistoryGenerationGcOutcome::RecoveredTrash
        );
        assert!(!trash_generation.exists());
    }

    #[test]
    fn remote_generation_gc_rejects_unknown_generation_and_trash_entries() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let candidate = generation(GENERATION_A);
        let binding = default_binding();
        history
            .ensure_remote_history_generation(&source, redaction, &candidate, &binding)
            .unwrap();
        let candidate_directory =
            history.source_remote_history_generation_directory(&source, redaction, &candidate);
        let unknown = candidate_directory
            .join(BUCKETS_DIRECTORY)
            .join(".unknown-gc-file");
        write_private_atomically(&unknown, b"unknown\n").unwrap();
        assert_eq!(
            history
                .garbage_collect_remote_history_generation(
                    &source,
                    redaction,
                    &candidate,
                    &BTreeSet::new(),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            history
                .sweep_remote_history_generations(&source, redaction, &BTreeSet::new(), 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(candidate_directory.is_dir());

        fs::remove_file(unknown).unwrap();
        let trash = history
            .source_remote_history_directory(&source, redaction)
            .join(REMOTE_GC_TRASH_DIRECTORY);
        history.prepare_private_directory(&trash).unwrap();
        write_private_atomically(&trash.join(".unknown"), b"unknown\n").unwrap();
        assert_eq!(
            history
                .garbage_collect_remote_history_generation(
                    &source,
                    redaction,
                    &candidate,
                    &BTreeSet::new(),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            history
                .sweep_remote_history_generations(&source, redaction, &BTreeSet::new(), 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(candidate_directory.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn remote_generation_gc_refuses_symlinks_before_rename() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let candidate = generation(GENERATION_A);
        let binding = default_binding();
        history
            .ensure_remote_history_generation(&source, redaction, &candidate, &binding)
            .unwrap();
        let candidate_directory =
            history.source_remote_history_generation_directory(&source, redaction, &candidate);
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"outside\n").unwrap();
        symlink(
            &outside,
            candidate_directory
                .join(BUCKETS_DIRECTORY)
                .join("2026-08-30.json"),
        )
        .unwrap();

        assert_eq!(
            history
                .garbage_collect_remote_history_generation(
                    &source,
                    redaction,
                    &candidate,
                    &BTreeSet::new(),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            history
                .sweep_remote_history_generations(&source, redaction, &BTreeSet::new(), 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(candidate_directory.is_dir());
        assert_eq!(fs::read(outside).unwrap(), b"outside\n");
    }

    #[test]
    fn windows_gc_rename_policy_is_write_through_and_never_replaces() {
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

        let flags = remote_gc_windows_move_flags();
        assert_ne!(flags & MOVEFILE_WRITE_THROUGH, 0);
        assert_eq!(flags & MOVEFILE_REPLACE_EXISTING, 0);
    }

    #[test]
    fn local_source_keeps_direct_bucket_layout_and_query_behavior() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Local);
        let record = bucket(at(10, 0), 7);
        history
            .record_source_bucket_changes(
                &source_id(),
                RedactionProfile::Redacted,
                std::slice::from_ref(&record),
            )
            .unwrap();

        let loaded = history
            .load_source_records_since(&source_id(), RedactionProfile::Redacted, at(9, 0))
            .unwrap();
        assert_eq!(loaded.records, vec![record]);
        assert!(
            !history
                .source_remote_history_directory(&source_id(), RedactionProfile::Redacted)
                .exists()
        );
        assert_eq!(
            history
                .ensure_remote_history_generation(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &generation(GENERATION_A),
                    &default_binding(),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn ssh_legacy_direct_rows_are_invisible_without_an_active_manifest() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        history
            .record_source_bucket_changes(&source, redaction, &[bucket(at(10, 0), 7)])
            .unwrap();
        history
            .record_source_session_digest_changes(
                &source,
                redaction,
                &[digest("legacy-thread", at(10, 0), 7)],
            )
            .unwrap();

        assert!(
            history
                .load_source_records_since(&source, redaction, at(9, 0))
                .unwrap()
                .records
                .is_empty()
        );
        assert!(
            history
                .load_source_session_digest_records_since(&source, redaction, at(9, 0))
                .unwrap()
                .records
                .is_empty()
        );
    }

    #[test]
    fn ensuring_an_empty_generation_materializes_its_bound_namespace() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let generation = generation(GENERATION_A);
        let binding = default_binding();
        history
            .ensure_remote_history_generation(&source, redaction, &generation, &binding)
            .unwrap();
        let report = history
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &generation,
                &binding,
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(report, RemoteHistoryPageWriteReport::default());

        let directory =
            history.source_remote_history_generation_directory(&source, redaction, &generation);
        assert!(directory.join(REMOTE_GENERATION_METADATA_FILE).is_file());
        assert!(directory.join(BUCKETS_DIRECTORY).is_dir());
        assert!(directory.join(DIGESTS_DIRECTORY).is_dir());
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            None
        );
    }

    #[test]
    fn staging_is_invisible_until_one_manifest_switches_buckets_and_digests() {
        let temporary = tempdir().unwrap();
        let state_root = temporary.path().join("state");
        let history = store(state_root.clone(), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let first = generation(GENERATION_A);
        let replacement = generation(GENERATION_B);
        let binding = default_binding();

        history
            .ensure_remote_history_generation(&source, redaction, &first, &binding)
            .unwrap();
        history
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &first,
                &binding,
                &[bucket(at(10, 0), 10)],
                &[digest("old-thread", at(10, 0), 10)],
            )
            .unwrap();
        history
            .activate_remote_history_generation(
                &source,
                redaction,
                None,
                &first,
                &binding,
                at(11, 0),
            )
            .unwrap();
        let first_active =
            SourceHistoryRemoteActiveRef::new(first.clone(), binding.clone()).unwrap();

        history
            .ensure_remote_history_generation(&source, redaction, &replacement, &binding)
            .unwrap();
        let first_page = history
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &replacement,
                &binding,
                &[bucket(at(10, 15), 20)],
                &[digest("new-thread", at(10, 15), 20)],
            )
            .unwrap();
        assert_eq!(first_page.bucket_history.shards_written, 1);
        assert_eq!(first_page.session_digests.shards_written, 1);

        // A restarted reader still resolves the preceding manifest while the
        // replacement is only partially staged.
        let restarted = SourceHistoryStore::new(state_root, profile());
        let before_buckets = restarted
            .load_source_records_since(&source, redaction, at(9, 0))
            .unwrap();
        let before_digests = restarted
            .load_source_session_digest_records_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(before_buckets.records.len(), 1);
        assert_eq!(bucket_total(&before_buckets.records[0]), 10);
        assert_eq!(
            before_digests
                .records
                .iter()
                .map(digest_thread)
                .collect::<Vec<_>>(),
            vec!["old-thread"]
        );

        // Replaying the same durable page is a semantic no-op.
        let replay = restarted
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &replacement,
                &binding,
                &[bucket(at(10, 15), 20)],
                &[digest("new-thread", at(10, 15), 20)],
            )
            .unwrap();
        assert_eq!(replay.bucket_history.shards_skipped, 1);
        assert_eq!(replay.session_digests.shards_skipped, 1);

        restarted
            .activate_remote_history_generation(
                &source,
                redaction,
                Some(&first_active),
                &replacement,
                &binding,
                at(11, 15),
            )
            .unwrap();
        let after_buckets = restarted
            .load_source_records_since(&source, redaction, at(9, 0))
            .unwrap();
        let after_digests = restarted
            .load_source_session_digest_records_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(after_buckets.records.len(), 1);
        assert_eq!(bucket_total(&after_buckets.records[0]), 20);
        assert_eq!(after_buckets.records[0].starts_at(), at(10, 15));
        assert_eq!(
            after_digests
                .records
                .iter()
                .map(digest_thread)
                .collect::<Vec<_>>(),
            vec!["new-thread"]
        );
        assert_eq!(
            restarted
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(replacement)
        );
    }

    #[test]
    fn combined_snapshot_holds_one_manifest_across_bucket_and_digest_reads() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let first = generation(GENERATION_A);
        let replacement = generation(GENERATION_B);
        let binding = default_binding();
        let first_active =
            activate_initial_generation(&history, &source, redaction, &first, &binding);

        history
            .ensure_remote_history_generation(&source, redaction, &replacement, &binding)
            .unwrap();
        history
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &replacement,
                &binding,
                &[bucket(at(10, 15), 20)],
                &[digest("new-thread", at(10, 15), 20)],
            )
            .unwrap();

        let (between_sender, between_receiver) = mpsc::channel();
        let (continue_sender, continue_receiver) = mpsc::channel();
        let reader_history = history.clone();
        let reader_source = source.clone();
        let reader = thread::spawn(move || {
            reader_history.load_remote_history_snapshot_since_with_between_families(
                &reader_source,
                redaction,
                at(9, 0),
                || {
                    between_sender.send(()).unwrap();
                    continue_receiver
                        .recv_timeout(StdDuration::from_secs(5))
                        .expect("snapshot test did not release the family boundary");
                },
            )
        });
        between_receiver
            .recv_timeout(StdDuration::from_secs(5))
            .expect("snapshot did not reach the boundary between record families");

        let (started_sender, started_receiver) = mpsc::channel();
        let (activated_sender, activated_receiver) = mpsc::channel();
        let activation_history = history.clone();
        let activation_source = source.clone();
        let activation_binding = binding.clone();
        let activation_replacement = replacement.clone();
        let activation = thread::spawn(move || {
            started_sender.send(()).unwrap();
            let result = activation_history.activate_remote_history_generation(
                &activation_source,
                redaction,
                Some(&first_active),
                &activation_replacement,
                &activation_binding,
                at(11, 15),
            );
            activated_sender.send(()).unwrap();
            result
        });
        started_receiver
            .recv_timeout(StdDuration::from_secs(5))
            .expect("activation thread did not start");
        assert!(
            activated_receiver
                .recv_timeout(StdDuration::from_millis(100))
                .is_err(),
            "manifest activation must wait for the combined snapshot root lock"
        );

        continue_sender.send(()).unwrap();
        let before = reader.join().unwrap().unwrap();
        assert_eq!(before.active_ref.as_ref().unwrap().generation(), &first);
        assert_eq!(before.bucket_records.len(), 1);
        assert_eq!(bucket_total(&before.bucket_records[0]), 10);
        assert_eq!(
            before
                .session_digest_records
                .iter()
                .map(digest_thread)
                .collect::<Vec<_>>(),
            vec!["old-thread"]
        );

        activation.join().unwrap().unwrap();
        activated_receiver
            .recv_timeout(StdDuration::from_secs(5))
            .expect("activation did not finish after the snapshot released its lock");
        let after = history
            .load_remote_history_snapshot_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(
            after.active_ref.as_ref().unwrap().generation(),
            &replacement
        );
        assert_eq!(after.bucket_records.len(), 1);
        assert_eq!(bucket_total(&after.bucket_records[0]), 20);
        assert_eq!(
            after
                .session_digest_records
                .iter()
                .map(digest_thread)
                .collect::<Vec<_>>(),
            vec!["new-thread"]
        );
    }

    #[test]
    fn cow_digest_failure_keeps_both_active_families_unchanged() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let active = generation(GENERATION_A);
        let replacement = generation(GENERATION_B);
        let binding = default_binding();
        let active_ref =
            activate_initial_generation(&history, &source, redaction, &active, &binding);

        // The bucket addition is valid, while the digest intentionally
        // conflicts at an equal revision after the bucket family was written.
        let bucket_page = vec![bucket(at(10, 15), 20)];
        let digest_page = vec![digest("old-thread", at(10, 0), 99)];
        let error = history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &active_ref,
                &replacement,
                &binding,
                &bucket_page,
                &digest_page,
                at(11, 15),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(active.clone())
        );

        let visible_buckets = history
            .load_source_records_since(&source, redaction, at(9, 0))
            .unwrap();
        let visible_digests = history
            .load_source_session_digest_records_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(visible_buckets.records.len(), 1);
        assert_eq!(bucket_total(&visible_buckets.records[0]), 10);
        assert_eq!(visible_digests.records.len(), 1);
        assert_eq!(digest_total(&visible_digests.records[0]), 10);

        // The invisible replacement retained the already-applied bucket page.
        // Replaying the same deterministic WAL target must not overwrite it
        // with the baseline clone, even though the digest still fails.
        let replacement_directory =
            history.source_remote_history_generation_directory(&source, redaction, &replacement);
        let staged_buckets = history
            .load_source_bucket_records_from_directory(
                &source,
                redaction,
                at(9, 0),
                &replacement_directory.join(BUCKETS_DIRECTORY),
            )
            .unwrap();
        assert_eq!(staged_buckets.len(), 2);
        assert!(
            history
                .apply_remote_history_active_page_cow(
                    &source,
                    redaction,
                    &active_ref,
                    &replacement,
                    &binding,
                    &bucket_page,
                    &digest_page,
                    at(11, 15),
                )
                .is_err()
        );
        let replayed_buckets = history
            .load_source_bucket_records_from_directory(
                &source,
                redaction,
                at(9, 0),
                &replacement_directory.join(BUCKETS_DIRECTORY),
            )
            .unwrap();
        assert_eq!(replayed_buckets.len(), 2);
        assert_eq!(
            history
                .activate_remote_history_generation(
                    &source,
                    redaction,
                    Some(&active_ref),
                    &replacement,
                    &binding,
                    at(11, 30),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn cow_success_switches_buckets_and_digests_once_and_replays() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let active = generation(GENERATION_A);
        let replacement = generation(GENERATION_B);
        let binding = default_binding();
        let active_ref =
            activate_initial_generation(&history, &source, redaction, &active, &binding);
        let bucket_page = vec![bucket(at(10, 15), 20)];
        let digest_page = vec![digest("new-thread", at(10, 15), 20)];

        let report = history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &active_ref,
                &replacement,
                &binding,
                &bucket_page,
                &digest_page,
                at(11, 15),
            )
            .unwrap();
        assert_eq!(report.bucket_history.shards_written, 1);
        assert_eq!(report.session_digests.shards_written, 1);
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(replacement.clone())
        );
        let buckets = history
            .load_source_records_since(&source, redaction, at(9, 0))
            .unwrap();
        let digests = history
            .load_source_session_digest_records_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(buckets.records.len(), 2);
        assert_eq!(digests.records.len(), 2);

        let replay = history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &active_ref,
                &replacement,
                &binding,
                &bucket_page,
                &digest_page,
                at(11, 45),
            )
            .unwrap();
        assert_eq!(replay, RemoteHistoryPageWriteReport::default());
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(replacement)
        );
    }

    #[test]
    fn cow_clone_recovers_exact_shard_temps_and_rejects_unknown_hidden_files() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let active = generation(GENERATION_A);
        let replacement = generation(GENERATION_B);
        let binding = default_binding();
        let active_ref =
            activate_initial_generation(&history, &source, redaction, &active, &binding);
        let active_directory =
            history.source_remote_history_generation_directory(&source, redaction, &active);
        let bucket_temp = active_directory
            .join(BUCKETS_DIRECTORY)
            .join(".2026-08-30.json.201.1.tmp");
        let digest_temp = active_directory
            .join(DIGESTS_DIRECTORY)
            .join(".2026-08-30.json.gz.202.2.tmp");
        write_private_atomically(&bucket_temp, b"interrupted bucket write\n").unwrap();
        write_private_atomically(&digest_temp, b"interrupted digest write\n").unwrap();

        history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &active_ref,
                &replacement,
                &binding,
                &[bucket(at(10, 15), 20)],
                &[digest("new-thread", at(10, 15), 20)],
                at(11, 15),
            )
            .unwrap();
        assert!(!bucket_temp.exists());
        assert!(!digest_temp.exists());

        let replacement_ref =
            SourceHistoryRemoteActiveRef::new(replacement.clone(), binding.clone()).unwrap();
        let replacement_directory =
            history.source_remote_history_generation_directory(&source, redaction, &replacement);
        let unknown = replacement_directory
            .join(BUCKETS_DIRECTORY)
            .join(".2026-08-30.json.bad.tmp");
        write_private_atomically(&unknown, b"not one of our temps\n").unwrap();
        let next = generation(GENERATION_C);
        assert_eq!(
            history
                .apply_remote_history_active_page_cow(
                    &source,
                    redaction,
                    &replacement_ref,
                    &next,
                    &binding,
                    &[bucket_revision(2, at(10, 30), 30)],
                    &[],
                    at(11, 30),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(unknown.exists());
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(replacement.clone())
        );

        // A crash can also strand a temporary in the partially materialized
        // replacement. Once the operator removes the unrelated unknown file,
        // replay of the exact same page cleans the destination temp and
        // completes the COW generation.
        fs::remove_file(&unknown).unwrap();
        let next_directory =
            history.source_remote_history_generation_directory(&source, redaction, &next);
        let destination_temp = next_directory
            .join(BUCKETS_DIRECTORY)
            .join(".2026-08-30.json.203.3.tmp");
        write_private_atomically(&destination_temp, b"interrupted baseline clone\n").unwrap();
        history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &replacement_ref,
                &next,
                &binding,
                &[bucket_revision(2, at(10, 30), 30)],
                &[],
                at(11, 30),
            )
            .unwrap();
        assert!(!destination_temp.exists());
        assert_eq!(
            history
                .active_remote_history_generation(&source, redaction)
                .unwrap(),
            Some(next)
        );
    }

    #[test]
    fn cow_rejects_stale_active_cas_and_replacement_reuse() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let first = generation(GENERATION_A);
        let second = generation(GENERATION_B);
        let stale_replacement = generation(GENERATION_C);
        let binding = default_binding();
        let first_ref = activate_initial_generation(&history, &source, redaction, &first, &binding);
        history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &first_ref,
                &second,
                &binding,
                &[bucket(at(10, 15), 20)],
                &[digest("new-thread", at(10, 15), 20)],
                at(11, 15),
            )
            .unwrap();

        let stale = history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &first_ref,
                &stale_replacement,
                &binding,
                &[bucket(at(10, 30), 30)],
                &[],
                at(11, 30),
            )
            .unwrap_err();
        assert_eq!(stale.kind(), io::ErrorKind::WouldBlock);
        assert!(
            !history
                .source_remote_history_generation_directory(&source, redaction, &stale_replacement,)
                .exists()
        );

        // An already-published deterministic target can only replay the exact
        // page fingerprint and expected generation stored in its metadata.
        let collision = history
            .apply_remote_history_active_page_cow(
                &source,
                redaction,
                &first_ref,
                &second,
                &binding,
                &[bucket(at(10, 30), 31)],
                &[],
                at(11, 45),
            )
            .unwrap_err();
        assert_eq!(collision.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn bootstrap_activation_cas_is_idempotent_and_prevents_binding_rollback() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let first_generation = generation(GENERATION_A);
        let second_generation = generation(GENERATION_B);
        let third_generation = generation(GENERATION_C);
        let first_binding = binding(7, [2; 5]);
        let second_binding = binding(8, [3; 5]);
        let rollback_binding = binding(9, [4, 4, 2, 4, 4]);

        let first_active = activate_initial_generation(
            &history,
            &source,
            redaction,
            &first_generation,
            &first_binding,
        );
        history
            .ensure_remote_history_generation(
                &source,
                redaction,
                &second_generation,
                &second_binding,
            )
            .unwrap();
        history
            .activate_remote_history_generation(
                &source,
                redaction,
                Some(&first_active),
                &second_generation,
                &second_binding,
                at(12, 0),
            )
            .unwrap();
        let second_active =
            SourceHistoryRemoteActiveRef::new(second_generation.clone(), second_binding.clone())
                .unwrap();
        assert_eq!(
            history
                .active_remote_history_ref(&source, redaction)
                .unwrap(),
            Some(second_active.clone())
        );

        // A retry with the original expected ref succeeds only because the
        // exact candidate generation+binding is already active.
        history
            .activate_remote_history_generation(
                &source,
                redaction,
                Some(&first_active),
                &second_generation,
                &second_binding,
                at(12, 15),
            )
            .unwrap();

        history
            .ensure_remote_history_generation(
                &source,
                redaction,
                &third_generation,
                &rollback_binding,
            )
            .unwrap();
        assert_eq!(
            history
                .activate_remote_history_generation(
                    &source,
                    redaction,
                    Some(&second_active),
                    &third_generation,
                    &rollback_binding,
                    at(12, 30),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let lower_source_binding = binding(7, [4; 5]);
        let lower_source_candidate = generation("ingest-gen-55555555555555555555555555555555");
        history
            .ensure_remote_history_generation(
                &source,
                redaction,
                &lower_source_candidate,
                &lower_source_binding,
            )
            .unwrap();
        assert_eq!(
            history
                .activate_remote_history_generation(
                    &source,
                    redaction,
                    Some(&second_active),
                    &lower_source_candidate,
                    &lower_source_binding,
                    at(12, 40),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let safe_binding = binding(9, [4; 5]);
        let stale_candidate = generation("ingest-gen-44444444444444444444444444444444");
        history
            .ensure_remote_history_generation(&source, redaction, &stale_candidate, &safe_binding)
            .unwrap();
        assert_eq!(
            history
                .activate_remote_history_generation(
                    &source,
                    redaction,
                    Some(&first_active),
                    &stale_candidate,
                    &safe_binding,
                    at(12, 45),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn generation_binding_is_exact_and_cow_cannot_change_it() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let active_generation = generation(GENERATION_A);
        let replacement = generation(GENERATION_B);
        let active_binding = binding(7, [1; 5]);
        let changed_binding = binding(8, [2; 5]);
        let active = activate_initial_generation(
            &history,
            &source,
            redaction,
            &active_generation,
            &active_binding,
        );

        assert_eq!(
            history
                .ensure_remote_history_generation(
                    &source,
                    redaction,
                    &active_generation,
                    &changed_binding,
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            history
                .apply_remote_history_active_page_cow(
                    &source,
                    redaction,
                    &active,
                    &replacement,
                    &changed_binding,
                    &[bucket(at(10, 15), 20)],
                    &[],
                    at(11, 15),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            !history
                .source_remote_history_generation_directory(&source, redaction, &replacement)
                .exists()
        );
    }

    #[test]
    fn exact_atomic_metadata_temps_are_recovered_but_unknown_dotfiles_fail() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let candidate = generation(GENERATION_A);
        let binding = default_binding();
        let generation_directory =
            history.source_remote_history_generation_directory(&source, redaction, &candidate);
        history
            .prepare_private_directory(&generation_directory)
            .unwrap();
        let generation_temp = generation_directory.join(".generation.json.123.7.tmp");
        write_private_atomically(&generation_temp, b"orphan generation metadata\n").unwrap();
        history
            .ensure_remote_history_generation(&source, redaction, &candidate, &binding)
            .unwrap();
        assert!(!generation_temp.exists());

        let root = history.source_remote_history_directory(&source, redaction);
        let active_temp = root.join(".active.json.456.8.tmp");
        write_private_atomically(&active_temp, b"orphan active manifest\n").unwrap();
        let next = generation(GENERATION_B);
        history
            .ensure_remote_history_generation(&source, redaction, &next, &binding)
            .unwrap();
        assert!(!active_temp.exists());

        let unknown = generation_directory.join(".generation.json.bad.tmp");
        write_private_atomically(&unknown, b"not ours\n").unwrap();
        assert_eq!(
            history
                .apply_remote_history_generation_page(
                    &source,
                    redaction,
                    &candidate,
                    &binding,
                    &[],
                    &[],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(unknown.exists());
    }

    #[test]
    fn explicit_and_active_generation_fences_fail_closed() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::PreviewEnabled;
        let active = generation(GENERATION_A);
        let staged = generation(GENERATION_B);
        let unknown = generation(GENERATION_C);
        let binding = default_binding();
        history
            .ensure_remote_history_generation(&source, redaction, &active, &binding)
            .unwrap();
        history
            .activate_remote_history_generation(
                &source,
                redaction,
                None,
                &active,
                &binding,
                at(12, 0),
            )
            .unwrap();
        assert_eq!(
            history
                .apply_remote_history_generation_page(
                    &source,
                    redaction,
                    &active,
                    &binding,
                    &[bucket(at(12, 0), 1)],
                    &[],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        history
            .ensure_remote_history_generation(&source, redaction, &staged, &binding)
            .unwrap();

        assert_eq!(
            history
                .apply_remote_history_generation_page(
                    &source,
                    redaction,
                    &unknown,
                    &binding,
                    &[bucket(at(12, 0), 1)],
                    &[],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        history
            .validate_active_remote_history_generation_unfenced(&source, redaction, &active)
            .unwrap();
        assert_eq!(
            history
                .validate_active_remote_history_generation_unfenced(&source, redaction, &staged,)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            history
                .apply_remote_history_generation_page_unfenced(
                    &source,
                    redaction,
                    &staged,
                    &binding,
                    true,
                    &[bucket(at(12, 15), 2)],
                    &[],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn manifest_and_generation_paths_are_profile_source_and_redaction_scoped() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let generation = generation(GENERATION_A);
        let redacted = history.source_remote_history_generation_directory(
            &source_id(),
            RedactionProfile::Redacted,
            &generation,
        );
        let preview = history.source_remote_history_generation_directory(
            &source_id(),
            RedactionProfile::PreviewEnabled,
            &generation,
        );
        assert_ne!(redacted, preview);
        assert!(redacted.starts_with(history.profile_directory()));
        assert_eq!(redacted.file_name().unwrap(), generation.as_str());
        assert!(
            redacted
                .strip_prefix(history.state_root())
                .unwrap()
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        );

        #[cfg(windows)]
        assert_eq!(stable_lock_share_mode_for_test(), 0x1 | 0x2);
    }
}
