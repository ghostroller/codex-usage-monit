//! Shared Summary query/report preparation for the CLI and TUI.
//!
//! [`PreparedSummary`] and [`SummaryChartData`] intentionally retain the
//! additive domain types from [`crate::summary`], so the TUI can render them
//! without performing a second aggregation. [`SummaryReport`] is the stable,
//! camel-case JSON projection used by non-interactive consumers.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[cfg(test)]
use chrono::FixedOffset;
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::api_cost::API_PRICING_CATALOG_REVISION;
use crate::config::CollectConfig;
use crate::domain::{ApiCostAmount, Snapshot, TokenUsage, WindowAnalysis};
use crate::history::{
    HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, HistoryData,
    HistoryObservation, LOCAL_BUCKET_MINUTES,
};
use crate::summary::{
    ProjectSummary, SessionSummary, SummaryMetrics, SummarySample, SummaryTurnKey, SummaryWindow,
    TurnSummary, UsageSummary, summarize_samples_with_local_time,
};

const WEEK_MINUTES: i64 = 10_080;
pub const SUMMARY_REPORT_SCHEMA_VERSION: u32 = 1;
pub const SUMMARY_HISTORY_DAYS: i64 = 31;
pub const SUMMARY_BACKFILL_MAX_FILES: usize = 5_000;
pub const SUMMARY_BACKFILL_RETRY_DAYS: i64 = 7;

#[cfg(test)]
thread_local! {
    static TEST_LOCAL_OFFSET: std::cell::Cell<Option<FixedOffset>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct SummaryTestLocalOffsetGuard(Option<FixedOffset>);

#[cfg(test)]
impl Drop for SummaryTestLocalOffsetGuard {
    fn drop(&mut self) {
        TEST_LOCAL_OFFSET.with(|current| current.set(self.0));
    }
}

/// Override the shared default local-time mapper on the current test thread.
/// Production builds always resolve [`Local`] separately for each timestamp.
#[cfg(test)]
pub(crate) fn set_test_local_offset(offset: FixedOffset) -> SummaryTestLocalOffsetGuard {
    SummaryTestLocalOffsetGuard(TEST_LOCAL_OFFSET.with(|current| current.replace(Some(offset))))
}

fn default_local_time(timestamp: DateTime<Utc>) -> NaiveDateTime {
    #[cfg(test)]
    if let Some(offset) = TEST_LOCAL_OFFSET.with(std::cell::Cell::get) {
        return timestamp.with_timezone(&offset).naive_local();
    }

    timestamp.with_timezone(&Local).naive_local()
}

/// Time range queried by Summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SummaryRange {
    #[default]
    #[serde(rename = "cycle")]
    Cycle,
    #[serde(rename = "7d")]
    SevenDays,
    #[serde(rename = "30d")]
    ThirtyDays,
}

impl SummaryRange {
    pub const ALL: [Self; 3] = [Self::Cycle, Self::SevenDays, Self::ThirtyDays];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Cycle => "Cycle",
            Self::SevenDays => "7d",
            Self::ThirtyDays => "30d",
        }
    }

    /// Resolves the half-open query window `[starts_at, ends_at)`.
    ///
    /// Cycle uses the active, non-stale Codex weekly window. When that window
    /// is unavailable it deliberately falls back to a rolling seven days and
    /// returns `"7d fallback"` as the range note, matching the TUI.
    pub fn window(
        self,
        snapshot: &Snapshot,
        query_now: DateTime<Utc>,
    ) -> (SummaryWindow, Option<&'static str>) {
        let query_now = query_now.max(snapshot.as_of);
        let ends_at = query_now
            .checked_add_signed(Duration::nanoseconds(1))
            .unwrap_or(query_now);
        let (starts_at, note) = match self {
            Self::Cycle => active_week_analysis(snapshot, query_now)
                .and_then(|analysis| analysis.attribution.window.as_ref())
                .map_or_else(
                    || (query_now - Duration::days(7), Some("7d fallback")),
                    |window| (window.starts_at, None),
                ),
            Self::SevenDays => (query_now - Duration::days(7), None),
            Self::ThirtyDays => (query_now - Duration::days(30), None),
        };
        (
            SummaryWindow::new(starts_at.min(query_now), ends_at)
                .expect("summary ranges always have a positive duration"),
            note,
        )
    }
}

/// Calendar bucket size used by the Summary chart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SummaryGrain {
    #[default]
    #[serde(rename = "1d")]
    Day,
    #[serde(rename = "12h")]
    Hours12,
    #[serde(rename = "6h")]
    Hours6,
    #[serde(rename = "3h")]
    Hours3,
    #[serde(rename = "1h")]
    Hour,
}

impl SummaryGrain {
    pub const ALL: [Self; 5] = [
        Self::Day,
        Self::Hours12,
        Self::Hours6,
        Self::Hours3,
        Self::Hour,
    ];

    pub const fn hours(self) -> u32 {
        match self {
            Self::Day => 24,
            Self::Hours12 => 12,
            Self::Hours6 => 6,
            Self::Hours3 => 3,
            Self::Hour => 1,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Day => "1d",
            Self::Hours12 => "12h",
            Self::Hours6 => "6h",
            Self::Hours3 => "3h",
            Self::Hour => "1h",
        }
    }

    pub fn bucket_start(self, hour: NaiveDateTime) -> NaiveDateTime {
        let bucket_hour = match self {
            Self::Day => 0,
            _ => hour.hour().div_euclid(self.hours()) * self.hours(),
        };
        hour.date().and_hms_opt(bucket_hour, 0, 0).unwrap_or(hour)
    }
}

/// Metric used to order the tree and select report values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SummaryMetric {
    #[default]
    Tokens,
    Estimated,
    ApiEquivalent,
}

impl SummaryMetric {
    pub const ALL: [Self; 3] = [Self::Tokens, Self::Estimated, Self::ApiEquivalent];

    pub fn value(self, metrics: SummaryMetrics, api_long_context: bool) -> u128 {
        match self {
            Self::Tokens => u128::from(metrics.token_usage.total_tokens),
            Self::Estimated => metrics.estimated_units(api_long_context),
            Self::ApiEquivalent => metrics.api_equivalent_cost.minimum_pico_usd.value(),
        }
    }

    pub fn share_percent(
        self,
        metrics: SummaryMetrics,
        total: SummaryMetrics,
        api_long_context: bool,
    ) -> f64 {
        percent_u128(
            self.value(metrics, api_long_context),
            self.value(total, api_long_context),
        )
    }
}

/// Complete query options for a JSON Summary report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryReportQuery {
    pub range: SummaryRange,
    pub grain: SummaryGrain,
    pub metric: SummaryMetric,
    pub api_long_context: bool,
    pub query_now: DateTime<Utc>,
}

impl SummaryReportQuery {
    pub const fn new(
        range: SummaryRange,
        grain: SummaryGrain,
        metric: SummaryMetric,
        api_long_context: bool,
        query_now: DateTime<Utc>,
    ) -> Self {
        Self {
            range,
            grain,
            metric,
            api_long_context,
            query_now,
        }
    }
}

/// Coverage inputs for one local day or hour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryDailyCoverage {
    pub expected_buckets: usize,
    pub covered_buckets: usize,
    pub available_tokens: u64,
    pub represented_tokens: u64,
    pub estimated_covered_tokens: u64,
    pub long_context_breakdown_complete: bool,
    pub source_partial: bool,
}

impl Default for SummaryDailyCoverage {
    fn default() -> Self {
        Self {
            expected_buckets: 0,
            covered_buckets: 0,
            available_tokens: 0,
            represented_tokens: 0,
            estimated_covered_tokens: 0,
            long_context_breakdown_complete: true,
            source_partial: false,
        }
    }
}

