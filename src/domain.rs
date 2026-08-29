use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

mod u128_string {
    use super::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
    #[default]
    Unknown,
}

impl Confidence {
    pub fn combine(self, other: Self) -> Self {
        use Confidence::{High, Low, Medium, Unknown};
        match (self, other) {
            (Unknown, value) | (value, Unknown) => value,
            (Low, _) | (_, Low) => Low,
            (Medium, _) | (_, Medium) => Medium,
            (High, High) => High,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Live,
    ServerSnapshot,
    LocalExact,
    Inferred,
    Estimated,
    Stale,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    WaitingApproval,
    WaitingInput,
    Idle,
    Completed,
    Interrupted,
    Failed,
    Stale,
    Unknown,
}

impl TaskStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Running | Self::WaitingApproval | Self::WaitingInput
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "RUN",
            Self::WaitingApproval => "APPROVAL",
            Self::WaitingInput => "INPUT",
            Self::Idle => "IDLE",
            Self::Completed => "DONE",
            Self::Interrupted => "STOPPED",
            Self::Failed => "FAILED",
            Self::Stale => "STALE",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
    Stale,
    #[default]
    Unknown,
}

impl TurnStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    pub fn add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }

    pub fn is_zero(self) -> bool {
        self.total_tokens == 0
            && self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_write_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
    }

    pub fn delta_from(self, previous: Self) -> Option<Self> {
        if self.input_tokens < previous.input_tokens
            || self.cached_input_tokens < previous.cached_input_tokens
            || self.cache_write_input_tokens < previous.cache_write_input_tokens
            || self.output_tokens < previous.output_tokens
            || self.reasoning_output_tokens < previous.reasoning_output_tokens
            || self.total_tokens < previous.total_tokens
        {
            return None;
        }

        Some(Self {
            input_tokens: self.input_tokens - previous.input_tokens,
            cached_input_tokens: self.cached_input_tokens - previous.cached_input_tokens,
            cache_write_input_tokens: self.cache_write_input_tokens
                - previous.cache_write_input_tokens,
            output_tokens: self.output_tokens - previous.output_tokens,
            reasoning_output_tokens: self.reasoning_output_tokens
                - previous.reasoning_output_tokens,
            total_tokens: self.total_tokens - previous.total_tokens,
        })
    }
}

/// Exact fixed-point money amount in trillionths of one US dollar.
///
/// JSON represents this value as a decimal string so JavaScript consumers do
/// not lose precision on large cumulative totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PicoUsd(#[serde(with = "u128_string")] pub u128);

impl PicoUsd {
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u128 {
        self.0
    }
}

/// Token-only API-equivalent model-call cost at the bundled catalog's rates.
///
/// `minimum` and `maximum` differ only when a cumulative rollout delta could
/// represent either several short requests or at least one long-context
/// request. Unpriced samples are excluded from both subtotals and reported via
/// coverage and partial reasons.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiCostAmount {
    pub minimum_pico_usd: PicoUsd,
    pub maximum_pico_usd: PicoUsd,
    /// Rollout token-usage deltas. One non-exact sample can cover multiple
    /// model requests, so this must not be interpreted as a request count.
    pub observed_samples: u64,
    pub priced_samples: u64,
    pub observed_tokens: u64,
    pub priced_tokens: u64,
}

impl ApiCostAmount {
    pub fn add_assign(&mut self, other: Self) {
        self.minimum_pico_usd.0 = self
            .minimum_pico_usd
            .0
            .saturating_add(other.minimum_pico_usd.0);
        self.maximum_pico_usd.0 = self
            .maximum_pico_usd
            .0
            .saturating_add(other.maximum_pico_usd.0);
        self.observed_samples = self.observed_samples.saturating_add(other.observed_samples);
        self.priced_samples = self.priced_samples.saturating_add(other.priced_samples);
        self.observed_tokens = self.observed_tokens.saturating_add(other.observed_tokens);
        self.priced_tokens = self.priced_tokens.saturating_add(other.priced_tokens);
    }

