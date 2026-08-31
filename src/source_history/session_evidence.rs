use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Deserializer, Serialize};

use super::*;
use crate::domain::{ApiCostAmount, TokenUsage};
use crate::source_model::{ObservedProjectKey, SessionReplicaKey, ThreadId, ThreadShardKey};

pub(super) const DIGESTS_DIRECTORY: &str = "digests";
const FACTS_DIRECTORY: &str = "facts";
const FACT_MANIFESTS_DIRECTORY: &str = "fact-manifests";
const FACT_STAGING_DIRECTORY: &str = "fact-staging";
pub(super) const DIGESTS_LOCK_FILE: &str = "digests.lock";
const FACT_STAGING_LOCK_FILE: &str = "fact-staging.lock";
const STAGED_BATCH_FILE: &str = "batch.json";
const STAGED_GENERATION_DIRECTORY: &str = "generation";
const STAGED_PUBLICATION_FILE: &str = "publication.json";
const SESSION_DIGEST_SHARD_FORMAT_VERSION: u32 = 3;
const FACT_SHARD_FORMAT_VERSION: u32 = 2;
const FACT_BATCH_FORMAT_VERSION: u32 = 5;
const FACT_MANIFEST_FORMAT_VERSION: u32 = 4;
const SESSION_EVIDENCE_METRIC_REVISION: u32 = 1;
const MAX_EVIDENCE_SHARD_BYTES: u64 = 128 * 1024 * 1024;
pub(super) const MAX_COMPRESSED_EVIDENCE_SHARD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FACT_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_FACT_BATCH_CHANGES: usize = 250_000;
const MAX_FACT_GENERATION_RECORDS: usize = 1_000_000;
const MAX_FACT_RETENTION_DAYS: i64 = 35;
const MAX_FACT_GENERATION_DECODED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FACT_NAMESPACE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FACT_NAMESPACE_ENTRIES: u64 = 250_000;
const MAX_USAGE_EVENT_ID_BYTES: usize = 256;
const MAX_TURN_ID_BYTES: usize = 256;
const MAX_MODEL_BYTES: usize = 256;
const MAX_SERVICE_TIER_BYTES: usize = 64;
const MAX_PARTIAL_REASON_BYTES: usize = 160;
const MAX_PARTIAL_REASONS: usize = 128;
const MAX_FACT_DIGEST_BINDINGS: usize = 36;
const DIGEST_FINGERPRINT_PREFIX: &str = "session-digest-sha256-v1-";
const FACT_BATCH_ID_PREFIX: &str = "fact-batch-";
const DIGEST_FINGERPRINT_HEX_LEN: usize = 64;
const FACT_BATCH_RANDOM_BYTES: usize = 16;
const FACT_STAGING_TTL_HOURS: i64 = 24;

/// Content-free fingerprint used to identify matching session evidence.
///
/// The exporter owns the canonical digest input. Persistence deliberately only
/// accepts a fixed SHA-256 representation, so raw messages can never be
/// smuggled into the replica-detection index.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SessionDigestFingerprint(String);