impl SummaryDailyCoverage {
    pub fn add_assign(&mut self, other: &Self) {
        self.expected_buckets = self.expected_buckets.saturating_add(other.expected_buckets);
        self.covered_buckets = self.covered_buckets.saturating_add(other.covered_buckets);
        self.available_tokens = self.available_tokens.saturating_add(other.available_tokens);
        self.represented_tokens = self
            .represented_tokens
            .saturating_add(other.represented_tokens);
        self.estimated_covered_tokens = self
            .estimated_covered_tokens
            .saturating_add(other.estimated_covered_tokens);
        self.long_context_breakdown_complete &= other.long_context_breakdown_complete;
        self.source_partial |= other.source_partial;
    }

    pub fn coverage_percent(&self, totals: SummaryMetrics, metric: SummaryMetric) -> f64 {
        coverage_percent(
            self.represented_tokens,
            self.available_tokens,
            self.covered_buckets,
            self.expected_buckets,
            self.estimated_covered_tokens,
            totals,
            metric,
        )
    }
}

/// Metric-aware status of a report or chart bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SummaryCoverageState {
    Complete,
    Partial,
    Missing,
}

/// Shared, lossless result of preparing history for Summary.
///
/// This is intentionally not a wire type: it retains [`UsageSummary`] and
/// [`SummaryMetrics`] so renderers can use all additive fields directly.
#[derive(Clone, Debug)]
pub struct PreparedSummary {
    pub usage: UsageSummary,
    pub range_note: Option<&'static str>,
    pub represented_tokens: u64,
    pub available_tokens: u64,
    pub covered_buckets: usize,
    pub expected_buckets: usize,
    pub estimated_covered_tokens: u64,
    pub long_context_breakdown_complete: bool,
    pub daily_coverage: BTreeMap<NaiveDate, SummaryDailyCoverage>,
    pub hourly_coverage: BTreeMap<NaiveDateTime, SummaryDailyCoverage>,
    pub partial_reasons: Vec<String>,
}

impl PreparedSummary {
    pub fn coverage_percent(&self, metric: SummaryMetric) -> f64 {
        coverage_percent(
            self.represented_tokens,
            self.available_tokens,
            self.covered_buckets,
            self.expected_buckets,
            self.estimated_covered_tokens,
            self.usage.totals,
            metric,
        )
    }

    pub fn partial(&self, metric: SummaryMetric, api_long_context: bool) -> bool {
        !self.partial_reasons.is_empty()
            || self.represented_tokens < self.available_tokens
            || self.covered_buckets < self.expected_buckets
            || (metric == SummaryMetric::ApiEquivalent && {
                let amount = self.usage.totals.api_equivalent_cost;
                amount.priced_samples < amount.observed_samples
                    || amount.priced_tokens < amount.observed_tokens
            })
            || (metric == SummaryMetric::Estimated
                && api_long_context
                && !self.long_context_breakdown_complete)
    }

    pub fn coverage_state(
        &self,
        metric: SummaryMetric,
        api_long_context: bool,
    ) -> SummaryCoverageState {
        if self.expected_buckets > 0 && self.covered_buckets == 0 {
            SummaryCoverageState::Missing
        } else if self.partial(metric, api_long_context) {
            SummaryCoverageState::Partial
        } else {
            SummaryCoverageState::Complete
        }
    }

    pub fn api_chart_is_lower_bound(&self) -> bool {
        let amount = self.usage.totals.api_equivalent_cost;
        !amount.range_is_exact()
            || amount.priced_samples < amount.observed_samples
            || amount.priced_tokens < amount.observed_tokens
            || self.represented_tokens < self.available_tokens
            || self.covered_buckets < self.expected_buckets
            || self.partial_reasons.iter().any(|reason| {
                reason.starts_with("rollout_")
                    || matches!(
                        reason.as_str(),
                        "local_scan_disabled" | "range_starts_within_15m_bucket"
                    )
            })
    }

    /// Whether the selected chart total is only a known lower bound. This is
    /// the shared rule used by the TUI annotation and one-shot report.
    pub fn chart_value_is_lower_bound(
        &self,
        chart: &SummaryChartData,
        metric: SummaryMetric,
        api_long_context: bool,
    ) -> bool {
        summary_chart_value_is_lower_bound_with_local_time(
            self,
            chart,
            metric,
            api_long_context,
            &default_local_time,
        )
    }

    /// Metric-aware state using the host's true local offset at each edge.
    pub fn daily_state(
        &self,
        date: NaiveDate,
        totals: SummaryMetrics,
        metric: SummaryMetric,
        api_long_context: bool,
    ) -> SummaryCoverageState {
        self.daily_state_with_local_time(date, totals, metric, api_long_context, default_local_time)
    }

    pub fn daily_state_with_local_time(
        &self,
        date: NaiveDate,
        totals: SummaryMetrics,
        metric: SummaryMetric,
        api_long_context: bool,
        local_time: impl Fn(DateTime<Utc>) -> NaiveDateTime,
    ) -> SummaryCoverageState {
        let Some(coverage) = self.daily_coverage.get(&date) else {
            return SummaryCoverageState::Missing;
        };
        if coverage.covered_buckets == 0 {
            return SummaryCoverageState::Missing;
        }
        if coverage_is_partial(coverage, totals, metric, api_long_context)
            || summary_day_is_partial_window_edge(self.usage.window, date, &local_time)
        {
            SummaryCoverageState::Partial
        } else {
            SummaryCoverageState::Complete
        }
    }

    /// Metric-aware chart state using the host's true local offset at each
    /// edge. Day buckets share the exact daily-state rules used by the TUI.
    pub fn chart_bucket_state(
        &self,
        bucket: &SummaryChartBucket,
        grain: SummaryGrain,
        metric: SummaryMetric,
        api_long_context: bool,
    ) -> SummaryCoverageState {
        self.chart_bucket_state_with_local_time(
            bucket,
            grain,
            metric,
            api_long_context,
            default_local_time,
        )
    }

    pub fn chart_bucket_state_with_local_time(
        &self,
        bucket: &SummaryChartBucket,
        grain: SummaryGrain,
        metric: SummaryMetric,
        api_long_context: bool,
        local_time: impl Fn(DateTime<Utc>) -> NaiveDateTime,
    ) -> SummaryCoverageState {
        if grain == SummaryGrain::Day {
            return self.daily_state_with_local_time(
                bucket.starts_at.date(),
                bucket.totals,
                metric,
                api_long_context,
                local_time,
            );
        }
        if bucket.coverage.covered_buckets == 0 {
            return SummaryCoverageState::Missing;
        }
        if coverage_is_partial(&bucket.coverage, bucket.totals, metric, api_long_context)
            || summary_bucket_is_partial_window_edge(
                self.usage.window,
                bucket.starts_at,
                grain,
                &local_time,
            )
        {
            SummaryCoverageState::Partial
        } else {
            SummaryCoverageState::Complete
        }
    }
}

#[derive(Clone, Debug)]
pub struct SummaryChartBucket {
    pub starts_at: NaiveDateTime,
    pub totals: SummaryMetrics,
    pub coverage: SummaryDailyCoverage,
}

#[derive(Clone, Debug)]
pub struct SummaryChartData {
    pub grain: SummaryGrain,
    pub buckets: Vec<SummaryChartBucket>,
    pub project_values: HashMap<String, BTreeMap<NaiveDateTime, SummaryMetrics>>,
}

fn summary_chart_value_is_lower_bound_with_local_time(
    prepared: &PreparedSummary,
    chart: &SummaryChartData,
    metric: SummaryMetric,
    api_long_context: bool,
    local_time: &impl Fn(DateTime<Utc>) -> NaiveDateTime,
) -> bool {
    chart.buckets.iter().any(|bucket| {
        prepared.chart_bucket_state_with_local_time(
            bucket,
            chart.grain,
            metric,
            api_long_context,
            local_time,
        ) != SummaryCoverageState::Complete
    }) || prepared.partial(metric, api_long_context)
        || (metric == SummaryMetric::ApiEquivalent && prepared.api_chart_is_lower_bound())
}

/// Earliest timestamp loaded by the shared history view. The timestamp is
/// aligned down to the persisted 15-minute bucket boundary before retaining
/// the extra 31st day needed to answer a rolling 30-day query safely.
pub fn history_view_since(now: DateTime<Utc>) -> DateTime<Utc> {
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let aligned_seconds = now.timestamp().div_euclid(bucket_seconds) * bucket_seconds;
    DateTime::from_timestamp(aligned_seconds, 0).unwrap_or(now)
        - Duration::days(SUMMARY_HISTORY_DAYS)
}