    pub fn range_is_exact(self) -> bool {
        self.minimum_pico_usd == self.maximum_pico_usd
    }

    pub fn has_priced_usage(self) -> bool {
        self.priced_samples > 0
    }

    pub fn priced_token_percent(self) -> f64 {
        if self.observed_tokens == 0 {
            if self.observed_samples == 0 {
                100.0
            } else {
                self.priced_samples as f64 / self.observed_samples as f64 * 100.0
            }
        } else {
            self.priced_tokens as f64 / self.observed_tokens as f64 * 100.0
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiModelCost {
    pub model: String,
    #[serde(flatten)]
    pub amount: ApiCostAmount,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiEquivalentCost {
    #[serde(flatten)]
    pub amount: ApiCostAmount,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
    /// Price coverage by observed model, including API-only/unpriced models
    /// such as Spark that are excluded from the Codex quota estimator.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_breakdown: Vec<ApiModelCost>,
}

impl ApiEquivalentCost {
    pub fn is_partial(&self) -> bool {
        self.amount.priced_samples < self.amount.observed_samples
            || self.amount.priced_tokens < self.amount.observed_tokens
            || !self.partial_reasons.is_empty()
    }

    pub fn is_fully_exact(&self) -> bool {
        !self.is_partial() && self.amount.range_is_exact()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiPricingMetadata {
    pub catalog_revision: u32,
    pub rates_as_of: String,
    pub source_url: String,
    pub basis: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub window_duration_mins: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
}

impl LimitWindow {
    pub fn new(
        used_percent: f64,
        window_duration_mins: Option<i64>,
        resets_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            used_percent,
            remaining_percent: (100.0 - used_percent).clamp(0.0, 100.0),
            window_duration_mins,
            resets_at,
        }
    }

    pub fn label(&self) -> String {
        match self.window_duration_mins {
            Some(300) => "5h".to_string(),
            Some(10_080) => "week".to_string(),
            Some(minutes) => format!("{minutes}m"),
            None => "window".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitResetCreditsSnapshot {
    pub available_count: u64,
    /// `None` means only the count is known; `Some([])` means details were fetched and empty.
    #[serde(default)]
    pub credits: Option<Vec<RateLimitResetCredit>>,
    pub provenance: Provenance,
    pub as_of: DateTime<Utc>,
}

impl RateLimitResetCreditsSnapshot {
    pub fn details_are_truncated(&self) -> bool {
        self.credits
            .as_ref()
            .is_some_and(|credits| (credits.len() as u64) < self.available_count)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitResetCredit {
    pub granted_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Raw protocol value, preserved so future App Server variants remain visible.
    pub status: String,
    /// Raw protocol value, preserved so future App Server variants remain visible.
    pub reset_type: String,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitBucket {
    pub limit_id: String,
    pub limit_name: Option<String>,
    pub plan_type: Option<String>,
    pub primary: Option<LimitWindow>,
    pub secondary: Option<LimitWindow>,
    pub credits: Option<CreditsSnapshot>,
    pub rate_limit_reached_type: Option<String>,
    pub provenance: Provenance,
    pub as_of: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTokenUsage {
    pub lifetime_tokens: Option<u64>,
    pub peak_daily_tokens: Option<u64>,
    pub longest_running_turn_sec: Option<u64>,
    pub current_streak_days: Option<u64>,
    pub longest_streak_days: Option<u64>,
    pub daily_usage_buckets: Vec<DailyTokenBucket>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyTokenBucket {
    pub start_date: String,
    pub tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(default)]
    pub archived: bool,
    pub title: String,
    #[serde(default, with = "crate::exact_json::optional_pathbuf_lossy")]
    pub cwd: Option<PathBuf>,
    pub source: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub status: TaskStatus,
    pub status_provenance: Provenance,
    pub status_confidence: Confidence,
    pub token_usage: TokenUsage,
    pub turn_count: usize,
    pub window_token_usage: TokenUsage,
    pub local_token_share_percent: f64,
    pub estimated_quota_percent: f64,
    pub quota_confidence: Confidence,
    /// API-equivalent model-token cost in the preferred current 5-hour window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_cost: Option<ApiCostAmount>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRecord {
    pub thread_id: String,
    pub turn_id: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Service tier captured from Codex's `thread_settings_applied` event.
    /// `None` means the rollout did not expose the setting for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub message_preview: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub status: TurnStatus,
    pub token_usage: TokenUsage,
    pub window_token_usage: TokenUsage,
    pub local_token_share_percent: f64,
    pub estimated_quota_percent: f64,
    pub quota_confidence: Confidence,
    /// API-equivalent model-token cost in the preferred current 5-hour window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_cost: Option<ApiCostAmount>,
}

impl TurnRecord {
    pub fn is_fast(&self) -> bool {
        is_fast_service_tier(self.service_tier.as_deref())
    }
}

/// A locally observed, exact interaction between one parent turn and a child
/// agent thread.
///
/// Rollout files expose the two halves independently: a parent-side function
/// call carries the parent turn id and a later `sub_agent_activity` event
/// carries the child thread id. Records are emitted only when their opaque
/// call/event ids match exactly; metadata-only or time-based guesses are not
/// represented here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInteraction {
    #[serde(default)]
    pub kind: AgentInteractionKind,
    pub parent_thread_id: String,
    pub parent_turn_id: String,
    pub child_thread_id: String,
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(default = "unknown_provenance")]
    pub provenance: Provenance,
}

impl Default for AgentInteraction {
    fn default() -> Self {
        Self {
            kind: AgentInteractionKind::default(),
            parent_thread_id: String::new(),
            parent_turn_id: String::new(),
            child_thread_id: String::new(),
            call_id: String::new(),
            requested_at: None,
            occurred_at: None,
            provenance: Provenance::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInteractionKind {
    #[default]
    Unknown,
    SpawnStarted,
    Interacted,
}

fn unknown_provenance() -> Provenance {
    Provenance::Unknown
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    pub token_usage: TokenUsage,
    pub local_token_share_percent: f64,
    pub estimated_quota_percent: f64,
    pub quota_confidence: Confidence,
    #[serde(default)]
    pub api_equivalent_cost: ApiCostAmount,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowDescriptor {
    pub limit_id: String,
    pub label: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub used_percent: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttributionSummary {
    pub window: Option<WindowDescriptor>,
    pub local_token_usage: TokenUsage,
    pub observed_delta_percent: f64,
    pub estimated_assigned_percent: f64,
    /// Current normal-Codex gauge percentage projected across entities using
    /// the local Codex credit-rate-weighted denominator. This is a
    /// low-confidence estimate, not server-side entity accounting.
    #[serde(default)]
    pub proxy_projected_percent: f64,
    pub unattributed_percent: f64,
    pub attribution_coverage_percent: f64,
    pub external_activity_possible: bool,
    pub confidence: Confidence,
    pub method: String,
    pub settled: bool,
}

impl Default for AttributionSummary {
    fn default() -> Self {
        Self {
            window: None,
            local_token_usage: TokenUsage::default(),
            observed_delta_percent: 0.0,
            estimated_assigned_percent: 0.0,
            proxy_projected_percent: 0.0,
            unattributed_percent: 0.0,
            attribution_coverage_percent: 0.0,
            external_activity_possible: false,
            confidence: Confidence::Unknown,
            method: "unavailable".to_string(),
            settled: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowUsage {
    pub token_usage: TokenUsage,
    pub local_token_share_percent: f64,
    pub estimated_quota_percent: f64,
    pub quota_confidence: Confidence,
    #[serde(default)]
    pub api_equivalent_cost: ApiCostAmount,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadWindowUsage {
    pub thread_id: String,
    pub usage: WindowUsage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnWindowUsage {
    pub thread_id: String,
    pub turn_id: String,
    pub usage: WindowUsage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowAnalysis {
    pub duration_mins: i64,
    pub attribution: AttributionSummary,
    #[serde(default)]
    pub partial: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
    pub threads: Vec<ThreadWindowUsage>,
    pub turns: Vec<TurnWindowUsage>,
    pub models: Vec<ModelUsage>,
    #[serde(default)]
    pub api_equivalent_cost: ApiEquivalentCost,
    #[serde(default)]
    pub api_pricing: ApiPricingMetadata,
    /// Alternative projection that applies the API-published long-context
    /// multiplier. It is a local display option, not part of snapshot JSON.
    #[serde(skip)]
    pub api_long_context: Option<Box<WindowAnalysis>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub source: String,
    pub status: String,
    pub as_of: DateTime<Utc>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionStats {
    pub discovered_files: usize,
    pub scanned_files: usize,
    pub truncated_files: usize,
    pub unreadable_files: usize,
    pub parsed_lines: usize,
    pub skipped_lines: usize,
    pub ambiguous_token_resets: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub schema_version: u32,
    #[serde(default)]
    pub api_pricing: ApiPricingMetadata,
    /// API-equivalent model-token total in the preferred current 5-hour window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_cost: Option<ApiEquivalentCost>,
    pub as_of: DateTime<Utc>,
    pub partial: bool,
    #[serde(with = "crate::exact_json::pathbuf_lossy")]
    pub codex_home: PathBuf,
    pub sources: Vec<SourceStatus>,
    pub limits: Vec<LimitBucket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_reset_credits: Option<RateLimitResetCreditsSnapshot>,
    #[serde(default, skip_serializing)]
    pub rate_limit_reset_credits_partial: bool,
    pub account_usage: Option<AccountTokenUsage>,
    pub tasks: Vec<TaskRecord>,
    pub turns: Vec<TurnRecord>,
    pub models: Vec<ModelUsage>,
    pub attribution: AttributionSummary,
    #[serde(default)]
    pub window_analyses: Vec<WindowAnalysis>,
    pub stats: CollectionStats,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageCall {
    pub timestamp: DateTime<Utc>,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub tokens: TokenUsage,
    /// Whether this call's token delta is the exact usage of one model request.
    ///
    /// Rollout counters are cumulative, so this is available only when the
    /// reported `last_token_usage` exactly matches the safe cumulative delta.
    pub request_usage_exact: bool,
}

impl UsageCall {
    pub fn is_fast(&self) -> bool {
        is_fast_service_tier(self.service_tier.as_deref())
    }
}

fn is_fast_service_tier(service_tier: Option<&str>) -> bool {
    service_tier.is_some_and(|tier| {
        let tier = tier.trim();
        tier.eq_ignore_ascii_case("fast") || tier.eq_ignore_ascii_case("priority")
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct RateObservation {
    pub timestamp: DateTime<Utc>,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub limit_id: String,
    pub primary: Option<LimitWindow>,
    pub secondary: Option<LimitWindow>,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Default)]
pub struct RolloutDataset {
    pub tasks: Vec<TaskRecord>,
    pub turns: Vec<TurnRecord>,
    /// Exact parent-turn to child-agent links reconstructed from matching
    /// parent function calls and `sub_agent_activity` events.
    pub agent_interactions: Vec<AgentInteraction>,
    pub calls: Vec<UsageCall>,
    pub rate_observations: Vec<RateObservation>,
    pub stats: CollectionStats,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AccountSnapshot {
    pub limits: Vec<LimitBucket>,
    pub rate_limit_reset_credits: Option<RateLimitResetCreditsSnapshot>,
    pub rate_limit_reset_credits_partial: bool,
    pub usage: Option<AccountTokenUsage>,
    pub rate_observations: Vec<RateObservation>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

pub fn aggregate_tokens_by_model(calls: &[UsageCall]) -> BTreeMap<String, TokenUsage> {
    let mut result = BTreeMap::new();
    for call in calls {
        let model = call.model.as_deref().unwrap_or("unknown").to_string();
        result
            .entry(model)
            .or_insert_with(TokenUsage::default)
            .add_assign(call.tokens);
    }
    result
}

pub fn terminal_safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() || is_bidi_control(character) {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}