impl SessionDigestFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionDigestFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SessionDigestFingerprint {
    type Err = SessionEvidenceIdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_lower_hex(value, DIGEST_FINGERPRINT_PREFIX, DIGEST_FINGERPRINT_HEX_LEN)?;
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for SessionDigestFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Stable, source-independent identity of one normalized usage event.
///
/// This value is protocol data, never a path component. Exporters may retain a
/// native stable call/event ID or a deterministic fallback ID, but must expose
/// ambiguity through `exact_event_identity` and partial reasons.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct UsageEventId(String);

impl UsageEventId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UsageEventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for UsageEventId {
    type Err = SessionEvidenceIdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_opaque_id(value, MAX_USAGE_EVENT_ID_BYTES, "usage event ID")?;
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for UsageEventId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Random path-safe identity for one complete staged fact batch.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct FactBatchId(String);

impl FactBatchId {
    pub fn generate() -> io::Result<Self> {
        let mut random = [0_u8; FACT_BATCH_RANDOM_BYTES];
        getrandom::fill(&mut random).map_err(|error| {
            io::Error::other(format!("could not generate fact batch ID: {error}"))
        })?;
        if random.iter().all(|byte| *byte == 0) {
            return Err(io::Error::other(
                "secure random provider returned an unusable batch ID",
            ));
        }
        let mut value = String::with_capacity(FACT_BATCH_ID_PREFIX.len() + random.len() * 2);
        value.push_str(FACT_BATCH_ID_PREFIX);
        append_lower_hex(&mut value, &random);
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FactBatchId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for FactBatchId {
    type Err = SessionEvidenceIdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_lower_hex(value, FACT_BATCH_ID_PREFIX, FACT_BATCH_RANDOM_BYTES * 2)?;
        if value[FACT_BATCH_ID_PREFIX.len()..]
            .bytes()
            .all(|byte| byte == b'0')
        {
            return Err(SessionEvidenceIdentityError(
                "fact batch ID must not be all zeroes",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for FactBatchId {
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
pub struct SessionEvidenceIdentityError(&'static str);

impl fmt::Display for SessionEvidenceIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for SessionEvidenceIdentityError {}

/// Additive, per-request metrics retained by both digests and event facts.
/// Revisions are data, not acceptance gates: a future reader can retain token
/// evidence while marking incompatible EST/API projections partial.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionUsageMetrics {
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<u128>,
    #[serde(default)]
    pub api_equivalent_cost: ApiCostAmount,
    pub call_count: u64,
    pub metric_revision: u32,
    pub estimator_revision: u32,
    pub project_breakdown_revision: u32,
    pub api_pricing_catalog_revision: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
}

impl SessionUsageMetrics {
    fn validate(&self) -> io::Result<()> {
        if self.metric_revision == 0
            || self.estimator_revision == 0
            || self.project_breakdown_revision == 0
            || self.api_pricing_catalog_revision == 0
        {
            return Err(invalid_data("session evidence revisions must be nonzero"));
        }
        validate_api_cost(self.api_equivalent_cost)?;
        validate_partial_reasons(&self.partial_reasons)
    }
}

/// Source-scoped summary for one thread range. The stable key is
/// `(thread_id, range_start)`; source identity is supplied by its namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceSessionDigest {
    replica: SessionReplicaKey,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    covered_through: DateTime<Utc>,
    fingerprint: SessionDigestFingerprint,
    /// Content-free hash of the normalized per-bucket project/turn metric
    /// breakdown represented by this digest. It prevents fresh event totals
    /// from being paired with stale project attribution facts.
    project_breakdown_fingerprint: SessionDigestFingerprint,
    event_count: u64,
    exact_event_identity: bool,
    coverage_complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    observed_project_keys: Vec<ObservedProjectKey>,
    metrics: SessionUsageMetrics,
}

impl SourceSessionDigest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        replica: SessionReplicaKey,
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
        covered_through: DateTime<Utc>,
        fingerprint: SessionDigestFingerprint,
        project_breakdown_fingerprint: SessionDigestFingerprint,
        event_count: u64,
        exact_event_identity: bool,
        coverage_complete: bool,
        observed_project_keys: Vec<ObservedProjectKey>,
        metrics: SessionUsageMetrics,
    ) -> io::Result<Self> {
        let digest = Self {
            replica,
            range_start,
            range_end,
            covered_through,
            fingerprint,
            project_breakdown_fingerprint,
            event_count,
            exact_event_identity,
            coverage_complete,
            observed_project_keys,
            metrics,
        };
        digest.validate()?;
        Ok(digest)
    }

    pub fn replica(&self) -> &SessionReplicaKey {
        &self.replica
    }

    pub fn range_start(&self) -> DateTime<Utc> {
        self.range_start
    }

    pub fn range_end(&self) -> DateTime<Utc> {
        self.range_end
    }

    pub fn covered_through(&self) -> DateTime<Utc> {
        self.covered_through
    }

    pub fn fingerprint(&self) -> &SessionDigestFingerprint {
        &self.fingerprint
    }

    pub fn project_breakdown_fingerprint(&self) -> &SessionDigestFingerprint {
        &self.project_breakdown_fingerprint
    }

    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    pub fn exact_event_identity(&self) -> bool {
        self.exact_event_identity
    }

    pub fn coverage_complete(&self) -> bool {
        self.coverage_complete
    }

    pub fn observed_project_keys(&self) -> &[ObservedProjectKey] {
        &self.observed_project_keys
    }

    pub fn metrics(&self) -> &SessionUsageMetrics {
        &self.metrics
    }

    fn validate(&self) -> io::Result<()> {
        if self.range_end <= self.range_start {
            return Err(invalid_data("session digest range must be nonempty"));
        }
        if self.covered_through < self.range_start || self.covered_through > self.range_end {
            return Err(invalid_data(
                "session digest coveredThrough must fall within its range",
            ));
        }
        if self.coverage_complete && self.covered_through != self.range_end {
            return Err(invalid_data(
                "a complete session digest must cover its full range",
            ));
        }
        if self.event_count == 0 && !self.metrics.token_usage.is_zero() {
            return Err(invalid_data(
                "a nonzero session digest must report at least one event",
            ));
        }
        if self.metrics.call_count > self.event_count {
            return Err(invalid_data(
                "session digest call count cannot exceed its event count",
            ));
        }
        if self
            .observed_project_keys
            .windows(2)
            .any(|projects| projects[0].as_str() >= projects[1].as_str())
        {
            return Err(invalid_data(
                "session digest observed project keys must be sorted and unique",
            ));
        }
        self.metrics.validate()
    }
}

/// Exact digest identities that were revalidated by the same complete scan
/// which produced an active fact generation. A fact set may span several UTC
/// days, so the manifest carries a bounded set instead of only the candidate
/// which happened to trigger the refresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FactDigestBinding {
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    covered_through: DateTime<Utc>,
    coverage_complete: bool,
    fingerprint: SessionDigestFingerprint,
    project_breakdown_fingerprint: SessionDigestFingerprint,
    event_count: u64,
    metric_revision: u32,
    estimator_revision: u32,
    project_breakdown_revision: u32,
    api_pricing_catalog_revision: u32,
}

impl FactDigestBinding {
    pub fn from_digest(digest: &SourceSessionDigest) -> io::Result<Self> {
        if !digest.exact_event_identity() {
            return Err(invalid_data(
                "fact digest binding requires exact session event identity",
            ));
        }
        let binding = Self {
            range_start: digest.range_start(),
            range_end: digest.range_end(),
            covered_through: digest.covered_through(),
            coverage_complete: digest.coverage_complete(),
            fingerprint: digest.fingerprint().clone(),
            project_breakdown_fingerprint: digest.project_breakdown_fingerprint().clone(),
            event_count: digest.event_count(),
            metric_revision: digest.metrics().metric_revision,
            estimator_revision: digest.metrics().estimator_revision,
            project_breakdown_revision: digest.metrics().project_breakdown_revision,
            api_pricing_catalog_revision: digest.metrics().api_pricing_catalog_revision,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub fn range_start(&self) -> DateTime<Utc> {
        self.range_start
    }

    pub fn range_end(&self) -> DateTime<Utc> {
        self.range_end
    }

    pub fn covered_through(&self) -> DateTime<Utc> {
        self.covered_through
    }

    pub fn coverage_complete(&self) -> bool {
        self.coverage_complete
    }

    pub fn fingerprint(&self) -> &SessionDigestFingerprint {
        &self.fingerprint
    }

    pub fn project_breakdown_fingerprint(&self) -> &SessionDigestFingerprint {
        &self.project_breakdown_fingerprint
    }

    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    pub fn metric_revision(&self) -> u32 {
        self.metric_revision
    }

    pub fn estimator_revision(&self) -> u32 {
        self.estimator_revision
    }

    pub fn project_breakdown_revision(&self) -> u32 {
        self.project_breakdown_revision
    }

    pub fn api_pricing_catalog_revision(&self) -> u32 {
        self.api_pricing_catalog_revision
    }

    pub fn matches_digest(&self, digest: &SourceSessionDigest) -> bool {
        self.range_start == digest.range_start()
            && self.range_end == digest.range_end()
            && self.covered_through == digest.covered_through()
            && self.coverage_complete == digest.coverage_complete()
            && self.fingerprint == *digest.fingerprint()
            && self.project_breakdown_fingerprint == *digest.project_breakdown_fingerprint()
            && self.event_count == digest.event_count()
            && self.metric_revision == digest.metrics().metric_revision
            && self.estimator_revision == digest.metrics().estimator_revision
            && self.project_breakdown_revision == digest.metrics().project_breakdown_revision
            && self.api_pricing_catalog_revision == digest.metrics().api_pricing_catalog_revision
    }

    fn validate(&self) -> io::Result<()> {
        if self.range_end <= self.range_start
            || self.range_end.signed_duration_since(self.range_start) > Duration::days(1)
            || self.covered_through < self.range_start
            || self.covered_through > self.range_end
            || (self.coverage_complete && self.covered_through != self.range_end)
            || self.metric_revision == 0
            || self.estimator_revision == 0
            || self.project_breakdown_revision == 0
            || self.api_pricing_catalog_revision == 0
        {
            return Err(invalid_data("fact digest binding range is invalid"));
        }
        Ok(())
    }
}

fn validate_fact_digest_bindings(bindings: &[FactDigestBinding]) -> io::Result<()> {
    if bindings.len() > MAX_FACT_DIGEST_BINDINGS {
        return Err(invalid_data(
            "fact digest binding count exceeds its hard bound",
        ));
    }
    let mut previous = None;
    for binding in bindings {
        binding.validate()?;
        if previous.is_some_and(|start| start >= binding.range_start) {
            return Err(invalid_data(
                "fact digest bindings must be sorted and unique",
            ));
        }
        previous = Some(binding.range_start);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceSessionDigestRecord {
    thread_id: ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    changed_at: DateTime<Utc>,
    /// Monotonic suppression horizon for older revisions of this key. A later
    /// correction may shrink its visible range, but it must not let GC forget
    /// the revision floor while an older, wider upsert could still intersect a
    /// retained query window.
    retention_through: DateTime<Utc>,
    revision: u64,
    change: SourceSessionDigestChange,
}

impl SourceSessionDigestRecord {
    pub fn upsert(revision: u64, digest: SourceSessionDigest) -> io::Result<Self> {
        let retention_through = digest.range_end.max(digest.covered_through);
        Self::upsert_with_retention_through(revision, digest, retention_through)
    }

    /// Builds an upsert while preserving an upstream revision-suppression
    /// horizon. Remote journals can retain an older, wider revision beyond the
    /// digest's current range, so importers must not shorten this value to the
    /// payload bounds.
    pub fn upsert_with_retention_through(
        revision: u64,
        digest: SourceSessionDigest,
        retention_through: DateTime<Utc>,
    ) -> io::Result<Self> {
        let record = Self {
            thread_id: digest.replica.thread_id().clone(),
            range_start: digest.range_start,
            range_end: digest.range_end,
            changed_at: digest.covered_through,
            retention_through,
            revision,
            change: SourceSessionDigestChange::Upsert(Box::new(digest)),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn tombstone(
        thread_id: ThreadId,
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
        changed_at: DateTime<Utc>,
        revision: u64,
    ) -> io::Result<Self> {
        let retention_through = range_end.max(changed_at);
        Self::tombstone_with_retention_through(
            thread_id,
            range_start,
            range_end,
            changed_at,
            retention_through,
            revision,
        )
    }

    /// Builds a tombstone while preserving an upstream revision-suppression
    /// horizon. See [`Self::upsert_with_retention_through`].
    pub fn tombstone_with_retention_through(
        thread_id: ThreadId,
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
        changed_at: DateTime<Utc>,
        retention_through: DateTime<Utc>,
        revision: u64,
    ) -> io::Result<Self> {
        let record = Self {
            thread_id,
            range_start,
            range_end,
            changed_at,
            retention_through,
            revision,
            change: SourceSessionDigestChange::Tombstone,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    pub fn range_start(&self) -> DateTime<Utc> {
        self.range_start
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn range_end(&self) -> DateTime<Utc> {
        self.range_end
    }

    pub fn changed_at(&self) -> DateTime<Utc> {
        self.changed_at
    }

    pub fn retention_through(&self) -> DateTime<Utc> {
        self.retention_through
    }

    pub fn change(&self) -> &SourceSessionDigestChange {
        &self.change
    }

    fn validate(&self) -> io::Result<()> {
        if self.revision == 0 {
            return Err(invalid_data("session digest revision must be nonzero"));
        }
        if self.range_end <= self.range_start
            || self.changed_at < self.range_start
            || self.retention_through < self.range_end
            || self.retention_through < self.changed_at
        {
            return Err(invalid_data(
                "session digest record retention bounds are invalid",
            ));
        }
        if let SourceSessionDigestChange::Upsert(digest) = &self.change {
            digest.validate()?;
            if digest.replica.thread_id() != &self.thread_id
                || digest.range_start != self.range_start
                || digest.range_end != self.range_end
                || digest.covered_through != self.changed_at
            {
                return Err(invalid_data(
                    "session digest record key does not match its payload",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSessionDigestChange {
    Upsert(Box<SourceSessionDigest>),
    Tombstone,
}

/// Normalized additive usage evidence. It intentionally has no title, prompt,
/// assistant, reasoning, or tool-content field, so both redaction namespaces
/// remain content-free even if an upstream caller is buggy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageEventFact {
    replica: SessionReplicaKey,
    event_id: UsageEventId,
    occurred_at: DateTime<Utc>,
    observed_project_key: ObservedProjectKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    emitting_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_thread_id: Option<ThreadId>,
    /// Exact optional project-group session field used by the canonical
    /// project-breakdown fingerprint. `root_session_thread_id` remains the
    /// query fallback when this source field was absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_session_thread_id: Option<ThreadId>,
    root_session_thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_session_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    /// Exact source event token breakdown used by the canonical session
    /// fingerprint. This differs from accounting metrics only for models such
    /// as Spark whose product usage is intentionally excluded from totals.
    digest_token_usage: TokenUsage,
    request_usage_exact: bool,
    exact_event_identity: bool,
    metrics: SessionUsageMetrics,
}

impl UsageEventFact {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        replica: SessionReplicaKey,
        event_id: UsageEventId,
        occurred_at: DateTime<Utc>,
        observed_project_key: ObservedProjectKey,
        emitting_turn_id: Option<String>,
        parent_thread_id: Option<ThreadId>,
        project_session_thread_id: Option<ThreadId>,
        root_session_thread_id: ThreadId,
        root_session_turn_id: Option<String>,
        model: Option<String>,
        service_tier: Option<String>,
        digest_token_usage: TokenUsage,
        request_usage_exact: bool,
        exact_event_identity: bool,
        metrics: SessionUsageMetrics,
    ) -> io::Result<Self> {
        let fact = Self {
            replica,
            event_id,
            occurred_at,
            observed_project_key,
            emitting_turn_id,
            parent_thread_id,
            project_session_thread_id,
            root_session_thread_id,
            root_session_turn_id,
            model,
            service_tier,
            digest_token_usage,
            request_usage_exact,
            exact_event_identity,
            metrics,
        };
        fact.validate()?;
        Ok(fact)
    }

    pub fn replica(&self) -> &SessionReplicaKey {
        &self.replica
    }

    pub fn event_id(&self) -> &UsageEventId {
        &self.event_id
    }

    pub fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    pub fn observed_project_key(&self) -> &ObservedProjectKey {
        &self.observed_project_key
    }

    pub fn emitting_turn_id(&self) -> Option<&str> {
        self.emitting_turn_id.as_deref()
    }

    pub fn parent_thread_id(&self) -> Option<&ThreadId> {
        self.parent_thread_id.as_ref()
    }

    pub fn project_session_thread_id(&self) -> Option<&ThreadId> {
        self.project_session_thread_id.as_ref()
    }

    pub fn root_session_thread_id(&self) -> &ThreadId {
        &self.root_session_thread_id
    }

    pub fn root_session_turn_id(&self) -> Option<&str> {
        self.root_session_turn_id.as_deref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn service_tier(&self) -> Option<&str> {
        self.service_tier.as_deref()
    }

    pub fn digest_token_usage(&self) -> TokenUsage {
        self.digest_token_usage
    }

    pub fn request_usage_exact(&self) -> bool {
        self.request_usage_exact
    }

    pub fn exact_event_identity(&self) -> bool {
        self.exact_event_identity
    }

    pub fn metrics(&self) -> &SessionUsageMetrics {
        &self.metrics
    }

    fn validate(&self) -> io::Result<()> {
        validate_optional_protocol_text(
            self.emitting_turn_id.as_deref(),
            MAX_TURN_ID_BYTES,
            "emitting turn ID",
        )?;
        validate_optional_protocol_text(
            self.root_session_turn_id.as_deref(),
            MAX_TURN_ID_BYTES,
            "root session turn ID",
        )?;
        validate_optional_protocol_text(self.model.as_deref(), MAX_MODEL_BYTES, "model")?;
        validate_optional_protocol_text(
            self.service_tier.as_deref(),
            MAX_SERVICE_TIER_BYTES,
            "service tier",
        )?;
        if self
            .project_session_thread_id
            .as_ref()
            .is_some_and(|session| session != &self.root_session_thread_id)
            || (self.project_session_thread_id.is_none()
                && &self.root_session_thread_id != self.replica.thread_id())
        {
            return Err(invalid_data(
                "usage event fact project session does not match its root fallback",
            ));
        }
        if self.metrics.call_count == 0 {
            return Err(invalid_data(
                "usage event fact must represent at least one call",
            ));
        }
        self.metrics.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageEventFactRecord {
    event_id: UsageEventId,
    occurred_at: DateTime<Utc>,
    revision: u64,
    change: UsageEventFactChange,
}

impl UsageEventFactRecord {
    pub fn upsert(revision: u64, fact: UsageEventFact) -> io::Result<Self> {
        let record = Self {
            event_id: fact.event_id.clone(),
            occurred_at: fact.occurred_at,
            revision,
            change: UsageEventFactChange::Upsert(Box::new(fact)),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn tombstone(
        event_id: UsageEventId,
        occurred_at: DateTime<Utc>,
        revision: u64,
    ) -> io::Result<Self> {
        let record = Self {
            event_id,
            occurred_at,
            revision,
            change: UsageEventFactChange::Tombstone,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn event_id(&self) -> &UsageEventId {
        &self.event_id
    }

    pub fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn change(&self) -> &UsageEventFactChange {
        &self.change
    }

    fn validate(&self) -> io::Result<()> {
        if self.revision == 0 {
            return Err(invalid_data("usage event fact revision must be nonzero"));
        }
        if let UsageEventFactChange::Upsert(fact) = &self.change {
            fact.validate()?;
            if fact.event_id != self.event_id || fact.occurred_at != self.occurred_at {
                return Err(invalid_data(
                    "usage event fact record key does not match its payload",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UsageEventFactChange {
    Upsert(Box<UsageEventFact>),
    Tombstone,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FactCursor {
    fact_generation: u64,
    through_sequence: u64,
}

/// Exact compare-and-swap identity of an active fact set.
///
/// The remote cursor alone is insufficient because local retention GC can
/// rewrite an active generation without advancing that cursor. Callers must
/// bind both values so a pre-GC staged batch cannot restore pruned records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveFactVersion {
    active_generation: FactBatchId,
    cursor: FactCursor,
    /// Exact remote exporter generation/revisions represented by this fact
    /// set. Local facts have no remote binding.
    remote_binding: Option<SourceHistoryRemoteBinding>,
    /// Exact source digests revalidated by the complete scan which produced
    /// this generation. Empty legacy manifests are safe but never satisfy a
    /// current digest and therefore force a refresh.
    #[serde(default)]
    validated_digests: Vec<FactDigestBinding>,
    /// Center-trusted lower bound established by retention GC. Including it in
    /// the compare-and-swap identity invalidates a batch staged before GC even
    /// when the active fact generation and remote cursor did not otherwise
    /// change.
    retained_since: Option<DateTime<Utc>>,
}

impl ActiveFactVersion {
    pub fn active_generation(&self) -> &FactBatchId {
        &self.active_generation
    }

    pub fn cursor(&self) -> FactCursor {
        self.cursor
    }

    pub fn remote_binding(&self) -> Option<&SourceHistoryRemoteBinding> {
        self.remote_binding.as_ref()
    }

    pub fn validated_digests(&self) -> &[FactDigestBinding] {
        &self.validated_digests
    }

    pub fn retained_since(&self) -> Option<DateTime<Utc>> {
        self.retained_since
    }

    fn validate(&self) -> io::Result<()> {
        self.cursor.validate()?;
        validate_fact_digest_bindings(&self.validated_digests)?;
        if let Some(binding) = &self.remote_binding {
            binding.validate_namespace(&binding.source().node_id)?;
        }
        Ok(())
    }
}

impl FactCursor {
    pub fn new(fact_generation: u64, through_sequence: u64) -> io::Result<Self> {
        if fact_generation == 0 {
            return Err(invalid_data("fact generation must be nonzero"));
        }
        Ok(Self {
            fact_generation,
            through_sequence,
        })
    }

    pub fn fact_generation(self) -> u64 {
        self.fact_generation
    }

    pub fn through_sequence(self) -> u64 {
        self.through_sequence
    }

    fn validate(self) -> io::Result<()> {
        Self::new(self.fact_generation, self.through_sequence).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactBatchKind {
    Snapshot,
    Delta,
}

/// A complete fact batch ready to stage. Page tokens and partial-page state are
/// deliberately absent: protocol code must assemble all pages before calling
/// this API, and only the fenced `SourceHistoryWriter` activation operation
/// can make the candidate visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteFactBatch {
    pub batch_id: FactBatchId,
    pub kind: FactBatchKind,
    pub replica: SessionReplicaKey,
    pub expected_active_version: Option<ActiveFactVersion>,
    /// Required for SSH sources and absent for local facts. Delta batches must
    /// retain the exact active binding; snapshots may replace a stale binding.
    pub remote_binding: Option<SourceHistoryRemoteBinding>,
    pub validated_digests: Vec<FactDigestBinding>,
    pub activate_cursor: FactCursor,
    pub completed_at: DateTime<Utc>,
    pub changes: Vec<UsageEventFactRecord>,
}

impl CompleteFactBatch {
    pub fn validate(&self) -> io::Result<()> {
        self.activate_cursor.validate()?;
        validate_fact_digest_bindings(&self.validated_digests)?;
        validate_fact_batch_change_count(self.changes.len())?;
        validate_fact_record_span(&self.changes)?;
        if let Some(version) = &self.expected_active_version {
            version.validate()?;
        }
        if let Some(binding) = &self.remote_binding {
            binding.validate_namespace(self.replica.source_id())?;
        }
        match self.kind {
            FactBatchKind::Snapshot => {
                if self
                    .expected_active_version
                    .as_ref()
                    .is_some_and(|expected| {
                        expected.cursor == self.activate_cursor
                            && expected.remote_binding == self.remote_binding
                    })
                {
                    return Err(invalid_data(
                        "a fact snapshot cannot replace active facts at the same cursor",
                    ));
                }
            }
            FactBatchKind::Delta => {
                let expected = self.expected_active_version.as_ref().ok_or_else(|| {
                    invalid_data("a fact delta requires an expected active cursor")
                })?;
                if expected.remote_binding != self.remote_binding {
                    return Err(invalid_data(
                        "a fact delta cannot change its remote source binding",
                    ));
                }
                if expected.cursor.fact_generation != self.activate_cursor.fact_generation {
                    return Err(invalid_data("a fact delta cannot change fact generation"));
                }
                if self.activate_cursor.through_sequence < expected.cursor.through_sequence {
                    return Err(invalid_data("a fact delta cursor cannot move backwards"));
                }
                if !self.changes.is_empty()
                    && self.activate_cursor.through_sequence == expected.cursor.through_sequence
                {
                    return Err(invalid_data(
                        "a nonempty fact delta must advance its cursor",
                    ));
                }
            }
        }
        for record in &self.changes {
            record.validate()?;
            if let UsageEventFactChange::Upsert(fact) = record.change()
                && fact.replica() != &self.replica
            {
                return Err(invalid_data(
                    "fact batch contains a different session replica",
                ));
            }
        }
        Ok(())
    }
}

fn validate_fact_batch_change_count(change_count: usize) -> io::Result<()> {
    if change_count > MAX_FACT_BATCH_CHANGES {
        return Err(invalid_data("fact batch contains too many changes"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSessionDigestRecordsData {
    pub source: SourceMetadata,
    pub redaction_profile: RedactionProfile,
    pub records: Vec<SourceSessionDigestRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveFactSet {
    pub replica: SessionReplicaKey,
    pub redaction_profile: RedactionProfile,
    pub version: ActiveFactVersion,
    pub cursor: FactCursor,
    pub remote_binding: Option<SourceHistoryRemoteBinding>,
    /// Center-local activation time used only for bounded/fair refresh
    /// planning. It is never sent to another source.
    pub activated_at: DateTime<Utc>,
    pub records: Vec<UsageEventFactRecord>,
}

impl ActiveFactSet {
    pub fn facts(&self) -> Vec<&UsageEventFact> {
        self.records
            .iter()
            .filter_map(|record| match record.change() {
                UsageEventFactChange::Upsert(fact) => Some(fact.as_ref()),
                UsageEventFactChange::Tombstone => None,
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FactActivationReport {
    pub activated: bool,
    pub cleanup_pending: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionDigestShard {
    format_version: u32,
    metric_revision: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    utc_day: NaiveDate,
    records: Vec<SourceSessionDigestRecord>,
}

impl SessionDigestShard {
    fn new(
        profile_id: HistoryProfileId,
        source_id: NodeId,
        redaction_profile: RedactionProfile,
        utc_day: NaiveDate,
    ) -> Self {
        Self {
            format_version: SESSION_DIGEST_SHARD_FORMAT_VERSION,
            metric_revision: SESSION_EVIDENCE_METRIC_REVISION,
            profile_id,
            source_id,
            redaction_profile,
            utc_day,
            records: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FactShard {
    format_version: u32,
    metric_revision: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    replica: SessionReplicaKey,
    thread_shard_key: ThreadShardKey,
    generation: FactBatchId,
    utc_day: NaiveDate,
    records: Vec<UsageEventFactRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StagedFactBatch {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    thread_shard_key: ThreadShardKey,
    batch_id: FactBatchId,
    kind: FactBatchKind,
    replica: SessionReplicaKey,
    expected_active_version: Option<ActiveFactVersion>,
    remote_binding: Option<SourceHistoryRemoteBinding>,
    #[serde(default)]
    validated_digests: Vec<FactDigestBinding>,
    retained_since: Option<DateTime<Utc>>,
    activate_cursor: FactCursor,
    completed_at: DateTime<Utc>,
    shard_days: Vec<NaiveDate>,
    change_count: usize,
    record_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActiveFactManifest {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    thread_shard_key: ThreadShardKey,
    replica: SessionReplicaKey,
    active_generation: FactBatchId,
    cursor: FactCursor,
    remote_binding: Option<SourceHistoryRemoteBinding>,
    #[serde(default)]
    validated_digests: Vec<FactDigestBinding>,
    retained_since: Option<DateTime<Utc>>,
    activated_at: DateTime<Utc>,
    shard_days: Vec<NaiveDate>,
    record_count: usize,
}

#[derive(Debug)]
enum PrevalidatedFactPublicationMode {
    NoOp,
    Publish {
        manifest: Box<ActiveFactManifest>,
        previous_active_generation: Option<FactBatchId>,
    },
}

/// Durable, content-validated fact publication prepared outside the remotes
/// config lock. Its generation and candidate manifest remain invisible until
/// the short exact-config publication step replaces the active manifest.
#[derive(Debug)]
pub(crate) struct PrevalidatedFactPublication {
    descriptor: StagedFactBatch,
    mode: PrevalidatedFactPublicationMode,
}

impl PrevalidatedFactPublication {
    pub(super) fn redaction_profile(&self) -> RedactionProfile {
        self.descriptor.redaction_profile
    }
}

impl ActiveFactManifest {
    fn version(&self) -> ActiveFactVersion {
        ActiveFactVersion {
            active_generation: self.active_generation.clone(),
            cursor: self.cursor,
            remote_binding: self.remote_binding.clone(),
            validated_digests: self.validated_digests.clone(),
            retained_since: self.retained_since,
        }
    }
}

impl SourceHistoryStore {
    pub fn source_digests_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source_id)
            .join(redaction_profile.directory_name())
            .join(DIGESTS_DIRECTORY)
    }

    pub fn source_facts_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source_id)
            .join(redaction_profile.directory_name())
            .join(FACTS_DIRECTORY)
    }

    pub fn source_fact_manifests_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source_id)
            .join(redaction_profile.directory_name())
            .join(FACT_MANIFESTS_DIRECTORY)
    }

    pub fn source_fact_staging_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source_id)
            .join(redaction_profile.directory_name())
            .join(FACT_STAGING_DIRECTORY)
    }

    #[cfg(test)]
    pub(crate) fn acquire_fact_staging_lock_for_test(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> io::Result<std::fs::File> {
        let staging_root = self.source_fact_staging_directory(source_id, redaction_profile);
        self.prepare_private_directory(&staging_root)?;
        let lock = open_lock_file(&staging_root, FACT_STAGING_LOCK_FILE)?;
        lock_exclusive(&lock, &staging_root, FACT_STAGING_LOCK_FILE)?;
        Ok(lock)
    }

    #[cfg(test)]
    pub(crate) fn record_source_session_digest_changes(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceSessionDigestRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.record_source_session_digest_changes_unfenced(source_id, redaction_profile, records)
    }

    #[cfg(test)]
    pub(crate) fn stage_complete_fact_batch(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch: &CompleteFactBatch,
    ) -> io::Result<()> {
        self.stage_complete_fact_batch_unfenced(source_id, redaction_profile, batch)
    }

    #[cfg(test)]
    pub(crate) fn activate_staged_fact_batch(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch_id: &FactBatchId,
    ) -> io::Result<FactActivationReport> {
        self.activate_staged_fact_batch_unfenced(source_id, redaction_profile, batch_id)
    }

    pub(super) fn record_source_session_digest_changes_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        records: &[SourceSessionDigestRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        let _ = self.load_source_metadata(source_id)?;
        self.record_source_session_digest_changes_in_directory_unfenced(
            source_id,
            redaction_profile,
            &self.source_digests_directory(source_id, redaction_profile),
            records,
        )
    }

    pub(super) fn record_source_session_digest_changes_in_directory_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        directory: &Path,
        records: &[SourceSessionDigestRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        let additions = group_digest_records_by_day(source_id, records)?;
        if additions.is_empty() {
            return Ok(SourceHistoryWriteReport::default());
        }
        self.prepare_private_directory(directory)?;
        let lock = open_lock_file(directory, DIGESTS_LOCK_FILE)?;
        lock_exclusive(&lock, directory, DIGESTS_LOCK_FILE)?;
        cleanup_atomic_shard_temporary_files(self, directory, AtomicShardFileKind::GzipJson)?;
        let mut report = SourceHistoryWriteReport::default();
        for (day, additions) in additions {
            let path = evidence_shard_path(directory, day);
            let mut shard = match read_digest_shard(
                &path,
                &self.profile_id,
                source_id,
                redaction_profile,
                day,
            )? {
                Some(shard) => shard,
                None => SessionDigestShard::new(
                    self.profile_id.clone(),
                    source_id.clone(),
                    redaction_profile,
                    day,
                ),
            };
            let mut changed = false;
            for record in additions {
                changed |= apply_digest_record(&mut shard.records, record)?;
            }
            if !changed {
                report.shards_skipped += 1;
                continue;
            }
            sort_digest_records(&mut shard.records);
            write_gzip_json_atomically(self, &path, &shard)?;
            report.shards_written += 1;
        }
        Ok(report)
    }

    pub fn load_source_session_digest_records_since(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<SourceSessionDigestRecordsData> {
        self.with_source_metadata_shared(source_id, |source| {
            let records = if source.kind() == SourceKind::Ssh {
                self.with_active_remote_history_generation(
                    source_id,
                    redaction_profile,
                    |generation_directory| {
                        let Some(generation_directory) = generation_directory else {
                            return Ok(Vec::new());
                        };
                        self.load_source_session_digest_records_from_directory(
                            source_id,
                            redaction_profile,
                            since,
                            &generation_directory.join(DIGESTS_DIRECTORY),
                        )
                    },
                )?
            } else {
                self.load_source_session_digest_records_from_directory(
                    source_id,
                    redaction_profile,
                    since,
                    &self.source_digests_directory(source_id, redaction_profile),
                )?
            };
            Ok(SourceSessionDigestRecordsData {
                source: source.clone(),
                redaction_profile,
                records,
            })
        })
    }

    pub(super) fn load_source_session_digest_records_from_directory(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        directory: &Path,
    ) -> io::Result<Vec<SourceSessionDigestRecord>> {
        if !self.private_directory_exists(directory)? {
            return Ok(Vec::new());
        }
        let lock = open_lock_file(directory, DIGESTS_LOCK_FILE)?;
        lock_shared(&lock, directory, DIGESTS_LOCK_FILE)?;
        let mut records = Vec::new();
        for (day, path) in evidence_shard_entries_since(self, directory, since)? {
            let Some(shard) =
                read_digest_shard(&path, &self.profile_id, source_id, redaction_profile, day)?
            else {
                continue;
            };
            for record in shard.records {
                if digest_record_intersects_since(&record, since) {
                    let _ = apply_digest_record(&mut records, record)?;
                }
            }
        }
        sort_digest_records(&mut records);
        Ok(records)
    }

    pub(super) fn stage_complete_fact_batch_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch: &CompleteFactBatch,
    ) -> io::Result<()> {
        batch.validate()?;
        if batch.replica.source_id() != source_id {
            return Err(invalid_data(
                "fact batch source does not match its namespace",
            ));
        }
        let source = self.load_source_metadata(source_id)?;
        validate_fact_remote_binding(source.kind(), source_id, batch.remote_binding.as_ref())?;
        let shard_key = ThreadShardKey::from_replica(&batch.replica);
        let staging_root = self.source_fact_staging_directory(source_id, redaction_profile);
        self.prepare_private_directory(&staging_root)?;
        let staging_lock = open_lock_file(&staging_root, FACT_STAGING_LOCK_FILE)?;
        lock_exclusive(&staging_lock, &staging_root, FACT_STAGING_LOCK_FILE)?;
        ensure_fact_namespace_within_cap(self, source_id, redaction_profile)?;
        let manifests = self.source_fact_manifests_directory(source_id, redaction_profile);
        self.prepare_private_directory(&manifests)?;
        let lock_name = fact_lock_name(&shard_key);
        let lock = open_lock_file(&manifests, &lock_name)?;
        lock_exclusive(&lock, &manifests, &lock_name)?;
        let current = self.read_active_fact_manifest_unlocked(
            source_id,
            redaction_profile,
            &batch.replica,
            &shard_key,
        )?;
        if current.as_ref().map(ActiveFactManifest::version) != batch.expected_active_version {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "active fact version changed before staging",
            ));
        }
        let retained_since = current
            .as_ref()
            .and_then(|manifest| manifest.retained_since);

        let mut records = match batch.kind {
            FactBatchKind::Snapshot => Vec::new(),
            FactBatchKind::Delta => current
                .as_ref()
                .map(|manifest| {
                    self.read_fact_generation_unlocked(source_id, redaction_profile, manifest)
                })
                .transpose()?
                .unwrap_or_default(),
        };
        for change in batch.changes.iter().cloned() {
            validate_fact_record_namespace(&change, &batch.replica)?;
            if retained_since.is_some_and(|cutoff| change.occurred_at() < cutoff) {
                // GC has already made this time range permanently invisible.
                // Advancing the remote cursor is still safe, but retaining the
                // stale change would allow a previously collected tombstone to
                // resurrect after its record was pruned.
                continue;
            }
            let _ = apply_fact_record(&mut records, change)?;
        }
        sort_fact_records(&mut records);
        validate_fact_generation_limits(&records)?;
        let facts = records
            .iter()
            .filter_map(|record| match record.change() {
                UsageEventFactChange::Upsert(fact) => Some(fact.as_ref()),
                UsageEventFactChange::Tombstone => None,
            })
            .collect::<Vec<_>>();
        crate::source_export::validate_fact_digest_bindings_against_facts(
            &batch.replica,
            &facts,
            &batch.validated_digests,
            retained_since,
        )?;

        let staging = staging_root.join(batch.batch_id.as_str());
        create_new_private_directory(self, &staging)?;
        let generation = staging.join(STAGED_GENERATION_DIRECTORY);
        create_new_private_directory(self, &generation)?;
        let result = (|| {
            let binding = FactGenerationBinding {
                profile_id: &self.profile_id,
                source_id,
                redaction_profile,
                replica: &batch.replica,
                shard_key: &shard_key,
                generation: &batch.batch_id,
            };
            let shard_days = write_fact_generation(self, &generation, &binding, &records)?;
            let descriptor = StagedFactBatch {
                format_version: FACT_BATCH_FORMAT_VERSION,
                profile_id: self.profile_id.clone(),
                source_id: source_id.clone(),
                redaction_profile,
                thread_shard_key: shard_key,
                batch_id: batch.batch_id.clone(),
                kind: batch.kind,
                replica: batch.replica.clone(),
                expected_active_version: batch.expected_active_version.clone(),
                remote_binding: batch.remote_binding.clone(),
                validated_digests: batch.validated_digests.clone(),
                retained_since,
                activate_cursor: batch.activate_cursor,
                completed_at: batch.completed_at,
                shard_days,
                change_count: batch.changes.len(),
                record_count: records.len(),
            };
            write_private_atomically_beneath(
                self,
                &staging.join(STAGED_BATCH_FILE),
                &encode_pretty_bounded(&descriptor, MAX_FACT_MANIFEST_BYTES)?,
            )?;
            sync_store_directory(self, &staging)?;
            ensure_fact_namespace_within_cap(self, source_id, redaction_profile)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = remove_private_tree(self, &staging);
        }
        result
    }

    pub(super) fn activate_staged_fact_batch_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch_id: &FactBatchId,
    ) -> io::Result<FactActivationReport> {
        let publication =
            self.prevalidate_staged_fact_batch_unfenced(source_id, redaction_profile, batch_id)?;
        let mut report = self.publish_prevalidated_fact_batch_unfenced(&publication)?;
        report.cleanup_pending = self.cleanup_prevalidated_fact_publication_unfenced(&publication);
        Ok(report)
    }

    pub(super) fn prevalidate_staged_fact_batch_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch_id: &FactBatchId,
    ) -> io::Result<PrevalidatedFactPublication> {
        let source = self.load_source_metadata(source_id)?;
        let staging_root = self.source_fact_staging_directory(source_id, redaction_profile);
        self.validate_private_path(&staging_root)?;
        let staging_lock = open_lock_file(&staging_root, FACT_STAGING_LOCK_FILE)?;
        lock_exclusive(&staging_lock, &staging_root, FACT_STAGING_LOCK_FILE)?;
        let staging = staging_root.join(batch_id.as_str());
        self.validate_private_path(&staging)?;
        let descriptor = read_staged_batch(
            &staging.join(STAGED_BATCH_FILE),
            &self.profile_id,
            source_id,
            redaction_profile,
            batch_id,
        )?;
        validate_fact_remote_binding(source.kind(), source_id, descriptor.remote_binding.as_ref())?;
        let manifests = self.source_fact_manifests_directory(source_id, redaction_profile);
        self.prepare_private_directory(&manifests)?;
        let lock_name = fact_lock_name(&descriptor.thread_shard_key);
        let current = {
            let lock = open_lock_file(&manifests, &lock_name)?;
            lock_exclusive(&lock, &manifests, &lock_name)?;
            self.read_active_fact_manifest_unlocked(
                source_id,
                redaction_profile,
                &descriptor.replica,
                &descriptor.thread_shard_key,
            )?
        };
        if current.as_ref().is_some_and(|manifest| {
            manifest.active_generation == descriptor.batch_id
                && manifest.cursor == descriptor.activate_cursor
                && manifest.remote_binding == descriptor.remote_binding
        }) {
            return Ok(PrevalidatedFactPublication {
                descriptor,
                mode: PrevalidatedFactPublicationMode::NoOp,
            });
        }
        if current.as_ref().map(ActiveFactManifest::version) != descriptor.expected_active_version {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "active fact version changed before activation",
            ));
        }
        if descriptor.kind == FactBatchKind::Delta
            && descriptor
                .expected_active_version
                .as_ref()
                .is_some_and(|expected| expected.cursor == descriptor.activate_cursor)
        {
            return Ok(PrevalidatedFactPublication {
                descriptor,
                mode: PrevalidatedFactPublicationMode::NoOp,
            });
        }

        let staged_generation = staging.join(STAGED_GENERATION_DIRECTORY);
        let facts_thread = self
            .source_facts_directory(source_id, redaction_profile)
            .join(descriptor.thread_shard_key.as_str());
        self.prepare_private_directory(&facts_thread)?;
        let active_generation = facts_thread.join(descriptor.batch_id.as_str());
        if self.private_directory_exists(&staged_generation)? {
            if self.private_directory_exists(&active_generation)? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "fact generation already exists for staged batch",
                ));
            }
            self.validate_private_path(&staged_generation)?;
            self.validate_private_path(&staging)?;
            self.validate_private_path(&facts_thread)?;
            fs::rename(&staged_generation, &active_generation)?;
            self.validate_private_path(&active_generation)?;
            sync_store_directory(self, &facts_thread)?;
            sync_store_directory(self, &staging)?;
        } else {
            self.validate_private_path(&active_generation)?;
        }
        let records = read_fact_generation(
            self,
            &active_generation,
            &self.profile_id,
            source_id,
            redaction_profile,
            &descriptor.replica,
            &descriptor.thread_shard_key,
            &descriptor.batch_id,
            &descriptor.shard_days,
        )?;
        if records.len() != descriptor.record_count {
            return Err(invalid_data(
                "staged fact generation record count does not match its descriptor",
            ));
        }
        let facts = records
            .iter()
            .filter_map(|record| match record.change() {
                UsageEventFactChange::Upsert(fact) => Some(fact.as_ref()),
                UsageEventFactChange::Tombstone => None,
            })
            .collect::<Vec<_>>();
        crate::source_export::validate_fact_digest_bindings_against_facts(
            &descriptor.replica,
            &facts,
            &descriptor.validated_digests,
            descriptor.retained_since,
        )?;

        let manifest = ActiveFactManifest {
            format_version: FACT_MANIFEST_FORMAT_VERSION,
            profile_id: self.profile_id.clone(),
            source_id: source_id.clone(),
            redaction_profile,
            thread_shard_key: descriptor.thread_shard_key.clone(),
            replica: descriptor.replica.clone(),
            active_generation: descriptor.batch_id.clone(),
            cursor: descriptor.activate_cursor,
            remote_binding: descriptor.remote_binding.clone(),
            validated_digests: descriptor.validated_digests.clone(),
            retained_since: descriptor.retained_since,
            activated_at: descriptor.completed_at,
            shard_days: descriptor.shard_days.clone(),
            record_count: descriptor.record_count,
        };
        let candidate_manifest = staging.join(STAGED_PUBLICATION_FILE);
        write_private_atomically_beneath(
            self,
            &candidate_manifest,
            &encode_pretty_bounded(&manifest, MAX_FACT_MANIFEST_BYTES)?,
        )?;
        if let Err(error) = ensure_fact_namespace_within_cap(self, source_id, redaction_profile) {
            let _ = fs::remove_file(&candidate_manifest);
            let _ = sync_store_directory(self, &staging);
            return Err(error);
        }
        Ok(PrevalidatedFactPublication {
            descriptor,
            mode: PrevalidatedFactPublicationMode::Publish {
                manifest: Box::new(manifest),
                previous_active_generation: current.map(|current| current.active_generation),
            },
        })
    }

    pub(super) fn publish_prevalidated_fact_batch_unfenced(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> io::Result<FactActivationReport> {
        let descriptor = &publication.descriptor;
        let source_id = &descriptor.source_id;
        let redaction_profile = descriptor.redaction_profile;
        let source = self.load_source_metadata(source_id)?;
        validate_fact_remote_binding(source.kind(), source_id, descriptor.remote_binding.as_ref())?;

        // Publication is invoked while the exact remotes config shared fence
        // is held. Never wait there for staging, GC, a long reader, or another
        // publisher: a busy lock makes this attempt retryable instead.
        let staging_root = self.source_fact_staging_directory(source_id, redaction_profile);
        self.validate_private_path(&staging_root)?;
        let staging_lock = open_lock_file(&staging_root, FACT_STAGING_LOCK_FILE)?;
        try_lock_exclusive_for_fact_publication(
            &staging_lock,
            &staging_root,
            FACT_STAGING_LOCK_FILE,
        )?;
        let staging = staging_root.join(descriptor.batch_id.as_str());
        self.validate_private_path(&staging)?;
        let persisted = read_staged_batch(
            &staging.join(STAGED_BATCH_FILE),
            &self.profile_id,
            source_id,
            redaction_profile,
            &descriptor.batch_id,
        )?;
        if &persisted != descriptor {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "staged fact descriptor changed after prevalidation",
            ));
        }

        let manifests = self.source_fact_manifests_directory(source_id, redaction_profile);
        self.prepare_private_directory(&manifests)?;
        let lock_name = fact_lock_name(&descriptor.thread_shard_key);
        let manifest_lock = open_lock_file(&manifests, &lock_name)?;
        try_lock_exclusive_for_fact_publication(&manifest_lock, &manifests, &lock_name)?;
        let current = self.read_active_fact_manifest_unlocked(
            source_id,
            redaction_profile,
            &descriptor.replica,
            &descriptor.thread_shard_key,
        )?;

        match &publication.mode {
            PrevalidatedFactPublicationMode::NoOp => {
                let already_active = current.as_ref().is_some_and(|manifest| {
                    manifest.active_generation == descriptor.batch_id
                        && manifest.cursor == descriptor.activate_cursor
                        && manifest.remote_binding == descriptor.remote_binding
                });
                let empty_delta = descriptor.kind == FactBatchKind::Delta
                    && current.as_ref().map(ActiveFactManifest::version)
                        == descriptor.expected_active_version
                    && descriptor
                        .expected_active_version
                        .as_ref()
                        .is_some_and(|expected| expected.cursor == descriptor.activate_cursor);
                if !already_active && !empty_delta {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "active fact version changed before no-op publication",
                    ));
                }
                Ok(FactActivationReport {
                    activated: false,
                    cleanup_pending: false,
                })
            }
            PrevalidatedFactPublicationMode::Publish { manifest, .. } => {
                if current.as_ref().map(ActiveFactManifest::version)
                    != descriptor.expected_active_version
                {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "active fact version changed before publication",
                    ));
                }
                let active_generation = self
                    .source_facts_directory(source_id, redaction_profile)
                    .join(descriptor.thread_shard_key.as_str())
                    .join(descriptor.batch_id.as_str());
                self.validate_private_path(&active_generation)?;
                if !self.private_directory_exists(&active_generation)? {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "prevalidated fact generation is missing",
                    ));
                }
                let candidate_path = staging.join(STAGED_PUBLICATION_FILE);
                let candidate = read_optional_json_file::<ActiveFactManifest>(
                    &candidate_path,
                    MAX_FACT_MANIFEST_BYTES,
                )?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "prevalidated fact manifest is missing",
                    )
                })?;
                validate_active_manifest(
                    &candidate,
                    &self.profile_id,
                    source_id,
                    redaction_profile,
                    &descriptor.replica,
                    &descriptor.thread_shard_key,
                )?;
                if &candidate != manifest.as_ref() {
                    return Err(invalid_data(
                        "prevalidated fact manifest changed before publication",
                    ));
                }
                let manifest_path = fact_manifest_path(&manifests, &descriptor.thread_shard_key);
                validate_data_file_metadata(
                    &candidate_path,
                    &fs::symlink_metadata(&candidate_path)?,
                )?;
                replace_file(&candidate_path, &manifest_path)?;
                validate_data_file_metadata(
                    &manifest_path,
                    &fs::symlink_metadata(&manifest_path)?,
                )?;
                sync_store_directory(self, &manifests)?;
                Ok(FactActivationReport {
                    activated: true,
                    cleanup_pending: false,
                })
            }
        }
    }

    pub(super) fn cleanup_prevalidated_fact_publication_unfenced(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> bool {
        (|| -> io::Result<()> {
            let descriptor = &publication.descriptor;
            let source_id = &descriptor.source_id;
            let redaction_profile = descriptor.redaction_profile;
            let staging_root = self.source_fact_staging_directory(source_id, redaction_profile);
            self.prepare_private_directory(&staging_root)?;
            let staging_lock = open_lock_file(&staging_root, FACT_STAGING_LOCK_FILE)?;
            lock_exclusive(&staging_lock, &staging_root, FACT_STAGING_LOCK_FILE)?;
            let manifests = self.source_fact_manifests_directory(source_id, redaction_profile);
            self.prepare_private_directory(&manifests)?;
            let lock_name = fact_lock_name(&descriptor.thread_shard_key);
            let manifest_lock = open_lock_file(&manifests, &lock_name)?;
            lock_exclusive(&manifest_lock, &manifests, &lock_name)?;
            let current = self.read_active_fact_manifest_unlocked(
                source_id,
                redaction_profile,
                &descriptor.replica,
                &descriptor.thread_shard_key,
            )?;
            if let PrevalidatedFactPublicationMode::Publish {
                previous_active_generation: Some(previous),
                ..
            } = &publication.mode
                && current
                    .as_ref()
                    .is_some_and(|manifest| manifest.active_generation == descriptor.batch_id)
                && previous != &descriptor.batch_id
            {
                let previous_path = self
                    .source_facts_directory(source_id, redaction_profile)
                    .join(descriptor.thread_shard_key.as_str())
                    .join(previous.as_str());
                remove_private_tree(self, &previous_path)?;
            }
            let staging = staging_root.join(descriptor.batch_id.as_str());
            match remove_private_tree(self, &staging) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            ensure_fact_namespace_within_cap(self, source_id, redaction_profile)
        })()
        .is_err()
    }

    pub fn load_active_fact_set(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        thread_id: &ThreadId,
    ) -> io::Result<Option<ActiveFactSet>> {
        let source = self.load_source_metadata(source_id)?;
        let replica = SessionReplicaKey::new(source_id.clone(), thread_id.clone());
        let shard_key = ThreadShardKey::from_replica(&replica);
        let manifests = self.source_fact_manifests_directory(source_id, redaction_profile);
        if !self.private_directory_exists(&manifests)? {
            return Ok(None);
        }
        let lock_name = fact_lock_name(&shard_key);
        let lock = open_lock_file(&manifests, &lock_name)?;
        lock_shared(&lock, &manifests, &lock_name)?;
        let Some(manifest) = self.read_active_fact_manifest_unlocked(
            source_id,
            redaction_profile,
            &replica,
            &shard_key,
        )?
        else {
            return Ok(None);
        };
        validate_fact_remote_binding(source.kind(), source_id, manifest.remote_binding.as_ref())?;
        let records =
            self.read_fact_generation_unlocked(source_id, redaction_profile, &manifest)?;
        Ok(Some(ActiveFactSet {
            replica,
            redaction_profile,
            version: manifest.version(),
            cursor: manifest.cursor,
            remote_binding: manifest.remote_binding.clone(),
            activated_at: manifest.activated_at,
            records,
        }))
    }

    fn read_active_fact_manifest_unlocked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        replica: &SessionReplicaKey,
        shard_key: &ThreadShardKey,
    ) -> io::Result<Option<ActiveFactManifest>> {
        let directory = self.source_fact_manifests_directory(source_id, redaction_profile);
        self.validate_private_path(&directory)?;
        let path = fact_manifest_path(&directory, shard_key);
        let manifest: ActiveFactManifest =
            match read_optional_json_file(&path, MAX_FACT_MANIFEST_BYTES)? {
                Some(manifest) => manifest,
                None => return Ok(None),
            };
        validate_active_manifest(
            &manifest,
            &self.profile_id,
            source_id,
            redaction_profile,
            replica,
            shard_key,
        )?;
        Ok(Some(manifest))
    }

    fn read_fact_generation_unlocked(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        manifest: &ActiveFactManifest,
    ) -> io::Result<Vec<UsageEventFactRecord>> {
        let directory = self
            .source_facts_directory(source_id, redaction_profile)
            .join(manifest.thread_shard_key.as_str())
            .join(manifest.active_generation.as_str());
        let records = read_fact_generation(
            self,
            &directory,
            &self.profile_id,
            source_id,
            redaction_profile,
            &manifest.replica,
            &manifest.thread_shard_key,
            &manifest.active_generation,
            &manifest.shard_days,
        )?;
        if records.len() != manifest.record_count {
            return Err(invalid_data(
                "active fact generation record count does not match its manifest",
            ));
        }
        if manifest
            .retained_since
            .is_some_and(|cutoff| records.iter().any(|record| record.occurred_at() < cutoff))
        {
            return Err(invalid_data(
                "active fact generation contains a record below its retention floor",
            ));
        }
        Ok(records)
    }
}

fn validate_prefixed_lower_hex(
    value: &str,
    prefix: &str,
    hex_length: usize,
) -> Result<(), SessionEvidenceIdentityError> {
    let Some(hex) = value.strip_prefix(prefix) else {
        return Err(SessionEvidenceIdentityError(
            "opaque ID has the wrong prefix",
        ));
    };
    if hex.len() != hex_length {
        return Err(SessionEvidenceIdentityError(
            "opaque ID has the wrong length",
        ));
    }
    if !hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SessionEvidenceIdentityError(
            "opaque ID must use lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_opaque_id(
    value: &str,
    maximum_bytes: usize,
    subject: &'static str,
) -> Result<(), SessionEvidenceIdentityError> {
    if value.is_empty() || value.trim() != value || value.len() > maximum_bytes {
        return Err(SessionEvidenceIdentityError(match subject {
            "usage event ID" => "usage event ID has an invalid length or whitespace",
            _ => "opaque ID has an invalid length or whitespace",
        }));
    }
    if value.chars().any(|character| {
        character.is_control()
            || is_bidi_control(character)
            || matches!(character, '\u{2028}' | '\u{2029}')
    }) {
        return Err(SessionEvidenceIdentityError(match subject {
            "usage event ID" => "usage event ID contains unsafe protocol characters",
            _ => "opaque ID contains unsafe protocol characters",
        }));
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

fn validate_optional_protocol_text(
    value: Option<&str>,
    maximum_bytes: usize,
    subject: &str,
) -> io::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty()
        || value.trim() != value
        || value.len() > maximum_bytes
        || value.chars().any(|character| {
            character.is_control()
                || is_bidi_control(character)
                || matches!(character, '\u{2028}' | '\u{2029}')
        })
    {
        return Err(invalid_data(format!(
            "session evidence {subject} is invalid"
        )));
    }
    Ok(())
}

fn validate_partial_reasons(reasons: &[String]) -> io::Result<()> {
    if reasons.len() > MAX_PARTIAL_REASONS {
        return Err(invalid_data("too many session evidence partial reasons"));
    }
    for reason in reasons {
        if reason.is_empty()
            || reason.len() > MAX_PARTIAL_REASON_BYTES
            || !reason.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-' | b'.' | b':')
            })
        {
            return Err(invalid_data("session evidence partial reason is invalid"));
        }
    }
    if reasons.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid_data(
            "session evidence partial reasons must be sorted and unique",
        ));
    }
    Ok(())
}

fn validate_api_cost(amount: ApiCostAmount) -> io::Result<()> {
    if amount.minimum_pico_usd > amount.maximum_pico_usd
        || amount.priced_samples > amount.observed_samples
        || amount.priced_tokens > amount.observed_tokens
    {
        return Err(invalid_data(
            "session evidence API cost coverage is invalid",
        ));
    }
    if amount.priced_samples == 0
        && (amount.minimum_pico_usd.value() != 0 || amount.maximum_pico_usd.value() != 0)
    {
        return Err(invalid_data(
            "unpriced session evidence cannot contain API cost",
        ));
    }
    Ok(())
}

fn group_digest_records_by_day(
    source_id: &NodeId,
    records: &[SourceSessionDigestRecord],
) -> io::Result<BTreeMap<NaiveDate, Vec<SourceSessionDigestRecord>>> {
    let mut result = BTreeMap::new();
    for record in records {
        record.validate()?;
        if let SourceSessionDigestChange::Upsert(digest) = record.change()
            && digest.replica().source_id() != source_id
        {
            return Err(invalid_data(
                "session digest source does not match its namespace",
            ));
        }
        result
            .entry(record.range_start.date_naive())
            .or_insert_with(Vec::new)
            .push(record.clone());
    }
    Ok(result)
}

fn apply_digest_record(
    records: &mut Vec<SourceSessionDigestRecord>,
    mut incoming: SourceSessionDigestRecord,
) -> io::Result<bool> {
    incoming.validate()?;
    let Some(index) = records.iter().position(|record| {
        record.thread_id == incoming.thread_id && record.range_start == incoming.range_start
    }) else {
        records.push(incoming);
        return Ok(true);
    };
    let existing = &records[index];
    if incoming.revision > existing.revision {
        incoming.retention_through = incoming.retention_through.max(existing.retention_through);
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
            "conflicting session digest changes share key ({}, {}) and revision {}",
            incoming.thread_id,
            incoming.range_start.to_rfc3339(),
            incoming.revision
        )))
    }
}

fn sort_digest_records(records: &mut [SourceSessionDigestRecord]) {
    records.sort_by(|left, right| {
        left.range_start
            .cmp(&right.range_start)
            .then_with(|| left.thread_id.as_str().cmp(right.thread_id.as_str()))
    });
}

fn digest_record_intersects_since(
    record: &SourceSessionDigestRecord,
    since: DateTime<Utc>,
) -> bool {
    // The record query is also the revision-floor surface used by remote
    // import. A corrected upsert may end before `since` while its retained
    // older revision could still overlap the window, so both upserts and
    // tombstones remain visible through their suppression horizon.
    record.retention_through() >= since
}

fn read_digest_shard(
    path: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    day: NaiveDate,
) -> io::Result<Option<SessionDigestShard>> {
    let mut shard: SessionDigestShard = match read_optional_gzip_json_file(path)? {
        Some(shard) => shard,
        None => return Ok(None),
    };
    if shard.format_version != SESSION_DIGEST_SHARD_FORMAT_VERSION
        || shard.metric_revision != SESSION_EVIDENCE_METRIC_REVISION
        || &shard.profile_id != profile_id
        || &shard.source_id != source_id
        || shard.redaction_profile != redaction_profile
        || shard.utc_day != day
    {
        return Err(envelope_mismatch(path, "session digest envelope"));
    }
    let mut unique = Vec::with_capacity(shard.records.len());
    for record in std::mem::take(&mut shard.records) {
        record.validate()?;
        if record.range_start().date_naive() != day {
            return Err(envelope_mismatch(path, "session digest record UTC day"));
        }
        if let SourceSessionDigestChange::Upsert(digest) = record.change()
            && digest.replica().source_id() != source_id
        {
            return Err(envelope_mismatch(path, "session digest replica source"));
        }
        let _ = apply_digest_record(&mut unique, record)?;
    }
    sort_digest_records(&mut unique);
    shard.records = unique;
    Ok(Some(shard))
}

pub(super) fn validate_digest_shard_for_remote_clone(
    path: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    day: NaiveDate,
) -> io::Result<()> {
    read_digest_shard(path, profile_id, source_id, redaction_profile, day)?
        .ok_or_else(|| invalid_data("remote clone digest shard disappeared"))?;
    Ok(())
}

fn validate_fact_record_namespace(
    record: &UsageEventFactRecord,
    replica: &SessionReplicaKey,
) -> io::Result<()> {
    record.validate()?;
    if let UsageEventFactChange::Upsert(fact) = record.change()
        && fact.replica() != replica
    {
        return Err(invalid_data(
            "usage event fact replica does not match its fact batch",
        ));
    }
    Ok(())
}

fn validate_fact_remote_binding(
    source_kind: SourceKind,
    source_id: &NodeId,
    remote_binding: Option<&SourceHistoryRemoteBinding>,
) -> io::Result<()> {
    match (source_kind, remote_binding) {
        (SourceKind::Local, None) => Ok(()),
        (SourceKind::Ssh, Some(binding)) => binding.validate_namespace(source_id),
        (SourceKind::Local, Some(_)) => Err(invalid_data("local fact set has a remote binding")),
        (SourceKind::Ssh, None) => Err(invalid_data("SSH fact set is missing its remote binding")),
    }
}

fn apply_fact_record(
    records: &mut Vec<UsageEventFactRecord>,
    incoming: UsageEventFactRecord,
) -> io::Result<bool> {
    incoming.validate()?;
    let Some(index) = records
        .iter()
        .position(|record| record.event_id == incoming.event_id)
    else {
        records.push(incoming);
        return Ok(true);
    };
    let existing = &records[index];
    if incoming.occurred_at != existing.occurred_at {
        return Err(invalid_data(format!(
            "usage event ID {} changed occurredAt across revisions",
            incoming.event_id
        )));
    }
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
            "conflicting usage event fact changes share event ID {} and revision {}",
            incoming.event_id, incoming.revision
        )))
    }
}

fn sort_fact_records(records: &mut [UsageEventFactRecord]) {
    records.sort_by(|left, right| {
        left.occurred_at
            .cmp(&right.occurred_at)
            .then_with(|| left.event_id.as_str().cmp(right.event_id.as_str()))
    });
}

fn validate_fact_record_span(records: &[UsageEventFactRecord]) -> io::Result<()> {
    let Some(first) = records.iter().map(UsageEventFactRecord::occurred_at).min() else {
        return Ok(());
    };
    let last = records
        .iter()
        .map(UsageEventFactRecord::occurred_at)
        .max()
        .expect("a nonempty record set has a maximum timestamp");
    let day_span = last
        .date_naive()
        .signed_duration_since(first.date_naive())
        .num_days();
    if day_span >= MAX_FACT_RETENTION_DAYS {
        return Err(invalid_data(
            "fact records span more than the 35-day retention window",
        ));
    }
    Ok(())
}

fn validate_fact_generation_limits(records: &[UsageEventFactRecord]) -> io::Result<()> {
    if records.len() > MAX_FACT_GENERATION_RECORDS {
        return Err(invalid_data("fact generation contains too many records"));
    }
    validate_fact_record_span(records)
}

struct FactGenerationBinding<'a> {
    profile_id: &'a HistoryProfileId,
    source_id: &'a NodeId,
    redaction_profile: RedactionProfile,
    replica: &'a SessionReplicaKey,
    shard_key: &'a ThreadShardKey,
    generation: &'a FactBatchId,
}

fn write_fact_generation(
    store: &SourceHistoryStore,
    directory: &Path,
    binding: &FactGenerationBinding<'_>,
    records: &[UsageEventFactRecord],
) -> io::Result<Vec<NaiveDate>> {
    store.validate_private_path(directory)?;
    validate_fact_generation_limits(records)?;
    let mut grouped = BTreeMap::<NaiveDate, Vec<UsageEventFactRecord>>::new();
    for record in records {
        validate_fact_record_namespace(record, binding.replica)?;
        grouped
            .entry(record.occurred_at().date_naive())
            .or_default()
            .push(record.clone());
    }
    let mut days = Vec::with_capacity(grouped.len());
    let mut total_decoded_bytes = 0_u64;
    for (day, mut records) in grouped {
        sort_fact_records(&mut records);
        let shard = FactShard {
            format_version: FACT_SHARD_FORMAT_VERSION,
            metric_revision: SESSION_EVIDENCE_METRIC_REVISION,
            profile_id: binding.profile_id.clone(),
            source_id: binding.source_id.clone(),
            redaction_profile: binding.redaction_profile,
            replica: binding.replica.clone(),
            thread_shard_key: binding.shard_key.clone(),
            generation: binding.generation.clone(),
            utc_day: day,
            records,
        };
        let remaining = MAX_FACT_GENERATION_DECODED_BYTES
            .checked_sub(total_decoded_bytes)
            .ok_or_else(|| invalid_data("fact generation decoded size exceeds its hard cap"))?;
        if remaining == 0 {
            return Err(invalid_data(
                "fact generation decoded size exceeds its hard cap",
            ));
        }
        let decoded_bytes = write_gzip_json_atomically_with_limit(
            store,
            &evidence_shard_path(directory, day),
            &shard,
            remaining.min(MAX_EVIDENCE_SHARD_BYTES),
        )?;
        total_decoded_bytes = total_decoded_bytes
            .checked_add(decoded_bytes)
            .ok_or_else(|| invalid_data("fact generation decoded size overflowed"))?;
        days.push(day);
    }
    sync_store_directory(store, directory)?;
    Ok(days)
}

#[allow(clippy::too_many_arguments)]
fn read_fact_generation(
    store: &SourceHistoryStore,
    directory: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    replica: &SessionReplicaKey,
    shard_key: &ThreadShardKey,
    generation: &FactBatchId,
    expected_days: &[NaiveDate],
) -> io::Result<Vec<UsageEventFactRecord>> {
    store.validate_private_path(directory)?;
    validate_sorted_unique_days(expected_days)?;
    let entries = evidence_shard_entries(store, directory)?;
    let actual_days = entries.iter().map(|(day, _)| *day).collect::<Vec<_>>();
    if actual_days != expected_days {
        return Err(invalid_data(
            "fact generation shard set does not match its manifest",
        ));
    }
    let mut records = Vec::new();
    let mut event_ids = BTreeSet::new();
    let mut total_decoded_bytes = 0_u64;
    for (day, path) in entries {
        let remaining = MAX_FACT_GENERATION_DECODED_BYTES
            .checked_sub(total_decoded_bytes)
            .ok_or_else(|| invalid_data("fact generation decoded size exceeds its hard cap"))?;
        if remaining == 0 {
            return Err(invalid_data(
                "fact generation decoded size exceeds its hard cap",
            ));
        }
        let (shard, decoded_bytes): (FactShard, u64) =
            read_gzip_json_file_with_limit(&path, remaining.min(MAX_EVIDENCE_SHARD_BYTES))?;
        total_decoded_bytes = total_decoded_bytes
            .checked_add(decoded_bytes)
            .ok_or_else(|| invalid_data("fact generation decoded size overflowed"))?;
        if shard.format_version != FACT_SHARD_FORMAT_VERSION
            || shard.metric_revision != SESSION_EVIDENCE_METRIC_REVISION
            || &shard.profile_id != profile_id
            || &shard.source_id != source_id
            || shard.redaction_profile != redaction_profile
            || &shard.replica != replica
            || &shard.thread_shard_key != shard_key
            || &shard.generation != generation
            || shard.utc_day != day
        {
            return Err(envelope_mismatch(&path, "fact shard envelope"));
        }
        for record in shard.records {
            validate_fact_record_namespace(&record, replica)?;
            if record.occurred_at().date_naive() != day {
                return Err(envelope_mismatch(&path, "fact record UTC day"));
            }
            if !event_ids.insert(record.event_id().clone()) {
                return Err(invalid_data(
                    "fact generation contains duplicate usage event IDs",
                ));
            }
            records.push(record);
            if records.len() > MAX_FACT_GENERATION_RECORDS {
                return Err(invalid_data("fact generation contains too many records"));
            }
        }
    }
    sort_fact_records(&mut records);
    validate_fact_generation_limits(&records)?;
    Ok(records)
}

fn read_staged_batch(
    path: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    batch_id: &FactBatchId,
) -> io::Result<StagedFactBatch> {
    let descriptor: StagedFactBatch = read_json_file(path, MAX_FACT_MANIFEST_BYTES)?;
    if descriptor.format_version != FACT_BATCH_FORMAT_VERSION
        || &descriptor.profile_id != profile_id
        || &descriptor.source_id != source_id
        || descriptor.redaction_profile != redaction_profile
        || &descriptor.batch_id != batch_id
        || descriptor.replica.source_id() != source_id
        || ThreadShardKey::from_replica(&descriptor.replica) != descriptor.thread_shard_key
    {
        return Err(envelope_mismatch(path, "staged fact batch envelope"));
    }
    descriptor.activate_cursor.validate()?;
    validate_fact_digest_bindings(&descriptor.validated_digests)?;
    if descriptor.change_count > MAX_FACT_BATCH_CHANGES
        || descriptor.record_count > MAX_FACT_GENERATION_RECORDS
    {
        return Err(invalid_data("staged fact batch exceeds record limits"));
    }
    if let Some(version) = &descriptor.expected_active_version {
        version.validate()?;
    }
    if let Some(binding) = &descriptor.remote_binding {
        binding.validate_namespace(source_id)?;
    }
    let expected_retention_floor = descriptor
        .expected_active_version
        .as_ref()
        .and_then(ActiveFactVersion::retained_since);
    if descriptor.retained_since != expected_retention_floor {
        return Err(invalid_data(
            "staged fact batch retention floor does not match its expected active version",
        ));
    }
    if descriptor.kind == FactBatchKind::Delta {
        let expected = descriptor
            .expected_active_version
            .as_ref()
            .ok_or_else(|| invalid_data("staged fact delta has no expected active cursor"))?;
        if expected.cursor.fact_generation != descriptor.activate_cursor.fact_generation
            || expected.remote_binding != descriptor.remote_binding
            || descriptor.activate_cursor.through_sequence < expected.cursor.through_sequence
            || (descriptor.change_count > 0
                && descriptor.activate_cursor.through_sequence == expected.cursor.through_sequence)
        {
            return Err(invalid_data("staged fact delta cursor is invalid"));
        }
    } else if descriptor
        .expected_active_version
        .as_ref()
        .is_some_and(|expected| {
            expected.cursor == descriptor.activate_cursor
                && expected.remote_binding == descriptor.remote_binding
        })
    {
        return Err(invalid_data(
            "staged fact snapshot cannot replace facts at the same cursor",
        ));
    }
    validate_sorted_unique_days(&descriptor.shard_days)?;
    Ok(descriptor)
}

fn validate_active_manifest(
    manifest: &ActiveFactManifest,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    replica: &SessionReplicaKey,
    shard_key: &ThreadShardKey,
) -> io::Result<()> {
    if manifest.format_version != FACT_MANIFEST_FORMAT_VERSION
        || &manifest.profile_id != profile_id
        || &manifest.source_id != source_id
        || manifest.redaction_profile != redaction_profile
        || &manifest.replica != replica
        || &manifest.thread_shard_key != shard_key
        || ThreadShardKey::from_replica(&manifest.replica) != manifest.thread_shard_key
    {
        return Err(invalid_data("active fact manifest envelope is invalid"));
    }
    manifest.cursor.validate()?;
    validate_fact_digest_bindings(&manifest.validated_digests)?;
    if let Some(binding) = &manifest.remote_binding {
        binding.validate_namespace(source_id)?;
    }
    if manifest.record_count > MAX_FACT_GENERATION_RECORDS {
        return Err(invalid_data(
            "active fact manifest exceeds the record limit",
        ));
    }
    validate_sorted_unique_days(&manifest.shard_days)
}

fn validate_sorted_unique_days(days: &[NaiveDate]) -> io::Result<()> {
    if days.len() > usize::try_from(MAX_FACT_RETENTION_DAYS).unwrap_or(usize::MAX) {
        return Err(invalid_data(
            "fact shard set exceeds the 35-day retention window",
        ));
    }
    if days.windows(2).any(|window| window[0] >= window[1]) {
        return Err(invalid_data("fact shard days must be sorted and unique"));
    }
    if days.first().zip(days.last()).is_some_and(|(first, last)| {
        last.signed_duration_since(*first).num_days() >= MAX_FACT_RETENTION_DAYS
    }) {
        return Err(invalid_data(
            "fact shard set exceeds the 35-day retention window",
        ));
    }
    Ok(())
}

fn fact_lock_name(shard_key: &ThreadShardKey) -> String {
    format!("{}.lock", shard_key.as_str())
}

fn fact_manifest_path(directory: &Path, shard_key: &ThreadShardKey) -> PathBuf {
    directory.join(format!("{}.json", shard_key.as_str()))
}

fn evidence_shard_path(directory: &Path, day: NaiveDate) -> PathBuf {
    directory.join(format!("{}.json.gz", day.format("%Y-%m-%d")))
}

fn evidence_shard_day(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_str()?;
    let date = name.strip_suffix(".json.gz")?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

fn is_atomic_evidence_temporary_file(name: &OsStr) -> bool {
    is_atomic_shard_temporary_file(name, AtomicShardFileKind::GzipJson)
}

fn is_atomic_fact_manifest_temporary_file(name: &OsStr) -> bool {
    atomic_temporary_target_name(name).is_some_and(|target| {
        target
            .strip_suffix(".json")
            .is_some_and(|key| key.parse::<ThreadShardKey>().is_ok())
    })
}

fn evidence_shard_entries(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    store.validate_private_path(directory)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        if is_atomic_evidence_temporary_file(&entry.file_name()) {
            validate_data_file_metadata(&path, &fs::symlink_metadata(&path)?)?;
            continue;
        }
        let Some(day) = evidence_shard_day(&path) else {
            return Err(invalid_data(format!(
                "unexpected path in session evidence generation {}",
                path.display()
            )));
        };
        entries.push((day, path));
    }
    entries.sort_by_key(|(day, _)| *day);
    Ok(entries)
}

fn evidence_shard_entries_since(
    store: &SourceHistoryStore,
    directory: &Path,
    _since: DateTime<Utc>,
) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    // A digest can start before a query and still cover the query boundary.
    // Retention bounds this directory to 35 days, so scanning all daily shards
    // is both correct and bounded.
    evidence_shard_entries_ignoring_lock(store, directory)
}

fn evidence_shard_entries_ignoring_lock(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    store.validate_private_path(directory)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == OsStr::new(DIGESTS_LOCK_FILE) {
            continue;
        }
        if is_atomic_evidence_temporary_file(&entry.file_name()) {
            validate_data_file_metadata(&path, &fs::symlink_metadata(&path)?)?;
            continue;
        }
        let Some(day) = evidence_shard_day(&path) else {
            return Err(invalid_data(format!(
                "unexpected path in session digest directory {}",
                path.display()
            )));
        };
        entries.push((day, path));
    }
    entries.sort_by_key(|(day, _)| *day);
    Ok(entries)
}

fn remove_atomic_evidence_temporary_files(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<()> {
    cleanup_atomic_shard_temporary_files(store, directory, AtomicShardFileKind::GzipJson)
        .map(|_| ())
}

fn write_gzip_json_atomically<T: Serialize>(
    store: &SourceHistoryStore,
    path: &Path,
    value: &T,
) -> io::Result<()> {
    write_gzip_json_atomically_with_limit(store, path, value, MAX_EVIDENCE_SHARD_BYTES).map(|_| ())
}

fn write_gzip_json_atomically_with_limit<T: Serialize>(
    store: &SourceHistoryStore,
    path: &Path,
    value: &T,
    maximum_decoded_bytes: u64,
) -> io::Result<u64> {
    let encoded = encode_pretty_bounded(value, maximum_decoded_bytes)?;
    let decoded_bytes = u64::try_from(encoded.len())
        .map_err(|_| invalid_data("session evidence encoded size overflowed"))?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&encoded)?;
    let compressed = encoder.finish()?;
    if compressed.len() as u64 > MAX_COMPRESSED_EVIDENCE_SHARD_BYTES {
        return Err(invalid_data(
            "compressed session evidence shard is too large",
        ));
    }
    write_private_atomically_beneath(store, path, &compressed)?;
    Ok(decoded_bytes)
}

fn read_optional_gzip_json_file<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> io::Result<Option<T>> {
    match read_gzip_json_file(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_gzip_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    read_gzip_json_file_with_limit(path, MAX_EVIDENCE_SHARD_BYTES).map(|(value, _)| value)
}

fn read_gzip_json_file_with_limit<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum_decoded_bytes: u64,
) -> io::Result<(T, u64)> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_data_file_metadata(path, &path_metadata)?;
    if path_metadata.len() > MAX_COMPRESSED_EVIDENCE_SHARD_BYTES {
        return Err(invalid_data(format!(
            "compressed session evidence file {} is too large",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let subject = format!("session evidence path {}", path.display());
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, &subject))?;
    let metadata = file.metadata()?;
    validate_data_file_metadata(path, &metadata)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &metadata, &subject)?;
    if metadata.len() > MAX_COMPRESSED_EVIDENCE_SHARD_BYTES {
        return Err(invalid_data(format!(
            "compressed session evidence file {} is too large",
            path.display()
        )));
    }
    let mut decoded = Vec::new();
    GzDecoder::new(file)
        .take(maximum_decoded_bytes.saturating_add(1))
        .read_to_end(&mut decoded)?;
    if decoded.len() as u64 > maximum_decoded_bytes {
        return Err(invalid_data(format!(
            "decompressed session evidence file {} is too large",
            path.display()
        )));
    }
    let decoded_bytes = u64::try_from(decoded.len())
        .map_err(|_| invalid_data("session evidence decoded size overflowed"))?;
    let value = serde_json::from_slice(&decoded).map_err(|error| {
        invalid_data(format!(
            "session evidence file {} is invalid: {error}",
            path.display()
        ))
    })?;
    Ok((value, decoded_bytes))
}

fn write_private_atomically_beneath(
    store: &SourceHistoryStore,
    path: &Path,
    contents: &[u8],
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("private file has no parent"))?;
    store.prepare_private_directory(parent)?;
    write_private_atomically(path, contents)?;
    store.validate_private_path(parent)
}

fn sync_store_directory(store: &SourceHistoryStore, path: &Path) -> io::Result<()> {
    store.validate_private_path(path)?;
    sync_directory(path)
}

fn create_new_private_directory(store: &SourceHistoryStore, path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("private directory has no parent"))?;
    store.prepare_private_directory(parent)?;
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    builder.create(path)?;
    store.validate_private_path(path)
}

fn remove_private_tree(store: &SourceHistoryStore, path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("session evidence cleanup path has no parent"))?;
    store.validate_private_path(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
                return Err(invalid_data(format!(
                    "session evidence cleanup path {} is not a directory",
                    path.display()
                )));
            }
            ensure_private_directory(&metadata)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    store.validate_private_path(path)?;
    for entry in fs::read_dir(path)? {
        store.validate_private_path(path)?;
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err(invalid_data(format!(
                "session evidence cleanup refuses link or reparse point {}",
                child.display()
            )));
        }
        if metadata.file_type().is_dir() {
            store.validate_private_path(&child)?;
            remove_private_tree(store, &child)?;
        } else {
            validate_data_file_metadata(&child, &metadata)?;
            store.validate_private_path(path)?;
            fs::remove_file(&child)?;
        }
    }
    store.validate_private_path(path)?;
    fs::remove_dir(path)?;
    sync_store_directory(store, parent)?;
    Ok(())
}

fn ensure_fact_namespace_within_cap(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
) -> io::Result<()> {
    ensure_fact_namespace_with_reserve(store, source_id, redaction_profile, 0, 0)
}

fn try_lock_exclusive_for_fact_publication(
    file: &std::fs::File,
    directory: &Path,
    name: &str,
) -> io::Result<()> {
    match fs2::FileExt::try_lock_exclusive(file) {
        Ok(()) => validate_locked_file(file, directory, name),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "fact publication lock is busy; retry later",
        )),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FactNamespaceUsage {
    bytes: u64,
    entries: u64,
}

fn ensure_fact_namespace_with_reserve(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    reserve_bytes: u64,
    reserve_entries: u64,
) -> io::Result<()> {
    let usage = FactNamespaceUsage {
        bytes: reserve_bytes,
        entries: reserve_entries,
    };
    validate_fact_namespace_usage(usage, MAX_FACT_NAMESPACE_BYTES, MAX_FACT_NAMESPACE_ENTRIES)?;
    fact_namespace_usage_bounded(
        store,
        source_id,
        redaction_profile,
        usage,
        MAX_FACT_NAMESPACE_BYTES,
        MAX_FACT_NAMESPACE_ENTRIES,
    )?;
    Ok(())
}

fn fact_namespace_usage_bounded(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    mut usage: FactNamespaceUsage,
    maximum_bytes: u64,
    maximum_entries: u64,
) -> io::Result<FactNamespaceUsage> {
    validate_fact_namespace_usage(usage, maximum_bytes, maximum_entries)?;
    for directory in [
        store.source_facts_directory(source_id, redaction_profile),
        store.source_fact_staging_directory(source_id, redaction_profile),
        store.source_fact_manifests_directory(source_id, redaction_profile),
    ] {
        usage =
            private_tree_usage_bounded(store, &directory, usage, maximum_bytes, maximum_entries)?;
    }
    Ok(usage)
}

fn validate_fact_namespace_usage(
    usage: FactNamespaceUsage,
    maximum_bytes: u64,
    maximum_entries: u64,
) -> io::Result<()> {
    if usage.bytes > maximum_bytes {
        return Err(invalid_data("fact namespace exceeds the 512 MiB hard cap"));
    }
    if usage.entries > maximum_entries {
        return Err(invalid_data("fact namespace exceeds its entry hard cap"));
    }
    Ok(())
}

fn private_tree_usage_bounded(
    store: &SourceHistoryStore,
    path: &Path,
    mut usage: FactNamespaceUsage,
    maximum_bytes: u64,
    maximum_entries: u64,
) -> io::Result<FactNamespaceUsage> {
    if !store.private_directory_exists(path)? {
        return Ok(usage);
    }
    for entry in fs::read_dir(path)? {
        store.validate_private_path(path)?;
        let child = entry?.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err(invalid_data(format!(
                "session evidence namespace refuses link or reparse point {}",
                child.display()
            )));
        }
        usage.entries = usage
            .entries
            .checked_add(1)
            .ok_or_else(|| invalid_data("fact namespace entry count overflowed"))?;
        validate_fact_namespace_usage(usage, maximum_bytes, maximum_entries)?;
        if metadata.file_type().is_dir() {
            store.validate_private_path(&child)?;
            usage =
                private_tree_usage_bounded(store, &child, usage, maximum_bytes, maximum_entries)?;
        } else {
            validate_data_file_metadata(&child, &metadata)?;
            usage.bytes = usage
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| invalid_data("fact namespace size overflowed"))?;
            validate_fact_namespace_usage(usage, maximum_bytes, maximum_entries)?;
        }
    }
    Ok(usage)
}

pub(super) fn earliest_session_evidence_time(
    store: &SourceHistoryStore,
    sources: &[SourceMetadata],
    redaction_profiles: &[RedactionProfile],
) -> io::Result<Option<DateTime<Utc>>> {
    let mut earliest = None;
    for source in sources {
        for &redaction_profile in redaction_profiles {
            let digests = store.source_digests_directory(source.source_id(), redaction_profile);
            if store.private_directory_exists(&digests)? {
                let lock = open_lock_file(&digests, DIGESTS_LOCK_FILE)?;
                lock_shared(&lock, &digests, DIGESTS_LOCK_FILE)?;
                if let Some(day) = evidence_shard_entries_ignoring_lock(store, &digests)?
                    .into_iter()
                    .map(|(day, _)| day)
                    .min()
                {
                    let timestamp = day
                        .and_hms_opt(0, 0, 0)
                        .expect("midnight is always valid")
                        .and_utc();
                    earliest = Some(
                        earliest.map_or(timestamp, |current: DateTime<Utc>| current.min(timestamp)),
                    );
                }
            }
            let manifests =
                store.source_fact_manifests_directory(source.source_id(), redaction_profile);
            if !store.private_directory_exists(&manifests)? {
                continue;
            }
            for (shard_key, path) in fact_manifest_entries(store, &manifests)? {
                let manifest: ActiveFactManifest = read_json_file(&path, MAX_FACT_MANIFEST_BYTES)?;
                validate_active_manifest(
                    &manifest,
                    &store.profile_id,
                    source.source_id(),
                    redaction_profile,
                    &manifest.replica,
                    &shard_key,
                )?;
                if let Some(day) = manifest.shard_days.first() {
                    let timestamp = day
                        .and_hms_opt(0, 0, 0)
                        .expect("midnight is always valid")
                        .and_utc();
                    earliest = Some(
                        earliest.map_or(timestamp, |current: DateTime<Utc>| current.min(timestamp)),
                    );
                }
            }
        }
    }
    Ok(earliest)
}

pub(super) fn garbage_collect_session_evidence_for_source(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    cutoff_day: NaiveDate,
    trusted_at: DateTime<Utc>,
) -> io::Result<usize> {
    let mut pruned = prune_digest_evidence(store, source_id, redaction_profile, cutoff_day)?;
    pruned += prune_active_fact_evidence(store, source_id, redaction_profile, cutoff_day)?;
    garbage_collect_fact_artifacts(store, source_id, redaction_profile, trusted_at)?;
    Ok(pruned)
}

fn prune_digest_evidence(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    cutoff_day: NaiveDate,
) -> io::Result<usize> {
    let directory = store.source_digests_directory(source_id, redaction_profile);
    if !store.private_directory_exists(&directory)? {
        return Ok(0);
    }
    let lock = open_lock_file(&directory, DIGESTS_LOCK_FILE)?;
    lock_exclusive(&lock, &directory, DIGESTS_LOCK_FILE)?;
    remove_atomic_evidence_temporary_files(store, &directory)?;
    let cutoff = cutoff_day
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc();
    let mut pruned = 0;
    for (day, path) in evidence_shard_entries_ignoring_lock(store, &directory)? {
        if day >= cutoff_day {
            continue;
        }
        let Some(mut shard) =
            read_digest_shard(&path, &store.profile_id, source_id, redaction_profile, day)?
        else {
            continue;
        };
        let before = shard.records.len();
        shard
            .records
            .retain(|record| record.retention_through() >= cutoff);
        if shard.records.len() == before {
            continue;
        }
        if shard.records.is_empty() {
            store.validate_private_path(&directory)?;
            fs::remove_file(&path)?;
            pruned += 1;
        } else {
            sort_digest_records(&mut shard.records);
            write_gzip_json_atomically(store, &path, &shard)?;
        }
    }
    if pruned > 0 {
        sync_store_directory(store, &directory)?;
    }
    Ok(pruned)
}

fn prune_active_fact_evidence(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    cutoff_day: NaiveDate,
) -> io::Result<usize> {
    let manifests = store.source_fact_manifests_directory(source_id, redaction_profile);
    if !store.private_directory_exists(&manifests)? {
        return Ok(0);
    }
    let cutoff = cutoff_day
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc();
    let mut pruned = 0;
    for (shard_key, _) in fact_manifest_entries(store, &manifests)? {
        let lock_name = fact_lock_name(&shard_key);
        let lock = open_lock_file(&manifests, &lock_name)?;
        lock_exclusive(&lock, &manifests, &lock_name)?;
        let path = fact_manifest_path(&manifests, &shard_key);
        let manifest: ActiveFactManifest = read_json_file(&path, MAX_FACT_MANIFEST_BYTES)?;
        validate_active_manifest(
            &manifest,
            &store.profile_id,
            source_id,
            redaction_profile,
            &manifest.replica,
            &shard_key,
        )?;
        let retained_since = manifest
            .retained_since
            .map_or(cutoff, |existing| existing.max(cutoff));
        if manifest.retained_since == Some(retained_since) {
            continue;
        }
        let records =
            store.read_fact_generation_unlocked(source_id, redaction_profile, &manifest)?;
        let mut retained = records
            .iter()
            .filter(|record| record.occurred_at() >= retained_since)
            .cloned()
            .collect::<Vec<_>>();
        if retained.len() == records.len() {
            let replacement_manifest = ActiveFactManifest {
                retained_since: Some(retained_since),
                ..manifest
            };
            write_private_atomically_beneath(
                store,
                &path,
                &encode_pretty_bounded(&replacement_manifest, MAX_FACT_MANIFEST_BYTES)?,
            )?;
            continue;
        }
        sort_fact_records(&mut retained);
        let replacement = FactBatchId::generate()?;
        let facts_thread = store
            .source_facts_directory(source_id, redaction_profile)
            .join(shard_key.as_str());
        store.prepare_private_directory(&facts_thread)?;
        let generation_directory = facts_thread.join(replacement.as_str());
        create_new_private_directory(store, &generation_directory)?;
        let binding = FactGenerationBinding {
            profile_id: &store.profile_id,
            source_id,
            redaction_profile,
            replica: &manifest.replica,
            shard_key: &shard_key,
            generation: &replacement,
        };
        let result = write_fact_generation(store, &generation_directory, &binding, &retained);
        let shard_days = match result {
            Ok(days) => days,
            Err(error) => {
                let _ = remove_private_tree(store, &generation_directory);
                return Err(error);
            }
        };
        let replacement_manifest = ActiveFactManifest {
            active_generation: replacement,
            retained_since: Some(retained_since),
            shard_days,
            record_count: retained.len(),
            ..manifest.clone()
        };
        if let Err(error) = write_private_atomically_beneath(
            store,
            &path,
            &encode_pretty_bounded(&replacement_manifest, MAX_FACT_MANIFEST_BYTES)?,
        ) {
            let _ = remove_private_tree(store, &generation_directory);
            return Err(error);
        }
        let old_generation = facts_thread.join(manifest.active_generation.as_str());
        remove_private_tree(store, &old_generation)?;
        pruned += manifest
            .shard_days
            .iter()
            .filter(|day| **day < cutoff_day)
            .count();
    }
    Ok(pruned)
}

fn garbage_collect_fact_artifacts(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    trusted_at: DateTime<Utc>,
) -> io::Result<()> {
    let staging_root = store.source_fact_staging_directory(source_id, redaction_profile);
    let facts_root = store.source_facts_directory(source_id, redaction_profile);
    if !store.private_directory_exists(&staging_root)?
        && !store.private_directory_exists(&facts_root)?
    {
        return Ok(());
    }
    store.prepare_private_directory(&staging_root)?;
    let staging_lock = open_lock_file(&staging_root, FACT_STAGING_LOCK_FILE)?;
    lock_exclusive(&staging_lock, &staging_root, FACT_STAGING_LOCK_FILE)?;
    let staged_references = garbage_collect_fact_staging_unlocked(
        store,
        source_id,
        redaction_profile,
        trusted_at,
        &staging_root,
    )?;
    garbage_collect_orphan_fact_generations_unlocked(
        store,
        source_id,
        redaction_profile,
        &staged_references,
    )?;
    ensure_fact_namespace_within_cap(store, source_id, redaction_profile)
}

fn garbage_collect_fact_staging_unlocked(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    trusted_at: DateTime<Utc>,
    staging_root: &Path,
) -> io::Result<BTreeSet<(ThreadShardKey, FactBatchId)>> {
    store.validate_private_path(staging_root)?;
    let expires_before = trusted_at
        .checked_sub_signed(Duration::hours(FACT_STAGING_TTL_HOURS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let mut staged_references = BTreeSet::new();
    for entry in fs::read_dir(staging_root)? {
        store.validate_private_path(staging_root)?;
        let entry = entry?;
        let file_name = entry.file_name();
        if file_name == OsStr::new(FACT_STAGING_LOCK_FILE) {
            validate_data_file_metadata(&entry.path(), &fs::symlink_metadata(entry.path())?)?;
            continue;
        }
        let batch_id = file_name
            .to_str()
            .ok_or_else(|| invalid_data("fact staging directory name is not UTF-8"))?
            .parse::<FactBatchId>()
            .map_err(|error| invalid_data(error.to_string()))?;
        let path = entry.path();
        store.validate_private_path(&path)?;
        let descriptor_path = path.join(STAGED_BATCH_FILE);
        let descriptor = match read_staged_batch(
            &descriptor_path,
            &store.profile_id,
            source_id,
            redaction_profile,
            &batch_id,
        ) {
            Ok(descriptor) => Some(descriptor),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        // TTL is based on the center's local filesystem timestamp, never a
        // remote-provided event or completion timestamp.
        let staged_at = DateTime::<Utc>::from(fs::symlink_metadata(&path)?.modified()?);
        if staged_at < expires_before {
            remove_private_tree(store, &path)?;
        } else if let Some(descriptor) = descriptor {
            staged_references.insert((descriptor.thread_shard_key, descriptor.batch_id));
        }
    }
    Ok(staged_references)
}

fn garbage_collect_orphan_fact_generations_unlocked(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    staged_references: &BTreeSet<(ThreadShardKey, FactBatchId)>,
) -> io::Result<()> {
    let facts_root = store.source_facts_directory(source_id, redaction_profile);
    if !store.private_directory_exists(&facts_root)? {
        return Ok(());
    }
    let manifests = store.source_fact_manifests_directory(source_id, redaction_profile);
    store.prepare_private_directory(&manifests)?;
    for entry in fs::read_dir(&facts_root)? {
        store.validate_private_path(&facts_root)?;
        let entry = entry?;
        let thread_path = entry.path();
        store.validate_private_path(&thread_path)?;
        let shard_key = entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("fact thread directory name is not UTF-8"))?
            .parse::<ThreadShardKey>()
            .map_err(|error| invalid_data(error.to_string()))?;
        let lock_name = fact_lock_name(&shard_key);
        let manifest_lock = open_lock_file(&manifests, &lock_name)?;
        lock_exclusive(&manifest_lock, &manifests, &lock_name)?;

        let manifest_path = fact_manifest_path(&manifests, &shard_key);
        let active_generation = match read_optional_json_file::<ActiveFactManifest>(
            &manifest_path,
            MAX_FACT_MANIFEST_BYTES,
        )? {
            Some(manifest) => {
                validate_active_manifest(
                    &manifest,
                    &store.profile_id,
                    source_id,
                    redaction_profile,
                    &manifest.replica,
                    &shard_key,
                )?;
                Some(manifest.active_generation)
            }
            None => None,
        };

        for generation_entry in fs::read_dir(&thread_path)? {
            store.validate_private_path(&thread_path)?;
            let generation_entry = generation_entry?;
            let generation_path = generation_entry.path();
            store.validate_private_path(&generation_path)?;
            let generation = generation_entry
                .file_name()
                .to_str()
                .ok_or_else(|| invalid_data("fact generation directory name is not UTF-8"))?
                .parse::<FactBatchId>()
                .map_err(|error| invalid_data(error.to_string()))?;
            if active_generation.as_ref() == Some(&generation)
                || staged_references.contains(&(shard_key.clone(), generation.clone()))
            {
                continue;
            }
            remove_private_tree(store, &generation_path)?;
        }
        if fs::read_dir(&thread_path)?.next().is_none() {
            store.validate_private_path(&thread_path)?;
            fs::remove_dir(&thread_path)?;
            sync_store_directory(store, &facts_root)?;
        }
    }
    Ok(())
}

fn fact_manifest_entries(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<Vec<(ThreadShardKey, PathBuf)>> {
    store.validate_private_path(directory)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        store.validate_private_path(directory)?;
        let entry = entry?;
        let path = entry.path();
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("fact manifest path is not UTF-8"))?
            .to_owned();
        if is_atomic_fact_manifest_temporary_file(&entry.file_name()) {
            validate_data_file_metadata(&path, &fs::symlink_metadata(&path)?)?;
            continue;
        }
        if name.ends_with(".lock") {
            let key = name
                .strip_suffix(".lock")
                .expect("checked suffix")
                .parse::<ThreadShardKey>()
                .map_err(|error| invalid_data(error.to_string()))?;
            let _ = key;
            continue;
        }
        let key = name
            .strip_suffix(".json")
            .ok_or_else(|| invalid_data("unexpected fact manifest path"))?
            .parse::<ThreadShardKey>()
            .map_err(|error| invalid_data(error.to_string()))?;
        entries.push((key, path));
    }
    entries.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;
    use crate::domain::PicoUsd;

    const PROFILE: &str = "0123456789abcdef";
    const SOURCE: &str = "node-0123456789abcdef0123456789abcdef";

    fn at(month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, month, day, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn source_id() -> NodeId {
        SOURCE.parse().unwrap()
    }

    fn store(root: &Path) -> SourceHistoryStore {
        let store = SourceHistoryStore::new(root.join("state-root"), PROFILE.parse().unwrap());
        store
            .save_source_metadata(
                &SourceMetadata::new(source_id(), SourceKind::Local, "build-host").unwrap(),
            )
            .unwrap();
        store
    }

    fn thread(value: &str) -> ThreadId {
        value.parse().unwrap()
    }

    fn replica(value: &str) -> SessionReplicaKey {
        SessionReplicaKey::new(source_id(), thread(value))
    }

    fn project(hex: char) -> ObservedProjectKey {
        format!("opk-hmac-sha256-v1-{}", hex.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    fn fingerprint(hex: char) -> SessionDigestFingerprint {
        format!("{DIGEST_FINGERPRINT_PREFIX}{}", hex.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    fn metrics(total: u64) -> SessionUsageMetrics {
        SessionUsageMetrics {
            token_usage: TokenUsage {
                input_tokens: total.saturating_sub(1),
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: u64::from(total > 0),
                reasoning_output_tokens: 0,
                total_tokens: total,
            },
            estimated_cost_units: u128::from(total) * 10,
            api_long_context_extra_cost_units: Some(0),
            api_equivalent_cost: ApiCostAmount {
                minimum_pico_usd: PicoUsd::new(u128::from(total) * 2),
                maximum_pico_usd: PicoUsd::new(u128::from(total) * 2),
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: total,
                priced_tokens: total,
            },
            call_count: 1,
            metric_revision: 1,
            estimator_revision: 1,
            project_breakdown_revision: 1,
            api_pricing_catalog_revision: 1,
            partial_reasons: Vec::new(),
        }
    }

    fn digest(
        thread_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        total: u64,
    ) -> SourceSessionDigest {
        SourceSessionDigest::new(
            replica(thread_id),
            start,
            end,
            end,
            fingerprint('a'),
            fingerprint('b'),
            1,
            true,
            true,
            vec![project('b')],
            metrics(total),
        )
        .unwrap()
    }

    fn fact(
        thread_id: &str,
        event_id: &str,
        occurred_at: DateTime<Utc>,
        total: u64,
    ) -> UsageEventFact {
        fact_with_project(thread_id, event_id, occurred_at, total, project('c'))
    }

    fn fact_with_project(
        thread_id: &str,
        event_id: &str,
        occurred_at: DateTime<Utc>,
        total: u64,
        observed_project_key: ObservedProjectKey,
    ) -> UsageEventFact {
        UsageEventFact::new(
            replica(thread_id),
            event_id.parse().unwrap(),
            occurred_at,
            observed_project_key,
            Some("turn-root".to_string()),
            None,
            Some(thread(thread_id)),
            thread(thread_id),
            Some("turn-root".to_string()),
            Some("gpt-5.6-sol".to_string()),
            Some("standard".to_string()),
            metrics(total).token_usage,
            true,
            true,
            metrics(total),
        )
        .unwrap()
    }

    fn canonical_digest_for_fact(
        fact: &UsageEventFact,
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
    ) -> SourceSessionDigest {
        let (fingerprint, project_breakdown_fingerprint) =
            crate::source_export::canonical_fact_fingerprints_for_test(
                fact.replica(),
                range_start,
                range_end,
                &[fact],
            )
            .unwrap();
        SourceSessionDigest::new(
            fact.replica().clone(),
            range_start,
            range_end,
            range_end,
            fingerprint,
            project_breakdown_fingerprint,
            1,
            true,
            true,
            vec![fact.observed_project_key().clone()],
            fact.metrics().clone(),
        )
        .unwrap()
    }

    fn batch(
        id: FactBatchId,
        kind: FactBatchKind,
        thread_id: &str,
        expected: Option<ActiveFactVersion>,
        activate: FactCursor,
        completed_at: DateTime<Utc>,
        changes: Vec<UsageEventFactRecord>,
    ) -> CompleteFactBatch {
        CompleteFactBatch {
            batch_id: id,
            kind,
            replica: replica(thread_id),
            expected_active_version: expected,
            remote_binding: None,
            validated_digests: Vec::new(),
            activate_cursor: activate,
            completed_at,
            changes,
        }
    }

    fn version(id: &FactBatchId, cursor: FactCursor) -> ActiveFactVersion {
        ActiveFactVersion {
            active_generation: id.clone(),
            cursor,
            remote_binding: None,
            validated_digests: Vec::new(),
            retained_since: None,
        }
    }

    #[test]
    fn evidence_identities_are_bounded_and_path_safe_where_required() {
        assert!("event:opaque/allowed".parse::<UsageEventId>().is_ok());
        assert!(" event".parse::<UsageEventId>().is_err());
        assert!("event\nvalue".parse::<UsageEventId>().is_err());
        assert!(
            format!("event-{}", "x".repeat(MAX_USAGE_EVENT_ID_BYTES))
                .parse::<UsageEventId>()
                .is_err()
        );

        let batch = FactBatchId::generate().unwrap();
        assert_eq!(batch, batch.as_str().parse().unwrap());
        assert!(
            batch
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        assert!(
            format!("{DIGEST_FINGERPRINT_PREFIX}{}", "A".repeat(64))
                .parse::<SessionDigestFingerprint>()
                .is_err()
        );
    }

    #[test]
    fn digest_store_applies_revision_tombstone_and_redaction_namespaces() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let start = at(8, 28, 1);
        let end = start + Duration::hours(2);
        let first =
            SourceSessionDigestRecord::upsert(1, digest("thread-a", start, end, 10)).unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                std::slice::from_ref(&first),
            )
            .unwrap();
        let repeated = store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                std::slice::from_ref(&first),
            )
            .unwrap();
        assert_eq!(repeated.shards_skipped, 1);

        let mut stronger_digest = digest("thread-a", start, end, 20);
        stronger_digest.fingerprint = fingerprint('d');
        let stronger = SourceSessionDigestRecord::upsert(2, stronger_digest).unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[stronger],
            )
            .unwrap();
        let loaded = store
            .load_source_session_digest_records_since(
                &source_id(),
                RedactionProfile::Redacted,
                start,
            )
            .unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].revision(), 2);
        assert!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::PreviewEnabled,
                    start,
                )
                .unwrap()
                .records
                .is_empty()
        );

        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[SourceSessionDigestRecord::tombstone(
                    thread("thread-a"),
                    start,
                    end,
                    end + Duration::minutes(1),
                    3,
                )
                .unwrap()],
            )
            .unwrap();
        assert!(matches!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::Redacted,
                    start,
                )
                .unwrap()
                .records[0]
                .change(),
            SourceSessionDigestChange::Tombstone
        ));