/// Pure configuration projection for a local-only 30-day Summary backfill.
pub fn summary_backfill_config(config: &CollectConfig) -> CollectConfig {
    let mut config = config.clone();
    config.lookback_days = SUMMARY_HISTORY_DAYS;
    config.max_files = config.max_files.max(SUMMARY_BACKFILL_MAX_FILES);
    // The backfill reconstructs local project history only. The regular
    // refresh worker remains the owner of account/App Server data.
    config.offline = true;
    config
}

/// Whether a new 30-day reconstruction should be attempted now.
pub fn summary_history_backfill_needed(history: &HistoryData, now: DateTime<Utc>) -> bool {
    if history.read_only {
        return false;
    }
    if history
        .summary_backfill_attempted_at
        .is_some_and(|attempted_at| {
            attempted_at > now || now - attempted_at < Duration::days(SUMMARY_BACKFILL_RETRY_DAYS)
        })
    {
        return false;
    }
    history.summary_backfill_attempt_complete == Some(false)
        || !summary_history_coverage_complete(history, now)
}

/// Checks the 30-day history contract required for exact Summary output.
///
/// A covered bucket must have current project attribution, estimator and API
/// pricing revisions, plus the separately stored Longx component. This remains
/// a pure query; it does not trust a prior backfill marker over actual buckets.
pub fn summary_history_coverage_complete(history: &HistoryData, now: DateTime<Utc>) -> bool {
    let ends_at = now
        .checked_add_signed(Duration::nanoseconds(1))
        .unwrap_or(now);
    let Some(window) = SummaryWindow::new(now - Duration::days(30), ends_at) else {
        return true;
    };
    let expected =
        expected_summary_coverage(window, |timestamp| default_local_time(timestamp).date())
            .values()
            .map(|coverage| coverage.expected_buckets)
            .fold(0_usize, usize::saturating_add);
    let covered = history
        .half_hour_buckets
        .iter()
        .filter(|bucket| {
            window.contains(bucket.starts_at)
                && bucket.project_breakdown_revision == HISTORY_PROJECT_BREAKDOWN_REVISION
                && bucket.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION
                && bucket.estimator_revision == HISTORY_ESTIMATOR_REVISION
                && bucket.api_long_context_extra_cost_units.is_some()
        })
        .count();
    covered >= expected
}

/// Whether the rollout scan itself is complete enough to mark a backfill
/// attempt successful.
pub fn summary_backfill_scan_complete(snapshot: &Snapshot) -> bool {
    let stats = &snapshot.stats;
    stats.truncated_files == 0
        && stats.unreadable_files == 0
        && stats.skipped_lines == 0
        && stats.ambiguous_token_resets == 0
        && stats.scanned_files == stats.discovered_files
        && snapshot
            .sources
            .iter()
            .any(|source| source.source == "rollout_jsonl" && source.status == "ok")
}

/// Retain only reconstructed buckets backed by actual usage evidence.
///
/// A complete filesystem walk cannot prove that an absent rollout never
/// existed. Prospective zero buckets already in HistoryStore are preserved by
/// its merge; synthesized backfill zeros are discarded here. Spark contributes
/// a project group but no bucket-level Codex call count, so either signal is
/// sufficient evidence.
pub fn retain_summary_backfill_evidence_buckets(observation: &mut HistoryObservation) {
    observation
        .half_hour_buckets
        .retain(|bucket| bucket.call_count > 0 || !bucket.project_groups.is_empty());
}

/// Prepare Summary with the host's local timezone, resolving the real offset
/// separately for every timestamp so DST transitions remain correct.
pub fn prepare_summary(
    snapshot: &Snapshot,
    history: &HistoryData,
    range: SummaryRange,
    query_now: DateTime<Utc>,
) -> PreparedSummary {
    prepare_summary_with_local_time(snapshot, history, range, query_now, default_local_time)
}

/// Deterministic/timezone-pluggable variant used by tests and UTC consumers.
pub fn prepare_summary_with_local_time(
    snapshot: &Snapshot,
    history: &HistoryData,
    range: SummaryRange,
    query_now: DateTime<Utc>,
    local_time: impl Fn(DateTime<Utc>) -> NaiveDateTime,
) -> PreparedSummary {
    let (window, range_note) = range.window(snapshot, query_now);
    let mut samples = Vec::new();
    let mut represented_tokens = 0_u64;
    let mut available_tokens = 0_u64;
    let mut covered_buckets = 0_usize;
    let mut estimated_covered_tokens = 0_u64;
    let mut long_context_breakdown_complete = true;
    let mut daily_coverage =
        expected_summary_coverage(window, |timestamp| local_time(timestamp).date());
    let mut hourly_coverage =
        expected_summary_coverage(window, |timestamp| local_hour(local_time(timestamp)));
    let mut partial_reasons = history.warnings.clone();
    if range_note.is_some() {
        partial_reasons.push("cycle_window_unavailable".to_string());
    }

    for bucket in &history.half_hour_buckets {
        if !window.contains(bucket.starts_at) {
            continue;
        }
        let local_datetime = local_time(bucket.starts_at);
        let local_date = local_datetime.date();
        let local_hour = local_hour(local_datetime);
        for coverage in [
            daily_coverage.entry(local_date).or_default(),
            hourly_coverage.entry(local_hour).or_default(),
        ] {
            coverage.available_tokens = coverage
                .available_tokens
                .saturating_add(bucket.token_usage.total_tokens);
            coverage.long_context_breakdown_complete &= !bucket.long_context_usage_unknown;
            coverage.source_partial |=
                !bucket.partial_reasons.is_empty() || bucket.sampled_at < bucket.ends_at;
        }
        if bucket.sampled_at < bucket.ends_at {
            partial_reasons.push("history_bucket_open".to_string());
        }

        let project_breakdown_current =
            bucket.project_breakdown_revision == HISTORY_PROJECT_BREAKDOWN_REVISION;
        if project_breakdown_current {
            covered_buckets = covered_buckets.saturating_add(1);
            for coverage in [
                daily_coverage.entry(local_date).or_default(),
                hourly_coverage.entry(local_hour).or_default(),
            ] {
                coverage.covered_buckets = coverage.covered_buckets.saturating_add(1);
            }
        } else {
            partial_reasons.push("project_breakdown_unavailable".to_string());
        }

        available_tokens = available_tokens.saturating_add(bucket.token_usage.total_tokens);
        partial_reasons.extend(bucket.partial_reasons.iter().cloned());
        long_context_breakdown_complete &= !bucket.long_context_usage_unknown;
        if bucket.api_pricing_catalog_revision != API_PRICING_CATALOG_REVISION {
            partial_reasons.push("api_pricing_catalog_outdated".to_string());
        }

        let estimator_current = bucket.estimator_revision == HISTORY_ESTIMATOR_REVISION;
        if estimator_current {
            estimated_covered_tokens =
                estimated_covered_tokens.saturating_add(bucket.token_usage.total_tokens);
            for coverage in [
                daily_coverage.entry(local_date).or_default(),
                hourly_coverage.entry(local_hour).or_default(),
            ] {
                coverage.estimated_covered_tokens = coverage
                    .estimated_covered_tokens
                    .saturating_add(bucket.token_usage.total_tokens);
            }
        } else {
            partial_reasons.push("estimator_revision_changed".to_string());
        }

        if !project_breakdown_current {
            continue;
        }
        for group in &bucket.project_groups {
            represented_tokens = represented_tokens.saturating_add(group.token_usage.total_tokens);
            for coverage in [
                daily_coverage.entry(local_date).or_default(),
                hourly_coverage.entry(local_hour).or_default(),
            ] {
                coverage.represented_tokens = coverage
                    .represented_tokens
                    .saturating_add(group.token_usage.total_tokens);
            }
            if estimator_current
                && (!group.token_usage.is_zero() || group.estimated_cost_units > 0)
                && group.api_long_context_extra_cost_units.is_none()
            {
                long_context_breakdown_complete = false;
                daily_coverage
                    .entry(local_date)
                    .or_default()
                    .long_context_breakdown_complete = false;
                hourly_coverage
                    .entry(local_hour)
                    .or_default()
                    .long_context_breakdown_complete = false;
            }
            let (estimated_cost_units, api_long_context_extra_cost_units) =
                summary_estimated_units_for_revision(
                    group.estimated_cost_units,
                    group.api_long_context_extra_cost_units.unwrap_or_default(),
                    bucket.estimator_revision,
                );
            samples.push(SummarySample {
                timestamp: bucket.starts_at,
                thread_id: group.thread_id.clone(),
                parent_thread_id: group.parent_thread_id.clone(),
                turn_id: group.turn_id.clone(),
                session_thread_id: group.session_thread_id.clone(),
                session_turn_id: group.session_turn_id.clone(),
                message_preview: group.message_preview.clone(),
                turn_started_at: group.turn_started_at,
                project_key: group.project_id.clone(),
                project_label: group.project_label.clone(),
                cwd: None,
                title: group.title.clone(),
                source: group.source.clone(),
                token_usage: group.token_usage,
                estimated_cost_units,
                api_long_context_extra_cost_units,
                api_equivalent_cost: summary_api_cost_for_catalog(
                    group.api_equivalent_cost,
                    bucket.api_pricing_catalog_revision,
                ),
                call_count: group.call_count,
            });
        }
    }

    // Live tasks are metadata-only overlays. They update titles, source and
    // project labels without adding usage or disturbing closed history.
    samples.extend(snapshot.tasks.iter().map(|task| {
        SummarySample {
            timestamp: snapshot.as_of,
            thread_id: task.thread_id.clone(),
            parent_thread_id: task.parent_thread_id.clone(),
            turn_id: None,
            session_thread_id: None,
            session_turn_id: None,
            message_preview: None,
            turn_started_at: None,
            project_key: None,
            project_label: task
                .cwd
                .as_deref()
                .and_then(|cwd| cwd.file_name())
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            cwd: task.cwd.clone(),
            title: Some(task.title.clone()),
            source: task.source.clone(),
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: 0,
            api_equivalent_cost: ApiCostAmount::default(),
            call_count: 0,
        }
    }));

    if window
        .starts_at
        .timestamp()
        .rem_euclid(LOCAL_BUCKET_MINUTES * 60)
        != 0
    {
        partial_reasons.push("range_starts_within_15m_bucket".to_string());
    }
    if history.read_only {
        partial_reasons.push("history_read_only".to_string());
    }
    partial_reasons.sort();
    partial_reasons.dedup();

    let expected_buckets = daily_coverage
        .values()
        .map(|coverage| coverage.expected_buckets)
        .fold(0_usize, usize::saturating_add);
    let usage = summarize_samples_with_local_time(&samples, window, local_time);
    PreparedSummary {
        usage,
        range_note,
        represented_tokens,
        available_tokens,
        covered_buckets,
        expected_buckets,
        estimated_covered_tokens,
        long_context_breakdown_complete,
        daily_coverage,
        hourly_coverage,
        partial_reasons,
    }
}

