//! Crash-safe center-side state for aggregate remote-delta ingestion.
//!
//! The state machine intentionally separates durability from transport and
//! scheduling.  A page is first persisted as a pending WAL record, then the
//! caller applies and acknowledges the returned revisioned bucket/digest
//! records through [`apply_and_commit_remote_delta_page`].
//! Reopening a session exposes the same pending records, so an interrupted
//! apply is replayed idempotently.
//!
//! Bootstrap is a distinct copy-on-write path.  Pages are directed to a fresh
//! staging generation and the old active generation remains selected until the
//! final staged page has been applied.  [`activate_remote_delta_bootstrap`]
//! atomically switches the source-history manifest before committing the
//! ingest cursor.  Repeating that ordering after a crash is safe because both
//! operations are idempotent.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_file::replace_file;
use crate::domain::{ApiCostAmount, PicoUsd, TokenUsage};
use crate::history::{
    HISTORY_METRIC_REVISION, LocalHalfHourBucket, LocalProjectUsageGroup, LocalUsageGroup,
};
use crate::remote_protocol::{
    DeltaCursor, DeltaPage, DeltaPayload, DeltaRequest, ExportRange, ProtocolRevisions,
    RemoteApiCostAmount, RemoteDeltaPayloadContext, RemoteDeltaResponse, RemoteExportRequest,
    RemoteExportRequestBody, RemoteExportResponseBody, RemotePagePayload, RemoteProjectDescriptor,
    RemoteSessionDigest, RemoteSessionDigestMutation, RemoteSessionUsageMetrics, RemoteTokenUsage,
    RemoteUsageBucket, RemoteUsageBucketMutation, SourceGeneration,
};
use crate::source_history::{
    HistoryProfileId, RedactionProfile, RemoteHistoryGenerationGcOutcome,
    RemoteHistoryGenerationSweepReport, SessionDigestFingerprint, SessionUsageMetrics,
    SourceBucketRecord, SourceHistoryRemoteActiveRef, SourceHistoryRemoteBinding,
    SourceHistoryRemoteGenerationId, SourceHistoryStore, SourceHistoryWriter, SourceSessionDigest,
    SourceSessionDigestRecord, sync_directory,
};
use crate::source_identity::NodeId;
#[cfg(windows)]
use crate::source_identity::validate_windows_private_file;
use crate::source_model::{ObservedProjectKey, SessionReplicaKey};

const INGEST_LAYOUT_DIRECTORY: &str = "remote-ingest-v1";
const INGEST_STATE_FILE: &str = "ingest-state.json";
const INGEST_ANCHOR_FILE: &str = "ingest-state.anchor";
const INGEST_LOCK_FILE: &str = "ingest.lock";
const INGEST_RETIREMENTS_DIRECTORY: &str = "retirements-v1";
const INGEST_RETIREMENT_LOCK_FILE: &str = "retirement.lock";
const INGEST_RETIREMENT_MARKER_FILE: &str = "preview-to-redacted.json";
const INGEST_STATE_FORMAT_VERSION: u32 = 4;
const INGEST_ANCHOR_FORMAT_VERSION: u32 = 1;
const INGEST_RETIREMENT_FORMAT_VERSION: u32 = 1;
const INGEST_GENERATION_PREFIX: &str = "ingest-gen-";
const INGEST_GENERATION_RANDOM_BYTES: usize = 16;
const BINDING_NAMESPACE_PREFIX: &str = "binding-sha256-";
const PREPARED_PAGE_PREFIX: &str = "page-sha256-";
const ACTIVE_REPLACEMENT_GENERATION_DOMAIN: &[u8] =
    b"codex-usage-monit/remote-active-page-generation/v1\0";
const SHA256_HEX_LEN: usize = 64;
const MAX_WINDOW_MINUTES: u32 = 35 * 24 * 60;
const MAX_OVERLAP_MINUTES: u16 = 24 * 60;
const MAX_INGEST_STATE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INGEST_ANCHOR_BYTES: u64 = 64 * 1024;
const MAX_BINDING_NAMESPACES_PER_SOURCE: usize = 64;
const REMOTE_GENERATION_SWEEP_WORK_LIMIT: usize = 8;
const INGEST_RETIREMENT_WORK_LIMIT: usize = 128;
const TEMP_FILE_ATTEMPTS: usize = 128;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Fixed center policy for a rolling remote-delta range.
///
/// Exact `from`/`to` timestamps move with each completed sync; their duration,
/// overlap behavior, and live-state policy do not.  Multi-page continuations
/// additionally remain bound to the exact first-page range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteDeltaRangePolicy {
    window_minutes: NonZeroU32,
    overlap_minutes: u16,
    include_live: bool,
}

impl RemoteDeltaRangePolicy {
    pub fn new(
        window_minutes: NonZeroU32,
        overlap_minutes: u16,
        include_live: bool,
    ) -> io::Result<Self> {
        let policy = Self {
            window_minutes,
            overlap_minutes,
            include_live,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn window_minutes(&self) -> NonZeroU32 {
        self.window_minutes
    }

    pub fn overlap_minutes(&self) -> u16 {
        self.overlap_minutes
    }

    pub fn include_live(&self) -> bool {
        self.include_live
    }

    fn validate(&self) -> io::Result<()> {
        if self.window_minutes.get() > MAX_WINDOW_MINUTES {
            return Err(invalid_data(
                "remote ingest range policy exceeds the retained 35-day window",
            ));
        }
        if self.overlap_minutes > MAX_OVERLAP_MINUTES {
            return Err(invalid_data(
                "remote ingest range policy overlap exceeds 24 hours",
            ));
        }
        Ok(())
    }

    fn validate_request(&self, request: &DeltaRequest) -> io::Result<()> {
        let expected = Duration::minutes(i64::from(self.window_minutes.get()));
        if request.range.to.signed_duration_since(request.range.from) != expected
            || request.overlap_minutes != self.overlap_minutes
            || request.include_live != self.include_live
        {
            return Err(invalid_data(
                "remote delta request does not match its fixed ingest range policy",
            ));
        }
        Ok(())
    }
}

/// Exact durable namespace binding.  Changing any member requires a separate
/// state namespace/bootstrap and can never silently reuse the old cursor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteDeltaIngestBinding {
    profile_id: HistoryProfileId,
    source: SourceGeneration,
    redaction_profile: RedactionProfile,
    revisions: ProtocolRevisions,
    range_policy: RemoteDeltaRangePolicy,
}

impl RemoteDeltaIngestBinding {
    pub fn new(
        profile_id: HistoryProfileId,
        source: SourceGeneration,
        redaction_profile: RedactionProfile,
        revisions: ProtocolRevisions,
        range_policy: RemoteDeltaRangePolicy,
    ) -> io::Result<Self> {
        let binding = Self {
            profile_id,
            source,
            redaction_profile,
            revisions,
            range_policy,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub fn profile_id(&self) -> &HistoryProfileId {
        &self.profile_id
    }

    pub fn source(&self) -> &SourceGeneration {
        &self.source
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.redaction_profile
    }

    pub fn revisions(&self) -> &ProtocolRevisions {
        &self.revisions
    }

    pub fn range_policy(&self) -> &RemoteDeltaRangePolicy {
        &self.range_policy
    }

    fn validate(&self) -> io::Result<()> {
        self.range_policy.validate()?;
        if self.revisions.metric.get() != HISTORY_METRIC_REVISION {
            return Err(invalid_data(
                "remote ingest metric revision cannot be represented by source history",
            ));
        }
        Ok(())
    }

    fn validate_exchange(
        &self,
        request: &RemoteExportRequest,
        response: &RemoteDeltaResponse,
    ) -> io::Result<()> {
        response
            .validate_for_request(request)
            .map_err(|error| invalid_data(format!("remote delta exchange is invalid: {error}")))?;
        if request.expected_source.as_ref() != Some(&self.source)
            || request.redaction_profile != self.redaction_profile
            || response.source != self.source
            || response.redaction_profile != self.redaction_profile
            || response.revisions != self.revisions
        {
            return Err(invalid_data(
                "remote delta exchange does not match its durable ingest binding",
            ));
        }
        let RemoteExportRequestBody::Delta(delta) = &request.request else {
            return Err(invalid_data("remote ingest accepts only delta requests"));
        };
        self.range_policy.validate_request(delta)
    }
}

/// Opaque center-owned generation for one materialized history snapshot.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteHistoryGenerationId(String);

impl RemoteHistoryGenerationId {
    pub fn generate() -> io::Result<Self> {
        let mut random = [0_u8; INGEST_GENERATION_RANDOM_BYTES];
        getrandom::fill(&mut random).map_err(|error| {
            io::Error::other(format!(
                "could not generate remote history generation: {error}"
            ))
        })?;
        if random.iter().all(|byte| *byte == 0) {
            return Err(io::Error::other(
                "secure random provider returned an unusable history generation",
            ));
        }
        let mut value = String::with_capacity(INGEST_GENERATION_PREFIX.len() + random.len() * 2);
        value.push_str(INGEST_GENERATION_PREFIX);
        append_lower_hex(&mut value, &random);
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> io::Result<()> {
        validate_prefixed_hex(
            &self.0,
            INGEST_GENERATION_PREFIX,
            INGEST_GENERATION_RANDOM_BYTES * 2,
            "remote history generation",
        )
    }
}

impl fmt::Display for RemoteHistoryGenerationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Content identity for one persisted pending page.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PreparedRemoteDeltaPageId(String);

impl PreparedRemoteDeltaPageId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> io::Result<()> {
        validate_prefixed_hex(
            &self.0,
            PREPARED_PAGE_PREFIX,
            SHA256_HEX_LEN,
            "prepared remote delta page ID",
        )
    }
}

impl fmt::Display for PreparedRemoteDeltaPageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The exact materialization namespace to which the WAL page must be applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "targetKind", content = "target", rename_all = "snake_case")]
pub enum RemoteDeltaApplyTarget {
    ActiveNoop {
        expected_generation: RemoteHistoryGenerationId,
    },
    ActiveCow {
        expected_generation: RemoteHistoryGenerationId,
        replacement_generation: RemoteHistoryGenerationId,
    },
    Staging(RemoteHistoryGenerationId),
}

impl RemoteDeltaApplyTarget {
    pub fn resulting_generation(&self) -> &RemoteHistoryGenerationId {
        match self {
            Self::ActiveNoop {
                expected_generation,
            } => expected_generation,
            Self::ActiveCow {
                replacement_generation,
                ..
            } => replacement_generation,
            Self::Staging(generation) => generation,
        }
    }

    fn validate(&self) -> io::Result<()> {
        self.resulting_generation().validate()?;
        if let Self::ActiveCow {
            expected_generation,
            replacement_generation,
        } = self
        {
            expected_generation.validate()?;
            if expected_generation == replacement_generation {
                return Err(invalid_data(
                    "remote active COW replacement must differ from its expected generation",
                ));
            }
        }
        Ok(())
    }

