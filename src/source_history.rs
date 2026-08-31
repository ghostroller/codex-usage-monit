use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Days, Duration, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::atomic_file::replace_file;
use crate::history::{
    HISTORY_METRIC_REVISION, LocalHalfHourBucket, QuotaPoint, WeeklyLocalPoint,
    is_current_local_bucket, upsert_quota_point,
};
use crate::history_ownership::HistoryWriteAuthority;
use crate::source_identity::NodeId;
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

mod session_evidence;
pub use session_evidence::*;
mod remote_generation;
pub use remote_generation::*;
mod remote_live;
pub use remote_live::*;
mod local_observation;
mod redaction_retirement;
pub use local_observation::*;
mod source_purge;
pub use source_purge::*;

#[cfg(test)]
use crate::api_cost::API_PRICING_CATALOG_REVISION;
#[cfg(test)]
use crate::history::{HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION};

pub const SOURCE_HISTORY_LAYOUT_VERSION: u32 = 2;
pub const SOURCE_METADATA_VERSION: u32 = 2;

const LAYOUT_DIRECTORY: &str = "history-v2";
const ACCOUNT_DIRECTORY: &str = "account";
const SOURCES_DIRECTORY: &str = "sources";
const BUCKETS_DIRECTORY: &str = "buckets";
const WEEKLY_DIRECTORY: &str = "weekly";
const SOURCE_METADATA_FILE: &str = "source.json";
const ACCOUNT_LOCK_FILE: &str = "account.lock";
const SOURCE_LOCK_FILE: &str = "source.lock";
const BUCKETS_LOCK_FILE: &str = "buckets.lock";
const WEEKLY_LOCK_FILE: &str = "weekly.lock";
const RETENTION_CLOCK_FILE: &str = "retention-clock.json";
const RETENTION_LOCK_FILE: &str = "retention.lock";
const GARBAGE_COLLECTION_SCHEDULE_FILE: &str = "garbage-collection-schedule.json";
const GARBAGE_COLLECTION_SCHEDULE_LOCK_FILE: &str = "garbage-collection-schedule.lock";
const SOURCE_METADATA_ENVELOPE_FORMAT_VERSION: u32 = 1;
const ACCOUNT_SHARD_FORMAT_VERSION: u32 = 1;
const ACCOUNT_QUOTA_REVISION: u32 = 1;
const SOURCE_BUCKET_SHARD_FORMAT_VERSION: u32 = 1;
const SOURCE_WEEKLY_SHARD_FORMAT_VERSION: u32 = 1;
const RETENTION_CLOCK_FORMAT_VERSION: u32 = 1;
const GARBAGE_COLLECTION_SCHEDULE_FORMAT_VERSION: u32 = 1;
const SOURCE_HISTORY_RETENTION_DAYS: i64 = 35;
const RETENTION_CLOCK_MAX_UNCONFIRMED_FORWARD_HOURS: i64 = 24;
const RETENTION_CLOCK_CONFIRMATION_HOURS: i64 = 48;
const RETENTION_CLOCK_MIN_CONFIRMATIONS: u32 = 3;
const MAX_METADATA_FILE_BYTES: u64 = 64 * 1024;
const MAX_SHARD_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_PROFILE_ID_LEN: usize = 64;
const MAX_SOURCE_LABEL_CHARS: usize = 160;
const MAX_SOURCE_LABEL_BYTES: usize = 512;
const MAX_QUOTA_LIMIT_ID_CHARS: usize = 128;
const SOURCE_BUCKET_SECONDS: i64 = 15 * 60;
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AtomicShardFileKind {
    Json,
    GzipJson,
}

impl AtomicShardFileKind {
    fn target_day(self, target: &str) -> Option<NaiveDate> {
        match self {
            Self::Json => shard_day_from_path(Path::new(target)),
            Self::GzipJson => {
                let date = target.strip_suffix(".json.gz")?;
                NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
            }
        }
    }
}

/// Stable profile namespace used below the v2 history layout.
///
/// This deliberately accepts only a conservative path-safe alphabet. The
/// current Codex-home namespace is a 16-character lowercase hexadecimal value,
/// while the wider bound leaves room for a future opaque profile identifier.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct HistoryProfileId(String);

impl HistoryProfileId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for HistoryProfileId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for HistoryProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for HistoryProfileId {
    type Err = HistoryProfileIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > MAX_PROFILE_ID_LEN {
            return Err(HistoryProfileIdParseError(
                "history profile ID has an invalid length",
            ));
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        }) {
            return Err(HistoryProfileIdParseError(
                "history profile ID contains unsafe characters",
            ));
        }
        if value == "." || value == ".." {
            return Err(HistoryProfileIdParseError(
                "history profile ID must not be a path component",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for HistoryProfileId {
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
pub struct HistoryProfileIdParseError(&'static str);

impl fmt::Display for HistoryProfileIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for HistoryProfileIdParseError {}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactionProfile {
    Redacted,
    PreviewEnabled,
}

impl RedactionProfile {
    pub const fn directory_name(self) -> &'static str {
        match self {
            Self::Redacted => "redacted",
            Self::PreviewEnabled => "preview-enabled",
        }
    }
}

impl fmt::Display for RedactionProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.directory_name())
    }
}

impl FromStr for RedactionProfile {
    type Err = RedactionProfileParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "redacted" => Ok(Self::Redacted),
            "preview-enabled" => Ok(Self::PreviewEnabled),
            _ => Err(RedactionProfileParseError),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedactionProfileParseError;

impl fmt::Display for RedactionProfileParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unknown redaction profile")
    }
}

impl std::error::Error for RedactionProfileParseError {}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Local,
    Ssh,
}

/// Source identity and query policy stored independently from SSH connection
/// configuration. `detached` never changes `include_in_aggregates` implicitly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceMetadata {
    schema_version: u32,
    source_id: NodeId,
    kind: SourceKind,
    display_label: String,
    aggregate_redaction_profile: RedactionProfile,
    include_in_aggregates: bool,
    detached: bool,
}

impl SourceMetadata {
    pub fn new(
        source_id: NodeId,
        kind: SourceKind,
        display_label: impl Into<String>,
    ) -> io::Result<Self> {
        Self::new_with_redaction_profile(source_id, kind, display_label, RedactionProfile::Redacted)
    }