/// Re-bucket a prepared summary without rescanning history.
pub fn prepare_summary_chart(prepared: &PreparedSummary, grain: SummaryGrain) -> SummaryChartData {
    let mut coverage_by_bucket = BTreeMap::<NaiveDateTime, SummaryDailyCoverage>::new();
    for (hour, coverage) in &prepared.hourly_coverage {
        coverage_by_bucket
            .entry(grain.bucket_start(*hour))
            .or_default()
            .add_assign(coverage);
    }

    let mut totals_by_bucket = BTreeMap::<NaiveDateTime, SummaryMetrics>::new();
    for hour in &prepared.usage.hours {
        totals_by_bucket
            .entry(grain.bucket_start(hour.starts_at))
            .or_default()
            .add_assign(hour.totals);
    }
    for starts_at in totals_by_bucket.keys().copied().collect::<Vec<_>>() {
        coverage_by_bucket.entry(starts_at).or_default();
    }

    let buckets = coverage_by_bucket
        .iter()
        .map(|(starts_at, coverage)| SummaryChartBucket {
            starts_at: *starts_at,
            totals: totals_by_bucket.get(starts_at).copied().unwrap_or_default(),
            coverage: coverage.clone(),
        })
        .collect();
    let project_values = prepared
        .usage
        .projects
        .iter()
        .map(|project| {
            let mut by_bucket = BTreeMap::<NaiveDateTime, SummaryMetrics>::new();
            for hour in &project.hours {
                by_bucket
                    .entry(grain.bucket_start(hour.starts_at))
                    .or_default()
                    .add_assign(hour.totals);
            }
            (project.key.clone(), by_bucket)
        })
        .collect();

    SummaryChartData {
        grain,
        buckets,
        project_values,
    }
}

/// JSON-safe UTC window metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryReportWindow {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// JSON projection of all additive metrics. Both estimated variants are kept
/// so clients can audit what the Longx selection changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryReportMetrics {
    pub token_usage: TokenUsage,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub estimated_cost_units: u128,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub api_long_context_extra_cost_units: u128,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub estimated_with_api_long_context_cost_units: u128,
    pub api_equivalent_cost: ApiCostAmount,
    pub call_count: u64,
}

impl From<SummaryMetrics> for SummaryReportMetrics {
    fn from(metrics: SummaryMetrics) -> Self {
        Self {
            token_usage: metrics.token_usage,
            estimated_cost_units: metrics.estimated_cost_units,
            api_long_context_extra_cost_units: metrics.api_long_context_extra_cost_units,
            estimated_with_api_long_context_cost_units: metrics.estimated_units(true),
            api_equivalent_cost: metrics.api_equivalent_cost,
            call_count: metrics.call_count,
        }
    }
}

/// JSON coverage details for either the whole report or one chart bucket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryCoverageReport {
    pub state: SummaryCoverageState,
    pub percent: f64,
    pub expected_buckets: usize,
    pub covered_buckets: usize,
    pub available_tokens: u64,
    pub represented_tokens: u64,
    pub estimated_covered_tokens: u64,
    pub long_context_breakdown_complete: bool,
    pub source_partial: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryBucketReport {
    /// Start of the bucket in the local wall clock selected by the builder.
    pub starts_at: NaiveDateTime,
    pub metrics: SummaryReportMetrics,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub value: u128,
    pub coverage: SummaryCoverageReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryProjectBucketReport {
    /// Start of the sparse project bucket in the selected local wall clock.
    pub starts_at: NaiveDateTime,
    pub metrics: SummaryReportMetrics,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub value: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SummaryTurnAttribution {
    Exact,
    UnassignedSession,
    UnassignedDelegated,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryTurnReport {
    pub attribution: SummaryTurnAttribution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    pub metrics: SummaryReportMetrics,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub value: u128,
    pub share_percent: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarySessionReport {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::exact_json::optional_pathbuf_lossy"
    )]
    pub cwd: Option<PathBuf>,
    pub metrics: SummaryReportMetrics,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub value: u128,
    pub share_percent: f64,
    pub turns: Vec<SummaryTurnReport>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryProjectReport {
    pub key: String,
    pub label: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::exact_json::optional_pathbuf_lossy"
    )]
    pub cwd: Option<PathBuf>,
    pub metrics: SummaryReportMetrics,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub value: u128,
    pub share_percent: f64,
    /// Sparse buckets: absent entries mean this project has no represented
    /// usage in that chart bucket; global coverage determines zero vs unknown.
    pub buckets: Vec<SummaryProjectBucketReport>,
    pub sessions: Vec<SummarySessionReport>,
}

/// Stable, camel-case JSON report derived from the same prepared data rendered
/// by the TUI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryReport {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub range: SummaryRange,
    pub grain: SummaryGrain,
    pub metric: SummaryMetric,
    pub api_long_context: bool,
    pub window: SummaryReportWindow,
    pub metrics: SummaryReportMetrics,
    #[serde(with = "crate::exact_json::u128_decimal")]
    pub value: u128,
    pub coverage: SummaryCoverageReport,
    pub value_is_lower_bound: bool,
    pub api_chart_is_lower_bound: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
    pub buckets: Vec<SummaryBucketReport>,
    pub projects: Vec<SummaryProjectReport>,
}

