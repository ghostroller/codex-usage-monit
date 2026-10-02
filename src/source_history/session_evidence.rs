use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use super::*;

#[path = "sqlite_evidence.rs"]
mod sqlite_evidence;
use crate::domain::{ApiCostAmount, TokenUsage};
use crate::source_model::{ObservedProjectKey, SessionReplicaKey, ThreadId, ThreadShardKey};

pub(super) const DIGESTS_DIRECTORY: &str = "digests";
const FACTS_DIRECTORY: &str = "facts";
const FACT_MANIFESTS_DIRECTORY: &str = "fact-manifests";
const FACT_STAGING_DIRECTORY: &str = "fact-staging";
const STAGED_BATCH_FILE: &str = "batch.json";
const STAGED_PUBLICATION_FILE: &str = "publication.json";
const FACT_BATCH_FORMAT_VERSION: u32 = 5;
const FACT_MANIFEST_FORMAT_VERSION: u32 = 4;
const MAX_FACT_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_FACT_BATCH_CHANGES: usize = 250_000;
const MAX_FACT_GENERATION_RECORDS: usize = 1_000_000;
const MAX_FACT_RETENTION_DAYS: i64 = 35;
// An exact 35-day interval can touch 36 distinct UTC calendar days when its
// endpoints are not midnight. Record timestamps remain the authoritative
// retention bound; this only caps the number of possible daily shards.
const MAX_FACT_RETENTION_UTC_DAYS: usize = 36;
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

    pub(super) fn validate(&self) -> io::Result<()> {
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
    /// this generation. An empty proof never satisfies a current digest and
    /// therefore requires a refresh.
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
        self.sqlite_database()
            .expect("source history is SQL-only")
            .write(|_| {
                self.load_source_metadata(source_id)?;
                self.record_source_session_digest_changes_in_directory_unfenced(
                    source_id,
                    redaction_profile,
                    &self.source_digests_directory(source_id, redaction_profile),
                    records,
                )
            })
    }

    pub(super) fn record_source_session_digest_changes_in_directory_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        directory: &Path,
        records: &[SourceSessionDigestRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        self.sqlite_record_digest_changes(source_id, redaction_profile, directory, records)
    }

    pub fn load_source_session_digest_records_since(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<SourceSessionDigestRecordsData> {
        let mut budget = SourceHistoryReadBudget::for_query();
        self.load_source_session_digest_records_since_with_budget(
            source_id,
            redaction_profile,
            since,
            &mut budget,
        )
    }

    pub(crate) fn load_source_session_digest_records_since_with_budget(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<SourceSessionDigestRecordsData> {
        budget.charge_source()?;
        self.with_source_metadata_shared(source_id, |source| {
            let records = if source.kind() == SourceKind::Ssh {
                self.with_active_remote_history_generation(
                    source_id,
                    redaction_profile,
                    |generation_directory| {
                        let Some(generation_directory) = generation_directory else {
                            return Ok(Vec::new());
                        };
                        self.load_source_session_digest_records_from_directory_with_budget(
                            source_id,
                            redaction_profile,
                            since,
                            &generation_directory.join(DIGESTS_DIRECTORY),
                            budget,
                        )
                    },
                )?
            } else {
                self.load_source_session_digest_records_from_directory_with_budget(
                    source_id,
                    redaction_profile,
                    since,
                    &self.source_digests_directory(source_id, redaction_profile),
                    budget,
                )?
            };
            Ok(SourceSessionDigestRecordsData {
                source: source.clone(),
                redaction_profile,
                records,
            })
        })
    }

    pub(super) fn load_source_session_digest_records_from_directory_with_budget(
        &self,
        source_id: &NodeId,
        _redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
        directory: &Path,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<Vec<SourceSessionDigestRecord>> {
        self.sqlite_load_digest_records(source_id, since, directory, budget)
    }

    pub(super) fn stage_complete_fact_batch_unfenced(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        batch: &CompleteFactBatch,
    ) -> io::Result<()> {
        self.sqlite_stage_fact_batch(source_id, redaction_profile, batch)
    }

    #[cfg(test)]
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
        self.sqlite_prevalidate_fact_batch(source_id, redaction_profile, batch_id)
    }

    pub(super) fn publish_prevalidated_fact_batch_unfenced(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> io::Result<FactActivationReport> {
        self.sqlite_publish_fact_batch(publication)
    }

    pub(super) fn cleanup_prevalidated_fact_publication_unfenced(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> bool {
        self.sqlite_cleanup_fact_publication(publication).is_err()
    }

    pub fn load_active_fact_set(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        thread_id: &ThreadId,
    ) -> io::Result<Option<ActiveFactSet>> {
        let mut budget = SourceHistoryReadBudget::for_query();
        budget.charge_source()?;
        self.load_active_fact_set_with_budget(source_id, redaction_profile, thread_id, &mut budget)
    }

    pub(crate) fn load_active_fact_set_with_budget(
        &self,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
        thread_id: &ThreadId,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<Option<ActiveFactSet>> {
        self.sqlite_load_active_fact_set(source_id, redaction_profile, thread_id, budget)
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

type DigestRecordKey = (ThreadId, DateTime<Utc>);
type DigestRecordIndex = HashMap<DigestRecordKey, usize>;

fn digest_record_index(records: &[SourceSessionDigestRecord]) -> io::Result<DigestRecordIndex> {
    let mut index = HashMap::new();
    index.try_reserve(records.len()).map_err(|error| {
        io::Error::other(format!(
            "could not allocate session digest record index: {error}"
        ))
    })?;
    for (position, record) in records.iter().enumerate() {
        let key = (record.thread_id.clone(), record.range_start);
        if index.insert(key, position).is_some() {
            return Err(invalid_data(
                "session digest record set contains duplicate thread/range keys",
            ));
        }
    }
    Ok(index)
}

fn apply_digest_record(
    records: &mut Vec<SourceSessionDigestRecord>,
    record_index: &mut DigestRecordIndex,
    mut incoming: SourceSessionDigestRecord,
) -> io::Result<bool> {
    incoming.validate()?;
    let key = (incoming.thread_id.clone(), incoming.range_start);
    let Some(index) = record_index.get(&key).copied() else {
        records.try_reserve(1).map_err(|error| {
            io::Error::other(format!(
                "could not allocate session digest record buffer: {error}"
            ))
        })?;
        record_index.try_reserve(1).map_err(|error| {
            io::Error::other(format!(
                "could not allocate session digest record index: {error}"
            ))
        })?;
        let index = records.len();
        records.push(incoming);
        record_index.insert(key, index);
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

fn apply_fact_record<S: std::hash::BuildHasher>(
    records: &mut Vec<UsageEventFactRecord>,
    record_index: &mut HashMap<UsageEventId, usize, S>,
    incoming: UsageEventFactRecord,
) -> io::Result<bool> {
    incoming.validate()?;
    let Some(&index) = record_index.get(&incoming.event_id) else {
        record_index.insert(incoming.event_id.clone(), records.len());
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
    if last.signed_duration_since(first) > Duration::days(MAX_FACT_RETENTION_DAYS) {
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

fn validate_staged_batch(
    descriptor: StagedFactBatch,
    path: &Path,
    profile_id: &HistoryProfileId,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    batch_id: &FactBatchId,
) -> io::Result<StagedFactBatch> {
    if descriptor.format_version != FACT_BATCH_FORMAT_VERSION
        || &descriptor.profile_id != profile_id
        || &descriptor.source_id != source_id
        || descriptor.redaction_profile != redaction_profile
        || &descriptor.batch_id != batch_id
        || descriptor.replica.source_id() != source_id
        || ThreadShardKey::from_replica(&descriptor.replica) != descriptor.thread_shard_key
    {
        return Err(invalid_data(format!(
            "staged fact batch envelope does not match {}",
            path.display()
        )));
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
    if days.len() > MAX_FACT_RETENTION_UTC_DAYS {
        return Err(invalid_data(
            "fact shard set exceeds the 35-day retention window",
        ));
    }
    if days.windows(2).any(|window| window[0] >= window[1]) {
        return Err(invalid_data("fact shard days must be sorted and unique"));
    }
    if days.first().zip(days.last()).is_some_and(|(first, last)| {
        last.signed_duration_since(*first).num_days() > MAX_FACT_RETENTION_DAYS
    }) {
        return Err(invalid_data(
            "fact shard set exceeds the 35-day retention window",
        ));
    }
    Ok(())
}

fn fact_manifest_path(directory: &Path, shard_key: &ThreadShardKey) -> PathBuf {
    directory.join(format!("{}.json", shard_key.as_str()))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FactNamespaceUsage {
    bytes: u64,
    entries: u64,
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

pub(super) fn earliest_session_evidence_time(
    store: &SourceHistoryStore,
    sources: &[SourceMetadata],
    redaction_profiles: &[RedactionProfile],
) -> io::Result<Option<DateTime<Utc>>> {
    sqlite_evidence::earliest_session_evidence_time(store, sources, redaction_profiles)
}

#[cfg(test)]
fn garbage_collect_session_evidence_for_source(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    cutoff_day: NaiveDate,
    trusted_at: DateTime<Utc>,
) -> io::Result<usize> {
    garbage_collect_session_evidence_for_source_with_changes(
        store,
        source_id,
        redaction_profile,
        cutoff_day,
        trusted_at,
    )
    .map(|(pruned, _)| pruned)
}

pub(super) fn garbage_collect_session_evidence_for_source_with_changes(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    cutoff_day: NaiveDate,
    trusted_at: DateTime<Utc>,
) -> io::Result<(usize, bool)> {
    sqlite_evidence::garbage_collect_session_evidence(
        store,
        source_id,
        redaction_profile,
        cutoff_day,
        trusted_at,
    )
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
        store_with_kind(root, SourceKind::Local)
    }

    fn store_with_kind(root: &Path, kind: SourceKind) -> SourceHistoryStore {
        let ownership = crate::history_ownership::HistoryOwnershipStore::new(
            root.join("state-root"),
            PROFILE.parse().unwrap(),
            RedactionProfile::Redacted,
        );
        let (_, store) =
            crate::sqlite_history_initialization::initialize_for_test(&ownership).unwrap();
        store
            .save_source_metadata(&SourceMetadata::new(source_id(), kind, "build-host").unwrap())
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
                unclassified_tokens: 0,
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

    #[test]
    fn indexed_fact_merge_preserves_revision_conflict_and_tombstone_rules() {
        let occurred_at = at(8, 27, 1);
        let initial =
            UsageEventFactRecord::upsert(1, fact("thread-a", "event-1", occurred_at, 10)).unwrap();
        let mut records = Vec::new();
        let mut index = HashMap::new();
        assert!(apply_fact_record(&mut records, &mut index, initial.clone()).unwrap());
        assert!(!apply_fact_record(&mut records, &mut index, initial.clone()).unwrap());
        let conflict =
            UsageEventFactRecord::upsert(1, fact("thread-a", "event-1", occurred_at, 20)).unwrap();
        assert!(apply_fact_record(&mut records, &mut index, conflict).is_err());
        let updated =
            UsageEventFactRecord::upsert(2, fact("thread-a", "event-1", occurred_at, 20)).unwrap();
        assert!(apply_fact_record(&mut records, &mut index, updated.clone()).unwrap());
        assert!(!apply_fact_record(&mut records, &mut index, initial).unwrap());
        let moved = UsageEventFactRecord::tombstone(
            "event-1".parse().unwrap(),
            occurred_at + Duration::seconds(1),
            3,
        )
        .unwrap();
        assert!(apply_fact_record(&mut records, &mut index, moved).is_err());
        let deleted =
            UsageEventFactRecord::tombstone("event-1".parse().unwrap(), occurred_at, 3).unwrap();
        assert!(apply_fact_record(&mut records, &mut index, deleted.clone()).unwrap());
        assert!(!apply_fact_record(&mut records, &mut index, deleted.clone()).unwrap());
        assert!(!apply_fact_record(&mut records, &mut index, updated).unwrap());
        assert_eq!(records, vec![deleted]);
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn indexed_fact_merge_uses_linear_hash_lookups_for_large_batches() {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        #[derive(Clone, Default)]
        struct CountedBuildHasher(Arc<AtomicUsize>);
        struct CountedHasher(std::collections::hash_map::DefaultHasher, Arc<AtomicUsize>);
        impl Hasher for CountedHasher {
            fn finish(&self) -> u64 {
                self.1.fetch_add(1, Ordering::Relaxed);
                self.0.finish()
            }
            fn write(&mut self, bytes: &[u8]) {
                self.0.write(bytes);
            }
        }
        impl BuildHasher for CountedBuildHasher {
            type Hasher = CountedHasher;
            fn build_hasher(&self) -> Self::Hasher {
                CountedHasher(
                    std::collections::hash_map::DefaultHasher::new(),
                    self.0.clone(),
                )
            }
        }
        const COUNT: usize = 20_000;
        let hasher = CountedBuildHasher::default();
        let mut index = HashMap::with_capacity_and_hasher(COUNT, hasher.clone());
        let mut records = Vec::new();
        for (pass, revision) in [1, 2, 2].into_iter().enumerate() {
            for event in 0..COUNT {
                let record = UsageEventFactRecord::tombstone(
                    format!("event-{event}").parse().unwrap(),
                    at(8, 27, 1),
                    revision,
                )
                .unwrap();
                let changed = apply_fact_record(&mut records, &mut index, record).unwrap();
                assert_eq!(changed, pass < 2);
            }
        }
        assert_eq!(records.len(), COUNT);
        // One lookup per change plus one insertion per new ID. Count actual
        // hash computations, independent of wall time and machine speed.
        let hashes = hasher.0.load(Ordering::Relaxed);
        assert!((COUNT * 3..=COUNT * 4).contains(&hashes), "{hashes} hashes");
        assert!(records.iter().all(|record| record.revision == 2));
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

        let loaded = store
            .load_source_session_digest_records_since(
                &source_id(),
                RedactionProfile::Redacted,
                start,
            )
            .unwrap();
        let text = serde_json::to_string(&loaded.records).unwrap();
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
    fn indexed_digest_application_handles_large_unique_sets_and_revisions() {
        let start = at(8, 28, 1);
        let end = start + Duration::hours(1);
        let mut records = Vec::new();
        let mut record_index = HashMap::new();
        for offset in 0_u64..10_000 {
            let thread_id = format!("thread-indexed-{offset}");
            let record =
                SourceSessionDigestRecord::upsert(1, digest(&thread_id, start, end, offset + 1))
                    .unwrap();
            assert!(apply_digest_record(&mut records, &mut record_index, record).unwrap());
        }
        assert_eq!(records.len(), 10_000);
        assert_eq!(record_index.len(), records.len());

        let replacement =
            SourceSessionDigestRecord::upsert(2, digest("thread-indexed-5000", start, end, 99_999))
                .unwrap();
        assert!(apply_digest_record(&mut records, &mut record_index, replacement).unwrap());
        assert_eq!(records.len(), 10_000);
        let key = (thread("thread-indexed-5000"), start);
        assert_eq!(records[*record_index.get(&key).unwrap()].revision(), 2);
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

        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            at(7, 26, 0).date_naive(),
            Utc::now(),
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
        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            cutoff.date_naive(),
            Utc::now(),
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
        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            cutoff.date_naive(),
            Utc::now(),
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
        let database = store.sqlite_database().unwrap();
        let descriptor_key = database
            .namespace(&staging.join(STAGED_BATCH_FILE))
            .unwrap();
        let descriptor = database
            .read(|connection| database::state::<StagedFactBatch>(connection, &descriptor_key))
            .unwrap()
            .unwrap();
        assert_eq!(descriptor.batch_id, batch_id);
        assert!(
            !serde_json::to_string(&descriptor)
                .unwrap()
                .contains("pageToken")
        );
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
    fn active_fact_reads_share_the_query_record_budget() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let cursor = FactCursor::new(7, 10).unwrap();
        let batch_id = FactBatchId::generate().unwrap();
        let snapshot = batch(
            batch_id.clone(),
            FactBatchKind::Snapshot,
            "thread-budget",
            None,
            cursor,
            at(8, 28, 2),
            vec![
                UsageEventFactRecord::upsert(
                    1,
                    fact("thread-budget", "event-budget", at(8, 28, 1), 10),
                )
                .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &snapshot)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &batch_id)
            .unwrap();

        let mut budget =
            SourceHistoryReadBudget::with_limits(MAX_HISTORY_QUERY_DECODED_BYTES, 0, 1);
        let error = store
            .load_active_fact_set_with_budget(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-budget"),
                &mut budget,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(SourceHistoryReadBudget::is_exhaustion(&error));
        assert!(error.to_string().contains("record budget"));
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
    fn staged_delta_and_cursor_cas_reject_stale_batches() {
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
        assert!(fact_generation_records(&store, &replica("thread-a"), &first_id).is_empty());

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
            Utc::now() + Duration::hours(FACT_STAGING_TTL_HOURS + 1),
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
        assert!(staged_descriptor(&store, &abandoned_id).is_none());
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
    fn fact_batch_span_count_and_namespace_limits_fail_closed() {
        assert!(validate_fact_batch_change_count(MAX_FACT_BATCH_CHANGES).is_ok());
        assert_eq!(
            validate_fact_batch_change_count(MAX_FACT_BATCH_CHANGES + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let crosses_thirty_six_utc_days_within_window = vec![
            UsageEventFactRecord::upsert(1, fact("thread-limit", "event-1", at(8, 1, 23), 1))
                .unwrap(),
            UsageEventFactRecord::upsert(
                1,
                fact(
                    "thread-limit",
                    "event-2",
                    at(9, 5, 23) - Duration::seconds(1),
                    1,
                ),
            )
            .unwrap(),
        ];
        assert_eq!(
            crosses_thirty_six_utc_days_within_window[1]
                .occurred_at()
                .date_naive()
                .signed_duration_since(
                    crosses_thirty_six_utc_days_within_window[0]
                        .occurred_at()
                        .date_naive(),
                )
                .num_days(),
            MAX_FACT_RETENTION_DAYS
        );
        assert!(
            crosses_thirty_six_utc_days_within_window[1]
                .occurred_at()
                .signed_duration_since(crosses_thirty_six_utc_days_within_window[0].occurred_at(),)
                < Duration::days(MAX_FACT_RETENTION_DAYS)
        );
        assert!(validate_fact_record_span(&crosses_thirty_six_utc_days_within_window).is_ok());
        let exact_window = vec![
            crosses_thirty_six_utc_days_within_window[0].clone(),
            UsageEventFactRecord::upsert(1, fact("thread-limit", "event-3", at(9, 5, 23), 1))
                .unwrap(),
        ];
        assert!(validate_fact_record_span(&exact_window).is_ok());
        let outside_window = vec![
            exact_window[0].clone(),
            UsageEventFactRecord::upsert(
                1,
                fact(
                    "thread-limit",
                    "event-4",
                    at(9, 5, 23) + Duration::seconds(1),
                    1,
                ),
            )
            .unwrap(),
        ];
        assert_eq!(
            validate_fact_record_span(&outside_window)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        for (usage, bytes, entries) in [
            (
                FactNamespaceUsage {
                    bytes: 4,
                    entries: 0,
                },
                3,
                u64::MAX,
            ),
            (
                FactNamespaceUsage {
                    bytes: 0,
                    entries: 1,
                },
                u64::MAX,
                0,
            ),
        ] {
            assert_eq!(
                validate_fact_namespace_usage(usage, bytes, entries)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert!(
            validate_fact_namespace_usage(
                FactNamespaceUsage {
                    bytes: MAX_FACT_NAMESPACE_BYTES,
                    entries: MAX_FACT_NAMESPACE_ENTRIES
                },
                MAX_FACT_NAMESPACE_BYTES,
                MAX_FACT_NAMESPACE_ENTRIES,
            )
            .is_ok()
        );
    }

    #[test]
    fn exact_retention_window_round_trips_through_local_and_remote_fact_batches() {
        let root = tempdir().unwrap();
        let first = at(8, 1, 23);
        let changes = (0..=MAX_FACT_RETENTION_DAYS)
            .map(|day| {
                UsageEventFactRecord::upsert(
                    1,
                    fact(
                        "thread-retention-boundary",
                        &format!("event-{day:02}"),
                        first + Duration::days(day),
                        1,
                    ),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            changes
                .iter()
                .map(UsageEventFactRecord::occurred_at)
                .map(|timestamp| timestamp.date_naive())
                .collect::<BTreeSet<_>>()
                .len(),
            MAX_FACT_RETENTION_UTC_DAYS
        );

        for (directory, kind) in [("local", SourceKind::Local), ("remote", SourceKind::Ssh)] {
            let store = store_with_kind(&root.path().join(directory), kind);
            let remote_binding = (kind == SourceKind::Ssh)
                .then(|| {
                    SourceHistoryRemoteBinding::new(
                        crate::remote_protocol::SourceGeneration {
                            node_id: source_id(),
                            generation: std::num::NonZeroU64::new(1).unwrap(),
                        },
                        crate::remote_agent::current_revisions(),
                    )
                })
                .transpose()
                .unwrap();
            let batch_id = FactBatchId::generate().unwrap();
            let batch = CompleteFactBatch {
                batch_id: batch_id.clone(),
                kind: FactBatchKind::Snapshot,
                replica: replica("thread-retention-boundary"),
                expected_active_version: None,
                remote_binding,
                validated_digests: Vec::new(),
                activate_cursor: FactCursor::new(1, changes.len() as u64).unwrap(),
                completed_at: first + Duration::days(MAX_FACT_RETENTION_DAYS),
                changes: changes.clone(),
            };

            store
                .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &batch)
                .unwrap();
            store
                .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &batch_id)
                .unwrap();
            let active = store
                .load_active_fact_set(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &thread("thread-retention-boundary"),
                )
                .unwrap()
                .unwrap();
            assert_eq!(active.records.len(), MAX_FACT_RETENTION_UTC_DAYS);
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

    fn fact_generation_records(
        store: &SourceHistoryStore,
        replica: &SessionReplicaKey,
        generation: &FactBatchId,
    ) -> Vec<UsageEventFactRecord> {
        let database = store.sqlite_database().unwrap();
        let namespace = database
            .namespace(
                &store
                    .source_facts_directory(replica.source_id(), RedactionProfile::Redacted)
                    .join(ThreadShardKey::from_replica(replica).as_str())
                    .join(generation.as_str()),
            )
            .unwrap();
        database
            .read(|connection| {
                let mut budget = SourceHistoryReadBudget::for_query();
                database::records(connection, &namespace, i64::MIN, &mut budget)
            })
            .unwrap()
    }

    fn staged_descriptor(
        store: &SourceHistoryStore,
        batch_id: &FactBatchId,
    ) -> Option<StagedFactBatch> {
        let database = store.sqlite_database().unwrap();
        let key = database
            .namespace(
                &store
                    .source_fact_staging_directory(&source_id(), RedactionProfile::Redacted)
                    .join(batch_id.as_str())
                    .join(STAGED_BATCH_FILE),
            )
            .unwrap();
        database
            .read(|connection| database::state(connection, &key))
            .unwrap()
    }

    fn sqlite_store(root: &Path) -> SourceHistoryStore {
        store(root)
    }

    #[test]
    fn sqlite_fact_compare_and_swap_keeps_complete_cursor_and_proof_atomic() {
        let root = tempdir().unwrap();
        let store = sqlite_store(root.path());
        let occurred = at(8, 28, 1);
        let first = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Snapshot,
            "thread-a",
            None,
            FactCursor::new(u64::MAX, 1).unwrap(),
            occurred,
            vec![
                UsageEventFactRecord::upsert(u64::MAX, fact("thread-a", "event-1", occurred, 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &first)
            .unwrap();
        assert!(
            store
                .load_active_fact_set(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &thread("thread-a")
                )
                .unwrap()
                .is_none()
        );
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &first.batch_id)
            .unwrap();
        let initial = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-a"),
            )
            .unwrap()
            .unwrap();
        let delta = |event: &str| {
            batch(
                FactBatchId::generate().unwrap(),
                FactBatchKind::Delta,
                "thread-a",
                Some(initial.version.clone()),
                FactCursor::new(u64::MAX, 2).unwrap(),
                occurred + Duration::minutes(1),
                vec![
                    UsageEventFactRecord::upsert(
                        1,
                        fact("thread-a", event, occurred + Duration::minutes(1), 20),
                    )
                    .unwrap(),
                ],
            )
        };
        let left = delta("event-left");
        let right = delta("event-right");
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &left)
            .unwrap();
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &right)
            .unwrap();
        let left = store
            .prevalidate_staged_fact_batch_unfenced(
                &source_id(),
                RedactionProfile::Redacted,
                &left.batch_id,
            )
            .unwrap();
        let right = store
            .prevalidate_staged_fact_batch_unfenced(
                &source_id(),
                RedactionProfile::Redacted,
                &right.batch_id,
            )
            .unwrap();
        assert!(
            store
                .publish_prevalidated_fact_batch_unfenced(&left)
                .unwrap()
                .activated
        );
        assert_eq!(
            store
                .publish_prevalidated_fact_batch_unfenced(&right)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-a"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.cursor, FactCursor::new(u64::MAX, 2).unwrap());
        assert_eq!(active.records.len(), 2);
        assert!(
            active
                .records
                .iter()
                .any(|record| record.event_id().as_str() == "event-left")
        );
        assert!(
            !active
                .records
                .iter()
                .any(|record| record.event_id().as_str() == "event-right")
        );
        assert_eq!(
            fact_generation_records(&store, &active.replica, active.version.active_generation()),
            active.records
        );
    }

    #[test]
    fn sqlite_digest_revision_preserves_suppression_horizon_and_rejects_equal_conflict() {
        let root = tempdir().unwrap();
        let store = sqlite_store(root.path());
        let start = at(8, 28, 1);
        let long = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-a", start, start + Duration::hours(2), 10),
        )
        .unwrap();
        store
            .record_source_session_digest_changes(&source_id(), RedactionProfile::Redacted, &[long])
            .unwrap();
        let short = SourceSessionDigestRecord::upsert(
            2,
            digest("thread-a", start, start + Duration::hours(1), 10),
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[short],
            )
            .unwrap();
        let loaded = store
            .load_source_session_digest_records_since(
                &source_id(),
                RedactionProfile::Redacted,
                start + Duration::minutes(90),
            )
            .unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(
            loaded.records[0].retention_through(),
            start + Duration::hours(2)
        );
        let conflict = SourceSessionDigestRecord::upsert(
            2,
            digest("thread-a", start, start + Duration::hours(1), 20),
        )
        .unwrap();
        assert_eq!(
            store
                .record_source_session_digest_changes(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &[conflict]
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn sqlite_fact_gc_fences_stale_staging_and_preserves_tombstone_retention_floor() {
        let root = tempdir().unwrap();
        let store = sqlite_store(root.path());
        let occurred = at(8, 1, 1);
        let first = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Snapshot,
            "thread-a",
            None,
            FactCursor::new(1, 1).unwrap(),
            occurred,
            vec![
                UsageEventFactRecord::tombstone("event-old".parse().unwrap(), occurred, 5).unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &first)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &first.batch_id)
            .unwrap();
        let initial = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-a"),
            )
            .unwrap()
            .unwrap();
        let stale = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Delta,
            "thread-a",
            Some(initial.version),
            FactCursor::new(1, 2).unwrap(),
            occurred,
            vec![
                UsageEventFactRecord::upsert(6, fact("thread-a", "event-old", occurred, 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &stale)
            .unwrap();
        let stale = store
            .prevalidate_staged_fact_batch_unfenced(
                &source_id(),
                RedactionProfile::Redacted,
                &stale.batch_id,
            )
            .unwrap();
        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            at(8, 2, 0).date_naive(),
            at(8, 2, 0),
        )
        .unwrap();
        assert_eq!(
            store
                .publish_prevalidated_fact_batch_unfenced(&stale)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-a"),
            )
            .unwrap()
            .unwrap();
        assert!(active.records.is_empty());
        assert_eq!(active.version.retained_since(), Some(at(8, 2, 0)));
        let next = batch(
            FactBatchId::generate().unwrap(),
            FactBatchKind::Delta,
            "thread-a",
            Some(active.version),
            FactCursor::new(1, 2).unwrap(),
            occurred,
            vec![
                UsageEventFactRecord::upsert(6, fact("thread-a", "event-old", occurred, 10))
                    .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &next)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &next.batch_id)
            .unwrap();
        assert!(
            store
                .load_active_fact_set(
                    &source_id(),
                    RedactionProfile::Redacted,
                    &thread("thread-a")
                )
                .unwrap()
                .unwrap()
                .records
                .is_empty()
        );
    }
    #[test]
    fn sqlite_fact_gc_reclaims_orphans_and_preserves_prevalidated_staging() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let first_id = FactBatchId::generate().unwrap();
        let first = batch(
            first_id.clone(),
            FactBatchKind::Snapshot,
            "thread-orphan",
            None,
            FactCursor::new(2, 1).unwrap(),
            at(8, 28, 2),
            vec![
                UsageEventFactRecord::upsert(
                    1,
                    fact("thread-orphan", "event-one", at(8, 28, 1), 10),
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
        garbage_collect_session_evidence_for_source(
            &store,
            &source_id(),
            RedactionProfile::Redacted,
            at(8, 1, 0).date_naive(),
            Utc::now(),
        )
        .unwrap();
        let before = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-orphan"),
            )
            .unwrap()
            .unwrap();
        let staged_id = FactBatchId::generate().unwrap();
        let staged = batch(
            staged_id.clone(),
            FactBatchKind::Delta,
            "thread-orphan",
            Some(before.version.clone()),
            FactCursor::new(2, 2).unwrap(),
            at(8, 28, 3),
            vec![
                UsageEventFactRecord::upsert(
                    2,
                    fact("thread-orphan", "event-two", at(8, 28, 2), 20),
                )
                .unwrap(),
            ],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &staged)
            .unwrap();
        let publication = store
            .prevalidate_staged_fact_batch_unfenced(
                &source_id(),
                RedactionProfile::Redacted,
                &staged_id,
            )
            .unwrap();
        let orphan_id = FactBatchId::generate().unwrap();
        let database = store.sqlite_database().unwrap();
        let namespace = database
            .namespace(
                &store
                    .source_facts_directory(&source_id(), RedactionProfile::Redacted)
                    .join(ThreadShardKey::from_replica(&before.replica).as_str())
                    .join(orphan_id.as_str()),
            )
            .unwrap();
        let orphan = UsageEventFactRecord::upsert(
            1,
            fact("thread-orphan", "event-orphan", at(8, 28, 1), 999),
        )
        .unwrap();
        database
            .write(|connection| {
                database::put_record(
                    connection,
                    &namespace,
                    orphan.event_id().as_str(),
                    orphan.occurred_at().timestamp_millis(),
                    &orphan,
                )
            })
            .unwrap();
        // Artifact cleanup cannot change a visible query or its cache stamp.
        let (pruned, query_visible_deleted) =
            garbage_collect_session_evidence_for_source_with_changes(
                &store,
                &source_id(),
                RedactionProfile::Redacted,
                at(8, 1, 0).date_naive(),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(pruned, 0);
        assert!(!query_visible_deleted);
        assert!(fact_generation_records(&store, &before.replica, &orphan_id).is_empty());
        assert!(staged_descriptor(&store, &staged_id).is_some());
        let report = store
            .publish_prevalidated_fact_batch_unfenced(&publication)
            .unwrap();
        assert!(report.activated);
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-orphan"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.cursor, staged.activate_cursor);
        assert_eq!(
            active.records,
            vec![first.changes[0].clone(), staged.changes[0].clone()]
        );
        assert_eq!(
            fact_generation_records(&store, &before.replica, &staged_id),
            active.records
        );
    }

    #[test]
    fn sqlite_evidence_round_trips_full_u128_costs() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let mut event = fact("thread-u128", "event-u128", at(8, 28, 1), 10);
        event.metrics.estimated_cost_units = u128::MAX;
        event.metrics.api_long_context_extra_cost_units = Some(u128::MAX - 1);
        let mut cost = event.metrics.api_equivalent_cost;
        cost.minimum_pico_usd = PicoUsd::new(u128::MAX - 2);
        cost.maximum_pico_usd = PicoUsd::new(u128::MAX);
        event.metrics.api_equivalent_cost = cost;
        let mut session = digest("thread-u128", at(8, 28, 1), at(8, 28, 2), 10);
        session.metrics = event.metrics.clone();
        let digest_record = SourceSessionDigestRecord::upsert(u64::MAX, session).unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                std::slice::from_ref(&digest_record),
            )
            .unwrap();
        let fact_record = UsageEventFactRecord::upsert(u64::MAX, event).unwrap();
        let id = FactBatchId::generate().unwrap();
        let snapshot = batch(
            id.clone(),
            FactBatchKind::Snapshot,
            "thread-u128",
            None,
            FactCursor::new(u64::MAX, u64::MAX).unwrap(),
            at(8, 28, 2),
            vec![fact_record.clone()],
        );
        store
            .stage_complete_fact_batch(&source_id(), RedactionProfile::Redacted, &snapshot)
            .unwrap();
        store
            .activate_staged_fact_batch(&source_id(), RedactionProfile::Redacted, &id)
            .unwrap();
        let active = store
            .load_active_fact_set(
                &source_id(),
                RedactionProfile::Redacted,
                &thread("thread-u128"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.records, vec![fact_record]);
        assert_eq!(active.cursor, snapshot.activate_cursor);
        assert_eq!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::Redacted,
                    at(8, 28, 0)
                )
                .unwrap()
                .records,
            vec![digest_record]
        );
    }
    #[test]
    fn sqlite_digest_gc_reports_partial_day_deletion_without_pruning_the_day() {
        let root = tempdir().unwrap();
        let store = store(root.path());
        let start = at(7, 25, 1);
        let expired = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-expired", start, start + Duration::hours(1), 10),
        )
        .unwrap();
        let crossing = SourceSessionDigestRecord::upsert(
            1,
            digest("thread-crossing", start, at(7, 27, 1), 20),
        )
        .unwrap();
        store
            .record_source_session_digest_changes(
                &source_id(),
                RedactionProfile::Redacted,
                &[expired, crossing.clone()],
            )
            .unwrap();
        let collect = || {
            garbage_collect_session_evidence_for_source_with_changes(
                &store,
                &source_id(),
                RedactionProfile::Redacted,
                at(7, 26, 0).date_naive(),
                Utc::now(),
            )
            .unwrap()
        };
        assert_eq!(collect(), (0, true));
        assert_eq!(
            store
                .load_source_session_digest_records_since(
                    &source_id(),
                    RedactionProfile::Redacted,
                    start,
                )
                .unwrap()
                .records,
            vec![crossing]
        );
        assert_eq!(collect(), (0, false));
    }
}
