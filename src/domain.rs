use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
    }

    pub fn delta_from(self, previous: Self) -> Option<Self> {
        if self.input_tokens < previous.input_tokens
            || self.cached_input_tokens < previous.cached_input_tokens
            || self.output_tokens < previous.output_tokens
            || self.reasoning_output_tokens < previous.reasoning_output_tokens
            || self.total_tokens < previous.total_tokens
        {
            return None;
        }

        Some(Self {
            input_tokens: self.input_tokens - previous.input_tokens,
            cached_input_tokens: self.cached_input_tokens - previous.cached_input_tokens,
            output_tokens: self.output_tokens - previous.output_tokens,
            reasoning_output_tokens: self.reasoning_output_tokens
                - previous.reasoning_output_tokens,
            total_tokens: self.total_tokens - previous.total_tokens,
        })
    }
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
    pub title: String,
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
}

impl TurnRecord {
    pub fn is_fast(&self) -> bool {
        self.service_tier.as_deref() == Some("priority")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    pub token_usage: TokenUsage,
    pub local_token_share_percent: f64,
    pub estimated_quota_percent: f64,
    pub quota_confidence: Confidence,
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
    pub as_of: DateTime<Utc>,
    pub partial: bool,
    pub codex_home: PathBuf,
    pub sources: Vec<SourceStatus>,
    pub limits: Vec<LimitBucket>,
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
    pub tokens: TokenUsage,
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
    pub calls: Vec<UsageCall>,
    pub rate_observations: Vec<RateObservation>,
    pub stats: CollectionStats,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AccountSnapshot {
    pub limits: Vec<LimitBucket>,
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
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}
