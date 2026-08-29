use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::api_cost::ApiCostAggregation;
use crate::api_cost::{API_PRICING_CATALOG_REVISION, ApiCostAccumulator};
use crate::atomic_file::replace_file;
use crate::attribution::{ESTIMATOR_REVISION, estimate_call_weight, is_spark_model};
use crate::domain::{ApiCostAmount, LimitBucket, Provenance, TaskRecord, TokenUsage, UsageCall};

pub const HISTORY_FORMAT_VERSION: u32 = 2;
pub const HISTORY_METRIC_REVISION: u32 = 4;
pub const HISTORY_ESTIMATOR_REVISION: u32 = ESTIMATOR_REVISION;
pub const HISTORY_PROJECT_BREAKDOWN_REVISION: u32 = 1;
pub const HISTORY_RETENTION_DAYS: i64 = 90;
pub const LOCAL_BUCKET_MINUTES: i64 = 15;

const APP_DIRECTORY: &str = "codex-usage-monit";
const HISTORY_DIRECTORY: &str = "history-v1";
const STATE_DIRECTORY_ENV: &str = "CODEX_USAGE_MONIT_STATE_DIR";
const LOCK_FILE: &str = "history.lock";
const SUMMARY_BACKFILL_MARKER_FILE: &str = "summary-backfill.json";
const SUMMARY_BACKFILL_MARKER_VERSION: u32 = 1;
const LEGACY_HISTORY_FORMAT_VERSION: u32 = 1;
const LOCAL_BUCKET_SECS: i64 = LOCAL_BUCKET_MINUTES * 60;
const WEEKLY_SAMPLE_SECS: i64 = 30 * 60;
const QUOTA_SAMPLE_SECS: i64 = 5 * 60;
const FIVE_HOURS_MINS: i64 = 300;
const WEEK_MINS: i64 = 10_080;
const RESET_DRIFT_SECS: i64 = 120;
const FULL_HISTORY_MERGE_SECS: i64 = 30 * 60;
const RECENT_BUCKET_OVERLAP_SECS: i64 = 60 * 60;
const HISTORY_READ_CACHE_TTL: StdDuration = StdDuration::from_secs(30);
const TEMP_FILE_ATTEMPTS: usize = 128;
const PROJECT_TITLE_MAX_CHARS: usize = 160;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPoint {
    pub observed_at: DateTime<Utc>,
    pub limit_id: String,
    pub duration_mins: i64,
    pub resets_at: DateTime<Utc>,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalUsageGroup {
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    /// Extra units for the optional API long-context projection. `None` means
    /// the historical observation predates dual-weight recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<u128>,
    pub call_count: u64,
    pub used_model_fallback: bool,
    pub used_token_breakdown_fallback: bool,
    #[serde(default)]
    pub used_long_context_pricing: bool,
    #[serde(default)]
    pub used_long_context_detection_fallback: bool,
}

/// One thread's own usage within a fixed-width local bucket.
///
/// Parent totals are intentionally not materialized here. Summary views build
/// subtree totals at query time so a subagent can never be counted both as its
/// own usage and as part of an already-aggregated parent sample.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProjectUsageGroup {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<u128>,
    #[serde(default)]
    pub api_equivalent_cost: ApiCostAmount,
    pub call_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// A fixed-width local usage bucket.
///
/// The type and serialized collection keep their original "half hour" names
/// so format-1 shards can preserve non-bucket history during migration.
pub struct LocalHalfHourBucket {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub sampled_at: DateTime<Utc>,
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<u128>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub long_context_usage_unknown: bool,
    pub estimator_revision: u32,
    /// Version of the per-project/thread breakdown. Zero means the shard
    /// predates project attribution, even when the bucket itself is a real
    /// observed zero.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub project_breakdown_revision: u32,
    /// API catalog used to materialize `project_groups[*].api_equivalent_cost`.
    /// Outdated amounts are treated as unpriced by Summary instead of mixing
    /// prices from different catalogs.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub api_pricing_catalog_revision: u32,
    pub call_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<LocalUsageGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_groups: Vec<LocalProjectUsageGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyLocalPoint {
    pub observed_at: DateTime<Utc>,
    pub resets_at: DateTime<Utc>,
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<u128>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub long_context_usage_unknown: bool,
    pub estimator_revision: u32,
    pub call_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryObservation {
    pub observed_at: DateTime<Utc>,
    pub quota_points: Vec<QuotaPoint>,
    pub half_hour_buckets: Vec<LocalHalfHourBucket>,
    pub weekly_local_points: Vec<WeeklyLocalPoint>,
}

impl HistoryObservation {
    pub fn from_sources(
        observed_at: DateTime<Utc>,
        calls: &[UsageCall],
        limits: &[LimitBucket],
        partial_reasons: &[String],
    ) -> Self {
        Self::from_sources_with_coverage(observed_at, calls, limits, partial_reasons, None)
    }

    pub fn from_sources_with_coverage(
        observed_at: DateTime<Utc>,
        calls: &[UsageCall],
        limits: &[LimitBucket],
        partial_reasons: &[String],
        local_coverage_starts_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self::from_sources_with_tasks_and_coverage(
            observed_at,
            calls,
            &[],
            limits,
            partial_reasons,
            local_coverage_starts_at,
        )
    }

    pub fn from_sources_with_tasks_and_coverage(
        observed_at: DateTime<Utc>,
        calls: &[UsageCall],
        tasks: &[TaskRecord],
        limits: &[LimitBucket],
        partial_reasons: &[String],
        local_coverage_starts_at: Option<DateTime<Utc>>,
    ) -> Self {
        let weekly_local_points = weekly_local_points_from_sources(
            observed_at,
            calls,
            limits,
            partial_reasons,
            local_coverage_starts_at,
        );
        Self {
            observed_at,
            quota_points: quota_points_from_limits(limits),
            half_hour_buckets: local_buckets_from_calls(
                observed_at,
                calls,
                tasks,
                partial_reasons,
                local_coverage_starts_at,
            ),
            weekly_local_points,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryData {
    pub quota_points: Vec<QuotaPoint>,
    pub half_hour_buckets: Vec<LocalHalfHourBucket>,
    pub weekly_local_points: Vec<WeeklyLocalPoint>,
    pub warnings: Vec<String>,
    pub read_only: bool,
    /// Most recent project-history reconstruction attempt for the current
    /// metric, estimator, pricing, and bucket revisions in this Codex-home
    /// namespace. Actual bucket coverage remains the source of truth.
    pub(crate) summary_backfill_attempted_at: Option<DateTime<Utc>>,
    pub(crate) summary_backfill_attempt_complete: Option<bool>,
}

impl HistoryData {
    pub fn remaining_series(&self, duration_mins: i64) -> Vec<QuotaPoint> {
        self.quota_points
            .iter()
            .filter(|point| point.duration_mins == duration_mins)
            .cloned()
            .collect()
    }

    pub fn half_hour_series(&self) -> &[LocalHalfHourBucket] {
        &self.half_hour_buckets
    }

    pub fn latest_weekly_reset(&self) -> Option<DateTime<Utc>> {
        self.quota_points
            .iter()
            .filter(|point| point.duration_mins == WEEK_MINS)
            .map(|point| (point.observed_at, point.resets_at))
            .chain(
                self.weekly_local_points
                    .iter()
                    .map(|point| (point.observed_at, point.resets_at)),
            )
            .max_by_key(|(observed_at, _)| *observed_at)
            .map(|(_, resets_at)| resets_at)
    }

    pub fn estimated_half_hour_series(
        &self,
        weekly_resets_at: DateTime<Utc>,
    ) -> Vec<HalfHourSeriesPoint> {
        self.estimated_half_hour_series_with_api_long_context(weekly_resets_at, false)
    }

    pub fn estimated_half_hour_series_with_api_long_context(
        &self,
        weekly_resets_at: DateTime<Utc>,
        api_long_context: bool,
    ) -> Vec<HalfHourSeriesPoint> {
        let cycle_starts_at = weekly_resets_at - Duration::minutes(WEEK_MINS);
        let boundary_crosses_bucket =
            cycle_starts_at.timestamp().rem_euclid(LOCAL_BUCKET_SECS) != 0;
        let cycle = self.weekly_cycle_buckets(weekly_resets_at);
        let estimate = self.estimate_context(weekly_resets_at, &cycle, api_long_context);
        cycle
            .into_iter()
            .map(|bucket| {
                let mut partial_reasons = bucket
                    .partial_reasons
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if boundary_crosses_bucket {
                    partial_reasons
                        .insert("reset_boundary_excludes_partial_local_buckets".to_string());
                }
                if !estimate.revisions_are_consistent {
                    partial_reasons.insert("estimator_revision_changed".to_string());
                }
                if api_long_context && !estimate.costs_are_available {
                    partial_reasons.insert("api_long_context_history_unavailable".to_string());
                }
                if api_long_context && bucket.long_context_usage_unknown {
                    partial_reasons.insert("long_context_usage_unknown".to_string());
                }
                let estimated_cost_units = selected_cost_units(
                    bucket.estimated_cost_units,
                    bucket.api_long_context_extra_cost_units,
                    api_long_context,
                );
                if estimated_cost_units.is_none() {
                    partial_reasons.insert("api_long_context_history_unavailable".to_string());
                }
                HalfHourSeriesPoint {
                    starts_at: bucket.starts_at,
                    ends_at: bucket.ends_at,
                    token_usage: bucket.token_usage,
                    estimated_cost_units: estimated_cost_units.unwrap_or_default(),
                    estimated_quota_percent: estimated_cost_units
                        .and_then(|units| estimate.percent_for(units)),
                    estimator_revision: bucket.estimator_revision,
                    partial_reasons: partial_reasons.into_iter().collect(),
                }
            })
            .collect()
    }

    pub fn weekly_cumulative_series(
        &self,
        weekly_resets_at: DateTime<Utc>,
    ) -> Vec<WeeklyCumulativePoint> {
        self.weekly_cumulative_series_with_api_long_context(weekly_resets_at, false)
    }

    pub fn weekly_cumulative_series_with_api_long_context(
        &self,
        weekly_resets_at: DateTime<Utc>,
        api_long_context: bool,
    ) -> Vec<WeeklyCumulativePoint> {
        if let Some(points) =
            self.recorded_weekly_cumulative_series(weekly_resets_at, api_long_context)
        {
            return points;
        }
        self.derived_weekly_cumulative_series(weekly_resets_at, api_long_context)
    }

    fn recorded_weekly_cumulative_series(
        &self,
        weekly_resets_at: DateTime<Utc>,
        api_long_context: bool,
    ) -> Option<Vec<WeeklyCumulativePoint>> {
        let cycle_starts_at = weekly_resets_at - Duration::minutes(WEEK_MINS);
        let mut recorded = self
            .weekly_local_points
            .iter()
            .filter(|point| {
                (point.resets_at - weekly_resets_at).num_seconds().abs() <= RESET_DRIFT_SECS
                    && point.observed_at >= cycle_starts_at
                    && point.observed_at < weekly_resets_at
            })
            .collect::<Vec<_>>();
        recorded.sort_by_key(|point| point.observed_at);
        if recorded.is_empty() {
            return None;
        }
        let revisions = recorded
            .iter()
            .map(|point| point.estimator_revision)
            .collect::<BTreeSet<_>>();
        let revisions_are_consistent = revisions.len() <= 1;
        let estimator_revision = revisions.iter().next().copied().unwrap_or_default();
        let latest = recorded
            .iter()
            .max_by_key(|point| point.observed_at)
            .copied()
            .expect("recorded weekly points are non-empty");
        let used_percent = self.latest_weekly_used_percent(weekly_resets_at);
        let latest_cost_units = selected_cost_units(
            latest.estimated_cost_units,
            latest.api_long_context_extra_cost_units,
            api_long_context,
        );
        let estimate_percent = |cost_units: Option<u128>| {
            let (Some(cost_units), Some(latest_cost_units)) = (cost_units, latest_cost_units)
            else {
                return None;
            };
            if !revisions_are_consistent || latest_cost_units == 0 {
                return None;
            }
            used_percent.map(|used| used * cost_units as f64 / latest_cost_units as f64)
        };
        let mut initial_reasons = recorded
            .first()
            .into_iter()
            .flat_map(|point| point.partial_reasons.iter().cloned())
            .collect::<BTreeSet<_>>();
        if !revisions_are_consistent {
            initial_reasons.insert("estimator_revision_changed".to_string());
        }
        if api_long_context
            && recorded
                .iter()
                .any(|point| point.api_long_context_extra_cost_units.is_none())
        {
            initial_reasons.insert("api_long_context_history_unavailable".to_string());
        }
        let mut points = vec![WeeklyCumulativePoint {
            at: cycle_starts_at,
            sampled_at: None,
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            estimated_quota_percent: estimate_percent(Some(0)),
            estimator_revision,
            partial_reasons: initial_reasons.into_iter().collect(),
        }];
        points.extend(recorded.into_iter().map(|point| {
            let mut partial_reasons = point
                .partial_reasons
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if !revisions_are_consistent {
                partial_reasons.insert("estimator_revision_changed".to_string());
            }
            if api_long_context && point.long_context_usage_unknown {
                partial_reasons.insert("long_context_usage_unknown".to_string());
            }
            let estimated_cost_units = selected_cost_units(
                point.estimated_cost_units,
                point.api_long_context_extra_cost_units,
                api_long_context,
            );
            if estimated_cost_units.is_none() {
                partial_reasons.insert("api_long_context_history_unavailable".to_string());
            }
            WeeklyCumulativePoint {
                at: point.observed_at.min(weekly_resets_at),
                sampled_at: Some(point.observed_at),
                token_usage: point.token_usage,
                estimated_cost_units: estimated_cost_units.unwrap_or_default(),
                estimated_quota_percent: estimate_percent(estimated_cost_units),
                estimator_revision,
                partial_reasons: partial_reasons.into_iter().collect(),
            }
        }));
        let mut projected = points
            .into_iter()
            .map(|point| (point.at, point))
            .collect::<BTreeMap<_, _>>();
        let mut buckets = self.weekly_cycle_buckets(weekly_resets_at);
        buckets.sort_by_key(|bucket| bucket.starts_at);
        for bucket in buckets {
            if !confirmed_zero_bucket(bucket) || projected.contains_key(&bucket.ends_at) {
                continue;
            }
            let Some(anchor) = projected
                .range(..=bucket.ends_at)
                .next_back()
                .map(|(_, point)| point.clone())
            else {
                continue;
            };
            if anchor.at < bucket.starts_at {
                continue;
            }
            projected.insert(
                bucket.ends_at,
                WeeklyCumulativePoint {
                    at: bucket.ends_at,
                    sampled_at: Some(bucket.sampled_at),
                    ..anchor
                },
            );
        }
        Some(projected.into_values().collect())
    }

    fn derived_weekly_cumulative_series(
        &self,
        weekly_resets_at: DateTime<Utc>,
        api_long_context: bool,
    ) -> Vec<WeeklyCumulativePoint> {
        let cycle_starts_at = weekly_resets_at - Duration::minutes(WEEK_MINS);
        let buckets = self.weekly_cycle_buckets(weekly_resets_at);
        let estimate = self.estimate_context(weekly_resets_at, &buckets, api_long_context);
        let boundary_crosses_bucket =
            cycle_starts_at.timestamp().rem_euclid(LOCAL_BUCKET_SECS) != 0;
        let mut token_usage = TokenUsage::default();
        let mut estimated_cost_units = 0_u128;
        let mut partial_reasons = BTreeSet::new();
        if boundary_crosses_bucket {
            partial_reasons.insert("reset_boundary_excludes_partial_local_buckets".to_string());
        }
        if !estimate.revisions_are_consistent {
            partial_reasons.insert("estimator_revision_changed".to_string());
        }
        if api_long_context && !estimate.costs_are_available {
            partial_reasons.insert("api_long_context_history_unavailable".to_string());
        }

        let mut points = vec![WeeklyCumulativePoint {
            at: cycle_starts_at,
            sampled_at: None,
            token_usage,
            estimated_cost_units,
            estimated_quota_percent: estimate.percent_for(0),
            estimator_revision: estimate.estimator_revision,
            partial_reasons: partial_reasons.iter().cloned().collect(),
        }];
        for bucket in buckets {
            token_usage.add_assign(bucket.token_usage);
            let bucket_cost_units = selected_cost_units(
                bucket.estimated_cost_units,
                bucket.api_long_context_extra_cost_units,
                api_long_context,
            );
            if let Some(bucket_cost_units) = bucket_cost_units {
                estimated_cost_units = estimated_cost_units.saturating_add(bucket_cost_units);
            } else {
                partial_reasons.insert("api_long_context_history_unavailable".to_string());
            }
            if api_long_context && bucket.long_context_usage_unknown {
                partial_reasons.insert("long_context_usage_unknown".to_string());
            }
            partial_reasons.extend(bucket.partial_reasons.iter().cloned());
            points.push(WeeklyCumulativePoint {
                at: bucket.sampled_at.min(bucket.ends_at).min(weekly_resets_at),
                sampled_at: Some(bucket.sampled_at),
                token_usage,
                estimated_cost_units,
                estimated_quota_percent: bucket_cost_units
                    .and_then(|_| estimate.percent_for(estimated_cost_units)),
                estimator_revision: estimate.estimator_revision,
                partial_reasons: partial_reasons.iter().cloned().collect(),
            });
        }
        points
    }

    fn latest_weekly_used_percent(&self, weekly_resets_at: DateTime<Utc>) -> Option<f64> {
        self.quota_points
            .iter()
            .filter(|point| {
                point.duration_mins == WEEK_MINS
                    && (point.resets_at - weekly_resets_at).num_seconds().abs() <= RESET_DRIFT_SECS
            })
            .max_by_key(|point| point.observed_at)
            .map(|point| point.used_percent)
    }

    fn weekly_cycle_buckets(&self, weekly_resets_at: DateTime<Utc>) -> Vec<&LocalHalfHourBucket> {
        let starts_at = weekly_resets_at - Duration::minutes(WEEK_MINS);
        self.half_hour_buckets
            .iter()
            // A local aggregate cannot be split accurately after the fact. Keep
            // only buckets wholly inside the quota cycle so a non-aligned reset
            // never imports usage from the preceding or following cycle.
            .filter(|bucket| {
                is_current_local_bucket(bucket)
                    && bucket.starts_at >= starts_at
                    && bucket.ends_at <= weekly_resets_at
            })
            .collect()
    }

    fn estimate_context(
        &self,
        weekly_resets_at: DateTime<Utc>,
        buckets: &[&LocalHalfHourBucket],
        api_long_context: bool,
    ) -> EstimateContext {
        let revisions = buckets
            .iter()
            .map(|bucket| bucket.estimator_revision)
            .collect::<BTreeSet<_>>();
        let revisions_are_consistent = revisions.len() <= 1;
        let estimator_revision = revisions.iter().next().copied().unwrap_or_default();
        let bucket_costs = buckets
            .iter()
            .map(|bucket| {
                selected_cost_units(
                    bucket.estimated_cost_units,
                    bucket.api_long_context_extra_cost_units,
                    api_long_context,
                )
            })
            .collect::<Vec<_>>();
        let bucket_costs_available = bucket_costs.iter().all(Option::is_some);
        let total_cost_units = bucket_costs
            .into_iter()
            .flatten()
            .fold(0_u128, u128::saturating_add);
        let weekly_point = self
            .weekly_local_points
            .iter()
            .filter(|point| {
                (point.resets_at - weekly_resets_at).num_seconds().abs() <= RESET_DRIFT_SECS
            })
            .max_by_key(|point| point.observed_at);
        let weekly_cost_units = weekly_point.and_then(|point| {
            selected_cost_units(
                point.estimated_cost_units,
                point.api_long_context_extra_cost_units,
                api_long_context,
            )
        });
        let costs_are_available = weekly_cost_units.is_some() || bucket_costs_available;
        let total_cost_units = weekly_cost_units.unwrap_or(total_cost_units);
        let revisions_are_consistent = revisions_are_consistent
            && weekly_point.is_none_or(|point| {
                revisions.is_empty() || revisions.contains(&point.estimator_revision)
            });
        let estimator_revision = weekly_point
            .map(|point| point.estimator_revision)
            .unwrap_or(estimator_revision);
        let used_percent = self.latest_weekly_used_percent(weekly_resets_at);
        EstimateContext {
            used_percent,
            total_cost_units,
            estimator_revision,
            revisions_are_consistent,
            costs_are_available,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HalfHourSeriesPoint {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    pub estimated_quota_percent: Option<f64>,
    pub estimator_revision: u32,
    pub partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WeeklyCumulativePoint {
    pub at: DateTime<Utc>,
    pub sampled_at: Option<DateTime<Utc>>,
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    pub estimated_quota_percent: Option<f64>,
    pub estimator_revision: u32,
    pub partial_reasons: Vec<String>,
}

struct EstimateContext {
    used_percent: Option<f64>,
    total_cost_units: u128,
    estimator_revision: u32,
    revisions_are_consistent: bool,
    costs_are_available: bool,
}

fn selected_cost_units(
    base_units: u128,
    api_long_context_extra_units: Option<u128>,
    api_long_context: bool,
) -> Option<u128> {
    if api_long_context {
        api_long_context_extra_units.map(|extra| base_units.saturating_add(extra))
    } else {
        Some(base_units)
    }
}

fn confirmed_zero_bucket(bucket: &LocalHalfHourBucket) -> bool {
    bucket.sampled_at == bucket.ends_at
        && bucket.token_usage.is_zero()
        && bucket.estimated_cost_units == 0
        && bucket.call_count == 0
        && bucket.partial_reasons.is_empty()
}

impl EstimateContext {
    fn percent_for(&self, cost_units: u128) -> Option<f64> {
        if !self.revisions_are_consistent || !self.costs_are_available || self.total_cost_units == 0
        {
            return None;
        }
        self.used_percent
            .map(|used_percent| used_percent * cost_units as f64 / self.total_cost_units as f64)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryWriteReport {
    pub shards_written: usize,
    pub shards_skipped: usize,
    pub shards_pruned: usize,
    pub warnings: Vec<String>,
    pub read_only: bool,
}

#[derive(Debug)]
pub struct HistoryStore {
    history_root: Option<PathBuf>,
    namespace: String,
    namespace_dir: Option<PathBuf>,
    redact_content: bool,
    read_only: bool,
    namespace_checked: bool,
    last_full_merge_at: Option<DateTime<Utc>>,
    cached_since: Option<DateTime<Utc>>,
    cached_data: Option<HistoryData>,
    cache_loaded_at: Option<Instant>,
    staged_observation: Option<HistoryObservation>,
    staged_force_full_merge: bool,
    last_staged_flush_attempt: Option<Instant>,
}

impl HistoryStore {
    pub fn discover(codex_home: &Path) -> Self {
        Self::discover_with_redaction(codex_home, false)
    }

    pub fn discover_with_redaction(codex_home: &Path, redact_content: bool) -> Self {
        Self::from_optional_root(default_history_root(), codex_home, redact_content)
    }

    pub fn new(history_root: PathBuf, codex_home: &Path) -> Self {
        Self::new_with_redaction(history_root, codex_home, false)
    }

    pub fn new_with_redaction(
        history_root: PathBuf,
        codex_home: &Path,
        redact_content: bool,
    ) -> Self {
        Self::from_optional_root(Some(history_root), codex_home, redact_content)
    }

    fn from_optional_root(
        history_root: Option<PathBuf>,
        codex_home: &Path,
        redact_content: bool,
    ) -> Self {
        let namespace = history_namespace_with_redaction(codex_home, redact_content);
        let namespace_dir = history_root.as_ref().map(|root| root.join(&namespace));
        Self {
            history_root,
            namespace,
            namespace_dir,
            redact_content,
            read_only: false,
            namespace_checked: false,
            last_full_merge_at: None,
            cached_since: None,
            cached_data: None,
            cache_loaded_at: None,
            staged_observation: None,
            staged_force_full_merge: false,
            last_staged_flush_attempt: None,
        }
    }

    pub fn history_root(&self) -> Option<&Path> {
        self.history_root.as_deref()
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn namespace_dir(&self) -> Option<&Path> {
        self.namespace_dir.as_deref()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn record(&mut self, observation: &HistoryObservation) -> io::Result<HistoryWriteReport> {
        let redacted_observation = self
            .redact_content
            .then(|| redacted_history_observation(observation));
        let observation = redacted_observation.as_ref().unwrap_or(observation);
        let full_merge = self.full_merge_due(observation.observed_at);
        self.record_with_merge_mode(observation, full_merge, full_merge)
    }

    fn full_merge_due(&self, observed_at: DateTime<Utc>) -> bool {
        self.last_full_merge_at.is_none_or(|last| {
            observed_at < last || observed_at - last >= Duration::seconds(FULL_HISTORY_MERGE_SECS)
        })
    }

    fn record_with_merge_mode(
        &mut self,
        observation: &HistoryObservation,
        full_merge: bool,
        include_all_observation_points: bool,
    ) -> io::Result<HistoryWriteReport> {
        let mut report = HistoryWriteReport::default();
        let Some(directory) = self.namespace_dir.as_deref() else {
            report
                .warnings
                .push("history state directory is unavailable".to_string());
            return Ok(report);
        };
        if self.read_only {
            report.read_only = true;
            return Ok(report);
        }

        create_private_directory(directory)?;
        let lock = open_lock_file(directory)?;
        fs2::FileExt::lock_exclusive(&lock)?;

        if !self.namespace_checked {
            let preflight = inspect_namespace(directory, &self.namespace);
            report.warnings.extend(preflight.warnings);
            if preflight.future_version {
                self.read_only = true;
                self.cached_data = None;
                report.read_only = true;
                return Ok(report);
            }
            self.namespace_checked = true;
        }

        let cutoff = observation.observed_at - Duration::days(HISTORY_RETENTION_DAYS);
        let additions = additions_by_day(observation, cutoff, include_all_observation_points);
        for (day, additions) in additions {
            let path = shard_path(directory, day);
            let (mut shard, migrated) = match read_shard(&path, &self.namespace, day) {
                ShardRead::Missing => (HistoryShard::new(self.namespace.clone(), day), false),
                ShardRead::Current { shard, migrated } => (shard, migrated),
                ShardRead::FutureFormat(version) => {
                    self.read_only = true;
                    self.cached_data = None;
                    report.read_only = true;
                    report.warnings.push(format!(
                        "{} uses future history format version {version}; writes are disabled",
                        path.display()
                    ));
                    return Ok(report);
                }
                ShardRead::FutureMetric(revision) => {
                    self.read_only = true;
                    self.cached_data = None;
                    report.read_only = true;
                    report.warnings.push(format!(
                        "{} uses future history metric revision {revision}; writes are disabled",
                        path.display()
                    ));
                    return Ok(report);
                }
                ShardRead::Corrupt(message) => {
                    report.shards_skipped += 1;
                    report.warnings.push(message);
                    continue;
                }
            };
            let mut changed = migrated | shard.retain_since(cutoff);
            for point in additions.quota_points {
                changed |= upsert_quota_point(&mut shard.quota_points, point);
            }
            for bucket in additions.half_hour_buckets {
                changed |= upsert_half_hour_bucket(&mut shard.half_hour_buckets, bucket);
            }
            for point in additions.weekly_local_points {
                changed |= upsert_weekly_local_point(&mut shard.weekly_local_points, point);
            }
            if !changed {
                report.shards_skipped += 1;
                continue;
            }
            shard.sort();
            if let Err(error) = write_shard_atomically(&path, &shard) {
                self.cached_data = None;
                return Err(error);
            }
            report.shards_written += 1;
        }

        if full_merge {
            report.shards_pruned =
                prune_old_shards(directory, cutoff.date_naive(), &mut report.warnings);
            self.last_full_merge_at = Some(observation.observed_at);
        }
        report.read_only = self.read_only;
        if report.warnings.is_empty() {
            self.merge_cached_observation(observation, include_all_observation_points);
        } else {
            self.cached_data = None;
        }
        Ok(report)
    }

    /// Stage a TUI observation for live display and a later batched write.
    ///
    /// Staging never acquires the on-disk history lock. Multiple observations
    /// are compacted with the same replacement rules used by shard writes so
    /// reset transitions, closed local buckets, and stronger external evidence
    /// retain their normal semantics.
    pub(crate) fn stage(&mut self, observation: &HistoryObservation) {
        let redacted_observation = self
            .redact_content
            .then(|| redacted_history_observation(observation));
        let observation = redacted_observation.as_ref().unwrap_or(observation);
        let pending_clock_rollback = self
            .staged_observation
            .as_ref()
            .is_some_and(|staged| observation.observed_at < staged.observed_at);
        self.staged_force_full_merge |=
            self.full_merge_due(observation.observed_at) || pending_clock_rollback;
        let full_merge = self.staged_force_full_merge;
        match self.staged_observation.as_mut() {
            Some(staged) => merge_history_observation(staged, observation, full_merge),
            None => {
                let mut staged = HistoryObservation {
                    observed_at: observation.observed_at,
                    ..HistoryObservation::default()
                };
                merge_history_observation(&mut staged, observation, full_merge);
                self.staged_observation = Some(staged);
            }
        }
    }

    /// Stage a complete lookback observation, preserving buckets outside the
    /// normal recent-writer overlap. This is reserved for explicit history
    /// reconstruction such as Summary's one-time 30-day backfill.
    pub(crate) fn stage_full_observation(&mut self, observation: &HistoryObservation) {
        let redacted_observation = self
            .redact_content
            .then(|| redacted_history_observation(observation));
        let observation = redacted_observation.as_ref().unwrap_or(observation);
        self.staged_force_full_merge = true;
        match self.staged_observation.as_mut() {
            Some(staged) => merge_history_observation(staged, observation, true),
            None => {
                let mut staged = HistoryObservation {
                    observed_at: observation.observed_at,
                    ..HistoryObservation::default()
                };
                merge_history_observation(&mut staged, observation, true);
                self.staged_observation = Some(staged);
            }
        }
    }

    /// Persist a namespace-scoped marker after a Summary backfill has been
    /// flushed. Complete and partial attempts share a retry cooldown so a
    /// deterministic parser ambiguity cannot trigger the same expensive scan
    /// on every launch.
    pub(crate) fn mark_summary_backfill_attempt(
        &mut self,
        completed_at: DateTime<Utc>,
        complete: bool,
    ) -> io::Result<()> {
        let Some(directory) = self.namespace_dir.as_deref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "history state directory is unavailable",
            ));
        };
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "history store is read-only",
            ));
        }
        if self.staged_observation.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history backfill is not fully persisted",
            ));
        }
        let marker = SummaryBackfillMarker::current(completed_at, complete);
        let mut contents = serde_json::to_vec_pretty(&marker)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        contents.push(b'\n');
        write_private_atomically(&directory.join(SUMMARY_BACKFILL_MARKER_FILE), &contents)?;
        if let Some(data) = self.cached_data.as_mut() {
            data.summary_backfill_attempted_at = Some(completed_at);
            data.summary_backfill_attempt_complete = Some(complete);
        }
        Ok(())
    }

    /// Flush a staged observation regardless of the normal batching interval.
    ///
    /// A failed, read-only, or warning-bearing write keeps the staged data so
    /// the live view does not regress and a later attempt can recover it.
    pub(crate) fn flush_staged(&mut self) -> io::Result<Option<HistoryWriteReport>> {
        self.flush_staged_at(Instant::now())
    }

    /// Flush staged data once the monotonic batching interval has elapsed.
    pub(crate) fn flush_staged_if_due(
        &mut self,
        interval: StdDuration,
    ) -> io::Result<Option<HistoryWriteReport>> {
        self.flush_staged_if_due_at(interval, Instant::now())
    }

    fn flush_staged_if_due_at(
        &mut self,
        interval: StdDuration,
        now: Instant,
    ) -> io::Result<Option<HistoryWriteReport>> {
        if self.staged_observation.is_none()
            || self
                .last_staged_flush_attempt
                .is_some_and(|last| now.saturating_duration_since(last) < interval)
        {
            return Ok(None);
        }
        self.flush_staged_at(now)
    }

    fn flush_staged_at(&mut self, attempted_at: Instant) -> io::Result<Option<HistoryWriteReport>> {
        let Some(observation) = self.staged_observation.clone() else {
            return Ok(None);
        };
        self.last_staged_flush_attempt = Some(attempted_at);
        // The staged observation has already been reduced to the desired
        // recent/full delta. Persist every staged point so a bucket cannot age
        // past the overlap boundary while waiting for the batch deadline.
        let result = self.record_with_merge_mode(&observation, self.staged_force_full_merge, true);
        if let Ok(report) = &result
            && !report.read_only
            && report.warnings.is_empty()
        {
            self.staged_observation = None;
            self.staged_force_full_merge = false;
        }
        result.map(Some)
    }

    /// Load persisted history and overlay any not-yet-flushed TUI observation.
    pub(crate) fn load_since_with_staged(&mut self, since: DateTime<Utc>) -> HistoryData {
        let mut data = self.load_since(since);
        if let Some(observation) = self.staged_observation.as_ref() {
            merge_observation_into_history(&mut data, observation, since);
        }
        data
    }

    /// Reload external writer changes when stale, then restore the staged view.
    pub(crate) fn reload_since_if_stale_with_staged(
        &mut self,
        since: DateTime<Utc>,
    ) -> Option<HistoryData> {
        let mut data = self.reload_since_if_stale(since)?;
        if let Some(observation) = self.staged_observation.as_ref() {
            merge_observation_into_history(&mut data, observation, since);
        }
        Some(data)
    }

    pub fn load_since(&mut self, since: DateTime<Utc>) -> HistoryData {
        if self.cached_since == Some(since)
            && self
                .cache_loaded_at
                .is_some_and(|loaded| loaded.elapsed() < HISTORY_READ_CACHE_TTL)
            && let Some(data) = self.cached_data.as_ref()
        {
            let mut data = data.clone();
            data.read_only = self.read_only;
            return data;
        }
        let mut data = HistoryData::default();
        let Some(directory) = self.namespace_dir.as_deref() else {
            data.warnings
                .push("history state directory is unavailable".to_string());
            return data;
        };
        if !directory.is_dir() {
            return data;
        }
        let lock = match open_lock_file(directory) {
            Ok(lock) => lock,
            Err(error) => {
                data.warnings.push(format!(
                    "could not open history lock in {}: {error}",
                    directory.display()
                ));
                return data;
            }
        };
        if let Err(error) = fs2::FileExt::lock_shared(&lock) {
            data.warnings.push(format!(
                "could not lock history in {}: {error}",
                directory.display()
            ));
            return data;
        }

        let entries = match shard_entries(directory) {
            Ok(entries) => entries,
            Err(error) => {
                data.warnings.push(format!(
                    "could not list history in {}: {error}",
                    directory.display()
                ));
                return data;
            }
        };
        for (day, path) in entries {
            if day < since.date_naive() - Duration::days(1) {
                continue;
            }
            match read_shard(&path, &self.namespace, day) {
                ShardRead::Current { shard, .. } => {
                    data.quota_points.extend(
                        shard
                            .quota_points
                            .into_iter()
                            .filter(|point| point.observed_at >= since),
                    );
                    data.half_hour_buckets.extend(
                        shard
                            .half_hour_buckets
                            .into_iter()
                            .filter(|bucket| bucket.ends_at > since),
                    );
                    data.weekly_local_points.extend(
                        shard
                            .weekly_local_points
                            .into_iter()
                            .filter(|point| point.observed_at >= since),
                    );
                }
                ShardRead::FutureFormat(version) => {
                    self.read_only = true;
                    data.warnings.push(format!(
                        "{} uses future history format version {version}; writes are disabled",
                        path.display()
                    ));
                }
                ShardRead::FutureMetric(revision) => {
                    self.read_only = true;
                    data.warnings.push(format!(
                        "{} uses future history metric revision {revision}; writes are disabled",
                        path.display()
                    ));
                }
                ShardRead::Corrupt(message) => data.warnings.push(message),
                ShardRead::Missing => {}
            }
        }
        normalize_loaded_data(&mut data);
        if self.redact_content {
            redact_history_data_titles(&mut data);
        }
        data.read_only = self.read_only;
        if let Some(marker) = read_current_summary_backfill_marker(directory) {
            data.summary_backfill_attempted_at = Some(marker.completed_at);
            data.summary_backfill_attempt_complete = Some(marker.complete);
        }
        self.cached_since = Some(since);
        self.cached_data = Some(data.clone());
        self.cache_loaded_at = Some(Instant::now());
        data
    }

    pub fn reload_since_if_stale(&mut self, since: DateTime<Utc>) -> Option<HistoryData> {
        let cache_is_fresh = self.cached_since == Some(since)
            && self
                .cache_loaded_at
                .is_some_and(|loaded| loaded.elapsed() < HISTORY_READ_CACHE_TTL)
            && self.cached_data.is_some();
        (!cache_is_fresh).then(|| self.load_since(since))
    }

    fn merge_cached_observation(
        &mut self,
        observation: &HistoryObservation,
        include_all_observation_points: bool,
    ) {
        let (Some(since), Some(data)) = (self.cached_since, self.cached_data.as_mut()) else {
            return;
        };
        for point in &observation.quota_points {
            if point.observed_at >= since {
                let _ = upsert_quota_point(&mut data.quota_points, point.clone());
            }
        }
        let recent_cutoff = observation.observed_at - Duration::seconds(RECENT_BUCKET_OVERLAP_SECS);
        for bucket in &observation.half_hour_buckets {
            if bucket.ends_at > since
                && (include_all_observation_points || bucket.ends_at > recent_cutoff)
            {
                let _ = upsert_half_hour_bucket(&mut data.half_hour_buckets, bucket.clone());
            }
        }
        for point in &observation.weekly_local_points {
            if point.observed_at >= since
                && (include_all_observation_points || point.observed_at > recent_cutoff)
            {
                let _ = upsert_weekly_local_point(&mut data.weekly_local_points, point.clone());
            }
        }
        data.quota_points.retain(|point| point.observed_at >= since);
        data.half_hour_buckets
            .retain(|bucket| bucket.ends_at > since);
        data.weekly_local_points
            .retain(|point| point.observed_at >= since);
        sort_history_data(data);
        data.read_only = self.read_only;
    }
}

fn merge_history_observation(
    staged: &mut HistoryObservation,
    incoming: &HistoryObservation,
    full_merge: bool,
) {
    staged.observed_at = staged.observed_at.max(incoming.observed_at);
    for point in &incoming.quota_points {
        let _ = upsert_quota_point(&mut staged.quota_points, point.clone());
    }
    let recent_cutoff = incoming.observed_at - Duration::seconds(RECENT_BUCKET_OVERLAP_SECS);
    for bucket in &incoming.half_hour_buckets {
        if full_merge || bucket.ends_at > recent_cutoff {
            let _ = upsert_half_hour_bucket(&mut staged.half_hour_buckets, bucket.clone());
        }
    }
    for point in &incoming.weekly_local_points {
        if full_merge || point.observed_at > recent_cutoff {
            let _ = upsert_weekly_local_point(&mut staged.weekly_local_points, point.clone());
        }
    }

    let cutoff = staged.observed_at - Duration::days(HISTORY_RETENTION_DAYS);
    staged
        .quota_points
        .retain(|point| point.observed_at >= cutoff);
    staged
        .half_hour_buckets
        .retain(|bucket| bucket.ends_at > cutoff);
    staged
        .weekly_local_points
        .retain(|point| point.observed_at >= cutoff);
    staged.quota_points.sort_by(|left, right| {
        left.observed_at
            .cmp(&right.observed_at)
            .then_with(|| left.duration_mins.cmp(&right.duration_mins))
            .then_with(|| left.resets_at.cmp(&right.resets_at))
    });
    staged
        .half_hour_buckets
        .sort_by_key(|bucket| bucket.starts_at);
    staged
        .weekly_local_points
        .sort_by_key(|point| point.observed_at);
}

fn redacted_history_observation(observation: &HistoryObservation) -> HistoryObservation {
    let mut observation = observation.clone();
    for bucket in &mut observation.half_hour_buckets {
        redact_project_group_titles(&mut bucket.project_groups);
    }
    observation
}

fn redact_history_data_titles(data: &mut HistoryData) {
    for bucket in &mut data.half_hour_buckets {
        redact_project_group_titles(&mut bucket.project_groups);
    }
}

fn redact_project_group_titles(groups: &mut [LocalProjectUsageGroup]) {
    for group in groups {
        if group.title.is_some() {
            group.title = Some("[redacted]".to_string());
        }
    }
}

fn merge_observation_into_history(
    data: &mut HistoryData,
    observation: &HistoryObservation,
    since: DateTime<Utc>,
) {
    for point in &observation.quota_points {
        if point.observed_at >= since {
            let _ = upsert_quota_point(&mut data.quota_points, point.clone());
        }
    }
    for bucket in &observation.half_hour_buckets {
        if bucket.ends_at > since {
            let _ = upsert_half_hour_bucket(&mut data.half_hour_buckets, bucket.clone());
        }
    }
    for point in &observation.weekly_local_points {
        if point.observed_at >= since {
            let _ = upsert_weekly_local_point(&mut data.weekly_local_points, point.clone());
        }
    }
    data.quota_points.retain(|point| point.observed_at >= since);
    data.half_hour_buckets
        .retain(|bucket| bucket.ends_at > since);
    data.weekly_local_points
        .retain(|point| point.observed_at >= since);
    sort_history_data(data);
}

#[derive(Default)]
struct DayAdditions {
    quota_points: Vec<QuotaPoint>,
    half_hour_buckets: Vec<LocalHalfHourBucket>,
    weekly_local_points: Vec<WeeklyLocalPoint>,
}

fn additions_by_day(
    observation: &HistoryObservation,
    cutoff: DateTime<Utc>,
    include_all_observation_points: bool,
) -> BTreeMap<NaiveDate, DayAdditions> {
    let mut additions: BTreeMap<NaiveDate, DayAdditions> = BTreeMap::new();
    for point in &observation.quota_points {
        if point.observed_at >= cutoff {
            additions
                .entry(point.observed_at.date_naive())
                .or_default()
                .quota_points
                .push(point.clone());
        }
    }
    let recent_cutoff = observation.observed_at - Duration::seconds(RECENT_BUCKET_OVERLAP_SECS);
    for bucket in &observation.half_hour_buckets {
        if bucket.ends_at > cutoff
            && (include_all_observation_points || bucket.ends_at > recent_cutoff)
        {
            additions
                .entry(bucket.starts_at.date_naive())
                .or_default()
                .half_hour_buckets
                .push(bucket.clone());
        }
    }
    for point in &observation.weekly_local_points {
        if point.observed_at >= cutoff
            && (include_all_observation_points || point.observed_at > recent_cutoff)
        {
            additions
                .entry(point.observed_at.date_naive())
                .or_default()
                .weekly_local_points
                .push(point.clone());
        }
    }
    additions
}

fn quota_points_from_limits(limits: &[LimitBucket]) -> Vec<QuotaPoint> {
    let mut selected: BTreeMap<i64, QuotaPoint> = BTreeMap::new();
    for bucket in limits {
        if bucket.provenance != Provenance::ServerSnapshot
            || !bucket.limit_id.trim().eq_ignore_ascii_case("codex")
        {
            continue;
        }
        for window in [bucket.primary.as_ref(), bucket.secondary.as_ref()]
            .into_iter()
            .flatten()
        {
            let Some(duration_mins) = window.window_duration_mins else {
                continue;
            };
            if !matches!(duration_mins, FIVE_HOURS_MINS | WEEK_MINS)
                || !window.used_percent.is_finite()
                || !window.remaining_percent.is_finite()
            {
                continue;
            }
            let Some(resets_at) = window.resets_at else {
                continue;
            };
            let starts_at = resets_at - Duration::minutes(duration_mins);
            if bucket.as_of < starts_at - Duration::seconds(RESET_DRIFT_SECS)
                || bucket.as_of >= resets_at
            {
                continue;
            }
            let point = QuotaPoint {
                observed_at: bucket.as_of,
                limit_id: bucket.limit_id.clone(),
                duration_mins,
                resets_at,
                used_percent: window.used_percent.clamp(0.0, 100.0),
                remaining_percent: window.remaining_percent.clamp(0.0, 100.0),
                provenance: bucket.provenance,
            };
            let replace = selected
                .get(&duration_mins)
                .is_none_or(|current| point.observed_at > current.observed_at);
            if replace {
                selected.insert(duration_mins, point);
            }
        }
    }
    selected.into_values().collect()
}

fn weekly_local_points_from_sources(
    observed_at: DateTime<Utc>,
    calls: &[UsageCall],
    limits: &[LimitBucket],
    partial_reasons: &[String],
    local_coverage_starts_at: Option<DateTime<Utc>>,
) -> Vec<WeeklyLocalPoint> {
    let mut selected = None;
    for bucket in limits {
        if !bucket.limit_id.trim().eq_ignore_ascii_case("codex")
            || !matches!(
                bucket.provenance,
                Provenance::ServerSnapshot | Provenance::Stale
            )
        {
            continue;
        }
        for window in [bucket.primary.as_ref(), bucket.secondary.as_ref()]
            .into_iter()
            .flatten()
        {
            if window.window_duration_mins != Some(WEEK_MINS) {
                continue;
            }
            let Some(resets_at) = window.resets_at else {
                continue;
            };
            let starts_at = resets_at - Duration::minutes(WEEK_MINS);
            if observed_at < starts_at - Duration::seconds(RESET_DRIFT_SECS)
                || observed_at >= resets_at
            {
                continue;
            }
            let candidate = (bucket.as_of, resets_at, bucket.provenance);
            if selected.is_none_or(
                |(selected_as_of, _, _): (DateTime<Utc>, DateTime<Utc>, Provenance)| {
                    candidate.0 > selected_as_of
                },
            ) {
                selected = Some(candidate);
            }
        }
    }
    let Some((_, resets_at, provenance)) = selected else {
        return Vec::new();
    };
    let starts_at = resets_at - Duration::minutes(WEEK_MINS);
    let mut buckets = BTreeMap::<DateTime<Utc>, WeeklyAccumulator>::new();
    let mut first_call_bucket = None;
    for call in calls {
        if call.timestamp < starts_at
            || call.timestamp > observed_at
            || call.timestamp >= resets_at
            || is_spark_model(call.model.as_deref())
        {
            continue;
        }
        let bucket_starts_at = floor_weekly_sample(call.timestamp);
        first_call_bucket = Some(
            first_call_bucket.map_or(bucket_starts_at, |first: DateTime<Utc>| {
                first.min(bucket_starts_at)
            }),
        );
        let bucket = buckets.entry(bucket_starts_at).or_default();
        let weight = estimate_call_weight(call);
        bucket.token_usage.add_assign(call.tokens);
        bucket.estimated_cost_units = bucket.estimated_cost_units.saturating_add(weight.units);
        bucket.api_long_context_extra_cost_units = bucket
            .api_long_context_extra_cost_units
            .saturating_add(weight.api_long_context_extra_units);
        bucket.long_context_usage_unknown |= weight.used_long_context_detection_fallback;
        bucket.call_count = bucket.call_count.saturating_add(1);
        if weight.used_model_fallback {
            bucket
                .partial_reasons
                .insert("unpriced_model_rate_fallback".to_string());
        }
        if weight.used_token_breakdown_fallback {
            bucket
                .partial_reasons
                .insert("token_breakdown_missing".to_string());
        }
    }

    let last_bucket = floor_weekly_sample(observed_at);
    let coverage_starts_within_cycle =
        local_coverage_starts_at.is_some_and(|coverage| coverage > starts_at);
    let coverage_starts_at = local_coverage_starts_at.map(|coverage| coverage.max(starts_at));
    let first_coverage_bucket = coverage_starts_at.map(floor_weekly_sample);
    let first_bucket = match (first_call_bucket, first_coverage_bucket) {
        (Some(call), Some(coverage)) => call.min(coverage),
        (Some(call), None) => call,
        (None, Some(coverage)) => coverage,
        (None, None) => last_bucket,
    };
    if let Some(first_coverage_bucket) = first_coverage_bucket {
        // A live recorder can prove zero usage only from the point at which it
        // began observing. Keep older call-backed samples, but do not fill the
        // unknown gap between those calls and prospective coverage with zeros.
        let mut bucket_starts_at = first_coverage_bucket;
        while bucket_starts_at <= last_bucket {
            let bucket = buckets.entry(bucket_starts_at).or_default();
            if bucket_starts_at == first_coverage_bucket
                && coverage_starts_within_cycle
                && coverage_starts_at.is_some_and(|coverage| coverage > first_coverage_bucket)
            {
                bucket
                    .partial_reasons
                    .insert("coverage_starts_within_local_bucket".to_string());
            }
            bucket_starts_at += Duration::seconds(WEEKLY_SAMPLE_SECS);
        }
    } else {
        buckets.entry(last_bucket).or_default();
    }

    let mut token_usage = TokenUsage::default();
    let mut estimated_cost_units = 0_u128;
    let mut api_long_context_extra_cost_units = 0_u128;
    let mut long_context_usage_unknown = false;
    let mut call_count = 0_u64;
    let mut reasons = partial_reasons.iter().cloned().collect::<BTreeSet<_>>();
    if coverage_starts_within_cycle {
        // A prospective recorder cannot prove the reset-to-coverage prefix was
        // zero. Every cumulative point in this window is therefore a lower
        // bound, including call-backed points before coverage and samples
        // emitted after an exactly aligned coverage boundary.
        reasons.insert("rollout_local_coverage_unverified".to_string());
    }
    if provenance != Provenance::ServerSnapshot {
        reasons.insert("weekly_window_stale".to_string());
    }
    buckets
        .into_iter()
        .filter(|(bucket_starts_at, _)| *bucket_starts_at >= first_bucket)
        .map(|(bucket_starts_at, bucket)| {
            token_usage.add_assign(bucket.token_usage);
            estimated_cost_units = estimated_cost_units.saturating_add(bucket.estimated_cost_units);
            api_long_context_extra_cost_units = api_long_context_extra_cost_units
                .saturating_add(bucket.api_long_context_extra_cost_units);
            long_context_usage_unknown |= bucket.long_context_usage_unknown;
            call_count = call_count.saturating_add(bucket.call_count);
            reasons.extend(bucket.partial_reasons);
            WeeklyLocalPoint {
                observed_at: (bucket_starts_at + Duration::seconds(WEEKLY_SAMPLE_SECS))
                    .min(observed_at),
                resets_at,
                token_usage,
                estimated_cost_units,
                api_long_context_extra_cost_units: Some(api_long_context_extra_cost_units),
                long_context_usage_unknown,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                call_count,
                partial_reasons: reasons.iter().cloned().collect(),
            }
        })
        .collect()
}

#[derive(Default)]
struct WeeklyAccumulator {
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: u128,
    long_context_usage_unknown: bool,
    call_count: u64,
    partial_reasons: BTreeSet<String>,
}

#[derive(Default)]
struct BucketAccumulator {
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: u128,
    long_context_usage_unknown: bool,
    call_count: u64,
    groups: BTreeMap<(Option<String>, Option<String>), LocalUsageGroup>,
    project_groups: BTreeMap<String, LocalProjectUsageGroup>,
    api_cost_by_thread: BTreeMap<String, ApiCostAccumulator>,
    partial_reasons: BTreeSet<String>,
}

fn local_buckets_from_calls(
    observed_at: DateTime<Utc>,
    calls: &[UsageCall],
    tasks: &[TaskRecord],
    partial_reasons: &[String],
    local_coverage_starts_at: Option<DateTime<Utc>>,
) -> Vec<LocalHalfHourBucket> {
    let tasks_by_thread = tasks
        .iter()
        .map(|task| (task.thread_id.as_str(), task))
        .collect::<HashMap<_, _>>();
    let project_group_templates = project_group_templates(&tasks_by_thread);
    let mut buckets: BTreeMap<DateTime<Utc>, BucketAccumulator> = BTreeMap::new();
    for call in calls {
        if call.timestamp > observed_at {
            continue;
        }
        let starts_at = floor_local_bucket(call.timestamp);
        let bucket = buckets.entry(starts_at).or_default();
        bucket
            .api_cost_by_thread
            .entry(call.thread_id.clone())
            .or_default()
            .add_call(call);
        bucket
            .partial_reasons
            .extend(partial_reasons.iter().cloned());
        insert_project_lineage(
            &mut bucket.project_groups,
            &call.thread_id,
            &tasks_by_thread,
            &project_group_templates,
        );
        if is_spark_model(call.model.as_deref()) {
            let project_group = bucket
                .project_groups
                .entry(call.thread_id.clone())
                .or_insert_with(|| {
                    project_group_template(&call.thread_id, &project_group_templates)
                });
            project_group
                .api_long_context_extra_cost_units
                .get_or_insert(0);
            project_group.call_count = project_group.call_count.saturating_add(1);
            continue;
        }
        let weight = estimate_call_weight(call);
        bucket.token_usage.add_assign(call.tokens);
        bucket.estimated_cost_units = bucket.estimated_cost_units.saturating_add(weight.units);
        bucket.api_long_context_extra_cost_units = bucket
            .api_long_context_extra_cost_units
            .saturating_add(weight.api_long_context_extra_units);
        bucket.long_context_usage_unknown |= weight.used_long_context_detection_fallback;
        bucket.call_count = bucket.call_count.saturating_add(1);
        if weight.used_model_fallback {
            bucket
                .partial_reasons
                .insert("unpriced_model_rate_fallback".to_string());
        }
        if weight.used_token_breakdown_fallback {
            bucket
                .partial_reasons
                .insert("token_breakdown_missing".to_string());
        }

        let key = (
            normalized_optional(&call.model),
            normalized_optional(&call.service_tier),
        );
        let group = bucket
            .groups
            .entry(key.clone())
            .or_insert_with(|| LocalUsageGroup {
                model: key.0,
                service_tier: key.1,
                ..LocalUsageGroup::default()
            });
        group.token_usage.add_assign(call.tokens);
        group.estimated_cost_units = group.estimated_cost_units.saturating_add(weight.units);
        group.api_long_context_extra_cost_units = Some(
            group
                .api_long_context_extra_cost_units
                .unwrap_or_default()
                .saturating_add(weight.api_long_context_extra_units),
        );
        group.call_count = group.call_count.saturating_add(1);
        group.used_model_fallback |= weight.used_model_fallback;
        group.used_token_breakdown_fallback |= weight.used_token_breakdown_fallback;
        group.used_long_context_pricing |= weight.used_long_context_pricing;
        group.used_long_context_detection_fallback |= weight.used_long_context_detection_fallback;

        let project_group = bucket
            .project_groups
            .entry(call.thread_id.clone())
            .or_insert_with(|| project_group_template(&call.thread_id, &project_group_templates));
        project_group.token_usage.add_assign(call.tokens);
        project_group.estimated_cost_units = project_group
            .estimated_cost_units
            .saturating_add(weight.units);
        project_group.api_long_context_extra_cost_units = Some(
            project_group
                .api_long_context_extra_cost_units
                .unwrap_or_default()
                .saturating_add(weight.api_long_context_extra_units),
        );
        project_group.call_count = project_group.call_count.saturating_add(1);
    }

    if let Some(coverage_starts_at) =
        local_coverage_starts_at.filter(|starts_at| *starts_at <= observed_at)
    {
        // Keep concrete calls that predate prospective coverage, but label
        // those buckets as lower bounds. Mark only call-backed buckets here;
        // the synthesized leading coverage bucket retains the more precise
        // within-bucket edge reason below.
        for (starts_at, bucket) in &mut buckets {
            if *starts_at < coverage_starts_at {
                bucket
                    .partial_reasons
                    .insert("rollout_local_coverage_unverified".to_string());
            }
        }
        let first_bucket = floor_local_bucket(coverage_starts_at);
        let last_bucket = floor_local_bucket(observed_at);
        let mut starts_at = first_bucket;
        while starts_at <= last_bucket {
            let bucket = buckets.entry(starts_at).or_default();
            if starts_at == first_bucket && coverage_starts_at > first_bucket {
                bucket
                    .partial_reasons
                    .insert("coverage_starts_within_local_bucket".to_string());
            }
            starts_at += Duration::seconds(LOCAL_BUCKET_SECS);
        }
    }

    buckets
        .into_iter()
        .map(|(starts_at, bucket)| {
            let ends_at = starts_at + Duration::seconds(LOCAL_BUCKET_SECS);
            let project_groups = bucket
                .project_groups
                .into_values()
                .map(|mut group| {
                    group.api_equivalent_cost = bucket
                        .api_cost_by_thread
                        .get(&group.thread_id)
                        .map(ApiCostAccumulator::amount)
                        .unwrap_or_default();
                    group
                })
                .collect();
            LocalHalfHourBucket {
                starts_at,
                ends_at,
                // Closed buckets are stable across later full-lookback observations. The
                // open bucket retains the exact observation time so it can be replaced.
                sampled_at: observed_at.min(ends_at),
                token_usage: bucket.token_usage,
                estimated_cost_units: bucket.estimated_cost_units,
                api_long_context_extra_cost_units: Some(bucket.api_long_context_extra_cost_units),
                long_context_usage_unknown: bucket.long_context_usage_unknown,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
                api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
                call_count: bucket.call_count,
                groups: bucket.groups.into_values().collect(),
                project_groups,
                partial_reasons: bucket.partial_reasons.into_iter().collect(),
            }
        })
        .collect()
}

fn insert_project_lineage(
    groups: &mut BTreeMap<String, LocalProjectUsageGroup>,
    thread_id: &str,
    tasks_by_thread: &HashMap<&str, &TaskRecord>,
    templates: &HashMap<String, LocalProjectUsageGroup>,
) {
    let mut current = Some(thread_id);
    let mut visited = BTreeSet::new();
    while let Some(candidate_id) = current {
        if !visited.insert(candidate_id) {
            break;
        }
        if groups.contains_key(candidate_id) {
            break;
        }
        groups.insert(
            candidate_id.to_string(),
            project_group_template(candidate_id, templates),
        );
        current = tasks_by_thread
            .get(candidate_id)
            .and_then(|task| task.parent_thread_id.as_deref());
    }
}

fn project_group_templates(
    tasks_by_thread: &HashMap<&str, &TaskRecord>,
) -> HashMap<String, LocalProjectUsageGroup> {
    let mut project_descriptors = HashMap::<PathBuf, (String, String)>::new();
    tasks_by_thread
        .keys()
        .map(|thread_id| {
            (
                (*thread_id).to_string(),
                build_project_group_template(thread_id, tasks_by_thread, &mut project_descriptors),
            )
        })
        .collect()
}

fn project_group_template(
    thread_id: &str,
    templates: &HashMap<String, LocalProjectUsageGroup>,
) -> LocalProjectUsageGroup {
    templates
        .get(thread_id)
        .cloned()
        .unwrap_or_else(|| LocalProjectUsageGroup {
            thread_id: thread_id.to_string(),
            ..LocalProjectUsageGroup::default()
        })
}

fn build_project_group_template(
    thread_id: &str,
    tasks_by_thread: &HashMap<&str, &TaskRecord>,
    project_descriptors: &mut HashMap<PathBuf, (String, String)>,
) -> LocalProjectUsageGroup {
    let task = tasks_by_thread.get(thread_id).copied();
    let mut current = task;
    let mut visited = BTreeSet::new();
    let mut project_path = None;
    while let Some(candidate) = current {
        if !visited.insert(candidate.thread_id.as_str()) {
            break;
        }
        if candidate.cwd.is_some() {
            project_path = candidate.cwd.clone();
            break;
        }
        current = candidate
            .parent_thread_id
            .as_deref()
            .and_then(|parent| tasks_by_thread.get(parent).copied());
    }
    let project_descriptor = project_path.as_ref().map(|path| {
        project_descriptors
            .entry(path.clone())
            .or_insert_with(|| (project_id(path), project_label(path)))
            .clone()
    });
    LocalProjectUsageGroup {
        thread_id: thread_id.to_string(),
        parent_thread_id: task.and_then(|task| task.parent_thread_id.clone()),
        project_id: project_descriptor.as_ref().map(|(id, _)| id.clone()),
        project_label: project_descriptor.map(|(_, label)| label),
        title: task
            .map(|task| task.title.trim())
            .filter(|title| !title.is_empty())
            .map(|title| bounded_chars(title, PROJECT_TITLE_MAX_CHARS)),
        source: task
            .and_then(|task| task.source.as_deref())
            .map(str::trim)
            .filter(|source| !source.is_empty())
            .map(str::to_string),
        ..LocalProjectUsageGroup::default()
    }
}

fn bounded_chars(value: &str, maximum: usize) -> String {
    let mut characters = value.chars();
    let mut bounded = characters.by_ref().take(maximum).collect::<String>();
    if characters.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn project_id(path: &Path) -> String {
    let normalized = normalized_path(path);
    format!(
        "project-{:016x}",
        stable_hash(&history_namespace_bytes(&normalized))
    )
}

fn project_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn normalized_optional(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn floor_local_bucket(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    let seconds = timestamp.timestamp().div_euclid(LOCAL_BUCKET_SECS) * LOCAL_BUCKET_SECS;
    DateTime::from_timestamp(seconds, 0).expect("a valid DateTime has a valid local-bucket floor")
}

fn floor_weekly_sample(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    let seconds = timestamp.timestamp().div_euclid(WEEKLY_SAMPLE_SECS) * WEEKLY_SAMPLE_SECS;
    DateTime::from_timestamp(seconds, 0).expect("a valid DateTime has a valid weekly-sample floor")
}

fn is_exact_weekly_sample_boundary(timestamp: DateTime<Utc>) -> bool {
    timestamp.timestamp().rem_euclid(WEEKLY_SAMPLE_SECS) == 0
        && timestamp.timestamp_subsec_nanos() == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryShard {
    format_version: u32,
    metric_revision: u32,
    namespace: String,
    utc_day: NaiveDate,
    #[serde(default)]
    quota_points: Vec<QuotaPoint>,
    #[serde(default)]
    half_hour_buckets: Vec<LocalHalfHourBucket>,
    #[serde(default)]
    weekly_local_points: Vec<WeeklyLocalPoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummaryBackfillMarker {
    schema_version: u32,
    completed_at: DateTime<Utc>,
    #[serde(default)]
    complete: bool,
    history_metric_revision: u32,
    estimator_revision: u32,
    project_breakdown_revision: u32,
    api_pricing_catalog_revision: u32,
    bucket_minutes: i64,
}

impl SummaryBackfillMarker {
    fn current(completed_at: DateTime<Utc>, complete: bool) -> Self {
        Self {
            schema_version: SUMMARY_BACKFILL_MARKER_VERSION,
            completed_at,
            complete,
            history_metric_revision: HISTORY_METRIC_REVISION,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            bucket_minutes: LOCAL_BUCKET_MINUTES,
        }
    }

    fn revisions_are_current(&self) -> bool {
        self.schema_version == SUMMARY_BACKFILL_MARKER_VERSION
            && self.history_metric_revision == HISTORY_METRIC_REVISION
            && self.estimator_revision == HISTORY_ESTIMATOR_REVISION
            && self.project_breakdown_revision == HISTORY_PROJECT_BREAKDOWN_REVISION
            && self.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION
            && self.bucket_minutes == LOCAL_BUCKET_MINUTES
    }
}

fn read_current_summary_backfill_marker(directory: &Path) -> Option<SummaryBackfillMarker> {
    fs::read(directory.join(SUMMARY_BACKFILL_MARKER_FILE))
        .ok()
        .and_then(|contents| serde_json::from_slice::<SummaryBackfillMarker>(&contents).ok())
        .filter(SummaryBackfillMarker::revisions_are_current)
}

impl HistoryShard {
    fn new(namespace: String, utc_day: NaiveDate) -> Self {
        Self {
            format_version: HISTORY_FORMAT_VERSION,
            metric_revision: HISTORY_METRIC_REVISION,
            namespace,
            utc_day,
            quota_points: Vec::new(),
            half_hour_buckets: Vec::new(),
            weekly_local_points: Vec::new(),
        }
    }

    fn retain_since(&mut self, cutoff: DateTime<Utc>) -> bool {
        let quota_len = self.quota_points.len();
        let bucket_len = self.half_hour_buckets.len();
        let weekly_len = self.weekly_local_points.len();
        self.quota_points
            .retain(|point| point.observed_at >= cutoff);
        self.half_hour_buckets
            .retain(|bucket| bucket.ends_at > cutoff);
        self.weekly_local_points
            .retain(|point| point.observed_at >= cutoff);
        quota_len != self.quota_points.len()
            || bucket_len != self.half_hour_buckets.len()
            || weekly_len != self.weekly_local_points.len()
    }

    fn sort(&mut self) {
        self.quota_points.sort_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.duration_mins.cmp(&right.duration_mins))
                .then_with(|| left.resets_at.cmp(&right.resets_at))
        });
        self.half_hour_buckets
            .sort_by_key(|bucket| bucket.starts_at);
        self.weekly_local_points
            .sort_by_key(|point| point.observed_at);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionProbe {
    format_version: Option<u32>,
}

enum ShardRead {
    Missing,
    Current { shard: HistoryShard, migrated: bool },
    FutureFormat(u32),
    FutureMetric(u32),
    Corrupt(String),
}

fn read_shard(path: &Path, namespace: &str, day: NaiveDate) -> ShardRead {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ShardRead::Missing,
        Err(error) => {
            return ShardRead::Corrupt(format!(
                "could not read history shard {}: {error}",
                path.display()
            ));
        }
    };
    let probe = match serde_json::from_slice::<VersionProbe>(&contents) {
        Ok(probe) => probe,
        Err(error) => {
            return ShardRead::Corrupt(format!(
                "history shard {} is malformed: {error}",
                path.display()
            ));
        }
    };
    let Some(version) = probe.format_version else {
        return ShardRead::Corrupt(format!(
            "history shard {} is missing formatVersion",
            path.display()
        ));
    };
    if version > HISTORY_FORMAT_VERSION {
        return ShardRead::FutureFormat(version);
    }
    if !matches!(
        version,
        LEGACY_HISTORY_FORMAT_VERSION | HISTORY_FORMAT_VERSION
    ) {
        return ShardRead::Corrupt(format!(
            "history shard {} has unsupported format version {version}",
            path.display()
        ));
    }
    let mut shard = match serde_json::from_slice::<HistoryShard>(&contents) {
        Ok(shard) => shard,
        Err(error) => {
            return ShardRead::Corrupt(format!(
                "history shard {} could not be decoded: {error}",
                path.display()
            ));
        }
    };
    if shard.namespace != namespace {
        return ShardRead::Corrupt(format!(
            "history shard {} belongs to namespace {}, expected {namespace}",
            path.display(),
            shard.namespace
        ));
    }
    if shard.utc_day != day {
        return ShardRead::Corrupt(format!(
            "history shard {} declares {}, expected {day}",
            path.display(),
            shard.utc_day
        ));
    }
    if shard.metric_revision > HISTORY_METRIC_REVISION {
        return ShardRead::FutureMetric(shard.metric_revision);
    }

    let source_metric_revision = shard.metric_revision;
    let mut migrated = false;
    if version == LEGACY_HISTORY_FORMAT_VERSION || shard.metric_revision < 2 {
        // Revision 1 local buckets were 30-minute aggregates. They cannot be
        // split honestly, so let retained rollout events rebuild local buckets
        // at 15 minutes.
        shard.half_hour_buckets.clear();
        migrated = true;
    }
    if source_metric_revision < 4 {
        // Revisions 1-3 treated a complete filesystem walk as proof of zero
        // usage throughout the entire lookback. Once rollout files have been
        // deleted there is no way to distinguish those synthetic zeros from
        // genuinely observed prospective zeros. Revision 1 buckets were
        // already removed above; for revisions 2-3 preserve only buckets with
        // concrete call/project/usage evidence. Old weekly points are
        // cumulative, so an unchanged plateau cannot be distinguished from a
        // call-backed sample; discard them and let retained evidence rebuild.
        if source_metric_revision >= 2 {
            shard
                .half_hour_buckets
                .retain(local_bucket_has_call_evidence);
            for bucket in &mut shard.half_hour_buckets {
                if !bucket
                    .partial_reasons
                    .iter()
                    .any(|reason| reason == "rollout_local_coverage_unverified")
                {
                    bucket
                        .partial_reasons
                        .push("rollout_local_coverage_unverified".to_string());
                    bucket.partial_reasons.sort();
                }
            }
        }
        shard.weekly_local_points.clear();
        migrated = true;
    }
    if shard.metric_revision < HISTORY_METRIC_REVISION {
        // Estimator revision 4 was briefly used by development builds for a
        // single, always-on API long-context value. Its base and optional
        // components cannot be separated after the fact. Keep released
        // revision-3 base history, but let retained rollouts rebuild rev-4
        // development observations as revision 5 dual weights.
        shard
            .half_hour_buckets
            .retain(|bucket| bucket.estimator_revision != 4);
        shard
            .weekly_local_points
            .retain(|point| point.estimator_revision != 4);
        shard.format_version = HISTORY_FORMAT_VERSION;
        shard.metric_revision = HISTORY_METRIC_REVISION;
        migrated = true;
    }
    let bucket_count = shard.half_hour_buckets.len();
    shard.half_hour_buckets.retain(is_current_local_bucket);
    migrated |= bucket_count != shard.half_hour_buckets.len();

    ShardRead::Current { shard, migrated }
}

fn is_current_local_bucket(bucket: &LocalHalfHourBucket) -> bool {
    bucket.ends_at - bucket.starts_at == Duration::seconds(LOCAL_BUCKET_SECS)
        && floor_local_bucket(bucket.starts_at) == bucket.starts_at
        && bucket.sampled_at >= bucket.starts_at
        && bucket.sampled_at <= bucket.ends_at
}

fn local_bucket_has_call_evidence(bucket: &LocalHalfHourBucket) -> bool {
    bucket.call_count > 0
        || !bucket.token_usage.is_zero()
        || bucket.estimated_cost_units > 0
        || bucket
            .api_long_context_extra_cost_units
            .is_some_and(|units| units > 0)
        || !bucket.groups.is_empty()
        || !bucket.project_groups.is_empty()
}

fn upsert_quota_point(points: &mut Vec<QuotaPoint>, incoming: QuotaPoint) -> bool {
    let slot = incoming
        .observed_at
        .timestamp()
        .div_euclid(QUOTA_SAMPLE_SECS);
    let existing = points.iter().position(|point| {
        point.duration_mins == incoming.duration_mins
            && point.limit_id.eq_ignore_ascii_case(&incoming.limit_id)
            && point.observed_at.timestamp().div_euclid(QUOTA_SAMPLE_SECS) == slot
            && (point.resets_at - incoming.resets_at).num_seconds().abs() <= RESET_DRIFT_SECS
    });
    if let Some(index) = existing {
        if quota_point_payload_eq(&incoming, &points[index]) {
            return false;
        }
        let replace = incoming.observed_at > points[index].observed_at
            || (incoming.observed_at == points[index].observed_at
                && (incoming.used_percent, -incoming.remaining_percent)
                    > (points[index].used_percent, -points[index].remaining_percent));
        if replace {
            points[index] = incoming;
            true
        } else {
            false
        }
    } else {
        points.push(incoming);
        true
    }
}

fn upsert_half_hour_bucket(
    buckets: &mut Vec<LocalHalfHourBucket>,
    mut incoming: LocalHalfHourBucket,
) -> bool {
    if !is_current_local_bucket(&incoming) {
        return false;
    }
    let existing = buckets
        .iter()
        .position(|bucket| bucket.starts_at == incoming.starts_at);
    if let Some(index) = existing {
        if half_hour_bucket_payload_eq(&incoming, &buckets[index]) {
            let closes_open_bucket = buckets[index].sampled_at < buckets[index].ends_at
                && incoming.sampled_at == incoming.ends_at;
            if closes_open_bucket {
                buckets[index] = incoming;
                return true;
            }
            return false;
        }
        let existing_strictly_dominates = bucket_evidence_dominates(&buckets[index], &incoming)
            && !bucket_evidence_dominates(&incoming, &buckets[index]);
        if existing_strictly_dominates
            && incoming.sampled_at > buckets[index].sampled_at
            && has_coverage_issue(&incoming.partial_reasons)
            && (coverage_issue_state_differs(
                &incoming.partial_reasons,
                &buckets[index].partial_reasons,
            ) || incoming.sampled_at == incoming.ends_at)
        {
            // A restart can reconstruct fewer calls than a prior process while
            // still providing newer evidence that coverage was discontinuous.
            // Preserve the stronger counters, but advance closure and attach
            // the newer gap so the old exact sample cannot survive unqualified.
            let existing = &mut buckets[index];
            existing.sampled_at = incoming.sampled_at;
            merge_coverage_issues(&mut existing.partial_reasons, &incoming.partial_reasons);
            return true;
        }
        if project_breakdown_can_upgrade_without_replacing(&incoming, &buckets[index]) {
            let existing = &mut buckets[index];
            existing.sampled_at = existing.sampled_at.max(incoming.sampled_at);
            existing.project_breakdown_revision = incoming.project_breakdown_revision;
            existing.api_pricing_catalog_revision = incoming.api_pricing_catalog_revision;
            existing.project_groups = incoming.project_groups;
            // The aggregate counters can remain trustworthy while a rollout
            // scan only partially reconstructs the project/API split. Keep
            // that collection quality attached to the grafted breakdown so
            // Summary cannot present it as exact. A later complete enrichment
            // through this same branch clears the old collection issues.
            existing
                .partial_reasons
                .retain(|reason| !is_collection_issue_reason(reason));
            existing.partial_reasons.extend(
                incoming
                    .partial_reasons
                    .into_iter()
                    .filter(|reason| is_collection_issue_reason(reason)),
            );
            existing.partial_reasons.sort();
            existing.partial_reasons.dedup();
            return true;
        }
        if project_breakdown_can_upgrade_without_replacing(&buckets[index], &incoming) {
            // Keep the richer breakdown while still allowing fresher bucket
            // timing or stronger non-project evidence to advance normally.
            incoming.project_breakdown_revision = buckets[index].project_breakdown_revision;
            incoming.api_pricing_catalog_revision = buckets[index].api_pricing_catalog_revision;
            incoming.project_groups = buckets[index].project_groups.clone();
        }
        if should_replace_half_hour_bucket(&incoming, &buckets[index]) {
            buckets[index] = incoming;
            true
        } else {
            false
        }
    } else {
        buckets.push(incoming);
        true
    }
}

fn project_breakdown_can_upgrade_without_replacing(
    incoming: &LocalHalfHourBucket,
    existing: &LocalHalfHourBucket,
) -> bool {
    let newer_breakdown = incoming.project_breakdown_revision > existing.project_breakdown_revision
        || incoming.api_pricing_catalog_revision > existing.api_pricing_catalog_revision;
    let richer_breakdown = incoming.project_breakdown_revision
        == existing.project_breakdown_revision
        && incoming.api_pricing_catalog_revision == existing.api_pricing_catalog_revision
        && project_groups_are_strictly_richer(&incoming.project_groups, &existing.project_groups);
    (newer_breakdown || richer_breakdown)
        && incoming.project_breakdown_revision >= existing.project_breakdown_revision
        && incoming.api_pricing_catalog_revision >= existing.api_pricing_catalog_revision
        && incoming.estimator_revision == existing.estimator_revision
        && incoming.token_usage == existing.token_usage
        && incoming.estimated_cost_units == existing.estimated_cost_units
        && incoming.api_long_context_extra_cost_units == existing.api_long_context_extra_cost_units
        && incoming.long_context_usage_unknown == existing.long_context_usage_unknown
        && incoming.call_count == existing.call_count
}

/// Returns true only when the candidate retains every known thread and every
/// piece of metadata/API coverage while adding at least one piece of
/// information. This partial order lets a later full scan enrich a breakdown
/// grafted from a partial scan without allowing a sparse writer to undo it.
fn project_groups_are_strictly_richer(
    candidate: &[LocalProjectUsageGroup],
    existing: &[LocalProjectUsageGroup],
) -> bool {
    let candidate_by_thread = candidate
        .iter()
        .map(|group| (group.thread_id.as_str(), group))
        .collect::<HashMap<_, _>>();
    if candidate_by_thread.len() != candidate.len() {
        return false;
    }

    let mut strictly_richer = candidate.len() > existing.len();
    for existing_group in existing {
        let Some(candidate_group) = candidate_by_thread.get(existing_group.thread_id.as_str())
        else {
            return false;
        };
        let Some(group_is_richer) =
            project_group_information_dominates(candidate_group, existing_group)
        else {
            return false;
        };
        strictly_richer |= group_is_richer;
    }
    strictly_richer
}

/// `Some(strict)` means the candidate retains all information in `existing`;
/// `None` means it would regress or replace equally complete metadata with a
/// conflicting value.
fn project_group_information_dominates(
    candidate: &LocalProjectUsageGroup,
    existing: &LocalProjectUsageGroup,
) -> Option<bool> {
    let mut strictly_richer = false;
    for (candidate_value, existing_value) in [
        (&candidate.parent_thread_id, &existing.parent_thread_id),
        (&candidate.project_id, &existing.project_id),
        (&candidate.project_label, &existing.project_label),
        (&candidate.source, &existing.source),
    ] {
        match (
            informative_text(candidate_value),
            informative_text(existing_value),
        ) {
            (Some(candidate_value), Some(existing_value)) if candidate_value != existing_value => {
                return None;
            }
            (None, Some(_)) => return None,
            (Some(_), None) => strictly_richer = true,
            _ => {}
        }
    }
    match (
        informative_title(&candidate.title),
        informative_title(&existing.title),
    ) {
        (Some(candidate_value), Some(existing_value)) if candidate_value != existing_value => {
            return None;
        }
        (None, Some(_)) => return None,
        (Some(_), None) => strictly_richer = true,
        _ => {}
    }

    match (
        candidate.api_long_context_extra_cost_units,
        existing.api_long_context_extra_cost_units,
    ) {
        (None, Some(_)) => return None,
        (Some(_), None) => strictly_richer = true,
        _ => {}
    }

    let candidate_api = candidate.api_equivalent_cost;
    let existing_api = existing.api_equivalent_cost;
    for (candidate_value, existing_value) in [
        (
            candidate_api.observed_samples,
            existing_api.observed_samples,
        ),
        (candidate_api.priced_samples, existing_api.priced_samples),
        (candidate_api.observed_tokens, existing_api.observed_tokens),
        (candidate_api.priced_tokens, existing_api.priced_tokens),
    ] {
        if candidate_value < existing_value {
            return None;
        }
        strictly_richer |= candidate_value > existing_value;
    }
    let same_api_coverage = candidate_api.observed_samples == existing_api.observed_samples
        && candidate_api.priced_samples == existing_api.priced_samples
        && candidate_api.observed_tokens == existing_api.observed_tokens
        && candidate_api.priced_tokens == existing_api.priced_tokens;
    if same_api_coverage {
        if existing_api.range_is_exact() && !candidate_api.range_is_exact() {
            return None;
        }
        strictly_richer |= !existing_api.range_is_exact() && candidate_api.range_is_exact();
    }

    Some(strictly_richer)
}

fn informative_text(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.trim().is_empty())
}

fn informative_title(value: &Option<String>) -> Option<&str> {
    informative_text(value).filter(|value| *value != "Untitled task")
}

fn upsert_weekly_local_point(
    points: &mut Vec<WeeklyLocalPoint>,
    incoming: WeeklyLocalPoint,
) -> bool {
    let incoming_is_boundary = is_exact_weekly_sample_boundary(incoming.observed_at);
    let slot = incoming
        .observed_at
        .timestamp()
        .div_euclid(QUOTA_SAMPLE_SECS);
    let same_cycle = |point: &&WeeklyLocalPoint| {
        (point.resets_at - incoming.resets_at).num_seconds().abs() <= RESET_DRIFT_SECS
    };
    if let Some(index) = points.iter().position(|point| {
        same_cycle(&point)
            && point.observed_at.timestamp().div_euclid(QUOTA_SAMPLE_SECS) == slot
            && is_exact_weekly_sample_boundary(point.observed_at) == incoming_is_boundary
    }) {
        if weekly_local_point_payload_eq(&incoming, &points[index]) {
            return false;
        }
        let existing_strictly_dominates = weekly_evidence_dominates(&points[index], &incoming)
            && !weekly_evidence_dominates(&incoming, &points[index]);
        if existing_strictly_dominates
            && incoming.observed_at > points[index].observed_at
            && has_coverage_issue(&incoming.partial_reasons)
            && coverage_issue_state_differs(
                &incoming.partial_reasons,
                &points[index].partial_reasons,
            )
        {
            let existing = &mut points[index];
            existing.observed_at = incoming.observed_at;
            merge_coverage_issues(&mut existing.partial_reasons, &incoming.partial_reasons);
            return true;
        }
        if should_replace_weekly_local_point(&incoming, &points[index]) {
            points[index] = incoming;
            return true;
        }
        return false;
    }

    let latest = points
        .iter()
        .filter(same_cycle)
        .max_by_key(|point| point.observed_at);
    if let Some(latest) = latest {
        if weekly_local_point_payload_eq(&incoming, latest) {
            if incoming_is_boundary {
                points.push(incoming);
                return true;
            }
            return false;
        }
        let latest_strictly_dominates = weekly_evidence_dominates(latest, &incoming)
            && !weekly_evidence_dominates(&incoming, latest);
        if latest_strictly_dominates
            && incoming.observed_at > latest.observed_at
            && has_coverage_issue(&incoming.partial_reasons)
            && coverage_issue_state_differs(&incoming.partial_reasons, &latest.partial_reasons)
        {
            let mut merged = latest.clone();
            merged.observed_at = incoming.observed_at;
            merge_coverage_issues(&mut merged.partial_reasons, &incoming.partial_reasons);
            points.push(merged);
            return true;
        }
        if incoming.observed_at > latest.observed_at
            && weekly_point_can_suppress_later(latest, &incoming)
            && !(weekly_local_point_evidence_eq(latest, &incoming)
                && coverage_issue_state_differs(&latest.partial_reasons, &incoming.partial_reasons))
            && collection_issue_count(&latest.partial_reasons)
                <= collection_issue_count(&incoming.partial_reasons)
        {
            return false;
        }
    }
    points.push(incoming);
    true
}

fn quota_point_payload_eq(left: &QuotaPoint, right: &QuotaPoint) -> bool {
    left.limit_id.eq_ignore_ascii_case(&right.limit_id)
        && left.duration_mins == right.duration_mins
        && left.resets_at == right.resets_at
        && left.used_percent == right.used_percent
        && left.remaining_percent == right.remaining_percent
        && left.provenance == right.provenance
}

fn half_hour_bucket_payload_eq(left: &LocalHalfHourBucket, right: &LocalHalfHourBucket) -> bool {
    half_hour_bucket_evidence_eq(left, right) && left.partial_reasons == right.partial_reasons
}

fn half_hour_bucket_evidence_eq(left: &LocalHalfHourBucket, right: &LocalHalfHourBucket) -> bool {
    left.starts_at == right.starts_at
        && left.ends_at == right.ends_at
        && left.token_usage == right.token_usage
        && left.estimated_cost_units == right.estimated_cost_units
        && left.api_long_context_extra_cost_units == right.api_long_context_extra_cost_units
        && left.long_context_usage_unknown == right.long_context_usage_unknown
        && left.estimator_revision == right.estimator_revision
        && left.project_breakdown_revision == right.project_breakdown_revision
        && left.api_pricing_catalog_revision == right.api_pricing_catalog_revision
        && left.call_count == right.call_count
        && left.groups == right.groups
        && left.project_groups == right.project_groups
}

fn weekly_local_point_payload_eq(left: &WeeklyLocalPoint, right: &WeeklyLocalPoint) -> bool {
    weekly_local_point_evidence_eq(left, right) && left.partial_reasons == right.partial_reasons
}

fn weekly_local_point_evidence_eq(left: &WeeklyLocalPoint, right: &WeeklyLocalPoint) -> bool {
    left.resets_at == right.resets_at
        && left.token_usage == right.token_usage
        && left.estimated_cost_units == right.estimated_cost_units
        && left.api_long_context_extra_cost_units == right.api_long_context_extra_cost_units
        && left.long_context_usage_unknown == right.long_context_usage_unknown
        && left.estimator_revision == right.estimator_revision
        && left.call_count == right.call_count
}

fn should_replace_half_hour_bucket(
    incoming: &LocalHalfHourBucket,
    existing: &LocalHalfHourBucket,
) -> bool {
    if half_hour_bucket_evidence_eq(incoming, existing)
        && coverage_issue_state_differs(&incoming.partial_reasons, &existing.partial_reasons)
        && incoming.sampled_at != existing.sampled_at
    {
        // Prospective coverage quality is itself time-varying evidence. A
        // newer observation may reveal a restart gap even when its counters
        // are identical to an older exact open bucket; conversely, an older
        // exact writer must not erase a newer partial/closed observation.
        return incoming.sampled_at > existing.sampled_at;
    }
    if incoming.estimator_revision != existing.estimator_revision {
        return incoming.estimator_revision > existing.estimator_revision
            && bucket_unweighted_evidence_dominates(incoming, existing)
            && bucket_collection_issue_count(incoming) <= bucket_collection_issue_count(existing)
            && incoming.sampled_at >= existing.sampled_at;
    }

    let incoming_dominates = bucket_evidence_dominates(incoming, existing);
    let existing_dominates = bucket_evidence_dominates(existing, incoming);
    if incoming_dominates != existing_dominates {
        return incoming_dominates;
    }

    let incoming_collection_issues = bucket_collection_issue_count(incoming);
    let existing_collection_issues = bucket_collection_issue_count(existing);
    if incoming_collection_issues != existing_collection_issues {
        return incoming_collection_issues < existing_collection_issues;
    }
    if incoming.sampled_at != existing.sampled_at {
        return incoming.sampled_at > existing.sampled_at;
    }

    bucket_evidence_key(incoming) > bucket_evidence_key(existing)
}

fn bucket_collection_issue_count(bucket: &LocalHalfHourBucket) -> usize {
    collection_issue_count(&bucket.partial_reasons)
}

fn should_replace_weekly_local_point(
    incoming: &WeeklyLocalPoint,
    existing: &WeeklyLocalPoint,
) -> bool {
    if weekly_local_point_evidence_eq(incoming, existing)
        && coverage_issue_state_differs(&incoming.partial_reasons, &existing.partial_reasons)
        && incoming.observed_at != existing.observed_at
    {
        return incoming.observed_at > existing.observed_at;
    }
    if incoming.estimator_revision != existing.estimator_revision {
        return incoming.estimator_revision > existing.estimator_revision
            && weekly_unweighted_evidence_dominates(incoming, existing)
            && collection_issue_count(&incoming.partial_reasons)
                <= collection_issue_count(&existing.partial_reasons)
            && incoming.observed_at >= existing.observed_at;
    }

    let incoming_dominates = weekly_evidence_dominates(incoming, existing);
    let existing_dominates = weekly_evidence_dominates(existing, incoming);
    if incoming_dominates != existing_dominates {
        return incoming_dominates;
    }
    let incoming_collection_issues = collection_issue_count(&incoming.partial_reasons);
    let existing_collection_issues = collection_issue_count(&existing.partial_reasons);
    if incoming_collection_issues != existing_collection_issues {
        return incoming_collection_issues < existing_collection_issues;
    }
    if incoming.observed_at != existing.observed_at {
        return incoming.observed_at > existing.observed_at;
    }
    weekly_evidence_key(incoming) > weekly_evidence_key(existing)
}

fn collection_issue_count(partial_reasons: &[String]) -> usize {
    partial_reasons
        .iter()
        .filter(|reason| is_collection_issue_reason(reason))
        .count()
}

fn is_collection_issue_reason(reason: &str) -> bool {
    reason.starts_with("rollout_")
        || reason == "coverage_starts_within_local_bucket"
        || reason == "local_scan_disabled"
}

fn coverage_issue_state_differs(left: &[String], right: &[String]) -> bool {
    [
        "rollout_local_coverage_unverified",
        "coverage_starts_within_local_bucket",
    ]
    .into_iter()
    .any(|reason| {
        left.iter().any(|value| value == reason) != right.iter().any(|value| value == reason)
    })
}

fn has_coverage_issue(reasons: &[String]) -> bool {
    reasons
        .iter()
        .any(|reason| is_coverage_issue_reason(reason))
}

fn is_coverage_issue_reason(reason: &str) -> bool {
    matches!(
        reason,
        "rollout_local_coverage_unverified" | "coverage_starts_within_local_bucket"
    )
}

fn merge_coverage_issues(existing: &mut Vec<String>, incoming: &[String]) {
    existing.extend(
        incoming
            .iter()
            .filter(|reason| is_coverage_issue_reason(reason))
            .cloned(),
    );
    existing.sort();
    existing.dedup();
}

fn bucket_evidence_dominates(candidate: &LocalHalfHourBucket, other: &LocalHalfHourBucket) -> bool {
    bucket_unweighted_evidence_dominates(candidate, other)
        && candidate.estimated_cost_units >= other.estimated_cost_units
}

fn bucket_unweighted_evidence_dominates(
    candidate: &LocalHalfHourBucket,
    other: &LocalHalfHourBucket,
) -> bool {
    candidate.call_count >= other.call_count
        && candidate.token_usage.input_tokens >= other.token_usage.input_tokens
        && candidate.token_usage.cached_input_tokens >= other.token_usage.cached_input_tokens
        && candidate.token_usage.cache_write_input_tokens
            >= other.token_usage.cache_write_input_tokens
        && candidate.token_usage.output_tokens >= other.token_usage.output_tokens
        && candidate.token_usage.reasoning_output_tokens
            >= other.token_usage.reasoning_output_tokens
        && candidate.token_usage.total_tokens >= other.token_usage.total_tokens
}

fn bucket_evidence_key(
    bucket: &LocalHalfHourBucket,
) -> (
    u64,
    u128,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    usize,
    u32,
    u32,
) {
    let project_call_count = bucket
        .project_groups
        .iter()
        .map(|group| group.call_count)
        .fold(0_u64, u64::saturating_add);
    (
        bucket.token_usage.total_tokens,
        bucket.estimated_cost_units,
        bucket.call_count,
        bucket.token_usage.input_tokens,
        bucket.token_usage.cached_input_tokens,
        bucket.token_usage.cache_write_input_tokens,
        bucket.token_usage.output_tokens,
        bucket.token_usage.reasoning_output_tokens,
        project_call_count,
        bucket.project_groups.len(),
        bucket.project_breakdown_revision,
        bucket.api_pricing_catalog_revision,
    )
}

fn weekly_evidence_dominates(candidate: &WeeklyLocalPoint, other: &WeeklyLocalPoint) -> bool {
    weekly_unweighted_evidence_dominates(candidate, other)
        && candidate.estimated_cost_units >= other.estimated_cost_units
}

fn weekly_point_can_suppress_later(candidate: &WeeklyLocalPoint, later: &WeeklyLocalPoint) -> bool {
    if candidate.estimator_revision == later.estimator_revision {
        return weekly_evidence_dominates(candidate, later);
    }
    if candidate.estimator_revision > later.estimator_revision {
        return weekly_unweighted_evidence_dominates(candidate, later);
    }

    weekly_unweighted_evidence_dominates(candidate, later)
        && !weekly_unweighted_evidence_dominates(later, candidate)
}

fn weekly_unweighted_evidence_dominates(
    candidate: &WeeklyLocalPoint,
    other: &WeeklyLocalPoint,
) -> bool {
    candidate.call_count >= other.call_count
        && candidate.token_usage.input_tokens >= other.token_usage.input_tokens
        && candidate.token_usage.cached_input_tokens >= other.token_usage.cached_input_tokens
        && candidate.token_usage.cache_write_input_tokens
            >= other.token_usage.cache_write_input_tokens
        && candidate.token_usage.output_tokens >= other.token_usage.output_tokens
        && candidate.token_usage.reasoning_output_tokens
            >= other.token_usage.reasoning_output_tokens
        && candidate.token_usage.total_tokens >= other.token_usage.total_tokens
}

fn weekly_evidence_key(point: &WeeklyLocalPoint) -> (u64, u128, u64, u64, u64, u64, u64, u64) {
    (
        point.token_usage.total_tokens,
        point.estimated_cost_units,
        point.call_count,
        point.token_usage.input_tokens,
        point.token_usage.cached_input_tokens,
        point.token_usage.cache_write_input_tokens,
        point.token_usage.output_tokens,
        point.token_usage.reasoning_output_tokens,
    )
}

fn normalize_loaded_data(data: &mut HistoryData) {
    let mut quota = Vec::new();
    for point in std::mem::take(&mut data.quota_points) {
        let _ = upsert_quota_point(&mut quota, point);
    }
    data.quota_points = quota;

    let mut buckets = Vec::new();
    for bucket in std::mem::take(&mut data.half_hour_buckets) {
        let _ = upsert_half_hour_bucket(&mut buckets, bucket);
    }
    data.half_hour_buckets = buckets;

    let mut loaded_weekly = std::mem::take(&mut data.weekly_local_points);
    loaded_weekly.sort_by_key(|point| point.observed_at);
    let mut weekly = Vec::new();
    for point in loaded_weekly {
        let _ = upsert_weekly_local_point(&mut weekly, point);
    }
    data.weekly_local_points = weekly;
    sort_history_data(data);
}

fn sort_history_data(data: &mut HistoryData) {
    data.quota_points.sort_by(|left, right| {
        left.observed_at
            .cmp(&right.observed_at)
            .then_with(|| left.duration_mins.cmp(&right.duration_mins))
            .then_with(|| left.resets_at.cmp(&right.resets_at))
    });
    data.half_hour_buckets
        .sort_by_key(|bucket| bucket.starts_at);
    data.weekly_local_points
        .sort_by_key(|point| point.observed_at);
}

struct NamespaceInspection {
    future_version: bool,
    warnings: Vec<String>,
}

fn inspect_namespace(directory: &Path, namespace: &str) -> NamespaceInspection {
    let mut inspection = NamespaceInspection {
        future_version: false,
        warnings: Vec::new(),
    };
    let entries = match shard_entries(directory) {
        Ok(entries) => entries,
        Err(error) => {
            inspection.warnings.push(format!(
                "could not list history in {}: {error}",
                directory.display()
            ));
            return inspection;
        }
    };
    for (day, path) in entries {
        match read_shard(&path, namespace, day) {
            ShardRead::FutureFormat(version) => {
                inspection.future_version = true;
                inspection.warnings.push(format!(
                    "{} uses future history format version {version}; writes are disabled",
                    path.display()
                ));
            }
            ShardRead::FutureMetric(revision) => {
                inspection.future_version = true;
                inspection.warnings.push(format!(
                    "{} uses future history metric revision {revision}; writes are disabled",
                    path.display()
                ));
            }
            ShardRead::Corrupt(message) => inspection.warnings.push(message),
            ShardRead::Missing | ShardRead::Current { .. } => {}
        }
    }
    inspection
}

fn shard_entries(directory: &Path) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(day) = shard_day_from_path(&path) else {
            continue;
        };
        entries.push((day, path));
    }
    entries.sort_by_key(|(day, _)| *day);
    Ok(entries)
}

fn shard_day_from_path(path: &Path) -> Option<NaiveDate> {
    if path.extension().and_then(OsStr::to_str) != Some("json") {
        return None;
    }
    NaiveDate::parse_from_str(path.file_stem()?.to_str()?, "%Y-%m-%d").ok()
}

fn shard_path(directory: &Path, day: NaiveDate) -> PathBuf {
    directory.join(format!("{day}.json"))
}

fn write_shard_atomically(path: &Path, shard: &HistoryShard) -> io::Result<()> {
    let mut contents = serde_json::to_vec_pretty(shard)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');
    write_private_atomically(path, &contents)
}

fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("history"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        sync_directory(parent);
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
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique history temporary file",
    ))
}

fn open_lock_file(directory: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(directory.join(LOCK_FILE))
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn prune_old_shards(directory: &Path, cutoff_day: NaiveDate, warnings: &mut Vec<String>) -> usize {
    let entries = match shard_entries(directory) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!(
                "could not inspect old history shards in {}: {error}",
                directory.display()
            ));
            return 0;
        }
    };
    let mut pruned = 0;
    for (day, path) in entries {
        if day >= cutoff_day {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => pruned += 1,
            Err(error) => warnings.push(format!(
                "could not prune old history shard {}: {error}",
                path.display()
            )),
        }
    }
    pruned
}