    pub fn new_with_redaction_profile(
        source_id: NodeId,
        kind: SourceKind,
        display_label: impl Into<String>,
        aggregate_redaction_profile: RedactionProfile,
    ) -> io::Result<Self> {
        let metadata = Self {
            schema_version: SOURCE_METADATA_VERSION,
            source_id,
            kind,
            display_label: display_label.into(),
            aggregate_redaction_profile,
            include_in_aggregates: true,
            detached: false,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn source_id(&self) -> &NodeId {
        &self.source_id
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    pub fn display_label(&self) -> &str {
        &self.display_label
    }

    pub fn aggregate_redaction_profile(&self) -> RedactionProfile {
        self.aggregate_redaction_profile
    }

    pub fn include_in_aggregates(&self) -> bool {
        self.include_in_aggregates
    }

    pub fn detached(&self) -> bool {
        self.detached
    }

    pub fn set_display_label(&mut self, display_label: impl Into<String>) -> io::Result<()> {
        let previous = std::mem::replace(&mut self.display_label, display_label.into());
        if let Err(error) = self.validate() {
            self.display_label = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn set_include_in_aggregates(&mut self, include: bool) {
        self.include_in_aggregates = include;
    }

    pub fn set_aggregate_redaction_profile(&mut self, profile: RedactionProfile) {
        self.aggregate_redaction_profile = profile;
    }

    pub fn set_detached(&mut self, detached: bool) {
        self.detached = detached;
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema_version != SOURCE_METADATA_VERSION {
            return Err(invalid_data(format!(
                "unsupported source metadata version {}; expected {}",
                self.schema_version, SOURCE_METADATA_VERSION
            )));
        }
        let label = self.display_label.as_str();
        if label.is_empty()
            || label != label.trim()
            || label.chars().count() > MAX_SOURCE_LABEL_CHARS
            || label.len() > MAX_SOURCE_LABEL_BYTES
            || label.chars().any(|character| {
                character.is_control()
                    || matches!(character, '\u{2028}' | '\u{2029}')
                    || is_bidi_control(character)
            })
        {
            return Err(invalid_data("source display label is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountHistoryData {
    pub quota_points: Vec<QuotaPoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceHistoryData {
    pub source: SourceMetadata,
    pub redaction_profile: RedactionProfile,
    pub buckets: Vec<LocalHalfHourBucket>,
    pub weekly_local_points: Vec<WeeklyLocalPoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceHistoryRecordsData {
    pub source: SourceMetadata,
    pub redaction_profile: RedactionProfile,
    pub records: Vec<SourceBucketRecord>,
    pub weekly_records: Vec<SourceWeeklyRecord>,
}

/// Revisioned change for one stable 15-minute source bucket identity.
///
/// Revisions are source/exporter-owned and must be nonzero. A higher revision
/// replaces the currently persisted change for the same `starts_at`; an equal
/// revision must be byte-for-byte semantically identical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceBucketRecord {
    starts_at: DateTime<Utc>,
    revision: u64,
    change: SourceBucketChange,
}

impl SourceBucketRecord {
    pub fn upsert(revision: u64, bucket: LocalHalfHourBucket) -> io::Result<Self> {
        let record = Self {
            starts_at: bucket.starts_at,
            revision,
            change: SourceBucketChange::Upsert(Box::new(bucket)),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn tombstone(starts_at: DateTime<Utc>, revision: u64) -> io::Result<Self> {
        let record = Self {
            starts_at,
            revision,
            change: SourceBucketChange::Tombstone,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn starts_at(&self) -> DateTime<Utc> {
        self.starts_at
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn change(&self) -> &SourceBucketChange {
        &self.change
    }

    fn validate(&self) -> io::Result<()> {
        if self.revision == 0 {
            return Err(invalid_data("source bucket revision must be nonzero"));
        }
        if !is_aligned_bucket_start(self.starts_at) {
            return Err(invalid_data(
                "source bucket record startsAt must be aligned to 15 minutes",
            ));
        }
        if let SourceBucketChange::Upsert(bucket) = &self.change {
            validate_source_bucket(bucket)?;
            if bucket.starts_at != self.starts_at {
                return Err(invalid_data(
                    "source bucket record startsAt does not match its upsert payload",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceBucketChange {
    Upsert(Box<LocalHalfHourBucket>),
    Tombstone,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceWeeklyRecord {
    observed_at: DateTime<Utc>,
    resets_at: DateTime<Utc>,
    revision: u64,
    change: SourceWeeklyChange,
}

impl SourceWeeklyRecord {
    pub fn upsert(revision: u64, point: WeeklyLocalPoint) -> io::Result<Self> {
        let record = Self {
            observed_at: point.observed_at,
            resets_at: point.resets_at,
            revision,
            change: SourceWeeklyChange::Upsert(Box::new(point)),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn tombstone(
        observed_at: DateTime<Utc>,
        resets_at: DateTime<Utc>,
        revision: u64,
    ) -> io::Result<Self> {
        let record = Self {
            observed_at,
            resets_at,
            revision,
            change: SourceWeeklyChange::Tombstone,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn resets_at(&self) -> DateTime<Utc> {
        self.resets_at
    }
    pub fn change(&self) -> &SourceWeeklyChange {
        &self.change
    }

    fn validate(&self) -> io::Result<()> {
        if self.revision == 0 {
            return Err(invalid_data("source weekly revision must be nonzero"));
        }
        if self.resets_at <= self.observed_at {
            return Err(invalid_data("source weekly reset must follow observedAt"));
        }
        if let SourceWeeklyChange::Upsert(point) = &self.change {
            validate_weekly_local_point(point)?;
            if point.observed_at != self.observed_at || point.resets_at != self.resets_at {
                return Err(invalid_data(
                    "source weekly record key does not match its upsert payload",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceWeeklyChange {
    Upsert(Box<WeeklyLocalPoint>),
    Tombstone,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceHistoryGcReport {
    pub shards_pruned: usize,
    pub pruning_deferred: bool,
    pub trusted_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceHistoryWriteReport {
    pub shards_written: usize,
    pub shards_skipped: usize,
}

/// Opt-in v2 history persistence. Constructing this store has no effect on the
/// existing `HistoryStore` and does not create or migrate any files.
#[derive(Clone, Debug)]
pub struct SourceHistoryStore {
    state_root: PathBuf,
    profile_id: HistoryProfileId,
}

/// The only production write surface for source-aware v2 history.
///
/// Construction requires an ownership writer lease bound to the exact
/// durable epoch. Every operation validates that fence before and after its
/// underlying atomic/locked persistence work.
pub struct SourceHistoryWriter<'store, 'authority, 'lease> {
    store: &'store SourceHistoryStore,
    authority: &'authority HistoryWriteAuthority<'lease>,
}

impl SourceHistoryWriter<'_, '_, '_> {
    pub(crate) fn validate_store_binding(&self, expected: &SourceHistoryStore) -> io::Result<()> {
        self.validate()?;
        let writer_root = fs::canonicalize(self.store.state_root())?;
        let expected_root = fs::canonicalize(expected.state_root())?;
        if writer_root != expected_root || self.store.profile_id() != expected.profile_id() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "v2 history writer does not match the expected state root and profile",
            ));
        }
        Ok(())
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.authority.expected_manifest().redaction_profile()
    }

    pub fn validate(&self) -> io::Result<()> {
        self.authority.validate_v2_namespace(
            self.store.state_root(),
            self.store.profile_id(),
            self.redaction_profile(),
        )
    }

    fn validate_redaction(&self, redaction_profile: RedactionProfile) -> io::Result<()> {
        if redaction_profile != self.redaction_profile() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "v2 history writer authority does not match the requested redaction namespace",
            ));
        }
        self.validate()
    }

    fn fenced<T>(
        &self,
        operation: impl FnOnce(&SourceHistoryStore) -> io::Result<T>,
    ) -> io::Result<T> {
        self.validate()?;
        let result = operation(self.store);
        let fence = self.validate();
        match (result, fence) {
            (_, Err(error)) => Err(error),
            (result, Ok(())) => result,
        }
    }

    pub fn save_source_metadata(&self, metadata: &SourceMetadata) -> io::Result<()> {
        self.fenced(|store| store.save_source_metadata_unfenced(metadata))
    }

    pub fn update_source_metadata<F>(
        &self,
        source_id: &NodeId,
        update: F,
    ) -> io::Result<SourceMetadata>
    where
        F: FnOnce(&mut SourceMetadata) -> io::Result<()>,
    {
        self.fenced(|store| store.update_source_metadata_unfenced(source_id, update))
    }

    pub fn garbage_collect(&self, observed_at: DateTime<Utc>) -> io::Result<SourceHistoryGcReport> {
        let redaction_profile = self.redaction_profile();
        self.fenced(|store| {
            store.garbage_collect_unfenced(observed_at, std::slice::from_ref(&redaction_profile))
        })
    }

    /// Runs a production retention pass only when its durable, per-redaction
    /// schedule is due.
    ///
    /// The attempt timestamp is atomically published before scanning. This is
    /// deliberate: a malformed old shard yields one visible failure per
    /// interval instead of forcing every 30-second recorder observation to
    /// repeat the full traversal. A backwards wall-clock step is considered
    /// due once so the durable schedule can rebase; destructive retention
    /// remains governed by the independent conservative retention clock.
    pub fn garbage_collect_if_due(
        &self,
        observed_at: DateTime<Utc>,
        minimum_interval: StdDuration,
    ) -> io::Result<Option<SourceHistoryGcReport>> {
        let redaction_profile = self.redaction_profile();
        self.fenced(|store| {
            store.garbage_collect_if_due_unfenced(observed_at, minimum_interval, redaction_profile)
        })
    }

    pub fn record_account_points(
        &self,
        points: &[QuotaPoint],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.fenced(|store| store.record_account_points_unfenced(points))
    }

    pub fn record_source_bucket_changes(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceBucketRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.record_source_bucket_changes_unfenced(source_id, redaction_profile, records)
        })
    }

    pub fn record_source_weekly_changes(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceWeeklyRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.record_source_weekly_changes_unfenced(source_id, redaction_profile, records)
        })
    }

    pub fn record_source_session_digest_changes(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceSessionDigestRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.record_source_session_digest_changes_unfenced(
                source_id,
                redaction_profile,
                records,
            )
        })
    }

    pub fn stage_complete_fact_batch(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch: &CompleteFactBatch,
    ) -> io::Result<()> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.stage_complete_fact_batch_unfenced(source_id, redaction_profile, batch)
        })
    }

    pub(crate) fn prevalidate_staged_fact_batch(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch_id: &FactBatchId,
    ) -> io::Result<PrevalidatedFactPublication> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.prevalidate_staged_fact_batch_unfenced(source_id, redaction_profile, batch_id)
        })
    }

    pub fn activate_staged_fact_batch(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch_id: &FactBatchId,
    ) -> io::Result<FactActivationReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.activate_staged_fact_batch_unfenced(source_id, redaction_profile, batch_id)
        })
    }

    /// Atomically publishes one already staged and fully prevalidated fact
    /// generation. The expensive merge, namespace scan, generation read, and
    /// digest validation must have completed before entering this method.
    pub(crate) fn publish_prevalidated_fact_batch(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> io::Result<FactActivationReport> {
        self.validate_redaction(publication.redaction_profile())?;
        self.fenced(|store| store.publish_prevalidated_fact_batch_unfenced(publication))
    }

    pub(crate) fn cleanup_prevalidated_fact_publication(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> bool {
        if self
            .validate_redaction(publication.redaction_profile())
            .is_err()
        {
            return true;
        }
        self.fenced(|store| Ok(store.cleanup_prevalidated_fact_publication_unfenced(publication)))
            .unwrap_or(true)
    }

    /// Stages one complete, invisible fact candidate and atomically publishes
    /// it under the same ownership fence.
    ///
    /// Pagination must be completed before entering this method. If staging or
    /// activation fails, the previous active generation remains authoritative;
    /// an unactivated staging directory is harmless and is eligible for the
    /// normal staging cleanup pass.
    pub fn stage_and_activate_complete_fact_batch(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch: &CompleteFactBatch,
    ) -> io::Result<FactActivationReport> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.stage_complete_fact_batch_unfenced(source_id, redaction_profile, batch)?;
            store.activate_staged_fact_batch_unfenced(source_id, redaction_profile, &batch.batch_id)
        })
    }
}

impl SourceHistoryStore {
    pub fn new(state_root: PathBuf, profile_id: HistoryProfileId) -> Self {
        Self {
            state_root,
            profile_id,
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn profile_id(&self) -> &HistoryProfileId {
        &self.profile_id
    }

    pub fn writer<'store, 'authority, 'lease>(
        &'store self,
        authority: &'authority HistoryWriteAuthority<'lease>,
    ) -> io::Result<SourceHistoryWriter<'store, 'authority, 'lease>> {
        let writer = SourceHistoryWriter {
            store: self,
            authority,
        };
        writer.validate()?;
        Ok(writer)
    }

    // Unit tests exercise the storage algorithms directly; production code
    // cannot compile against these unfenced conveniences.
    #[cfg(test)]
    pub(crate) fn save_source_metadata(&self, metadata: &SourceMetadata) -> io::Result<()> {
        self.save_source_metadata_unfenced(metadata)
    }

    #[cfg(test)]
    pub(crate) fn update_source_metadata<F>(
        &self,
        source_id: &NodeId,
        update: F,
    ) -> io::Result<SourceMetadata>
    where
        F: FnOnce(&mut SourceMetadata) -> io::Result<()>,
    {
        self.update_source_metadata_unfenced(source_id, update)
    }

    #[cfg(test)]
    pub(crate) fn garbage_collect(
        &self,
        observed_at: DateTime<Utc>,
    ) -> io::Result<SourceHistoryGcReport> {
        self.garbage_collect_unfenced(
            observed_at,
            &[RedactionProfile::Redacted, RedactionProfile::PreviewEnabled],
        )
    }

    #[cfg(test)]
    pub(crate) fn garbage_collect_if_due(
        &self,
        observed_at: DateTime<Utc>,
        minimum_interval: StdDuration,
        redaction_profile: RedactionProfile,
    ) -> io::Result<Option<SourceHistoryGcReport>> {
        self.garbage_collect_if_due_unfenced(observed_at, minimum_interval, redaction_profile)
    }

    #[cfg(test)]
    pub(crate) fn record_account_points(
        &self,
        points: &[QuotaPoint],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.record_account_points_unfenced(points)
    }

    #[cfg(test)]
    pub(crate) fn record_source_bucket_changes(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceBucketRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.record_source_bucket_changes_unfenced(source_id, redaction_profile, records)
    }

    #[cfg(test)]
    pub(crate) fn record_source_weekly_changes(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceWeeklyRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.record_source_weekly_changes_unfenced(source_id, redaction_profile, records)
    }

    pub fn layout_root(&self) -> PathBuf {
        self.state_root.join(LAYOUT_DIRECTORY)
    }

    pub fn profile_directory(&self) -> PathBuf {
        self.layout_root().join(self.profile_id.as_str())
    }

    pub fn account_directory(&self) -> PathBuf {
        self.profile_directory().join(ACCOUNT_DIRECTORY)
    }

    pub fn sources_directory(&self) -> PathBuf {
        self.profile_directory().join(SOURCES_DIRECTORY)
    }

    pub fn source_directory(&self, source_id: &NodeId) -> PathBuf {
        self.sources_directory().join(source_id.as_str())
    }

    pub fn source_buckets_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source_id)
            .join(redaction_profile.directory_name())
            .join(BUCKETS_DIRECTORY)
    }

    pub fn source_weekly_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source_id)
            .join(redaction_profile.directory_name())
            .join(WEEKLY_DIRECTORY)
    }

    /// Creates a store-owned directory without following links or Windows
    /// reparse points in any component below the trusted state root.
    pub(crate) fn prepare_private_directory(&self, path: &Path) -> io::Result<()> {
        create_private_directory_beneath(&self.state_root, path)
    }

    /// Validates every existing component of a store-owned path below the
    /// trusted state root. Call this before pathname-based reads and removal.
    pub(crate) fn validate_private_path(&self, path: &Path) -> io::Result<()> {
        validate_private_directory_beneath(&self.state_root, path)
    }

    fn private_directory_exists(&self, path: &Path) -> io::Result<bool> {
        private_directory_exists_beneath(&self.state_root, path)
    }

    /// Registers a source descriptor. Re-registering the exact descriptor is
    /// idempotent, but replacing mutable fields through this whole-record API
    /// is rejected so two stale readers cannot silently overwrite each other.
    /// Use the fenced writer's update API for later changes.
    fn save_source_metadata_unfenced(&self, metadata: &SourceMetadata) -> io::Result<()> {
        metadata.validate()?;
        let directory = self.source_directory(metadata.source_id());
        self.prepare_private_directory(&directory)?;
        let lock = open_lock_file(&directory, SOURCE_LOCK_FILE)?;
        lock_exclusive(&lock, &directory, SOURCE_LOCK_FILE)?;
        let path = directory.join(SOURCE_METADATA_FILE);

        if let Some(existing) =
            read_optional_source_metadata_file(&path, &self.profile_id, metadata.source_id())?
        {
            if existing.kind != metadata.kind {
                return Err(invalid_data(
                    "source kind cannot change for an existing source identity",
                ));
            }
            if existing == *metadata {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "source metadata already exists; use update_source_metadata",
            ));
        }

        let envelope = SourceMetadataEnvelope::new(self.profile_id.clone(), metadata.clone());
        let contents = encode_pretty_bounded(&envelope, MAX_METADATA_FILE_BYTES)?;
        write_private_atomically(&path, &contents)
    }

    /// Atomically reads, mutates, validates, and publishes one source
    /// descriptor while holding its stable lock-file inode. This is the safe
    /// API for independent UI/CLI updates to label, inclusion, or detach state.
    fn update_source_metadata_unfenced<F>(
        &self,
        source_id: &NodeId,
        update: F,
    ) -> io::Result<SourceMetadata>
    where
        F: FnOnce(&mut SourceMetadata) -> io::Result<()>,
    {
        let directory = self.source_directory(source_id);
        self.validate_private_path(&directory)?;
        let lock = open_lock_file(&directory, SOURCE_LOCK_FILE)?;
        lock_exclusive(&lock, &directory, SOURCE_LOCK_FILE)?;
        source_purge::reject_source_metadata_update_during_purge(self, &directory, source_id)?;
        let path = directory.join(SOURCE_METADATA_FILE);
        let mut metadata = read_source_metadata_file(&path, &self.profile_id, source_id)?;
        let previous = metadata.clone();
        update(&mut metadata)?;
        metadata.validate()?;
        if metadata.source_id != previous.source_id || metadata.kind != previous.kind {
            return Err(invalid_data(
                "source identity and kind are immutable metadata fields",
            ));
        }
        if metadata != previous {
            let envelope = SourceMetadataEnvelope::new(self.profile_id.clone(), metadata.clone());
            let contents = encode_pretty_bounded(&envelope, MAX_METADATA_FILE_BYTES)?;
            write_private_atomically(&path, &contents)?;
        }
        Ok(metadata)
    }

    pub fn load_source_metadata(&self, source_id: &NodeId) -> io::Result<SourceMetadata> {
        self.with_source_metadata_shared(source_id, |metadata| Ok(metadata.clone()))
    }

    /// Holds the stable source lock across a metadata-dependent read.
    ///
    /// Privacy namespace retirement takes the same lock exclusively before it
    /// publishes a new aggregate profile and moves the old profile out of the
    /// live namespace. Keeping this guard for the complete read prevents a
    /// query that already selected the old profile from losing its files
    /// halfway through the operation.
    pub(super) fn with_source_metadata_shared<T>(
        &self,
        source_id: &NodeId,
        operation: impl FnOnce(&SourceMetadata) -> io::Result<T>,
    ) -> io::Result<T> {
        let directory = self.source_directory(source_id);
        self.validate_private_path(&directory)?;
        let lock = open_lock_file(&directory, SOURCE_LOCK_FILE)?;
        lock_shared(&lock, &directory, SOURCE_LOCK_FILE)?;
        let metadata = read_source_metadata_file(
            &directory.join(SOURCE_METADATA_FILE),
            &self.profile_id,
            source_id,
        )?;
        operation(&metadata)
    }

    /// Enumerates only registered, strictly validated source descriptors.
    /// A malformed `node-*` entry or a valid node ID with the wrong file type
    /// fails closed; unrelated files/directories are ignored.
    pub fn list_source_metadata(&self) -> io::Result<Vec<SourceMetadata>> {
        let directory = self.sources_directory();
        if !self.private_directory_exists(&directory)? {
            return Ok(Vec::new());
        }
        let mut source_ids = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            let source_looking = name.starts_with("node-");
            let parsed = name.parse::<NodeId>();
            let Ok(source_id) = parsed else {
                if source_looking {
                    return Err(invalid_data(format!(
                        "malformed source directory {}",
                        entry.path().display()
                    )));
                }
                continue;
            };
            self.validate_private_path(&entry.path())?;
            source_ids.push(source_id);
        }
        source_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        source_ids
            .into_iter()
            .map(|source_id| self.load_source_metadata(&source_id))
            .collect()
    }

    /// Loads each included source into a separate result. Detached sources
    /// remain eligible when explicitly included; detach and aggregation policy
    /// are intentionally independent.
    pub fn load_included_sources_since(
        &self,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<Vec<SourceHistoryData>> {
        let mut sources = Vec::new();
        for metadata in self.list_source_metadata()? {
            if !metadata.include_in_aggregates() {
                continue;
            }
            let data = self.load_source_since(metadata.source_id(), redaction_profile, since)?;
            if data.source.include_in_aggregates() {
                sources.push(data);
            }
        }
        sources.sort_by(|left, right| {
            left.source
                .source_id()
                .as_str()
                .cmp(right.source.source_id().as_str())
        });
        Ok(sources)
    }

    /// Prunes complete UTC shards older than the 35-day query/repair window.
    /// `observed_at` must be supplied by the central machine's local clock,
    /// not by a remote bucket timestamp. Large clock jumps are persisted as a
    /// pending timeline and cannot immediately authorize destructive pruning.
    fn garbage_collect_if_due_unfenced(
        &self,
        observed_at: DateTime<Utc>,
        minimum_interval: StdDuration,
        redaction_profile: RedactionProfile,
    ) -> io::Result<Option<SourceHistoryGcReport>> {
        if minimum_interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source history garbage-collection interval must be nonzero",
            ));
        }
        let minimum_interval = Duration::from_std(minimum_interval).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "source history garbage-collection interval is too large",
            )
        })?;
        let schedule_directory = self
            .profile_directory()
            .join(redaction_profile.directory_name());
        self.prepare_private_directory(&schedule_directory)?;
        let schedule_lock =
            open_lock_file(&schedule_directory, GARBAGE_COLLECTION_SCHEDULE_LOCK_FILE)?;
        lock_exclusive(
            &schedule_lock,
            &schedule_directory,
            GARBAGE_COLLECTION_SCHEDULE_LOCK_FILE,
        )?;
        let schedule_path = schedule_directory.join(GARBAGE_COLLECTION_SCHEDULE_FILE);
        let current = read_optional_json_file::<GarbageCollectionSchedule>(
            &schedule_path,
            MAX_METADATA_FILE_BYTES,
        )?;
        if let Some(current) = current.as_ref() {
            current.validate(&self.profile_id, redaction_profile)?;
            let elapsed = observed_at.signed_duration_since(current.last_attempted_at);
            if elapsed >= Duration::zero() && elapsed < minimum_interval {
                return Ok(None);
            }
        }

        // Publish the attempt before scanning. GC failures are consequently
        // rate-limited across both process restarts and multiple short-lived
        // CLI invocations using this state root.
        let schedule =
            GarbageCollectionSchedule::new(self.profile_id.clone(), redaction_profile, observed_at);
        let contents = encode_pretty_bounded(&schedule, MAX_METADATA_FILE_BYTES)?;
        write_private_atomically(&schedule_path, &contents)?;