    fn seed(&self) -> RemoteDeltaApplyTargetSeed {
        match self {
            Self::ActiveNoop {
                expected_generation,
            } => RemoteDeltaApplyTargetSeed::ActiveNoop(expected_generation.clone()),
            Self::ActiveCow {
                expected_generation,
                ..
            } => RemoteDeltaApplyTargetSeed::ActiveCow(expected_generation.clone()),
            Self::Staging(generation) => RemoteDeltaApplyTargetSeed::Staging(generation.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "targetKind", content = "generation", rename_all = "snake_case")]
enum RemoteDeltaApplyTargetSeed {
    ActiveNoop(RemoteHistoryGenerationId),
    ActiveCow(RemoteHistoryGenerationId),
    Staging(RemoteHistoryGenerationId),
}

impl RemoteDeltaApplyTargetSeed {
    fn without_history_mutations(self) -> Self {
        match self {
            Self::ActiveCow(expected_generation) => Self::ActiveNoop(expected_generation),
            target => target,
        }
    }

    fn materialize(
        &self,
        page_id: &PreparedRemoteDeltaPageId,
    ) -> io::Result<RemoteDeltaApplyTarget> {
        Ok(match self {
            Self::ActiveNoop(expected_generation) => RemoteDeltaApplyTarget::ActiveNoop {
                expected_generation: expected_generation.clone(),
            },
            Self::ActiveCow(expected_generation) => RemoteDeltaApplyTarget::ActiveCow {
                expected_generation: expected_generation.clone(),
                replacement_generation: active_replacement_generation(page_id)?,
            },
            Self::Staging(generation) => RemoteDeltaApplyTarget::Staging(generation.clone()),
        })
    }
}

/// Source-history mutations decoded from one already validated remote page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteDeltaHistoryRecords {
    pub bucket_records: Vec<SourceBucketRecord>,
    pub session_digest_records: Vec<SourceSessionDigestRecord>,
}

impl RemoteDeltaHistoryRecords {
    fn is_empty(&self) -> bool {
        self.bucket_records.is_empty() && self.session_digest_records.is_empty()
    }
}

/// Durable pending page plus its replay-safe local mutations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedRemoteDeltaPage {
    pub id: PreparedRemoteDeltaPageId,
    pub target: RemoteDeltaApplyTarget,
    pub records: RemoteDeltaHistoryRecords,
    pub next_cursor: DeltaCursor,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBootstrapActivation {
    pub generation: RemoteHistoryGenerationId,
    pub cursor: DeltaCursor,
}

/// Best-effort cleanup performed only after the durable ingest transition has
/// committed. A deferred cleanup never makes the already-committed page or
/// activation ambiguous; a later orphan sweep may safely retry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteGenerationCleanup {
    NotRequired,
    Completed(RemoteHistoryGenerationGcOutcome),
    Deferred {
        error_kind: io::ErrorKind,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDeltaCommitReport {
    pub cleanup: RemoteGenerationCleanup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBootstrapActivationReport {
    pub activation: RemoteBootstrapActivation,
    pub cleanup: RemoteGenerationCleanup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDeltaIngestStatus {
    pub active_generation: Option<RemoteHistoryGenerationId>,
    pub active_cursor: Option<DeltaCursor>,
    pub bootstrap_generation: Option<RemoteHistoryGenerationId>,
    pub bootstrap_cursor: Option<DeltaCursor>,
    pub pending_page: Option<PreparedRemoteDeltaPageId>,
    pub activation_required: bool,
    pub deferred_cleanup_count: usize,
}

/// Durable position for constructing the next transport request.  A present
/// `exact_range` is a pagination fence: the caller must reuse it verbatim
/// instead of recomputing a rolling range after process restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDeltaNextRequestPosition {
    pub delta_cursor: Option<DeltaCursor>,
    pub exact_range: Option<ExportRange>,
    pub known_live_revision: Option<NonZeroU64>,
}

/// Center-side filesystem namespace for one profile/source/profile binding.
#[derive(Clone, Debug)]
pub struct RemoteDeltaIngestStateStore {
    history_store: SourceHistoryStore,
    binding: RemoteDeltaIngestBinding,
    binding_namespace: String,
}

/// Result of one bounded center-ingest privacy retirement pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteIngestProfileRetirementStatus {
    NotRequired,
    Complete,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRemoteIngestProfileRetirement {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    retiring_profile: RedactionProfile,
    replacement_profile: RedactionProfile,
}

impl StoredRemoteIngestProfileRetirement {
    fn preview_to_redacted(profile_id: HistoryProfileId, source_id: NodeId) -> Self {
        Self {
            format_version: INGEST_RETIREMENT_FORMAT_VERSION,
            profile_id,
            source_id,
            retiring_profile: RedactionProfile::PreviewEnabled,
            replacement_profile: RedactionProfile::Redacted,
        }
    }

    fn validate(&self, profile_id: &HistoryProfileId, source_id: &NodeId) -> io::Result<()> {
        if self.format_version != INGEST_RETIREMENT_FORMAT_VERSION
            || &self.profile_id != profile_id
            || &self.source_id != source_id
            || self.retiring_profile != RedactionProfile::PreviewEnabled
            || self.replacement_profile != RedactionProfile::Redacted
        {
            return Err(invalid_data(
                "remote ingest retirement marker does not match its namespace",
            ));
        }
        Ok(())
    }
}

/// Durably queues retirement of every PreviewEnabled ingest binding for one
/// source. The marker is outside the profile being retired, so removing the
/// old cursor/WAL namespace can never remove its own recovery evidence. The
/// marker remains as a small tombstone after completion and is removed only by
/// an explicit source purge. That makes a rolled-back Windows directory unlink
/// replayable without relying on unsupported directory-handle flushing.
///
/// Callers must already hold the exact remotes-config fence. `writer` proves
/// the redacted v2 ownership epoch and serializes this transition with every
/// cooperative history/ingest writer.
pub(crate) fn queue_remote_preview_ingest_retirement(
    history_store: &SourceHistoryStore,
    source_id: &NodeId,
    writer: &SourceHistoryWriter<'_, '_, '_>,
) -> io::Result<RemoteIngestProfileRetirementStatus> {
    validate_ingest_retirement_writer(history_store, writer)?;
    let paths = RemoteIngestRetirementPaths::new(history_store, source_id);
    let source_exists = private_directory_exists(history_store, &paths.preview_source)?;
    let marker_exists = private_file_exists(&paths.marker)?;
    if !source_exists && !marker_exists {
        return Ok(RemoteIngestProfileRetirementStatus::NotRequired);
    }

    history_store.prepare_private_directory(&paths.marker_directory)?;
    let marker_lock = open_private_lock(&paths.marker_lock)?;
    try_lock_private_lock(&paths.marker_lock, &marker_lock)?;
    cleanup_remote_ingest_temporary_files(&paths.marker_directory)?;

    let ingest_lock = if source_exists {
        let lock = open_private_lock(&paths.preview_lock)?;
        try_lock_private_lock(&paths.preview_lock, &lock)?;
        Some(lock)
    } else {
        None
    };
    ensure_remote_ingest_retirement_marker(&paths)?;
    drop(ingest_lock);
    Ok(RemoteIngestProfileRetirementStatus::Pending)
}

/// Makes one bounded, crash-recoverable cleanup pass over a queued preview
/// ingest namespace. Metadata is checked under its stable source lock, and the
/// caller's exact config fence plus this function's writer authority keep that
/// decision stable while the preview ingest lock is held. A still-visible
/// PreviewEnabled source is never touched even if a marker was published
/// immediately before a process crash.
pub(crate) fn retry_remote_preview_ingest_retirement(
    history_store: &SourceHistoryStore,
    source_id: &NodeId,
    writer: &SourceHistoryWriter<'_, '_, '_>,
) -> io::Result<RemoteIngestProfileRetirementStatus> {
    validate_ingest_retirement_writer(history_store, writer)?;
    let metadata = history_store.load_source_metadata(source_id)?;
    if metadata.kind() != crate::source_history::SourceKind::Ssh {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote ingest retirement is only valid for SSH sources",
        ));
    }
    if metadata.aggregate_redaction_profile() != RedactionProfile::Redacted {
        return Ok(RemoteIngestProfileRetirementStatus::Pending);
    }

    let paths = RemoteIngestRetirementPaths::new(history_store, source_id);
    let source_exists = private_directory_exists(history_store, &paths.preview_source)?;
    let marker_exists = private_file_exists(&paths.marker)?;
    if !source_exists && !marker_exists {
        return Ok(RemoteIngestProfileRetirementStatus::NotRequired);
    }

    history_store.prepare_private_directory(&paths.marker_directory)?;
    let marker_lock = open_private_lock(&paths.marker_lock)?;
    try_lock_private_lock(&paths.marker_lock, &marker_lock)?;
    cleanup_remote_ingest_temporary_files(&paths.marker_directory)?;
    ensure_remote_ingest_retirement_marker(&paths)?;

    if private_directory_exists(history_store, &paths.preview_source)?
        && !remove_remote_preview_ingest_source_bounded(history_store, &paths)?
    {
        return Ok(RemoteIngestProfileRetirementStatus::Pending);
    }
    if private_directory_exists(history_store, &paths.preview_source)? {
        return Ok(RemoteIngestProfileRetirementStatus::Pending);
    }
    // Keep the out-of-namespace marker as a durable tombstone. If a Windows
    // crash exposes a directory entry that appeared removed before shutdown,
    // the next retry sees the marker and repeats the bounded cleanup.
    sync_directory(&paths.preview_profile)?;
    Ok(RemoteIngestProfileRetirementStatus::Complete)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteIngestSourcePurgeReport {
    pub namespaces_removed: usize,
}

/// Removes only cursor/WAL state owned by one explicitly purged SSH source.
///
/// Each source namespace is fully validated and then isolated with a
/// same-parent deterministic rename before removal. A restart can therefore
/// finish a partially removed trash namespace without ever considering any
/// account or other-source path. The caller holds the remotes-config lock;
/// `writer` serializes this operation with cooperative history/ingest writes.
pub(crate) fn purge_remote_ingest_state_for_source(
    history_store: &SourceHistoryStore,
    source_id: &NodeId,
    writer: &SourceHistoryWriter<'_, '_, '_>,
) -> io::Result<RemoteIngestSourcePurgeReport> {
    writer.validate_store_binding(history_store)?;
    let profile_root = history_store
        .state_root()
        .join(INGEST_LAYOUT_DIRECTORY)
        .join(history_store.profile_id().as_str());
    let mut report = RemoteIngestSourcePurgeReport::default();

    for redaction_profile in [RedactionProfile::Redacted, RedactionProfile::PreviewEnabled] {
        let parent = profile_root.join(redaction_profile.directory_name());
        let source = parent.join(source_id.as_str());
        let trash = parent.join(format!(".source-purge-{source_id}.trash"));
        if purge_ingest_namespace(history_store, source_id, redaction_profile, &source, &trash)? {
            report.namespaces_removed += 1;
        }
    }

    let retirement_parent = profile_root.join(INGEST_RETIREMENTS_DIRECTORY);
    let retirement = retirement_parent.join(source_id.as_str());
    let retirement_trash = retirement_parent.join(format!(".source-purge-{source_id}.trash"));
    if purge_ingest_retirement_namespace(history_store, source_id, &retirement, &retirement_trash)?
    {
        report.namespaces_removed += 1;
    }
    writer.validate()?;
    Ok(report)
}

fn purge_ingest_namespace(
    history_store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    source: &Path,
    trash: &Path,
) -> io::Result<bool> {
    let parent = source
        .parent()
        .ok_or_else(|| invalid_data("remote ingest source has no parent"))?;
    if !private_directory_exists(history_store, parent)? {
        return Ok(false);
    }
    // Unix persists the directory entry here. Windows publication uses the
    // write-through rename below and recovers through deterministic trash.
    sync_directory(parent)?;
    let source_exists = private_directory_exists(history_store, source)?;
    let trash_exists = private_directory_exists(history_store, trash)?;
    if source_exists && trash_exists {
        return Err(invalid_data(
            "remote ingest purge found both a live namespace and recovery trash",
        ));
    }
    if source_exists {
        let lock_path = source.join(INGEST_LOCK_FILE);
        if !private_file_exists(&lock_path)? {
            return Err(invalid_data(
                "remote ingest purge source is missing its stable lock",
            ));
        }
        let lock = open_private_lock(&lock_path)?;
        try_lock_private_lock(&lock_path, &lock)?;
        validate_purge_ingest_source(history_store, source, source_id, redaction_profile, true)?;
        FileExt::unlock(&lock)?;
        drop(lock);
        history_store.validate_private_path(source)?;
        rename_ingest_purge_namespace(source, trash)?;
        sync_directory(parent)?;
    }
    if private_directory_exists(history_store, trash)? {
        validate_purge_ingest_source(history_store, trash, source_id, redaction_profile, false)?;
        remove_purge_ingest_source(history_store, trash)?;
        return Ok(true);
    }
    // Repeating this is a real directory barrier on Unix. On Windows an absent
    // deterministic trash path is already an idempotent terminal state.
    sync_directory(parent)?;
    Ok(false)
}

fn validate_purge_ingest_source(
    history_store: &SourceHistoryStore,
    directory: &Path,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    require_complete: bool,
) -> io::Result<()> {
    history_store.validate_private_path(directory)?;
    let mut lock_seen = false;
    let mut binding_count = 0_usize;
    for entry in fs::read_dir(directory)? {
        history_store.validate_private_path(directory)?;
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        if name == OsStr::new(INGEST_LOCK_FILE) {
            lock_seen = true;
            let file = open_existing_private_file(&path, "remote ingest purge lock")?;
            drop(file);
            continue;
        }
        let name = name
            .to_str()
            .ok_or_else(|| invalid_data("remote ingest purge entry is not UTF-8"))?;
        validate_prefixed_hex(
            name,
            BINDING_NAMESPACE_PREFIX,
            SHA256_HEX_LEN,
            "remote ingest purge binding namespace",
        )?;
        binding_count += 1;
        if binding_count > MAX_BINDING_NAMESPACES_PER_SOURCE {
            return Err(invalid_data(
                "remote ingest purge exceeds the binding namespace bound",
            ));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(invalid_data(format!(
                "remote ingest purge binding {} is not a real directory",
                path.display()
            )));
        }
        history_store.validate_private_path(&path)?;
        validate_purge_ingest_binding(
            &path,
            history_store.profile_id(),
            source_id,
            redaction_profile,
            name,
            require_complete,
        )?;
    }
    if require_complete && !lock_seen {
        return Err(invalid_data(
            "remote ingest purge source is missing its stable lock",
        ));
    }
    Ok(())
}

fn validate_purge_ingest_binding(
    directory: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    namespace: &str,
    require_complete: bool,
) -> io::Result<()> {
    let state = read_optional_retired_ingest_binding(
        &directory.join(INGEST_STATE_FILE),
        MAX_INGEST_STATE_BYTES,
        "remote ingest purge state",
    )?;
    let anchor = read_optional_retired_ingest_binding(
        &directory.join(INGEST_ANCHOR_FILE),
        MAX_INGEST_ANCHOR_BYTES,
        "remote ingest purge anchor",
    )?;
    if require_complete && (state.is_none() || anchor.is_none()) {
        return Err(invalid_data(
            "remote ingest purge binding is missing state or anchor",
        ));
    }
    if let (Some(state), Some(anchor)) = (&state, &anchor)
        && state != anchor
    {
        return Err(invalid_data(
            "remote ingest purge state and anchor bindings disagree",
        ));
    }
    if let Some(binding) = state.as_ref().or(anchor.as_ref())
        && (binding.profile_id() != profile_id
            || &binding.source().node_id != source_id
            || binding.redaction_profile() != redaction_profile
            || binding_namespace_component(binding)? != namespace)
    {
        return Err(invalid_data(
            "remote ingest purge binding does not match its source/profile path",
        ));
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != OsStr::new(INGEST_STATE_FILE)
            && name != OsStr::new(INGEST_ANCHOR_FILE)
            && !is_remote_ingest_temporary_name(&name)
        {
            return Err(invalid_data(format!(
                "remote ingest purge binding contains unexpected entry {}",
                entry.path().display()
            )));
        }
        let file = open_existing_private_file(&entry.path(), "remote ingest purge file")?;
        drop(file);
    }
    Ok(())
}

fn remove_purge_ingest_source(
    history_store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<()> {
    history_store.validate_private_path(directory)?;
    for entry in fs::read_dir(directory)? {
        history_store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "remote ingest purge refuses symbolic link {}",
                path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            history_store.validate_private_path(&path)?;
            for file_entry in fs::read_dir(&path)? {
                let file_entry = file_entry?;
                let file = open_existing_private_file(
                    &file_entry.path(),
                    "remote ingest purge binding file",
                )?;
                drop(file);
                fs::remove_file(file_entry.path())?;
            }
            sync_directory(&path)?;
            fs::remove_dir(&path)?;
        } else {
            let file = open_existing_private_file(&path, "remote ingest purge source file")?;
            drop(file);
            fs::remove_file(&path)?;
        }
    }
    sync_directory(directory)?;
    let parent = directory
        .parent()
        .ok_or_else(|| invalid_data("remote ingest purge source has no parent"))?;
    fs::remove_dir(directory)?;
    sync_directory(parent)
}

fn purge_ingest_retirement_namespace(
    history_store: &SourceHistoryStore,
    source_id: &NodeId,
    source: &Path,
    trash: &Path,
) -> io::Result<bool> {
    let parent = source
        .parent()
        .ok_or_else(|| invalid_data("remote ingest retirement has no parent"))?;
    if !private_directory_exists(history_store, parent)? {
        return Ok(false);
    }
    sync_directory(parent)?;
    let source_exists = private_directory_exists(history_store, source)?;
    let trash_exists = private_directory_exists(history_store, trash)?;
    if source_exists && trash_exists {
        return Err(invalid_data(
            "remote ingest purge found both retirement state and recovery trash",
        ));
    }
    if source_exists {
        let lock_path = source.join(INGEST_RETIREMENT_LOCK_FILE);
        if !private_file_exists(&lock_path)? {
            return Err(invalid_data(
                "remote ingest purge retirement state is missing its lock",
            ));
        }
        let lock = open_private_lock(&lock_path)?;
        try_lock_private_lock(&lock_path, &lock)?;
        validate_purge_retirement_directory(history_store, source, source_id, true)?;
        FileExt::unlock(&lock)?;
        drop(lock);
        rename_ingest_purge_namespace(source, trash)?;
        sync_directory(parent)?;
    }
    if private_directory_exists(history_store, trash)? {
        validate_purge_retirement_directory(history_store, trash, source_id, false)?;
        remove_purge_ingest_source(history_store, trash)?;
        return Ok(true);
    }
    sync_directory(parent)?;
    Ok(false)
}

#[cfg(not(windows))]
fn rename_ingest_purge_namespace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn rename_ingest_purge_namespace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

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
    // No replace flag: a concurrently materialized/unknown destination must
    // make the purge fail without displacing it.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn validate_purge_retirement_directory(
    history_store: &SourceHistoryStore,
    directory: &Path,
    source_id: &NodeId,
    require_lock: bool,
) -> io::Result<()> {
    history_store.validate_private_path(directory)?;
    let mut lock_seen = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == OsStr::new(INGEST_RETIREMENT_LOCK_FILE) {
            lock_seen = true;
        } else if name == OsStr::new(INGEST_RETIREMENT_MARKER_FILE) {
            let marker: StoredRemoteIngestProfileRetirement = read_private_json(
                &entry.path(),
                MAX_INGEST_ANCHOR_BYTES,
                "remote ingest purge retirement marker",
            )?;
            marker.validate(history_store.profile_id(), source_id)?;
        } else if !is_remote_ingest_temporary_name(&name) {
            return Err(invalid_data(format!(
                "remote ingest purge retirement contains unexpected entry {}",
                entry.path().display()
            )));
        }
        let file =
            open_existing_private_file(&entry.path(), "remote ingest purge retirement file")?;
        drop(file);
    }
    if require_lock && !lock_seen {
        return Err(invalid_data(
            "remote ingest purge retirement state is missing its stable lock",
        ));
    }
    Ok(())
}

struct RemoteIngestRetirementPaths {
    profile_id: HistoryProfileId,
    source_id: NodeId,
    preview_profile: PathBuf,
    preview_source: PathBuf,
    preview_lock: PathBuf,
    marker_directory: PathBuf,
    marker_lock: PathBuf,
    marker: PathBuf,
}

impl RemoteIngestRetirementPaths {
    fn new(history_store: &SourceHistoryStore, source_id: &NodeId) -> Self {
        let profile_root = history_store
            .state_root()
            .join(INGEST_LAYOUT_DIRECTORY)
            .join(history_store.profile_id().as_str());
        let preview_profile = profile_root.join(RedactionProfile::PreviewEnabled.directory_name());
        let preview_source = preview_profile.join(source_id.as_str());
        let marker_directory = profile_root
            .join(INGEST_RETIREMENTS_DIRECTORY)
            .join(source_id.as_str());
        Self {
            profile_id: history_store.profile_id().clone(),
            source_id: source_id.clone(),
            preview_profile,
            preview_lock: preview_source.join(INGEST_LOCK_FILE),
            preview_source,
            marker_lock: marker_directory.join(INGEST_RETIREMENT_LOCK_FILE),
            marker: marker_directory.join(INGEST_RETIREMENT_MARKER_FILE),
            marker_directory,
        }
    }
}

impl RemoteDeltaIngestStateStore {
    pub fn new(
        history_store: SourceHistoryStore,
        binding: RemoteDeltaIngestBinding,
    ) -> io::Result<Self> {
        if history_store.profile_id() != binding.profile_id() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote ingest binding profile does not match its history store",
            ));
        }
        binding.validate()?;
        let binding_namespace = binding_namespace_component(&binding)?;
        Ok(Self {
            history_store,
            binding,
            binding_namespace,
        })
    }

    pub fn binding(&self) -> &RemoteDeltaIngestBinding {
        &self.binding
    }

    pub fn namespace_directory(&self) -> PathBuf {
        self.source_namespace_directory()
            .join(&self.binding_namespace)
    }

    fn source_namespace_directory(&self) -> PathBuf {
        self.history_store
            .state_root()
            .join(INGEST_LAYOUT_DIRECTORY)
            .join(self.binding.profile_id.as_str())
            .join(self.binding.redaction_profile.directory_name())
            .join(self.binding.source.node_id.as_str())
    }

    fn lock_path(&self) -> PathBuf {
        self.source_namespace_directory().join(INGEST_LOCK_FILE)
    }

    /// Takes the source/profile ingest lock without waiting.
    pub fn try_begin(&self) -> io::Result<RemoteDeltaIngestSession<'_>> {
        self.prepare_source_namespace()?;
        let lock_path = self.lock_path();
        let lock = open_private_lock(&lock_path)?;
        try_lock_private_lock(&lock_path, &lock)?;
        self.prepare_binding_namespace_locked()?;
        self.validate_namespace()?;
        cleanup_remote_ingest_temporary_files(&self.namespace_directory())?;
        let state = self.load_or_create_state()?;
        Ok(RemoteDeltaIngestSession {
            store: self,
            lock,
            state,
        })
    }

    fn state_path(&self) -> PathBuf {
        self.namespace_directory().join(INGEST_STATE_FILE)
    }

    fn anchor_path(&self) -> PathBuf {
        self.namespace_directory().join(INGEST_ANCHOR_FILE)
    }

    fn prepare_source_namespace(&self) -> io::Result<()> {
        self.history_store
            .prepare_private_directory(&self.source_namespace_directory())
    }

    fn prepare_binding_namespace_locked(&self) -> io::Result<()> {
        let source_directory = self.source_namespace_directory();
        self.history_store
            .validate_private_path(&source_directory)?;
        let mut count = 0_usize;
        let mut requested_exists = false;
        for entry in fs::read_dir(&source_directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == OsStr::new(INGEST_LOCK_FILE) {
                continue;
            }
            let Some(name) = name.to_str() else {
                return Err(invalid_data(
                    "remote ingest source namespace contains a non-UTF-8 entry",
                ));
            };
            validate_prefixed_hex(
                name,
                BINDING_NAMESPACE_PREFIX,
                SHA256_HEX_LEN,
                "remote ingest binding namespace",
            )?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(invalid_data(format!(
                    "remote ingest binding namespace {} is not a directory",
                    path.display()
                )));
            }
            self.history_store.validate_private_path(&path)?;
            count = count.saturating_add(1);
            requested_exists |= name == self.binding_namespace;
        }
        if count > MAX_BINDING_NAMESPACES_PER_SOURCE
            || (!requested_exists && count == MAX_BINDING_NAMESPACES_PER_SOURCE)
        {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "remote ingest source reached its {MAX_BINDING_NAMESPACES_PER_SOURCE}-binding namespace limit"
                ),
            ));
        }
        if requested_exists {
            return Ok(());
        }
        self.history_store
            .prepare_private_directory(&self.namespace_directory())
    }

    fn validate_namespace(&self) -> io::Result<()> {
        self.history_store
            .validate_private_path(&self.namespace_directory())
    }

    fn load_or_create_state(&self) -> io::Result<StoredRemoteDeltaIngestState> {
        match self.read_state() {
            Ok(state) => {
                self.ensure_anchor()?;
                Ok(state)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match self.read_anchor() {
                    Ok(_) => {
                        return Err(invalid_data(
                            "remote ingest state is missing from an initialized namespace",
                        ));
                    }
                    Err(anchor_error) if anchor_error.kind() == io::ErrorKind::NotFound => {}
                    Err(anchor_error) => return Err(anchor_error),
                }
                let state = StoredRemoteDeltaIngestState::new(self.binding.clone());
                self.write_state(&state)?;
                self.ensure_anchor()?;
                Ok(state)
            }
            Err(error) => Err(error),
        }
    }

    fn read_state(&self) -> io::Result<StoredRemoteDeltaIngestState> {
        let state: StoredRemoteDeltaIngestState = read_private_json(
            &self.state_path(),
            MAX_INGEST_STATE_BYTES,
            "remote ingest state",
        )?;
        state.validate(&self.binding)?;
        Ok(state)
    }

    fn write_state(&self, state: &StoredRemoteDeltaIngestState) -> io::Result<()> {
        self.validate_namespace()?;
        state.validate(&self.binding)?;
        write_private_json_atomically(
            &self.state_path(),
            state,
            MAX_INGEST_STATE_BYTES,
            "remote ingest state",
        )
    }

    fn read_anchor(&self) -> io::Result<StoredRemoteDeltaIngestAnchor> {
        let anchor: StoredRemoteDeltaIngestAnchor = read_private_json(
            &self.anchor_path(),
            MAX_INGEST_ANCHOR_BYTES,
            "remote ingest anchor",
        )?;
        anchor.validate(&self.binding)?;
        Ok(anchor)
    }

    fn ensure_anchor(&self) -> io::Result<()> {
        let expected = StoredRemoteDeltaIngestAnchor {
            format_version: INGEST_ANCHOR_FORMAT_VERSION,
            binding: self.binding.clone(),
        };
        match self.read_anchor() {
            Ok(anchor) if anchor == expected => Ok(()),
            Ok(_) => Err(invalid_data(
                "remote ingest anchor does not match its configured binding",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_json_once(
                    &self.anchor_path(),
                    &expected,
                    MAX_INGEST_ANCHOR_BYTES,
                    "remote ingest anchor",
                )?;
                let published = self.read_anchor()?;
                if published == expected {
                    Ok(())
                } else {
                    Err(invalid_data(
                        "remote ingest anchor raced with an incompatible binding",
                    ))
                }
            }
            Err(error) => Err(error),
        }
    }
}

/// Locked state-machine session.  Transport should not be run while holding
/// this lock; prepare/acknowledge are short local persistence sections.
pub struct RemoteDeltaIngestSession<'a> {
    store: &'a RemoteDeltaIngestStateStore,
    lock: File,
    state: StoredRemoteDeltaIngestState,
}