/// Build a complete report in the host's local timezone.
pub fn build_summary_report(
    snapshot: &Snapshot,
    history: &HistoryData,
    query: SummaryReportQuery,
) -> SummaryReport {
    build_summary_report_with_local_time(snapshot, history, query, default_local_time)
}

/// Build a complete report with an explicit per-timestamp wall-clock mapper.
/// Passing `|timestamp| timestamp.naive_utc()` yields UTC buckets.
pub fn build_summary_report_with_local_time(
    snapshot: &Snapshot,
    history: &HistoryData,
    query: SummaryReportQuery,
    local_time: impl Fn(DateTime<Utc>) -> NaiveDateTime,
) -> SummaryReport {
    let prepared = prepare_summary_with_local_time(
        snapshot,
        history,
        query.range,
        query.query_now,
        &local_time,
    );
    let chart = prepare_summary_chart(&prepared, query.grain);
    summary_report_from_prepared_with_local_time(&prepared, &chart, query, local_time)
}

/// Project a prepared result to JSON without rescanning history.
pub fn summary_report_from_prepared(
    prepared: &PreparedSummary,
    chart: &SummaryChartData,
    query: SummaryReportQuery,
) -> SummaryReport {
    summary_report_from_prepared_with_local_time(prepared, chart, query, default_local_time)
}

pub fn summary_report_from_prepared_with_local_time(
    prepared: &PreparedSummary,
    chart: &SummaryChartData,
    query: SummaryReportQuery,
    local_time: impl Fn(DateTime<Utc>) -> NaiveDateTime,
) -> SummaryReport {
    debug_assert_eq!(chart.grain, query.grain);
    let total_metrics = prepared.usage.totals;
    let source_partial = prepared
        .daily_coverage
        .values()
        .any(|coverage| coverage.source_partial);
    let coverage = SummaryCoverageReport {
        state: prepared.coverage_state(query.metric, query.api_long_context),
        percent: prepared.coverage_percent(query.metric),
        expected_buckets: prepared.expected_buckets,
        covered_buckets: prepared.covered_buckets,
        available_tokens: prepared.available_tokens,
        represented_tokens: prepared.represented_tokens,
        estimated_covered_tokens: prepared.estimated_covered_tokens,
        long_context_breakdown_complete: prepared.long_context_breakdown_complete,
        source_partial,
    };
    let buckets = chart
        .buckets
        .iter()
        .map(|bucket| SummaryBucketReport {
            starts_at: bucket.starts_at,
            metrics: bucket.totals.into(),
            value: query.metric.value(bucket.totals, query.api_long_context),
            coverage: SummaryCoverageReport {
                state: prepared.chart_bucket_state_with_local_time(
                    bucket,
                    query.grain,
                    query.metric,
                    query.api_long_context,
                    &local_time,
                ),
                percent: bucket
                    .coverage
                    .coverage_percent(bucket.totals, query.metric),
                expected_buckets: bucket.coverage.expected_buckets,
                covered_buckets: bucket.coverage.covered_buckets,
                available_tokens: bucket.coverage.available_tokens,
                represented_tokens: bucket.coverage.represented_tokens,
                estimated_covered_tokens: bucket.coverage.estimated_covered_tokens,
                long_context_breakdown_complete: bucket.coverage.long_context_breakdown_complete,
                source_partial: bucket.coverage.source_partial,
            },
        })
        .collect();

    let projects = sorted_projects(&prepared.usage, query.metric, query.api_long_context)
        .into_iter()
        .map(|project| {
            project_report(
                project,
                chart.project_values.get(&project.key),
                total_metrics,
                query.metric,
                query.api_long_context,
            )
        })
        .collect();
    let value_is_lower_bound = summary_chart_value_is_lower_bound_with_local_time(
        prepared,
        chart,
        query.metric,
        query.api_long_context,
        &local_time,
    );

    SummaryReport {
        schema_version: SUMMARY_REPORT_SCHEMA_VERSION,
        generated_at: query.query_now.max(
            prepared
                .usage
                .window
                .ends_at
                .checked_sub_signed(Duration::nanoseconds(1))
                .unwrap_or(query.query_now),
        ),
        range: query.range,
        grain: query.grain,
        metric: query.metric,
        api_long_context: query.api_long_context,
        window: SummaryReportWindow {
            starts_at: prepared.usage.window.starts_at,
            ends_at: prepared.usage.window.ends_at,
            note: prepared.range_note.map(str::to_string),
        },
        metrics: total_metrics.into(),
        value: query.metric.value(total_metrics, query.api_long_context),
        coverage,
        value_is_lower_bound,
        api_chart_is_lower_bound: prepared.api_chart_is_lower_bound(),
        partial_reasons: prepared.partial_reasons.clone(),
        buckets,
        projects,
    }
}

fn project_report(
    project: &ProjectSummary,
    project_buckets: Option<&BTreeMap<NaiveDateTime, SummaryMetrics>>,
    total_metrics: SummaryMetrics,
    metric: SummaryMetric,
    api_long_context: bool,
) -> SummaryProjectReport {
    let sessions = sorted_sessions(&project.sessions, metric, api_long_context)
        .into_iter()
        .map(|session| session_report(session, total_metrics, metric, api_long_context))
        .collect();
    let buckets = project_buckets
        .into_iter()
        .flat_map(|buckets| buckets.iter())
        .map(|(starts_at, totals)| SummaryProjectBucketReport {
            starts_at: *starts_at,
            metrics: (*totals).into(),
            value: metric.value(*totals, api_long_context),
        })
        .collect();
    SummaryProjectReport {
        key: project.key.clone(),
        label: project.label.clone(),
        cwd: project.cwd.clone(),
        metrics: project.totals.into(),
        value: metric.value(project.totals, api_long_context),
        share_percent: metric.share_percent(project.totals, total_metrics, api_long_context),
        buckets,
        sessions,
    }
}

fn session_report(
    session: &SessionSummary,
    total_metrics: SummaryMetrics,
    metric: SummaryMetric,
    api_long_context: bool,
) -> SummarySessionReport {
    let turns = sorted_turns(&session.turns, metric, api_long_context)
        .into_iter()
        .map(|turn| turn_report(turn, total_metrics, metric, api_long_context))
        .collect();
    SummarySessionReport {
        thread_id: session.thread_id.clone(),
        title: session.title.clone(),
        source: session.source.clone(),
        cwd: session.cwd.clone(),
        metrics: session.totals.into(),
        value: metric.value(session.totals, api_long_context),
        share_percent: metric.share_percent(session.totals, total_metrics, api_long_context),
        turns,
    }
}

fn turn_report(
    turn: &TurnSummary,
    total_metrics: SummaryMetrics,
    metric: SummaryMetric,
    api_long_context: bool,
) -> SummaryTurnReport {
    let (attribution, turn_id) = match &turn.key {
        SummaryTurnKey::Exact(turn_id) => (SummaryTurnAttribution::Exact, Some(turn_id.clone())),
        SummaryTurnKey::UnassignedSession => (SummaryTurnAttribution::UnassignedSession, None),
        SummaryTurnKey::UnassignedDelegated => (SummaryTurnAttribution::UnassignedDelegated, None),
    };
    SummaryTurnReport {
        attribution,
        turn_id,
        message_preview: turn.message_preview.clone(),
        started_at: turn.started_at,
        metrics: turn.totals.into(),
        value: metric.value(turn.totals, api_long_context),
        share_percent: metric.share_percent(turn.totals, total_metrics, api_long_context),
    }
}