        self.garbage_collect_unfenced(observed_at, std::slice::from_ref(&redaction_profile))
            .map(Some)
    }

    /// Prunes complete UTC shards older than the 35-day query/repair window.
    /// `observed_at` must be supplied by the central machine's local clock,
    /// not by a remote bucket timestamp. Large clock jumps are persisted as a
    /// pending timeline and cannot immediately authorize destructive pruning.
    fn garbage_collect_unfenced(
        &self,
        observed_at: DateTime<Utc>,
        redaction_profiles: &[RedactionProfile],
    ) -> io::Result<SourceHistoryGcReport> {
        let profile_directory = self.profile_directory();
        self.prepare_private_directory(&profile_directory)?;
        let retention_lock = open_lock_file(&profile_directory, RETENTION_LOCK_FILE)?;
        lock_exclusive(&retention_lock, &profile_directory, RETENTION_LOCK_FILE)?;

        let sources = self.list_source_metadata()?;
        let current = read_retention_clock(&profile_directory, &self.profile_id)?;
        let initial_anchor = if current.is_none() {
            self.earliest_persisted_shard_time(&sources, redaction_profiles)?
                .map(|anchor| anchor.min(observed_at))
        } else {
            None
        };
        let (clock, pruning_deferred) = next_retention_clock(current, initial_anchor, observed_at);
        write_retention_clock(&profile_directory, &self.profile_id, &clock)?;
        let cutoff = clock
            .trusted_at
            .checked_sub_signed(Duration::days(SOURCE_HISTORY_RETENTION_DAYS))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let cutoff_day = cutoff.date_naive();

        let mut shards_pruned = prune_account_shards(
            self,
            &self.account_directory(),
            &self.profile_id,
            cutoff_day,
        )?;
        for source in &sources {
            for &redaction_profile in redaction_profiles {
                shards_pruned += prune_source_shards(
                    self,
                    &self.source_buckets_directory(source.source_id(), redaction_profile),
                    &self.profile_id,
                    source.source_id(),
                    redaction_profile,
                    cutoff_day,
                )?;
                shards_pruned += prune_source_weekly_shards(
                    self,
                    &self.source_weekly_directory(source.source_id(), redaction_profile),
                    &self.profile_id,
                    source.source_id(),
                    redaction_profile,
                    cutoff_day,
                )?;
                shards_pruned += session_evidence::garbage_collect_session_evidence_for_source(
                    self,
                    source.source_id(),
                    redaction_profile,
                    cutoff_day,
                    clock.trusted_at,
                )?;
            }
        }
        Ok(SourceHistoryGcReport {
            shards_pruned,
            pruning_deferred,
            trusted_at: Some(clock.trusted_at),
        })
    }

    fn earliest_persisted_shard_time(
        &self,
        sources: &[SourceMetadata],
        redaction_profiles: &[RedactionProfile],
    ) -> io::Result<Option<DateTime<Utc>>> {
        let mut directories = vec![self.account_directory()];
        for source in sources {
            for &redaction_profile in redaction_profiles {
                directories
                    .push(self.source_buckets_directory(source.source_id(), redaction_profile));
                directories
                    .push(self.source_weekly_directory(source.source_id(), redaction_profile));
            }
        }
        let mut earliest = None;
        for directory in directories {
            let Some(day) = earliest_shard_day(self, &directory)? else {
                continue;
            };
            let Some(timestamp) = day.and_hms_opt(0, 0, 0).map(|value| value.and_utc()) else {
                continue;
            };
            earliest =
                Some(earliest.map_or(timestamp, |current: DateTime<Utc>| current.min(timestamp)));
        }
        if let Some(timestamp) =
            session_evidence::earliest_session_evidence_time(self, sources, redaction_profiles)?
        {
            earliest =
                Some(earliest.map_or(timestamp, |current: DateTime<Utc>| current.min(timestamp)));
        }
        Ok(earliest)
    }

    fn record_account_points_unfenced(
        &self,
        points: &[QuotaPoint],
    ) -> io::Result<SourceHistoryWriteReport> {
        // Validate the complete batch before creating a directory, lock, or
        // shard. A single malformed server sample must not leave a partial
        // account write behind.
        for point in points {
            validate_account_quota_point(point)?;
        }
        let additions = group_quota_points_by_day(points);
        if additions.is_empty() {
            return Ok(SourceHistoryWriteReport::default());
        }

        let directory = self.account_directory();
        self.prepare_private_directory(&directory)?;
        let lock = open_lock_file(&directory, ACCOUNT_LOCK_FILE)?;
        lock_exclusive(&lock, &directory, ACCOUNT_LOCK_FILE)?;
        cleanup_atomic_shard_temporary_files(self, &directory, AtomicShardFileKind::Json)?;
        let mut report = SourceHistoryWriteReport::default();

        for (day, points) in additions {
            let path = shard_path(&directory, day);
            let mut shard = match read_account_shard(&path, &self.profile_id, day)? {
                Some(shard) => shard,
                None => AccountShard::new(self.profile_id.clone(), day),
            };
            let mut changed = false;
            for point in points {
                changed |= upsert_quota_point(&mut shard.quota_points, point);
            }
            if !changed {
                report.shards_skipped += 1;
                continue;
            }
            shard.sort();
            write_private_atomically(&path, &encode_pretty_bounded(&shard, MAX_SHARD_FILE_BYTES)?)?;
            report.shards_written += 1;
        }
        Ok(report)
    }

    fn record_source_bucket_changes_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceBucketRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        // An explicit source descriptor is the durable authority for source
        // policy. Refuse to create orphan usage shards.
        let _ = self.load_source_metadata(source_id)?;
        self.record_source_bucket_changes_in_directory_unfenced(
            source_id,
            redaction_profile,
            &self.source_buckets_directory(source_id, redaction_profile),
            records,
        )
    }

    pub(super) fn record_source_bucket_changes_in_directory_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        directory: &Path,
        records: &[SourceBucketRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        let additions = group_source_records_by_day(records, redaction_profile)?;
        if additions.is_empty() {
            return Ok(SourceHistoryWriteReport::default());
        }
        self.prepare_private_directory(directory)?;
        let lock = open_lock_file(directory, BUCKETS_LOCK_FILE)?;
        lock_exclusive(&lock, directory, BUCKETS_LOCK_FILE)?;
        cleanup_atomic_shard_temporary_files(self, directory, AtomicShardFileKind::Json)?;
        let mut report = SourceHistoryWriteReport::default();

        for (day, records) in additions {
            let path = shard_path(directory, day);
            let mut shard = match read_source_bucket_shard(
                &path,
                &self.profile_id,
                source_id,
                redaction_profile,
                day,
            )? {
                Some(shard) => shard,
                None => SourceBucketShard::new(
                    self.profile_id.clone(),
                    source_id.clone(),
                    redaction_profile,
                    day,
                ),
            };
            let mut changed = false;
            for record in records {
                changed |= apply_source_bucket_record(&mut shard.records, record)?;
            }
            if !changed {
                report.shards_skipped += 1;
                continue;
            }
            shard.sort();
            write_private_atomically(&path, &encode_pretty_bounded(&shard, MAX_SHARD_FILE_BYTES)?)?;
            report.shards_written += 1;
        }
        Ok(report)
    }

    fn record_source_weekly_changes_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceWeeklyRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        let additions = group_source_weekly_records_by_day(records)?;
        if additions.is_empty() {
            return Ok(SourceHistoryWriteReport::default());
        }
        let _ = self.load_source_metadata(source_id)?;
        let directory = self.source_weekly_directory(source_id, redaction_profile);
        self.prepare_private_directory(&directory)?;
        let lock = open_lock_file(&directory, WEEKLY_LOCK_FILE)?;
        lock_exclusive(&lock, &directory, WEEKLY_LOCK_FILE)?;
        cleanup_atomic_shard_temporary_files(self, &directory, AtomicShardFileKind::Json)?;
        let mut report = SourceHistoryWriteReport::default();
        for (day, records) in additions {
            let path = shard_path(&directory, day);
            let mut shard = match read_source_weekly_shard(
                &path,
                &self.profile_id,
                source_id,
                redaction_profile,
                day,
            )? {
                Some(shard) => shard,
                None => SourceWeeklyShard::new(
                    self.profile_id.clone(),
                    source_id.clone(),
                    redaction_profile,
                    day,
                ),
            };
            let mut changed = false;
            for record in records {
                changed |= apply_source_weekly_record(&mut shard.records, record)?;
            }
            if !changed {
                report.shards_skipped += 1;
                continue;
            }
            shard.sort();
            write_private_atomically(&path, &encode_pretty_bounded(&shard, MAX_SHARD_FILE_BYTES)?)?;
            report.shards_written += 1;
        }
        Ok(report)
    }

    pub fn load_account_since(&self, since: DateTime<Utc>) -> io::Result<AccountHistoryData> {
        let directory = self.account_directory();
        if !self.private_directory_exists(&directory)? {
            return Ok(AccountHistoryData::default());
        }
        let lock = open_lock_file(&directory, ACCOUNT_LOCK_FILE)?;
        lock_shared(&lock, &directory, ACCOUNT_LOCK_FILE)?;
        let mut points = Vec::new();
        for (day, path) in shard_entries_since(&directory, since)? {
            let Some(shard) = read_account_shard(&path, &self.profile_id, day)? else {
                continue;
            };
            for point in shard
                .quota_points
                .into_iter()
                .filter(|point| point.observed_at >= since)
            {
                let _ = upsert_quota_point(&mut points, point);
            }
        }
        points.sort_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.duration_mins.cmp(&right.duration_mins))
                .then_with(|| left.resets_at.cmp(&right.resets_at))
        });
        Ok(AccountHistoryData {
            quota_points: points,
        })
    }

    /// Loads exactly one source and redaction namespace. This deliberately
    /// returns a source-scoped value and performs no cross-source aggregation.
    pub fn load_source_records_since(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<SourceHistoryRecordsData> {
        self.with_source_metadata_shared(source_id, |source| {
            let records = if source.kind() == SourceKind::Ssh {
                self.with_active_remote_history_generation(
                    source_id,
                    redaction_profile,
                    |generation_directory| {
                        let Some(generation_directory) = generation_directory else {
                            return Ok(Vec::new());
                        };
                        self.load_source_bucket_records_from_directory(
                            source_id,
                            redaction_profile,
                            since,
                            &generation_directory.join(BUCKETS_DIRECTORY),
                        )
                    },
                )?
            } else {
                self.load_source_bucket_records_from_directory(
                    source_id,
                    redaction_profile,
                    since,
                    &self.source_buckets_directory(source_id, redaction_profile),
                )?
            };
            Ok(SourceHistoryRecordsData {
                source: source.clone(),
                redaction_profile,
                records,
                weekly_records: self.load_source_weekly_records_since(
                    source_id,
                    redaction_profile,
                    since,
                )?,
            })
        })
    }

    pub(super) fn load_source_bucket_records_from_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        directory: &Path,
    ) -> io::Result<Vec<SourceBucketRecord>> {
        if !self.private_directory_exists(directory)? {
            return Ok(Vec::new());
        }
        let lock = open_lock_file(directory, BUCKETS_LOCK_FILE)?;
        lock_shared(&lock, directory, BUCKETS_LOCK_FILE)?;
        let mut records = Vec::new();
        for (day, path) in shard_entries_since(directory, since)? {
            let Some(shard) = read_source_bucket_shard(
                &path,
                &self.profile_id,
                source_id,
                redaction_profile,
                day,
            )?
            else {
                continue;
            };
            for record in shard.records {
                if source_record_intersects_since(&record, since) {
                    let _ = apply_source_bucket_record(&mut records, record)?;
                }
            }
        }
        records.sort_by_key(|record| record.starts_at);
        Ok(records)
    }

    fn load_source_weekly_records_since(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<Vec<SourceWeeklyRecord>> {
        let directory = self.source_weekly_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&directory)? {
            return Ok(Vec::new());
        }
        let lock = open_lock_file(&directory, WEEKLY_LOCK_FILE)?;
        lock_shared(&lock, &directory, WEEKLY_LOCK_FILE)?;
        let mut records = Vec::new();
        for (day, path) in shard_entries_since(&directory, since)? {
            let Some(shard) = read_source_weekly_shard(
                &path,
                &self.profile_id,
                source_id,
                redaction_profile,
                day,
            )?
            else {
                continue;
            };
            for record in shard.records {
                if record.observed_at >= since {
                    let _ = apply_source_weekly_record(&mut records, record)?;
                }
            }
        }
        records.sort_by_key(|record| (record.observed_at, record.resets_at));
        Ok(records)
    }

    /// Projects source-scoped revision records into live buckets. Tombstones
    /// remain available through [`Self::load_source_records_since`] but are
    /// excluded from this normal query surface.
    pub fn load_source_since(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<SourceHistoryData> {
        let records = self.load_source_records_since(source_id, redaction_profile, since)?;
        let mut buckets = records
            .records
            .into_iter()
            .filter_map(|record| match record.change {
                SourceBucketChange::Upsert(bucket) => Some(*bucket),
                SourceBucketChange::Tombstone => None,
            })
            .collect::<Vec<_>>();
        let mut weekly_local_points = records
            .weekly_records
            .into_iter()
            .filter_map(|record| match record.change {
                SourceWeeklyChange::Upsert(point) => Some(*point),
                SourceWeeklyChange::Tombstone => None,
            })
            .collect::<Vec<_>>();
        buckets.sort_by_key(|bucket| bucket.starts_at);
        weekly_local_points.sort_by_key(|point| (point.observed_at, point.resets_at));
        Ok(SourceHistoryData {
            source: records.source,
            redaction_profile: records.redaction_profile,
            buckets,
            weekly_local_points,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceMetadataEnvelope {
    format_version: u32,
    profile_id: HistoryProfileId,
    source: SourceMetadata,
}

impl SourceMetadataEnvelope {
    fn new(profile_id: HistoryProfileId, source: SourceMetadata) -> Self {
        Self {
            format_version: SOURCE_METADATA_ENVELOPE_FORMAT_VERSION,
            profile_id,
            source,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionClock {
    trusted_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_last_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pending_confirmations: u32,
}

impl RetentionClock {
    fn current(trusted_at: DateTime<Utc>) -> Self {
        Self {
            trusted_at,
            pending_started_at: None,
            pending_last_at: None,
            pending_confirmations: 0,
        }
    }

    fn pending(trusted_at: DateTime<Utc>, observed_at: DateTime<Utc>) -> Self {
        Self {
            trusted_at,
            pending_started_at: Some(observed_at),
            pending_last_at: Some(observed_at),
            pending_confirmations: 1,
        }
    }

    fn validate(&self) -> io::Result<()> {
        match (self.pending_started_at, self.pending_last_at) {
            (None, None) if self.pending_confirmations == 0 => Ok(()),
            (Some(started_at), Some(last_at))
                if self.pending_confirmations > 0
                    && started_at <= last_at
                    && self.trusted_at < started_at =>
            {
                Ok(())
            }
            _ => Err(invalid_data("source history retention clock is invalid")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionClockEnvelope {
    format_version: u32,
    profile_id: HistoryProfileId,
    clock: RetentionClock,
}

/// Durable throttle for production retention scans.
///
/// This marker is stored below the selected redaction namespace. A writer
/// authorized for one namespace therefore neither suppresses nor inspects GC
/// work for the other namespace. `last_attempted_at` is published before the
/// scan so a damaged shard cannot turn a long-running recorder into a tight,
/// expensive retry loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GarbageCollectionSchedule {
    format_version: u32,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    last_attempted_at: DateTime<Utc>,
}

impl GarbageCollectionSchedule {
    fn new(
        profile_id: HistoryProfileId,
        redaction_profile: RedactionProfile,
        last_attempted_at: DateTime<Utc>,
    ) -> Self {
        Self {
            format_version: GARBAGE_COLLECTION_SCHEDULE_FORMAT_VERSION,
            profile_id,
            redaction_profile,
            last_attempted_at,
        }
    }

    fn validate(
        &self,
        profile_id: &HistoryProfileId,
        redaction_profile: RedactionProfile,
    ) -> io::Result<()> {
        if self.format_version != GARBAGE_COLLECTION_SCHEDULE_FORMAT_VERSION {
            return Err(invalid_data(
                "source history garbage-collection schedule format is invalid",
            ));
        }
        if &self.profile_id != profile_id || self.redaction_profile != redaction_profile {
            return Err(invalid_data(
                "source history garbage-collection schedule namespace is invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountShard {
    format_version: u32,
    quota_revision: u32,
    profile_id: HistoryProfileId,
    utc_day: NaiveDate,
    quota_points: Vec<QuotaPoint>,
}

impl AccountShard {
    fn new(profile_id: HistoryProfileId, utc_day: NaiveDate) -> Self {
        Self {
            format_version: ACCOUNT_SHARD_FORMAT_VERSION,
            quota_revision: ACCOUNT_QUOTA_REVISION,
            profile_id,
            utc_day,
            quota_points: Vec::new(),
        }
    }

    fn sort(&mut self) {
        self.quota_points.sort_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.duration_mins.cmp(&right.duration_mins))
                .then_with(|| left.resets_at.cmp(&right.resets_at))
        });
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceBucketShard {
    format_version: u32,
    metric_revision: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    utc_day: NaiveDate,
    records: Vec<SourceBucketRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceWeeklyShard {
    format_version: u32,
    metric_revision: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    utc_day: NaiveDate,
    records: Vec<SourceWeeklyRecord>,
}

impl SourceWeeklyShard {
    fn new(
        profile_id: HistoryProfileId,
        source_id: NodeId,
        redaction_profile: RedactionProfile,
        utc_day: NaiveDate,
    ) -> Self {
        Self {
            format_version: SOURCE_WEEKLY_SHARD_FORMAT_VERSION,
            metric_revision: HISTORY_METRIC_REVISION,
            profile_id,
            source_id,
            redaction_profile,
            utc_day,
            records: Vec::new(),
        }
    }

    fn sort(&mut self) {
        self.records
            .sort_by_key(|record| (record.observed_at, record.resets_at));
    }
}

impl SourceBucketShard {
    fn new(
        profile_id: HistoryProfileId,
        source_id: NodeId,
        redaction_profile: RedactionProfile,
        utc_day: NaiveDate,
    ) -> Self {
        Self {
            format_version: SOURCE_BUCKET_SHARD_FORMAT_VERSION,
            metric_revision: HISTORY_METRIC_REVISION,
            profile_id,
            source_id,
            redaction_profile,
            utc_day,
            records: Vec::new(),
        }
    }

    fn sort(&mut self) {
        self.records.sort_by_key(|record| record.starts_at);
    }
}

fn read_optional_source_metadata_file(
    path: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
) -> io::Result<Option<SourceMetadata>> {
    match read_source_metadata_file(path, profile_id, source_id) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_source_metadata_file(
    path: &Path,
    profile_id: &HistoryProfileId,
    expected: &NodeId,
) -> io::Result<SourceMetadata> {
    let envelope: SourceMetadataEnvelope = read_json_file(path, MAX_METADATA_FILE_BYTES)?;
    if envelope.format_version != SOURCE_METADATA_ENVELOPE_FORMAT_VERSION {
        return Err(envelope_mismatch(path, "source metadata format version"));
    }
    if &envelope.profile_id != profile_id {
        return Err(envelope_mismatch(path, "profile ID"));
    }
    envelope.source.validate()?;
    if envelope.source.source_id() != expected {
        return Err(envelope_mismatch(path, "source ID"));
    }
    Ok(envelope.source)
}

fn read_account_shard(
    path: &Path,
    profile_id: &HistoryProfileId,
    day: NaiveDate,
) -> io::Result<Option<AccountShard>> {
    let shard: AccountShard = match read_optional_json_file(path, MAX_SHARD_FILE_BYTES)? {
        Some(shard) => shard,
        None => return Ok(None),
    };
    if shard.format_version != ACCOUNT_SHARD_FORMAT_VERSION {
        return Err(envelope_mismatch(path, "account format version"));
    }
    if shard.quota_revision != ACCOUNT_QUOTA_REVISION {
        return Err(envelope_mismatch(path, "quota revision"));
    }
    if &shard.profile_id != profile_id {
        return Err(envelope_mismatch(path, "profile ID"));
    }
    if shard.utc_day != day {
        return Err(envelope_mismatch(path, "UTC day"));
    }
    if shard
        .quota_points
        .iter()
        .any(|point| point.observed_at.date_naive() != day)
    {
        return Err(envelope_mismatch(path, "quota point UTC day"));
    }
    for point in &shard.quota_points {
        validate_account_quota_point(point)?;
    }
    Ok(Some(shard))
}

fn read_source_bucket_shard(
    path: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    day: NaiveDate,
) -> io::Result<Option<SourceBucketShard>> {
    let mut shard: SourceBucketShard = match read_optional_json_file(path, MAX_SHARD_FILE_BYTES)? {
        Some(shard) => shard,
        None => return Ok(None),
    };
    if shard.format_version != SOURCE_BUCKET_SHARD_FORMAT_VERSION {
        return Err(envelope_mismatch(path, "source bucket format version"));
    }
    if shard.metric_revision != HISTORY_METRIC_REVISION {
        return Err(envelope_mismatch(path, "source bucket metric revision"));
    }
    if &shard.profile_id != profile_id {
        return Err(envelope_mismatch(path, "profile ID"));
    }
    if &shard.source_id != source_id {
        return Err(envelope_mismatch(path, "source ID"));
    }
    if shard.redaction_profile != redaction_profile {
        return Err(envelope_mismatch(path, "redaction profile"));
    }
    if shard.utc_day != day {
        return Err(envelope_mismatch(path, "UTC day"));
    }
    let mut unique_records = Vec::with_capacity(shard.records.len());
    for record in std::mem::take(&mut shard.records) {
        record.validate()?;
        if record.starts_at.date_naive() != day {
            return Err(envelope_mismatch(path, "source bucket record UTC day"));
        }
        apply_source_bucket_record(&mut unique_records, record)?;
    }
    shard.records = unique_records;
    if redaction_profile == RedactionProfile::Redacted {
        redact_source_records(&mut shard.records);
    }
    Ok(Some(shard))
}

fn read_source_weekly_shard(
    path: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    day: NaiveDate,
) -> io::Result<Option<SourceWeeklyShard>> {
    let mut shard: SourceWeeklyShard = match read_optional_json_file(path, MAX_SHARD_FILE_BYTES)? {
        Some(shard) => shard,
        None => return Ok(None),
    };
    if shard.format_version != SOURCE_WEEKLY_SHARD_FORMAT_VERSION
        || shard.metric_revision != HISTORY_METRIC_REVISION
        || &shard.profile_id != profile_id
        || &shard.source_id != source_id
        || shard.redaction_profile != redaction_profile
        || shard.utc_day != day
    {
        return Err(envelope_mismatch(path, "source weekly envelope"));
    }
    let mut unique = Vec::with_capacity(shard.records.len());
    for record in std::mem::take(&mut shard.records) {
        record.validate()?;
        if record.observed_at.date_naive() != day {
            return Err(envelope_mismatch(path, "source weekly record UTC day"));
        }
        apply_source_weekly_record(&mut unique, record)?;
    }
    shard.records = unique;
    Ok(Some(shard))
}

fn validate_source_bucket(bucket: &LocalHalfHourBucket) -> io::Result<()> {
    if !is_current_local_bucket(bucket) {
        return Err(invalid_data(
            "source history accepts only aligned 15m buckets",
        ));
    }
    Ok(())
}

fn is_aligned_bucket_start(starts_at: DateTime<Utc>) -> bool {
    starts_at.timestamp().rem_euclid(SOURCE_BUCKET_SECONDS) == 0
        && starts_at.timestamp_subsec_nanos() == 0
}

fn apply_source_bucket_record(
    records: &mut Vec<SourceBucketRecord>,
    incoming: SourceBucketRecord,
) -> io::Result<bool> {
    incoming.validate()?;
    let Some(index) = records
        .iter()
        .position(|record| record.starts_at == incoming.starts_at)
    else {
        records.push(incoming);
        return Ok(true);
    };
    let existing = &records[index];
    if incoming.revision > existing.revision {
        records[index] = incoming;
        return Ok(true);
    }
    if incoming.revision < existing.revision {
        return Ok(false);
    }
    if incoming == *existing {
        Ok(false)
    } else {
        Err(invalid_data(format!(
            "conflicting source bucket changes share startsAt {} and revision {}",
            incoming.starts_at.to_rfc3339(),
            incoming.revision
        )))
    }
}

fn apply_source_weekly_record(
    records: &mut Vec<SourceWeeklyRecord>,
    incoming: SourceWeeklyRecord,
) -> io::Result<bool> {
    incoming.validate()?;
    let Some(index) = records.iter().position(|record| {
        record.observed_at == incoming.observed_at && record.resets_at == incoming.resets_at
    }) else {
        records.push(incoming);
        return Ok(true);
    };
    let existing = &records[index];
    if incoming.revision > existing.revision {
        records[index] = incoming;
        return Ok(true);
    }
    if incoming.revision < existing.revision {
        return Ok(false);
    }
    if incoming == *existing {
        Ok(false)
    } else {
        Err(invalid_data(format!(
            "conflicting source weekly changes share key ({}, {}) and revision {}",
            incoming.observed_at.to_rfc3339(),
            incoming.resets_at.to_rfc3339(),
            incoming.revision
        )))
    }
}

fn validate_weekly_local_point(point: &WeeklyLocalPoint) -> io::Result<()> {
    if point.resets_at <= point.observed_at {
        return Err(invalid_data("source weekly reset must follow observedAt"));
    }
    if point.estimator_revision == 0 {
        return Err(invalid_data("source weekly estimator revision is invalid"));
    }
    if point
        .partial_reasons
        .iter()
        .any(|reason| reason.trim().is_empty() || reason.chars().any(char::is_control))
    {
        return Err(invalid_data("source weekly partial reason is invalid"));
    }
    Ok(())
}

fn source_record_intersects_since(record: &SourceBucketRecord, since: DateTime<Utc>) -> bool {
    record
        .starts_at
        .checked_add_signed(chrono::Duration::seconds(SOURCE_BUCKET_SECONDS))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
        > since
}

fn validate_account_quota_point(point: &QuotaPoint) -> io::Result<()> {
    let limit_id = point.limit_id.trim();
    if limit_id.is_empty()
        || limit_id != point.limit_id
        || limit_id.chars().count() > MAX_QUOTA_LIMIT_ID_CHARS
        || limit_id.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{2028}' | '\u{2029}')
                || is_bidi_control(character)
        })
    {
        return Err(invalid_data("account quota limit ID is invalid"));
    }
    if point.duration_mins <= 0 {
        return Err(invalid_data(
            "account quota duration must be greater than zero",
        ));
    }
    if !point.used_percent.is_finite()
        || !(0.0..=100.0).contains(&point.used_percent)
        || !point.remaining_percent.is_finite()
        || !(0.0..=100.0).contains(&point.remaining_percent)
    {
        return Err(invalid_data(
            "account quota percentages must be finite values from 0 to 100",
        ));
    }
    Ok(())
}

fn redact_source_records(records: &mut [SourceBucketRecord]) {
    for record in records {
        let SourceBucketChange::Upsert(bucket) = &mut record.change else {
            continue;
        };
        for group in &mut bucket.project_groups {
            if group.title.is_some() {
                group.title = Some("[redacted]".to_string());
            }
            if group.message_preview.is_some() {
                group.message_preview = Some("[redacted]".to_string());
            }
        }
    }
}

fn group_quota_points_by_day(points: &[QuotaPoint]) -> BTreeMap<NaiveDate, Vec<QuotaPoint>> {
    let mut result = BTreeMap::new();
    for point in points {
        result
            .entry(point.observed_at.date_naive())
            .or_insert_with(Vec::new)
            .push(point.clone());
    }
    result
}

fn group_source_records_by_day(
    records: &[SourceBucketRecord],
    redaction_profile: RedactionProfile,
) -> io::Result<BTreeMap<NaiveDate, Vec<SourceBucketRecord>>> {
    let mut result = BTreeMap::new();
    for record in records {
        record.validate()?;
        let mut record = record.clone();
        if redaction_profile == RedactionProfile::Redacted {
            redact_source_records(std::slice::from_mut(&mut record));
        }
        result
            .entry(record.starts_at.date_naive())
            .or_insert_with(Vec::new)
            .push(record);
    }
    Ok(result)
}

fn group_source_weekly_records_by_day(
    records: &[SourceWeeklyRecord],
) -> io::Result<BTreeMap<NaiveDate, Vec<SourceWeeklyRecord>>> {
    let mut result = BTreeMap::new();
    for record in records {
        record.validate()?;
        result
            .entry(record.observed_at.date_naive())
            .or_insert_with(Vec::new)
            .push(record.clone());
    }
    Ok(result)
}

fn shard_entries_since(
    directory: &Path,
    since: DateTime<Utc>,
) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    let earliest_day = since
        .date_naive()
        .checked_sub_days(Days::new(1))
        .unwrap_or(NaiveDate::MIN);
    let mut result = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let Some(day) = shard_day_from_path(&path) else {
            continue;
        };
        if day >= earliest_day {
            result.push((day, path));
        }
    }
    result.sort_by_key(|(day, _)| *day);
    Ok(result)
}

fn shard_entries(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    if !store.private_directory_exists(directory)? {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let path = entry?.path();
        let Some(day) = shard_day_from_path(&path) else {
            continue;
        };
        result.push((day, path));
    }
    result.sort_by_key(|(day, _)| *day);
    Ok(result)
}

fn earliest_shard_day(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<Option<NaiveDate>> {
    Ok(shard_entries(store, directory)?
        .into_iter()
        .map(|(day, _)| day)
        .min())
}

fn read_retention_clock(
    directory: &Path,
    profile_id: &HistoryProfileId,
) -> io::Result<Option<RetentionClock>> {
    let path = directory.join(RETENTION_CLOCK_FILE);
    let envelope: RetentionClockEnvelope =
        match read_optional_json_file(&path, MAX_METADATA_FILE_BYTES)? {
            Some(envelope) => envelope,
            None => return Ok(None),
        };
    if envelope.format_version != RETENTION_CLOCK_FORMAT_VERSION {
        return Err(envelope_mismatch(&path, "retention clock format version"));
    }
    if &envelope.profile_id != profile_id {
        return Err(envelope_mismatch(&path, "profile ID"));
    }
    envelope.clock.validate()?;
    Ok(Some(envelope.clock))
}

fn next_retention_clock(
    current: Option<RetentionClock>,
    initial_anchor: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
) -> (RetentionClock, bool) {
    let Some(mut clock) = current.or_else(|| initial_anchor.map(RetentionClock::current)) else {
        return (RetentionClock::current(observed_at), false);
    };
    if observed_at <= clock.trusted_at {
        return (RetentionClock::current(clock.trusted_at), false);
    }
    let maximum = clock
        .trusted_at
        .checked_add_signed(Duration::hours(
            RETENTION_CLOCK_MAX_UNCONFIRMED_FORWARD_HOURS,
        ))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    if observed_at <= maximum {
        return (RetentionClock::current(observed_at), false);
    }
    let continues = clock
        .pending_started_at
        .zip(clock.pending_last_at)
        .is_some_and(|(_, last_at)| observed_at >= last_at);
    if !continues {
        return (RetentionClock::pending(clock.trusted_at, observed_at), true);
    }
    clock.pending_last_at = Some(observed_at);
    clock.pending_confirmations = clock.pending_confirmations.saturating_add(1);
    let stable_since = clock
        .pending_started_at
        .and_then(|started_at| {
            started_at.checked_add_signed(Duration::hours(RETENTION_CLOCK_CONFIRMATION_HOURS))
        })
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    if observed_at >= stable_since
        && clock.pending_confirmations >= RETENTION_CLOCK_MIN_CONFIRMATIONS
    {
        (RetentionClock::current(observed_at), false)
    } else {
        (clock, true)
    }
}

fn write_retention_clock(
    directory: &Path,
    profile_id: &HistoryProfileId,
    clock: &RetentionClock,
) -> io::Result<()> {
    clock.validate()?;
    let envelope = RetentionClockEnvelope {
        format_version: RETENTION_CLOCK_FORMAT_VERSION,
        profile_id: profile_id.clone(),
        clock: *clock,
    };
    let contents = encode_pretty_bounded(&envelope, MAX_METADATA_FILE_BYTES)?;
    write_private_atomically(&directory.join(RETENTION_CLOCK_FILE), &contents)
}

fn prune_account_shards(
    store: &SourceHistoryStore,
    directory: &Path,
    profile_id: &HistoryProfileId,
    cutoff_day: NaiveDate,
) -> io::Result<usize> {
    if !store.private_directory_exists(directory)? {
        return Ok(0);
    }
    let lock = open_lock_file(directory, ACCOUNT_LOCK_FILE)?;
    lock_exclusive(&lock, directory, ACCOUNT_LOCK_FILE)?;
    cleanup_atomic_shard_temporary_files(store, directory, AtomicShardFileKind::Json)?;
    let mut pruned = 0;
    for (day, path) in shard_entries(store, directory)? {
        if day >= cutoff_day {
            continue;
        }
        let Some(_) = read_account_shard(&path, profile_id, day)? else {
            continue;
        };
        store.validate_private_path(directory)?;
        fs::remove_file(path)?;
        pruned += 1;
    }
    if pruned > 0 {
        store.validate_private_path(directory)?;
        sync_directory(directory)?;
    }
    Ok(pruned)
}

fn prune_source_shards(
    store: &SourceHistoryStore,
    directory: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    cutoff_day: NaiveDate,
) -> io::Result<usize> {
    if !store.private_directory_exists(directory)? {
        return Ok(0);
    }
    let lock = open_lock_file(directory, BUCKETS_LOCK_FILE)?;
    lock_exclusive(&lock, directory, BUCKETS_LOCK_FILE)?;
    cleanup_atomic_shard_temporary_files(store, directory, AtomicShardFileKind::Json)?;
    let mut pruned = 0;
    for (day, path) in shard_entries(store, directory)? {
        if day >= cutoff_day {
            continue;
        }
        let Some(_) =
            read_source_bucket_shard(&path, profile_id, source_id, redaction_profile, day)?
        else {
            continue;
        };
        store.validate_private_path(directory)?;
        fs::remove_file(path)?;
        pruned += 1;
    }
    if pruned > 0 {
        store.validate_private_path(directory)?;
        sync_directory(directory)?;
    }
    Ok(pruned)
}

fn prune_source_weekly_shards(
    store: &SourceHistoryStore,
    directory: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    cutoff_day: NaiveDate,
) -> io::Result<usize> {
    if !store.private_directory_exists(directory)? {
        return Ok(0);
    }
    let lock = open_lock_file(directory, WEEKLY_LOCK_FILE)?;
    lock_exclusive(&lock, directory, WEEKLY_LOCK_FILE)?;
    cleanup_atomic_shard_temporary_files(store, directory, AtomicShardFileKind::Json)?;
    let mut pruned = 0;
    for (day, path) in shard_entries(store, directory)? {
        if day >= cutoff_day {
            continue;
        }
        let Some(_) =
            read_source_weekly_shard(&path, profile_id, source_id, redaction_profile, day)?
        else {
            continue;
        };
        store.validate_private_path(directory)?;
        fs::remove_file(path)?;
        pruned += 1;
    }
    if pruned > 0 {
        store.validate_private_path(directory)?;
        sync_directory(directory)?;
    }
    Ok(pruned)
}

fn shard_day_from_path(path: &Path) -> Option<NaiveDate> {
    if path.extension().and_then(OsStr::to_str) != Some("json") {
        return None;
    }
    NaiveDate::parse_from_str(path.file_stem()?.to_str()?, "%Y-%m-%d").ok()
}

fn shard_path(directory: &Path, day: NaiveDate) -> PathBuf {
    directory.join(format!("{}.json", day.format("%Y-%m-%d")))
}

/// Returns the exact published target encoded in a temporary file created by
/// `write_private_atomically`.
///
/// The target is deliberately part of the name. Recovery may therefore
/// distinguish one of our interrupted shard writes from an arbitrary hidden
/// file without relying on an age or process-liveness heuristic.
pub(super) fn atomic_temporary_target_name(name: &OsStr) -> Option<&str> {
    let name = name.to_str()?;
    let body = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))?;
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

pub(super) fn is_atomic_shard_temporary_file(name: &OsStr, kind: AtomicShardFileKind) -> bool {
    atomic_temporary_target_name(name).is_some_and(|target| kind.target_day(target).is_some())
}

/// Removes only target-bound atomic shard temporaries while the caller holds
/// the corresponding family lock exclusively. Unknown hidden names fail
/// closed and are never removed. The published-file validation rejects
/// symlinks and, on Windows, every form of reparse point before deletion.
pub(super) fn cleanup_atomic_shard_temporary_files(
    store: &SourceHistoryStore,
    directory: &Path,
    kind: AtomicShardFileKind,
) -> io::Result<usize> {
    store.validate_private_path(directory)?;
    let mut temporary_files = Vec::new();
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let name = entry.file_name();
        if is_atomic_shard_temporary_file(&name, kind) {
            let path = entry.path();
            validate_published_private_file(&path)?;
            temporary_files.push(path);
            continue;
        }
        if name.to_string_lossy().starts_with('.') {
            return Err(invalid_data(format!(
                "unexpected hidden path in source history shard family {}",
                entry.path().display()
            )));
        }
    }

    for path in &temporary_files {
        store.validate_private_path(directory)?;
        validate_published_private_file(path)?;
        fs::remove_file(path)?;
    }
    if !temporary_files.is_empty() {
        store.validate_private_path(directory)?;
        sync_directory(directory)?;
    }
    Ok(temporary_files.len())
}

fn encode_pretty<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut contents = serde_json::to_vec_pretty(value)
        .map_err(|error| invalid_data(format!("could not encode source history: {error}")))?;
    contents.push(b'\n');
    Ok(contents)
}

fn encode_pretty_bounded<T: Serialize>(value: &T, maximum: u64) -> io::Result<Vec<u8>> {
    let contents = encode_pretty(value)?;
    if contents.len() as u64 > maximum {
        return Err(invalid_data("encoded source history shard is too large"));
    }
    Ok(contents)
}

fn read_optional_json_file<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum: u64,
) -> io::Result<Option<T>> {
    match read_json_file(path, maximum) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path, maximum: u64) -> io::Result<T> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_data_file_metadata(path, &path_metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let subject = format!("source history path {}", path.display());
    let mut file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, &subject))?;
    let metadata = file.metadata()?;
    validate_data_file_metadata(path, &metadata)?;
    ensure_opened_file_matches_path(
        path,
        &file,
        &path_metadata,
        &metadata,
        "source history file",
    )?;
    if metadata.len() > maximum {
        return Err(invalid_data(format!(
            "source history file {} is too large",
            path.display()
        )));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > maximum {
        return Err(invalid_data(format!(
            "source history file {} is too large",
            path.display()
        )));
    }
    serde_json::from_slice(&contents).map_err(|error| {
        invalid_data(format!(
            "source history file {} is invalid: {error}",
            path.display()
        ))
    })
}

fn open_lock_file(directory: &Path, name: &str) -> io::Result<File> {
    create_private_directory(directory)?;
    let path = directory.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_lock_metadata(&path, &metadata)?,
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
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        options.share_mode(stable_lock_share_mode());
    }
    add_nofollow_flags(&mut options);
    let subject = format!("source history lock {}", path.display());
    let file = options
        .open(&path)
        .map_err(|error| map_nofollow_error(error, &subject))?;
    let metadata = file.metadata()?;
    validate_lock_metadata(&path, &metadata)?;
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path, &path_metadata)?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &metadata,
        "source history lock",
    )?;
    Ok(file)
}

fn lock_exclusive(file: &File, directory: &Path, name: &str) -> io::Result<()> {
    fs2::FileExt::lock_exclusive(file)?;
    validate_locked_file(file, directory, name)
}

fn lock_shared(file: &File, directory: &Path, name: &str) -> io::Result<()> {
    fs2::FileExt::lock_shared(file)?;
    validate_locked_file(file, directory, name)
}

fn validate_locked_file(file: &File, directory: &Path, name: &str) -> io::Result<()> {
    validate_private_directory(directory)?;
    let path = directory.join(name);
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path, &path_metadata)?;
    let opened_metadata = file.metadata()?;
    validate_lock_metadata(&path, &opened_metadata)?;
    ensure_opened_file_matches_path(
        &path,
        file,
        &path_metadata,
        &opened_metadata,
        "source history lock",
    )
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

fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;
    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("source-history"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_published_private_file(path)?;
        sync_directory(parent)?;
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
            Ok(file) => {
                let validation = (|| {
                    let metadata = file.metadata()?;
                    validate_data_file_metadata(&temporary, &metadata)?;
                    #[cfg(windows)]
                    validate_windows_private_file(
                        &temporary,
                        &file,
                        "source history temporary file",
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
        "could not allocate a unique source history temporary file",
    ))
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
        #[cfg(windows)]
        reject_windows_reparse_components_before_create(path, "source history directory")?;
        fs::create_dir_all(path)?;
    }
    validate_private_directory(path)
}

fn create_private_directory_beneath(root: &Path, path: &Path) -> io::Result<()> {
    match validate_trusted_state_root(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_trusted_state_root(root)?;
            validate_trusted_state_root(root)?;
        }
        Err(error) => return Err(error),
    }
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "source history path {} is outside trusted state root {}",
                path.display(),
                root.display()
            ),
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source history path contains a non-normal component",
            ));
        };
        if current == root {
            validate_trusted_state_root(&current)?;
        } else {
            validate_private_directory(&current)?;
        }
        current.push(name);
        match validate_private_directory(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_child_directory(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    if current == root {
        validate_trusted_state_root(&current)
    } else {
        validate_private_directory(&current)
    }
}

fn validate_private_directory_beneath(root: &Path, path: &Path) -> io::Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "source history path {} is outside trusted state root {}",
                path.display(),
                root.display()
            ),
        )
    })?;
    validate_trusted_state_root(root)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source history path contains a non-normal component",
            ));
        };
        current.push(name);
        validate_private_directory(&current)?;
    }
    Ok(())
}