impl fmt::Debug for RemoteDeltaIngestSession<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteDeltaIngestSession")
            .field("binding", self.store.binding())
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl RemoteDeltaIngestSession<'_> {
    pub fn status(&self) -> RemoteDeltaIngestStatus {
        RemoteDeltaIngestStatus {
            active_generation: self
                .state
                .active
                .as_ref()
                .map(|active| active.generation.clone()),
            active_cursor: self.state.active.as_ref().map(|active| active.cursor),
            bootstrap_generation: self
                .state
                .bootstrap
                .as_ref()
                .map(|bootstrap| bootstrap.generation.clone()),
            bootstrap_cursor: self
                .state
                .bootstrap
                .as_ref()
                .and_then(|bootstrap| bootstrap.cursor),
            pending_page: self
                .state
                .pending
                .as_ref()
                .map(|pending| pending.id.clone()),
            activation_required: self
                .state
                .bootstrap
                .as_ref()
                .is_some_and(|bootstrap| bootstrap.ready_to_activate),
            deferred_cleanup_count: self.state.retired_generations.len(),
        }
    }

    /// Returns both the cursor and any exact multi-page range fence.  The
    /// range is durable so pagination can resume after a process restart even
    /// though the ordinary rolling range has advanced with wall-clock time.
    pub fn next_request_position(&self) -> io::Result<RemoteDeltaNextRequestPosition> {
        self.validate_fence()?;
        let known_live_revision = self.store.history_store.remote_live_revision_for_binding(
            &self.store.binding.source,
            &self.store.binding.revisions,
            self.store.binding.redaction_profile,
        )?;
        if self.state.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a pending remote delta page must be replayed before another request",
            ));
        }
        if let Some(bootstrap) = &self.state.bootstrap {
            if bootstrap.ready_to_activate {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a completed bootstrap must be activated before another request",
                ));
            }
            return Ok(RemoteDeltaNextRequestPosition {
                delta_cursor: bootstrap.cursor,
                exact_range: bootstrap.cursor.and(bootstrap.exact_range.clone()),
                known_live_revision,
            });
        }
        Ok(match &self.state.active {
            Some(active) => RemoteDeltaNextRequestPosition {
                delta_cursor: Some(active.cursor),
                exact_range: active.continuation_range.clone(),
                known_live_revision,
            },
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "initial remote bootstrap must be initialized with a history writer before transport",
                ));
            }
        })
    }

    /// Starts a fresh bootstrap after cursor expiry while retaining the old
    /// active generation for readers. Repeated calls before the first page are
    /// idempotent.
    pub fn start_bootstrap(
        &mut self,
        writer: &SourceHistoryWriter<'_, '_, '_>,
    ) -> io::Result<RemoteHistoryGenerationId> {
        self.validate_fence()?;
        self.validate_history_writer(writer)?;
        if self.state.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot start bootstrap while a page WAL is pending",
            ));
        }
        if let Some(bootstrap) = &self.state.bootstrap {
            return Ok(bootstrap.generation.clone());
        }
        self.create_fresh_bootstrap(writer)
    }

    /// Replaces an expired bootstrap continuation with a fresh cursorless
    /// staging generation. The caller must first validate a fully bound
    /// `CursorExpired` response; ordinary startup should use
    /// [`Self::start_bootstrap`] so an in-progress bootstrap is resumed.
    pub fn restart_bootstrap_after_cursor_expiry(
        &mut self,
        writer: &SourceHistoryWriter<'_, '_, '_>,
    ) -> io::Result<RemoteHistoryGenerationId> {
        self.validate_fence()?;
        self.validate_history_writer(writer)?;
        self.create_fresh_bootstrap(writer)
    }

    fn create_fresh_bootstrap(
        &mut self,
        writer: &SourceHistoryWriter<'_, '_, '_>,
    ) -> io::Result<RemoteHistoryGenerationId> {
        if self.state.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot start bootstrap while a page WAL is pending",
            ));
        }
        let active_ref = self.store.history_store.active_remote_history_ref(
            &self.store.binding.source.node_id,
            self.store.binding.redaction_profile,
        )?;
        let expected_active = active_ref
            .as_ref()
            .map(stored_active_ref_from_history)
            .transpose()?;
        if let Some(active) = &self.state.active {
            let expected_active = expected_active.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "ingest active generation has no matching history manifest",
                )
            })?;
            if expected_active.generation != active.generation
                || expected_active.source != self.store.binding.source
                || expected_active.revisions != self.store.binding.revisions
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "ingest active generation does not match the history manifest binding",
                ));
            }
        }
        let retired_staging = self
            .state
            .bootstrap
            .as_ref()
            .map(|bootstrap| bootstrap.generation.clone());
        let generation = RemoteHistoryGenerationId::generate()?;
        let history_generation = source_history_generation(&generation)?;
        let history_binding = source_history_binding(&self.store.binding)?;
        writer.ensure_remote_history_generation(
            &self.store.binding.source.node_id,
            self.store.binding.redaction_profile,
            &history_generation,
            &history_binding,
        )?;
        let mut next = self.state.clone();
        next.bootstrap = Some(StoredBootstrap {
            generation: generation.clone(),
            expected_active,
            exact_range: None,
            cursor: None,
            ready_to_activate: false,
        });
        if let Some(retired) = &retired_staging {
            queue_retired_generation(&mut next, retired);
        }
        self.publish(next)?;
        if let Some(retired) = &retired_staging {
            let _ = cleanup_retired_generation_after_commit(self, writer, retired);
        }
        Ok(generation)
    }

    /// Persists a validated response as the pending-page WAL before returning
    /// any records to the caller.
    pub fn prepare_page(
        &mut self,
        request: &RemoteExportRequest,
        response: &RemoteDeltaResponse,
        received_at: DateTime<Utc>,
    ) -> io::Result<PreparedRemoteDeltaPage> {
        self.validate_fence()?;
        if self.state.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a pending remote delta page must be replayed first",
            ));
        }
        self.store.binding.validate_exchange(request, response)?;
        let delta_request = delta_request(request)?;
        let (page, payload) = delta_response_page(response)?;
        let records = history_records_from_payload(
            &self.store.binding.source.node_id,
            self.store.binding.redaction_profile,
            payload,
        )?;
        let target_seed = if records.is_empty() {
            self.prepare_target(delta_request)?
                .without_history_mutations()
        } else {
            self.prepare_target(delta_request)?
        };

        let id = pending_page_id_from_parts(&target_seed, request, response, received_at)?;
        let pending = StoredPendingPage {
            id: id.clone(),
            target: target_seed.materialize(&id)?,
            request: request.clone(),
            response: response.clone(),
            received_at,
        };
        pending.validate(&self.store.binding)?;

        let mut next = self.state.clone();
        if let Some(bootstrap) = next.bootstrap.as_mut()
            && bootstrap.exact_range.is_none()
        {
            bootstrap.exact_range = Some(delta_request.range.clone());
        }
        next.pending = Some(pending.clone());
        self.publish(next)?;
        Ok(prepared_from_pending(&pending, records, page))
    }

    /// Returns the exact pending WAL page after restart. Applying its records
    /// again is safe because their remote journal sequence is the local
    /// revision.
    pub fn pending_page(&self) -> io::Result<Option<PreparedRemoteDeltaPage>> {
        self.validate_fence()?;
        self.state
            .pending
            .as_ref()
            .map(|pending| {
                let (page, payload) = delta_response_page(&pending.response)?;
                let records = history_records_from_payload(
                    &self.store.binding.source.node_id,
                    self.store.binding.redaction_profile,
                    payload,
                )?;
                Ok(prepared_from_pending(pending, records, page))
            })
            .transpose()
    }

    /// Acknowledges that the pending records were durably applied. Ordinary
    /// incremental pages advance the active cursor atomically with WAL removal.
    /// Bootstrap pages advance only staging state; the final page transitions
    /// to `activation_required` while the old active generation remains intact.
    fn mark_page_applied(&mut self, id: &PreparedRemoteDeltaPageId) -> io::Result<()> {
        self.validate_fence()?;
        let pending = self
            .state
            .pending
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no pending delta page"))?;
        if &pending.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared page ID does not match the durable pending WAL",
            ));
        }
        let delta_request = delta_request(&pending.request)?;
        let (page, _) = delta_response_page(&pending.response)?;
        let mut next = self.state.clone();
        match &pending.target {
            RemoteDeltaApplyTarget::ActiveNoop {
                expected_generation,
            } => {
                let active = next.active.as_mut().ok_or_else(|| {
                    invalid_data("pending active page has no active history generation")
                })?;
                if &active.generation != expected_generation {
                    return Err(invalid_data(
                        "pending page targets the wrong active history generation",
                    ));
                }
                active.cursor = page.next_delta_cursor;
                active.continuation_range = page.has_more.then(|| delta_request.range.clone());
            }
            RemoteDeltaApplyTarget::ActiveCow {
                expected_generation,
                replacement_generation,
            } => {
                let active = next.active.as_mut().ok_or_else(|| {
                    invalid_data("pending active page has no active history generation")
                })?;
                if &active.generation != expected_generation {
                    return Err(invalid_data(
                        "pending page targets the wrong active history generation",
                    ));
                }
                active.generation = replacement_generation.clone();
                active.cursor = page.next_delta_cursor;
                active.continuation_range = page.has_more.then(|| delta_request.range.clone());
                queue_retired_generation(&mut next, expected_generation);
            }
            RemoteDeltaApplyTarget::Staging(generation) => {
                let bootstrap = next.bootstrap.as_mut().ok_or_else(|| {
                    invalid_data("pending staging page has no bootstrap generation")
                })?;
                if &bootstrap.generation != generation {
                    return Err(invalid_data(
                        "pending page targets the wrong bootstrap generation",
                    ));
                }
                bootstrap.cursor = Some(page.next_delta_cursor);
                bootstrap.ready_to_activate = !page.has_more;
            }
        }
        next.pending = None;
        self.publish(next)
    }

    pub fn bootstrap_activation_required(&self) -> io::Result<Option<RemoteBootstrapActivation>> {
        self.validate_fence()?;
        Ok(self.state.bootstrap.as_ref().and_then(|bootstrap| {
            if !bootstrap.ready_to_activate {
                return None;
            }
            bootstrap.cursor.map(|cursor| RemoteBootstrapActivation {
                generation: bootstrap.generation.clone(),
                cursor,
            })
        }))
    }

    /// Commits an already completed external atomic generation switch.
    /// Calling this before the external switch would make readers trust data
    /// which may not exist; callers must preserve that ordering.
    fn commit_bootstrap_activation(
        &mut self,
        activation: &RemoteBootstrapActivation,
    ) -> io::Result<()> {
        self.validate_fence()?;
        if self.state.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot activate bootstrap while a page WAL is pending",
            ));
        }
        let bootstrap =
            self.state.bootstrap.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no bootstrap to activate")
            })?;
        if !bootstrap.ready_to_activate
            || bootstrap.cursor != Some(activation.cursor)
            || bootstrap.generation != activation.generation
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bootstrap activation does not match durable staging state",
            ));
        }
        let mut next = self.state.clone();
        next.active = Some(StoredActiveGeneration {
            generation: activation.generation.clone(),
            cursor: activation.cursor,
            continuation_range: None,
        });
        if let Some(expected_active) = &bootstrap.expected_active {
            queue_retired_generation(&mut next, &expected_active.generation);
        }
        next.bootstrap = None;
        self.publish(next)
    }

    fn prepare_target(&self, request: &DeltaRequest) -> io::Result<RemoteDeltaApplyTargetSeed> {
        if let Some(bootstrap) = &self.state.bootstrap {
            if bootstrap.ready_to_activate {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "completed bootstrap must be activated before accepting another page",
                ));
            }
            if request.delta_cursor != bootstrap.cursor {
                return Err(invalid_data(
                    "remote bootstrap request does not continue its staged cursor",
                ));
            }
            if let Some(exact_range) = &bootstrap.exact_range
                && &request.range != exact_range
            {
                return Err(invalid_data(
                    "remote bootstrap continuation changed its exact export range",
                ));
            }
            return Ok(RemoteDeltaApplyTargetSeed::Staging(
                bootstrap.generation.clone(),
            ));
        }
        if let Some(active) = &self.state.active {
            if request.delta_cursor != Some(active.cursor) {
                return Err(invalid_data(
                    "remote incremental request does not continue its active cursor",
                ));
            }
            if let Some(exact_range) = &active.continuation_range
                && &request.range != exact_range
            {
                return Err(invalid_data(
                    "remote incremental continuation changed its exact export range",
                ));
            }
            return Ok(RemoteDeltaApplyTargetSeed::ActiveCow(
                active.generation.clone(),
            ));
        }
        if request.delta_cursor.is_some() {
            return Err(invalid_data(
                "initial remote ingest must begin with a cursorless bootstrap",
            ));
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "initial remote bootstrap must be initialized with a history writer before transport",
        ))
    }

    fn publish(&mut self, next: StoredRemoteDeltaIngestState) -> io::Result<()> {
        self.validate_fence()?;
        self.store.write_state(&next)?;
        self.validate_fence()?;
        self.state = next;
        Ok(())
    }

    fn validate_history_writer(&self, writer: &SourceHistoryWriter<'_, '_, '_>) -> io::Result<()> {
        self.validate_fence()?;
        writer.validate_store_binding(&self.store.history_store)?;
        if writer.redaction_profile() != self.store.binding.redaction_profile {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "history writer redaction namespace does not match remote ingest binding",
            ));
        }
        Ok(())
    }

    fn validate_pending_page(&self, page: &PreparedRemoteDeltaPage) -> io::Result<()> {
        self.validate_fence()?;
        let expected = self.pending_page()?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "remote delta page is not backed by a pending WAL",
            )
        })?;
        if &expected != page {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote delta page does not match the durable pending WAL",
            ));
        }
        Ok(())
    }

    /// Scans every binding namespace while the source-wide ingest lock is
    /// held. Any unreadable, mismatched, or unexpected entry fails closed so
    /// generation GC can never infer an incomplete protected set.
    fn protected_history_generations(
        &self,
    ) -> io::Result<BTreeSet<SourceHistoryRemoteGenerationId>> {
        self.validate_fence()?;
        let source_directory = self.store.source_namespace_directory();
        self.store
            .history_store
            .validate_private_path(&source_directory)?;
        let mut protected = BTreeSet::new();
        let mut binding_count = 0_usize;

        for entry in fs::read_dir(&source_directory)? {
            self.validate_fence()?;
            let entry = entry?;
            let name = entry.file_name();
            if name == OsStr::new(INGEST_LOCK_FILE) {
                continue;
            }
            let Some(name) = name.to_str() else {
                return Err(invalid_data(
                    "remote ingest source namespace contains a non-UTF-8 entry",
                ));
            };
            validate_prefixed_hex(
                name,
                BINDING_NAMESPACE_PREFIX,
                SHA256_HEX_LEN,
                "remote ingest binding namespace",
            )?;
            binding_count = binding_count.saturating_add(1);
            if binding_count > MAX_BINDING_NAMESPACES_PER_SOURCE {
                return Err(invalid_data(format!(
                    "remote ingest source exceeds {MAX_BINDING_NAMESPACES_PER_SOURCE} binding namespaces"
                )));
            }

            let directory = entry.path();
            let metadata = fs::symlink_metadata(&directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(invalid_data(format!(
                    "remote ingest binding namespace {} is not a directory",
                    directory.display()
                )));
            }
            self.store.history_store.validate_private_path(&directory)?;
            cleanup_remote_ingest_temporary_files(&directory)?;
            validate_ingest_binding_namespace_entries(&directory)?;

            let anchor: StoredRemoteDeltaIngestAnchor = read_private_json(
                &directory.join(INGEST_ANCHOR_FILE),
                MAX_INGEST_ANCHOR_BYTES,
                "remote ingest anchor",
            )?;
            anchor.validate(&anchor.binding)?;
            if anchor.binding.profile_id != self.store.binding.profile_id
                || anchor.binding.source.node_id != self.store.binding.source.node_id
                || anchor.binding.redaction_profile != self.store.binding.redaction_profile
                || binding_namespace_component(&anchor.binding)? != name
            {
                return Err(invalid_data(
                    "remote ingest binding namespace does not match its source/profile path",
                ));
            }
            let state: StoredRemoteDeltaIngestState = read_private_json(
                &directory.join(INGEST_STATE_FILE),
                MAX_INGEST_STATE_BYTES,
                "remote ingest state",
            )?;
            state.validate(&anchor.binding)?;
            collect_state_history_generations(&state, &mut protected)?;
        }
        Ok(protected)
    }

    fn validate_fence(&self) -> io::Result<()> {
        self.store.validate_namespace()?;
        validate_private_file(&self.store.lock_path(), &self.lock, "remote ingest lock")
    }
}

impl Drop for RemoteDeltaIngestSession<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
    }
}

/// Applies exactly the pending WAL page to its revision-aware source-history
/// generation. Active pages are fenced against the manifest selected at write
/// time. Staging pages first materialize their invisible generation and then
/// apply both record families there. Repeating either path after a crash is
/// safe because equal-revision, byte-identical records are idempotent.
fn apply_remote_delta_records(
    session: &RemoteDeltaIngestSession<'_>,
    writer: &SourceHistoryWriter<'_, '_, '_>,
    page: &PreparedRemoteDeltaPage,
    activated_at: chrono::DateTime<chrono::Utc>,
) -> io::Result<()> {
    session.validate_history_writer(writer)?;
    session.validate_pending_page(page)?;
    let source_id = &session.store.binding.source.node_id;
    let redaction_profile = session.store.binding.redaction_profile;
    let binding = source_history_binding(&session.store.binding)?;
    match &page.target {
        RemoteDeltaApplyTarget::ActiveNoop {
            expected_generation,
        } => {
            if !page.records.is_empty() {
                return Err(invalid_data(
                    "remote active no-op page unexpectedly contains history mutations",
                ));
            }
            let expected_active = SourceHistoryRemoteActiveRef::new(
                source_history_generation(expected_generation)?,
                binding,
            )?;
            let actual_active = session
                .store
                .history_store
                .active_remote_history_ref(source_id, redaction_profile)?;
            if actual_active.as_ref() != Some(&expected_active) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "remote active history generation changed before no-op apply",
                ));
            }
        }
        RemoteDeltaApplyTarget::ActiveCow {
            expected_generation,
            replacement_generation,
        } => {
            let expected_active = SourceHistoryRemoteActiveRef::new(
                source_history_generation(expected_generation)?,
                binding.clone(),
            )?;
            writer.apply_remote_history_active_page_cow(
                source_id,
                redaction_profile,
                &expected_active,
                &source_history_generation(replacement_generation)?,
                &binding,
                &page.records.bucket_records,
                &page.records.session_digest_records,
                activated_at,
            )?;
        }
        RemoteDeltaApplyTarget::Staging(generation) => {
            let generation = source_history_generation(generation)?;
            writer.ensure_remote_history_generation(
                source_id,
                redaction_profile,
                &generation,
                &binding,
            )?;
            writer.apply_remote_history_generation_page(
                source_id,
                redaction_profile,
                &generation,
                &binding,
                &page.records.bucket_records,
                &page.records.session_digest_records,
            )?;
        }
    }
    Ok(())
}