fn active_week_analysis(snapshot: &Snapshot, query_now: DateTime<Utc>) -> Option<&WindowAnalysis> {
    let analysis = snapshot.window_analyses.iter().find(|analysis| {
        analysis.duration_mins == WEEK_MINUTES
            && analysis
                .attribution
                .window
                .as_ref()
                .is_some_and(|window| window.limit_id.trim().eq_ignore_ascii_case("codex"))
    })?;
    (!analysis
        .partial_reasons
        .iter()
        .any(|reason| reason == "quota_window_stale")
        && analysis
            .attribution
            .window
            .as_ref()
            .is_some_and(|window| window.starts_at <= query_now && query_now < window.ends_at))
    .then_some(analysis)
}

pub(crate) fn expected_summary_coverage<K: Ord>(
    window: SummaryWindow,
    local_key: impl Fn(DateTime<Utc>) -> K,
) -> BTreeMap<K, SummaryDailyCoverage> {
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let mut starts_at_seconds = window
        .starts_at
        .timestamp()
        .div_euclid(bucket_seconds)
        .saturating_mul(bucket_seconds);
    let aligned = DateTime::from_timestamp(starts_at_seconds, 0).unwrap_or(window.starts_at);
    if aligned < window.starts_at {
        starts_at_seconds = starts_at_seconds.saturating_add(bucket_seconds);
    }
    let mut starts_at = DateTime::from_timestamp(starts_at_seconds, 0).unwrap_or(window.starts_at);
    let mut coverage = BTreeMap::<K, SummaryDailyCoverage>::new();
    while starts_at < window.ends_at {
        let bucket = coverage.entry(local_key(starts_at)).or_default();
        bucket.expected_buckets = bucket.expected_buckets.saturating_add(1);
        let Some(next) = starts_at.checked_add_signed(Duration::seconds(bucket_seconds)) else {
            break;
        };
        starts_at = next;
    }
    coverage
}

pub(crate) fn summary_api_cost_for_catalog(
    amount: ApiCostAmount,
    catalog_revision: u32,
) -> ApiCostAmount {
    if catalog_revision == API_PRICING_CATALOG_REVISION {
        amount
    } else {
        ApiCostAmount {
            observed_samples: amount.observed_samples,
            observed_tokens: amount.observed_tokens,
            ..ApiCostAmount::default()
        }
    }
}

fn summary_estimated_units_for_revision(
    base_units: u128,
    long_context_extra_units: u128,
    estimator_revision: u32,
) -> (u128, u128) {
    if estimator_revision == HISTORY_ESTIMATOR_REVISION {
        (base_units, long_context_extra_units)
    } else {
        (0, 0)
    }
}

fn coverage_percent(
    represented_tokens: u64,
    available_tokens: u64,
    covered_buckets: usize,
    expected_buckets: usize,
    estimated_covered_tokens: u64,
    totals: SummaryMetrics,
    metric: SummaryMetric,
) -> f64 {
    let token_coverage = if available_tokens == 0 {
        100.0
    } else {
        represented_tokens as f64 / available_tokens as f64 * 100.0
    };
    let time_coverage = if expected_buckets == 0 {
        100.0
    } else {
        covered_buckets as f64 / expected_buckets as f64 * 100.0
    };
    let pricing_coverage = if metric == SummaryMetric::ApiEquivalent {
        totals.api_equivalent_cost.priced_token_percent()
    } else {
        100.0
    };
    let estimator_coverage = if metric != SummaryMetric::Estimated || available_tokens == 0 {
        100.0
    } else {
        estimated_covered_tokens as f64 / available_tokens as f64 * 100.0
    };
    token_coverage
        .min(time_coverage)
        .min(pricing_coverage)
        .min(estimator_coverage)
        .clamp(0.0, 100.0)
}

fn coverage_is_partial(
    coverage: &SummaryDailyCoverage,
    totals: SummaryMetrics,
    metric: SummaryMetric,
    api_long_context: bool,
) -> bool {
    let metric_partial = match metric {
        SummaryMetric::Tokens => false,
        SummaryMetric::Estimated => {
            coverage.estimated_covered_tokens < coverage.available_tokens
                || (api_long_context && !coverage.long_context_breakdown_complete)
        }
        SummaryMetric::ApiEquivalent => {
            let amount = totals.api_equivalent_cost;
            amount.priced_samples < amount.observed_samples
                || amount.priced_tokens < amount.observed_tokens
                || !amount.range_is_exact()
        }
    };
    coverage.covered_buckets < coverage.expected_buckets
        || coverage.represented_tokens < coverage.available_tokens
        || coverage.source_partial
        || metric_partial
}

fn summary_day_is_partial_window_edge(
    window: SummaryWindow,
    date: NaiveDate,
    local_time: &impl Fn(DateTime<Utc>) -> NaiveDateTime,
) -> bool {
    let local_start = local_time(window.starts_at);
    let starts_on_date = local_start.date() == date;
    let local_end = local_time(window.ends_at);
    let ends_on_date = window
        .ends_at
        .checked_sub_signed(Duration::nanoseconds(1))
        .is_some_and(|last| local_time(last).date() == date);
    (starts_on_date && local_start.time() != NaiveTime::MIN)
        || (ends_on_date && local_end.time() != NaiveTime::MIN)
}

fn summary_bucket_is_partial_window_edge(
    window: SummaryWindow,
    starts_at: NaiveDateTime,
    grain: SummaryGrain,
    local_time: &impl Fn(DateTime<Utc>) -> NaiveDateTime,
) -> bool {
    let local_start = local_time(window.starts_at);
    let local_end = local_time(window.ends_at);
    let local_last = window
        .ends_at
        .checked_sub_signed(Duration::nanoseconds(1))
        .map(local_time)
        .unwrap_or(local_end);
    let bucket_end = starts_at
        .checked_add_signed(Duration::hours(i64::from(grain.hours())))
        .unwrap_or(starts_at);
    (grain.bucket_start(local_start) == starts_at && local_start != starts_at)
        || (grain.bucket_start(local_last) == starts_at && local_end != bucket_end)
}

fn local_hour(value: NaiveDateTime) -> NaiveDateTime {
    value
        .date()
        .and_hms_opt(value.hour(), 0, 0)
        .unwrap_or(value)
}

fn sorted_projects(
    summary: &UsageSummary,
    metric: SummaryMetric,
    api_long_context: bool,
) -> Vec<&ProjectSummary> {
    let mut projects = summary.projects.iter().collect::<Vec<_>>();
    projects.sort_by(|left, right| {
        metric
            .value(right.totals, api_long_context)
            .cmp(&metric.value(left.totals, api_long_context))
            .then_with(|| left.key.cmp(&right.key))
    });
    projects
}

fn sorted_sessions(
    sessions: &[SessionSummary],
    metric: SummaryMetric,
    api_long_context: bool,
) -> Vec<&SessionSummary> {
    let mut sessions = sessions.iter().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        metric
            .value(right.totals, api_long_context)
            .cmp(&metric.value(left.totals, api_long_context))
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    sessions
}

fn sorted_turns(
    turns: &[TurnSummary],
    metric: SummaryMetric,
    api_long_context: bool,
) -> Vec<&TurnSummary> {
    let mut turns = turns.iter().collect::<Vec<_>>();
    turns.sort_by(|left, right| {
        metric
            .value(right.totals, api_long_context)
            .cmp(&metric.value(left.totals, api_long_context))
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| left.key.cmp(&right.key))
    });
    turns
}