fn private_directory_exists_beneath(root: &Path, path: &Path) -> io::Result<bool> {
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source history path is outside trusted state root",
        )
    })?;
    match validate_trusted_state_root(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source history path contains a non-normal component",
            ));
        };
        current.push(name);
        match validate_private_directory(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn validate_trusted_state_root(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
        return Err(invalid_data(format!(
            "source history state root {} must be a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    validate_unix_private_state_root(&metadata, path)?;
    #[cfg(windows)]
    validate_windows_private_directory(path, "source history state root")?;
    Ok(())
}

fn create_trusted_state_root(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(windows)]
    {
        reject_windows_reparse_components_before_create(path, "source history state root")?;
        fs::create_dir_all(path)?;
    }
    #[cfg(not(any(unix, windows)))]
    fs::create_dir_all(path)?;
    validate_trusted_state_root(path)
}

fn create_private_child_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)?;
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_data(format!(
            "source history directory {} must not be a symbolic link",
            path.display()
        )));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_data(format!(
            "source history path {} must be a directory",
            path.display()
        )));
    }
    ensure_private_directory(&metadata)?;
    #[cfg(windows)]
    validate_windows_private_directory(path, "source history directory")?;
    Ok(())
}

fn validate_published_private_file(path: &Path) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_data_file_metadata(path, &path_metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options.open(path).map_err(|error| {
        map_nofollow_error(error, &format!("source history path {}", path.display()))
    })?;
    let metadata = file.metadata()?;
    validate_data_file_metadata(path, &metadata)?;
    ensure_opened_file_matches_path(
        path,
        &file,
        &path_metadata,
        &metadata,
        "source history published file",
    )
}

