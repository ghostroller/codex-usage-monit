//! Generation-scoped bucket and session-digest persistence for SSH sources.
//!
//! A bootstrap is written into an explicit, initially invisible generation.
//! Readers resolve both data families through one active manifest, so a
//! multi-page bootstrap can never expose a half-populated replacement or keep
//! stale rows from the preceding generation alive after activation.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use super::remote_quota::{REMOTE_QUOTA_FILE, apply_remote_quota};
use super::session_evidence::DIGESTS_DIRECTORY;
use super::*;
use crate::remote_protocol::{ModelCatalogFingerprint, ProtocolRevisions, SourceGeneration};
use crate::remote_quota::RemoteQuotaChange;

const REMOTE_HISTORY_DIRECTORY: &str = "remote-history-v1";
const REMOTE_GENERATIONS_DIRECTORY: &str = "generations";
const REMOTE_GENERATION_METADATA_FILE: &str = "generation.json";
const REMOTE_ACTIVE_MANIFEST_FILE: &str = "active.json";
const REMOTE_PUBLICATION_REVISION_FILE: &str = "remote-publication-revision.json";
const MAX_REMOTE_GENERATION_FILE_BYTES: u64 = 64 * 1024;
const REMOTE_GENERATION_FORMAT_VERSION: u32 = 3;
const REMOTE_ACTIVE_MANIFEST_FORMAT_VERSION: u32 = 2;
const MAX_REMOTE_BINDING_BYTES: u64 = 4 * 1024;
const MAX_REMOTE_HISTORY_GENERATIONS: usize = 32;
const REMOTE_GENERATION_PREFIX: &str = "ingest-gen-";
const REMOTE_GENERATION_HEX_LEN: usize = 32;

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceHistoryRemoteBinding {
    source: SourceGeneration,
    revisions: ProtocolRevisions,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedSourceHistoryRemoteBinding {
    source: SourceGeneration,
    revisions: PersistedProtocolRevisions,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedProtocolRevisions {
    history_format: std::num::NonZeroU32,
    metric: std::num::NonZeroU32,
    estimator: std::num::NonZeroU32,
    project_breakdown: std::num::NonZeroU32,
    api_pricing_catalog: std::num::NonZeroU32,
    #[serde(default)]
    model_catalog_fingerprint: Option<ModelCatalogFingerprint>,
}

impl<'de> Deserialize<'de> for SourceHistoryRemoteBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let persisted = PersistedSourceHistoryRemoteBinding::deserialize(deserializer)?;
        let revisions = persisted.revisions;
        let binding = Self {
            source: persisted.source,
            revisions: ProtocolRevisions {
                history_format: revisions.history_format,
                metric: revisions.metric,
                estimator: revisions.estimator,
                project_breakdown: revisions.project_breakdown,
                api_pricing_catalog: revisions.api_pricing_catalog,
                model_catalog_fingerprint: revisions.model_catalog_fingerprint.unwrap_or_else(
                    crate::remote_protocol::legacy_unknown_model_catalog_fingerprint,
                ),
            },
        };
        binding.validate().map_err(serde::de::Error::custom)?;
        Ok(binding)
    }
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
    SkippedActive,
    SkippedProtected,
    NotFound,
}

/// Observable result of one bounded tracing sweep.
///
/// `deleted` counts generations moved out of the live namespace and fully
/// removed in this call. `skipped` counts active or caller-protected roots,
/// while `remaining` counts otherwise
/// collectible entries deferred solely by `max_work`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoteHistoryGenerationSweepReport {
    pub deleted: usize,
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
        Ok(())
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