fn percent_u128(value: u128, total: u128) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone};
    use serde_json::Value;

    use super::*;
    use crate::domain::{
        AttributionSummary, CollectionStats, Confidence, LimitBucket, PicoUsd, Provenance,
        SourceStatus, TaskRecord, TaskStatus, WindowDescriptor,
    };
    use crate::history::{LocalHalfHourBucket, LocalProjectUsageGroup};

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn token_usage(total_tokens: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: total_tokens,
            total_tokens,
            ..TokenUsage::default()
        }
    }

    fn snapshot(as_of: DateTime<Utc>) -> Snapshot {
        Snapshot {
            schema_version: 2,
            api_pricing: Default::default(),
            api_equivalent_cost: None,
            as_of,
            partial: false,
            codex_home: PathBuf::from("/tmp/codex"),
            sources: Vec::<SourceStatus>::new(),
            limits: Vec::<LimitBucket>::new(),
            rate_limit_reset_credits: None,
            rate_limit_reset_credits_partial: false,
            account_usage: None,
            tasks: Vec::new(),
            turns: Vec::new(),
            models: Vec::new(),
            attribution: AttributionSummary::default(),
            window_analyses: Vec::new(),
            stats: CollectionStats::default(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn project_group(
        thread_id: &str,
        session_thread_id: &str,
        session_turn_id: &str,
        tokens: u64,
        estimated: u128,
        long_context: Option<u128>,
    ) -> LocalProjectUsageGroup {
        LocalProjectUsageGroup {
            thread_id: thread_id.to_string(),
            session_thread_id: Some(session_thread_id.to_string()),
            session_turn_id: Some(session_turn_id.to_string()),
            message_preview: Some("Root request".to_string()),
            turn_started_at: Some(at(29, 8, 0)),
            project_id: Some("project-alpha".to_string()),
            project_label: Some("alpha".to_string()),
            token_usage: token_usage(tokens),
            estimated_cost_units: estimated,
            api_long_context_extra_cost_units: long_context,
            api_equivalent_cost: ApiCostAmount {
                minimum_pico_usd: PicoUsd::new(u128::from(tokens) * 10),
                maximum_pico_usd: PicoUsd::new(u128::from(tokens) * 10),
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: tokens,
                priced_tokens: tokens,
            },
            call_count: 1,
            ..LocalProjectUsageGroup::default()
        }
    }

    fn bucket(
        starts_at: DateTime<Utc>,
        token_total: u64,
        groups: Vec<LocalProjectUsageGroup>,
    ) -> LocalHalfHourBucket {
        let ends_at = starts_at + Duration::minutes(LOCAL_BUCKET_MINUTES);
        LocalHalfHourBucket {
            starts_at,
            ends_at,
            sampled_at: ends_at,
            token_usage: token_usage(token_total),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: groups.iter().map(|group| group.call_count).sum(),
            groups: Vec::new(),
            project_groups: groups,
            partial_reasons: Vec::new(),
        }
    }

    #[test]
    fn cycle_uses_active_week_window_and_falls_back_when_stale() {
        let now = at(30, 10, 7);
        let starts_at = at(25, 2, 0);
        let mut snapshot = snapshot(now);
        snapshot.window_analyses.push(WindowAnalysis {
            duration_mins: WEEK_MINUTES,
            attribution: AttributionSummary {
                window: Some(WindowDescriptor {
                    limit_id: "codex".to_string(),
                    label: "Weekly".to_string(),
                    starts_at,
                    ends_at: at(31, 2, 0),
                    used_percent: 10.0,
                }),
                confidence: Confidence::High,
                ..AttributionSummary::default()
            },
            partial: false,
            partial_reasons: Vec::new(),
            threads: Vec::new(),
            turns: Vec::new(),
            models: Vec::new(),
            api_equivalent_cost: Default::default(),
            api_pricing: Default::default(),
            api_long_context: None,
        });

        let (window, note) = SummaryRange::Cycle.window(&snapshot, now);
        assert_eq!(window.starts_at, starts_at);
        assert_eq!(note, None);

        snapshot.window_analyses[0]
            .partial_reasons
            .push("quota_window_stale".to_string());
        let (window, note) = SummaryRange::Cycle.window(&snapshot, now);
        assert_eq!(window.starts_at, now - Duration::days(7));
        assert_eq!(note, Some("7d fallback"));
    }

    #[test]
    fn backfill_policy_is_local_bounded_and_retries_only_after_seven_days() {
        let now = at(30, 10, 7);
        assert_eq!(
            history_view_since(now),
            at(30, 10, 0) - Duration::days(SUMMARY_HISTORY_DAYS)
        );

        let config = CollectConfig {
            lookback_days: 90,
            max_files: 700,
            offline: false,
            ..CollectConfig::default()
        };
        let backfill = summary_backfill_config(&config);
        assert_eq!(backfill.lookback_days, SUMMARY_HISTORY_DAYS);
        assert_eq!(backfill.max_files, SUMMARY_BACKFILL_MAX_FILES);
        assert!(backfill.offline);
        assert_eq!(
            config.lookback_days, 90,
            "input config must remain unchanged"
        );

        let first_expected = now - Duration::days(30) + Duration::minutes(8);
        let mut starts_at = first_expected;
        let mut buckets = Vec::new();
        while starts_at <= now {
            buckets.push(bucket(starts_at, 0, Vec::new()));
            starts_at += Duration::minutes(LOCAL_BUCKET_MINUTES);
        }
        let complete = HistoryData {
            half_hour_buckets: buckets,
            ..HistoryData::default()
        };
        assert!(summary_history_coverage_complete(&complete, now));
        assert!(!summary_history_backfill_needed(&complete, now));

        let mut failed = complete.clone();
        failed.summary_backfill_attempt_complete = Some(false);
        failed.summary_backfill_attempted_at = Some(now - Duration::days(1));
        assert!(!summary_history_backfill_needed(&failed, now));
        failed.summary_backfill_attempted_at =
            Some(now - Duration::days(SUMMARY_BACKFILL_RETRY_DAYS + 1));
        assert!(summary_history_backfill_needed(&failed, now));
        failed.summary_backfill_attempted_at = Some(now + Duration::days(1));
        assert!(!summary_history_backfill_needed(&failed, now));

        let mut incomplete = complete;
        incomplete.half_hour_buckets.pop();
        assert!(summary_history_backfill_needed(&incomplete, now));
        incomplete.read_only = true;
        assert!(!summary_history_backfill_needed(&incomplete, now));
    }

    #[test]
    fn backfill_scan_requires_a_complete_successful_rollout_source() {
        let now = at(30, 10, 7);
        let mut snapshot = snapshot(now);
        snapshot.stats = CollectionStats {
            discovered_files: 1,
            scanned_files: 1,
            ..CollectionStats::default()
        };
        snapshot.sources = vec![SourceStatus {
            source: "rollout_jsonl".to_string(),
            status: "ok".to_string(),
            as_of: now,
            message: None,
        }];
        assert!(summary_backfill_scan_complete(&snapshot));

        snapshot.stats.ambiguous_token_resets = 1;
        assert!(!summary_backfill_scan_complete(&snapshot));
        snapshot.stats.ambiguous_token_resets = 0;
        snapshot.sources[0].status = "partial".to_string();
        assert!(!summary_backfill_scan_complete(&snapshot));
        snapshot.sources[0].status = "ok".to_string();
        snapshot.stats.scanned_files = 0;
        assert!(!summary_backfill_scan_complete(&snapshot));
    }

    #[test]
    fn backfill_retains_only_codex_or_spark_usage_evidence() {
        let starts_at = at(30, 8, 0);
        let mut codex = bucket(starts_at, 0, Vec::new());
        codex.call_count = 1;
        let spark = bucket(
            starts_at + Duration::minutes(15),
            0,
            vec![LocalProjectUsageGroup {
                thread_id: "spark-thread".to_string(),
                ..LocalProjectUsageGroup::default()
            }],
        );
        let empty = bucket(starts_at + Duration::minutes(30), 0, Vec::new());
        let mut observation = HistoryObservation {
            half_hour_buckets: vec![codex, spark, empty],
            ..HistoryObservation::default()
        };

        retain_summary_backfill_evidence_buckets(&mut observation);

        assert_eq!(observation.half_hour_buckets.len(), 2);
        assert_eq!(observation.half_hour_buckets[0].call_count, 1);
        assert_eq!(observation.half_hour_buckets[1].project_groups.len(), 1);
    }

    #[test]
    fn report_preserves_project_session_turn_tree_longx_and_sparse_buckets() {
        let now = at(30, 10, 7);
        let mut snapshot = snapshot(now);
        snapshot.tasks.push(TaskRecord {
            thread_id: "root".to_string(),
            parent_thread_id: None,
            archived: false,
            title: "Fresh title".to_string(),
            cwd: Some(PathBuf::from("/work/alpha")),
            source: Some("cli".to_string()),
            created_at: Some(at(29, 8, 0)),
            updated_at: Some(now),
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage::default(),
            turn_count: 1,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: None,
        });
        let history = HistoryData {
            half_hour_buckets: vec![bucket(
                at(30, 8, 15),
                150,
                vec![
                    project_group("root", "root", "turn-1", 50, 100, Some(25)),
                    project_group("child", "root", "turn-1", 100, 200, Some(75)),
                ],
            )],
            ..HistoryData::default()
        };
        let query = SummaryReportQuery::new(
            SummaryRange::SevenDays,
            SummaryGrain::Hours6,
            SummaryMetric::Estimated,
            true,
            now,
        );
        let report =
            build_summary_report_with_local_time(&snapshot, &history, query, |timestamp| {
                timestamp.naive_utc()
            });

        assert_eq!(report.metrics.token_usage.total_tokens, 150);
        assert_eq!(report.metrics.estimated_cost_units, 300);
        assert_eq!(report.metrics.api_long_context_extra_cost_units, 100);
        assert_eq!(report.value, 400);
        assert_eq!(report.projects.len(), 1);
        let project = &report.projects[0];
        assert_eq!(project.label, "alpha");
        assert_eq!(project.buckets.len(), 1);
        assert_eq!(project.buckets[0].starts_at, at(30, 6, 0).naive_utc());
        assert_eq!(project.sessions.len(), 1);
        let session = &project.sessions[0];
        assert_eq!(session.thread_id, "root");
        assert_eq!(session.title.as_deref(), Some("Fresh title"));
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(session.turns[0].metrics.token_usage.total_tokens, 150);
    }

    #[test]
    fn hourly_chart_distinguishes_complete_partial_and_missing_coverage() {
        let now = at(30, 10, 7);
        let snapshot = snapshot(now);
        let mut buckets = (0..4)
            .map(|quarter| bucket(at(30, 8, quarter * 15), 0, Vec::new()))
            .collect::<Vec<_>>();
        buckets.push(bucket(at(30, 9, 0), 0, Vec::new()));
        let history = HistoryData {
            half_hour_buckets: buckets,
            ..HistoryData::default()
        };
        let prepared = prepare_summary_with_local_time(
            &snapshot,
            &history,
            SummaryRange::SevenDays,
            now,
            |timestamp| timestamp.naive_utc(),
        );
        let chart = prepare_summary_chart(&prepared, SummaryGrain::Hour);
        let state_at = |starts_at| {
            let bucket = chart
                .buckets
                .iter()
                .find(|bucket| bucket.starts_at == starts_at)
                .unwrap();
            prepared.chart_bucket_state_with_local_time(
                bucket,
                SummaryGrain::Hour,
                SummaryMetric::Tokens,
                false,
                |timestamp| timestamp.naive_utc(),
            )
        };

        assert_eq!(
            state_at(at(30, 8, 0).naive_utc()),
            SummaryCoverageState::Complete
        );
        assert_eq!(
            state_at(at(30, 9, 0).naive_utc()),
            SummaryCoverageState::Partial
        );
        assert_eq!(
            state_at(at(30, 7, 0).naive_utc()),
            SummaryCoverageState::Missing
        );
    }

    #[test]
    fn outdated_estimator_pricing_and_missing_longx_are_reported_as_partial() {
        let now = at(30, 10, 7);
        let snapshot = snapshot(now);
        let mut old = bucket(
            at(30, 8, 15),
            100,
            vec![project_group("root", "root", "turn-1", 100, 500, None)],
        );
        old.estimator_revision = HISTORY_ESTIMATOR_REVISION.saturating_sub(1);
        old.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION.saturating_sub(1);
        let history = HistoryData {
            half_hour_buckets: vec![old],
            ..HistoryData::default()
        };
        let query = SummaryReportQuery::new(
            SummaryRange::SevenDays,
            SummaryGrain::Hour,
            SummaryMetric::ApiEquivalent,
            true,
            now,
        );
        let report =
            build_summary_report_with_local_time(&snapshot, &history, query, |timestamp| {
                timestamp.naive_utc()
            });

        assert_eq!(report.value, 0);
        assert_eq!(report.metrics.api_equivalent_cost.observed_tokens, 100);
        assert_eq!(report.metrics.api_equivalent_cost.priced_tokens, 0);
        assert_eq!(report.coverage.state, SummaryCoverageState::Partial);
        assert!(
            report
                .partial_reasons
                .contains(&"api_pricing_catalog_outdated".to_string())
        );
        assert!(
            report
                .partial_reasons
                .contains(&"estimator_revision_changed".to_string())
        );
    }

    #[test]
    fn report_json_uses_camel_case_and_cli_friendly_enum_values() {
        let now = at(30, 10, 7);
        let report = build_summary_report_with_local_time(
            &snapshot(now),
            &HistoryData::default(),
            SummaryReportQuery::new(
                SummaryRange::SevenDays,
                SummaryGrain::Hours6,
                SummaryMetric::ApiEquivalent,
                true,
                now,
            ),
            |timestamp| timestamp.naive_utc(),
        );
        let json = serde_json::to_value(report).unwrap();

        assert_eq!(json["range"], Value::String("7d".to_string()));
        assert_eq!(json["grain"], Value::String("6h".to_string()));
        assert_eq!(json["metric"], Value::String("apiEquivalent".to_string()));
        assert_eq!(json["apiLongContext"], Value::Bool(true));
        assert!(json["value"].is_string());
        assert!(json["metrics"]["estimatedCostUnits"].is_string());
        assert_eq!(json["valueIsLowerBound"], Value::Bool(true));
        assert!(json.get("partial_reasons").is_none());
        assert!(json.get("partialReasons").is_some());
        assert!(json["metrics"].get("estimatedCostUnits").is_some());
        assert_eq!(json["coverage"]["state"], "missing");
    }

    #[test]
    fn report_json_preserves_exact_values_above_javascript_integer_range() {
        let now = at(30, 10, 7);
        let mut report = build_summary_report_with_local_time(
            &snapshot(now),
            &HistoryData::default(),
            SummaryReportQuery::new(
                SummaryRange::SevenDays,
                SummaryGrain::Hour,
                SummaryMetric::Estimated,
                false,
                now,
            ),
            |timestamp| timestamp.naive_utc(),
        );
        let exact = u128::from(9_007_199_254_740_993_u64);
        report.value = exact;
        report.metrics.estimated_cost_units = exact;
        report.metrics.api_long_context_extra_cost_units = exact + 1;
        report.metrics.estimated_with_api_long_context_cost_units = exact + 2;

        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["value"], exact.to_string());
        assert_eq!(value["metrics"]["estimatedCostUnits"], exact.to_string());
        assert_eq!(
            serde_json::from_value::<SummaryReport>(value).unwrap(),
            report
        );
    }

    #[test]
    fn fixed_offset_mapper_is_applied_per_timestamp() {
        let offset = FixedOffset::east_opt(8 * 60 * 60).unwrap();
        let now = at(30, 10, 7);
        let history = HistoryData {
            half_hour_buckets: vec![bucket(
                at(30, 0, 15),
                10,
                vec![project_group("root", "root", "turn", 10, 1, Some(0))],
            )],
            ..HistoryData::default()
        };
        let report = build_summary_report_with_local_time(
            &snapshot(now),
            &history,
            SummaryReportQuery::new(
                SummaryRange::SevenDays,
                SummaryGrain::Hour,
                SummaryMetric::Tokens,
                false,
                now,
            ),
            |timestamp| timestamp.with_timezone(&offset).naive_local(),
        );

        assert_eq!(report.projects[0].buckets[0].starts_at.hour(), 8);
    }

    #[test]
    fn fixture_provenance_import_remains_serializable() {
        // Keep the test fixture imports tied to the same public domain enums
        // used by Snapshot JSON as those evolve.
        assert_eq!(
            serde_json::to_value(Provenance::LocalExact).unwrap(),
            "local_exact"
        );
    }
}