fn apply_remote_live_state(
    session: &RemoteDeltaIngestSession<'_>,
    writer: &SourceHistoryWriter<'_, '_, '_>,
    page: &PreparedRemoteDeltaPage,
) -> io::Result<()> {
    session.validate_history_writer(writer)?;
    session.validate_pending_page(page)?;
    let pending = session
        .state
        .pending
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no pending delta page"))?;
    let (_, payload) = delta_response_page(&pending.response)?;
    let Some(live) = payload.live.as_ref() else {
        if session.store.binding.range_policy.include_live() {
            return Err(invalid_data(
                "remote live-enabled ingest page has no live state",
            ));
        }
        return Ok(());
    };
    let referenced = live
        .snapshot
        .iter()
        .flat_map(|snapshot| snapshot.tasks.iter())
        .filter_map(|task| task.observed_project_key.as_ref())
        .map(|key| key.as_str())
        .collect::<BTreeSet<_>>();
    let live_descriptors = payload
        .project_descriptors
        .iter()
        .filter(|descriptor| referenced.contains(descriptor.observed_project_key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let mut warning_codes = payload
        .warnings
        .iter()
        .map(|warning| warning.code.clone())
        .collect::<Vec<_>>();
    warning_codes.sort();
    warning_codes.dedup();
    let mut live_partial_reasons = payload
        .coverage
        .partial_reasons
        .iter()
        .filter(|reason| reason.as_str() != "historical_coverage_unproven")
        .cloned()
        .collect::<Vec<_>>();
    live_partial_reasons.sort();
    live_partial_reasons.dedup();
    let live_complete = live_partial_reasons.is_empty() && warning_codes.is_empty();
    writer.record_remote_live_state(
        &session.store.binding.source,
        &session.store.binding.revisions,
        session.store.binding.redaction_profile,
        live,
        &live_descriptors,
        pending.response.observed_at,
        pending.received_at,
        live_complete,
        &live_partial_reasons,
        &warning_codes,
    )
}

/// Replays exactly one pending WAL page into source history and only then
/// commits its cursor/generation transition. Callers acquire the writer lease
/// before the ingest session and release both before any SSH transport I/O.
pub fn apply_and_commit_remote_delta_page(
    session: &mut RemoteDeltaIngestSession<'_>,
    writer: &SourceHistoryWriter<'_, '_, '_>,
    page: &PreparedRemoteDeltaPage,
    activated_at: chrono::DateTime<chrono::Utc>,
) -> io::Result<RemoteDeltaCommitReport> {
    let retired_generation = match &page.target {
        RemoteDeltaApplyTarget::ActiveCow {
            expected_generation,
            ..
        } => Some(expected_generation.clone()),
        RemoteDeltaApplyTarget::ActiveNoop { .. } | RemoteDeltaApplyTarget::Staging(_) => None,
    };
    apply_remote_delta_records(session, writer, page, activated_at)?;
    apply_remote_live_state(session, writer, page)?;
    session.mark_page_applied(&page.id)?;
    let cleanup = retired_generation
        .as_ref()
        .map_or(RemoteGenerationCleanup::NotRequired, |generation| {
            cleanup_retired_generation_after_commit(session, writer, generation)
        });
    Ok(RemoteDeltaCommitReport { cleanup })
}

/// Activates a fully applied bootstrap in strict crash-safe order.
///
/// The source-history manifest switch is idempotent and is always completed
/// before the ingest cursor is committed. If a process exits between those two
/// writes, reopening the session and calling this helper again observes the
/// same ready bootstrap, repeats the already-completed manifest switch, and
/// then commits the cursor.
pub fn activate_remote_delta_bootstrap(
    session: &mut RemoteDeltaIngestSession<'_>,
    writer: &SourceHistoryWriter<'_, '_, '_>,
    activated_at: chrono::DateTime<chrono::Utc>,
) -> io::Result<Option<RemoteBootstrapActivationReport>> {
    session.validate_history_writer(writer)?;
    let Some(activation) = session.bootstrap_activation_required()? else {
        return Ok(None);
    };
    let generation = source_history_generation(&activation.generation)?;
    let candidate_binding = source_history_binding(&session.store.binding)?;
    let expected_active = session
        .state
        .bootstrap
        .as_ref()
        .and_then(|bootstrap| bootstrap.expected_active.as_ref())
        .map(source_history_active_ref)
        .transpose()?;
    writer.activate_remote_history_generation(
        &session.store.binding.source.node_id,
        session.store.binding.redaction_profile,
        expected_active.as_ref(),
        &generation,
        &candidate_binding,
        activated_at,
    )?;
    session.commit_bootstrap_activation(&activation)?;
    let cleanup =
        expected_active
            .as_ref()
            .map_or(RemoteGenerationCleanup::NotRequired, |retired| {
                if retired.generation() == &generation {
                    RemoteGenerationCleanup::NotRequired
                } else {
                    let retired =
                        RemoteHistoryGenerationId(retired.generation().as_str().to_owned());
                    cleanup_retired_generation_after_commit(session, writer, &retired)
                }
            });
    Ok(Some(RemoteBootstrapActivationReport {
        activation,
        cleanup,
    }))
}

fn cleanup_retired_generation_after_commit(
    session: &mut RemoteDeltaIngestSession<'_>,
    writer: &SourceHistoryWriter<'_, '_, '_>,
    candidate: &RemoteHistoryGenerationId,
) -> RemoteGenerationCleanup {
    if !session
        .state
        .retired_generations
        .iter()
        .any(|generation| generation == candidate)
    {
        return RemoteGenerationCleanup::NotRequired;
    }
    let attempt = (|| {
        session.validate_history_writer(writer)?;
        let protected = session.protected_history_generations()?;
        writer.garbage_collect_remote_history_generation(
            &session.store.binding.source.node_id,
            session.store.binding.redaction_profile,
            &source_history_generation(candidate)?,
            &protected,
        )
    })();
    match attempt {
        Ok(outcome) => {
            if matches!(
                outcome,
                RemoteHistoryGenerationGcOutcome::Deleted
                    | RemoteHistoryGenerationGcOutcome::RecoveredTrash
                    | RemoteHistoryGenerationGcOutcome::NotFound
            ) {
                let mut next = session.state.clone();
                next.retired_generations
                    .retain(|generation| generation != candidate);
                if let Err(error) = session.publish(next) {
                    return RemoteGenerationCleanup::Deferred {
                        error_kind: error.kind(),
                        message: error.to_string(),
                    };
                }
            }
            RemoteGenerationCleanup::Completed(outcome)
        }
        Err(error) => RemoteGenerationCleanup::Deferred {
            error_kind: error.kind(),
            message: error.to_string(),
        },
    }
}

/// Retries durable generation retirements left by a crash or a previous
/// fail-closed cleanup. Cleanup failures do not invalidate the already
/// committed ingest position and remain queued for a later local phase.
pub fn retry_deferred_remote_generation_cleanup(
    session: &mut RemoteDeltaIngestSession<'_>,
    writer: &SourceHistoryWriter<'_, '_, '_>,
) -> io::Result<Vec<RemoteGenerationCleanup>> {
    session.validate_history_writer(writer)?;
    let candidates = session.state.retired_generations.clone();
    Ok(candidates
        .iter()
        .take(REMOTE_GENERATION_SWEEP_WORK_LIMIT)
        .map(|candidate| cleanup_retired_generation_after_commit(session, writer, candidate))
        .collect())
}

/// Performs one bounded source-wide trace-and-sweep pass while this session
/// holds the source ingest lock. This recovers deterministic GC trash and
/// removes unreferenced generations left by a crash before ingest state was
/// published. Any incomplete binding scan fails closed before deletion.
pub fn sweep_unreferenced_remote_history_generations(
    session: &RemoteDeltaIngestSession<'_>,
    writer: &SourceHistoryWriter<'_, '_, '_>,
) -> io::Result<RemoteHistoryGenerationSweepReport> {
    session.validate_history_writer(writer)?;
    let protected = session.protected_history_generations()?;
    writer.sweep_remote_history_generations(
        &session.store.binding.source.node_id,
        session.store.binding.redaction_profile,
        &protected,
        REMOTE_GENERATION_SWEEP_WORK_LIMIT,
    )
}

/// Deletes one explicitly selected retired history generation only after a
/// complete, source-wide scan of every durable ingest binding.
///
/// The writer is supplied before this function acquires the source ingest
/// lock, enforcing the global writer -> ingest -> remote-history lock order.
/// Any unknown namespace, unreadable anchor/state, or protected reference
/// fails closed before the history GC primitive is entered.
pub fn garbage_collect_retired_remote_history_generation(
    store: &RemoteDeltaIngestStateStore,
    writer: &SourceHistoryWriter<'_, '_, '_>,
    candidate: &RemoteHistoryGenerationId,
) -> io::Result<RemoteHistoryGenerationGcOutcome> {
    let session = store.try_begin()?;
    session.validate_history_writer(writer)?;
    let protected = session.protected_history_generations()?;
    writer.garbage_collect_remote_history_generation(
        &store.binding.source.node_id,
        store.binding.redaction_profile,
        &source_history_generation(candidate)?,
        &protected,
    )
}

fn source_history_generation(
    generation: &RemoteHistoryGenerationId,
) -> io::Result<SourceHistoryRemoteGenerationId> {
    generation
        .as_str()
        .parse()
        .map_err(|error| invalid_data(format!("remote history generation is invalid: {error}")))
}

fn source_history_binding(
    binding: &RemoteDeltaIngestBinding,
) -> io::Result<SourceHistoryRemoteBinding> {
    SourceHistoryRemoteBinding::new(binding.source.clone(), binding.revisions.clone())
}

fn stored_active_ref_from_history(
    active: &SourceHistoryRemoteActiveRef,
) -> io::Result<StoredRemoteHistoryActiveRef> {
    Ok(StoredRemoteHistoryActiveRef {
        generation: RemoteHistoryGenerationId(active.generation().as_str().to_owned()),
        source: active.binding().source().clone(),
        revisions: active.binding().revisions().clone(),
    })
}

fn source_history_active_ref(
    active: &StoredRemoteHistoryActiveRef,
) -> io::Result<SourceHistoryRemoteActiveRef> {
    active.validate()?;
    SourceHistoryRemoteActiveRef::new(
        source_history_generation(&active.generation)?,
        SourceHistoryRemoteBinding::new(active.source.clone(), active.revisions.clone())?,
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRemoteDeltaIngestAnchor {
    format_version: u32,
    binding: RemoteDeltaIngestBinding,
}

impl StoredRemoteDeltaIngestAnchor {
    fn validate(&self, expected: &RemoteDeltaIngestBinding) -> io::Result<()> {
        if self.format_version != INGEST_ANCHOR_FORMAT_VERSION || &self.binding != expected {
            return Err(invalid_data(
                "remote ingest anchor has an incompatible format or binding",
            ));
        }
        self.binding.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRemoteDeltaIngestState {
    format_version: u32,
    binding: RemoteDeltaIngestBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<StoredActiveGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bootstrap: Option<StoredBootstrap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<StoredPendingPage>,
    /// Generations retired by already-committed transitions. They are not
    /// reader roots; the queue only makes best-effort GC retryable after a
    /// process exit or a fail-closed cross-binding scan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retired_generations: Vec<RemoteHistoryGenerationId>,
}

impl StoredRemoteDeltaIngestState {
    fn new(binding: RemoteDeltaIngestBinding) -> Self {
        Self {
            format_version: INGEST_STATE_FORMAT_VERSION,
            binding,
            active: None,
            bootstrap: None,
            pending: None,
            retired_generations: Vec::new(),
        }
    }

    fn validate(&self, expected: &RemoteDeltaIngestBinding) -> io::Result<()> {
        if self.format_version != INGEST_STATE_FORMAT_VERSION || &self.binding != expected {
            return Err(invalid_data(
                "remote ingest state has an incompatible format or binding",
            ));
        }
        self.binding.validate()?;
        if let Some(active) = &self.active {
            active.validate()?;
        }
        if let Some(bootstrap) = &self.bootstrap {
            bootstrap.validate()?;
            if let Some(expected_active) = &bootstrap.expected_active
                && expected_active.source.node_id != self.binding.source.node_id
            {
                return Err(invalid_data(
                    "remote bootstrap expected-active source belongs to another node",
                ));
            }
            if let Some(active) = &self.active {
                let expected_active = bootstrap.expected_active.as_ref().ok_or_else(|| {
                    invalid_data("remote replacement bootstrap lacks its expected active manifest")
                })?;
                if expected_active.generation != active.generation
                    || expected_active.source != self.binding.source
                    || expected_active.revisions != self.binding.revisions
                {
                    return Err(invalid_data(
                        "remote replacement bootstrap expected-active binding is stale",
                    ));
                }
            }
        }
        if let Some(pending) = &self.pending {
            pending.validate(expected)?;
            match &pending.target {
                RemoteDeltaApplyTarget::ActiveNoop {
                    expected_generation,
                }
                | RemoteDeltaApplyTarget::ActiveCow {
                    expected_generation,
                    ..
                } => {
                    let active = self.active.as_ref().ok_or_else(|| {
                        invalid_data("pending active page has no active history generation")
                    })?;
                    if &active.generation != expected_generation || self.bootstrap.is_some() {
                        return Err(invalid_data(
                            "pending active page targets the wrong durable generation",
                        ));
                    }
                    validate_pending_continuation(
                        pending,
                        Some(active.cursor),
                        active.continuation_range.as_ref(),
                    )?;
                }
                RemoteDeltaApplyTarget::Staging(generation) => {
                    let bootstrap = self.bootstrap.as_ref().ok_or_else(|| {
                        invalid_data("pending staging page has no bootstrap generation")
                    })?;
                    if &bootstrap.generation != generation || bootstrap.ready_to_activate {
                        return Err(invalid_data(
                            "pending staging page targets the wrong durable generation",
                        ));
                    }
                    let exact_range = bootstrap.exact_range.as_ref().ok_or_else(|| {
                        invalid_data("pending staging page has no exact bootstrap range")
                    })?;
                    validate_pending_continuation(pending, bootstrap.cursor, Some(exact_range))?;
                }
            }
        }
        let mut retired = BTreeSet::new();
        let mut live = BTreeSet::new();
        collect_state_history_generations(self, &mut live)?;
        for generation in &self.retired_generations {
            generation.validate()?;
            let generation = source_history_generation(generation)?;
            if !retired.insert(generation.clone()) {
                return Err(invalid_data(
                    "remote ingest state contains a duplicate retired generation",
                ));
            }
            if live.contains(&generation) {
                return Err(invalid_data(
                    "remote ingest state retires a generation that is still live",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredActiveGeneration {
    generation: RemoteHistoryGenerationId,
    cursor: DeltaCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    continuation_range: Option<ExportRange>,
}

impl StoredActiveGeneration {
    fn validate(&self) -> io::Result<()> {
        self.generation.validate()?;
        if let Some(range) = &self.continuation_range {
            validate_export_range(range)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredBootstrap {
    generation: RemoteHistoryGenerationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_active: Option<StoredRemoteHistoryActiveRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exact_range: Option<ExportRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<DeltaCursor>,
    ready_to_activate: bool,
}

impl StoredBootstrap {
    fn validate(&self) -> io::Result<()> {
        self.generation.validate()?;
        if let Some(expected_active) = &self.expected_active {
            expected_active.validate()?;
            if expected_active.generation == self.generation {
                return Err(invalid_data(
                    "remote bootstrap generation must differ from expected active generation",
                ));
            }
        }
        if let Some(range) = &self.exact_range {
            validate_export_range(range)?;
        }
        if self.cursor.is_some() && self.exact_range.is_none() {
            return Err(invalid_data(
                "remote bootstrap cursor lacks its exact range",
            ));
        }
        if self.ready_to_activate && (self.cursor.is_none() || self.exact_range.is_none()) {
            return Err(invalid_data(
                "ready remote bootstrap lacks its cursor or exact range",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRemoteHistoryActiveRef {
    generation: RemoteHistoryGenerationId,
    source: SourceGeneration,
    revisions: ProtocolRevisions,
}

impl StoredRemoteHistoryActiveRef {
    fn validate(&self) -> io::Result<()> {
        self.generation.validate()?;
        SourceHistoryRemoteBinding::new(self.source.clone(), self.revisions.clone()).map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredPendingPage {
    id: PreparedRemoteDeltaPageId,
    target: RemoteDeltaApplyTarget,
    request: RemoteExportRequest,
    response: RemoteDeltaResponse,
    /// Center-clock time captured immediately after the SSH response arrived.
    /// Replaying this WAL must never replace it with the later recovery time,
    /// otherwise an old live snapshot can appear fresh again after a crash.
    received_at: DateTime<Utc>,
}

impl StoredPendingPage {
    fn validate(&self, binding: &RemoteDeltaIngestBinding) -> io::Result<()> {
        self.id.validate()?;
        self.target.validate()?;
        binding.validate_exchange(&self.request, &self.response)?;
        if pending_page_id(self)? != self.id {
            return Err(invalid_data(
                "prepared remote delta page content hash does not match its WAL",
            ));
        }
        if self.target.seed().materialize(&self.id)? != self.target {
            return Err(invalid_data(
                "pending active replacement generation does not match its page identity",
            ));
        }
        let (page, payload) = delta_response_page(&self.response)?;
        payload
            .validate_remote_delta_payload(&RemoteDeltaPayloadContext {
                page,
                request: Some(delta_request(&self.request)?),
                source: &binding.source,
                redaction_profile: binding.redaction_profile,
                revisions: &binding.revisions,
                observed_at: self.response.observed_at,
            })
            .map_err(|error| invalid_data(format!("pending delta payload is invalid: {error}")))?;
        let records = history_records_from_payload(
            &binding.source.node_id,
            binding.redaction_profile,
            payload,
        )?;
        if matches!(self.target, RemoteDeltaApplyTarget::ActiveNoop { .. }) && !records.is_empty() {
            return Err(invalid_data(
                "pending active no-op page contains history mutations",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingPageHashInput<'a> {
    target: RemoteDeltaApplyTargetSeed,
    request: &'a RemoteExportRequest,
    response: &'a RemoteDeltaResponse,
    received_at: DateTime<Utc>,
}

fn pending_page_id(pending: &StoredPendingPage) -> io::Result<PreparedRemoteDeltaPageId> {
    pending_page_id_from_parts(
        &pending.target.seed(),
        &pending.request,
        &pending.response,
        pending.received_at,
    )
}

fn pending_page_id_from_parts(
    target: &RemoteDeltaApplyTargetSeed,
    request: &RemoteExportRequest,
    response: &RemoteDeltaResponse,
    received_at: DateTime<Utc>,
) -> io::Result<PreparedRemoteDeltaPageId> {
    let bytes = serde_json::to_vec(&PendingPageHashInput {
        target: target.clone(),
        request,
        response,
        received_at,
    })
    .map_err(|error| invalid_data(format!("could not encode pending page identity: {error}")))?;
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(PREPARED_PAGE_PREFIX.len() + SHA256_HEX_LEN);
    value.push_str(PREPARED_PAGE_PREFIX);
    append_lower_hex(&mut value, &digest);
    Ok(PreparedRemoteDeltaPageId(value))
}

fn active_replacement_generation(
    page_id: &PreparedRemoteDeltaPageId,
) -> io::Result<RemoteHistoryGenerationId> {
    page_id.validate()?;
    let mut hasher = Sha256::new();
    hasher.update(ACTIVE_REPLACEMENT_GENERATION_DOMAIN);
    hasher.update(page_id.as_str().as_bytes());
    let digest = hasher.finalize();
    let bytes = &digest[..INGEST_GENERATION_RANDOM_BYTES];
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(invalid_data(
            "prepared page produced an unusable active replacement generation",
        ));
    }
    let mut value = String::with_capacity(INGEST_GENERATION_PREFIX.len() + bytes.len() * 2);
    value.push_str(INGEST_GENERATION_PREFIX);
    append_lower_hex(&mut value, bytes);
    let generation = RemoteHistoryGenerationId(value);
    generation.validate()?;
    Ok(generation)
}

fn validate_ingest_binding_namespace_entries(directory: &Path) -> io::Result<()> {
    let mut state_seen = false;
    let mut anchor_seen = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let label = if name == OsStr::new(INGEST_STATE_FILE) {
            state_seen = true;
            "remote ingest state"
        } else if name == OsStr::new(INGEST_ANCHOR_FILE) {
            anchor_seen = true;
            "remote ingest anchor"
        } else {
            return Err(invalid_data(format!(
                "remote ingest binding namespace contains unexpected entry {}",
                entry.path().display()
            )));
        };
        let mut options = OpenOptions::new();
        options.read(true);
        configure_private_open(&mut options, false);
        let file = options.open(entry.path())?;
        validate_private_file(&entry.path(), &file, label)?;
    }
    if !state_seen || !anchor_seen {
        return Err(invalid_data(
            "remote ingest binding namespace is missing its state or anchor",
        ));
    }
    Ok(())
}

fn collect_state_history_generations(
    state: &StoredRemoteDeltaIngestState,
    protected: &mut BTreeSet<SourceHistoryRemoteGenerationId>,
) -> io::Result<()> {
    let mut insert = |generation: &RemoteHistoryGenerationId| -> io::Result<()> {
        protected.insert(source_history_generation(generation)?);
        Ok(())
    };
    if let Some(active) = &state.active {
        insert(&active.generation)?;
    }
    if let Some(bootstrap) = &state.bootstrap {
        insert(&bootstrap.generation)?;
        if let Some(expected_active) = &bootstrap.expected_active {
            insert(&expected_active.generation)?;
        }
    }
    if let Some(pending) = &state.pending {
        match &pending.target {
            RemoteDeltaApplyTarget::ActiveNoop {
                expected_generation,
            } => insert(expected_generation)?,
            RemoteDeltaApplyTarget::ActiveCow {
                expected_generation,
                replacement_generation,
            } => {
                insert(expected_generation)?;
                insert(replacement_generation)?;
            }
            RemoteDeltaApplyTarget::Staging(generation) => insert(generation)?,
        }
    }
    Ok(())
}

fn queue_retired_generation(
    state: &mut StoredRemoteDeltaIngestState,
    generation: &RemoteHistoryGenerationId,
) {
    if !state
        .retired_generations
        .iter()
        .any(|candidate| candidate == generation)
    {
        state.retired_generations.push(generation.clone());
    }
}

fn binding_namespace_component(binding: &RemoteDeltaIngestBinding) -> io::Result<String> {
    let bytes = serde_json::to_vec(binding).map_err(|error| {
        invalid_data(format!("could not encode remote ingest binding: {error}"))
    })?;
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(BINDING_NAMESPACE_PREFIX.len() + SHA256_HEX_LEN);
    value.push_str(BINDING_NAMESPACE_PREFIX);
    append_lower_hex(&mut value, &digest);
    Ok(value)
}

#[derive(Deserialize)]
struct RetiredIngestBindingEnvelope {
    binding: RemoteDeltaIngestBinding,
}

fn validate_ingest_retirement_writer(
    history_store: &SourceHistoryStore,
    writer: &SourceHistoryWriter<'_, '_, '_>,
) -> io::Result<()> {
    writer.validate_store_binding(history_store)?;
    if writer.redaction_profile() != RedactionProfile::Redacted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote preview ingest retirement requires redacted writer authority",
        ));
    }
    Ok(())
}

fn private_directory_exists(store: &SourceHistoryStore, path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(invalid_data(format!(
                    "remote ingest retirement path {} is not a directory",
                    path.display()
                )));
            }
            store.validate_private_path(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn private_file_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(invalid_data(format!(
                    "remote ingest retirement path {} is not a regular file",
                    path.display()
                )));
            }
            let mut options = OpenOptions::new();
            options.read(true);
            configure_private_open(&mut options, false);
            let file = options.open(path)?;
            validate_private_file(path, &file, "remote ingest retirement marker")?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_remote_ingest_retirement_marker(paths: &RemoteIngestRetirementPaths) -> io::Result<()> {
    if private_file_exists(&paths.marker)? {
        let marker: StoredRemoteIngestProfileRetirement = read_private_json(
            &paths.marker,
            MAX_INGEST_ANCHOR_BYTES,
            "remote ingest retirement marker",
        )?;
        return marker.validate(&paths.profile_id, &paths.source_id);
    }
    let marker = StoredRemoteIngestProfileRetirement::preview_to_redacted(
        paths.profile_id.clone(),
        paths.source_id.clone(),
    );
    write_private_json_atomically(
        &paths.marker,
        &marker,
        MAX_INGEST_ANCHOR_BYTES,
        "remote ingest retirement marker",
    )
}

fn remove_remote_preview_ingest_source_bounded(
    history_store: &SourceHistoryStore,
    paths: &RemoteIngestRetirementPaths,
) -> io::Result<bool> {
    history_store.validate_private_path(&paths.preview_source)?;
    let lock = open_private_lock(&paths.preview_lock)?;
    try_lock_private_lock(&paths.preview_lock, &lock)?;
    // The external retirement marker remains until explicit source purge, so
    // every partial or rolled-back deletion stays replayable on Windows.
    sync_directory(&paths.preview_source)?;
    let mut remaining = INGEST_RETIREMENT_WORK_LIMIT;
    let mut binding_count = 0_usize;
    let mut completed = true;
    for entry in fs::read_dir(&paths.preview_source)? {
        history_store.validate_private_path(&paths.preview_source)?;
        let entry = entry?;
        if entry.file_name() == OsStr::new(INGEST_LOCK_FILE) {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("remote ingest retirement entry is not UTF-8"))?
            .to_owned();
        validate_prefixed_hex(
            &name,
            BINDING_NAMESPACE_PREFIX,
            SHA256_HEX_LEN,
            "retired remote ingest binding namespace",
        )?;
        binding_count = binding_count.saturating_add(1);
        if binding_count > MAX_BINDING_NAMESPACES_PER_SOURCE {
            return Err(invalid_data(format!(
                "retired remote ingest source exceeds {MAX_BINDING_NAMESPACES_PER_SOURCE} binding namespaces"
            )));
        }
        if remaining == 0 {
            completed = false;
            break;
        }
        if !remove_remote_ingest_binding_bounded(
            history_store,
            paths,
            &entry.path(),
            &name,
            &mut remaining,
        )? {
            completed = false;
            break;
        }
    }
    if !completed {
        return Ok(false);
    }

    // The iterator above completed, so the stable lock must be the only
    // remaining path. Revalidate its opened identity before releasing it.
    let mut entries = fs::read_dir(&paths.preview_source)?;
    while let Some(entry) = entries.next().transpose()? {
        if entry.file_name() != OsStr::new(INGEST_LOCK_FILE) {
            return Ok(false);
        }
    }
    // `ReadDir` owns a directory handle on Windows. Release it before trying
    // to remove the stable lock and its parent directory.
    drop(entries);
    validate_private_file(&paths.preview_lock, &lock, "remote ingest lock")?;
    FileExt::unlock(&lock)?;
    drop(lock);

    let lock = open_existing_private_file(&paths.preview_lock, "remote ingest lock")?;
    drop(lock);
    fs::remove_file(&paths.preview_lock)?;
    sync_directory(&paths.preview_source)?;
    fs::remove_dir(&paths.preview_source)?;
    sync_directory(&paths.preview_profile)?;
    Ok(true)
}

fn remove_remote_ingest_binding_bounded(
    history_store: &SourceHistoryStore,
    paths: &RemoteIngestRetirementPaths,
    directory: &Path,
    namespace: &str,
    remaining: &mut usize,
) -> io::Result<bool> {
    history_store.validate_private_path(directory)?;
    let state_path = directory.join(INGEST_STATE_FILE);
    let anchor_path = directory.join(INGEST_ANCHOR_FILE);
    let state_binding = read_optional_retired_ingest_binding(
        &state_path,
        MAX_INGEST_STATE_BYTES,
        "retired remote ingest state",
    )?;
    let anchor_binding = read_optional_retired_ingest_binding(
        &anchor_path,
        MAX_INGEST_ANCHOR_BYTES,
        "retired remote ingest anchor",
    )?;
    if let (Some(state), Some(anchor)) = (&state_binding, &anchor_binding)
        && state != anchor
    {
        return Err(invalid_data(
            "retired remote ingest state and anchor bindings disagree",
        ));
    }
    if let Some(binding) = state_binding.as_ref().or(anchor_binding.as_ref()) {
        validate_retired_ingest_binding(paths, namespace, binding)?;
    }

    for entry in fs::read_dir(directory)? {
        history_store.validate_private_path(directory)?;
        if *remaining == 0 {
            return Ok(false);
        }
        let entry = entry?;
        let name = entry.file_name();
        if name != OsStr::new(INGEST_STATE_FILE)
            && name != OsStr::new(INGEST_ANCHOR_FILE)
            && !is_remote_ingest_temporary_name(&name)
        {
            return Err(invalid_data(format!(
                "retired remote ingest binding contains unexpected entry {}",
                entry.path().display()
            )));
        }
        let file = open_existing_private_file(&entry.path(), "retired remote ingest binding file")?;
        drop(file);
        fs::remove_file(entry.path())?;
        *remaining -= 1;
    }
    if *remaining == 0 {
        return Ok(false);
    }
    history_store.validate_private_path(directory)?;
    fs::remove_dir(directory)?;
    *remaining -= 1;
    sync_directory(&paths.preview_source)?;
    Ok(true)
}

fn read_optional_retired_ingest_binding(
    path: &Path,
    maximum_bytes: u64,
    label: &str,
) -> io::Result<Option<RemoteDeltaIngestBinding>> {
    if !private_file_exists(path)? {
        return Ok(None);
    }
    let envelope: RetiredIngestBindingEnvelope = read_private_json(path, maximum_bytes, label)?;
    Ok(Some(envelope.binding))
}

fn validate_retired_ingest_binding(
    paths: &RemoteIngestRetirementPaths,
    namespace: &str,
    binding: &RemoteDeltaIngestBinding,
) -> io::Result<()> {
    if binding.profile_id() != &paths.profile_id
        || binding.source().node_id != paths.source_id
        || binding.redaction_profile() != RedactionProfile::PreviewEnabled
        || binding_namespace_component(binding)? != namespace
    {
        return Err(invalid_data(
            "retired remote ingest binding does not match its source/profile namespace",
        ));
    }
    Ok(())
}

fn open_existing_private_file(path: &Path, label: &str) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_private_open(&mut options, false);
    let file = options.open(path)?;
    validate_private_file(path, &file, label)?;
    Ok(file)
}

fn validate_pending_continuation(
    pending: &StoredPendingPage,
    expected_cursor: Option<DeltaCursor>,
    exact_range: Option<&ExportRange>,
) -> io::Result<()> {
    let request = delta_request(&pending.request)?;
    if request.delta_cursor != expected_cursor {
        return Err(invalid_data(
            "pending remote delta page does not continue its durable cursor",
        ));
    }
    if let Some(exact_range) = exact_range
        && &request.range != exact_range
    {
        return Err(invalid_data(
            "pending remote delta page changed its exact continuation range",
        ));
    }
    Ok(())
}

fn prepared_from_pending(
    pending: &StoredPendingPage,
    records: RemoteDeltaHistoryRecords,
    page: &DeltaPage,
) -> PreparedRemoteDeltaPage {
    PreparedRemoteDeltaPage {
        id: pending.id.clone(),
        target: pending.target.clone(),
        records,
        next_cursor: page.next_delta_cursor,
        has_more: page.has_more,
    }
}

fn delta_request(request: &RemoteExportRequest) -> io::Result<&DeltaRequest> {
    match &request.request {
        RemoteExportRequestBody::Delta(delta) => Ok(delta),
        _ => Err(invalid_data("remote ingest accepts only delta requests")),
    }
}

fn delta_response_page(response: &RemoteDeltaResponse) -> io::Result<(&DeltaPage, &DeltaPayload)> {
    match &response.result {
        RemoteExportResponseBody::Delta { page, payload } => Ok((page, payload)),
        RemoteExportResponseBody::Failure(_) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote failure cannot be persisted as a delta page",
        )),
        _ => Err(invalid_data("remote ingest accepts only delta responses")),
    }
}

fn history_records_from_payload(
    source_id: &crate::source_identity::NodeId,
    redaction_profile: RedactionProfile,
    payload: &DeltaPayload,
) -> io::Result<RemoteDeltaHistoryRecords> {
    let descriptors = descriptor_index(&payload.project_descriptors)?;
    let mut bucket_records = Vec::with_capacity(payload.bucket_changes.len());
    for change in &payload.bucket_changes {
        let record = match &change.mutation {
            RemoteUsageBucketMutation::Upsert(bucket) => SourceBucketRecord::upsert(
                change.revision.get(),
                local_bucket(bucket, redaction_profile, &descriptors)?,
            )?,
            RemoteUsageBucketMutation::Tombstone => {
                SourceBucketRecord::tombstone(change.starts_at, change.revision.get())?
            }
        };
        bucket_records.push(record);
    }

    let mut session_digest_records = Vec::with_capacity(payload.session_digest_changes.len());
    for change in &payload.session_digest_changes {
        let record = match &change.mutation {
            RemoteSessionDigestMutation::Upsert(digest) => {
                SourceSessionDigestRecord::upsert_with_retention_through(
                    change.revision.get(),
                    local_session_digest(source_id, digest)?,
                    change.retention_through,
                )?
            }
            RemoteSessionDigestMutation::Tombstone => {
                SourceSessionDigestRecord::tombstone_with_retention_through(
                    change.thread_id.clone(),
                    change.range_start,
                    change.range_end,
                    change.changed_at,
                    change.retention_through,
                    change.revision.get(),
                )?
            }
        };
        session_digest_records.push(record);
    }
    Ok(RemoteDeltaHistoryRecords {
        bucket_records,
        session_digest_records,
    })
}

fn descriptor_index(
    descriptors: &[RemoteProjectDescriptor],
) -> io::Result<BTreeMap<ObservedProjectKey, &RemoteProjectDescriptor>> {
    let mut result = BTreeMap::new();
    for descriptor in descriptors {
        if result
            .insert(descriptor.observed_project_key.clone(), descriptor)
            .is_some()
        {
            return Err(invalid_data(
                "remote delta page contains duplicate project descriptors",
            ));
        }
    }
    Ok(result)
}

fn local_bucket(
    bucket: &RemoteUsageBucket,
    redaction_profile: RedactionProfile,
    descriptors: &BTreeMap<ObservedProjectKey, &RemoteProjectDescriptor>,
) -> io::Result<LocalHalfHourBucket> {
    if bucket.metric_revision.get() != HISTORY_METRIC_REVISION {
        return Err(invalid_data(
            "remote bucket metric revision cannot be represented by source history",
        ));
    }
    let mut groups = Vec::with_capacity(bucket.model_groups.len());
    for group in &bucket.model_groups {
        groups.push(LocalUsageGroup {
            model: group.model.clone(),
            service_tier: group.service_tier.clone(),
            token_usage: local_token_usage(group.token_usage),
            estimated_cost_units: group.estimated_cost_units.value(),
            api_long_context_extra_cost_units: group
                .api_long_context_extra_cost_units
                .map(|value| value.value()),
            call_count: group.call_count,
            used_model_fallback: group.used_model_fallback,
            used_token_breakdown_fallback: group.used_token_breakdown_fallback,
            used_long_context_pricing: group.used_long_context_pricing,
            used_long_context_detection_fallback: group.used_long_context_detection_fallback,
        });
    }

    let mut project_groups = Vec::with_capacity(bucket.project_groups.len());
    let mut retained_api_cost = ApiCostAmount::default();
    for group in &bucket.project_groups {
        let (project_id, project_label) = match &group.observed_project_key {
            Some(key) => {
                let descriptor = descriptors.get(key).ok_or_else(|| {
                    invalid_data(format!(
                        "remote bucket references missing project descriptor {}",
                        key.as_str()
                    ))
                })?;
                (
                    Some(key.as_str().to_owned()),
                    Some(descriptor.display_label.as_str().to_owned()),
                )
            }
            None => (None, None),
        };
        let api_equivalent_cost = local_api_cost(group.api_equivalent_cost);
        retained_api_cost = checked_add_api_cost(retained_api_cost, api_equivalent_cost)?;
        project_groups.push(LocalProjectUsageGroup {
            thread_id: group.emitting_thread_id.as_str().to_owned(),
            turn_id: group.emitting_turn_id.clone(),
            parent_thread_id: group
                .parent_thread_id
                .as_ref()
                .map(|thread| thread.as_str().to_owned()),
            session_thread_id: group
                .root_session_thread_id
                .as_ref()
                .map(|thread| thread.as_str().to_owned()),
            session_turn_id: group.root_session_turn_id.clone(),
            message_preview: group.message_preview.clone(),
            turn_started_at: None,
            project_id,
            project_label,
            title: group.title_preview.clone(),
            source: None,
            token_usage: local_token_usage(group.token_usage),
            estimated_cost_units: group.estimated_cost_units.value(),
            api_long_context_extra_cost_units: group
                .api_long_context_extra_cost_units
                .map(|value| value.value()),
            api_equivalent_cost,
            call_count: group.call_count,
        });
    }
    let aggregate_api_cost = local_api_cost(bucket.api_equivalent_cost);
    if retained_api_cost != aggregate_api_cost {
        return Err(invalid_data(
            "source history cannot losslessly retain remote bucket API cost outside project groups",
        ));
    }
    if redaction_profile == RedactionProfile::Redacted
        && project_groups
            .iter()
            .any(|group| group.title.is_some() || group.message_preview.is_some())
    {
        return Err(invalid_data(
            "redacted remote bucket contains preview content",
        ));
    }

    Ok(LocalHalfHourBucket {
        starts_at: bucket.starts_at,
        ends_at: bucket.ends_at,
        sampled_at: bucket.sampled_at,
        token_usage: local_token_usage(bucket.token_usage),
        estimated_cost_units: bucket.estimated_cost_units.value(),
        api_long_context_extra_cost_units: bucket
            .api_long_context_extra_cost_units
            .map(|value| value.value()),
        long_context_usage_unknown: bucket.long_context_usage_unknown,
        estimator_revision: bucket.estimator_revision.get(),
        project_breakdown_revision: bucket.project_breakdown_revision.get(),
        api_pricing_catalog_revision: bucket.api_pricing_catalog_revision.get(),
        call_count: bucket.call_count,
        groups,
        project_groups,
        partial_reasons: bucket.partial_reasons.clone(),
    })
}

fn local_session_digest(
    source_id: &crate::source_identity::NodeId,
    digest: &RemoteSessionDigest,
) -> io::Result<SourceSessionDigest> {
    SourceSessionDigest::new(
        SessionReplicaKey::new(source_id.clone(), digest.thread_id.clone()),
        digest.range_start,
        digest.range_end,
        digest.covered_through,
        SessionDigestFingerprint::from_str(digest.fingerprint.as_str()).map_err(|error| {
            invalid_data(format!(
                "remote session digest fingerprint is invalid: {error}"
            ))
        })?,
        SessionDigestFingerprint::from_str(digest.project_breakdown_fingerprint.as_str()).map_err(
            |error| {
                invalid_data(format!(
                    "remote session project fingerprint is invalid: {error}"
                ))
            },
        )?,
        digest.event_count,
        digest.exact_event_identity,
        digest.coverage_complete,
        digest.observed_project_keys.clone(),
        local_session_metrics(&digest.metrics),
    )
}

fn local_session_metrics(metrics: &RemoteSessionUsageMetrics) -> SessionUsageMetrics {
    SessionUsageMetrics {
        token_usage: local_token_usage(metrics.token_usage),
        estimated_cost_units: metrics.estimated_cost_units.value(),
        api_long_context_extra_cost_units: metrics
            .api_long_context_extra_cost_units
            .map(|value| value.value()),
        api_equivalent_cost: local_api_cost(metrics.api_equivalent_cost),
        call_count: metrics.call_count,
        metric_revision: metrics.metric_revision.get(),
        estimator_revision: metrics.estimator_revision.get(),
        project_breakdown_revision: metrics.project_breakdown_revision.get(),
        api_pricing_catalog_revision: metrics.api_pricing_catalog_revision.get(),
        partial_reasons: metrics.partial_reasons.clone(),
    }
}

fn local_token_usage(usage: RemoteTokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        total_tokens: usage.total_tokens,
    }
}

fn local_api_cost(cost: RemoteApiCostAmount) -> ApiCostAmount {
    ApiCostAmount {
        minimum_pico_usd: PicoUsd::new(cost.minimum_pico_usd.value()),
        maximum_pico_usd: PicoUsd::new(cost.maximum_pico_usd.value()),
        observed_samples: cost.observed_samples,
        priced_samples: cost.priced_samples,
        observed_tokens: cost.observed_tokens,
        priced_tokens: cost.priced_tokens,
    }
}

fn checked_add_api_cost(left: ApiCostAmount, right: ApiCostAmount) -> io::Result<ApiCostAmount> {
    Ok(ApiCostAmount {
        minimum_pico_usd: PicoUsd::new(
            left.minimum_pico_usd
                .value()
                .checked_add(right.minimum_pico_usd.value())
                .ok_or_else(|| invalid_data("remote API cost minimum overflows"))?,
        ),
        maximum_pico_usd: PicoUsd::new(
            left.maximum_pico_usd
                .value()
                .checked_add(right.maximum_pico_usd.value())
                .ok_or_else(|| invalid_data("remote API cost maximum overflows"))?,
        ),
        observed_samples: left
            .observed_samples
            .checked_add(right.observed_samples)
            .ok_or_else(|| invalid_data("remote API cost sample count overflows"))?,
        priced_samples: left
            .priced_samples
            .checked_add(right.priced_samples)
            .ok_or_else(|| invalid_data("remote API priced sample count overflows"))?,
        observed_tokens: left
            .observed_tokens
            .checked_add(right.observed_tokens)
            .ok_or_else(|| invalid_data("remote API observed token count overflows"))?,
        priced_tokens: left
            .priced_tokens
            .checked_add(right.priced_tokens)
            .ok_or_else(|| invalid_data("remote API priced token count overflows"))?,
    })
}

fn validate_export_range(range: &ExportRange) -> io::Result<()> {
    if range.from >= range.to || range.to.signed_duration_since(range.from) > Duration::days(35) {
        return Err(invalid_data("stored remote export range is invalid"));
    }
    Ok(())
}

fn append_lower_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
}

fn validate_prefixed_hex(value: &str, prefix: &str, hex_len: usize, label: &str) -> io::Result<()> {
    let Some(hex) = value.strip_prefix(prefix) else {
        return Err(invalid_data(format!("{label} has an invalid prefix")));
    };
    if hex.len() != hex_len
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_data(format!("{label} is not canonical lower hex")));
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
}

fn open_private_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        options.share_mode(remote_ingest_lock_share_mode());
    }
    configure_private_open(&mut options, false);
    let file = options.open(path)?;
    validate_private_file(path, &file, "remote ingest lock")?;
    Ok(file)
}

fn try_lock_private_lock(path: &Path, file: &File) -> io::Result<()> {
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if lock_is_contended(&error) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote delta ingest is already running for this source",
            ));
        }
        Err(error) => return Err(error),
    }

    // The pathname may have been displaced after open but before the OS lock
    // was acquired. Revalidate only after acquisition so a session can never
    // coordinate through an unlinked/replaced lock-file identity.
    validate_private_file(path, file, "remote ingest lock")
}

#[cfg(any(test, windows))]
fn remote_ingest_lock_share_mode() -> u32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        FILE_SHARE_READ | FILE_SHARE_WRITE
    }
    #[cfg(not(windows))]
    {
        // Win32 constants retained in test builds so this policy has a
        // host-independent regression test.
        0x0000_0001 | 0x0000_0002
    }
}

fn read_private_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum_bytes: u64,
    label: &str,
) -> io::Result<T> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_private_open(&mut options, false);
    let mut file = options.open(path)?;
    validate_private_file(path, &file, label)?;
    let length = file.metadata()?.len();
    if length > maximum_bytes {
        return Err(invalid_data(format!("{label} exceeds its byte bound")));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(maximum_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(invalid_data(format!("{label} exceeds its byte bound")));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("could not decode {label}: {error}")))
}

fn write_private_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
    maximum_bytes: u64,
    label: &str,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| invalid_data(format!("could not encode {label}: {error}")))?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(invalid_data(format!("{label} exceeds its byte bound")));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data(format!("{label} has no parent directory")))?;
    let (temporary_path, mut temporary) = create_private_temp(parent, label)?;
    let result = (|| {
        temporary.write_all(&bytes)?;
        temporary.sync_all()?;
        replace_file(&temporary_path, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_private_json_once<T: Serialize>(
    path: &Path,
    value: &T,
    maximum_bytes: u64,
    label: &str,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| invalid_data(format!("could not encode {label}: {error}")))?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(invalid_data(format!("{label} exceeds its byte bound")));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data(format!("{label} has no parent directory")))?;
    let (temporary_path, mut temporary) = create_private_temp(parent, label)?;
    let result = (|| {
        temporary.write_all(&bytes)?;
        temporary.sync_all()?;
        drop(temporary);

        // Publishing a fully synced inode with a hard link gives portable
        // create-if-absent semantics: unlike a direct create_new write, a
        // crash can never expose a truncated final anchor; unlike rename on
        // Unix, a racing existing final path is never overwritten.
        match fs::hard_link(&temporary_path, path) {
            Ok(()) => sync_directory(parent),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let cleanup = fs::remove_file(&temporary_path);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        (Ok(()), Err(error)) => Err(error),
    }
}

fn cleanup_remote_ingest_temporary_files(directory: &Path) -> io::Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !is_remote_ingest_temporary_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid_data(format!(
                "remote ingest temporary path {} is not a regular file",
                path.display()
            )));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        configure_private_open(&mut options, false);
        let file = options.open(&path)?;
        validate_private_file(&path, &file, "remote ingest temporary file")?;
        drop(file);
        fs::remove_file(&path)?;
        removed += 1;
    }
    if removed > 0 {
        sync_directory(directory)?;
    }
    Ok(removed)
}

fn is_remote_ingest_temporary_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(suffix) = name.strip_prefix(".remote-ingest.tmp.") else {
        return false;
    };
    let Some((process_id, sequence)) = suffix.split_once('.') else {
        return false;
    };
    !process_id.is_empty()
        && !sequence.is_empty()
        && process_id.bytes().all(|byte| byte.is_ascii_digit())
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

fn create_private_temp(parent: &Path, label: &str) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".remote-ingest.tmp.{}.{}",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        configure_private_open(&mut options, false);
        match options.open(&path) {
            Ok(file) => {
                validate_private_file(&path, &file, label)?;
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("could not allocate a temporary file for {label}"),
    ))
}

fn validate_private_file(path: &Path, file: &File, label: &str) -> io::Result<()> {
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(invalid_data(format!("{label} is not a regular file")));
    }
    let linked = fs::symlink_metadata(path)?;
    if linked.file_type().is_symlink() || !linked.is_file() {
        return Err(invalid_data(format!("{label} path is not a regular file")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != linked.dev() || opened.ino() != linked.ino() {
            return Err(invalid_data(format!("{label} path changed while open")));
        }
        if opened.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{label} must not be accessible by group or other users"),
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_private_file(path, file, label)?;
    Ok(())
}

fn configure_private_open(options: &mut OpenOptions, _directory: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use chrono::{DateTime, TimeZone, Utc};
    use tempfile::tempdir;

    use super::*;
    use crate::history_ownership::{
        HistoryOwnershipStore, InitializeV1Outcome, OwnershipCasOutcome,
    };
    use crate::remote_protocol::{
        AcceptedRevisionRange, AcceptedRevisions, BinaryVersion, MAX_REMOTE_FRAME_ENCODED_BYTES,
        REMOTE_PROTOCOL_VERSION, RemoteDeltaCoverage, RemoteDeltaStats, RemoteDeltaWarning,
        RemoteExportResponse, RemoteLiveSnapshot, RemoteLiveState, RemoteSessionDigestChange,
        RemoteSessionDigestFingerprint, RemoteTiming, RemoteU128, RemoteUsageBucketChange,
    };
    use crate::source_history::{
        SourceBucketChange, SourceHistoryWriter, SourceKind, SourceMetadata,
        SourceSessionDigestChange,
    };

    const PROFILE: &str = "0123456789abcdef";
    const OTHER_PROFILE: &str = "fedcba9876543210";
    const SOURCE: &str = "node-0123456789abcdef0123456789abcdef";

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn nonzero32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn nonzero64(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn source(generation: u64) -> SourceGeneration {
        SourceGeneration {
            node_id: SOURCE.parse().unwrap(),
            generation: nonzero64(generation),
        }
    }

    fn revisions(estimator: u32) -> ProtocolRevisions {
        ProtocolRevisions {
            history_format: nonzero32(2),
            metric: nonzero32(HISTORY_METRIC_REVISION),
            estimator: nonzero32(estimator),
            project_breakdown: nonzero32(2),
            api_pricing_catalog: nonzero32(6),
        }
    }

    fn accepted(revisions: &ProtocolRevisions) -> AcceptedRevisions {
        fn exact(value: NonZeroU32) -> AcceptedRevisionRange {
            AcceptedRevisionRange {
                min: value,
                max: value,
            }
        }
        AcceptedRevisions {
            history_format: exact(revisions.history_format),
            metric: exact(revisions.metric),
            estimator: exact(revisions.estimator),
            project_breakdown: exact(revisions.project_breakdown),
            api_pricing_catalog: exact(revisions.api_pricing_catalog),
        }
    }

    fn binding_with(
        generation: u64,
        estimator: u32,
        window_minutes: u32,
    ) -> RemoteDeltaIngestBinding {
        RemoteDeltaIngestBinding::new(
            PROFILE.parse().unwrap(),
            source(generation),
            RedactionProfile::Redacted,
            revisions(estimator),
            RemoteDeltaRangePolicy::new(nonzero32(window_minutes), 60, false).unwrap(),
        )
        .unwrap()
    }

    fn live_binding_with(generation: u64, estimator: u32) -> RemoteDeltaIngestBinding {
        RemoteDeltaIngestBinding::new(
            PROFILE.parse().unwrap(),
            source(generation),
            RedactionProfile::Redacted,
            revisions(estimator),
            RemoteDeltaRangePolicy::new(nonzero32(60), 60, true).unwrap(),
        )
        .unwrap()
    }

    fn export_range(from: DateTime<Utc>, minutes: i64) -> ExportRange {
        ExportRange {
            from,
            to: from + Duration::minutes(minutes),
        }
    }

    fn request(
        binding: &RemoteDeltaIngestBinding,
        cursor: Option<DeltaCursor>,
        range: ExportRange,
    ) -> RemoteExportRequest {
        RemoteExportRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            client_version: BinaryVersion::from_str("0.4.0-test").unwrap(),
            expected_source: Some(binding.source.clone()),
            redaction_profile: binding.redaction_profile,
            max_page_bytes: MAX_REMOTE_FRAME_ENCODED_BYTES as u32,
            accepted_revisions: accepted(&binding.revisions),
            request: RemoteExportRequestBody::Delta(DeltaRequest {
                delta_cursor: cursor,
                range,
                overlap_minutes: binding.range_policy.overlap_minutes,
                include_live: binding.range_policy.include_live,
                known_live_revision: None,
            }),
        }
    }

    fn payload_for(
        range: ExportRange,
        bucket_changes: Vec<RemoteUsageBucketChange>,
        session_digest_changes: Vec<RemoteSessionDigestChange>,
    ) -> DeltaPayload {
        let scanned = bucket_changes.len() + session_digest_changes.len();
        DeltaPayload {
            coverage: RemoteDeltaCoverage {
                requested_range: range.clone(),
                covered_range: Some(range),
                range_complete: true,
                partial_reasons: Vec::new(),
            },
            project_descriptors: Vec::new(),
            stats: RemoteDeltaStats {
                journal_records_scanned: scanned as u64,
                bucket_changes_emitted: bucket_changes.len() as u64,
                session_digest_changes_emitted: session_digest_changes.len() as u64,
                ..RemoteDeltaStats::default()
            },
            bucket_changes,
            session_digest_changes,
            live: None,
            warnings: Vec::new(),
        }
    }

    fn response(
        binding: &RemoteDeltaIngestBinding,
        range: ExportRange,
        page: DeltaPage,
        payload: DeltaPayload,
    ) -> RemoteDeltaResponse {
        let received_at = at(30, 4, 0);
        RemoteExportResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: BinaryVersion::from_str("0.4.0-test").unwrap(),
            source: binding.source.clone(),
            redaction_profile: binding.redaction_profile,
            revisions: binding.revisions.clone(),
            observed_at: received_at + Duration::seconds(1),
            timing: RemoteTiming {
                remote_received_at: received_at,
                remote_sent_at: received_at + Duration::seconds(1),
            },
            result: RemoteExportResponseBody::Delta {
                page,
                payload: {
                    assert_eq!(payload.coverage.requested_range, range);
                    payload
                },
            },
        }
    }

    fn empty_page(generation: u64, sequence: u64) -> DeltaPage {
        DeltaPage {
            generation: nonzero64(generation),
            from_sequence: sequence,
            through_sequence: sequence,
            next_delta_cursor: DeltaCursor {
                generation: nonzero64(generation),
                sequence,
            },
            has_more: false,
        }
    }

    fn tombstone_page(
        generation: u64,
        sequence: u64,
        starts_at: DateTime<Utc>,
    ) -> (DeltaPage, Vec<RemoteUsageBucketChange>) {
        (
            DeltaPage {
                generation: nonzero64(generation),
                from_sequence: sequence,
                through_sequence: sequence,
                next_delta_cursor: DeltaCursor {
                    generation: nonzero64(generation),
                    sequence,
                },
                has_more: false,
            },
            vec![RemoteUsageBucketChange {
                sequence: nonzero64(sequence),
                starts_at,
                revision: nonzero64(sequence),
                mutation: RemoteUsageBucketMutation::Tombstone,
            }],
        )
    }

    fn bucket_change(
        binding: &RemoteDeltaIngestBinding,
        sequence: u64,
        starts_at: DateTime<Utc>,
        total_tokens: u64,
    ) -> RemoteUsageBucketChange {
        RemoteUsageBucketChange {
            sequence: nonzero64(sequence),
            starts_at,
            revision: nonzero64(sequence),
            mutation: RemoteUsageBucketMutation::Upsert(Box::new(RemoteUsageBucket {
                starts_at,
                ends_at: starts_at + Duration::minutes(15),
                sampled_at: starts_at + Duration::minutes(15),
                token_usage: RemoteTokenUsage {
                    input_tokens: total_tokens,
                    total_tokens,
                    ..RemoteTokenUsage::default()
                },
                estimated_cost_units: RemoteU128::new(u128::from(total_tokens)),
                api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
                long_context_usage_unknown: false,
                api_equivalent_cost: RemoteApiCostAmount::default(),
                call_count: 1,
                metric_revision: binding.revisions.metric,
                estimator_revision: binding.revisions.estimator,
                project_breakdown_revision: binding.revisions.project_breakdown,
                api_pricing_catalog_revision: binding.revisions.api_pricing_catalog,
                model_groups: Vec::new(),
                project_groups: Vec::new(),
                partial_reasons: Vec::new(),
            })),
        }
    }

    fn one_change_page(generation: u64, sequence: u64, has_more: bool) -> DeltaPage {
        DeltaPage {
            generation: nonzero64(generation),
            from_sequence: sequence,
            through_sequence: sequence,
            next_delta_cursor: DeltaCursor {
                generation: nonzero64(generation),
                sequence,
            },
            has_more,
        }
    }

    fn with_history_writer<T>(
        root: &Path,
        profile: &str,
        redaction_profile: RedactionProfile,
        action: impl FnOnce(&SourceHistoryStore, &SourceHistoryWriter<'_, '_, '_>) -> T,
    ) -> T {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let history = SourceHistoryStore::new(
            root.to_path_buf(),
            profile.parse::<HistoryProfileId>().unwrap(),
        );
        let ownership = HistoryOwnershipStore::new(
            root.to_path_buf(),
            profile.parse().unwrap(),
            redaction_profile,
        );
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
        action(&history, &writer)
    }

    fn loaded_bucket_totals(
        history: &SourceHistoryStore,
        binding: &RemoteDeltaIngestBinding,
    ) -> Vec<u64> {
        history
            .load_source_records_since(
                &binding.source.node_id,
                binding.redaction_profile,
                at(29, 0, 0),
            )
            .unwrap()
            .records
            .into_iter()
            .filter_map(|record| match record.change() {
                SourceBucketChange::Upsert(bucket) => Some(bucket.token_usage.total_tokens),
                SourceBucketChange::Tombstone => None,
            })
            .collect()
    }

    fn store(root: &Path, binding: RemoteDeltaIngestBinding) -> RemoteDeltaIngestStateStore {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        RemoteDeltaIngestStateStore::new(
            SourceHistoryStore::new(root.to_path_buf(), PROFILE.parse().unwrap()),
            binding,
        )
        .unwrap()
    }

    #[test]
    fn complete_binding_has_a_stable_isolated_namespace() {
        let root = tempdir().unwrap();
        let base = binding_with(1, 1, 60);
        let same = store(root.path(), base.clone());
        let generation_changed = store(root.path(), binding_with(2, 1, 60));
        let revision_changed = store(root.path(), binding_with(1, 2, 60));
        let policy_changed = store(root.path(), binding_with(1, 1, 120));

        assert_eq!(
            same.namespace_directory(),
            store(root.path(), base).namespace_directory()
        );
        let paths = [
            same.namespace_directory(),
            generation_changed.namespace_directory(),
            revision_changed.namespace_directory(),
            policy_changed.namespace_directory(),
        ];
        for (index, path) in paths.iter().enumerate() {
            assert!(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(BINDING_NAMESPACE_PREFIX)
            );
            assert!(paths[..index].iter().all(|other| other != path));
        }

        // Bindings have separate durable state, while one source-wide lock
        // still fences delayed work from another generation/revision.
        let held = same.try_begin().unwrap();
        assert_eq!(
            generation_changed.try_begin().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(held);

        // Each binding can initialize independently instead of colliding with
        // another binding's immutable anchor.
        generation_changed.try_begin().unwrap();
        revision_changed.try_begin().unwrap();
        policy_changed.try_begin().unwrap();
    }

    #[test]
    fn source_purge_removes_the_selected_ingest_namespace_idempotently() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        drop(ingest.try_begin().unwrap());
        let source_namespace = ingest.source_namespace_directory();
        assert!(source_namespace.is_dir());

        with_history_writer(
            root.path(),
            PROFILE,
            RedactionProfile::Redacted,
            |history, writer| {
                let first = purge_remote_ingest_state_for_source(
                    history,
                    &binding.source().node_id,
                    writer,
                )
                .unwrap();
                assert_eq!(first.namespaces_removed, 1);
                assert!(!source_namespace.exists());
                let replay = purge_remote_ingest_state_for_source(
                    history,
                    &binding.source().node_id,
                    writer,
                )
                .unwrap();
                assert_eq!(replay.namespaces_removed, 0);
            },
        );
    }

    #[test]
    fn source_purge_refuses_unknown_ingest_layout_without_removing_it() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        drop(ingest.try_begin().unwrap());
        let unexpected = ingest.source_namespace_directory().join("unexpected");
        fs::write(&unexpected, b"keep").unwrap();

        with_history_writer(
            root.path(),
            PROFILE,
            RedactionProfile::Redacted,
            |history, writer| {
                let error = purge_remote_ingest_state_for_source(
                    history,
                    &binding.source().node_id,
                    writer,
                )
                .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            },
        );
        assert!(unexpected.is_file());
    }

    #[test]
    fn active_replacement_generation_is_stable_and_page_bound() {
        let first = PreparedRemoteDeltaPageId(format!(
            "{PREPARED_PAGE_PREFIX}{}",
            "1".repeat(SHA256_HEX_LEN)
        ));
        let second = PreparedRemoteDeltaPageId(format!(
            "{PREPARED_PAGE_PREFIX}{}",
            "2".repeat(SHA256_HEX_LEN)
        ));
        let generation = active_replacement_generation(&first).unwrap();
        assert_eq!(generation, active_replacement_generation(&first).unwrap());
        assert_ne!(generation, active_replacement_generation(&second).unwrap());
        assert!(generation.as_str().starts_with(INGEST_GENERATION_PREFIX));
        assert_eq!(
            generation.as_str().len(),
            INGEST_GENERATION_PREFIX.len() + INGEST_GENERATION_RANDOM_BYTES * 2
        );
    }

    #[test]
    fn bootstrap_is_initialized_before_transport_without_retaining_local_locks() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |_history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let mut initialization = ingest.try_begin().unwrap();
                assert_eq!(
                    initialization.next_request_position().unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
                initialization.start_bootstrap(writer).unwrap();
                assert_eq!(
                    initialization.next_request_position().unwrap(),
                    RemoteDeltaNextRequestPosition {
                        delta_cursor: None,
                        exact_range: None,
                        known_live_revision: None,
                    }
                );
                drop(initialization);
            },
        );

        // This is the transport phase: both the ownership writer lease and
        // the ingest lock from initialization have been released. Acquiring
        // them again in the canonical order proves neither leaked into SSH.
        let ownership = HistoryOwnershipStore::new(
            root.path().to_path_buf(),
            PROFILE.parse().unwrap(),
            binding.redaction_profile,
        );
        let _lease = ownership.acquire_writer_lease().unwrap();
        let concurrent = ingest.try_begin().unwrap();
        assert_eq!(
            concurrent.next_request_position().unwrap(),
            RemoteDeltaNextRequestPosition {
                delta_cursor: None,
                exact_range: None,
                known_live_revision: None,
            }
        );
    }

    #[test]
    fn live_baseline_drives_requests_and_historical_uncertainty_does_not_poison_live_quality() {
        let root = tempdir().unwrap();
        let binding = live_binding_with(1, 1);
        let ingest = store(root.path(), binding.clone());
        let range = export_range(at(30, 3, 0), 60);
        let captured_at = at(30, 3, 59);
        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let mut initialized = ingest.try_begin().unwrap();
                initialized.start_bootstrap(writer).unwrap();
                drop(initialized);

                let initial_request = request(&binding, None, range.clone());
                let mut initial_payload = payload_for(range.clone(), Vec::new(), Vec::new());
                initial_payload.coverage.range_complete = false;
                initial_payload.coverage.partial_reasons =
                    vec!["historical_coverage_unproven".to_owned()];
                initial_payload.live = Some(RemoteLiveState {
                    live_revision: nonzero64(1),
                    snapshot: Some(RemoteLiveSnapshot {
                        captured_at,
                        tasks: Vec::new(),
                        turns: Vec::new(),
                    }),
                });
                let initial_response =
                    response(&binding, range.clone(), empty_page(9, 0), initial_payload);
                let mut initial = ingest.try_begin().unwrap();
                let page = initial
                    .prepare_page(
                        &initial_request,
                        &initial_response,
                        initial_response.observed_at,
                    )
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut initial, writer, &page, at(30, 4, 2))
                    .unwrap();
                activate_remote_delta_bootstrap(&mut initial, writer, at(30, 4, 2))
                    .unwrap()
                    .unwrap();
                let position = initial.next_request_position().unwrap();
                assert_eq!(position.known_live_revision, Some(nonzero64(1)));
                drop(initial);

                let stored = history
                    .load_remote_live_state(&binding.source.node_id)
                    .unwrap()
                    .unwrap();
                assert!(stored.range_complete);
                assert!(stored.partial_reasons.is_empty());
                assert!(stored.warning_codes.is_empty());

                let mut unchanged_request = request(&binding, position.delta_cursor, range.clone());
                let RemoteExportRequestBody::Delta(delta) = &mut unchanged_request.request else {
                    unreachable!();
                };
                delta.known_live_revision = Some(nonzero64(1));
                let mut unchanged_payload = payload_for(range.clone(), Vec::new(), Vec::new());
                unchanged_payload.coverage.range_complete = false;
                unchanged_payload.coverage.partial_reasons =
                    vec!["historical_coverage_unproven".to_owned()];
                unchanged_payload.live = Some(RemoteLiveState {
                    live_revision: nonzero64(1),
                    snapshot: None,
                });
                let unchanged_response =
                    response(&binding, range.clone(), empty_page(9, 0), unchanged_payload);
                let mut unchanged = ingest.try_begin().unwrap();
                let page = unchanged
                    .prepare_page(
                        &unchanged_request,
                        &unchanged_response,
                        unchanged_response.observed_at,
                    )
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut unchanged, writer, &page, at(30, 4, 3))
                    .unwrap();
                drop(unchanged);

                std::fs::remove_file(
                    history
                        .source_directory(&binding.source.node_id)
                        .join(binding.redaction_profile.directory_name())
                        .join("remote-live.json"),
                )
                .unwrap();
                let missing = ingest.try_begin().unwrap();
                let missing_position = missing.next_request_position().unwrap();
                assert_eq!(missing_position.delta_cursor, position.delta_cursor);
                assert_eq!(missing_position.known_live_revision, None);
                drop(missing);

                let restored_request =
                    request(&binding, missing_position.delta_cursor, range.clone());
                let mut restored_payload = payload_for(range.clone(), Vec::new(), Vec::new());
                restored_payload.coverage.range_complete = false;
                restored_payload.coverage.partial_reasons = vec![
                    "historical_coverage_unproven".to_owned(),
                    "live_snapshot_truncated".to_owned(),
                ];
                restored_payload.warnings = vec![RemoteDeltaWarning {
                    code: "live_snapshot_truncated".to_owned(),
                    occurrences: nonzero64(2),
                }];
                restored_payload.live = Some(RemoteLiveState {
                    live_revision: nonzero64(1),
                    snapshot: Some(RemoteLiveSnapshot {
                        captured_at,
                        tasks: Vec::new(),
                        turns: Vec::new(),
                    }),
                });
                let restored_response =
                    response(&binding, range, empty_page(9, 0), restored_payload);
                let mut restored = ingest.try_begin().unwrap();
                let page = restored
                    .prepare_page(
                        &restored_request,
                        &restored_response,
                        restored_response.observed_at,
                    )
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut restored, writer, &page, at(30, 4, 4))
                    .unwrap();
                assert_eq!(
                    restored
                        .next_request_position()
                        .unwrap()
                        .known_live_revision,
                    Some(nonzero64(1))
                );
                let stored = history
                    .load_remote_live_state(&binding.source.node_id)
                    .unwrap()
                    .unwrap();
                assert!(!stored.range_complete);
                assert_eq!(stored.partial_reasons, vec!["live_snapshot_truncated"]);
                assert_eq!(stored.warning_codes, vec!["live_snapshot_truncated"]);
            },
        );
    }

    #[test]
    fn multipage_bootstrap_is_invisible_until_activation_and_replays_manifest_first() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let range = export_range(at(30, 1, 0), 60);

                let mut initialized = ingest.try_begin().unwrap();
                initialized.start_bootstrap(writer).unwrap();
                drop(initialized);

                let first_request = request(&binding, None, range.clone());
                let first_response = response(
                    &binding,
                    range.clone(),
                    one_change_page(9, 1, true),
                    payload_for(
                        range.clone(),
                        vec![bucket_change(&binding, 1, at(30, 1, 0), 10)],
                        Vec::new(),
                    ),
                );
                let mut first = ingest.try_begin().unwrap();
                let first_page = first
                    .prepare_page(&first_request, &first_response, first_response.observed_at)
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut first, writer, &first_page, at(30, 4, 1))
                    .unwrap();
                drop(first);
                assert_eq!(loaded_bucket_totals(history, &binding), Vec::<u64>::new());

                let second_request = request(
                    &binding,
                    Some(DeltaCursor {
                        generation: nonzero64(9),
                        sequence: 1,
                    }),
                    range.clone(),
                );
                let second_response = response(
                    &binding,
                    range.clone(),
                    one_change_page(9, 2, false),
                    payload_for(
                        range.clone(),
                        vec![bucket_change(&binding, 2, at(30, 1, 15), 20)],
                        Vec::new(),
                    ),
                );
                let mut second = ingest.try_begin().unwrap();
                let second_page = second
                    .prepare_page(
                        &second_request,
                        &second_response,
                        second_response.observed_at,
                    )
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut second, writer, &second_page, at(30, 4, 2))
                    .unwrap();
                let first_activation =
                    activate_remote_delta_bootstrap(&mut second, writer, at(30, 4, 2))
                        .unwrap()
                        .unwrap();
                assert_eq!(
                    first_activation.cleanup,
                    RemoteGenerationCleanup::NotRequired
                );
                let first_active_generation = first_activation.activation.generation.clone();
                let first_active_directory = history.source_remote_history_generation_directory(
                    &binding.source.node_id,
                    binding.redaction_profile,
                    &source_history_generation(&first_active_generation).unwrap(),
                );
                drop(second);
                assert_eq!(loaded_bucket_totals(history, &binding), vec![10, 20]);

                let mut replacement = ingest.try_begin().unwrap();
                assert_eq!(
                    replacement.status().active_generation,
                    Some(first_active_generation)
                );
                let replacement_generation = replacement.start_bootstrap(writer).unwrap();
                drop(replacement);

                let replacement_first_request = request(&binding, None, range.clone());
                let replacement_first_response = response(
                    &binding,
                    range.clone(),
                    one_change_page(10, 1, true),
                    payload_for(
                        range.clone(),
                        vec![bucket_change(&binding, 1, at(30, 1, 0), 30)],
                        Vec::new(),
                    ),
                );
                let mut replacement_first = ingest.try_begin().unwrap();
                let replacement_first_page = replacement_first
                    .prepare_page(
                        &replacement_first_request,
                        &replacement_first_response,
                        replacement_first_response.observed_at,
                    )
                    .unwrap();
                assert_eq!(
                    replacement_first_page.target,
                    RemoteDeltaApplyTarget::Staging(replacement_generation.clone())
                );
                apply_and_commit_remote_delta_page(
                    &mut replacement_first,
                    writer,
                    &replacement_first_page,
                    at(30, 4, 3),
                )
                .unwrap();
                drop(replacement_first);
                assert_eq!(loaded_bucket_totals(history, &binding), vec![10, 20]);

                let replacement_second_request = request(
                    &binding,
                    Some(DeltaCursor {
                        generation: nonzero64(10),
                        sequence: 1,
                    }),
                    range.clone(),
                );
                let replacement_second_response = response(
                    &binding,
                    range.clone(),
                    one_change_page(10, 2, false),
                    payload_for(
                        range,
                        vec![bucket_change(&binding, 2, at(30, 1, 15), 40)],
                        Vec::new(),
                    ),
                );
                let mut replacement_second = ingest.try_begin().unwrap();
                let replacement_second_page = replacement_second
                    .prepare_page(
                        &replacement_second_request,
                        &replacement_second_response,
                        replacement_second_response.observed_at,
                    )
                    .unwrap();
                apply_and_commit_remote_delta_page(
                    &mut replacement_second,
                    writer,
                    &replacement_second_page,
                    at(30, 4, 4),
                )
                .unwrap();

                // Simulate a process exit after the atomic history switch but
                // before the ingest cursor commit.
                let ready = replacement_second
                    .bootstrap_activation_required()
                    .unwrap()
                    .unwrap();
                let history_generation = source_history_generation(&ready.generation).unwrap();
                let expected_active = replacement_second
                    .state
                    .bootstrap
                    .as_ref()
                    .unwrap()
                    .expected_active
                    .as_ref()
                    .map(source_history_active_ref)
                    .transpose()
                    .unwrap();
                let candidate_binding = source_history_binding(&binding).unwrap();
                writer
                    .activate_remote_history_generation(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        expected_active.as_ref(),
                        &history_generation,
                        &candidate_binding,
                        at(30, 4, 3),
                    )
                    .unwrap();
                drop(replacement_second);
                assert_eq!(loaded_bucket_totals(history, &binding), vec![30, 40]);

                let mut recovered = ingest.try_begin().unwrap();
                assert!(recovered.status().activation_required);
                let replayed =
                    activate_remote_delta_bootstrap(&mut recovered, writer, at(30, 4, 4))
                        .unwrap()
                        .unwrap();
                assert_eq!(replayed.activation, ready);
                assert_eq!(
                    replayed.cleanup,
                    RemoteGenerationCleanup::Completed(RemoteHistoryGenerationGcOutcome::Deleted)
                );
                assert!(!first_active_directory.exists());
                assert_eq!(recovered.status().active_generation, Some(ready.generation));
                assert_eq!(recovered.status().active_cursor, Some(ready.cursor));
                assert!(!recovered.status().activation_required);
                assert_eq!(loaded_bucket_totals(history, &binding), vec![30, 40]);
            },
        );
    }

    #[test]
    fn bootstrap_cas_rejects_stale_manifest_and_old_binding_rollback() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let range = export_range(at(30, 1, 0), 60);

                let mut initial = ingest.try_begin().unwrap();
                initial.start_bootstrap(writer).unwrap();
                let initial_request = request(&binding, None, range.clone());
                let initial_response = response(
                    &binding,
                    range.clone(),
                    empty_page(9, 0),
                    payload_for(range.clone(), Vec::new(), Vec::new()),
                );
                let initial_page = initial
                    .prepare_page(
                        &initial_request,
                        &initial_response,
                        initial_response.observed_at,
                    )
                    .unwrap();
                apply_and_commit_remote_delta_page(
                    &mut initial,
                    writer,
                    &initial_page,
                    at(30, 4, 0),
                )
                .unwrap();
                activate_remote_delta_bootstrap(&mut initial, writer, at(30, 4, 1))
                    .unwrap()
                    .unwrap();
                drop(initial);

                let expected_a = history
                    .active_remote_history_ref(&binding.source.node_id, binding.redaction_profile)
                    .unwrap()
                    .unwrap();
                let mut stale = ingest.try_begin().unwrap();
                stale.start_bootstrap(writer).unwrap();
                let stale_request = request(&binding, None, range.clone());
                let stale_response = response(
                    &binding,
                    range.clone(),
                    empty_page(10, 0),
                    payload_for(range.clone(), Vec::new(), Vec::new()),
                );
                let stale_page = stale
                    .prepare_page(&stale_request, &stale_response, stale_response.observed_at)
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut stale, writer, &stale_page, at(30, 4, 2))
                    .unwrap();
                drop(stale);

                let competing_generation = RemoteHistoryGenerationId::generate().unwrap();
                let competing_history_generation =
                    source_history_generation(&competing_generation).unwrap();
                let current_binding = source_history_binding(&binding).unwrap();
                writer
                    .ensure_remote_history_generation(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        &competing_history_generation,
                        &current_binding,
                    )
                    .unwrap();
                writer
                    .activate_remote_history_generation(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        Some(&expected_a),
                        &competing_history_generation,
                        &current_binding,
                        at(30, 4, 3),
                    )
                    .unwrap();

                let mut stale = ingest.try_begin().unwrap();
                let error =
                    activate_remote_delta_bootstrap(&mut stale, writer, at(30, 4, 4)).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                assert!(stale.status().activation_required);
                drop(stale);

                let newer_binding = binding_with(2, 1, 60);
                let newer_history_binding = source_history_binding(&newer_binding).unwrap();
                let current = history
                    .active_remote_history_ref(&binding.source.node_id, binding.redaction_profile)
                    .unwrap()
                    .unwrap();
                let newer_generation = RemoteHistoryGenerationId::generate().unwrap();
                let newer_history_generation =
                    source_history_generation(&newer_generation).unwrap();
                writer
                    .ensure_remote_history_generation(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        &newer_history_generation,
                        &newer_history_binding,
                    )
                    .unwrap();
                writer
                    .activate_remote_history_generation(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        Some(&current),
                        &newer_history_generation,
                        &newer_history_binding,
                        at(30, 4, 5),
                    )
                    .unwrap();

                // A separate old-binding namespace snapshots the newer active
                // ref correctly, but its candidate still cannot roll back the
                // source generation during CAS activation.
                let old_policy_binding = binding_with(1, 1, 120);
                let old_ingest = store(root.path(), old_policy_binding.clone());
                let old_range = export_range(at(30, 1, 0), 120);
                let mut old = old_ingest.try_begin().unwrap();
                old.start_bootstrap(writer).unwrap();
                let old_request = request(&old_policy_binding, None, old_range.clone());
                let old_response = response(
                    &old_policy_binding,
                    old_range.clone(),
                    empty_page(11, 0),
                    payload_for(old_range, Vec::new(), Vec::new()),
                );
                let old_page = old
                    .prepare_page(&old_request, &old_response, old_response.observed_at)
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut old, writer, &old_page, at(30, 4, 6))
                    .unwrap();
                let rollback =
                    activate_remote_delta_bootstrap(&mut old, writer, at(30, 4, 7)).unwrap_err();
                assert_eq!(rollback.kind(), io::ErrorKind::InvalidData);
                assert!(rollback.to_string().contains("roll back"));
                assert!(old.status().activation_required);
                assert_eq!(
                    history
                        .active_remote_history_ref(
                            &binding.source.node_id,
                            binding.redaction_profile,
                        )
                        .unwrap()
                        .unwrap()
                        .binding()
                        .source(),
                    newer_history_binding.source()
                );
            },
        );
    }

    #[test]
    fn bridge_rejects_tampered_pages_and_cross_namespace_writers() {
        let root = tempdir().unwrap();
        let other_root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        let range = export_range(at(30, 1, 0), 60);
        let request = request(&binding, None, range.clone());
        let response = response(
            &binding,
            range.clone(),
            one_change_page(9, 1, false),
            payload_for(
                range,
                vec![bucket_change(&binding, 1, at(30, 1, 0), 10)],
                Vec::new(),
            ),
        );
        let page = with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |_history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let mut session = ingest.try_begin().unwrap();
                session.start_bootstrap(writer).unwrap();
                let page = session
                    .prepare_page(&request, &response, response.observed_at)
                    .unwrap();
                let mut tampered = page.clone();
                tampered.target =
                    RemoteDeltaApplyTarget::Staging(RemoteHistoryGenerationId::generate().unwrap());
                let error = apply_remote_delta_records(&session, writer, &tampered, at(30, 4, 0))
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                assert!(error.to_string().contains("pending WAL"));
                page
            },
        );

        with_history_writer(
            other_root.path(),
            PROFILE,
            binding.redaction_profile,
            |_history, writer| {
                let session = ingest.try_begin().unwrap();
                let error =
                    apply_remote_delta_records(&session, writer, &page, at(30, 4, 0)).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                assert!(error.to_string().contains("state root and profile"));
            },
        );
        with_history_writer(
            root.path(),
            OTHER_PROFILE,
            binding.redaction_profile,
            |_history, writer| {
                let session = ingest.try_begin().unwrap();
                let error =
                    apply_remote_delta_records(&session, writer, &page, at(30, 4, 0)).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                assert!(error.to_string().contains("state root and profile"));
            },
        );

        assert_eq!(page.next_cursor.sequence, 1);
    }

    #[test]
    fn pending_wal_replays_out_of_range_global_transition_before_cursor_commit() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let history = SourceHistoryStore::new(root.path().to_path_buf(), PROFILE.parse().unwrap());
        let ingest = store(root.path(), binding.clone());
        let initial_range = export_range(at(30, 1, 0), 60);
        let initial_request = request(&binding, None, initial_range.clone());
        let initial_response = response(
            &binding,
            initial_range.clone(),
            empty_page(9, 0),
            payload_for(initial_range, Vec::new(), Vec::new()),
        );

        let ownership = HistoryOwnershipStore::new(
            root.path().to_path_buf(),
            PROFILE.parse().unwrap(),
            RedactionProfile::Redacted,
        );
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
                &SourceMetadata::new(binding.source.node_id.clone(), SourceKind::Ssh, "remote")
                    .unwrap(),
            )
            .unwrap();

        let mut session = ingest.try_begin().unwrap();
        session.start_bootstrap(&writer).unwrap();
        let bootstrap = session
            .prepare_page(
                &initial_request,
                &initial_response,
                initial_response.observed_at,
            )
            .unwrap();
        assert!(matches!(
            bootstrap.target,
            RemoteDeltaApplyTarget::Staging(_)
        ));
        apply_and_commit_remote_delta_page(&mut session, &writer, &bootstrap, at(30, 4, 1))
            .unwrap();
        let activation = activate_remote_delta_bootstrap(&mut session, &writer, at(30, 4, 2))
            .unwrap()
            .unwrap();
        assert_eq!(activation.cleanup, RemoteGenerationCleanup::NotRequired);
        let initial_active_generation = session.status().active_generation.unwrap();
        let initial_active_directory = history.source_remote_history_generation_directory(
            &binding.source.node_id,
            binding.redaction_profile,
            &source_history_generation(&initial_active_generation).unwrap(),
        );
        drop(session);

        let next_range = export_range(at(30, 2, 0), 60);
        let next_request = request(
            &binding,
            Some(DeltaCursor {
                generation: nonzero64(9),
                sequence: 0,
            }),
            next_range.clone(),
        );
        // This journal transition is intentionally outside the coverage range.
        // A global cursor must still ingest it before advancing.
        let outside_range = at(29, 0, 0);
        let (page, changes) = tombstone_page(9, 1, outside_range);
        let next_response = response(
            &binding,
            next_range.clone(),
            page,
            payload_for(next_range, changes, Vec::new()),
        );

        let mut session = ingest.try_begin().unwrap();
        let prepared = session
            .prepare_page(&next_request, &next_response, next_response.observed_at)
            .unwrap();
        assert!(matches!(
            prepared.target,
            RemoteDeltaApplyTarget::ActiveCow { .. }
        ));
        let RemoteDeltaApplyTarget::ActiveCow {
            expected_generation,
            replacement_generation,
        } = &prepared.target
        else {
            unreachable!();
        };
        assert_eq!(expected_generation, &initial_active_generation);
        assert_eq!(
            replacement_generation,
            &active_replacement_generation(&prepared.id).unwrap()
        );
        apply_remote_delta_records(&session, &writer, &prepared, at(30, 4, 3)).unwrap();
        assert_eq!(
            history
                .active_remote_history_ref(&binding.source.node_id, binding.redaction_profile)
                .unwrap()
                .unwrap()
                .generation()
                .as_str(),
            replacement_generation.as_str()
        );
        assert_eq!(
            session.status().active_generation,
            Some(initial_active_generation.clone())
        );
        assert!(initial_active_directory.exists());
        drop(session); // Crash after history apply but before cursor commit.

        let mut recovered = ingest.try_begin().unwrap();
        let replay = recovered.pending_page().unwrap().unwrap();
        assert_eq!(replay, prepared);
        let unknown_namespace = ingest.source_namespace_directory().join("unexpected");
        history
            .prepare_private_directory(&unknown_namespace)
            .unwrap();
        let replay_report =
            apply_and_commit_remote_delta_page(&mut recovered, &writer, &replay, at(30, 4, 4))
                .unwrap();
        assert!(matches!(
            replay_report.cleanup,
            RemoteGenerationCleanup::Deferred {
                error_kind: io::ErrorKind::InvalidData,
                ..
            }
        ));
        assert!(initial_active_directory.exists());
        assert_eq!(recovered.status().active_cursor.unwrap().sequence, 1);
        assert_eq!(
            recovered.status().active_generation,
            Some(replacement_generation.clone())
        );
        assert_eq!(recovered.status().deferred_cleanup_count, 1);
        drop(recovered);

        fs::remove_dir(unknown_namespace).unwrap();
        let mut cleanup = ingest.try_begin().unwrap();
        assert_eq!(
            retry_deferred_remote_generation_cleanup(&mut cleanup, &writer).unwrap(),
            vec![RemoteGenerationCleanup::Completed(
                RemoteHistoryGenerationGcOutcome::Deleted
            )]
        );
        assert_eq!(cleanup.status().deferred_cleanup_count, 0);
        assert!(!initial_active_directory.exists());
        drop(cleanup);

        let second_range = export_range(at(30, 3, 0), 60);
        let second_request = request(
            &binding,
            Some(DeltaCursor {
                generation: nonzero64(9),
                sequence: 1,
            }),
            second_range.clone(),
        );
        let (second_delta_page, second_changes) = tombstone_page(9, 2, at(29, 0, 15));
        let second_response = response(
            &binding,
            second_range.clone(),
            second_delta_page,
            payload_for(second_range, second_changes, Vec::new()),
        );
        let mut second = ingest.try_begin().unwrap();
        let second_prepared = second
            .prepare_page(
                &second_request,
                &second_response,
                second_response.observed_at,
            )
            .unwrap();
        let RemoteDeltaApplyTarget::ActiveCow {
            expected_generation: second_expected,
            replacement_generation: second_replacement,
        } = &second_prepared.target
        else {
            unreachable!();
        };
        assert_eq!(second_expected, replacement_generation);
        assert_ne!(second_replacement, replacement_generation);
        let second_report = apply_and_commit_remote_delta_page(
            &mut second,
            &writer,
            &second_prepared,
            at(30, 4, 5),
        )
        .unwrap();
        assert_eq!(
            second_report.cleanup,
            RemoteGenerationCleanup::Completed(RemoteHistoryGenerationGcOutcome::Deleted)
        );
        assert_eq!(second.status().active_cursor.unwrap().sequence, 2);
        assert_eq!(
            second.status().active_generation,
            Some(second_replacement.clone())
        );

        let records = history
            .load_source_records_since(
                &binding.source.node_id,
                binding.redaction_profile,
                outside_range,
            )
            .unwrap()
            .records;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].starts_at(), outside_range);
        assert_eq!(records[0].revision(), 1);

        let stable_generation = second_replacement.clone();
        let generations_directory = history
            .source_remote_history_generation_directory(
                &binding.source.node_id,
                binding.redaction_profile,
                &source_history_generation(&stable_generation).unwrap(),
            )
            .parent()
            .unwrap()
            .to_path_buf();
        let generation_count_before = fs::read_dir(&generations_directory).unwrap().count();
        drop(second);

        let empty_range = export_range(at(30, 4, 0), 60);
        let empty_request = request(
            &binding,
            Some(DeltaCursor {
                generation: nonzero64(9),
                sequence: 2,
            }),
            empty_range.clone(),
        );
        let empty_response = response(
            &binding,
            empty_range.clone(),
            empty_page(9, 2),
            payload_for(empty_range, Vec::new(), Vec::new()),
        );
        let mut empty = ingest.try_begin().unwrap();
        let empty_prepared = empty
            .prepare_page(&empty_request, &empty_response, empty_response.observed_at)
            .unwrap();
        assert_eq!(
            empty_prepared.target,
            RemoteDeltaApplyTarget::ActiveNoop {
                expected_generation: stable_generation.clone(),
            }
        );
        apply_remote_delta_records(&empty, &writer, &empty_prepared, at(30, 4, 6)).unwrap();
        assert_eq!(
            history
                .active_remote_history_ref(&binding.source.node_id, binding.redaction_profile)
                .unwrap()
                .unwrap()
                .generation(),
            &source_history_generation(&stable_generation).unwrap()
        );
        assert_eq!(
            fs::read_dir(&generations_directory).unwrap().count(),
            generation_count_before
        );
        drop(empty); // Crash after the no-op active-ref check, before cursor/WAL commit.

        let mut empty_replay = ingest.try_begin().unwrap();
        let replay = empty_replay.pending_page().unwrap().unwrap();
        assert_eq!(replay, empty_prepared);
        apply_and_commit_remote_delta_page(&mut empty_replay, &writer, &replay, at(30, 4, 7))
            .unwrap();
        assert_eq!(
            empty_replay.status().active_generation,
            Some(stable_generation)
        );
        assert_eq!(empty_replay.status().active_cursor.unwrap().sequence, 2);
        assert!(empty_replay.status().pending_page.is_none());
        assert_eq!(
            fs::read_dir(generations_directory).unwrap().count(),
            generation_count_before
        );
    }

    #[test]
    fn repeated_active_cow_pages_stay_below_generation_capacity() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        let range = export_range(at(30, 1, 0), 60);

        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();

                let mut initial = ingest.try_begin().unwrap();
                initial.start_bootstrap(writer).unwrap();
                let initial_request = request(&binding, None, range.clone());
                let initial_response = response(
                    &binding,
                    range.clone(),
                    empty_page(9, 0),
                    payload_for(range.clone(), Vec::new(), Vec::new()),
                );
                let initial_page = initial
                    .prepare_page(
                        &initial_request,
                        &initial_response,
                        initial_response.observed_at,
                    )
                    .unwrap();
                apply_and_commit_remote_delta_page(
                    &mut initial,
                    writer,
                    &initial_page,
                    at(30, 4, 0),
                )
                .unwrap();
                activate_remote_delta_bootstrap(&mut initial, writer, at(30, 4, 0))
                    .unwrap()
                    .unwrap();
                drop(initial);

                for sequence in 1..=40_u64 {
                    let request = request(
                        &binding,
                        Some(DeltaCursor {
                            generation: nonzero64(9),
                            sequence: sequence - 1,
                        }),
                        range.clone(),
                    );
                    let response = response(
                        &binding,
                        range.clone(),
                        one_change_page(9, sequence, false),
                        payload_for(
                            range.clone(),
                            vec![bucket_change(&binding, sequence, at(30, 1, 0), sequence)],
                            Vec::new(),
                        ),
                    );
                    let mut session = ingest.try_begin().unwrap();
                    let page = session
                        .prepare_page(&request, &response, response.observed_at)
                        .unwrap();
                    let report = apply_and_commit_remote_delta_page(
                        &mut session,
                        writer,
                        &page,
                        at(30, 4, 1),
                    )
                    .unwrap();
                    assert_eq!(
                        report.cleanup,
                        RemoteGenerationCleanup::Completed(
                            RemoteHistoryGenerationGcOutcome::Deleted
                        )
                    );
                    assert_eq!(session.status().deferred_cleanup_count, 0);
                }

                let active = history
                    .active_remote_history_ref(&binding.source.node_id, binding.redaction_profile)
                    .unwrap()
                    .unwrap();
                let generations = history
                    .source_remote_history_generation_directory(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        active.generation(),
                    )
                    .parent()
                    .unwrap()
                    .to_path_buf();
                assert_eq!(fs::read_dir(generations).unwrap().count(), 1);
            },
        );
    }

    #[test]
    fn has_more_continuation_restores_its_exact_range_after_reopen() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        let range = export_range(at(30, 1, 0), 60);
        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let mut initial = ingest.try_begin().unwrap();
                initial.start_bootstrap(writer).unwrap();
                assert_eq!(
                    initial.next_request_position().unwrap(),
                    RemoteDeltaNextRequestPosition {
                        delta_cursor: None,
                        exact_range: None,
                        known_live_revision: None,
                    }
                );
                drop(initial);

                let first_request = request(&binding, None, range.clone());
                let (mut first_page, changes) = tombstone_page(9, 1, at(29, 0, 0));
                first_page.has_more = true;
                let first_response = response(
                    &binding,
                    range.clone(),
                    first_page,
                    payload_for(range.clone(), changes, Vec::new()),
                );
                let mut first = ingest.try_begin().unwrap();
                let prepared = first
                    .prepare_page(&first_request, &first_response, first_response.observed_at)
                    .unwrap();
                assert_eq!(
                    first.next_request_position().unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
                apply_and_commit_remote_delta_page(&mut first, writer, &prepared, at(30, 4, 1))
                    .unwrap();
                drop(first);

                let mut resumed = ingest.try_begin().unwrap();
                let position = resumed.next_request_position().unwrap();
                assert_eq!(
                    position,
                    RemoteDeltaNextRequestPosition {
                        delta_cursor: Some(DeltaCursor {
                            generation: nonzero64(9),
                            sequence: 1,
                        }),
                        exact_range: Some(range.clone()),
                        known_live_revision: None,
                    }
                );
                let moved_range = export_range(at(30, 2, 0), 60);
                let moved_request = request(&binding, position.delta_cursor, moved_range.clone());
                let moved_response = response(
                    &binding,
                    moved_range.clone(),
                    empty_page(9, 1),
                    payload_for(moved_range, Vec::new(), Vec::new()),
                );
                let error = resumed
                    .prepare_page(&moved_request, &moved_response, moved_response.observed_at)
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(error.to_string().contains("exact export range"));

                let final_request = request(&binding, position.delta_cursor, range.clone());
                let final_response = response(
                    &binding,
                    range.clone(),
                    empty_page(9, 1),
                    payload_for(range.clone(), Vec::new(), Vec::new()),
                );
                let final_page = resumed
                    .prepare_page(&final_request, &final_response, final_response.observed_at)
                    .unwrap();
                apply_and_commit_remote_delta_page(&mut resumed, writer, &final_page, at(30, 4, 2))
                    .unwrap();
                assert!(resumed.bootstrap_activation_required().unwrap().is_some());
                let expired_generation = resumed.status().bootstrap_generation.unwrap();
                let expired_directory = history.source_remote_history_generation_directory(
                    &binding.source.node_id,
                    binding.redaction_profile,
                    &source_history_generation(&expired_generation).unwrap(),
                );
                assert!(expired_directory.exists());
                let replacement = resumed
                    .restart_bootstrap_after_cursor_expiry(writer)
                    .unwrap();
                assert_ne!(replacement, expired_generation);
                assert!(!expired_directory.exists());
                assert_eq!(resumed.status().deferred_cleanup_count, 0);
                assert_eq!(
                    resumed.next_request_position().unwrap(),
                    RemoteDeltaNextRequestPosition {
                        delta_cursor: None,
                        exact_range: None,
                        known_live_revision: None,
                    }
                );
                assert!(resumed.bootstrap_activation_required().unwrap().is_none());
            },
        );
    }

    #[test]
    fn remote_digest_retention_horizon_is_preserved() {
        let source = source(1);
        let range_start = at(1, 0, 0);
        let range_end = at(1, 1, 0);
        let retention_through = at(30, 0, 0);
        let digest = RemoteSessionDigest {
            thread_id: "thread-retention".parse().unwrap(),
            range_start,
            range_end,
            covered_through: range_end,
            fingerprint: RemoteSessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                "a".repeat(64)
            ))
            .unwrap(),
            project_breakdown_fingerprint: RemoteSessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                "b".repeat(64)
            ))
            .unwrap(),
            event_count: 0,
            exact_event_identity: true,
            coverage_complete: true,
            observed_project_keys: Vec::new(),
            metrics: RemoteSessionUsageMetrics {
                token_usage: RemoteTokenUsage::default(),
                estimated_cost_units: RemoteU128::new(0),
                api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
                api_equivalent_cost: RemoteApiCostAmount::default(),
                call_count: 0,
                metric_revision: nonzero32(HISTORY_METRIC_REVISION),
                estimator_revision: nonzero32(1),
                project_breakdown_revision: nonzero32(2),
                api_pricing_catalog_revision: nonzero32(6),
                partial_reasons: Vec::new(),
            },
        };
        let upsert = RemoteSessionDigestChange {
            sequence: nonzero64(1),
            thread_id: digest.thread_id.clone(),
            range_start,
            range_end,
            changed_at: range_end,
            retention_through,
            revision: nonzero64(7),
            mutation: RemoteSessionDigestMutation::Upsert(Box::new(digest)),
        };
        let tombstone = RemoteSessionDigestChange {
            sequence: nonzero64(2),
            thread_id: "thread-tombstone".parse().unwrap(),
            range_start,
            range_end,
            changed_at: range_end,
            retention_through,
            revision: nonzero64(8),
            mutation: RemoteSessionDigestMutation::Tombstone,
        };
        let records = history_records_from_payload(
            &source.node_id,
            RedactionProfile::Redacted,
            &payload_for(
                export_range(at(30, 1, 0), 60),
                Vec::new(),
                vec![upsert, tombstone],
            ),
        )
        .unwrap()
        .session_digest_records;

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].retention_through(), retention_through);
        assert_eq!(records[1].retention_through(), retention_through);
        assert!(matches!(
            records[0].change(),
            SourceSessionDigestChange::Upsert(_)
        ));
        assert!(matches!(
            records[1].change(),
            SourceSessionDigestChange::Tombstone
        ));
    }

    #[test]
    fn durable_state_rejects_a_pending_page_detached_from_its_cursor() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        let range = export_range(at(30, 1, 0), 60);
        let request = request(&binding, None, range.clone());
        let response = response(
            &binding,
            range.clone(),
            empty_page(9, 0),
            payload_for(range, Vec::new(), Vec::new()),
        );
        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |_history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let mut session = ingest.try_begin().unwrap();
                session.start_bootstrap(writer).unwrap();
                session
                    .prepare_page(&request, &response, response.observed_at)
                    .unwrap();

                let mut corrupted = session.state.clone();
                let pending = corrupted.pending.as_mut().unwrap();
                let RemoteExportRequestBody::Delta(delta) = &mut pending.request.request else {
                    unreachable!();
                };
                delta.delta_cursor = Some(DeltaCursor {
                    generation: nonzero64(9),
                    sequence: 99,
                });
                let RemoteExportResponseBody::Delta { page, .. } = &mut pending.response.result
                else {
                    unreachable!();
                };
                page.from_sequence = 99;
                page.through_sequence = 99;
                page.next_delta_cursor.sequence = 99;
                pending.id = pending_page_id(pending).unwrap();
                let error = corrupted.validate(&binding).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(error.to_string().contains("durable cursor"));
            },
        );
    }

    #[test]
    fn windows_ingest_lock_share_mode_excludes_delete() {
        const FILE_SHARE_READ_FOR_TEST: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE_FOR_TEST: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE_FOR_TEST: u32 = 0x0000_0004;

        let mode = remote_ingest_lock_share_mode();
        assert_eq!(
            mode & (FILE_SHARE_READ_FOR_TEST | FILE_SHARE_WRITE_FOR_TEST),
            FILE_SHARE_READ_FOR_TEST | FILE_SHARE_WRITE_FOR_TEST
        );
        assert_eq!(mode & FILE_SHARE_DELETE_FOR_TEST, 0);
    }

    #[test]
    fn ingest_create_once_publishes_complete_inode_without_clobbering() {
        let root = tempdir().unwrap();
        let path = root.path().join("anchor.json");
        let first = serde_json::json!({"winner": 1});
        let second = serde_json::json!({"winner": 2});

        create_private_json_once(&path, &first, 1024, "test anchor").unwrap();
        create_private_json_once(&path, &second, 1024, "test anchor").unwrap();

        let loaded: serde_json::Value = read_private_json(&path, 1024, "test anchor").unwrap();
        assert_eq!(loaded, first);
        assert!(
            fs::read_dir(root.path())
                .unwrap()
                .all(|entry| !is_remote_ingest_temporary_name(&entry.unwrap().file_name()))
        );
    }

    #[test]
    fn ingest_session_recovers_only_exact_atomic_temporary_files() {
        let root = tempdir().unwrap();
        let ingest = store(root.path(), binding_with(1, 1, 60));
        ingest.try_begin().unwrap();

        let (temporary_path, mut temporary) =
            create_private_temp(&ingest.namespace_directory(), "orphaned ingest write").unwrap();
        temporary.write_all(b"partial").unwrap();
        temporary.sync_all().unwrap();
        drop(temporary);
        let unknown = ingest
            .namespace_directory()
            .join(".remote-ingest.tmp.not-a-pid.1");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        configure_private_open(&mut options, false);
        options.open(&unknown).unwrap().sync_all().unwrap();

        ingest.try_begin().unwrap();

        assert!(!temporary_path.exists());
        assert!(unknown.exists());
        assert!(!is_remote_ingest_temporary_name(
            unknown.file_name().unwrap()
        ));
    }

    #[test]
    fn generation_gc_scans_every_binding_and_fails_closed_on_unknown_namespaces() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let other_binding = binding_with(1, 1, 120);
        let ingest = store(root.path(), binding.clone());
        let other_ingest = store(root.path(), other_binding.clone());

        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();

                let protected = {
                    let mut other = other_ingest.try_begin().unwrap();
                    other.start_bootstrap(writer).unwrap()
                };
                assert_eq!(
                    garbage_collect_retired_remote_history_generation(&ingest, writer, &protected,)
                        .unwrap(),
                    RemoteHistoryGenerationGcOutcome::SkippedProtected
                );
                assert!(
                    history
                        .source_remote_history_generation_directory(
                            &binding.source.node_id,
                            binding.redaction_profile,
                            &source_history_generation(&protected).unwrap(),
                        )
                        .exists()
                );

                let retired = RemoteHistoryGenerationId::generate().unwrap();
                writer
                    .ensure_remote_history_generation(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        &source_history_generation(&retired).unwrap(),
                        &source_history_binding(&binding).unwrap(),
                    )
                    .unwrap();
                let unknown = ingest.source_namespace_directory().join("unexpected");
                history.prepare_private_directory(&unknown).unwrap();
                let error =
                    garbage_collect_retired_remote_history_generation(&ingest, writer, &retired)
                        .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(error.to_string().contains("binding namespace"));
                assert!(
                    history
                        .source_remote_history_generation_directory(
                            &binding.source.node_id,
                            binding.redaction_profile,
                            &source_history_generation(&retired).unwrap(),
                        )
                        .exists()
                );
                fs::remove_dir(&unknown).unwrap();

                assert_eq!(
                    garbage_collect_retired_remote_history_generation(&ingest, writer, &retired)
                        .unwrap(),
                    RemoteHistoryGenerationGcOutcome::Deleted
                );
                assert!(
                    !history
                        .source_remote_history_generation_directory(
                            &binding.source.node_id,
                            binding.redaction_profile,
                            &source_history_generation(&retired).unwrap(),
                        )
                        .exists()
                );
            },
        );
    }

    #[test]
    fn bounded_sweep_removes_orphans_but_keeps_other_binding_roots() {
        let root = tempdir().unwrap();
        let binding = binding_with(1, 1, 60);
        let ingest = store(root.path(), binding.clone());
        let other_ingest = store(root.path(), binding_with(1, 1, 120));

        with_history_writer(
            root.path(),
            PROFILE,
            binding.redaction_profile,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new(
                            binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                        )
                        .unwrap(),
                    )
                    .unwrap();

                let protected = {
                    let mut other = other_ingest.try_begin().unwrap();
                    other.start_bootstrap(writer).unwrap()
                };
                let orphan = RemoteHistoryGenerationId::generate().unwrap();
                writer
                    .ensure_remote_history_generation(
                        &binding.source.node_id,
                        binding.redaction_profile,
                        &source_history_generation(&orphan).unwrap(),
                        &source_history_binding(&binding).unwrap(),
                    )
                    .unwrap();

                let session = ingest.try_begin().unwrap();
                assert_eq!(
                    sweep_unreferenced_remote_history_generations(&session, writer).unwrap(),
                    RemoteHistoryGenerationSweepReport {
                        deleted: 1,
                        recovered: 0,
                        skipped: 1,
                        remaining: 0,
                    }
                );
                assert!(
                    !history
                        .source_remote_history_generation_directory(
                            &binding.source.node_id,
                            binding.redaction_profile,
                            &source_history_generation(&orphan).unwrap(),
                        )
                        .exists()
                );
                assert!(
                    history
                        .source_remote_history_generation_directory(
                            &binding.source.node_id,
                            binding.redaction_profile,
                            &source_history_generation(&protected).unwrap(),
                        )
                        .exists()
                );
            },
        );
    }

    #[test]
    fn binding_namespace_cap_allows_recovery_but_rejects_new_bindings() {
        let root = tempdir().unwrap();
        let ingest = store(root.path(), binding_with(1, 1, 60));
        ingest.prepare_source_namespace().unwrap();
        ingest
            .history_store
            .prepare_private_directory(&ingest.namespace_directory())
            .unwrap();

        let mut created = 1_usize;
        for index in 0_u64.. {
            if created == MAX_BINDING_NAMESPACES_PER_SOURCE {
                break;
            }
            let name = format!("{BINDING_NAMESPACE_PREFIX}{index:064x}");
            if name == ingest.binding_namespace {
                continue;
            }
            ingest
                .history_store
                .prepare_private_directory(&ingest.source_namespace_directory().join(name))
                .unwrap();
            created += 1;
        }

        ingest.try_begin().unwrap();
        let additional = store(root.path(), binding_with(1, 1, 120));
        let error = additional.try_begin().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(error.to_string().contains("binding namespace limit"));
        assert!(!additional.namespace_directory().exists());
    }

    #[cfg(windows)]
    #[test]
    fn open_windows_ingest_lock_prevents_path_replacement() {
        let root = tempdir().unwrap();
        let ingest = store(root.path(), binding_with(1, 1, 60));
        ingest.prepare_source_namespace().unwrap();
        let lock_path = ingest.lock_path();
        let displaced_path = ingest
            .source_namespace_directory()
            .join("displaced-ingest.lock");
        let opened = open_private_lock(&lock_path).unwrap();

        assert!(fs::rename(&lock_path, &displaced_path).is_err());
        validate_private_file(&lock_path, &opened, "remote ingest lock").unwrap();

        drop(opened);
        fs::rename(lock_path, displaced_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ingest_lock_rejects_a_path_replaced_before_post_lock_validation() {
        let root = tempdir().unwrap();
        let ingest = store(root.path(), binding_with(1, 1, 60));
        ingest.prepare_source_namespace().unwrap();
        let lock_path = ingest.lock_path();
        let displaced = open_private_lock(&lock_path).unwrap();
        let displaced_path = ingest
            .source_namespace_directory()
            .join("displaced-ingest.lock");

        fs::rename(&lock_path, &displaced_path).unwrap();
        drop(open_private_lock(&lock_path).unwrap());

        let error = try_lock_private_lock(&lock_path, &displaced).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed while open"));

        drop(displaced);
        fs::remove_file(displaced_path).unwrap();
        ingest.try_begin().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ingest_namespace_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let ingest = store(root.path(), binding_with(1, 1, 60));
        ingest.try_begin().unwrap();
        assert_eq!(
            fs::metadata(ingest.namespace_directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for name in [INGEST_STATE_FILE, INGEST_ANCHOR_FILE] {
            assert_eq!(
                fs::metadata(ingest.namespace_directory().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(ingest.lock_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn preview_ingest_retirement_marker_survives_prepublication_crash_boundary() {
        let root = tempdir().unwrap();
        let preview_binding = RemoteDeltaIngestBinding::new(
            PROFILE.parse().unwrap(),
            source(1),
            RedactionProfile::PreviewEnabled,
            revisions(1),
            RemoteDeltaRangePolicy::new(nonzero32(60), 60, false).unwrap(),
        )
        .unwrap();
        let ingest = store(root.path(), preview_binding.clone());
        drop(ingest.try_begin().unwrap());
        assert!(ingest.namespace_directory().is_dir());

        with_history_writer(
            root.path(),
            PROFILE,
            RedactionProfile::Redacted,
            |history, writer| {
                writer
                    .save_source_metadata(
                        &SourceMetadata::new_with_redaction_profile(
                            preview_binding.source.node_id.clone(),
                            SourceKind::Ssh,
                            "remote",
                            RedactionProfile::PreviewEnabled,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                assert_eq!(
                    queue_remote_preview_ingest_retirement(
                        history,
                        &preview_binding.source.node_id,
                        writer,
                    )
                    .unwrap(),
                    RemoteIngestProfileRetirementStatus::Pending
                );
                assert_eq!(
                    retry_remote_preview_ingest_retirement(
                        history,
                        &preview_binding.source.node_id,
                        writer,
                    )
                    .unwrap(),
                    RemoteIngestProfileRetirementStatus::Pending,
                    "a marker queued before metadata publication must not delete visible preview state"
                );
                assert!(ingest.namespace_directory().is_dir());

                writer
                    .publish_remote_source_redaction_profile(
                        &preview_binding.source.node_id,
                        RedactionProfile::Redacted,
                    )
                    .unwrap();
                assert_eq!(
                    retry_remote_preview_ingest_retirement(
                        history,
                        &preview_binding.source.node_id,
                        writer,
                    )
                    .unwrap(),
                    RemoteIngestProfileRetirementStatus::Complete
                );
                assert!(!ingest.namespace_directory().exists());
                let retirement =
                    RemoteIngestRetirementPaths::new(history, &preview_binding.source.node_id);
                assert!(retirement.marker.is_file());

                // Simulate Windows exposing a directory entry that appeared
                // removed before a crash. The retained tombstone makes the
                // cleanup replayable instead of accepting reappeared preview
                // state as a completed retirement.
                drop(ingest.try_begin().unwrap());
                assert!(ingest.namespace_directory().is_dir());
                assert_eq!(
                    retry_remote_preview_ingest_retirement(
                        history,
                        &preview_binding.source.node_id,
                        writer,
                    )
                    .unwrap(),
                    RemoteIngestProfileRetirementStatus::Complete
                );
                assert!(!ingest.namespace_directory().exists());
                assert!(retirement.marker.is_file());
            },
        );
    }
}