        let shard = evidence_shard_path(
            &store.source_digests_directory(&source_id(), RedactionProfile::Redacted),
            start.date_naive(),
        );
        assert_eq!(shard.extension().and_then(OsStr::to_str), Some("gz"));
        let decoded: serde_json::Value = read_gzip_json_file(&shard).unwrap();
        let text = decoded.to_string();
        assert!(!text.contains("prompt") && !text.contains("messagePreview"));
    }

    #[test]
    fn equal_revision_conflicts_fail_closed_before_replacing_digest() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let start = at(8, 28, 1);
        let first = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-a", start, start + Duration::hours(1), 10),
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[first],
            )
            .unwrap();
        let conflict = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-a", start, start + Duration::hours(1), 99),
        )
        .unwrap();
        assert_eq!(
            store
                .record_source_session_digest_changes(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &[conflict],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn gc_retains_crossing_tombstone_and_rejects_late_old_upsert() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let start = at(7, 20, 0);
        let end = at(8, 1, 0);
        let old = SourceSessionDigestRecord::upsert(1, digest("thread-tombstone", start, end, 10))
            .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                std::slice::from_ref(&old),
            )
            .unwrap();
        let tombstone = SourceSessionDigestRecord::tombstone(
            thread("thread-tombstone"),
            start,
            end,
            at(8, 2, 0),
            2,
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                std::slice::from_ref(&tombstone),
            )
            .unwrap();

        prune_digest_evidence(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            at(7, 26, 0).date_naive(),
        )
        .unwrap();
        assert_eq!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::Redacted,
                    at(7, 26, 0),
                )
                .unwrap()
                .records,
            vec![tombstone.clone()]
        );

        let report = store
            .record_source_session_digest_changes(&source_id(), RedactionProfile::Redacted, &[old])
            .unwrap();
        assert_eq!(report.shards_skipped, 1);
        assert_eq!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::Redacted,
                    at(7, 26, 0),
                )
                .unwrap()
                .records,
            vec![tombstone]
        );
    }

    #[test]
    fn shorter_digest_revision_cannot_drop_the_prior_retention_horizon() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let start = at(7, 20, 0);
        let original_end = at(8, 1, 0);
        let old = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-short-tombstone", start, original_end, 10),
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                std::slice::from_ref(&old),
            )
            .unwrap();

        let short_end = start + Duration::hours(1);
        let tombstone = SourceSessionDigestRecord::tombstone(
            thread("thread-short-tombstone"),
            start,
            short_end,
            short_end,
            2,
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[tombstone],
            )
            .unwrap();

        let cutoff = at(7, 26, 0);
        prune_digest_evidence(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            cutoff.date_naive(),
        )
        .unwrap();
        let retained = store
            .load_source_session_digest_records_since(
                &source_id(),
                RedactionProfile::Redacted,
                cutoff,
            )
            .unwrap();
        assert_eq!(retained.records.len(), 1);
        assert_eq!(retained.records[0].revision(), 2);
        assert_eq!(retained.records[0].retention_through(), original_end);
        assert!(matches!(
            retained.records[0].change(),
            SourceSessionDigestChange::Tombstone
        ));

        let replay = store
            .record_source_session_digest_changes(&source_id(), RedactionProfile::Redacted, &[old])
            .unwrap();
        assert_eq!(replay.shards_skipped, 1);
        assert!(matches!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::Redacted,
                    cutoff,
                )
                .unwrap()
                .records[0]
                .change(),
            SourceSessionDigestChange::Tombstone
        ));
    }

    #[test]
    fn explicit_digest_retention_survives_round_trip_and_early_gc() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let start = at(7, 20, 0);
        let end = start + Duration::hours(1);
        let retention_through = at(8, 25, 0);
        let record = SourceSessionDigestRecord::upsert_with_retention_through(
            7,
            digest("thread-imported-retention", start, end, 10),
            retention_through,
        )
        .unwrap();

        let encoded = serde_json::to_vec(&record).unwrap();
        let decoded: SourceSessionDigestRecord = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(decoded.retention_through(), retention_through);

        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[record],
            )
            .unwrap();
        let cutoff = at(7, 26, 0);
        prune_digest_evidence(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            cutoff.date_naive(),
        )
        .unwrap();

        let retained = store
            .load_source_session_digest_records_since(
                &source_id(),
                RedactionProfile::Redacted,
                cutoff,
            )
            .unwrap();
        assert_eq!(retained.records.len(), 1);
        assert_eq!(retained.records[0].retention_through(), retention_through);

        let error = SourceSessionDigestRecord::tombstone_with_retention_through(
            thread("thread-invalid-retention"),
            start,
            end,
            end,
            start,
            8,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn staged_snapshot_is_invisible_until_atomic_manifest_activation() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let cursor = FactCursor::new(7, 10).unwrap();
        let batch_id = FactBatchId::generate().unwrap();
        let snapshot = batch(
            batch_id.clone(),
            FactBatchKind::Snapshot,
            "thread-a",
            None,
            cursor,
            at(8, 28, 2),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-a", "event-1", at(8, 28, 1), 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &snapshot)
            .unwrap();
        assert!(
            store
                .load_active_fact_set(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &thread("thread-a"),
                )
                .unwrap()
                .is_none()
        );

        let staging = store
            .source_fact_staging_directory(&source_id(), RedactionProfile::Redacted)
            .join(batch_id.as_str());
        let descriptor = fs::read_to_string(staging.join(STAGED_BATCH_FILE)).unwrap();
        assert!(!descriptor.contains("pageToken"));
        let report = store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &batch_id)
            .unwrap();
        assert!(report.activated);
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-a"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.cursor, cursor);
        assert_eq!(active.facts().len(), 1);
        assert_eq!(active.facts()[0].event_id().as_str(), "event-1");
        assert!(
            store
                .load_active_fact_set(
                    &source_id(),
                    RedactionProfile::PreviewEnabled,
                    &thread("thread-a"),
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn staged_facts_reject_a_forged_event_fingerprint_independently() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let day_start = at(8, 28, 0);
        let day_end = day_start + Duration::days(1);
        let valid_fact = fact("thread-bound", "event-forged", at(8, 28, 1), 10);
        let canonical = canonical_digest_for_fact(&valid_fact, day_start, day_end);
        let forged_event_digest = SourceSessionDigest::new(
            canonical.replica().clone(),
            canonical.range_start(),
            canonical.range_end(),
            canonical.covered_through(),
            fingerprint('a'),
            canonical.project_breakdown_fingerprint().clone(),
            canonical.event_count(),
            canonical.exact_event_identity(),
            canonical.coverage_complete(),
            canonical.observed_project_keys().to_vec(),
            canonical.metrics().clone(),
        )
        .unwrap();
        let batch_id = FactBatchId::generate().unwrap();
        let mut snapshot = batch(
            batch_id,
            FactBatchKind::Snapshot,
            "thread-bound",
            None,
            FactCursor::new(7, 1).unwrap(),
            at(8, 28, 2),
            vec![UsageEventFactRecord::upsert(1, valid_fact).unwrap()],
        );
        snapshot.validated_digests =
            vec![FactDigestBinding::from_digest(&forged_event_digest).unwrap()];

        let error = store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &snapshot)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("event fingerprint"));
    }

    #[test]
    fn staged_facts_reject_a_forged_project_fingerprint_independently() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let day_start = at(8, 28, 0);
        let day_end = day_start + Duration::days(1);
        let original = fact_with_project(
            "thread-project",
            "event-project",
            at(8, 28, 1),
            10,
            project('c'),
        );
        let canonical = canonical_digest_for_fact(&original, day_start, day_end);
        // Project attribution is not an event-semantic input, so this keeps
        // the canonical event fingerprint unchanged while changing only the
        // independently committed project breakdown.
        let forged_project_fact = fact_with_project(
            "thread-project",
            "event-project",
            at(8, 28, 1),
            10,
            project('d'),
        );
        let mut snapshot = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Snapshot,
            "thread-project",
            None,
            FactCursor::new(8, 1).unwrap(),
            at(8, 28, 2),
            vec![UsageEventFactRecord::upsert(1, forged_project_fact).unwrap()],
        );
        snapshot.validated_digests = vec![FactDigestBinding::from_digest(&canonical).unwrap()];

        let error = store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &snapshot)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("project-breakdown fingerprint"));
    }

    #[test]
    fn delta_is_copy_on_write_and_cursor_cas_rejects_stale_batches() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let first_cursor = FactCursor::new(3, 1).unwrap();
        let first_id = FactBatchId::generate().unwrap();
        let first = batch(
            first_id.clone(),
            FactBatchKind::Snapshot,
            "thread-a",
            None,
            first_cursor,
            at(8, 27, 2),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-a", "event-1", at(8, 27, 1), 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &first)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &first_id)
            .unwrap();

        let second_cursor = FactCursor::new(3, 4).unwrap();
        let second_id = FactBatchId::generate().unwrap();
        let second = batch(
            second_id.clone(),
            FactBatchKind::Delta,
            "thread-a",
            Some(version(&first_id, first_cursor)),
            second_cursor,
            at(8, 28, 3),
            vec![
                UsageEventFactRecord::upsert(2, fact("thread-a", "event-1", at(8, 27, 1), 20))
                    .unwrap(),
                UsageEventFactRecord::upsert(1, fact("thread-a", "event-2", at(8, 28, 2), 30))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &second)
            .unwrap();
        let before = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-a"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(before.cursor, first_cursor);
        assert_eq!(before.facts()[0].metrics().token_usage.total_tokens, 10);
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &second_id)
            .unwrap();
        let after = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-a"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(after.cursor, second_cursor);
        assert_eq!(after.facts().len(), 2);
        assert!(
            !store
                .source_facts_directory(&source_id(), RedactionProfile::Redacted)
                .join(ThreadShardKey::from_replica(&replica("thread-a")).as_str())
                .join(first_id.as_str())
                .exists()
        );

        let stale = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Delta,
            "thread-a",
            Some(version(&first_id, first_cursor)),
            FactCursor::new(3, 5).unwrap(),
            at(8, 28, 4),
            Vec::new(),
        );
        assert_eq!(
            store
                .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &stale,)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn fact_revision_cannot_move_an_event_to_an_earlier_gc_day() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let occurred_at = at(8, 28, 1);
        let first_cursor = FactCursor::new(11, 1).unwrap();
        let first_id = FactBatchId::generate().unwrap();
        let first = batch(
            first_id.clone(),
            FactBatchKind::Snapshot,
            "thread-time-key",
            None,
            first_cursor,
            at(8, 28, 2),
            vec![
                UsageEventFactRecord::upsert(
                    1,
                    fact("thread-time-key", "event-1", occurred_at, 10),
                )
                .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &first)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &first_id)
            .unwrap();

        let moved_tombstone = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Delta,
            "thread-time-key",
            Some(version(&first_id, first_cursor)),
            FactCursor::new(11, 2).unwrap(),
            at(8, 28, 3),
            vec![
                UsageEventFactRecord::tombstone("event-1".parse().unwrap(), at(7, 1, 0), 2)
                    .unwrap(),
            ],
        );
        let error = store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &moved_tombstone)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed occurredAt"));

        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-time-key"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.version, version(&first_id, first_cursor));
        assert_eq!(active.facts().len(), 1);
        assert_eq!(active.facts()[0].occurred_at(), occurred_at);
    }

    #[test]
    fn gc_retention_floor_blocks_a_late_old_upsert_after_tombstone_pruning() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let occurred_at = at(7, 1, 1);
        let first_cursor = FactCursor::new(12, 1).unwrap();
        let first_id = FactBatchId::generate().unwrap();
        let first = batch(
            first_id.clone(),
            FactBatchKind::Snapshot,
            "thread-retention-floor",
            None,
            first_cursor,
            at(7, 2, 0),
            vec![
                UsageEventFactRecord::upsert(
                    1,
                    fact("thread-retention-floor", "event-old", occurred_at, 10),
                )
                .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &first)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &first_id)
            .unwrap();

        let tombstone_cursor = FactCursor::new(12, 2).unwrap();
        let tombstone_id = FactBatchId::generate().unwrap();
        let tombstone = batch(
            tombstone_id.clone(),
            FactBatchKind::Delta,
            "thread-retention-floor",
            Some(version(&first_id, first_cursor)),
            tombstone_cursor,
            at(7, 3, 0),
            vec![
                UsageEventFactRecord::tombstone("event-old".parse().unwrap(), occurred_at, 2)
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &tombstone)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &tombstone_id)
            .unwrap();

        let cutoff = at(7, 26, 0);
        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            cutoff.date_naive(),
            at(9, 5, 0),
        )
        .unwrap();
        let after_gc = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-retention-floor"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(after_gc.version.retained_since(), Some(cutoff));
        assert!(after_gc.records.is_empty());

        let late_id = FactBatchId::generate().unwrap();
        let late = batch(
            late_id.clone(),
            FactBatchKind::Delta,
            "thread-retention-floor",
            Some(after_gc.version),
            FactCursor::new(12, 3).unwrap(),
            at(8, 28, 0),
            vec![
                UsageEventFactRecord::upsert(
                    1,
                    fact("thread-retention-floor", "event-old", occurred_at, 10),
                )
                .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &late)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &late_id)
            .unwrap();

        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-retention-floor"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.cursor, FactCursor::new(12, 3).unwrap());
        assert_eq!(active.version.retained_since(), Some(cutoff));
        assert!(active.records.is_empty());
        assert!(active.facts().is_empty());
    }

    #[test]
    fn activation_recovers_after_generation_move_before_manifest_publish() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let batch_id = FactBatchId::generate().unwrap();
        let snapshot = batch(
            batch_id.clone(),
            FactBatchKind::Snapshot,
            "thread-a",
            None,
            FactCursor::new(1, 1).unwrap(),
            at(8, 28, 2),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-a", "event-1", at(8, 28, 1), 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &snapshot)
            .unwrap();
        let shard_key = ThreadShardKey::from_replica(&replica("thread-a"));
        let staging = store
            .source_fact_staging_directory(&source_id(), RedactionProfile::Redacted)
            .join(batch_id.as_str());
        let destination = store
            .source_facts_directory(&source_id(), RedactionProfile::Redacted)
            .join(shard_key.as_str())
            .join(batch_id.as_str());
        store
            .prepare_private_directory(destination.parent().unwrap())
            .unwrap();
        fs::rename(staging.join(STAGED_GENERATION_DIRECTORY), &destination).unwrap();
        assert!(
            store
                .load_active_fact_set(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &thread("thread-a"),
                )
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &batch_id,)
                .unwrap()
                .activated
        );
    }

    #[test]
    fn gc_preserves_crossing_digest_and_atomically_rewrites_active_facts() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let cutoff_day = at(7, 26, 0).date_naive();
        let expired =
            SourceSessionDigestRecord::upsert(1, digest("thread-old", at(7, 1, 0), at(7, 2, 0), 5))
                .unwrap();
        let crossing = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-cross", at(7, 20, 0), at(8, 1, 0), 7),
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[expired, crossing.clone()],
            )
            .unwrap();

        let cursor = FactCursor::new(4, 2).unwrap();
        let active_id = FactBatchId::generate().unwrap();
        let snapshot = batch(
            active_id.clone(),
            FactBatchKind::Snapshot,
            "thread-facts",
            None,
            cursor,
            at(8, 2, 0),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-facts", "event-old", at(7, 1, 1), 10))
                    .unwrap(),
                UsageEventFactRecord::upsert(1, fact("thread-facts", "event-new", at(8, 1, 1), 20))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &snapshot)
            .unwrap();
        store
            .activate_staged_fact_batch(
                &source_id(),
                RedactionProfile::Redacted,
                &snapshot.batch_id,
            )
            .unwrap();

        let abandoned_id = FactBatchId::generate().unwrap();
        let abandoned = batch(
            abandoned_id.clone(),
            FactBatchKind::Delta,
            "thread-facts",
            Some(version(&active_id, cursor)),
            FactCursor::new(4, 3).unwrap(),
            at(8, 2, 0),
            Vec::new(),
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &abandoned)
            .unwrap();

        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            cutoff_day,
            at(9, 5, 0),
        )
        .unwrap();
        let digests = store
            .load_source_session_digest_records_since(
                &source_id(),
                RedactionProfile::Redacted,
                at(7, 26, 0),
            )
            .unwrap();
        assert_eq!(digests.records, vec![crossing]);
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-facts"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.cursor, cursor);
        assert_eq!(active.facts().len(), 1);
        assert_eq!(active.facts()[0].event_id().as_str(), "event-new");
        assert!(
            !store
                .source_fact_staging_directory(&source_id(), RedactionProfile::Redacted)
                .join(abandoned_id.as_str())
                .exists()
        );
    }

    #[test]
    fn gc_generation_rewrite_invalidates_a_pre_gc_staged_delta() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let cursor = FactCursor::new(6, 2).unwrap();
        let initial_id = FactBatchId::generate().unwrap();
        let initial = batch(
            initial_id.clone(),
            FactBatchKind::Snapshot,
            "thread-cas",
            None,
            cursor,
            at(8, 28, 3),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-cas", "event-old", at(8, 1, 1), 10))
                    .unwrap(),
                UsageEventFactRecord::upsert(1, fact("thread-cas", "event-new", at(8, 28, 1), 20))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &initial)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &initial_id)
            .unwrap();
        let before = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-cas"),
            )
            .unwrap()
            .unwrap();

        let staged_id = FactBatchId::generate().unwrap();
        let staged = batch(
            staged_id.clone(),
            FactBatchKind::Delta,
            "thread-cas",
            Some(before.version.clone()),
            FactCursor::new(6, 3).unwrap(),
            at(8, 28, 4),
            vec![
                UsageEventFactRecord::upsert(
                    1,
                    fact("thread-cas", "event-staged", at(8, 28, 2), 30),
                )
                .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &staged)
            .unwrap();

        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            at(8, 10, 0).date_naive(),
            Utc::now(),
        )
        .unwrap();
        let after_gc = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-cas"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(after_gc.cursor, cursor);
        assert_ne!(after_gc.version, before.version);
        assert_eq!(after_gc.facts().len(), 1);
        assert_eq!(after_gc.facts()[0].event_id().as_str(), "event-new");

        assert_eq!(
            store
                .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &staged_id,)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let still_pruned = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-cas"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(still_pruned.version, after_gc.version);
        assert_eq!(still_pruned.facts().len(), 1);
    }

    #[test]
    fn same_cursor_batches_cannot_silently_replace_active_facts() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let cursor = FactCursor::new(8, 1).unwrap();
        let initial_id = FactBatchId::generate().unwrap();
        let initial = batch(
            initial_id.clone(),
            FactBatchKind::Snapshot,
            "thread-same",
            None,
            cursor,
            at(8, 28, 2),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-same", "event-1", at(8, 28, 1), 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &initial)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &initial_id)
            .unwrap();
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-same"),
            )
            .unwrap()
            .unwrap();

        let changed_delta = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Delta,
            "thread-same",
            Some(active.version.clone()),
            cursor,
            at(8, 28, 3),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-same", "event-2", at(8, 28, 2), 20))
                    .unwrap(),
            ],
        );
        assert_eq!(
            changed_delta.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let replacement_snapshot = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Snapshot,
            "thread-same",
            Some(active.version.clone()),
            cursor,
            at(8, 28, 3),
            Vec::new(),
        );
        assert_eq!(
            replacement_snapshot.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let no_op_id = FactBatchId::generate().unwrap();
        let no_op_delta = batch(
            no_op_id.clone(),
            FactBatchKind::Delta,
            "thread-same",
            Some(active.version.clone()),
            cursor,
            at(8, 28, 3),
            Vec::new(),
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &no_op_delta)
            .unwrap();
        let report = store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &no_op_id)
            .unwrap();
        assert!(!report.activated);
        assert_eq!(
            store
                .load_active_fact_set(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &thread("thread-same"),
                )
                .unwrap()
                .unwrap()
                .version,
            active.version
        );
    }

    #[test]
    fn artifact_gc_reclaims_orphans_but_preserves_moved_staging_generation() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let cursor = FactCursor::new(9, 1).unwrap();
        let active_id = FactBatchId::generate().unwrap();
        let initial = batch(
            active_id.clone(),
            FactBatchKind::Snapshot,
            "thread-orphan",
            None,
            cursor,
            at(8, 28, 2),
            vec![
                UsageEventFactRecord::upsert(1, fact("thread-orphan", "event-1", at(8, 28, 1), 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &initial)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &active_id)
            .unwrap();
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-orphan"),
            )
            .unwrap()
            .unwrap();

        let moved_id = FactBatchId::generate().unwrap();
        let moved = batch(
            moved_id.clone(),
            FactBatchKind::Delta,
            "thread-orphan",
            Some(active.version),
            FactCursor::new(9, 2).unwrap(),
            at(8, 28, 3),
            Vec::new(),
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &moved)
            .unwrap();
        let shard_key = ThreadShardKey::from_replica(&replica("thread-orphan"));
        let staging = store
            .source_fact_staging_directory(&source_id(), RedactionProfile::Redacted)
            .join(moved_id.as_str());
        let facts_thread = store
            .source_facts_directory(&source_id(), RedactionProfile::Redacted)
            .join(shard_key.as_str());
        let moved_destination = facts_thread.join(moved_id.as_str());
        fs::rename(
            staging.join(STAGED_GENERATION_DIRECTORY),
            &moved_destination,
        )
        .unwrap();
        let orphan_id = FactBatchId::generate().unwrap();
        let orphan = facts_thread.join(orphan_id.as_str());
        create_new_private_directory(&store, &orphan).unwrap();

        garbage_collect_fact_artifacts(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            Utc::now(),
        )
        .unwrap();
        assert!(moved_destination.exists());
        assert!(!orphan.exists());
        assert!(
            store
                .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &moved_id,)
                .unwrap()
                .activated
        );
    }

    #[test]
    fn digest_writer_recovers_only_exact_target_bound_atomic_temps() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let start = at(8, 28, 1);
        let record = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-temp", start, start + Duration::hours(1), 10),
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[record],
            )
            .unwrap();
        let directory = store.source_digests_directory(&source_id(), RedactionProfile::Redacted);
        let temporary = directory.join(".2026-08-28.json.gz.123.456.tmp");
        write_private_atomically_beneath(&store, &temporary, b"interrupted-write").unwrap();
        assert!(is_atomic_evidence_temporary_file(
            temporary.file_name().unwrap()
        ));
        assert!(!is_atomic_evidence_temporary_file(OsStr::new(
            ".unrecognized.tmp"
        )));
        assert_eq!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::Redacted,
                    start,
                )
                .unwrap()
                .records
                .len(),
            1
        );
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[SourceSessionDigestRecord::upsert(
                    2,
                    digest("thread-temp", start, start + Duration::hours(1), 20),
                )
                .unwrap()],
            )
            .unwrap();
        assert!(!temporary.exists());

        let unknown = directory.join(".2026-08-28.json.gz.bad.tmp");
        write_private_atomically_beneath(&store, &unknown, b"not ours").unwrap();
        let error = store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[SourceSessionDigestRecord::upsert(
                    3,
                    digest("thread-temp", start, start + Duration::hours(1), 30),
                )
                .unwrap()],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(unknown.exists());
    }

    #[test]
    fn fact_batch_span_count_and_namespace_limits_fail_closed() {
        let root = tempdir().unwrap();
        let cap_store =
            SourceHistoryStore::new(root.path().join("cap-state"), PROFILE.parse().unwrap());
        assert!(validate_fact_batch_change_count(MAX_FACT_BATCH_CHANGES).is_ok());
        assert_eq!(
            validate_fact_batch_change_count(MAX_FACT_BATCH_CHANGES + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let within_window = vec![
            UsageEventFactRecord::upsert(1, fact("thread-limit", "event-1", at(8, 1, 0), 1))
                .unwrap(),
            UsageEventFactRecord::upsert(1, fact("thread-limit", "event-2", at(9, 4, 0), 1))
                .unwrap(),
        ];
        assert!(validate_fact_record_span(&within_window).is_ok());
        let outside_window = vec![
            within_window[0].clone(),
            UsageEventFactRecord::upsert(1, fact("thread-limit", "event-3", at(9, 5, 0), 1))
                .unwrap(),
        ];
        assert_eq!(
            validate_fact_record_span(&outside_window)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let namespace = cap_store.state_root().join("namespace-cap");
        cap_store.prepare_private_directory(&namespace).unwrap();
        write_private_atomically_beneath(&cap_store, &namespace.join("data"), b"four").unwrap();
        assert_eq!(
            private_tree_usage_bounded(
                &cap_store,
                &namespace,
                FactNamespaceUsage::default(),
                3,
                u64::MAX,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );

        let manifests =
            cap_store.source_fact_manifests_directory(&source_id(), RedactionProfile::Redacted);
        cap_store.prepare_private_directory(&manifests).unwrap();
        write_private_atomically_beneath(&cap_store, &manifests.join("manifest.json"), b"four")
            .unwrap();
        assert_eq!(
            fact_namespace_usage_bounded(
                &cap_store,
                &source_id(),
                RedactionProfile::Redacted,
                FactNamespaceUsage::default(),
                3,
                u64::MAX,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );

        let entry_store =
            SourceHistoryStore::new(root.path().join("entry-state"), PROFILE.parse().unwrap());
        let entry_manifests =
            entry_store.source_fact_manifests_directory(&source_id(), RedactionProfile::Redacted);
        entry_store
            .prepare_private_directory(&entry_manifests)
            .unwrap();
        write_private_atomically_beneath(&entry_store, &entry_manifests.join("zero.lock"), b"")
            .unwrap();
        assert_eq!(
            fact_namespace_usage_bounded(
                &entry_store,
                &source_id(),
                RedactionProfile::Redacted,
                FactNamespaceUsage::default(),
                u64::MAX,
                0,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn private_tree_helpers_reject_paths_outside_the_store_root() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let outside = root.path().join("outside-tree");
        fs::create_dir(&outside).unwrap();
        for error in [
            create_new_private_directory(&store, &outside.join("new")).unwrap_err(),
            private_tree_usage_bounded(
                &store,
                &outside,
                FactNamespaceUsage::default(),
                1024,
                u64::MAX,
            )
            .unwrap_err(),
            remove_private_tree(&store, &outside).unwrap_err(),
            write_private_atomically_beneath(&store, &outside.join("data"), b"x").unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn fact_payload_is_bound_to_source_thread_and_has_no_content_fields() {
        let value = serde_json::to_value(fact("thread-a", "event-1", at(8, 28, 1), 10)).unwrap();
        let object = value.as_object().unwrap();
        for forbidden in [
            "title",
            "message",
            "messagePreview",
            "prompt",
            "assistant",
            "reasoning",
            "tool",
        ] {
            assert!(!object.contains_key(forbidden));
        }

        let wrong =
            UsageEventFactRecord::upsert(1, fact("another-thread", "event-1", at(8, 28, 1), 10))
                .unwrap();
        let invalid = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Snapshot,
            "thread-a",
            None,
            FactCursor::new(1, 1).unwrap(),
            at(8, 28, 2),
            vec![wrong],
        );
        assert_eq!(
            invalid.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn evidence_directories_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let store = store(root.path());
        let target = root.path().join("outside");
        fs::create_dir(&target).unwrap();
        let profile = store
            .source_directory(&source_id())
            .join(RedactionProfile::Redacted.directory_name());
        store.prepare_private_directory(&profile).unwrap();
        symlink(&target, profile.join(DIGESTS_DIRECTORY)).unwrap();
        let start = at(8, 28, 1);
        let record = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-a", start, start + Duration::hours(1), 10),
        )
        .unwrap();
        assert_eq!(
            store
                .record_source_session_digest_changes(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &[record],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn evidence_directories_reject_an_ancestor_symlink_beneath_state_root() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let store = store(root.path());
        let outside = root.path().join("outside-ancestor");
        fs::create_dir(&outside).unwrap();
        let redaction_directory = store
            .source_directory(&source_id())
            .join(RedactionProfile::PreviewEnabled.directory_name());
        symlink(&outside, &redaction_directory).unwrap();
        let start = at(8, 28, 1);
        let record = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-ancestor", start, start + Duration::hours(1), 10),
        )
        .unwrap();
        assert_eq!(
            store
                .record_source_session_digest_changes(
                    &source_id(),
                    RedactionProfile::PreviewEnabled,
                    &[record],
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!outside.join(DIGESTS_DIRECTORY).exists());
    }
}