pub fn default_history_root() -> Option<PathBuf> {
    resolve_history_root(
        nonempty_env(STATE_DIRECTORY_ENV).as_deref(),
        nonempty_env("XDG_STATE_HOME").as_deref(),
        nonempty_env("HOME").as_deref(),
        nonempty_env("LOCALAPPDATA").as_deref(),
        current_platform(),
    )
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Windows,
    Unix,
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

fn resolve_history_root(
    state_directory: Option<&Path>,
    xdg_state_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(directory) = state_directory.filter(|path| !path.as_os_str().is_empty()) {
        return Some(directory.join(HISTORY_DIRECTORY));
    }
    if let Some(directory) = xdg_state_home.filter(|path| !path.as_os_str().is_empty()) {
        return Some(directory.join(APP_DIRECTORY).join(HISTORY_DIRECTORY));
    }
    let directory = match platform {
        Platform::MacOs => home.map(|path| path.join("Library/Application Support")),
        Platform::Windows => local_app_data.map(Path::to_path_buf),
        Platform::Unix => home.map(|path| path.join(".local/state")),
    }?;
    Some(directory.join(APP_DIRECTORY).join(HISTORY_DIRECTORY))
}

pub fn history_namespace(codex_home: &Path) -> String {
    let normalized = normalized_path(codex_home);
    let bytes = history_namespace_bytes(&normalized);
    format!("{:016x}", stable_hash(&bytes))
}

pub fn history_namespace_with_redaction(codex_home: &Path, redact_content: bool) -> String {
    let namespace = history_namespace(codex_home);
    if redact_content {
        format!("{namespace}-redacted")
    } else {
        namespace
    }
}

#[cfg(unix)]
fn history_namespace_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn history_namespace_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    if let Some(path) = path.to_str() {
        // Preserve existing history namespaces for ordinary Windows paths.
        return path.replace('\\', "/").to_ascii_lowercase().into_bytes();
    }

    // WTF-16 can contain unpaired surrogates, so encode code units explicitly.
    // The invalid UTF-8 prefix keeps this representation disjoint from every
    // legacy namespace input while remaining stable across Rust versions.
    let mut bytes = vec![0xff, b'w', b't', b'f', b'1', b'6', 0];
    for mut unit in path.as_os_str().encode_wide() {
        if unit == u16::from(b'\\') {
            unit = u16::from(b'/');
        } else if unit <= 0x7f {
            unit = u16::from((unit as u8).to_ascii_lowercase());
        }
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(not(any(unix, windows)))]
fn history_namespace_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

fn normalized_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    fs::canonicalize(&absolute).unwrap_or_else(|_| lexical_normalize(&absolute))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn stable_hash(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;
    use crate::domain::{Confidence, LimitWindow, TaskStatus, UsageCall};

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
            .single()
            .unwrap()
    }

    fn usage(total: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        }
    }

    fn call(
        timestamp: DateTime<Utc>,
        model: &str,
        service_tier: Option<&str>,
        total: u64,
    ) -> UsageCall {
        UsageCall {
            timestamp,
            thread_id: "thread".to_string(),
            turn_id: Some("turn".to_string()),
            model: Some(model.to_string()),
            service_tier: service_tier.map(str::to_string),
            tokens: usage(total),
            request_usage_exact: true,
        }
    }

    fn task(
        thread_id: &str,
        parent_thread_id: Option<&str>,
        cwd: Option<&str>,
        title: &str,
        timestamp: DateTime<Utc>,
    ) -> TaskRecord {
        TaskRecord {
            thread_id: thread_id.to_string(),
            parent_thread_id: parent_thread_id.map(str::to_string),
            archived: false,
            title: title.to_string(),
            cwd: cwd.map(PathBuf::from),
            source: Some("cli".to_string()),
            created_at: Some(timestamp),
            updated_at: Some(timestamp),
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
        }
    }

    fn bucket(as_of: DateTime<Utc>, provenance: Provenance, limit_id: &str) -> LimitBucket {
        LimitBucket {
            limit_id: limit_id.to_string(),
            limit_name: None,
            plan_type: None,
            primary: Some(LimitWindow::new(
                25.0,
                Some(FIVE_HOURS_MINS),
                Some(as_of + Duration::hours(2)),
            )),
            secondary: Some(LimitWindow::new(
                40.0,
                Some(WEEK_MINS),
                Some(as_of + Duration::days(3)),
            )),
            credits: None,
            rate_limit_reached_type: None,
            provenance,
            as_of,
        }
    }

    #[test]
    fn state_root_resolution_matches_all_supported_platforms() {
        let override_dir = Path::new("/override");
        let xdg = Path::new("/xdg");
        let home = Path::new("/home/user");
        let local = Path::new("C:/Users/user/AppData/Local");
        assert_eq!(
            resolve_history_root(
                Some(override_dir),
                Some(xdg),
                Some(home),
                Some(local),
                Platform::Unix,
            ),
            Some(override_dir.join(HISTORY_DIRECTORY))
        );
        assert_eq!(
            resolve_history_root(None, Some(xdg), Some(home), None, Platform::Unix),
            Some(xdg.join(APP_DIRECTORY).join(HISTORY_DIRECTORY))
        );
        assert_eq!(
            resolve_history_root(None, None, Some(home), None, Platform::MacOs),
            Some(
                home.join("Library/Application Support")
                    .join(APP_DIRECTORY)
                    .join(HISTORY_DIRECTORY)
            )
        );
        assert_eq!(
            resolve_history_root(None, None, Some(home), None, Platform::Unix),
            Some(
                home.join(".local/state")
                    .join(APP_DIRECTORY)
                    .join(HISTORY_DIRECTORY)
            )
        );
        assert_eq!(
            resolve_history_root(None, None, None, Some(local), Platform::Windows),
            Some(local.join(APP_DIRECTORY).join(HISTORY_DIRECTORY))
        );
        assert_eq!(
            resolve_history_root(None, None, None, None, Platform::Unix),
            None
        );
    }

    #[test]
    fn codex_home_namespace_is_stable_and_path_specific() {
        let root = tempdir().unwrap();
        let first = root.path().join("first/../first");
        let equivalent = root.path().join("first");
        let other = root.path().join("other");
        let mut legacy_bytes = normalized_path(&first).to_string_lossy().into_owned();
        if cfg!(windows) {
            legacy_bytes = legacy_bytes.replace('\\', "/").to_ascii_lowercase();
        }
        assert_eq!(history_namespace(&first), history_namespace(&equivalent));
        assert_ne!(history_namespace(&first), history_namespace(&other));
        assert_eq!(history_namespace(&first).len(), 16);
        assert_eq!(
            history_namespace(&first),
            format!("{:016x}", stable_hash(legacy_bytes.as_bytes()))
        );
        assert_eq!(
            history_namespace_with_redaction(&first, false),
            history_namespace(&first)
        );
        assert_eq!(
            history_namespace_with_redaction(&first, true),
            format!("{}-redacted", history_namespace(&first))
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_home_namespace_preserves_non_utf8_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first = PathBuf::from(OsString::from_vec(
            b"/tmp/codex-usage-monit-home-\xff".to_vec(),
        ));
        let second = PathBuf::from(OsString::from_vec(
            b"/tmp/codex-usage-monit-home-\xfe".to_vec(),
        ));

        assert_ne!(first, second);
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(history_namespace(&first), history_namespace(&second));
    }

    #[test]
    fn observation_uses_only_fresh_codex_limits_and_builds_exact_local_buckets() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let calls = vec![
            call(at(2026, 7, 28, 11, 59, 59), "gpt-5.4", None, 10),
            call(at(2026, 7, 28, 12, 0, 0), "gpt-5.4", Some("priority"), 20),
            call(at(2026, 7, 28, 12, 1, 0), "gpt-5.3-codex-spark", None, 999),
        ];
        let limits = vec![
            bucket(now, Provenance::ServerSnapshot, "codex"),
            bucket(now, Provenance::Stale, "codex"),
            bucket(now, Provenance::ServerSnapshot, "codex_bengalfox"),
        ];
        let observation = HistoryObservation::from_sources(
            now,
            &calls,
            &limits,
            &["rollout_scan_incomplete".to_string()],
        );
        assert_eq!(observation.quota_points.len(), 2);
        assert_eq!(observation.weekly_local_points.len(), 2);
        assert_eq!(
            observation
                .weekly_local_points
                .last()
                .unwrap()
                .token_usage
                .total_tokens,
            30
        );
        assert!(
            observation
                .weekly_local_points
                .last()
                .unwrap()
                .partial_reasons
                .contains(&"rollout_scan_incomplete".to_string())
        );
        assert!(
            observation
                .quota_points
                .iter()
                .all(|point| point.provenance == Provenance::ServerSnapshot)
        );
        assert_eq!(observation.half_hour_buckets.len(), 2);
        assert!(observation.half_hour_buckets.iter().all(|bucket| {
            bucket.project_breakdown_revision == HISTORY_PROJECT_BREAKDOWN_REVISION
                && bucket.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION
        }));
        assert_eq!(
            observation.half_hour_buckets[0].starts_at,
            at(2026, 7, 28, 11, 45, 0)
        );
        assert_eq!(
            observation.half_hour_buckets[0].ends_at,
            at(2026, 7, 28, 12, 0, 0)
        );
        assert_eq!(
            observation.half_hour_buckets[0].token_usage.total_tokens,
            10
        );
        assert_eq!(
            observation.half_hour_buckets[1].token_usage.total_tokens,
            20
        );
        assert_eq!(observation.half_hour_buckets[1].groups.len(), 1);
        assert_eq!(
            observation.half_hour_buckets[1].groups[0]
                .service_tier
                .as_deref(),
            Some("priority")
        );
        assert!(
            observation.half_hour_buckets[1].estimated_cost_units
                > observation.half_hour_buckets[0].estimated_cost_units
        );
        assert_eq!(
            observation.half_hour_buckets[1].partial_reasons,
            vec!["rollout_scan_incomplete"]
        );
    }

    #[test]
    fn spark_only_project_bucket_keeps_collection_partial_reasons() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let observation = HistoryObservation::from_sources(
            now,
            &[call(
                now - Duration::minutes(1),
                "gpt-5.3-codex-spark",
                None,
                999,
            )],
            &[],
            &["rollout_scan_incomplete".to_string()],
        );

        let bucket = observation.half_hour_buckets.last().unwrap();
        assert_eq!(bucket.token_usage.total_tokens, 0);
        assert_eq!(bucket.call_count, 0);
        assert_eq!(bucket.project_groups.len(), 1);
        assert_eq!(
            bucket.project_groups[0].api_long_context_extra_cost_units,
            Some(0)
        );
        assert_eq!(
            bucket.partial_reasons,
            ["rollout_scan_incomplete".to_string()]
        );
    }

    #[test]
    fn project_groups_inherit_the_parent_project_and_preserve_zero_usage_ancestors() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let timestamp = at(2026, 7, 28, 12, 1, 0);
        let tasks = vec![
            task(
                "root",
                None,
                Some("/work/alpha"),
                "Root task",
                timestamp - Duration::days(40),
            ),
            task("child", Some("root"), None, "Child task", timestamp),
        ];
        let mut child_call = call(timestamp, "gpt-5.6-luna", Some("standard"), 200);
        child_call.thread_id = "child".to_string();

        let observation = HistoryObservation::from_sources_with_tasks_and_coverage(
            now,
            &[child_call],
            &tasks,
            &[],
            &[],
            None,
        );

        assert_eq!(observation.half_hour_buckets.len(), 1);
        let bucket = observation
            .half_hour_buckets
            .iter()
            .find(|bucket| bucket.token_usage.total_tokens > 0)
            .unwrap();
        assert_eq!(bucket.project_groups.len(), 2);
        let root = bucket
            .project_groups
            .iter()
            .find(|group| group.thread_id == "root")
            .unwrap();
        let child = bucket
            .project_groups
            .iter()
            .find(|group| group.thread_id == "child")
            .unwrap();

        assert_eq!(root.project_id, child.project_id);
        assert_eq!(root.project_label.as_deref(), Some("alpha"));
        assert_eq!(child.project_label.as_deref(), Some("alpha"));
        assert_eq!(child.parent_thread_id.as_deref(), Some("root"));
        assert_eq!(root.title.as_deref(), Some("Root task"));
        assert_eq!(child.title.as_deref(), Some("Child task"));
        assert_eq!(root.source.as_deref(), Some("cli"));
        assert!(root.token_usage.is_zero());
        assert_eq!(root.call_count, 0);
        assert_eq!(root.api_equivalent_cost, ApiCostAmount::default());
        assert_eq!(child.token_usage.total_tokens, 200);
        assert_eq!(child.call_count, 1);
    }

    #[test]
    fn project_group_titles_are_bounded_before_being_repeated_across_buckets() {
        let timestamp = at(2026, 7, 28, 12, 1, 0);
        let title = "界".repeat(PROJECT_TITLE_MAX_CHARS + 20);
        let tasks = vec![task("thread", None, Some("/work/alpha"), &title, timestamp)];
        let mut usage_call = call(timestamp, "gpt-5.6-luna", Some("standard"), 10);
        usage_call.thread_id = "thread".to_string();

        let observation = HistoryObservation::from_sources_with_tasks_and_coverage(
            timestamp,
            &[usage_call],
            &tasks,
            &[],
            &[],
            None,
        );
        let stored = observation.half_hour_buckets[0].project_groups[0]
            .title
            .as_deref()
            .unwrap();

        assert_eq!(stored.chars().count(), PROJECT_TITLE_MAX_CHARS + 1);
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn project_groups_keep_own_usage_only_and_reconcile_with_the_bucket() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let timestamp = at(2026, 7, 28, 12, 1, 0);
        let tasks = vec![
            task("root", None, Some("/work/alpha"), "Root task", timestamp),
            task("child", Some("root"), None, "Child task", timestamp),
        ];
        let mut root_call = call(timestamp, "gpt-5.6-luna", Some("standard"), 100);
        root_call.thread_id = "root".to_string();
        let mut child_call = call(
            timestamp + Duration::minutes(1),
            "gpt-5.6-luna",
            Some("standard"),
            200,
        );
        child_call.thread_id = "child".to_string();
        let calls = vec![root_call, child_call];

        let observation = HistoryObservation::from_sources_with_tasks_and_coverage(
            now,
            &calls,
            &tasks,
            &[],
            &[],
            None,
        );

        let bucket = &observation.half_hour_buckets[0];
        let root = bucket
            .project_groups
            .iter()
            .find(|group| group.thread_id == "root")
            .unwrap();
        let child = bucket
            .project_groups
            .iter()
            .find(|group| group.thread_id == "child")
            .unwrap();
        assert_eq!(root.token_usage.total_tokens, 100);
        assert_eq!(child.token_usage.total_tokens, 200);
        assert_eq!(root.call_count, 1);
        assert_eq!(child.call_count, 1);

        let mut grouped_tokens = TokenUsage::default();
        let mut grouped_estimated_cost = 0_u128;
        let mut grouped_long_context_extra = 0_u128;
        let mut grouped_call_count = 0_u64;
        let mut grouped_api_cost = ApiCostAmount::default();
        for group in &bucket.project_groups {
            grouped_tokens.add_assign(group.token_usage);
            grouped_estimated_cost =
                grouped_estimated_cost.saturating_add(group.estimated_cost_units);
            grouped_long_context_extra = grouped_long_context_extra
                .saturating_add(group.api_long_context_extra_cost_units.unwrap_or_default());
            grouped_call_count = grouped_call_count.saturating_add(group.call_count);
            grouped_api_cost.add_assign(group.api_equivalent_cost);
        }
        let mut expected_api_cost = ApiCostAggregation::default();
        for call in &calls {
            expected_api_cost.add_call(call);
        }

        assert_eq!(grouped_tokens, bucket.token_usage);
        assert_eq!(grouped_estimated_cost, bucket.estimated_cost_units);
        assert_eq!(
            grouped_long_context_extra,
            bucket.api_long_context_extra_cost_units.unwrap()
        );
        assert_eq!(grouped_call_count, bucket.call_count);
        assert_eq!(grouped_api_cost, expected_api_cost.total().amount);
    }

    #[test]
    fn project_groups_apply_api_and_long_context_pricing_per_call() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let timestamp = at(2026, 7, 28, 12, 1, 0);
        let tasks = vec![task("thread", None, Some("/work/alpha"), "Task", timestamp)];
        let short = call(timestamp, "gpt-5.6-luna", Some("standard"), 100);
        let long = call(
            timestamp + Duration::minutes(1),
            "gpt-5.6-luna",
            Some("standard"),
            272_001,
        );
        let calls = vec![short.clone(), long.clone()];

        let observation = HistoryObservation::from_sources_with_tasks_and_coverage(
            now,
            &calls,
            &tasks,
            &[],
            &[],
            None,
        );
        let group = &observation.half_hour_buckets[0].project_groups[0];

        let mut expected_api_cost = ApiCostAggregation::default();
        expected_api_cost.add_call(&short);
        expected_api_cost.add_call(&long);
        let expected_api_cost = expected_api_cost.thread("thread");
        let short_weight = estimate_call_weight(&short);
        let long_weight = estimate_call_weight(&long);
        let expected_long_context_extra = short_weight
            .api_long_context_extra_units
            .saturating_add(long_weight.api_long_context_extra_units);

        assert_eq!(group.call_count, 2);
        assert_eq!(group.api_equivalent_cost, expected_api_cost);
        assert_eq!(group.api_equivalent_cost.observed_samples, 2);
        assert_eq!(group.api_equivalent_cost.priced_samples, 2);
        assert_eq!(
            group.api_long_context_extra_cost_units,
            Some(expected_long_context_extra)
        );
        assert_eq!(short_weight.api_long_context_extra_units, 0);
        assert!(long_weight.api_long_context_extra_units > 0);

        let mut combined = long;
        combined.tokens.add_assign(short.tokens);
        let mut aggregate_priced_as_one_call = ApiCostAggregation::default();
        aggregate_priced_as_one_call.add_call(&combined);
        assert_ne!(
            group.api_equivalent_cost,
            aggregate_priced_as_one_call.thread("thread")
        );
        assert_ne!(
            expected_long_context_extra,
            estimate_call_weight(&combined).api_long_context_extra_units
        );
    }

    #[test]
    fn long_context_detection_quality_propagates_to_local_and_weekly_history() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let mut unverified = call(now - Duration::minutes(1), "gpt-5.6-luna", None, 300_001);
        unverified.request_usage_exact = false;

        let observation = HistoryObservation::from_sources(
            now,
            &[unverified],
            &[bucket(now, Provenance::ServerSnapshot, "codex")],
            &[],
        );

        let local = observation.half_hour_buckets.last().unwrap();
        assert_eq!(local.estimator_revision, 5);
        assert!(local.partial_reasons.is_empty());
        assert!(local.long_context_usage_unknown);
        assert_eq!(local.groups.len(), 1);
        assert!(!local.groups[0].used_long_context_pricing);
        assert!(local.groups[0].used_long_context_detection_fallback);

        let weekly = observation.weekly_local_points.last().unwrap();
        assert_eq!(weekly.estimator_revision, 5);
        assert!(weekly.partial_reasons.is_empty());
        assert!(weekly.long_context_usage_unknown);

        let reset = weekly.resets_at;
        let history = HistoryData {
            quota_points: observation.quota_points.clone(),
            half_hour_buckets: observation.half_hour_buckets.clone(),
            weekly_local_points: observation.weekly_local_points.clone(),
            ..HistoryData::default()
        };
        let base = history.estimated_half_hour_series(reset);
        assert!(base.iter().all(|point| {
            !point
                .partial_reasons
                .contains(&"long_context_usage_unknown".to_string())
        }));
        let api = history.estimated_half_hour_series_with_api_long_context(reset, true);
        assert!(api.iter().any(|point| {
            point
                .partial_reasons
                .contains(&"long_context_usage_unknown".to_string())
        }));
    }

    #[test]
    fn one_observation_records_base_and_api_long_context_weights_together() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let observation = HistoryObservation::from_sources(
            now,
            &[call(
                now - Duration::minutes(1),
                "gpt-5.6-luna",
                None,
                300_001,
            )],
            &[bucket(now, Provenance::ServerSnapshot, "codex")],
            &[],
        );

        let local = observation.half_hour_buckets.last().unwrap();
        let extra = local.api_long_context_extra_cost_units.unwrap();
        assert!(extra > 0);
        assert_eq!(
            local.groups[0].api_long_context_extra_cost_units,
            Some(extra)
        );
        let weekly = observation.weekly_local_points.last().unwrap();
        assert_eq!(weekly.api_long_context_extra_cost_units, Some(extra));
        let reset = weekly.resets_at;

        let history = HistoryData {
            quota_points: observation.quota_points,
            half_hour_buckets: observation.half_hour_buckets,
            weekly_local_points: observation.weekly_local_points,
            ..HistoryData::default()
        };
        let base = history.estimated_half_hour_series(reset);
        let api = history.estimated_half_hour_series_with_api_long_context(reset, true);
        assert_eq!(base.len(), api.len());
        assert_eq!(
            api.last().unwrap().estimated_cost_units,
            base.last()
                .unwrap()
                .estimated_cost_units
                .saturating_add(extra)
        );
    }

    #[test]
    fn complete_local_coverage_materializes_zero_buckets_but_keeps_partial_edges_explicit() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let observation = HistoryObservation::from_sources_with_coverage(
            now,
            &[call(at(2026, 7, 28, 11, 45, 0), "gpt-5.4", None, 10)],
            &[],
            &[],
            Some(at(2026, 7, 28, 10, 10, 0)),
        );

        assert_eq!(observation.half_hour_buckets.len(), 9);
        assert_eq!(
            observation.half_hour_buckets[0].starts_at,
            at(2026, 7, 28, 10, 0, 0)
        );
        assert_eq!(
            observation.half_hour_buckets[0].partial_reasons,
            vec!["coverage_starts_within_local_bucket"]
        );
        assert_eq!(
            observation
                .half_hour_buckets
                .iter()
                .map(|bucket| bucket.token_usage.total_tokens)
                .collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 0, 0, 0, 10, 0]
        );
        assert_eq!(
            observation.half_hour_buckets.last().unwrap().sampled_at,
            now
        );
    }

    #[test]
    fn prospective_coverage_preserves_older_calls_without_filling_the_unknown_gap() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let observation = HistoryObservation::from_sources_with_coverage(
            now,
            &[call(at(2026, 7, 28, 9, 5, 0), "gpt-5.4", None, 10)],
            &[bucket(now, Provenance::ServerSnapshot, "codex")],
            &[],
            Some(at(2026, 7, 28, 11, 50, 0)),
        );

        assert_eq!(
            observation
                .weekly_local_points
                .iter()
                .map(|point| (point.observed_at, point.token_usage.total_tokens))
                .collect::<Vec<_>>(),
            vec![
                (at(2026, 7, 28, 9, 30, 0), 10),
                (at(2026, 7, 28, 12, 0, 0), 10),
                (now, 10),
            ]
        );
        assert_eq!(
            observation.weekly_local_points[0].partial_reasons,
            ["rollout_local_coverage_unverified".to_string()]
        );
        assert_eq!(
            observation.weekly_local_points[1].partial_reasons,
            [
                "coverage_starts_within_local_bucket".to_string(),
                "rollout_local_coverage_unverified".to_string(),
            ]
        );
        assert_eq!(
            observation.weekly_local_points[2].partial_reasons,
            [
                "coverage_starts_within_local_bucket".to_string(),
                "rollout_local_coverage_unverified".to_string(),
            ]
        );
        assert_eq!(observation.half_hour_buckets.len(), 3);
        assert_eq!(
            observation.half_hour_buckets[0].partial_reasons,
            ["rollout_local_coverage_unverified".to_string()]
        );
        assert_eq!(
            observation.half_hour_buckets[1].partial_reasons,
            ["coverage_starts_within_local_bucket".to_string()]
        );
        assert!(observation.half_hour_buckets[2].partial_reasons.is_empty());
    }

    #[test]
    fn aligned_prospective_coverage_keeps_the_unknown_weekly_prefix_partial() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let observation = HistoryObservation::from_sources_with_coverage(
            now,
            &[call(at(2026, 7, 28, 9, 5, 0), "gpt-5.4", None, 10)],
            &[bucket(now, Provenance::ServerSnapshot, "codex")],
            &[],
            Some(at(2026, 7, 28, 11, 30, 0)),
        );

        assert!(observation.weekly_local_points.iter().all(|point| {
            point.partial_reasons == ["rollout_local_coverage_unverified".to_string()]
        }));
        assert_eq!(
            observation.half_hour_buckets[0].partial_reasons,
            ["rollout_local_coverage_unverified".to_string()]
        );
        assert!(
            observation
                .half_hour_buckets
                .iter()
                .skip(1)
                .all(|bucket| bucket.partial_reasons.is_empty())
        );
    }

    #[test]
    fn coverage_before_a_nonaligned_weekly_cycle_does_not_create_a_partial_edge() {
        let now = at(2026, 7, 28, 12, 7, 0);
        let limits = [bucket(now, Provenance::ServerSnapshot, "codex")];

        let points =
            weekly_local_points_from_sources(now, &[], &limits, &[], Some(now - Duration::days(5)));

        assert!(!points.is_empty());
        assert!(points.iter().all(|point| point.partial_reasons.is_empty()));
    }

    #[test]
    fn store_round_trips_upserts_and_prunes_old_shards() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let old_at = at(2026, 1, 1, 12, 0, 0);
        let old = HistoryObservation {
            observed_at: old_at,
            quota_points: vec![quota_point(old_at, old_at + Duration::days(3), 20.0)],
            half_hour_buckets: vec![local_bucket(old_at, old_at, 5, 50)],
            weekly_local_points: vec![weekly_point(old_at, old_at + Duration::days(3), 5, 50)],
        };
        store.record(&old).unwrap();

        let now = at(2026, 7, 28, 12, 5, 0);
        let first = HistoryObservation {
            observed_at: now,
            quota_points: vec![quota_point(now, now + Duration::days(3), 40.0)],
            half_hour_buckets: vec![local_bucket(at(2026, 7, 28, 12, 0, 0), now, 10, 100)],
            weekly_local_points: vec![weekly_point(now, now + Duration::days(3), 10, 100)],
        };
        store.record(&first).unwrap();
        let replacement_at = now + Duration::minutes(2);
        let replacement = HistoryObservation {
            observed_at: replacement_at,
            quota_points: vec![quota_point(replacement_at, now + Duration::days(3), 45.0)],
            half_hour_buckets: vec![local_bucket(
                at(2026, 7, 28, 12, 0, 0),
                replacement_at,
                20,
                200,
            )],
            weekly_local_points: vec![weekly_point(
                replacement_at,
                now + Duration::days(3),
                20,
                200,
            )],
        };
        let report = store.record(&replacement).unwrap();
        assert_eq!(report.shards_written, 1);
        assert_eq!(report.shards_pruned, 0);

        let data = store.load_since(now - Duration::days(1));
        assert_eq!(data.quota_points.len(), 1);
        assert_eq!(data.quota_points[0].used_percent, 45.0);
        assert_eq!(data.half_hour_buckets.len(), 1);
        assert_eq!(data.half_hour_buckets[0].token_usage.total_tokens, 20);
        assert_eq!(data.weekly_local_points.len(), 1);
        assert_eq!(data.weekly_local_points[0].token_usage.total_tokens, 20);
        assert!(!shard_path(store.namespace_dir().unwrap(), old_at.date_naive()).exists());
        assert!(data.warnings.is_empty());
    }

    #[test]
    fn redacted_store_isolates_visible_shards_and_scrubs_persisted_titles() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let now = at(2026, 7, 28, 12, 5, 0);
        let mut bucket = local_bucket(at(2026, 7, 28, 12, 0, 0), now, 10, 100);
        bucket.project_groups = vec![LocalProjectUsageGroup {
            thread_id: "thread".to_string(),
            title: Some("private customer migration".to_string()),
            token_usage: usage(10),
            estimated_cost_units: 100,
            call_count: 1,
            ..LocalProjectUsageGroup::default()
        }];
        let observation = HistoryObservation {
            observed_at: now,
            half_hour_buckets: vec![bucket],
            ..HistoryObservation::default()
        };

        let mut visible = HistoryStore::new(history_root.clone(), &codex_home);
        visible.record(&observation).unwrap();
        let visible_namespace = visible.namespace().to_string();

        let mut redacted = HistoryStore::new_with_redaction(history_root, &codex_home, true);
        assert_ne!(redacted.namespace(), visible_namespace);
        assert!(
            redacted
                .load_since(now - Duration::days(1))
                .half_hour_buckets
                .is_empty()
        );

        redacted.record(&observation).unwrap();
        let path = shard_path(redacted.namespace_dir().unwrap(), now.date_naive());
        let contents = fs::read_to_string(path).unwrap();
        assert!(!contents.contains("private customer migration"));
        assert!(contents.contains("[redacted]"));

        let data = redacted.load_since(now - Duration::days(1));
        assert_eq!(
            data.half_hour_buckets[0].project_groups[0].title.as_deref(),
            Some("[redacted]")
        );
    }

    #[test]
    fn redacted_store_scrubs_staged_titles_before_live_display_and_flush() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let now = at(2026, 7, 28, 12, 5, 0);
        let mut bucket = local_bucket(at(2026, 7, 28, 12, 0, 0), now, 10, 100);
        bucket.project_groups = vec![LocalProjectUsageGroup {
            thread_id: "thread".to_string(),
            title: Some("private staged title".to_string()),
            ..LocalProjectUsageGroup::default()
        }];
        let observation = HistoryObservation {
            observed_at: now,
            half_hour_buckets: vec![bucket],
            ..HistoryObservation::default()
        };
        let mut store =
            HistoryStore::new_with_redaction(directory.path().join("state"), &codex_home, true);

        store.stage(&observation);
        let live = store.load_since_with_staged(now - Duration::days(1));
        assert_eq!(
            live.half_hour_buckets[0].project_groups[0].title.as_deref(),
            Some("[redacted]")
        );
        store.flush_staged().unwrap();
        let path = shard_path(store.namespace_dir().unwrap(), now.date_naive());
        assert!(
            !fs::read_to_string(path)
                .unwrap()
                .contains("private staged title")
        );
    }

    #[test]
    fn repeated_full_lookback_observations_do_not_rewrite_closed_bucket_shards() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let observed_at = at(2026, 7, 28, 12, 7, 0);
        let calls = vec![
            call(at(2026, 7, 27, 10, 1, 0), "gpt-5.4", None, 10),
            call(at(2026, 7, 28, 10, 1, 0), "gpt-5.4", None, 20),
        ];
        let first = HistoryObservation::from_sources(observed_at, &calls, &[], &[]);
        let first_report = store.record(&first).unwrap();
        assert_eq!(first_report.shards_written, 2);
        assert!(
            first
                .half_hour_buckets
                .iter()
                .all(|bucket| bucket.sampled_at == bucket.ends_at)
        );

        let second =
            HistoryObservation::from_sources(observed_at + Duration::minutes(1), &calls, &[], &[]);
        let second_report = store.record(&second).unwrap();
        assert_eq!(second_report.shards_written, 0);
        assert_eq!(second_report.shards_skipped, 0);

        let periodic =
            HistoryObservation::from_sources(observed_at + Duration::minutes(31), &calls, &[], &[]);
        let periodic_report = store.record(&periodic).unwrap();
        assert_eq!(periodic_report.shards_written, 0);
        assert_eq!(periodic_report.shards_skipped, 2);
    }

    #[test]
    fn read_cache_merges_local_writes_and_periodically_observes_external_writers() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let first_at = starts_at + Duration::minutes(5);
        let since = starts_at - Duration::days(1);
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);
        let first = HistoryObservation {
            observed_at: first_at,
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(starts_at, first_at, 10, 100)],
            weekly_local_points: Vec::new(),
        };
        store.record(&first).unwrap();
        assert_eq!(
            store.load_since(since).half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );
        assert!(store.reload_since_if_stale(since).is_none());

        let second_at = first_at + Duration::minutes(1);
        let second = HistoryObservation {
            observed_at: second_at,
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(starts_at, second_at, 20, 200)],
            weekly_local_points: Vec::new(),
        };
        store.record(&second).unwrap();
        assert_eq!(
            store.load_since(since).half_hour_buckets[0]
                .token_usage
                .total_tokens,
            20
        );

        let external_at = second_at + Duration::minutes(1);
        let mut external = HistoryStore::new(history_root, &codex_home);
        external
            .record(&HistoryObservation {
                observed_at: external_at,
                quota_points: Vec::new(),
                half_hour_buckets: vec![local_bucket(starts_at, external_at, 30, 300)],
                weekly_local_points: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            store.load_since(since).half_hour_buckets[0]
                .token_usage
                .total_tokens,
            20
        );
        assert!(store.reload_since_if_stale(since).is_none());

        store.cache_loaded_at =
            Some(Instant::now() - HISTORY_READ_CACHE_TTL - StdDuration::from_secs(1));
        assert_eq!(
            store
                .reload_since_if_stale(since)
                .unwrap()
                .half_hour_buckets[0]
                .token_usage
                .total_tokens,
            30
        );
    }

    #[test]
    fn staged_history_updates_live_data_and_flushes_on_the_monotonic_deadline() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let since = starts_at - Duration::days(1);
        let interval = StdDuration::from_secs(30);
        let first_attempt = Instant::now();
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);

        store.stage(&HistoryObservation {
            observed_at: starts_at + Duration::minutes(5),
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(
                starts_at,
                starts_at + Duration::minutes(5),
                10,
                100,
            )],
            weekly_local_points: Vec::new(),
        });
        assert!(
            store
                .flush_staged_if_due_at(interval, first_attempt)
                .unwrap()
                .is_some()
        );
        assert!(store.staged_observation.is_none());

        store.stage(&HistoryObservation {
            observed_at: starts_at + Duration::minutes(6),
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(
                starts_at,
                starts_at + Duration::minutes(6),
                20,
                200,
            )],
            weekly_local_points: Vec::new(),
        });
        assert!(
            store
                .flush_staged_if_due_at(interval, first_attempt + StdDuration::from_secs(29),)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.load_since_with_staged(since).half_hour_buckets[0]
                .token_usage
                .total_tokens,
            20
        );
        let mut persisted = HistoryStore::new(history_root.clone(), &codex_home);
        assert_eq!(
            persisted.load_since(since).half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );

        assert!(
            store
                .flush_staged_if_due_at(interval, first_attempt + StdDuration::from_secs(30),)
                .unwrap()
                .is_some()
        );
        assert!(store.staged_observation.is_none());
        let mut persisted = HistoryStore::new(history_root, &codex_home);
        assert_eq!(
            persisted.load_since(since).half_hour_buckets[0]
                .token_usage
                .total_tokens,
            20
        );
    }

    #[test]
    fn staged_history_keeps_recent_deltas_until_a_full_merge_is_due() {
        let directory = tempdir().unwrap();
        let observed_at = at(2026, 7, 28, 12, 5, 0);
        let recent_start = at(2026, 7, 28, 12, 0, 0);
        let old_start = at(2026, 7, 28, 9, 30, 0);
        let reset = observed_at + Duration::days(7);
        let observation = HistoryObservation {
            observed_at,
            quota_points: Vec::new(),
            half_hour_buckets: vec![
                local_bucket(old_start, old_start + Duration::minutes(30), 5, 50),
                local_bucket(recent_start, observed_at, 10, 100),
            ],
            weekly_local_points: vec![
                weekly_point(old_start + Duration::minutes(30), reset, 5, 50),
                weekly_point(observed_at, reset, 10, 100),
            ],
        };
        let mut store = HistoryStore::new(
            directory.path().join("state"),
            directory.path().join("codex").as_path(),
        );
        store.last_full_merge_at = Some(observed_at - Duration::minutes(5));

        store.stage(&observation);
        let staged = store.staged_observation.as_ref().unwrap();
        assert_eq!(staged.half_hour_buckets.len(), 1);
        assert_eq!(staged.weekly_local_points.len(), 1);
        assert!(!store.staged_force_full_merge);

        store.staged_observation = None;
        store.last_full_merge_at = Some(observed_at - Duration::minutes(31));
        store.stage(&observation);
        let staged = store.staged_observation.as_ref().unwrap();
        assert_eq!(staged.half_hour_buckets.len(), 2);
        assert_eq!(staged.weekly_local_points.len(), 2);
        assert!(store.staged_force_full_merge);
    }

    #[test]
    fn explicitly_staged_full_observation_persists_old_backfill_buckets() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let observed_at = at(2026, 7, 28, 12, 5, 0);
        let recent_start = at(2026, 7, 28, 12, 0, 0);
        let old_start = at(2026, 7, 8, 12, 0, 0);
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);

        store.stage(&HistoryObservation {
            observed_at,
            half_hour_buckets: vec![local_bucket(recent_start, observed_at, 10, 100)],
            ..HistoryObservation::default()
        });
        assert!(store.flush_staged().unwrap().is_some());
        assert_eq!(store.last_full_merge_at, Some(observed_at));

        let mut old_bucket = local_bucket(
            old_start,
            old_start + Duration::minutes(LOCAL_BUCKET_MINUTES),
            20,
            200,
        );
        old_bucket.project_groups = vec![LocalProjectUsageGroup {
            thread_id: "old-thread".to_string(),
            project_id: Some("old-project".to_string()),
            project_label: Some("old project".to_string()),
            token_usage: usage(20),
            estimated_cost_units: 200,
            api_long_context_extra_cost_units: Some(0),
            call_count: 1,
            ..LocalProjectUsageGroup::default()
        }];
        store.stage_full_observation(&HistoryObservation {
            observed_at: observed_at + Duration::minutes(1),
            half_hour_buckets: vec![old_bucket],
            ..HistoryObservation::default()
        });
        assert!(store.flush_staged().unwrap().is_some());

        let mut persisted = HistoryStore::new(history_root, &codex_home);
        let history = persisted.load_since(old_start - Duration::minutes(1));
        assert_eq!(history.half_hour_buckets.len(), 2);
        let old_bucket = history
            .half_hour_buckets
            .iter()
            .find(|bucket| bucket.starts_at == old_start)
            .unwrap();
        assert_eq!(old_bucket.project_groups.len(), 1);
        assert_eq!(
            old_bucket.project_groups[0].project_label.as_deref(),
            Some("old project")
        );
    }

    #[test]
    fn summary_backfill_marker_is_namespace_scoped_and_revision_checked() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let completed_at = at(2026, 7, 28, 12, 5, 0);
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);

        store
            .mark_summary_backfill_attempt(completed_at, true)
            .unwrap();
        let mut reloaded = HistoryStore::new(history_root.clone(), &codex_home);
        assert_eq!(
            reloaded
                .load_since(completed_at - Duration::days(31))
                .summary_backfill_attempted_at,
            Some(completed_at)
        );
        assert_eq!(
            reloaded
                .load_since(completed_at - Duration::days(31))
                .summary_backfill_attempt_complete,
            Some(true)
        );

        store
            .mark_summary_backfill_attempt(completed_at + Duration::minutes(1), false)
            .unwrap();
        let mut partial = HistoryStore::new(history_root.clone(), &codex_home);
        let partial = partial.load_since(completed_at - Duration::days(31));
        assert_eq!(partial.summary_backfill_attempt_complete, Some(false));
        assert!(partial.warnings.is_empty());

        let other_codex_home = directory.path().join("other-codex");
        let mut other = HistoryStore::new(history_root.clone(), &other_codex_home);
        assert_eq!(
            other
                .load_since(completed_at - Duration::days(31))
                .summary_backfill_attempted_at,
            None
        );

        let marker_path = store
            .namespace_dir()
            .unwrap()
            .join(SUMMARY_BACKFILL_MARKER_FILE);
        let mut marker: SummaryBackfillMarker =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker.estimator_revision = marker.estimator_revision.saturating_sub(1);
        let mut contents = serde_json::to_vec_pretty(&marker).unwrap();
        contents.push(b'\n');
        write_private_atomically(&marker_path, &contents).unwrap();

        let mut outdated = HistoryStore::new(history_root, &codex_home);
        assert_eq!(
            outdated
                .load_since(completed_at - Duration::days(31))
                .summary_backfill_attempted_at,
            None
        );
    }

    #[test]
    fn partial_summary_backfill_marker_is_not_a_global_history_warning() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let completed_at = at(2026, 7, 28, 12, 5, 0);
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);
        store
            .mark_summary_backfill_attempt(completed_at, false)
            .unwrap();

        let history = HistoryStore::new(history_root, &codex_home)
            .load_since(completed_at - Duration::days(31));
        assert_eq!(history.summary_backfill_attempt_complete, Some(false));
        assert!(history.warnings.is_empty());
    }

    #[test]
    fn staged_delta_is_persisted_after_crossing_the_recent_overlap_boundary() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let observed_at = at(2026, 7, 28, 12, 0, 0);
        let bucket_start = observed_at - Duration::minutes(60);
        let edge_time = bucket_start + Duration::minutes(15);
        let reset = observed_at + Duration::days(7);
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);
        store.last_full_merge_at = Some(observed_at - Duration::minutes(5));

        store.stage(&HistoryObservation {
            observed_at,
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(bucket_start, edge_time, 10, 100)],
            weekly_local_points: vec![weekly_point(edge_time, reset, 10, 100)],
        });
        store.stage(&HistoryObservation {
            observed_at: observed_at + Duration::minutes(15) + Duration::seconds(30),
            ..HistoryObservation::default()
        });
        assert!(store.flush_staged().unwrap().is_some());

        let mut persisted = HistoryStore::new(history_root, &codex_home);
        let data = persisted.load_since(bucket_start - Duration::minutes(1));
        assert_eq!(data.half_hour_buckets.len(), 1);
        assert_eq!(data.weekly_local_points.len(), 1);
        assert_eq!(data.half_hour_buckets[0].starts_at, bucket_start);
        assert_eq!(data.weekly_local_points[0].observed_at, edge_time);
    }

    #[test]
    fn forced_staged_flush_persists_before_the_batch_deadline() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let since = starts_at - Duration::days(1);
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);
        store.stage(&HistoryObservation {
            observed_at: starts_at + Duration::minutes(5),
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(
                starts_at,
                starts_at + Duration::minutes(5),
                10,
                100,
            )],
            weekly_local_points: Vec::new(),
        });

        assert!(store.flush_staged().unwrap().is_some());
        assert!(store.staged_observation.is_none());
        let mut persisted = HistoryStore::new(history_root, &codex_home);
        assert_eq!(
            persisted.load_since(since).half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );
    }

    #[test]
    fn failed_staged_flush_keeps_live_data_and_debounces_retries() {
        let directory = tempdir().unwrap();
        let unusable_root = directory.path().join("state-file");
        fs::write(&unusable_root, b"not a directory").unwrap();
        let codex_home = directory.path().join("codex");
        let observed_at = at(2026, 7, 28, 12, 5, 0);
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let interval = StdDuration::from_secs(30);
        let first_attempt = Instant::now();
        let mut store = HistoryStore::new(unusable_root, &codex_home);
        store.stage(&HistoryObservation {
            observed_at,
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(starts_at, observed_at, 10, 100)],
            weekly_local_points: Vec::new(),
        });

        assert!(
            store
                .flush_staged_if_due_at(interval, first_attempt)
                .is_err()
        );
        assert!(store.staged_observation.is_some());
        assert!(
            store
                .flush_staged_if_due_at(interval, first_attempt + StdDuration::from_secs(1),)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .load_since_with_staged(starts_at - Duration::hours(1))
                .half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );
    }

    #[test]
    fn staged_flush_preserves_reset_transitions_and_utc_day_shards() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let before_midnight = at(2026, 7, 28, 23, 59, 50);
        let midnight = at(2026, 7, 29, 0, 0, 0);
        let after_midnight = at(2026, 7, 29, 0, 0, 10);
        let old_reset = midnight;
        let new_reset = old_reset + Duration::days(7);
        let mut store = HistoryStore::new(history_root, &codex_home);

        store.stage(&HistoryObservation {
            observed_at: before_midnight,
            quota_points: vec![quota_point(before_midnight, old_reset, 90.0)],
            half_hour_buckets: vec![local_bucket(at(2026, 7, 28, 23, 30, 0), midnight, 10, 100)],
            weekly_local_points: vec![weekly_point(before_midnight, old_reset, 10, 100)],
        });
        store.stage(&HistoryObservation {
            observed_at: after_midnight,
            quota_points: vec![quota_point(after_midnight, new_reset, 1.0)],
            half_hour_buckets: vec![local_bucket(midnight, after_midnight, 20, 200)],
            weekly_local_points: vec![weekly_point(after_midnight, new_reset, 20, 200)],
        });
        let staged = store.staged_observation.as_ref().unwrap();
        assert_eq!(staged.quota_points.len(), 2);
        assert_eq!(staged.half_hour_buckets.len(), 2);
        assert_eq!(staged.weekly_local_points.len(), 2);

        assert!(store.flush_staged().unwrap().is_some());
        let namespace_dir = store.namespace_dir().unwrap();
        assert!(shard_path(namespace_dir, before_midnight.date_naive()).is_file());
        assert!(shard_path(namespace_dir, after_midnight.date_naive()).is_file());
        let data = store.load_since(before_midnight - Duration::hours(1));
        assert_eq!(data.quota_points.len(), 2);
        assert_eq!(data.half_hour_buckets.len(), 2);
        assert_eq!(data.weekly_local_points.len(), 2);
        assert_eq!(
            data.quota_points
                .iter()
                .map(|point| point.resets_at)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([old_reset, new_reset])
        );
    }

    #[test]
    fn stale_reload_merges_external_writes_with_staged_evidence() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let since = at(2026, 7, 28, 0, 0, 0);
        let first_start = at(2026, 7, 28, 12, 0, 0);
        let second_start = first_start + Duration::minutes(30);
        let third_start = second_start + Duration::minutes(30);
        let mut store = HistoryStore::new(history_root.clone(), &codex_home);
        store
            .record(&HistoryObservation {
                observed_at: first_start + Duration::minutes(10),
                quota_points: Vec::new(),
                half_hour_buckets: vec![local_bucket(
                    first_start,
                    first_start + Duration::minutes(10),
                    50,
                    500,
                )],
                weekly_local_points: Vec::new(),
            })
            .unwrap();
        assert_eq!(store.load_since(since).half_hour_buckets.len(), 1);

        let mut lower_quality =
            local_bucket(first_start, first_start + Duration::minutes(20), 20, 200);
        lower_quality.partial_reasons = vec!["rollout_scan_incomplete".to_string()];
        store.stage(&HistoryObservation {
            observed_at: second_start + Duration::minutes(10),
            quota_points: Vec::new(),
            half_hour_buckets: vec![
                lower_quality,
                local_bucket(second_start, second_start + Duration::minutes(10), 10, 100),
            ],
            weekly_local_points: Vec::new(),
        });

        let mut external = HistoryStore::new(history_root.clone(), &codex_home);
        external
            .record(&HistoryObservation {
                observed_at: third_start + Duration::minutes(10),
                quota_points: Vec::new(),
                half_hour_buckets: vec![local_bucket(
                    third_start,
                    third_start + Duration::minutes(10),
                    30,
                    300,
                )],
                weekly_local_points: Vec::new(),
            })
            .unwrap();
        store.cache_loaded_at =
            Some(Instant::now() - HISTORY_READ_CACHE_TTL - StdDuration::from_secs(1));

        let data = store
            .reload_since_if_stale_with_staged(since)
            .expect("the expired cache must reload");
        assert_eq!(data.half_hour_buckets.len(), 3);
        assert_eq!(data.half_hour_buckets[0].token_usage.total_tokens, 50);
        assert_eq!(data.half_hour_buckets[1].token_usage.total_tokens, 10);
        assert_eq!(data.half_hour_buckets[2].token_usage.total_tokens, 30);

        assert!(store.flush_staged().unwrap().is_some());
        let mut persisted = HistoryStore::new(history_root, &codex_home);
        let data = persisted.load_since(since);
        assert_eq!(data.half_hour_buckets.len(), 3);
        assert_eq!(data.half_hour_buckets[0].token_usage.total_tokens, 50);
    }

    #[test]
    fn staged_open_bucket_closes_and_clock_rollback_forces_a_full_merge() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let first_at = starts_at + Duration::minutes(10);
        store.stage(&HistoryObservation {
            observed_at: first_at,
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(starts_at, first_at, 10, 100)],
            weekly_local_points: Vec::new(),
        });
        store.stage(&HistoryObservation {
            observed_at: starts_at + Duration::minutes(15),
            quota_points: Vec::new(),
            half_hour_buckets: vec![local_bucket(
                starts_at,
                starts_at + Duration::minutes(15),
                10,
                100,
            )],
            weekly_local_points: Vec::new(),
        });
        assert_eq!(
            store.staged_observation.as_ref().unwrap().half_hour_buckets[0].sampled_at,
            starts_at + Duration::minutes(15)
        );
        store.flush_staged().unwrap();
        assert_eq!(
            store.last_full_merge_at,
            Some(starts_at + Duration::minutes(15))
        );

        store.stage(&HistoryObservation {
            observed_at: starts_at + Duration::minutes(40),
            ..HistoryObservation::default()
        });
        store.stage(&HistoryObservation {
            observed_at: starts_at - Duration::minutes(1),
            ..HistoryObservation::default()
        });
        assert!(store.staged_force_full_merge);
        store.flush_staged().unwrap();
        assert_eq!(
            store.last_full_merge_at,
            Some(starts_at + Duration::minutes(40))
        );
    }

    #[test]
    fn unchanged_open_bucket_and_same_slot_quota_do_not_rewrite_the_shard() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let observed_at = at(2026, 7, 28, 12, 7, 0);
        let reset = observed_at + Duration::days(3);
        let first = HistoryObservation {
            observed_at,
            quota_points: vec![quota_point(observed_at, reset, 40.0)],
            half_hour_buckets: vec![local_bucket(
                at(2026, 7, 28, 12, 0, 0),
                observed_at,
                10,
                100,
            )],
            weekly_local_points: Vec::new(),
        };
        assert_eq!(store.record(&first).unwrap().shards_written, 1);

        let later = observed_at + Duration::seconds(45);
        let unchanged = HistoryObservation {
            observed_at: later,
            quota_points: vec![quota_point(later, reset, 40.0)],
            half_hour_buckets: vec![local_bucket(at(2026, 7, 28, 12, 0, 0), later, 10, 100)],
            weekly_local_points: Vec::new(),
        };
        let report = store.record(&unchanged).unwrap();
        assert_eq!(report.shards_written, 0);
        assert_eq!(report.shards_skipped, 1);
    }

    #[test]
    fn unchanged_half_hour_bucket_writes_only_when_it_closes() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let mut open = local_bucket(starts_at, starts_at + Duration::minutes(10), 0, 0);
        open.call_count = 0;
        let mut buckets = vec![open.clone()];

        let mut later_open = open.clone();
        later_open.sampled_at = starts_at + Duration::minutes(12);
        assert!(!upsert_half_hour_bucket(&mut buckets, later_open));
        assert_eq!(buckets[0].sampled_at, open.sampled_at);

        let mut closed = open;
        closed.sampled_at = closed.ends_at;
        assert!(upsert_half_hour_bucket(&mut buckets, closed.clone()));
        assert_eq!(buckets, vec![closed.clone()]);
        assert!(!upsert_half_hour_bucket(&mut buckets, closed));
    }

    #[test]
    fn newer_coverage_partial_closes_equal_exact_bucket_without_later_regression() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let exact_open = local_bucket(starts_at, starts_at + Duration::minutes(7), 10, 100);
        let mut partial_closed = exact_open.clone();
        partial_closed.sampled_at = partial_closed.ends_at;
        partial_closed.partial_reasons = vec![
            "coverage_starts_within_local_bucket".to_string(),
            "rollout_local_coverage_unverified".to_string(),
        ];
        let mut buckets = vec![exact_open.clone()];

        assert_eq!(bucket_collection_issue_count(&partial_closed), 2);
        assert!(upsert_half_hour_bucket(
            &mut buckets,
            partial_closed.clone()
        ));
        assert_eq!(buckets, vec![partial_closed.clone()]);

        let mut older_exact = exact_open;
        older_exact.sampled_at = starts_at + Duration::minutes(10);
        assert!(!upsert_half_hour_bucket(&mut buckets, older_exact));
        assert_eq!(buckets, vec![partial_closed]);

        let strong_exact = local_bucket(starts_at, starts_at + Duration::minutes(7), 100, 1_000);
        let mut lower_partial = local_bucket(starts_at, starts_at + Duration::minutes(15), 80, 800);
        lower_partial.partial_reasons = vec!["rollout_local_coverage_unverified".to_string()];
        let mut stronger_buckets = vec![strong_exact.clone()];
        assert!(upsert_half_hour_bucket(
            &mut stronger_buckets,
            lower_partial.clone()
        ));
        assert_eq!(stronger_buckets[0].token_usage, strong_exact.token_usage);
        assert_eq!(stronger_buckets[0].estimated_cost_units, 1_000);
        assert_eq!(stronger_buckets[0].sampled_at, lower_partial.ends_at);
        assert_eq!(
            stronger_buckets[0].partial_reasons,
            lower_partial.partial_reasons
        );
    }

    #[test]
    fn project_breakdown_backfills_equal_legacy_bucket_without_later_regression() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let sampled_at = starts_at + Duration::minutes(LOCAL_BUCKET_MINUTES);
        let mut legacy = local_bucket(starts_at, sampled_at, 300, 3_000);
        legacy.call_count = 2;
        legacy.project_breakdown_revision = 0;
        legacy.api_pricing_catalog_revision = 0;
        let mut with_breakdown = legacy.clone();
        with_breakdown.project_breakdown_revision = HISTORY_PROJECT_BREAKDOWN_REVISION;
        with_breakdown.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION;
        with_breakdown.project_groups = vec![
            LocalProjectUsageGroup {
                thread_id: "root".to_string(),
                project_id: Some("project-alpha".to_string()),
                project_label: Some("alpha".to_string()),
                token_usage: usage(100),
                estimated_cost_units: 1_000,
                api_long_context_extra_cost_units: Some(0),
                call_count: 1,
                ..LocalProjectUsageGroup::default()
            },
            LocalProjectUsageGroup {
                thread_id: "child".to_string(),
                parent_thread_id: Some("root".to_string()),
                project_id: Some("project-alpha".to_string()),
                project_label: Some("alpha".to_string()),
                token_usage: usage(200),
                estimated_cost_units: 2_000,
                api_long_context_extra_cost_units: Some(0),
                call_count: 1,
                ..LocalProjectUsageGroup::default()
            },
        ];
        let mut buckets = vec![legacy.clone()];

        assert!(upsert_half_hour_bucket(
            &mut buckets,
            with_breakdown.clone()
        ));
        assert_eq!(buckets, vec![with_breakdown.clone()]);
        assert!(!upsert_half_hour_bucket(&mut buckets, legacy));
        assert_eq!(buckets, vec![with_breakdown]);

        let mut legacy_zero = local_bucket(starts_at, sampled_at, 0, 0);
        legacy_zero.call_count = 0;
        legacy_zero.project_breakdown_revision = 0;
        legacy_zero.api_pricing_catalog_revision = 0;
        let mut current_zero = legacy_zero.clone();
        current_zero.project_breakdown_revision = HISTORY_PROJECT_BREAKDOWN_REVISION;
        current_zero.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION;
        let mut zero_buckets = vec![legacy_zero];
        assert!(upsert_half_hour_bucket(
            &mut zero_buckets,
            current_zero.clone()
        ));
        assert_eq!(zero_buckets, [current_zero]);
    }

    #[test]
    fn project_backfill_marks_a_grafted_partial_scan_as_incomplete() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let sampled_at = starts_at + Duration::minutes(LOCAL_BUCKET_MINUTES);
        let mut legacy = local_bucket(starts_at, sampled_at, 300, 3_000);
        legacy.call_count = 2;
        legacy.project_breakdown_revision = 0;
        legacy.api_pricing_catalog_revision = 0;
        let mut incoming = legacy.clone();
        incoming.project_breakdown_revision = HISTORY_PROJECT_BREAKDOWN_REVISION;
        incoming.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION;
        incoming.partial_reasons = vec!["rollout_scan_incomplete".to_string()];
        incoming.project_groups = vec![LocalProjectUsageGroup {
            thread_id: "thread".to_string(),
            project_id: Some("project".to_string()),
            project_label: Some("project".to_string()),
            token_usage: usage(300),
            estimated_cost_units: 3_000,
            api_long_context_extra_cost_units: Some(0),
            call_count: 2,
            ..LocalProjectUsageGroup::default()
        }];
        let mut buckets = vec![legacy];

        assert!(upsert_half_hour_bucket(&mut buckets, incoming));
        assert_eq!(
            buckets[0].partial_reasons,
            ["rollout_scan_incomplete".to_string()]
        );
        assert_eq!(
            buckets[0].project_breakdown_revision,
            HISTORY_PROJECT_BREAKDOWN_REVISION
        );
        assert_eq!(
            buckets[0].api_pricing_catalog_revision,
            API_PRICING_CATALOG_REVISION
        );
        assert_eq!(buckets[0].project_groups.len(), 1);
    }

    #[test]
    fn same_revision_project_breakdown_accepts_strict_enrichment_without_regression() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let sampled_at = starts_at + Duration::minutes(10);
        let mut legacy = local_bucket(starts_at, sampled_at, 300, 3_000);
        legacy.call_count = 2;
        legacy.project_breakdown_revision = 0;
        legacy.api_pricing_catalog_revision = 0;

        let mut sparse = legacy.clone();
        sparse.project_breakdown_revision = HISTORY_PROJECT_BREAKDOWN_REVISION;
        sparse.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION;
        sparse.partial_reasons = vec!["rollout_scan_incomplete".to_string()];
        sparse.project_groups = vec![LocalProjectUsageGroup {
            thread_id: "thread".to_string(),
            title: Some("Untitled task".to_string()),
            token_usage: usage(300),
            estimated_cost_units: 3_000,
            call_count: 2,
            ..LocalProjectUsageGroup::default()
        }];
        let mut buckets = vec![legacy];
        assert!(upsert_half_hour_bucket(&mut buckets, sparse.clone()));

        let mut richer = sparse.clone();
        richer.partial_reasons.clear();
        let group = &mut richer.project_groups[0];
        group.project_id = Some("project-alpha".to_string());
        group.project_label = Some("alpha".to_string());
        group.title = Some("Project history backfill".to_string());
        group.source = Some("cli".to_string());
        group.api_long_context_extra_cost_units = Some(0);
        group.api_equivalent_cost = ApiCostAmount {
            minimum_pico_usd: crate::domain::PicoUsd::new(250),
            maximum_pico_usd: crate::domain::PicoUsd::new(300),
            observed_samples: 2,
            priced_samples: 2,
            observed_tokens: 300,
            priced_tokens: 300,
        };

        assert!(upsert_half_hour_bucket(&mut buckets, richer.clone()));
        assert_eq!(buckets[0].project_groups, richer.project_groups);
        assert!(buckets[0].partial_reasons.is_empty());
        assert!(!upsert_half_hour_bucket(&mut buckets, sparse.clone()));
        assert_eq!(buckets[0].project_groups, richer.project_groups);

        let mut later_sparse = sparse;
        later_sparse.partial_reasons.clear();
        later_sparse.sampled_at = starts_at + Duration::minutes(12);
        assert!(upsert_half_hour_bucket(&mut buckets, later_sparse));
        assert_eq!(buckets[0].sampled_at, starts_at + Duration::minutes(12));
        assert_eq!(buckets[0].project_groups, richer.project_groups);
    }

    #[test]
    fn lower_quality_writer_cannot_replace_a_complete_half_hour_bucket() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let mut complete = local_bucket(starts_at, starts_at + Duration::minutes(10), 50, 500);
        complete.call_count = 5;
        let mut partial = local_bucket(starts_at, starts_at + Duration::minutes(12), 20, 200);
        partial.call_count = 2;
        partial.partial_reasons = vec!["rollout_scan_incomplete".to_string()];
        let mut buckets = vec![complete.clone()];

        assert!(!upsert_half_hour_bucket(&mut buckets, partial));
        assert_eq!(buckets, vec![complete]);

        let mut fuller = local_bucket(starts_at, starts_at + Duration::minutes(14), 70, 700);
        fuller.call_count = 7;
        assert!(upsert_half_hour_bucket(&mut buckets, fuller.clone()));
        assert_eq!(buckets, vec![fuller]);
    }

    #[test]
    fn later_evidence_is_not_blocked_by_estimator_partial_reasons() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let mut existing = local_bucket(starts_at, starts_at + Duration::minutes(10), 50, 500);
        existing.call_count = 5;
        let mut incoming = local_bucket(starts_at, starts_at + Duration::minutes(14), 70, 700);
        incoming.call_count = 7;
        incoming.partial_reasons = vec!["unpriced_model_rate_fallback".to_string()];
        let mut buckets = vec![existing];

        assert!(upsert_half_hour_bucket(&mut buckets, incoming.clone()));
        assert_eq!(buckets, vec![incoming]);
    }

    #[test]
    fn newer_estimator_revision_replaces_equivalent_bucket_evidence_at_a_lower_weight() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let mut older = local_bucket(starts_at, starts_at + Duration::minutes(10), 50, 500);
        older.estimator_revision = 1;
        older.call_count = 5;
        let mut newer = local_bucket(starts_at, starts_at + Duration::minutes(12), 50, 100);
        newer.estimator_revision = 2;
        newer.call_count = 5;
        let mut buckets = vec![older];

        assert!(upsert_half_hour_bucket(&mut buckets, newer.clone()));
        assert_eq!(buckets, vec![newer.clone()]);

        let mut richer_but_older =
            local_bucket(starts_at, starts_at + Duration::minutes(14), 70, 700);
        richer_but_older.estimator_revision = 1;
        richer_but_older.call_count = 7;
        assert!(!upsert_half_hour_bucket(&mut buckets, richer_but_older));
        assert_eq!(buckets, vec![newer.clone()]);

        let mut poorer_but_newer =
            local_bucket(starts_at, starts_at + Duration::minutes(14), 40, 80);
        poorer_but_newer.estimator_revision = 3;
        poorer_but_newer.call_count = 4;
        assert!(!upsert_half_hour_bucket(&mut buckets, poorer_but_newer));
        assert_eq!(buckets, vec![newer]);
    }

    #[test]
    fn later_weekly_point_cannot_drop_on_a_lower_quality_scan() {
        let reset = at(2026, 7, 31, 12, 17, 0);
        let first_at = at(2026, 7, 28, 12, 5, 0);
        let complete = weekly_point(first_at, reset, 50, 500);
        let mut truncated = weekly_point(first_at + Duration::minutes(6), reset, 20, 200);
        truncated.partial_reasons = vec!["rollout_scan_incomplete".to_string()];
        let mut points = vec![complete.clone()];

        assert!(!upsert_weekly_local_point(&mut points, truncated));
        assert_eq!(points, vec![complete.clone()]);

        let mut richer = weekly_point(first_at + Duration::minutes(6), reset, 70, 700);
        richer.partial_reasons = vec!["unpriced_model_rate_fallback".to_string()];
        assert!(upsert_weekly_local_point(&mut points, richer.clone()));
        assert_eq!(points, vec![complete, richer]);
    }

    #[test]
    fn newer_weekly_coverage_partial_replaces_or_follows_equal_exact_evidence() {
        let reset = at(2026, 7, 31, 12, 17, 0);
        let exact = weekly_point(at(2026, 7, 28, 12, 11, 0), reset, 50, 500);
        let mut same_slot_partial = exact.clone();
        same_slot_partial.observed_at = at(2026, 7, 28, 12, 12, 0);
        same_slot_partial.partial_reasons = vec!["rollout_local_coverage_unverified".to_string()];
        let mut same_slot = vec![exact.clone()];

        assert!(upsert_weekly_local_point(
            &mut same_slot,
            same_slot_partial.clone()
        ));
        assert_eq!(same_slot, vec![same_slot_partial.clone()]);
        assert!(!upsert_weekly_local_point(&mut same_slot, exact));
        assert_eq!(same_slot, vec![same_slot_partial]);

        let exact_plateau = weekly_point(at(2026, 7, 28, 12, 1, 0), reset, 50, 500);
        let mut later_partial = exact_plateau.clone();
        later_partial.observed_at = at(2026, 7, 28, 12, 7, 0);
        later_partial.token_usage = usage(20);
        later_partial.estimated_cost_units = 200;
        later_partial.partial_reasons = vec!["coverage_starts_within_local_bucket".to_string()];
        let mut cross_slot = vec![exact_plateau];
        assert!(upsert_weekly_local_point(
            &mut cross_slot,
            later_partial.clone()
        ));
        let merged = cross_slot.last().unwrap();
        assert_eq!(merged.observed_at, later_partial.observed_at);
        assert_eq!(merged.token_usage.total_tokens, 50);
        assert_eq!(merged.estimated_cost_units, 500);
        assert_eq!(merged.partial_reasons, later_partial.partial_reasons);

        let older_exact_same_slot = weekly_point(at(2026, 7, 28, 12, 6, 0), reset, 50, 500);
        assert!(!upsert_weekly_local_point(
            &mut cross_slot,
            older_exact_same_slot
        ));
        assert_eq!(
            cross_slot.last().unwrap().partial_reasons,
            later_partial.partial_reasons
        );
    }

    #[test]
    fn newer_estimator_revision_replaces_equivalent_weekly_evidence_at_a_lower_weight() {
        let reset = at(2026, 7, 31, 12, 17, 0);
        let first_at = at(2026, 7, 28, 12, 6, 0);
        let mut older = weekly_point(first_at, reset, 50, 500);
        older.estimator_revision = 1;
        older.call_count = 5;
        let mut newer = weekly_point(first_at + Duration::minutes(1), reset, 50, 100);
        newer.estimator_revision = 2;
        newer.call_count = 5;
        let mut points = vec![older];

        assert!(upsert_weekly_local_point(&mut points, newer.clone()));
        assert_eq!(points, vec![newer.clone()]);

        let mut richer_but_older = weekly_point(first_at + Duration::minutes(2), reset, 70, 700);
        richer_but_older.estimator_revision = 1;
        richer_but_older.call_count = 7;
        assert!(!upsert_weekly_local_point(&mut points, richer_but_older));
        assert_eq!(points, vec![newer.clone()]);

        let mut poorer_but_newer = weekly_point(first_at + Duration::minutes(3), reset, 40, 80);
        poorer_but_newer.estimator_revision = 3;
        poorer_but_newer.call_count = 4;
        assert!(!upsert_weekly_local_point(&mut points, poorer_but_newer));
        assert_eq!(points, vec![newer]);
    }

    #[test]
    fn cross_slot_weekly_plateaus_keep_the_newer_estimator_revision() {
        let reset = at(2026, 7, 31, 12, 17, 0);
        let mut older = weekly_point(at(2026, 7, 28, 12, 1, 0), reset, 50, 500);
        older.estimator_revision = 1;
        older.call_count = 5;
        let mut newer = weekly_point(at(2026, 7, 28, 12, 7, 0), reset, 50, 100);
        newer.estimator_revision = 2;
        newer.call_count = 5;
        let mut points = vec![older.clone()];

        assert!(upsert_weekly_local_point(&mut points, newer.clone()));
        assert_eq!(points, vec![older, newer.clone()]);

        let mut older_downgrade = weekly_point(at(2026, 7, 28, 12, 12, 0), reset, 50, 500);
        older_downgrade.estimator_revision = 1;
        older_downgrade.call_count = 5;
        assert!(!upsert_weekly_local_point(&mut points, older_downgrade));
        assert_eq!(points.last(), Some(&newer));
    }

    #[test]
    fn weekly_plateaus_keep_boundaries_and_compress_open_samples() {
        let reset = at(2026, 7, 31, 12, 17, 0);
        let first_at = at(2026, 7, 28, 12, 40, 0);
        let mut points = vec![weekly_point(first_at, reset, 50, 500)];

        let first_boundary = weekly_point(at(2026, 7, 28, 13, 0, 0), reset, 50, 500);
        assert!(upsert_weekly_local_point(
            &mut points,
            first_boundary.clone()
        ));
        assert!(!upsert_weekly_local_point(
            &mut points,
            weekly_point(at(2026, 7, 28, 13, 3, 0), reset, 50, 500)
        ));

        let second_boundary = weekly_point(at(2026, 7, 28, 13, 30, 0), reset, 50, 500);
        assert!(upsert_weekly_local_point(
            &mut points,
            second_boundary.clone()
        ));
        assert_eq!(
            points
                .iter()
                .map(|point| point.observed_at)
                .collect::<Vec<_>>(),
            [
                first_at,
                first_boundary.observed_at,
                second_boundary.observed_at
            ]
        );

        let mut reverse_order = vec![weekly_point(at(2026, 7, 28, 14, 3, 0), reset, 50, 500)];
        let boundary = weekly_point(at(2026, 7, 28, 14, 0, 0), reset, 50, 500);
        assert!(upsert_weekly_local_point(
            &mut reverse_order,
            boundary.clone()
        ));
        assert!(reverse_order.contains(&boundary));
    }

    #[test]
    fn future_shard_disables_writes_without_overwriting_it() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let namespace_dir = store.namespace_dir().unwrap().to_path_buf();
        create_private_directory(&namespace_dir).unwrap();
        let now = at(2026, 7, 28, 12, 0, 0);
        let path = shard_path(&namespace_dir, now.date_naive());
        let future = format!(
            "{{\"formatVersion\":{},\"namespace\":\"{}\"}}",
            HISTORY_FORMAT_VERSION + 1,
            store.namespace()
        );
        fs::write(&path, &future).unwrap();

        let data = store.load_since(now - Duration::days(1));
        assert!(data.read_only);
        assert!(
            data.warnings
                .iter()
                .any(|warning| warning.contains("future"))
        );
        let report = store
            .record(&HistoryObservation {
                observed_at: now,
                ..HistoryObservation::default()
            })
            .unwrap();
        assert!(report.read_only);
        assert_eq!(fs::read_to_string(path).unwrap(), future);
    }

    #[test]
    fn future_metric_revision_disables_writes_without_overwriting_it() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let namespace_dir = store.namespace_dir().unwrap().to_path_buf();
        create_private_directory(&namespace_dir).unwrap();
        let now = at(2026, 7, 28, 12, 0, 0);
        let path = shard_path(&namespace_dir, now.date_naive());
        let future = HistoryShard {
            format_version: HISTORY_FORMAT_VERSION,
            metric_revision: HISTORY_METRIC_REVISION + 1,
            namespace: store.namespace().to_string(),
            utc_day: now.date_naive(),
            quota_points: Vec::new(),
            half_hour_buckets: Vec::new(),
            weekly_local_points: Vec::new(),
        };
        write_shard_atomically(&path, &future).unwrap();
        let original = fs::read(&path).unwrap();

        let data = store.load_since(now - Duration::days(1));
        assert!(data.read_only);
        assert!(
            data.warnings
                .iter()
                .any(|warning| warning.contains("future history metric revision"))
        );
        let report = store
            .record(&HistoryObservation {
                observed_at: now,
                ..HistoryObservation::default()
            })
            .unwrap();
        assert!(report.read_only);
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn legacy_shard_keeps_quota_but_drops_unproven_local_history() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let namespace_dir = store.namespace_dir().unwrap().to_path_buf();
        create_private_directory(&namespace_dir).unwrap();
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let reset = at(2026, 7, 31, 12, 0, 0);
        let path = shard_path(&namespace_dir, starts_at.date_naive());
        let mut legacy_bucket = local_bucket(starts_at, starts_at + Duration::minutes(20), 30, 300);
        legacy_bucket.ends_at = starts_at + Duration::minutes(30);
        write_shard_atomically(
            &path,
            &HistoryShard {
                format_version: LEGACY_HISTORY_FORMAT_VERSION,
                metric_revision: 1,
                namespace: store.namespace().to_string(),
                utc_day: starts_at.date_naive(),
                quota_points: vec![quota_point(starts_at, reset, 40.0)],
                half_hour_buckets: vec![legacy_bucket],
                weekly_local_points: vec![weekly_point(starts_at, reset, 30, 300)],
            },
        )
        .unwrap();

        let migrated_view = store.load_since(starts_at - Duration::minutes(1));
        assert_eq!(migrated_view.quota_points.len(), 1);
        assert!(migrated_view.weekly_local_points.is_empty());
        assert!(migrated_view.half_hour_buckets.is_empty());

        let current_bucket = local_bucket(starts_at, starts_at + Duration::minutes(15), 10, 100);
        let report = store
            .record(&HistoryObservation {
                observed_at: starts_at + Duration::minutes(20),
                quota_points: Vec::new(),
                half_hour_buckets: vec![current_bucket.clone()],
                weekly_local_points: Vec::new(),
            })
            .unwrap();
        assert_eq!(report.shards_written, 1);

        let persisted = match read_shard(&path, store.namespace(), starts_at.date_naive()) {
            ShardRead::Current { shard, migrated } => {
                assert!(!migrated);
                shard
            }
            _ => panic!("migrated shard should be readable"),
        };
        assert_eq!(persisted.format_version, HISTORY_FORMAT_VERSION);
        assert_eq!(persisted.metric_revision, HISTORY_METRIC_REVISION);
        assert_eq!(persisted.quota_points.len(), 1);
        assert!(persisted.weekly_local_points.is_empty());
        assert_eq!(persisted.half_hour_buckets, vec![current_bucket.clone()]);

        let repeated = store
            .record(&HistoryObservation {
                observed_at: starts_at + Duration::minutes(20),
                quota_points: Vec::new(),
                half_hour_buckets: vec![current_bucket],
                weekly_local_points: Vec::new(),
            })
            .unwrap();
        assert_eq!(repeated.shards_written, 0);
        assert_eq!(repeated.shards_skipped, 1);
    }

    #[test]
    fn metric_two_migration_keeps_call_evidence_but_drops_zeros_and_weekly_points() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let namespace_dir = store.namespace_dir().unwrap().to_path_buf();
        create_private_directory(&namespace_dir).unwrap();
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let reset = at(2026, 7, 31, 12, 0, 0);
        let path = shard_path(&namespace_dir, starts_at.date_naive());

        let mut released = local_bucket(starts_at, starts_at + Duration::minutes(15), 10, 100);
        released.estimator_revision = 3;
        released.api_long_context_extra_cost_units = None;
        let mut ambiguous = local_bucket(
            starts_at + Duration::minutes(15),
            starts_at + Duration::minutes(30),
            20,
            400,
        );
        ambiguous.estimator_revision = 4;
        ambiguous.api_long_context_extra_cost_units = None;
        let mut released_weekly = weekly_point(starts_at, reset, 10, 100);
        released_weekly.estimator_revision = 3;
        released_weekly.api_long_context_extra_cost_units = None;
        let mut ambiguous_weekly = weekly_point(starts_at + Duration::minutes(30), reset, 30, 500);
        ambiguous_weekly.estimator_revision = 4;
        ambiguous_weekly.api_long_context_extra_cost_units = None;
        let mut synthetic_zero = local_bucket(
            starts_at + Duration::minutes(30),
            starts_at + Duration::minutes(45),
            0,
            0,
        );
        synthetic_zero.call_count = 0;

        write_shard_atomically(
            &path,
            &HistoryShard {
                format_version: HISTORY_FORMAT_VERSION,
                metric_revision: 2,
                namespace: store.namespace().to_string(),
                utc_day: starts_at.date_naive(),
                quota_points: vec![quota_point(starts_at, reset, 40.0)],
                half_hour_buckets: vec![released.clone(), ambiguous, synthetic_zero],
                weekly_local_points: vec![released_weekly.clone(), ambiguous_weekly],
            },
        )
        .unwrap();

        released
            .partial_reasons
            .push("rollout_local_coverage_unverified".to_string());
        let migrated = store.load_since(starts_at - Duration::minutes(1));
        assert_eq!(migrated.quota_points.len(), 1);
        assert_eq!(migrated.half_hour_buckets, vec![released]);
        assert!(migrated.weekly_local_points.is_empty());
    }

    #[test]
    fn metric_three_migration_removes_unproven_zeros_and_weekly_plateaus() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let namespace_dir = store.namespace_dir().unwrap().to_path_buf();
        create_private_directory(&namespace_dir).unwrap();
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let reset = at(2026, 7, 31, 12, 0, 0);
        let path = shard_path(&namespace_dir, starts_at.date_naive());

        let call_backed = local_bucket(starts_at, starts_at + Duration::minutes(15), 10, 100);
        let mut synthetic_zero = local_bucket(
            starts_at + Duration::minutes(15),
            starts_at + Duration::minutes(30),
            0,
            0,
        );
        synthetic_zero.call_count = 0;
        let mut spark_backed = local_bucket(
            starts_at + Duration::minutes(30),
            starts_at + Duration::minutes(45),
            0,
            0,
        );
        spark_backed.call_count = 0;
        spark_backed.project_groups = vec![LocalProjectUsageGroup {
            thread_id: "spark-thread".to_string(),
            ..LocalProjectUsageGroup::default()
        }];
        let quota = quota_point(starts_at, reset, 40.0);

        write_shard_atomically(
            &path,
            &HistoryShard {
                format_version: HISTORY_FORMAT_VERSION,
                metric_revision: 3,
                namespace: store.namespace().to_string(),
                utc_day: starts_at.date_naive(),
                quota_points: vec![quota.clone()],
                half_hour_buckets: vec![call_backed.clone(), synthetic_zero, spark_backed.clone()],
                weekly_local_points: vec![
                    weekly_point(starts_at + Duration::minutes(30), reset, 10, 100),
                    weekly_point(starts_at + Duration::minutes(60), reset, 10, 100),
                ],
            },
        )
        .unwrap();

        let mut migrated_call_backed = call_backed.clone();
        migrated_call_backed
            .partial_reasons
            .push("rollout_local_coverage_unverified".to_string());
        let mut migrated_spark_backed = spark_backed.clone();
        migrated_spark_backed
            .partial_reasons
            .push("rollout_local_coverage_unverified".to_string());
        let migrated = store.load_since(starts_at - Duration::minutes(1));
        assert_eq!(migrated.quota_points, vec![quota.clone()]);
        assert_eq!(
            migrated.half_hour_buckets,
            vec![migrated_call_backed.clone(), migrated_spark_backed.clone()]
        );
        assert!(migrated.weekly_local_points.is_empty());

        let new_bucket = local_bucket(
            starts_at + Duration::minutes(45),
            starts_at + Duration::minutes(60),
            5,
            50,
        );
        store
            .record(&HistoryObservation {
                observed_at: starts_at + Duration::minutes(60),
                quota_points: Vec::new(),
                half_hour_buckets: vec![new_bucket.clone()],
                weekly_local_points: Vec::new(),
            })
            .unwrap();
        let persisted = match read_shard(&path, store.namespace(), starts_at.date_naive()) {
            ShardRead::Current { shard, migrated } => {
                assert!(!migrated);
                shard
            }
            _ => panic!("migrated metric-three shard should be readable"),
        };
        assert_eq!(persisted.metric_revision, HISTORY_METRIC_REVISION);
        assert_eq!(persisted.quota_points, vec![quota]);
        assert_eq!(
            persisted.half_hour_buckets,
            vec![migrated_call_backed, migrated_spark_backed, new_bucket]
        );
        assert!(persisted.weekly_local_points.is_empty());
    }

    #[test]
    fn normalization_rejects_legacy_width_without_double_counting_current_buckets() {
        let starts_at = at(2026, 7, 28, 12, 0, 0);
        let first = local_bucket(starts_at, starts_at + Duration::minutes(15), 10, 100);
        let second = local_bucket(
            starts_at + Duration::minutes(15),
            starts_at + Duration::minutes(30),
            20,
            200,
        );
        let mut legacy = local_bucket(starts_at, starts_at + Duration::minutes(15), 30, 300);
        legacy.ends_at = starts_at + Duration::minutes(30);
        let mut data = HistoryData {
            half_hour_buckets: vec![legacy, first, second],
            ..HistoryData::default()
        };

        normalize_loaded_data(&mut data);

        assert_eq!(data.half_hour_buckets.len(), 2);
        assert_eq!(
            data.half_hour_buckets
                .iter()
                .map(|bucket| bucket.token_usage.total_tokens)
                .sum::<u64>(),
            30
        );
    }

    #[test]
    fn corrupt_target_is_reported_and_never_replaced() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let mut store = HistoryStore::new(directory.path().join("state"), &codex_home);
        let namespace_dir = store.namespace_dir().unwrap().to_path_buf();
        create_private_directory(&namespace_dir).unwrap();
        let now = at(2026, 7, 28, 12, 0, 0);
        let path = shard_path(&namespace_dir, now.date_naive());
        fs::write(&path, b"not json").unwrap();
        let observation = HistoryObservation {
            observed_at: now,
            quota_points: vec![quota_point(now, now + Duration::days(3), 20.0)],
            half_hour_buckets: Vec::new(),
            weekly_local_points: Vec::new(),
        };
        let report = store.record(&observation).unwrap();
        assert_eq!(report.shards_skipped, 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("malformed"))
        );
        assert_eq!(fs::read(&path).unwrap(), b"not json");

        let data = store.load_since(now - Duration::days(1));
        assert!(
            data.warnings
                .iter()
                .any(|warning| warning.contains("malformed"))
        );
    }

    #[test]
    fn series_derive_weekly_tokens_and_credit_rate_weighted_estimates() {
        let reset = at(2026, 7, 31, 12, 0, 0);
        let start = reset - Duration::days(7);
        let data = HistoryData {
            quota_points: vec![quota_point(reset - Duration::minutes(1), reset, 60.0)],
            half_hour_buckets: vec![
                local_bucket(start, reset - Duration::hours(1), 10, 100),
                local_bucket(
                    start + Duration::minutes(30),
                    reset - Duration::minutes(1),
                    30,
                    300,
                ),
            ],
            ..HistoryData::default()
        };
        let half_hours = data.estimated_half_hour_series(reset);
        assert_eq!(half_hours.len(), 2);
        assert_eq!(half_hours[0].estimated_quota_percent, Some(15.0));
        assert_eq!(half_hours[1].estimated_quota_percent, Some(45.0));

        let cumulative = data.weekly_cumulative_series(reset);
        assert_eq!(cumulative.len(), 3);
        assert_eq!(cumulative[0].estimated_quota_percent, Some(0.0));
        assert_eq!(cumulative[1].token_usage.total_tokens, 10);
        assert_eq!(cumulative[1].estimated_quota_percent, Some(15.0));
        assert_eq!(cumulative[2].token_usage.total_tokens, 40);
        assert_eq!(cumulative[2].estimated_quota_percent, Some(60.0));
        assert_eq!(data.latest_weekly_reset(), Some(reset));
    }

    #[test]
    fn half_hour_estimate_keeps_confirmed_zero_buckets_as_zero_samples() {
        let reset = at(2026, 7, 31, 12, 0, 0);
        let start = reset - Duration::days(7);
        let mut zero = local_bucket(
            start + Duration::minutes(30),
            start + Duration::minutes(60),
            0,
            0,
        );
        zero.call_count = 0;
        let data = HistoryData {
            quota_points: vec![quota_point(reset - Duration::minutes(1), reset, 40.0)],
            half_hour_buckets: vec![
                local_bucket(start, start + Duration::minutes(30), 10, 100),
                zero,
            ],
            ..HistoryData::default()
        };

        let half_hours = data.estimated_half_hour_series(reset);
        assert_eq!(half_hours.len(), 2);
        assert_eq!(half_hours[0].estimated_quota_percent, Some(40.0));
        assert_eq!(half_hours[1].estimated_quota_percent, Some(0.0));
    }

    #[test]
    fn recorded_weekly_series_restores_only_confirmed_zero_plateaus() {
        let reset = at(2026, 7, 31, 12, 0, 0);
        let start = reset - Duration::days(7);
        let first_at = start + Duration::minutes(15);
        let second_at = start + Duration::minutes(60);
        let mut first_zero = local_bucket(
            start + Duration::minutes(15),
            start + Duration::minutes(30),
            0,
            0,
        );
        first_zero.call_count = 0;
        let mut second_zero = local_bucket(
            start + Duration::minutes(30),
            start + Duration::minutes(45),
            0,
            0,
        );
        second_zero.call_count = 0;
        let data = HistoryData {
            quota_points: vec![quota_point(second_at, reset, 40.0)],
            half_hour_buckets: vec![
                local_bucket(start, first_at, 10, 100),
                first_zero.clone(),
                second_zero.clone(),
                local_bucket(start + Duration::minutes(45), second_at, 10, 100),
            ],
            weekly_local_points: vec![
                weekly_point(first_at, reset, 10, 100),
                weekly_point(second_at, reset, 20, 200),
            ],
            ..HistoryData::default()
        };

        let restored = data.weekly_cumulative_series(reset);
        assert_eq!(
            restored.iter().map(|point| point.at).collect::<Vec<_>>(),
            [
                start,
                first_at,
                start + Duration::minutes(30),
                start + Duration::minutes(45),
                second_at,
            ]
        );
        assert_eq!(
            restored
                .iter()
                .map(|point| point.token_usage.total_tokens)
                .collect::<Vec<_>>(),
            [0, 10, 10, 10, 20]
        );
        assert_eq!(
            restored
                .iter()
                .map(|point| point.estimated_quota_percent)
                .collect::<Vec<_>>(),
            [Some(0.0), Some(20.0), Some(20.0), Some(20.0), Some(40.0)]
        );

        let mut missing = data.clone();
        missing
            .half_hour_buckets
            .retain(|bucket| bucket.starts_at != start + Duration::minutes(30));
        let still_gapped = missing.weekly_cumulative_series(reset);
        assert_eq!(
            still_gapped
                .iter()
                .map(|point| point.at)
                .collect::<Vec<_>>(),
            [start, first_at, start + Duration::minutes(30), second_at,]
        );
        assert_eq!(
            still_gapped[3].at - still_gapped[2].at,
            Duration::minutes(30)
        );

        first_zero.partial_reasons = vec!["rollout_scan_incomplete".to_string()];
        let mut partial = data;
        partial.half_hour_buckets[1] = first_zero;
        partial.half_hour_buckets[2] = second_zero;
        let not_confirmed = partial.weekly_cumulative_series(reset);
        assert!(!not_confirmed.iter().any(|point| {
            point.at == start + Duration::minutes(30) || point.at == start + Duration::minutes(45)
        }));
    }

    #[test]
    fn latest_weekly_reset_uses_the_newest_quota_or_local_observation() {
        let old_observed_at = at(2026, 7, 28, 12, 0, 0);
        let old_reset = at(2026, 7, 31, 12, 0, 0);
        let new_observed_at = at(2026, 8, 1, 12, 0, 0);
        let new_reset = at(2026, 8, 7, 12, 0, 0);
        let data = HistoryData {
            quota_points: vec![quota_point(old_observed_at, old_reset, 60.0)],
            weekly_local_points: vec![weekly_point(new_observed_at, new_reset, 10, 100)],
            ..HistoryData::default()
        };

        assert_eq!(data.latest_weekly_reset(), Some(new_reset));
    }

    #[test]
    fn non_aligned_weekly_reset_excludes_cross_cycle_local_buckets_and_marks_partial() {
        let reset = at(2026, 7, 31, 12, 17, 0);
        let start = reset - Duration::days(7);
        let first_full = floor_local_bucket(start) + Duration::minutes(15);
        let last_full = floor_local_bucket(reset) - Duration::minutes(15);
        let last_sample = last_full + Duration::minutes(15);
        let data = HistoryData {
            quota_points: vec![quota_point(reset - Duration::minutes(1), reset, 60.0)],
            half_hour_buckets: vec![
                local_bucket(floor_local_bucket(start), first_full, 100, 1_000),
                local_bucket(first_full, first_full + Duration::minutes(15), 10, 100),
                local_bucket(last_full, last_sample, 20, 200),
                local_bucket(floor_local_bucket(reset), reset, 200, 2_000),
            ],
            ..HistoryData::default()
        };

        let half_hours = data.estimated_half_hour_series(reset);
        assert_eq!(half_hours.len(), 2);
        assert_eq!(
            half_hours
                .iter()
                .map(|point| point.token_usage.total_tokens)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert!(half_hours.iter().all(|point| {
            point
                .partial_reasons
                .iter()
                .any(|reason| reason == "reset_boundary_excludes_partial_local_buckets")
        }));

        let cumulative = data.weekly_cumulative_series(reset);
        assert_eq!(cumulative.len(), 3);
        assert_eq!(cumulative.last().unwrap().token_usage.total_tokens, 30);
        assert_eq!(cumulative.last().unwrap().at, last_sample);
        assert!(cumulative.iter().all(|point| {
            point
                .partial_reasons
                .iter()
                .any(|reason| reason == "reset_boundary_excludes_partial_local_buckets")
        }));
    }

    #[test]
    fn recorded_weekly_points_cut_non_aligned_cycles_at_exact_call_times() {
        let reset = at(2026, 7, 31, 12, 17, 0);
        let start = reset - Duration::days(7);
        let observed_at = reset - Duration::minutes(7);
        let limits = vec![LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: None,
            plan_type: None,
            primary: None,
            secondary: Some(LimitWindow::new(40.0, Some(WEEK_MINS), Some(reset))),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: observed_at,
        }];
        let calls = vec![
            call(start - Duration::minutes(1), "gpt-5.4", None, 100),
            call(start + Duration::minutes(1), "gpt-5.4", None, 10),
            call(start + Duration::days(3), "gpt-5.4", None, 20),
            call(observed_at, "gpt-5.4", None, 30),
            call(observed_at + Duration::minutes(1), "gpt-5.4", None, 200),
        ];
        let observation = HistoryObservation::from_sources(observed_at, &calls, &limits, &[]);
        let data = HistoryData {
            quota_points: observation.quota_points,
            half_hour_buckets: observation.half_hour_buckets,
            weekly_local_points: observation.weekly_local_points,
            ..HistoryData::default()
        };

        let cumulative = data.weekly_cumulative_series(reset);
        assert_eq!(cumulative.len(), 4);
        assert_eq!(cumulative[0].at, start);
        assert_eq!(cumulative.last().unwrap().at, observed_at);
        assert_eq!(cumulative.last().unwrap().token_usage.total_tokens, 60);
        assert_eq!(
            cumulative.last().unwrap().estimated_quota_percent,
            Some(40.0)
        );
        assert!(cumulative.last().unwrap().partial_reasons.is_empty());
    }

    #[test]
    fn concurrent_writers_merge_under_the_namespace_lock() {
        let directory = tempdir().unwrap();
        let history_root = directory.path().join("state");
        let codex_home = directory.path().join("codex");
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for index in 0..4 {
            let history_root = history_root.clone();
            let codex_home = codex_home.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let mut store = HistoryStore::new(history_root, &codex_home);
                let starts_at = at(2026, 7, 28, 10 + index, 0, 0);
                let observation = HistoryObservation {
                    observed_at: at(2026, 7, 28, 14, 0, 0),
                    quota_points: Vec::new(),
                    half_hour_buckets: vec![local_bucket(
                        starts_at,
                        at(2026, 7, 28, 14, 0, 0),
                        index as u64 + 1,
                        index as u128 + 1,
                    )],
                    weekly_local_points: Vec::new(),
                };
                barrier.wait();
                store.record(&observation).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let mut store = HistoryStore::new(history_root, &codex_home);
        let data = store.load_since(at(2026, 7, 28, 0, 0, 0));
        assert_eq!(data.half_hour_buckets.len(), 4);
    }

    fn quota_point(
        observed_at: DateTime<Utc>,
        resets_at: DateTime<Utc>,
        used_percent: f64,
    ) -> QuotaPoint {
        QuotaPoint {
            observed_at,
            limit_id: "codex".to_string(),
            duration_mins: WEEK_MINS,
            resets_at,
            used_percent,
            remaining_percent: 100.0 - used_percent,
            provenance: Provenance::ServerSnapshot,
        }
    }

    fn local_bucket(
        starts_at: DateTime<Utc>,
        sampled_at: DateTime<Utc>,
        total_tokens: u64,
        estimated_cost_units: u128,
    ) -> LocalHalfHourBucket {
        LocalHalfHourBucket {
            starts_at,
            ends_at: starts_at + Duration::minutes(LOCAL_BUCKET_MINUTES),
            sampled_at: sampled_at.clamp(
                starts_at,
                starts_at + Duration::minutes(LOCAL_BUCKET_MINUTES),
            ),
            token_usage: usage(total_tokens),
            estimated_cost_units,
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

    fn weekly_point(
        observed_at: DateTime<Utc>,
        resets_at: DateTime<Utc>,
        total_tokens: u64,
        estimated_cost_units: u128,
    ) -> WeeklyLocalPoint {
        WeeklyLocalPoint {
            observed_at,
            resets_at,
            token_usage: usage(total_tokens),
            estimated_cost_units,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            call_count: 1,
            partial_reasons: Vec::new(),
        }
    }
}