/// SQL generations retain the protocol/CAS identity while incremental pages
/// update one stable stream inside a transaction. No shard baseline is copied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SqliteRemoteGeneration {
    metadata: RemoteGenerationMetadata,
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
    /// Revision of query-visible remote publication in the caller's SQL snapshot.
    /// Incremental pages retain their generation identity, so it cannot be used
    /// alone to decide whether a cached projection is still current.
    pub(crate) fn load_remote_history_projection_revision(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
    ) -> io::Result<u64> {
        let database = self.sqlite_database().expect("SQLite history backend");
        if !database.exists()? {
            return Ok(0);
        }
        database.read(|connection| {
            let key = database.namespace(
                &self
                    .source_directory(source)
                    .join(redaction.directory_name())
                    .join(REMOTE_PUBLICATION_REVISION_FILE),
            )?;
            Ok(database::state::<u64>(connection, &key)?.unwrap_or(0))
        })
    }

    pub(super) fn advance_remote_history_projection_revision(
        &self,
        connection: &rusqlite::Connection,
        source: &NodeId,
        redaction: RedactionProfile,
    ) -> io::Result<()> {
        let database = self.sqlite_database().expect("SQLite history backend");
        let key = database.namespace(
            &self
                .source_directory(source)
                .join(redaction.directory_name())
                .join(REMOTE_PUBLICATION_REVISION_FILE),
        )?;
        let next = database::state::<u64>(connection, &key)?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid_data("remote publication revision overflow"))?;
        database::set_state(connection, &key, &next)
    }

    fn sqlite_remote_generation_key(
        &self,
        database: &database::HistoryDatabase,
        source: &NodeId,
        redaction: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<String> {
        database.namespace(
            &self
                .source_remote_history_generation_directory(source, redaction, generation)
                .join(REMOTE_GENERATION_METADATA_FILE),
        )
    }

    fn sqlite_remote_active(
        &self,
        connection: &rusqlite::Connection,
        database: &database::HistoryDatabase,
        source: &NodeId,
        redaction: RedactionProfile,
    ) -> io::Result<Option<RemoteActiveManifest>> {
        let key = database.namespace(
            &self
                .source_remote_history_directory(source, redaction)
                .join(REMOTE_ACTIVE_MANIFEST_FILE),
        )?;
        let active: Option<RemoteActiveManifest> = database::state(connection, &key)?;
        if let Some(active) = &active {
            active.validate(&self.profile_id, source, redaction)?;
            let generation = self.sqlite_remote_generation(
                connection,
                database,
                source,
                redaction,
                &active.active_generation,
            )?;
            if generation.metadata.binding != active.binding {
                return Err(invalid_data(
                    "remote SQL active generation binding mismatch",
                ));
            }
        }
        Ok(active)
    }

    fn sqlite_remote_generation(
        &self,
        connection: &rusqlite::Connection,
        database: &database::HistoryDatabase,
        source: &NodeId,
        redaction: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<SqliteRemoteGeneration> {
        let key = self.sqlite_remote_generation_key(database, source, redaction, generation)?;
        let value: SqliteRemoteGeneration =
            database::state(connection, &key)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "remote SQL generation is missing")
            })?;
        value
            .metadata
            .validate_ready(&self.profile_id, source, redaction, generation)?;
        value.metadata.generation.validate()?;
        Ok(value)
    }

    fn sqlite_remote_catalog(
        &self,
        connection: &rusqlite::Connection,
        database: &database::HistoryDatabase,
        source: &NodeId,
        redaction: RedactionProfile,
    ) -> io::Result<Vec<(String, SqliteRemoteGeneration)>> {
        let prefix = format!(
            "{}/",
            database.namespace(
                &self
                    .source_remote_history_directory(source, redaction)
                    .join(REMOTE_GENERATIONS_DIRECTORY)
            )?
        );
        // Quota headers share the stable generation stream namespace. Only
        // generation descriptors identify logical aliases; do not decode or
        // count the other family's state as a generation.
        let mut values = Vec::new();
        let mut budget = SourceHistoryReadBudget::for_query();
        let suffix = format!("/{REMOTE_GENERATION_METADATA_FILE}");
        for key in database::state_keys(connection, &prefix)? {
            if !key.ends_with(&suffix) {
                continue;
            }
            if values.len() >= MAX_REMOTE_HISTORY_GENERATIONS {
                return Err(invalid_data(
                    "remote SQL generation count exceeds its bound",
                ));
            }
            budget.charge_records(1)?;
            let value: SqliteRemoteGeneration = sqlite_state_bounded(
                connection,
                &key,
                MAX_REMOTE_GENERATION_FILE_BYTES + 1024,
                Some(&mut budget),
            )?
            .ok_or_else(|| invalid_data("remote SQL generation descriptor disappeared"))?;
            values.push((key, value));
        }
        for (key, value) in &values {
            value.metadata.validate_ready(
                &self.profile_id,
                source,
                redaction,
                &value.metadata.generation,
            )?;
            value.metadata.generation.validate()?;
            if *key
                != self.sqlite_remote_generation_key(
                    database,
                    source,
                    redaction,
                    &value.metadata.generation,
                )?
            {
                return Err(invalid_data("remote SQL generation key mismatch"));
            }
        }
        Ok(values)
    }

    fn sqlite_remote_capacity(
        &self,
        connection: &rusqlite::Connection,
        database: &database::HistoryDatabase,
        source: &NodeId,
        redaction: RedactionProfile,
        candidate: &SourceHistoryRemoteGenerationId,
    ) -> io::Result<()> {
        let catalog = self.sqlite_remote_catalog(connection, database, source, redaction)?;
        if catalog.len() >= MAX_REMOTE_HISTORY_GENERATIONS
            && !catalog
                .iter()
                .any(|(_, value)| &value.metadata.generation == candidate)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote SQL generation capacity is exhausted",
            ));
        }
        Ok(())
    }

    fn sqlite_remote_data_directory(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        value: &SqliteRemoteGeneration,
    ) -> PathBuf {
        self.source_remote_history_generation_directory(
            source,
            redaction,
            &value.metadata.generation,
        )
    }

    fn sqlite_ensure_remote_generation(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
    ) -> io::Result<()> {
        let database = self
            .sqlite_database()
            .expect("SQL dispatch requires a database");
        database.write(|connection| {
            require_ssh_source(&self.load_source_metadata(source)?)?;
            self.sqlite_remote_capacity(connection, &database, source, redaction, generation)?;
            let key =
                self.sqlite_remote_generation_key(&database, source, redaction, generation)?;
            if let Some(existing) = database::state::<SqliteRemoteGeneration>(connection, &key)? {
                existing.metadata.validate_ready(
                    &self.profile_id,
                    source,
                    redaction,
                    generation,
                )?;
                if &existing.metadata.binding != binding {
                    return Err(invalid_data(
                        "remote SQL generation is bound to another bootstrap",
                    ));
                }
                existing.metadata.generation.validate()?;
                return Ok(());
            }
            let value = SqliteRemoteGeneration {
                metadata: RemoteGenerationMetadata::bootstrap(
                    self.profile_id.clone(),
                    source.clone(),
                    redaction,
                    generation.clone(),
                    binding.clone(),
                ),
            };
            database::set_state(connection, &key, &value)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn sqlite_apply_remote_generation_page(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
        require_active: bool,
        buckets: &[SourceBucketRecord],
        digests: &[SourceSessionDigestRecord],
        quotas: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        let database = self
            .sqlite_database()
            .expect("SQL dispatch requires a database");
        database.write(|connection| {
            require_ssh_source(&self.load_source_metadata(source)?)?;
            let active = self.sqlite_remote_active(connection, &database, source, redaction)?;
            if require_active
                || active
                    .as_ref()
                    .is_some_and(|active| &active.active_generation == generation)
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "remote active pages require the CAS apply path",
                ));
            }
            let value = self
                .sqlite_remote_generation(connection, &database, source, redaction, generation)?;
            if &value.metadata.binding != binding {
                return Err(invalid_data("remote SQL page generation binding mismatch"));
            }
            let directory = self.sqlite_remote_data_directory(source, redaction, &value);
            let bucket_history = self.record_source_bucket_changes_in_directory_unfenced(
                source,
                redaction,
                &directory.join(BUCKETS_DIRECTORY),
                buckets,
            )?;
            let session_digests = self.record_source_session_digest_changes_in_directory_unfenced(
                source,
                redaction,
                &directory.join(DIGESTS_DIRECTORY),
                digests,
            )?;
            apply_remote_quota(self, source, redaction, &directory, quotas)?;
            Ok(RemoteHistoryPageWriteReport {
                bucket_history,
                session_digests,
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn sqlite_apply_remote_active_page(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        expected: &SourceHistoryRemoteActiveRef,
        binding: &SourceHistoryRemoteBinding,
        buckets: &[SourceBucketRecord],
        digests: &[SourceSessionDigestRecord],
        activated_at: DateTime<Utc>,
        quotas: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        let database = self.sqlite_database().expect("SQLite history backend");
        database.write(|connection| {
            require_ssh_source(&self.load_source_metadata(source)?)?;
            let mut active = self
                .sqlite_remote_active(connection, &database, source, redaction)?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "remote SQL active generation is missing",
                    )
                })?;
            let actual = SourceHistoryRemoteActiveRef::new(
                active.active_generation.clone(),
                active.binding.clone(),
            )?;
            if &actual != expected {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "remote SQL active generation changed before apply",
                ));
            }
            if actual.binding() != binding {
                return Err(invalid_data(
                    "remote active page binding differs from its generation",
                ));
            }
            let generation = self.sqlite_remote_generation(
                connection,
                &database,
                source,
                redaction,
                expected.generation(),
            )?;
            let directory = self.sqlite_remote_data_directory(source, redaction, &generation);
            let bucket_history = self.record_source_bucket_changes_in_directory_unfenced(
                source,
                redaction,
                &directory.join(BUCKETS_DIRECTORY),
                buckets,
            )?;
            let session_digests = self.record_source_session_digest_changes_in_directory_unfenced(
                source,
                redaction,
                &directory.join(DIGESTS_DIRECTORY),
                digests,
            )?;
            let quota_changed = apply_remote_quota(self, source, redaction, &directory, quotas)?;
            if bucket_history.shards_written > 0
                || session_digests.shards_written > 0
                || quota_changed
            {
                self.advance_remote_history_projection_revision(connection, source, redaction)?;
            }
            active.activated_at = activated_at;
            database::set_state(
                connection,
                &database.namespace(
                    &self
                        .source_remote_history_directory(source, redaction)
                        .join(REMOTE_ACTIVE_MANIFEST_FILE),
                )?,
                &active,
            )?;
            Ok(RemoteHistoryPageWriteReport {
                bucket_history,
                session_digests,
            })
        })
    }

    fn sqlite_activate_remote_generation(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        expected: Option<&SourceHistoryRemoteActiveRef>,
        candidate: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
        activated_at: DateTime<Utc>,
    ) -> io::Result<()> {
        let database = self
            .sqlite_database()
            .expect("SQL dispatch requires a database");
        database.write(|connection| {
            require_ssh_source(&self.load_source_metadata(source)?)?;
            let value =
                self.sqlite_remote_generation(connection, &database, source, redaction, candidate)?;
            if &value.metadata.binding != binding {
                return Err(invalid_data(
                    "remote SQL bootstrap generation binding mismatch",
                ));
            }
            let actual = self
                .sqlite_remote_active(connection, &database, source, redaction)?
                .map(|active| {
                    SourceHistoryRemoteActiveRef::new(active.active_generation, active.binding)
                })
                .transpose()?;
            let wanted = SourceHistoryRemoteActiveRef::new(candidate.clone(), binding.clone())?;
            if actual.as_ref() == Some(&wanted) {
                return Ok(());
            }
            if actual.as_ref() != expected {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "remote SQL active generation changed before bootstrap",
                ));
            }
            if let Some(actual) = &actual {
                validate_binding_does_not_roll_back(actual.binding(), binding)?;
            }
            let manifest = RemoteActiveManifest {
                format_version: REMOTE_ACTIVE_MANIFEST_FORMAT_VERSION,
                profile_id: self.profile_id.clone(),
                source_id: source.clone(),
                redaction_profile: redaction,
                active_generation: candidate.clone(),
                binding: binding.clone(),
                activated_at,
            };
            database::set_state(
                connection,
                &database.namespace(
                    &self
                        .source_remote_history_directory(source, redaction)
                        .join(REMOTE_ACTIVE_MANIFEST_FILE),
                )?,
                &manifest,
            )?;
            self.advance_remote_history_projection_revision(connection, source, redaction)
        })
    }

    fn sqlite_gc_remote_generation(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        candidate: &SourceHistoryRemoteGenerationId,
        protected: &BTreeSet<SourceHistoryRemoteGenerationId>,
    ) -> io::Result<RemoteHistoryGenerationGcOutcome> {
        let database = self
            .sqlite_database()
            .expect("SQL dispatch requires a database");
        database.write(|connection| {
            require_ssh_source(&self.load_source_metadata(source)?)?;
            if self
                .sqlite_remote_active(connection, &database, source, redaction)?
                .is_some_and(|active| &active.active_generation == candidate)
            {
                return Ok(RemoteHistoryGenerationGcOutcome::SkippedActive);
            }
            if protected.contains(candidate) {
                return Ok(RemoteHistoryGenerationGcOutcome::SkippedProtected);
            }
            let key = self.sqlite_remote_generation_key(&database, source, redaction, candidate)?;
            let Some(value) = database::state::<SqliteRemoteGeneration>(connection, &key)? else {
                return Ok(RemoteHistoryGenerationGcOutcome::NotFound);
            };
            value
                .metadata
                .validate_ready(&self.profile_id, source, redaction, candidate)?;
            value.metadata.generation.validate()?;
            database::delete_state(connection, &key)?;
            let still_referenced = self
                .sqlite_remote_catalog(connection, &database, source, redaction)?
                .iter()
                .any(|(_, retained)| retained.metadata.generation == value.metadata.generation);
            if !still_referenced {
                let directory = self.sqlite_remote_data_directory(source, redaction, &value);
                for family in [BUCKETS_DIRECTORY, DIGESTS_DIRECTORY] {
                    database::delete_namespace(
                        connection,
                        &database.namespace(&directory.join(family))?,
                    )?;
                }
                database::delete_state(
                    connection,
                    &database.namespace(&directory.join(REMOTE_QUOTA_FILE))?,
                )?;
                database::delete_namespace(
                    connection,
                    &database.namespace(&directory.join(REMOTE_QUOTA_FILE))?,
                )?;
            }
            Ok(RemoteHistoryGenerationGcOutcome::Deleted)
        })
    }

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
    /// an unactivated direct namespace.
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
        let database = self.sqlite_database().expect("SQLite history backend");

        database.read(|connection| {
            require_ssh_source(&self.load_source_metadata(source_id)?)?;
            self.sqlite_remote_active(connection, &database, source_id, redaction_profile)?
                .map(|active| {
                    SourceHistoryRemoteActiveRef::new(active.active_generation, active.binding)
                })
                .transpose()
        })
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
        let mut budget = SourceHistoryReadBudget::for_query();
        self.load_remote_history_snapshot_since_with_budget(
            source_id,
            redaction_profile,
            since,
            &mut budget,
        )
    }

    pub(crate) fn load_remote_history_snapshot_since_with_budget(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<SourceHistoryRemoteSnapshot> {
        budget.charge_source()?;
        self.load_remote_history_snapshot_since_with_between_families_and_budget(
            source_id,
            redaction_profile,
            since,
            || {},
            budget,
        )
    }

    #[cfg(test)]
    fn load_remote_history_snapshot_since_with_between_families(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        between_families: impl FnOnce(),
    ) -> io::Result<SourceHistoryRemoteSnapshot> {
        let mut budget = SourceHistoryReadBudget::for_query();
        budget.charge_source()?;
        self.load_remote_history_snapshot_since_with_between_families_and_budget(
            source_id,
            redaction_profile,
            since,
            between_families,
            &mut budget,
        )
    }

    fn load_remote_history_snapshot_since_with_between_families_and_budget(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        between_families: impl FnOnce(),
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<SourceHistoryRemoteSnapshot> {
        let database = self.sqlite_database().expect("SQLite history backend");

        database.read(|_| {
            self.with_source_metadata_shared(source_id, |source| {
                require_ssh_source(source)?;
                self.with_active_remote_history_generation(
                    source_id,
                    redaction_profile,
                    |directory| {
                        let Some(directory) = directory else {
                            return Ok(SourceHistoryRemoteSnapshot::default());
                        };
                        let active_ref =
                            self.active_remote_history_ref(source_id, redaction_profile)?;
                        let bucket_records = self
                            .load_source_bucket_records_from_directory_with_budget(
                                source_id,
                                redaction_profile,
                                since,
                                &directory.join(BUCKETS_DIRECTORY),
                                budget,
                            )?;
                        between_families();
                        let session_digest_records = self
                            .load_source_session_digest_records_from_directory_with_budget(
                                source_id,
                                redaction_profile,
                                since,
                                &directory.join(DIGESTS_DIRECTORY),
                                budget,
                            )?;
                        Ok(SourceHistoryRemoteSnapshot {
                            active_ref,
                            bucket_records,
                            session_digest_records,
                        })
                    },
                )
            })
        })
    }

    pub(super) fn with_active_remote_history_generation<T>(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        operation: impl FnOnce(Option<&Path>) -> io::Result<T>,
    ) -> io::Result<T> {
        let database = self.sqlite_database().expect("SQLite history backend");

        database.read(|connection| {
            let directory = self
                .sqlite_remote_active(connection, &database, source_id, redaction_profile)?
                .map(|active| {
                    self.sqlite_remote_generation(
                        connection,
                        &database,
                        source_id,
                        redaction_profile,
                        &active.active_generation,
                    )
                    .map(|generation| {
                        self.sqlite_remote_data_directory(source_id, redaction_profile, &generation)
                    })
                })
                .transpose()?;
            operation(directory.as_deref())
        })
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
        self.sqlite_ensure_remote_generation(source_id, redaction_profile, generation, binding)
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
        quota_records: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        generation.validate()?;
        binding.validate_namespace(source_id)?;
        self.sqlite_apply_remote_generation_page(
            source_id,
            redaction_profile,
            generation,
            binding,
            require_active,
            bucket_records,
            digest_records,
            quota_records,
        )
    }

    /// Atomically applies an active page in its selected SQL generation.
    #[allow(clippy::too_many_arguments)]
    fn apply_remote_history_active_page_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: &SourceHistoryRemoteActiveRef,
        candidate_binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        activated_at: DateTime<Utc>,
        quota_records: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        expected_active.validate_namespace(source_id)?;
        candidate_binding.validate_namespace(source_id)?;
        self.sqlite_apply_remote_active_page(
            source_id,
            redaction_profile,
            expected_active,
            candidate_binding,
            bucket_records,
            digest_records,
            activated_at,
            quota_records,
        )
    }

    #[cfg_attr(test, allow(clippy::too_many_arguments))]
    fn activate_remote_history_generation_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: Option<&SourceHistoryRemoteActiveRef>,
        candidate_generation: &SourceHistoryRemoteGenerationId,
        candidate_binding: &SourceHistoryRemoteBinding,
        activated_at: DateTime<Utc>,
        #[cfg(test)] _before_manifest: Option<&dyn Fn()>,
    ) -> io::Result<()> {
        candidate_generation.validate()?;
        candidate_binding.validate_namespace(source_id)?;
        if let Some(expected) = expected_active {
            expected.validate_namespace(source_id)?;
        }
        self.sqlite_activate_remote_generation(
            source_id,
            redaction_profile,
            expected_active,
            candidate_generation,
            candidate_binding,
            activated_at,
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
        let active = self
            .active_remote_history_ref(source_id, redaction_profile)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "remote history has no active generation",
                )
            })?;
        if active.generation() != generation {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote active history generation does not match",
            ));
        }
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
        self.sqlite_gc_remote_generation(source_id, redaction_profile, candidate, protected)
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
        let database = self.sqlite_database().expect("SQLite history backend");

        for generation in protected {
            generation.validate()?;
        }
        database.write(|connection| {
            let catalog =
                self.sqlite_remote_catalog(connection, &database, source_id, redaction_profile)?;
            let active =
                self.sqlite_remote_active(connection, &database, source_id, redaction_profile)?;
            let mut report = RemoteHistoryGenerationSweepReport::default();
            for (_, generation) in catalog {
                let candidate = &generation.metadata.generation;
                if protected.contains(candidate)
                    || active
                        .as_ref()
                        .is_some_and(|active| &active.active_generation == candidate)
                {
                    report.skipped += 1;
                } else if report.deleted >= max_work {
                    report.remaining += 1;
                } else if self.sqlite_gc_remote_generation(
                    source_id,
                    redaction_profile,
                    candidate,
                    protected,
                )? == RemoteHistoryGenerationGcOutcome::Deleted
                {
                    report.deleted += 1;
                }
            }
            Ok(report)
        })
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_remote_history_generation_page(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        quota_records: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.apply_remote_history_generation_page_unfenced(
            source_id,
            redaction_profile,
            generation,
            binding,
            false,
            bucket_records,
            digest_records,
            quota_records,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_remote_history_active_page(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: &SourceHistoryRemoteActiveRef,
        candidate_binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        activated_at: DateTime<Utc>,
        quota_records: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.apply_remote_history_active_page_unfenced(
            source_id,
            redaction_profile,
            expected_active,
            candidate_binding,
            bucket_records,
            digest_records,
            activated_at,
            quota_records,
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
            None,
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
        self.transaction_fenced(|store| {
            store.ensure_remote_history_generation_unfenced(
                source_id,
                redaction_profile,
                generation,
                binding,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_remote_history_generation_page(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        generation: &SourceHistoryRemoteGenerationId,
        binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        quota_records: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.validate_redaction(redaction_profile)?;
        self.transaction_fenced(|store| {
            store.apply_remote_history_generation_page_unfenced(
                source_id,
                redaction_profile,
                generation,
                binding,
                false,
                bucket_records,
                digest_records,
                quota_records,
            )
        })
    }

    /// Applies one active incremental page atomically to its selected SQL generation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_remote_history_active_page(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        expected_active: &SourceHistoryRemoteActiveRef,
        candidate_binding: &SourceHistoryRemoteBinding,
        bucket_records: &[SourceBucketRecord],
        digest_records: &[SourceSessionDigestRecord],
        activated_at: DateTime<Utc>,
        quota_records: &[RemoteQuotaChange],
    ) -> io::Result<RemoteHistoryPageWriteReport> {
        self.validate_redaction(redaction_profile)?;
        self.transaction_fenced(|store| {
            store.apply_remote_history_active_page_unfenced(
                source_id,
                redaction_profile,
                expected_active,
                candidate_binding,
                bucket_records,
                digest_records,
                activated_at,
                quota_records,
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
        self.transaction_fenced(|store| {
            store.activate_remote_history_generation_unfenced(
                source_id,
                redaction_profile,
                expected_active,
                candidate_generation,
                candidate_binding,
                activated_at,
                #[cfg(test)]
                None,
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
        self.transaction_fenced(|store| {
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
        self.transaction_fenced(|store| {
            store.sweep_remote_history_generations_unfenced(
                source_id,
                redaction_profile,
                protected,
                max_work,
            )
        })
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
                model_catalog_fingerprint: crate::remote_protocol::test_model_catalog_fingerprint(
                    1,
                ),
            },
        )
        .unwrap()
    }

    fn default_binding() -> SourceHistoryRemoteBinding {
        binding(7, [1; 5])
    }

    #[test]
    fn persisted_v2_binding_without_catalog_fingerprint_loads_as_legacy_unknown() {
        let value = serde_json::json!({
            "source": {
                "nodeId": SOURCE,
                "generation": 1
            },
            "revisions": {
                "historyFormat": 1,
                "metric": 1,
                "estimator": 1,
                "projectBreakdown": 1,
                "apiPricingCatalog": 1
            }
        });
        let binding: SourceHistoryRemoteBinding = serde_json::from_value(value).unwrap();
        assert_eq!(
            binding.revisions().model_catalog_fingerprint,
            crate::remote_protocol::legacy_unknown_model_catalog_fingerprint()
        );
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
                &[],
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
                skipped: 0,
                remaining: MAX_REMOTE_HISTORY_GENERATIONS,
            }
        );

        for expected_remaining in [24, 16, 8, 0] {
            let report = history
                .sweep_remote_history_generations(&source, redaction, &BTreeSet::new(), 8)
                .unwrap();
            assert_eq!(report.deleted, 8);
            assert_eq!(report.skipped, 0);
            assert_eq!(report.remaining, expected_remaining);
            assert!(report.deleted <= 8);
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
            io::ErrorKind::WouldBlock
        );
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
        let db = history.sqlite_database().unwrap();
        db.read(|connection| {
            history.sqlite_remote_generation(connection, &db, &source, redaction, &active)
        })
        .unwrap();

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
        let db = history.sqlite_database().unwrap();
        db.read(|connection| {
            history.sqlite_remote_generation(
                connection,
                &db,
                &source,
                redaction,
                &protected_generation,
            )
        })
        .unwrap();
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
    fn ssh_unactivated_direct_rows_are_invisible_without_an_active_manifest() {
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
                &[],
            )
            .unwrap();
        assert_eq!(report, RemoteHistoryPageWriteReport::default());

        let db = history.sqlite_database().unwrap();
        let persisted = db
            .read(|connection| {
                history.sqlite_remote_generation(connection, &db, &source, redaction, &generation)
            })
            .unwrap();
        assert_eq!(persisted.metadata.binding, binding);
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
                &[],
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
                &[],
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
                &[],
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
        let second = generation(GENERATION_B);
        let binding = default_binding();
        let active = activate_initial_generation(&history, &source, redaction, &first, &binding);
        history
            .ensure_remote_history_generation(&source, redaction, &second, &binding)
            .unwrap();
        history
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &second,
                &binding,
                &[bucket(at(10, 15), 20)],
                &[digest("new-thread", at(10, 15), 20)],
                &[],
            )
            .unwrap();
        let writer = history.clone();
        let writer_source = source.clone();
        let writer_second = second.clone();
        let writer_binding = binding.clone();
        let before = history
            .load_remote_history_snapshot_since_with_between_families(
                &source,
                redaction,
                at(9, 0),
                || {
                    std::thread::spawn(move || {
                        writer.activate_remote_history_generation(
                            &writer_source,
                            redaction,
                            Some(&active),
                            &writer_second,
                            &writer_binding,
                            at(11, 15),
                        )
                    })
                    .join()
                    .unwrap()
                    .unwrap();
                },
            )
            .unwrap();
        assert_eq!(before.active_ref.as_ref().unwrap().generation(), &first);
        assert_eq!(bucket_total(&before.bucket_records[0]), 10);
        assert_eq!(
            digest_thread(&before.session_digest_records[0]),
            "old-thread"
        );
        let after = history
            .load_remote_history_snapshot_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(after.active_ref.as_ref().unwrap().generation(), &second);
        assert_eq!(bucket_total(&after.bucket_records[0]), 20);
        assert_eq!(
            digest_thread(&after.session_digest_records[0]),
            "new-thread"
        );
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
    #[test]
    fn active_sql_page_is_atomic_idempotent_and_does_not_allocate_generations() {
        let temporary = tempdir().unwrap();
        let history = store(temporary.path().join("state"), SourceKind::Ssh);
        let source = source_id();
        let redaction = RedactionProfile::Redacted;
        let current_generation = generation(GENERATION_A);
        let binding = default_binding();
        let active = activate_initial_generation(
            &history,
            &source,
            redaction,
            &current_generation,
            &binding,
        );
        let apply = |buckets: &[SourceBucketRecord], digests: &[SourceSessionDigestRecord]| {
            history.apply_remote_history_active_page(
                &source,
                redaction,
                &active,
                &binding,
                buckets,
                digests,
                at(12, 0),
                &[],
            )
        };
        assert!(
            apply(
                &[bucket_revision(2, at(10, 0), 20)],
                &[digest_revision(1, "old-thread", at(10, 0), 20)]
            )
            .is_err()
        );
        let old = history
            .load_remote_history_snapshot_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(bucket_total(&old.bucket_records[0]), 10);
        assert_eq!(digest_total(&old.session_digest_records[0]), 10);
        let buckets = [bucket_revision(2, at(10, 0), 20)];
        let digests = [digest_revision(2, "old-thread", at(10, 0), 20)];
        apply(&buckets, &digests).unwrap();
        let replay = apply(&buckets, &digests).unwrap();
        assert_eq!(replay.bucket_history.shards_written, 0);
        assert_eq!(replay.session_digests.shards_written, 0);
        let after = history
            .load_remote_history_snapshot_since(&source, redaction, at(9, 0))
            .unwrap();
        assert_eq!(after.active_ref.as_ref(), Some(&active));
        assert_eq!(bucket_total(&after.bucket_records[0]), 20);
        assert_eq!(digest_total(&after.session_digest_records[0]), 20);
        let db = history.sqlite_database().unwrap();
        assert_eq!(
            db.read(|connection| history
                .sqlite_remote_catalog(connection, &db, &source, redaction)
                .map(|rows| rows.len()))
                .unwrap(),
            1
        );
        let stale =
            SourceHistoryRemoteActiveRef::new(generation(GENERATION_B), binding.clone()).unwrap();
        assert_eq!(
            history
                .apply_remote_history_active_page(
                    &source,
                    redaction,
                    &stale,
                    &binding,
                    &[],
                    &[],
                    at(12, 1),
                    &[]
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