fn validate_data_file_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_data(format!(
            "source history path {} must not be a symbolic link",
            path.display()
        )));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_data(format!(
            "source history path {} must be a regular file",
            path.display()
        )));
    }
    ensure_private_file(metadata)
}

fn validate_lock_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_data(format!(
            "source history lock {} must not be a symbolic link",
            path.display()
        )));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_data(format!(
            "source history lock {} must be a regular file",
            path.display()
        )));
    }
    ensure_private_path(metadata, "source history lock")
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
        || windows_attributes_are_reparse(metadata.file_attributes(), FILE_ATTRIBUTE_REPARSE_POINT)
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(any(windows, test))]
fn windows_attributes_are_reparse(attributes: u32, reparse_flag: u32) -> bool {
    attributes & reparse_flag != 0
}

#[cfg(windows)]
fn stable_lock_share_mode() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    FILE_SHARE_READ | FILE_SHARE_WRITE
}

#[cfg(test)]
fn stable_lock_share_mode_for_test() -> u32 {
    // Win32 FILE_SHARE_READ | FILE_SHARE_WRITE. FILE_SHARE_DELETE (0x4) is
    // deliberately absent so a stable lock pathname cannot be replaced.
    0x1 | 0x2
}

#[cfg(unix)]
fn ensure_private_file(metadata: &fs::Metadata) -> io::Result<()> {
    ensure_private_path(metadata, "source history file")
}

#[cfg(unix)]
fn ensure_private_directory(metadata: &fs::Metadata) -> io::Result<()> {
    ensure_private_path(metadata, "source history directory")
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

#[cfg(unix)]
fn validate_unix_private_state_root(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // SAFETY: geteuid has no preconditions and retains no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "source history state root {} must be owned by the current user",
                path.display()
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "source history state root {} must have mode 0700 (found {mode:04o})",
                path.display(),
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_file(_metadata: &fs::Metadata) -> io::Result<()> {
    // std does not expose a portable Windows DACL check. The type and symlink
    // checks remain enforced; an explicit history root inherits its ACL.
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_directory(_metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_path(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    Ok(())
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
        Err(invalid_data(format!(
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
    validate_windows_private_file(path, opened_file, subject)?;
    let expected = windows_file_identity(opened_file, subject)?;

    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let current = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    validate_windows_private_file(path, &current, subject)?;
    if windows_file_identity(&current, subject)? == expected {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
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

#[cfg(windows)]
fn windows_file_identity(file: &File, subject: &str) -> io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the raw handle remains owned and live for this call; the API
    // initializes the output on success and retains neither pointer.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful API result initializes the complete structure.
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
        _ => Err(invalid_data(format!(
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

fn validate_directory_sync_target(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) || !metadata.file_type().is_dir() {
        return Err(invalid_data(format!(
            "directory sync target {} must be a real directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    validate_directory_sync_target(path, &fs::symlink_metadata(path)?)?;
    let directory = File::open(path)?;
    validate_directory_sync_target(path, &directory.metadata()?)?;
    directory.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    fn open_directory_handle(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let directory = options.open(path)?;
        validate_directory_sync_target(path, &directory.metadata()?)?;
        Ok(directory)
    }

    // Win32 does not document FlushFileBuffers as a supported directory-handle
    // operation. Requiring a writable directory handle makes ordinary usage
    // filesystem-dependent (notably across NTFS/ReFS/SMB), so Windows crash
    // durability is instead provided at the mutation sites: write-through
    // atomic publication plus persistent recovery markers/deterministic trash.
    // This platform hook still validates that callers reached the same real
    // directory and never silently follow a reparse point.
    validate_directory_sync_target(path, &fs::symlink_metadata(path)?)?;
    let directory = open_directory_handle(path)?;
    let expected_identity = windows_file_identity(&directory, "directory sync target")?;
    let current = open_directory_handle(path)?;
    if windows_file_identity(&current, "directory sync target")? != expected_identity {
        return Err(invalid_data(
            "directory sync target changed while it was being opened",
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory durability is unsupported on this platform",
    ))
}

fn envelope_mismatch(path: &Path, field: &str) -> io::Error {
    invalid_data(format!(
        "source history envelope {} does not match expected {field}",
        path.display()
    ))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use serde_json::Value;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;

    use super::*;
    use crate::domain::{Provenance, TokenUsage};
    use crate::history::LocalProjectUsageGroup;
    use crate::history_ownership::{
        HistoryOwnershipState, HistoryOwnershipStore, InitializeV1Outcome, OwnershipCasOutcome,
    };

    const PROFILE: &str = "0123456789abcdef";
    const SOURCE_A: &str = "node-0123456789abcdef0123456789abcdef";
    const SOURCE_B: &str = "node-fedcba9876543210fedcba9876543210";

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn store(root: &Path) -> SourceHistoryStore {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            if root.exists() {
                fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        SourceHistoryStore::new(root.to_path_buf(), PROFILE.parse().unwrap())
    }

    fn metadata(source: &str, label: &str) -> SourceMetadata {
        SourceMetadata::new(source.parse().unwrap(), SourceKind::Local, label).unwrap()
    }

    fn bucket(starts_at: DateTime<Utc>, total: u64) -> LocalHalfHourBucket {
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
        }
    }

    fn bucket_with_content(
        starts_at: DateTime<Utc>,
        title: &str,
        message_preview: &str,
    ) -> LocalHalfHourBucket {
        let mut bucket = bucket(starts_at, 10);
        bucket.project_groups.push(LocalProjectUsageGroup {
            thread_id: "thread-secret".to_string(),
            title: Some(title.to_string()),
            message_preview: Some(message_preview.to_string()),
            token_usage: bucket.token_usage,
            estimated_cost_units: bucket.estimated_cost_units,
            call_count: bucket.call_count,
            ..LocalProjectUsageGroup::default()
        });
        bucket
    }

    fn upsert_record(revision: u64, bucket: LocalHalfHourBucket) -> SourceBucketRecord {
        SourceBucketRecord::upsert(revision, bucket).unwrap()
    }

    fn quota_point(observed_at: DateTime<Utc>) -> QuotaPoint {
        QuotaPoint {
            observed_at,
            limit_id: "codex".to_string(),
            duration_mins: 10_080,
            resets_at: observed_at + Duration::days(3),
            used_percent: 25.0,
            remaining_percent: 75.0,
            provenance: Provenance::ServerSnapshot,
        }
    }

    fn weekly_point(observed_at: DateTime<Utc>, total: u64) -> WeeklyLocalPoint {
        WeeklyLocalPoint {
            observed_at,
            resets_at: observed_at + Duration::days(3),
            token_usage: TokenUsage {
                input_tokens: total,
                total_tokens: total,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(total),
            api_long_context_extra_cost_units: Some(u128::from(total / 2)),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            call_count: 2,
            partial_reasons: vec!["rollout_scan_incomplete".to_string()],
        }
    }

    #[test]
    fn profile_and_redaction_names_are_strict_and_path_safe() {
        assert_eq!(SOURCE_HISTORY_LAYOUT_VERSION, 2);
        assert_eq!(
            PROFILE.parse::<HistoryProfileId>().unwrap().as_str(),
            PROFILE
        );
        for invalid in ["", ".", "..", "UPPER", "a/b", "with space", "redacted!"] {
            assert!(invalid.parse::<HistoryProfileId>().is_err());
        }
        assert_eq!(
            "redacted".parse::<RedactionProfile>().unwrap(),
            RedactionProfile::Redacted
        );
        assert_eq!(
            "preview-enabled".parse::<RedactionProfile>().unwrap(),
            RedactionProfile::PreviewEnabled
        );
        assert!("preview_enabled".parse::<RedactionProfile>().is_err());
    }

    #[test]
    fn atomic_shard_temp_names_are_exact_and_target_bound() {
        assert_eq!(
            atomic_temporary_target_name(OsStr::new(".2026-08-30.json.123.7.tmp")),
            Some("2026-08-30.json")
        );
        assert!(is_atomic_shard_temporary_file(
            OsStr::new(".2026-08-30.json.123.7.tmp"),
            AtomicShardFileKind::Json,
        ));
        assert!(is_atomic_shard_temporary_file(
            OsStr::new(".2026-08-30.json.gz.123.7.tmp"),
            AtomicShardFileKind::GzipJson,
        ));
        for invalid in [
            ".2026-08-30.json.tmp",
            ".2026-08-30.json.bad.7.tmp",
            ".2026-08-30.json.123.bad.tmp",
            ".2026-08-30.json.123.7.tmp.extra",
            ".not-a-day.json.123.7.tmp",
        ] {
            assert!(!is_atomic_shard_temporary_file(
                OsStr::new(invalid),
                AtomicShardFileKind::Json,
            ));
        }
        assert!(!is_atomic_shard_temporary_file(
            OsStr::new(".2026-08-30.json.123.7.tmp"),
            AtomicShardFileKind::GzipJson,
        ));
    }

    #[test]
    fn ordinary_shard_writers_recover_exact_crash_temps_under_family_locks() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "temp-recovery"))
            .unwrap();
        let observed_at = at(30, 9, 0);

        store
            .record_account_points(&[quota_point(observed_at)])
            .unwrap();
        let account_temp = store.account_directory().join(".2026-08-30.json.101.1.tmp");
        write_private_atomically(&account_temp, b"interrupted account shard\n").unwrap();
        store
            .record_account_points(&[quota_point(observed_at + Duration::minutes(1))])
            .unwrap();
        assert!(!account_temp.exists());

        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(observed_at, 10))],
            )
            .unwrap();
        let bucket_directory = store.source_buckets_directory(&source, RedactionProfile::Redacted);
        let bucket_temp = bucket_directory.join(".2026-08-30.json.102.2.tmp");
        write_private_atomically(&bucket_temp, b"interrupted bucket shard\n").unwrap();
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(2, bucket(observed_at, 20))],
            )
            .unwrap();
        assert!(!bucket_temp.exists());

        let weekly = weekly_point(observed_at, 10);
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::Redacted,
                &[SourceWeeklyRecord::upsert(1, weekly.clone()).unwrap()],
            )
            .unwrap();
        let weekly_directory = store.source_weekly_directory(&source, RedactionProfile::Redacted);
        let weekly_temp = weekly_directory.join(".2026-08-30.json.103.3.tmp");
        write_private_atomically(&weekly_temp, b"interrupted weekly shard\n").unwrap();
        let replacement = weekly_point(observed_at, 20);
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::Redacted,
                &[SourceWeeklyRecord::upsert(2, replacement).unwrap()],
            )
            .unwrap();
        assert!(!weekly_temp.exists());
    }

    #[test]
    fn shard_temp_recovery_preserves_and_rejects_unknown_hidden_names() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "temp-recovery"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        let buckets = store.source_buckets_directory(&source, RedactionProfile::Redacted);
        let unknown = buckets.join(".2026-08-30.json.bad.tmp");
        write_private_atomically(&unknown, b"not one of our atomic temps\n").unwrap();

        let error = store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(2, bucket(starts_at, 20))],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(unknown.exists());

        fs::remove_file(&unknown).unwrap();
        let non_regular = buckets.join(".2026-08-30.json.105.5.tmp");
        store.prepare_private_directory(&non_regular).unwrap();
        let error = store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(2, bucket(starts_at, 20))],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(non_regular.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn shard_temp_recovery_refuses_an_exact_temp_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "temp-recovery"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, b"keep me").unwrap();
        let temporary = store
            .source_buckets_directory(&source, RedactionProfile::Redacted)
            .join(".2026-08-30.json.104.4.tmp");
        symlink(&outside, &temporary).unwrap();

        assert_eq!(
            store
                .record_source_bucket_changes(
                    &source,
                    RedactionProfile::Redacted,
                    &[upsert_record(2, bucket(starts_at, 20))],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            temporary
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(outside).unwrap(), b"keep me");
    }

    #[test]
    fn stable_file_identity_requires_both_volume_and_index() {
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
    fn directory_sync_accepts_a_real_directory_and_rejects_a_regular_file() {
        let directory = tempdir().unwrap();
        sync_directory(directory.path()).unwrap();

        let file = directory.path().join("not-a-directory");
        fs::write(&file, b"data").unwrap();
        let error = sync_directory(&file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("must be a real directory"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_sync_rejects_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();

        let error = sync_directory(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("must be a real directory"));
    }

    #[test]
    fn constructing_the_opt_in_store_does_not_create_the_v2_layout() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state-root");
        let store = store(&state_root);

        assert_eq!(store.state_root(), state_root);
        assert_eq!(store.layout_root(), state_root.join("history-v2"));
        assert_eq!(
            store.profile_directory(),
            state_root.join("history-v2").join(PROFILE)
        );
        assert!(!state_root.exists());
    }

    #[test]
    fn same_bucket_start_is_preserved_for_two_sources() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source_a: NodeId = SOURCE_A.parse().unwrap();
        let source_b: NodeId = SOURCE_B.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server a"))
            .unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_B, "server b"))
            .unwrap();
        let starts_at = at(30, 12, 0);

        store
            .record_source_bucket_changes(
                &source_a,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        store
            .record_source_bucket_changes(
                &source_b,
                RedactionProfile::Redacted,
                &[upsert_record(2, bucket(starts_at, 20))],
            )
            .unwrap();

        let loaded_a = store
            .load_source_since(
                &source_a,
                RedactionProfile::Redacted,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        let loaded_b = store
            .load_source_since(
                &source_b,
                RedactionProfile::Redacted,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(loaded_a.buckets[0].token_usage.total_tokens, 10);
        assert_eq!(loaded_b.buckets[0].token_usage.total_tokens, 20);
        assert_ne!(
            store.source_buckets_directory(&source_a, RedactionProfile::Redacted),
            store.source_buckets_directory(&source_b, RedactionProfile::Redacted)
        );
    }

    #[test]
    fn same_source_uses_existing_bucket_evidence_replacement() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(2, bucket(starts_at, 20))],
            )
            .unwrap();

        let loaded = store
            .load_source_since(
                &source,
                RedactionProfile::Redacted,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(loaded.buckets.len(), 1);
        assert_eq!(loaded.buckets[0].token_usage.total_tokens, 20);
    }

    #[test]
    fn revisioned_changes_apply_tombstones_and_reject_equal_conflicts() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        assert!(SourceBucketRecord::tombstone(starts_at, 0).is_err());
        assert!(SourceBucketRecord::tombstone(starts_at + Duration::seconds(1), 1).is_err());

        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        let identical = store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        assert_eq!(identical.shards_skipped, 1);

        let tombstone = SourceBucketRecord::tombstone(starts_at, 2).unwrap();
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                std::slice::from_ref(&tombstone),
            )
            .unwrap();
        let stale = store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(1, bucket(starts_at, 50))],
            )
            .unwrap();
        assert_eq!(stale.shards_skipped, 1);
        assert!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    starts_at - Duration::minutes(1),
                )
                .unwrap()
                .buckets
                .is_empty()
        );
        let records = store
            .load_source_records_since(
                &source,
                RedactionProfile::PreviewEnabled,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(records.records, vec![tombstone.clone()]);

        let unchanged = store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                std::slice::from_ref(&tombstone),
            )
            .unwrap();
        assert_eq!(unchanged.shards_skipped, 1);
        let conflict = upsert_record(2, bucket(starts_at, 99));
        assert_eq!(
            store
                .record_source_bucket_changes(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    &[conflict],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            store
                .load_source_records_since(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    starts_at - Duration::minutes(1),
                )
                .unwrap()
                .records,
            vec![tombstone]
        );

        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(3, bucket(starts_at, 30))],
            )
            .unwrap();
        assert_eq!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    starts_at - Duration::minutes(1),
                )
                .unwrap()
                .buckets[0]
                .token_usage
                .total_tokens,
            30
        );
    }

    #[test]
    fn revisioned_weekly_changes_deduplicate_conflict_and_tombstone() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let observed_at = at(30, 12, 5);
        let resets_at = weekly_point(observed_at, 10).resets_at;
        assert!(SourceWeeklyRecord::tombstone(observed_at, resets_at, 0).is_err());

        let first = SourceWeeklyRecord::upsert(1, weekly_point(observed_at, 10)).unwrap();
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[first.clone(), first.clone()],
            )
            .unwrap();
        assert!(
            store
                .record_source_weekly_changes(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    &[SourceWeeklyRecord::upsert(1, weekly_point(observed_at, 20)).unwrap()],
                )
                .is_err()
        );

        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[SourceWeeklyRecord::upsert(2, weekly_point(observed_at, 20)).unwrap()],
            )
            .unwrap();
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::Redacted,
                &[SourceWeeklyRecord::upsert(1, weekly_point(observed_at, 30)).unwrap()],
            )
            .unwrap();
        let loaded = store
            .load_source_since(
                &source,
                RedactionProfile::PreviewEnabled,
                observed_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(
            loaded.weekly_local_points,
            vec![weekly_point(observed_at, 20)]
        );

        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[SourceWeeklyRecord::tombstone(observed_at, resets_at, 3).unwrap()],
            )
            .unwrap();
        let records = store
            .load_source_records_since(
                &source,
                RedactionProfile::PreviewEnabled,
                observed_at - Duration::minutes(1),
            )
            .unwrap();
        assert!(
            records.weekly_records[0]
                .change()
                .eq(&SourceWeeklyChange::Tombstone)
        );
        assert!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    observed_at - Duration::minutes(1),
                )
                .unwrap()
                .weekly_local_points
                .is_empty()
        );
        assert_eq!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::Redacted,
                    observed_at - Duration::minutes(1),
                )
                .unwrap()
                .weekly_local_points,
            vec![weekly_point(observed_at, 30)]
        );
    }

    #[test]
    fn weekly_key_preserves_two_reset_cycles_at_the_same_observation_time() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let observed_at = at(30, 12, 5);
        let first = weekly_point(observed_at, 10);
        let mut second = weekly_point(observed_at, 20);
        second.resets_at += Duration::days(7);
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[
                    SourceWeeklyRecord::upsert(1, first.clone()).unwrap(),
                    SourceWeeklyRecord::upsert(1, second.clone()).unwrap(),
                ],
            )
            .unwrap();

        assert_eq!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    observed_at - Duration::minutes(1),
                )
                .unwrap()
                .weekly_local_points,
            vec![first.clone(), second.clone()]
        );
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[SourceWeeklyRecord::tombstone(observed_at, first.resets_at, 2).unwrap()],
            )
            .unwrap();
        assert_eq!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    observed_at - Duration::minutes(1),
                )
                .unwrap()
                .weekly_local_points,
            vec![second]
        );
    }

    #[test]
    fn account_and_source_payloads_are_physically_separate() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_account_points(&[quota_point(starts_at)])
            .unwrap();
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();

        let account_path = shard_path(&store.account_directory(), starts_at.date_naive());
        let source_path = shard_path(
            &store.source_buckets_directory(&source, RedactionProfile::Redacted),
            starts_at.date_naive(),
        );
        let account: Value = serde_json::from_slice(&fs::read(account_path).unwrap()).unwrap();
        let source_json: Value = serde_json::from_slice(&fs::read(source_path).unwrap()).unwrap();
        assert!(account.get("quotaPoints").is_some());
        assert!(account.get("buckets").is_none());
        assert!(source_json.get("records").is_some());
        assert!(source_json.get("quotaPoints").is_none());
        assert_eq!(
            store
                .load_account_since(starts_at - Duration::minutes(1))
                .unwrap()
                .quota_points
                .len(),
            1
        );
    }

    #[test]
    fn invalid_account_quota_batch_fails_before_creating_or_writing_storage() {
        let observed_at = at(30, 12, 0);
        let valid = quota_point(observed_at);
        let mut invalid = Vec::new();

        let mut point = valid.clone();
        point.used_percent = f64::NAN;
        invalid.push(point);
        let mut point = valid.clone();
        point.used_percent = -0.1;
        invalid.push(point);
        let mut point = valid.clone();
        point.used_percent = 100.1;
        invalid.push(point);
        let mut point = valid.clone();
        point.remaining_percent = f64::INFINITY;
        invalid.push(point);
        let mut point = valid.clone();
        point.remaining_percent = -0.1;
        invalid.push(point);
        let mut point = valid.clone();
        point.duration_mins = 0;
        invalid.push(point);
        let mut point = valid.clone();
        point.duration_mins = -1;
        invalid.push(point);
        let mut point = valid.clone();
        point.limit_id.clear();
        invalid.push(point);
        let mut point = valid.clone();
        point.limit_id = "   ".to_string();
        invalid.push(point);
        let mut point = valid.clone();
        point.limit_id = " codex".to_string();
        invalid.push(point);
        let mut point = valid.clone();
        point.limit_id = "codex\nsecondary".to_string();
        invalid.push(point);
        let mut point = valid.clone();
        point.limit_id = "x".repeat(MAX_QUOTA_LIMIT_ID_CHARS + 1);
        invalid.push(point);

        for invalid_point in invalid {
            let directory = tempdir().unwrap();
            let store = store(directory.path());
            let error = store
                .record_account_points(&[valid.clone(), invalid_point])
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!store.account_directory().exists());
            assert!(!store.layout_root().exists());
        }
    }

    #[test]
    fn production_writer_is_bound_to_v2_epoch_and_redaction_namespace() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state-root");
        let history = store(&state_root);
        let ownership = HistoryOwnershipStore::new(
            state_root,
            PROFILE.parse().unwrap(),
            RedactionProfile::Redacted,
        );
        let lease = ownership.acquire_writer_lease().unwrap();
        let v1 = match ownership.initialize_v1_active(&lease).unwrap() {
            InitializeV1Outcome::Initialized(manifest)
            | InitializeV1Outcome::Existing(manifest) => manifest,
        };
        let v1_authority = ownership.authorize_v1_write(&lease, &v1).unwrap();
        assert!(history.writer(&v1_authority).is_err());

        let migrating = match ownership.begin_migration(&lease, &v1).unwrap() {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected ownership conflict"),
        };
        let authority = ownership.authorize_v2_write(&lease, &migrating).unwrap();
        let writer = history.writer(&authority).unwrap();
        writer
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        assert!(
            writer
                .record_source_bucket_changes(
                    &SOURCE_A.parse().unwrap(),
                    RedactionProfile::PreviewEnabled,
                    &[],
                )
                .is_err()
        );

        let active = match ownership
            .compare_and_transition(&lease, &migrating, HistoryOwnershipState::V2Active)
            .unwrap()
        {
            OwnershipCasOutcome::Applied(manifest) => manifest,
            OwnershipCasOutcome::Conflict(_) => panic!("unexpected ownership conflict"),
        };
        assert_eq!(
            writer.validate().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        history
            .writer(&ownership.authorize_v2_write(&lease, &active).unwrap())
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn envelope_mismatches_are_rejected() {
        let fields = [
            ("profileId", Value::String("other-profile".to_string())),
            ("sourceId", Value::String(SOURCE_B.to_string())),
            (
                "redactionProfile",
                Value::String("preview-enabled".to_string()),
            ),
            ("utcDay", Value::String("2026-08-29".to_string())),
            (
                "metricRevision",
                Value::from(u64::from(HISTORY_METRIC_REVISION) + 1),
            ),
        ];
        for (field, replacement) in fields {
            let directory = tempdir().unwrap();
            let store = store(directory.path());
            let source: NodeId = SOURCE_A.parse().unwrap();
            store
                .save_source_metadata(&metadata(SOURCE_A, "server"))
                .unwrap();
            let starts_at = at(30, 12, 0);
            store
                .record_source_bucket_changes(
                    &source,
                    RedactionProfile::Redacted,
                    &[upsert_record(1, bucket(starts_at, 10))],
                )
                .unwrap();
            let path = shard_path(
                &store.source_buckets_directory(&source, RedactionProfile::Redacted),
                starts_at.date_naive(),
            );
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value[field] = replacement;
            write_private_atomically(&path, &encode_pretty(&value).unwrap()).unwrap();

            let error = store
                .load_source_since(
                    &source,
                    RedactionProfile::Redacted,
                    starts_at - Duration::minutes(1),
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "field={field}");
        }
    }

    #[test]
    fn account_envelope_mismatches_are_rejected() {
        let fields = [
            ("profileId", Value::String("other-profile".to_string())),
            ("utcDay", Value::String("2026-08-29".to_string())),
            (
                "quotaRevision",
                Value::from(u64::from(ACCOUNT_QUOTA_REVISION) + 1),
            ),
        ];
        for (field, replacement) in fields {
            let directory = tempdir().unwrap();
            let store = store(directory.path());
            let observed_at = at(30, 12, 0);
            store
                .record_account_points(&[quota_point(observed_at)])
                .unwrap();
            let path = shard_path(&store.account_directory(), observed_at.date_naive());
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value[field] = replacement;
            write_private_atomically(&path, &encode_pretty(&value).unwrap()).unwrap();

            let error = store
                .load_account_since(observed_at - Duration::minutes(1))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "field={field}");
        }
    }

    #[test]
    fn invalid_account_quota_payload_is_rejected_on_read() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let observed_at = at(30, 12, 0);
        store
            .record_account_points(&[quota_point(observed_at)])
            .unwrap();
        let path = shard_path(&store.account_directory(), observed_at.date_naive());
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["quotaPoints"][0]["usedPercent"] = Value::from(101.0);
        write_private_atomically(&path, &encode_pretty(&value).unwrap()).unwrap();

        let error = store
            .load_account_since(observed_at - Duration::minutes(1))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn redaction_profiles_are_strictly_isolated() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(2, bucket(starts_at, 20))],
            )
            .unwrap();

        let redacted = store
            .load_source_since(
                &source,
                RedactionProfile::Redacted,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        let preview = store
            .load_source_since(
                &source,
                RedactionProfile::PreviewEnabled,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(redacted.buckets[0].token_usage.total_tokens, 10);
        assert_eq!(preview.buckets[0].token_usage.total_tokens, 20);
    }

    #[test]
    fn redacted_storage_scrubs_content_before_write_and_defensively_on_read() {
        const SECRET_TITLE: &str = "private customer migration";
        const SECRET_MESSAGE: &str = "rotate production credential alpha";

        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(
                    1,
                    bucket_with_content(starts_at, SECRET_TITLE, SECRET_MESSAGE),
                )],
            )
            .unwrap();

        let path = shard_path(
            &store.source_buckets_directory(&source, RedactionProfile::Redacted),
            starts_at.date_naive(),
        );
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains(SECRET_TITLE));
        assert!(!contents.contains(SECRET_MESSAGE));
        assert!(contents.contains("[redacted]"));

        // Simulate a pre-boundary or externally modified shard. Loading a
        // redacted namespace must still never return its plaintext fields.
        let mut value: Value = serde_json::from_str(&contents).unwrap();
        value["records"][0]["change"]["upsert"]["projectGroups"][0]["title"] =
            Value::String(SECRET_TITLE.to_string());
        value["records"][0]["change"]["upsert"]["projectGroups"][0]["messagePreview"] =
            Value::String(SECRET_MESSAGE.to_string());
        write_private_atomically(&path, &encode_pretty(&value).unwrap()).unwrap();

        let loaded = store
            .load_source_since(
                &source,
                RedactionProfile::Redacted,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        let group = &loaded.buckets[0].project_groups[0];
        assert_eq!(group.title.as_deref(), Some("[redacted]"));
        assert_eq!(group.message_preview.as_deref(), Some("[redacted]"));
    }

    #[test]
    fn preview_enabled_storage_preserves_content() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(
                    1,
                    bucket_with_content(starts_at, "visible title", "visible request"),
                )],
            )
            .unwrap();

        let loaded = store
            .load_source_since(
                &source,
                RedactionProfile::PreviewEnabled,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        let group = &loaded.buckets[0].project_groups[0];
        assert_eq!(group.title.as_deref(), Some("visible title"));
        assert_eq!(group.message_preview.as_deref(), Some("visible request"));
    }

    #[test]
    fn old_and_future_bucket_revisions_preserve_token_history() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();

        let mut old = bucket(at(30, 12, 0), 111);
        old.estimator_revision = HISTORY_ESTIMATOR_REVISION.saturating_sub(1);
        old.project_breakdown_revision = HISTORY_PROJECT_BREAKDOWN_REVISION.saturating_sub(1);
        old.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION.saturating_sub(1);
        let mut future = bucket(at(30, 12, 15), 222);
        future.estimator_revision = HISTORY_ESTIMATOR_REVISION.saturating_add(100);
        future.project_breakdown_revision = HISTORY_PROJECT_BREAKDOWN_REVISION.saturating_add(100);
        future.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION.saturating_add(100);

        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[
                    upsert_record(1, old.clone()),
                    upsert_record(1, future.clone()),
                ],
            )
            .unwrap();
        let loaded = store
            .load_source_since(
                &source,
                RedactionProfile::PreviewEnabled,
                old.starts_at - Duration::minutes(1),
            )
            .unwrap();

        assert_eq!(loaded.buckets.len(), 2);
        assert_eq!(loaded.buckets[0].token_usage.total_tokens, 111);
        assert_eq!(loaded.buckets[0].estimator_revision, old.estimator_revision);
        assert_eq!(
            loaded.buckets[0].api_pricing_catalog_revision,
            old.api_pricing_catalog_revision
        );
        assert_eq!(loaded.buckets[1].token_usage.total_tokens, 222);
        assert_eq!(
            loaded.buckets[1].estimator_revision,
            future.estimator_revision
        );
        assert_eq!(
            loaded.buckets[1].project_breakdown_revision,
            future.project_breakdown_revision
        );
    }

    #[test]
    fn include_and_detached_flags_remain_independent() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();

        for (include, detached) in [(true, false), (true, true), (false, true), (false, false)] {
            store
                .update_source_metadata(&source, |source_metadata| {
                    source_metadata.set_include_in_aggregates(include);
                    source_metadata.set_detached(detached);
                    Ok(())
                })
                .unwrap();
            let loaded = store.load_source_metadata(&source).unwrap();
            assert_eq!(loaded.include_in_aggregates(), include);
            assert_eq!(loaded.detached(), detached);
        }
    }

    #[test]
    fn transactional_metadata_updates_do_not_lose_independent_changes() {
        let directory = tempdir().unwrap();
        let store = Arc::new(store(directory.path()));
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let include_worker = {
            let store = Arc::clone(&store);
            let source = source.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                store
                    .update_source_metadata(&source, |metadata| {
                        metadata.set_include_in_aggregates(false);
                        Ok(())
                    })
                    .unwrap();
            })
        };
        let detached_worker = {
            let store = Arc::clone(&store);
            let source = source.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                store
                    .update_source_metadata(&source, |metadata| {
                        metadata.set_detached(true);
                        Ok(())
                    })
                    .unwrap();
            })
        };
        barrier.wait();
        include_worker.join().unwrap();
        detached_worker.join().unwrap();

        let loaded = store.load_source_metadata(&source).unwrap();
        assert!(!loaded.include_in_aggregates());
        assert!(loaded.detached());
    }

    #[test]
    fn whole_record_metadata_save_cannot_overwrite_a_newer_update() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let mut stale = store.load_source_metadata(&source).unwrap();
        store
            .update_source_metadata(&source, |metadata| metadata.set_display_label("renamed"))
            .unwrap();
        stale.set_detached(true);

        assert_eq!(
            store.save_source_metadata(&stale).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let loaded = store.load_source_metadata(&source).unwrap();
        assert_eq!(loaded.display_label(), "renamed");
        assert!(!loaded.detached());
    }

    #[test]
    fn included_source_query_keeps_sources_separate_and_detach_independent() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source_a: NodeId = SOURCE_A.parse().unwrap();
        let source_b: NodeId = SOURCE_B.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server a"))
            .unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_B, "server b"))
            .unwrap();
        store
            .update_source_metadata(&source_a, |metadata| {
                metadata.set_detached(true);
                Ok(())
            })
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source_a,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        store
            .record_source_bucket_changes(
                &source_b,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(1, bucket(starts_at, 20))],
            )
            .unwrap();
        write_private_atomically(&store.sources_directory().join("README"), b"ignored\n").unwrap();

        let included = store
            .load_included_sources_since(
                RedactionProfile::PreviewEnabled,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(included.len(), 2);
        assert!(included[0].source.detached());
        assert_eq!(included[0].buckets[0].starts_at, starts_at);
        assert_eq!(included[1].buckets[0].starts_at, starts_at);
        assert_ne!(
            included[0].source.source_id(),
            included[1].source.source_id()
        );

        store
            .update_source_metadata(&source_b, |metadata| {
                metadata.set_include_in_aggregates(false);
                Ok(())
            })
            .unwrap();
        let included = store
            .load_included_sources_since(
                RedactionProfile::PreviewEnabled,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(included.len(), 1);
        assert_eq!(included[0].source.source_id(), &source_a);
        assert!(included[0].source.detached());

        create_private_directory(&store.sources_directory().join("node-malformed")).unwrap();
        assert_eq!(
            store.list_source_metadata().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn retention_prunes_complete_old_shards_and_defers_far_future_clock_jump() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        let now = at(30, 12, 0);
        let initial = store.garbage_collect(now).unwrap();
        assert_eq!(initial.shards_pruned, 0);
        assert!(!initial.pruning_deferred);
        assert_eq!(initial.trusted_at, Some(now));

        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let old = now - Duration::days(SOURCE_HISTORY_RETENTION_DAYS + 5);
        let recent = now - Duration::days(1);
        store
            .record_account_points(&[quota_point(old), quota_point(recent)])
            .unwrap();
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[
                    upsert_record(1, bucket(old, 10)),
                    upsert_record(1, bucket(recent, 20)),
                ],
            )
            .unwrap();
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[
                    SourceWeeklyRecord::upsert(1, weekly_point(old, 10)).unwrap(),
                    SourceWeeklyRecord::upsert(1, weekly_point(recent, 20)).unwrap(),
                ],
            )
            .unwrap();

        let pruned = store.garbage_collect(now).unwrap();
        assert_eq!(pruned.shards_pruned, 3);
        assert!(!pruned.pruning_deferred);
        assert_eq!(
            store
                .load_account_since(old - Duration::minutes(1))
                .unwrap()
                .quota_points
                .len(),
            1
        );
        let retained_source = store
            .load_source_since(
                &source,
                RedactionProfile::PreviewEnabled,
                old - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(retained_source.buckets.len(), 1);
        assert_eq!(retained_source.weekly_local_points.len(), 1);

        let future = now + Duration::days(365);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(1, bucket(future, 30))],
            )
            .unwrap();
        let deferred = store.garbage_collect(future).unwrap();
        assert!(deferred.pruning_deferred);
        assert_eq!(deferred.trusted_at, Some(now));
        assert_eq!(deferred.shards_pruned, 0);
        let retained = store
            .load_source_since(
                &source,
                RedactionProfile::PreviewEnabled,
                recent - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(retained.buckets.len(), 2);
        assert_eq!(retained.buckets[0].starts_at, recent);
        assert_eq!(retained.buckets[1].starts_at, future);
    }

    #[test]
    fn scheduled_retention_is_persistently_throttled_across_store_restarts() {
        let directory = tempdir().unwrap();
        let now = at(30, 12, 0);
        let interval = StdDuration::from_secs(6 * 60 * 60);
        let store = store(directory.path());

        let first = store
            .garbage_collect_if_due(now, interval, RedactionProfile::PreviewEnabled)
            .unwrap();
        assert!(first.is_some());
        assert!(
            store
                .garbage_collect_if_due(
                    now + Duration::hours(5),
                    interval,
                    RedactionProfile::PreviewEnabled,
                )
                .unwrap()
                .is_none()
        );

        // A fresh store has no process-local memory, so this specifically
        // proves that short-lived CLI invocations still honor the marker.
        let restarted = self::store(directory.path());
        assert!(
            restarted
                .garbage_collect_if_due(
                    now + Duration::hours(5),
                    interval,
                    RedactionProfile::PreviewEnabled,
                )
                .unwrap()
                .is_none()
        );
        assert!(
            restarted
                .garbage_collect_if_due(
                    now + Duration::hours(6),
                    interval,
                    RedactionProfile::PreviewEnabled,
                )
                .unwrap()
                .is_some()
        );

        // A rollback gets one pass to rebase the throttle instead of waiting
        // for the future timestamp to arrive again.
        assert!(
            restarted
                .garbage_collect_if_due(now, interval, RedactionProfile::PreviewEnabled)
                .unwrap()
                .is_some()
        );
        assert!(
            restarted
                .garbage_collect_if_due(
                    now + Duration::hours(1),
                    interval,
                    RedactionProfile::PreviewEnabled,
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn scheduled_retention_prunes_account_and_every_source_only_in_its_redaction() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let local: NodeId = SOURCE_A.parse().unwrap();
        let remote: NodeId = SOURCE_B.parse().unwrap();
        let now = at(30, 12, 0);
        let old = now - Duration::days(SOURCE_HISTORY_RETENTION_DAYS + 5);
        let interval = StdDuration::from_secs(6 * 60 * 60);

        store
            .garbage_collect_if_due(now, interval, RedactionProfile::PreviewEnabled)
            .unwrap()
            .unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "local"))
            .unwrap();
        store
            .save_source_metadata(
                &SourceMetadata::new(remote.clone(), SourceKind::Ssh, "remote").unwrap(),
            )
            .unwrap();
        store.record_account_points(&[quota_point(old)]).unwrap();
        for source in [&local, &remote] {
            for profile in [RedactionProfile::PreviewEnabled, RedactionProfile::Redacted] {
                store
                    .record_source_bucket_changes(
                        source,
                        profile,
                        &[upsert_record(1, bucket(old, 10))],
                    )
                    .unwrap();
                store
                    .record_source_weekly_changes(
                        source,
                        profile,
                        &[SourceWeeklyRecord::upsert(1, weekly_point(old, 10)).unwrap()],
                    )
                    .unwrap();
            }
        }

        let report = store
            .garbage_collect_if_due(
                now + Duration::hours(6),
                interval,
                RedactionProfile::PreviewEnabled,
            )
            .unwrap()
            .unwrap();
        assert_eq!(report.shards_pruned, 5);
        assert!(
            store
                .load_account_since(old - Duration::minutes(1))
                .unwrap()
                .quota_points
                .is_empty()
        );
        for source in [&local, &remote] {
            assert!(
                !shard_path(
                    &store.source_buckets_directory(source, RedactionProfile::PreviewEnabled,),
                    old.date_naive(),
                )
                .exists()
            );
            assert!(
                !shard_path(
                    &store.source_weekly_directory(source, RedactionProfile::PreviewEnabled),
                    old.date_naive(),
                )
                .exists()
            );
            assert!(
                shard_path(
                    &store.source_buckets_directory(source, RedactionProfile::Redacted),
                    old.date_naive(),
                )
                .exists()
            );
            assert!(
                shard_path(
                    &store.source_weekly_directory(source, RedactionProfile::Redacted),
                    old.date_naive(),
                )
                .exists()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn scheduled_retention_does_not_traverse_the_other_redaction_namespace() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "local"))
            .unwrap();
        let outside = directory.path().join("outside-redacted");
        fs::create_dir(&outside).unwrap();
        let redacted_parent = store
            .source_directory(&source)
            .join(RedactionProfile::Redacted.directory_name());
        store
            .prepare_private_directory(redacted_parent.parent().unwrap())
            .unwrap();
        symlink(&outside, &redacted_parent).unwrap();

        let report = store
            .garbage_collect_if_due(
                at(30, 12, 0),
                StdDuration::from_secs(6 * 60 * 60),
                RedactionProfile::PreviewEnabled,
            )
            .unwrap()
            .unwrap();
        assert_eq!(report.shards_pruned, 0);
        assert!(outside.exists());
    }

    #[test]
    fn failed_scheduled_retention_is_throttled_after_restart() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        let now = at(30, 12, 0);
        let interval = StdDuration::from_secs(6 * 60 * 60);
        store
            .garbage_collect_if_due(now, interval, RedactionProfile::PreviewEnabled)
            .unwrap()
            .unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "local"))
            .unwrap();

        let old = now - Duration::days(SOURCE_HISTORY_RETENTION_DAYS + 5);
        let bucket_directory =
            store.source_buckets_directory(&source, RedactionProfile::PreviewEnabled);
        store.prepare_private_directory(&bucket_directory).unwrap();
        write_private_atomically(
            &shard_path(&bucket_directory, old.date_naive()),
            b"not json\n",
        )
        .unwrap();

        let error = store
            .garbage_collect_if_due(
                now + Duration::hours(6),
                interval,
                RedactionProfile::PreviewEnabled,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let restarted = self::store(directory.path());
        assert!(
            restarted
                .garbage_collect_if_due(
                    now + Duration::hours(6) + Duration::minutes(1),
                    interval,
                    RedactionProfile::PreviewEnabled,
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn scheduled_retention_preserves_forward_clock_jump_confirmation() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let now = at(30, 12, 0);
        let vulnerable = now - Duration::days(SOURCE_HISTORY_RETENTION_DAYS - 1);
        let interval = StdDuration::from_secs(6 * 60 * 60);
        store
            .garbage_collect_if_due(now, interval, RedactionProfile::PreviewEnabled)
            .unwrap()
            .unwrap();
        store
            .record_account_points(&[quota_point(vulnerable)])
            .unwrap();

        let jumped = now + Duration::days(30);
        let first = store
            .garbage_collect_if_due(jumped, interval, RedactionProfile::PreviewEnabled)
            .unwrap()
            .unwrap();
        assert!(first.pruning_deferred);
        assert_eq!(first.shards_pruned, 0);
        let second = store
            .garbage_collect_if_due(
                jumped + Duration::hours(24),
                interval,
                RedactionProfile::PreviewEnabled,
            )
            .unwrap()
            .unwrap();
        assert!(second.pruning_deferred);
        assert_eq!(second.shards_pruned, 0);
        let confirmed = store
            .garbage_collect_if_due(
                jumped + Duration::hours(48),
                interval,
                RedactionProfile::PreviewEnabled,
            )
            .unwrap()
            .unwrap();
        assert!(!confirmed.pruning_deferred);
        assert_eq!(confirmed.shards_pruned, 1);
    }

    #[cfg(unix)]
    #[test]
    fn retention_never_follows_a_redaction_ancestor_symlink_for_deletion() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let store = store(&state_root);
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let old = at(1, 0, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[upsert_record(1, bucket(old, 10))],
            )
            .unwrap();
        store
            .record_source_weekly_changes(
                &source,
                RedactionProfile::PreviewEnabled,
                &[SourceWeeklyRecord::upsert(1, weekly_point(old, 10)).unwrap()],
            )
            .unwrap();

        let redaction_directory = store
            .source_directory(&source)
            .join(RedactionProfile::PreviewEnabled.directory_name());
        let outside = directory.path().join("outside-redaction");
        fs::rename(&redaction_directory, &outside).unwrap();
        symlink(&outside, &redaction_directory).unwrap();
        let bucket_shard = shard_path(&outside.join(BUCKETS_DIRECTORY), old.date_naive());
        let weekly_shard = shard_path(&outside.join(WEEKLY_DIRECTORY), old.date_naive());

        let bucket_error = prune_source_shards(
            &store,
            &store.source_buckets_directory(&source, RedactionProfile::PreviewEnabled),
            store.profile_id(),
            &source,
            RedactionProfile::PreviewEnabled,
            at(30, 0, 0).date_naive(),
        )
        .unwrap_err();
        assert_eq!(bucket_error.kind(), io::ErrorKind::InvalidData);
        assert!(bucket_shard.exists());

        let weekly_error = prune_source_weekly_shards(
            &store,
            &store.source_weekly_directory(&source, RedactionProfile::PreviewEnabled),
            store.profile_id(),
            &source,
            RedactionProfile::PreviewEnabled,
            at(30, 0, 0).date_naive(),
        )
        .unwrap_err();
        assert_eq!(weekly_error.kind(), io::ErrorKind::InvalidData);
        assert!(weekly_shard.exists());
    }

    #[test]
    fn retention_requires_confirmation_before_two_or_thirty_day_forward_jumps_prune() {
        let now = at(30, 12, 0);
        let vulnerable_point = now - Duration::days(SOURCE_HISTORY_RETENTION_DAYS - 1);

        for jump_days in [2, 30] {
            let directory = tempdir().unwrap();
            let store = store(directory.path());
            let initial = store.garbage_collect(now).unwrap();
            assert!(!initial.pruning_deferred, "jump={jump_days}d");
            assert_eq!(initial.trusted_at, Some(now), "jump={jump_days}d");
            store
                .record_account_points(&[quota_point(vulnerable_point)])
                .unwrap();

            let first_observation = now + Duration::days(jump_days);
            let first = store.garbage_collect(first_observation).unwrap();
            assert!(first.pruning_deferred, "jump={jump_days}d");
            assert_eq!(first.trusted_at, Some(now), "jump={jump_days}d");
            assert_eq!(first.shards_pruned, 0, "jump={jump_days}d");
            assert_eq!(
                store
                    .load_account_since(vulnerable_point - Duration::minutes(1))
                    .unwrap()
                    .quota_points
                    .len(),
                1,
                "jump={jump_days}d"
            );

            let second = store
                .garbage_collect(first_observation + Duration::hours(24))
                .unwrap();
            assert!(second.pruning_deferred, "jump={jump_days}d");
            assert_eq!(second.trusted_at, Some(now), "jump={jump_days}d");
            assert_eq!(second.shards_pruned, 0, "jump={jump_days}d");

            let confirmed_at = first_observation + Duration::hours(48);
            let confirmed = store.garbage_collect(confirmed_at).unwrap();
            assert!(!confirmed.pruning_deferred, "jump={jump_days}d");
            assert_eq!(
                confirmed.trusted_at,
                Some(confirmed_at),
                "jump={jump_days}d"
            );
            assert_eq!(confirmed.shards_pruned, 1, "jump={jump_days}d");
            assert!(
                store
                    .load_account_since(vulnerable_point - Duration::minutes(1))
                    .unwrap()
                    .quota_points
                    .is_empty(),
                "jump={jump_days}d"
            );
        }
    }

    #[test]
    fn source_metadata_rejects_kind_changes_and_invalid_labels() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let remote = SourceMetadata::new(source, SourceKind::Ssh, "remote").unwrap();
        assert_eq!(
            store.save_source_metadata(&remote).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(SourceMetadata::new(SOURCE_B.parse().unwrap(), SourceKind::Ssh, "\n").is_err());
        assert!(
            SourceMetadata::new(SOURCE_B.parse().unwrap(), SourceKind::Ssh, " server").is_err()
        );
        assert!(
            SourceMetadata::new(SOURCE_B.parse().unwrap(), SourceKind::Ssh, "server ").is_err()
        );
        assert!(
            SourceMetadata::new(
                SOURCE_B.parse().unwrap(),
                SourceKind::Ssh,
                format!("server{}", " ".repeat(MAX_SOURCE_LABEL_CHARS + 1)),
            )
            .is_err()
        );
        assert!(
            SourceMetadata::new(
                SOURCE_B.parse().unwrap(),
                SourceKind::Ssh,
                "a".repeat(MAX_SOURCE_LABEL_CHARS + 1),
            )
            .is_err()
        );
        assert!(
            SourceMetadata::new(
                SOURCE_B.parse().unwrap(),
                SourceKind::Ssh,
                "😀".repeat((MAX_SOURCE_LABEL_BYTES / 4) + 1),
            )
            .is_err()
        );
        assert!(
            SourceMetadata::new(
                SOURCE_B.parse().unwrap(),
                SourceKind::Ssh,
                "line\u{2028}break"
            )
            .is_err()
        );
        assert!(
            SourceMetadata::new(
                SOURCE_B.parse().unwrap(),
                SourceKind::Ssh,
                "safe\u{202e}unsafe"
            )
            .is_err()
        );
    }

    #[test]
    fn source_metadata_envelope_is_bound_to_profile_source_and_revision() {
        let replacements = [
            (vec!["formatVersion"], Value::from(2_u64)),
            (
                vec!["profileId"],
                Value::String("other-profile".to_string()),
            ),
            (
                vec!["source", "sourceId"],
                Value::String(SOURCE_B.to_string()),
            ),
            (
                vec!["source", "schemaVersion"],
                Value::from(u64::from(SOURCE_METADATA_VERSION) + 1),
            ),
        ];

        for (path_components, replacement) in replacements {
            let directory = tempdir().unwrap();
            let store = store(directory.path());
            let source: NodeId = SOURCE_A.parse().unwrap();
            store
                .save_source_metadata(&metadata(SOURCE_A, "server"))
                .unwrap();
            let path = store.source_directory(&source).join(SOURCE_METADATA_FILE);
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let mut cursor = &mut value;
            for component in &path_components[..path_components.len() - 1] {
                cursor = &mut cursor[*component];
            }
            cursor[path_components[path_components.len() - 1]] = replacement;
            write_private_atomically(&path, &encode_pretty(&value).unwrap()).unwrap();

            assert_eq!(
                store.load_source_metadata(&source).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn rejects_unaligned_source_buckets_before_writing() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        let mut invalid = bucket(starts_at, 10);
        invalid.ends_at += Duration::minutes(15);
        assert_eq!(
            SourceBucketRecord::upsert(1, invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            !store
                .source_buckets_directory(&source, RedactionProfile::Redacted)
                .join(format!("{}.json", starts_at.format("%Y-%m-%d")))
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn created_directories_shards_metadata_and_locks_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let root = directory.path().join("history");
        let store = store(&root);
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_account_points(&[quota_point(starts_at)])
            .unwrap();
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();

        for path in [
            store.layout_root(),
            store.profile_directory(),
            store.account_directory(),
            store.source_directory(&source),
            store.source_buckets_directory(&source, RedactionProfile::Redacted),
        ] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
        for path in [
            store.source_directory(&source).join(SOURCE_METADATA_FILE),
            store.source_directory(&source).join(SOURCE_LOCK_FILE),
            store.account_directory().join(ACCOUNT_LOCK_FILE),
            shard_path(&store.account_directory(), starts_at.date_naive()),
            store
                .source_buckets_directory(&source, RedactionProfile::Redacted)
                .join(BUCKETS_LOCK_FILE),
            shard_path(
                &store.source_buckets_directory(&source, RedactionProfile::Redacted),
                starts_at.date_naive(),
            ),
        ] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_public_directory_and_file_permissions_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let history_store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        history_store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        history_store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();

        let shard = shard_path(
            &history_store.source_buckets_directory(&source, RedactionProfile::Redacted),
            starts_at.date_naive(),
        );
        fs::set_permissions(&shard, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            history_store
                .load_source_since(
                    &source,
                    RedactionProfile::Redacted,
                    starts_at - Duration::minutes(1),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        let other_root = directory.path().join("other-history");
        let other_store = store(&other_root);
        let account = other_store.account_directory();
        fs::create_dir_all(&account).unwrap();
        fs::set_permissions(&account, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            other_store
                .record_account_points(&[quota_point(starts_at)])
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_shard_and_lock_files_fail_closed() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let source: NodeId = SOURCE_A.parse().unwrap();
        store
            .save_source_metadata(&metadata(SOURCE_A, "server"))
            .unwrap();
        let starts_at = at(30, 12, 0);
        store
            .record_source_bucket_changes(
                &source,
                RedactionProfile::Redacted,
                &[upsert_record(1, bucket(starts_at, 10))],
            )
            .unwrap();
        let buckets_directory = store.source_buckets_directory(&source, RedactionProfile::Redacted);
        let shard = shard_path(&buckets_directory, starts_at.date_naive());
        let shard_target = buckets_directory.join("target");
        fs::rename(&shard, &shard_target).unwrap();
        symlink(&shard_target, &shard).unwrap();

        assert_eq!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::Redacted,
                    starts_at - Duration::minutes(1),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        fs::remove_file(&shard).unwrap();
        fs::rename(&shard_target, &shard).unwrap();
        let lock = buckets_directory.join(BUCKETS_LOCK_FILE);
        let lock_target = buckets_directory.join("lock-target");
        fs::rename(&lock, &lock_target).unwrap();
        symlink(&lock_target, &lock).unwrap();
        assert_eq!(
            store
                .load_source_since(
                    &source,
                    RedactionProfile::Redacted,
                    starts_at - Duration::minutes(1),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn waiter_rejects_a_lock_inode_replaced_while_blocked() {
        use std::sync::mpsc;
        use std::time::Duration as StdDuration;

        for exclusive_waiter in [false, true] {
            let directory = tempdir().unwrap();
            let store = store(directory.path());
            let observed_at = at(30, 12, 0);
            store
                .record_account_points(&[quota_point(observed_at)])
                .unwrap();
            let account_directory = store.account_directory();

            let holder = open_lock_file(&account_directory, ACCOUNT_LOCK_FILE).unwrap();
            fs2::FileExt::lock_exclusive(&holder).unwrap();

            let (opened_sender, opened_receiver) = mpsc::channel();
            let waiter_directory = account_directory.clone();
            let waiter = thread::spawn(move || {
                let opened = open_lock_file(&waiter_directory, ACCOUNT_LOCK_FILE).unwrap();
                opened_sender.send(()).unwrap();
                if exclusive_waiter {
                    lock_exclusive(&opened, &waiter_directory, ACCOUNT_LOCK_FILE)
                } else {
                    lock_shared(&opened, &waiter_directory, ACCOUNT_LOCK_FILE)
                }
            });
            opened_receiver
                .recv_timeout(StdDuration::from_secs(5))
                .expect("waiter did not open the original lock inode");

            let lock_path = account_directory.join(ACCOUNT_LOCK_FILE);
            fs::rename(&lock_path, account_directory.join("displaced-account.lock")).unwrap();
            write_private_atomically(&lock_path, b"").unwrap();
            drop(holder);

            let error = waiter.join().unwrap().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("changed while"));

            assert_eq!(
                store
                    .load_account_since(observed_at - Duration::minutes(1))
                    .unwrap()
                    .quota_points
                    .len(),
                1
            );
        }
    }

    #[test]
    fn oversized_regular_shard_is_rejected_before_parsing() {
        let directory = tempdir().unwrap();
        let store = store(directory.path());
        let observed_at = at(30, 12, 0);
        store
            .record_account_points(&[quota_point(observed_at)])
            .unwrap();
        let path = shard_path(&store.account_directory(), observed_at.date_naive());
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SHARD_FILE_BYTES + 1)
            .unwrap();

        assert_eq!(
            store
                .load_account_since(observed_at - Duration::minutes(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn source_payload_does_not_claim_api_cost_data_it_does_not_store() {
        // Guard against accidentally adding account/API aggregate fields to
        // the source shard while its contract is intentionally bucket-only.
        let shard = SourceBucketShard::new(
            PROFILE.parse().unwrap(),
            SOURCE_A.parse().unwrap(),
            RedactionProfile::Redacted,
            at(30, 12, 0).date_naive(),
        );
        let value = serde_json::to_value(shard).unwrap();
        assert!(value.get("quotaPoints").is_none());
        assert!(value.get("weeklyLocalPoints").is_none());
        assert!(value.get("estimatorRevision").is_none());
        assert!(value.get("projectBreakdownRevision").is_none());
        assert!(value.get("apiPricingCatalogRevision").is_none());
    }

    #[test]
    fn windows_reparse_policy_and_stable_lock_sharing_fail_closed() {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        const FILE_SHARE_DELETE: u32 = 0x4;

        assert!(windows_attributes_are_reparse(
            FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(!windows_attributes_are_reparse(
            0,
            FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert_eq!(stable_lock_share_mode_for_test() & FILE_SHARE_DELETE, 0);
    }

    #[cfg(unix)]
    #[test]
    fn anchored_directory_creation_rejects_symlinked_existing_ancestor() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        create_private_directory(&state_root).unwrap();
        let outside = directory.path().join("outside");
        create_private_directory(&outside).unwrap();
        symlink(&outside, state_root.join(LAYOUT_DIRECTORY)).unwrap();
        let store = SourceHistoryStore::new(state_root, PROFILE.parse().unwrap());

        let error = store
            .prepare_private_directory(&store.account_directory())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
