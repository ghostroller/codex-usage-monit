//! Shared derivation and serializable data transfer objects for usage trends.
//!
//! The interactive UI and non-interactive callers should consume the same
//! report so quota-cycle selection, reset handling, calibration, and partial
//! state cannot drift between them.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::history::{HistoryData, LOCAL_BUCKET_MINUTES};

pub const FIVE_HOUR_WINDOW_MINUTES: i64 = 300;
pub const WEEKLY_WINDOW_MINUTES: i64 = 10_080;
pub const TRENDS_REPORT_SCHEMA_VERSION: u32 = 1;

fn datetime_saturating_add(timestamp: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    timestamp.checked_add_signed(duration).unwrap_or_else(|| {
        if duration < Duration::zero() {
            DateTime::<Utc>::MIN_UTC
        } else {
            DateTime::<Utc>::MAX_UTC
        }
    })
}

fn datetime_saturating_sub(timestamp: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    timestamp.checked_sub_signed(duration).unwrap_or_else(|| {
        if duration < Duration::zero() {
            DateTime::<Utc>::MAX_UTC
        } else {
            DateTime::<Utc>::MIN_UTC
        }
    })
}

fn interval_midpoint(starts_at: DateTime<Utc>, ends_at: DateTime<Utc>) -> DateTime<Utc> {
    datetime_saturating_add(starts_at, ends_at.signed_duration_since(starts_at) / 2)
}

/// A half-open interval represented by a trend sample: `[starts_at, ends_at)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendInterval {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// Exact value used for labels and machine-readable output.
///
/// Charts convert this value to `f64`, but token values remain `u64` here so
/// JSON output does not lose precision above `2^53`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum TrendReadoutValue {
    Percent(f64),
    Tokens(#[serde(with = "crate::exact_json::u64_decimal")] u64),
}

impl TrendReadoutValue {
    pub fn chart_value(self) -> f64 {
        match self {
            Self::Percent(value) => value,
            Self::Tokens(value) => value as f64,
        }
    }
}

/// One plotted sample together with its exact observation metadata.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendPoint {
    pub at: DateTime<Utc>,
    pub value: f64,
    pub readout_value: TrendReadoutValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<TrendInterval>,
    pub partial: bool,
}

impl TrendPoint {
    /// Returns an inspectable readout for real samples. Synthetic anchors have
    /// no `sampled_at` and intentionally do not produce a current readout.
    pub fn readout(self) -> Option<TrendReadout> {
        self.sampled_at.map(|sampled_at| TrendReadout {
            sampled_at,
            value: self.readout_value,
            interval: self.interval,
            partial: self.partial,
        })
    }
}

/// Exact current/inspected value for a trend line or bar.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendReadout {
    pub sampled_at: DateTime<Utc>,
    pub value: TrendReadoutValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<TrendInterval>,
    pub partial: bool,
}

/// All trend data derived from one immutable history snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendsReport {
    pub schema_version: u32,
    pub as_of: DateTime<Utc>,
    pub day_offset: u16,
    pub five_hour_remaining: Vec<TrendPoint>,
    pub weekly_remaining: Vec<TrendPoint>,
    pub weekly_tokens: Vec<TrendPoint>,
    pub weekly_estimated: Vec<TrendPoint>,
    /// Fifteen-minute token buckets in the selected aligned 24-hour window.
    #[serde(rename = "fifteenMinuteTokens", alias = "halfHourTokens")]
    pub half_hour_tokens: Vec<TrendPoint>,
    /// Fifteen-minute estimated-quota buckets in the selected 24-hour window.
    #[serde(rename = "fifteenMinuteEstimated", alias = "halfHourEstimated")]
    pub half_hour_estimated: Vec<TrendPoint>,
    pub five_hour_remaining_readout: Option<TrendReadout>,
    pub weekly_remaining_readout: Option<TrendReadout>,
    pub weekly_tokens_readout: Option<TrendReadout>,
    pub weekly_estimated_readout: Option<TrendReadout>,
    /// Bounds for the fifteen-minute series. The interval is half-open.
    #[serde(rename = "fifteenMinuteBounds", alias = "halfHourBounds")]
    pub half_hour_bounds: [DateTime<Utc>; 2],
    pub weekly_history_present: bool,
    #[serde(
        rename = "fifteenMinuteHistoryPresent",
        alias = "halfHourHistoryPresent"
    )]
    pub half_hour_history_present: bool,
    pub history_warning_count: usize,
    pub history_warnings: Vec<String>,
    pub history_read_only: bool,
    pub api_long_context_multiplier: bool,
}

/// Builds the same trend data used by the interactive view.
///
/// `now` controls both live readouts and the right edge of the aligned
/// 24-hour local-usage window. `day_offset = 0` selects the current window;
/// larger offsets move it back by whole days. Enabling
/// `api_long_context_multiplier` changes only estimated-quota series.
pub fn build_trends_report(
    history: &HistoryData,
    now: DateTime<Utc>,
    day_offset: u16,
    api_long_context_multiplier: bool,
) -> TrendsReport {
    let five_hour_remaining = remaining_trend_as_of(history, FIVE_HOUR_WINDOW_MINUTES, now);
    let weekly_remaining = remaining_trend_as_of(history, WEEKLY_WINDOW_MINUTES, now);
    let five_hour_remaining_readout =
        remaining_trend_readout(history, FIVE_HOUR_WINDOW_MINUTES, now);
    let weekly_remaining_readout = remaining_trend_readout(history, WEEKLY_WINDOW_MINUTES, now);
    let half_hour_bounds = trend_day_bounds(now, day_offset);
    let weekly_reset = history.latest_weekly_reset_as_of(now);

    let weekly_cumulative = weekly_reset
        .map(|reset| history.weekly_cumulative_series_as_of(reset, now))
        .unwrap_or_default();
    let weekly_estimated_cumulative = weekly_reset
        .map(|reset| {
            history.weekly_cumulative_series_with_api_long_context_as_of(
                reset,
                now,
                api_long_context_multiplier,
            )
        })
        .unwrap_or_default();
    let weekly_history_present = weekly_cumulative
        .iter()
        .any(|point| point.sampled_at.is_some());
    let weekly_tokens = weekly_cumulative
        .iter()
        .map(|point| {
            trend_point(
                point.at,
                TrendReadoutValue::Tokens(point.token_usage.total_tokens),
                point.sampled_at,
                None,
                !point.partial_reasons.is_empty(),
            )
        })
        .collect();
    let weekly_estimated = weekly_estimated_cumulative
        .iter()
        .filter_map(|point| {
            point.estimated_quota_percent.map(|value| {
                trend_point(
                    point.at,
                    TrendReadoutValue::Percent(value),
                    point.sampled_at,
                    None,
                    !point.partial_reasons.is_empty(),
                )
            })
        })
        .collect();

    let current_weekly_cycle = weekly_reset.filter(|reset| {
        let starts_at = datetime_saturating_sub(*reset, Duration::minutes(WEEKLY_WINDOW_MINUTES));
        starts_at <= now && now < *reset
    });
    let weekly_tokens_readout = current_weekly_cycle
        .and_then(|_| latest_real_point_at_or_before(&weekly_cumulative, now))
        .map(|(point, sampled_at)| TrendReadout {
            sampled_at,
            value: TrendReadoutValue::Tokens(point.token_usage.total_tokens),
            interval: None,
            partial: !point.partial_reasons.is_empty(),
        });
    let weekly_estimated_readout = current_weekly_cycle
        .and_then(|_| latest_real_point_at_or_before(&weekly_estimated_cumulative, now))
        .and_then(|(point, sampled_at)| {
            point.estimated_quota_percent.map(|value| TrendReadout {
                sampled_at,
                value: TrendReadoutValue::Percent(value),
                interval: None,
                partial: !point.partial_reasons.is_empty(),
            })
        });

    let half_hour_buckets = history
        .half_hour_series()
        .iter()
        .filter(|bucket| {
            bucket.starts_at >= half_hour_bounds[0]
                && bucket.starts_at < half_hour_bounds[1]
                && bucket.sampled_at <= now
        })
        .collect::<Vec<_>>();
    let half_hour_history_present = !half_hour_buckets.is_empty();
    let half_hour_tokens = half_hour_buckets
        .iter()
        .map(|bucket| {
            let interval = TrendInterval {
                starts_at: bucket.starts_at,
                ends_at: bucket.ends_at,
            };
            trend_point(
                interval_midpoint(interval.starts_at, interval.ends_at),
                TrendReadoutValue::Tokens(bucket.token_usage.total_tokens),
                Some(bucket.sampled_at),
                Some(interval),
                !bucket.partial_reasons.is_empty(),
            )
        })
        .collect();
    let half_hour_estimated =
        half_hour_estimated_trend(history, half_hour_bounds, now, api_long_context_multiplier);

    TrendsReport {
        schema_version: TRENDS_REPORT_SCHEMA_VERSION,
        as_of: now,
        day_offset,
        five_hour_remaining,
        weekly_remaining,
        weekly_tokens,
        weekly_estimated,
        half_hour_tokens,
        half_hour_estimated,
        five_hour_remaining_readout,
        weekly_remaining_readout,
        weekly_tokens_readout,
        weekly_estimated_readout,
        half_hour_bounds,
        weekly_history_present,
        half_hour_history_present,
        history_warning_count: history.warnings.len(),
        history_warnings: history.warnings.clone(),
        history_read_only: history.read_only,
        api_long_context_multiplier,
    }
}

/// Returns the aligned, half-open 24-hour interval selected by the Trends UI.
pub fn trend_day_bounds(as_of: DateTime<Utc>, day_offset: u16) -> [DateTime<Utc>; 2] {
    let shifted = datetime_saturating_sub(as_of, Duration::days(i64::from(day_offset)));
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let end_seconds = shifted
        .timestamp()
        .saturating_add(bucket_seconds - 1)
        .div_euclid(bucket_seconds)
        .saturating_mul(bucket_seconds);
    let end = DateTime::from_timestamp(end_seconds, 0).unwrap_or(shifted);
    [datetime_saturating_sub(end, Duration::hours(24)), end]
}

fn trend_point(
    at: DateTime<Utc>,
    readout_value: TrendReadoutValue,
    sampled_at: Option<DateTime<Utc>>,
    interval: Option<TrendInterval>,
    partial: bool,
) -> TrendPoint {
    TrendPoint {
        at,
        value: readout_value.chart_value(),
        readout_value,
        sampled_at,
        interval,
        partial,
    }
}

fn latest_real_point_at_or_before<T>(
    points: &[T],
    now: DateTime<Utc>,
) -> Option<(&T, DateTime<Utc>)>
where
    T: SampledPoint,
{
    points
        .iter()
        .rfind(|point| {
            point
                .sampled_at()
                .is_some_and(|sampled_at| sampled_at <= now)
        })
        .and_then(|point| point.sampled_at().map(|sampled_at| (point, sampled_at)))
}

trait SampledPoint {
    fn sampled_at(&self) -> Option<DateTime<Utc>>;
}

impl SampledPoint for crate::history::WeeklyCumulativePoint {
    fn sampled_at(&self) -> Option<DateTime<Utc>> {
        self.sampled_at
    }
}

fn remaining_trend_readout(
    history: &HistoryData,
    duration_mins: i64,
    now: DateTime<Utc>,
) -> Option<TrendReadout> {
    history
        .quota_points
        .iter()
        .filter(|point| {
            point.duration_mins == duration_mins
                && point.observed_at <= now
                && now < point.resets_at
        })
        .max_by_key(|point| (point.observed_at, point.resets_at))
        .map(|point| TrendReadout {
            sampled_at: point.observed_at,
            value: TrendReadoutValue::Percent(point.remaining_percent),
            interval: None,
            partial: false,
        })
}

fn half_hour_estimated_trend(
    history: &HistoryData,
    bounds: [DateTime<Utc>; 2],
    as_of: DateTime<Utc>,
    api_long_context: bool,
) -> Vec<TrendPoint> {
    let resets = weekly_resets_overlapping_as_of(history, bounds, as_of);
    let estimates_by_reset = resets
        .iter()
        .copied()
        .map(|reset| {
            let points = history
                .estimated_half_hour_series_with_api_long_context_as_of(
                    reset,
                    as_of,
                    api_long_context,
                )
                .into_iter()
                .filter(|point| point.starts_at >= bounds[0] && point.starts_at < bounds[1])
                .map(|point| (point.starts_at, point))
                .collect::<BTreeMap<_, _>>();
            (reset, points)
        })
        .collect::<BTreeMap<_, _>>();
    let mut points = BTreeMap::new();
    for bucket in history.half_hour_series().iter().filter(|bucket| {
        bucket.starts_at >= bounds[0] && bucket.starts_at < bounds[1] && bucket.sampled_at <= as_of
    }) {
        let crosses_reset = resets.iter().any(|reset| {
            let cycle_starts_at =
                datetime_saturating_sub(*reset, Duration::minutes(WEEKLY_WINDOW_MINUTES));
            bucket.starts_at < cycle_starts_at && cycle_starts_at < bucket.ends_at
        });
        if crosses_reset {
            continue;
        }
        let Some(reset) = resets
            .iter()
            .copied()
            .filter(|reset| {
                let cycle_starts_at =
                    datetime_saturating_sub(*reset, Duration::minutes(WEEKLY_WINDOW_MINUTES));
                bucket.starts_at >= cycle_starts_at && bucket.ends_at <= *reset
            })
            .max()
        else {
            continue;
        };
        let Some(point) = estimates_by_reset
            .get(&reset)
            .and_then(|points| points.get(&bucket.starts_at))
        else {
            continue;
        };
        let Some(value) = point.estimated_quota_percent else {
            continue;
        };
        let interval = TrendInterval {
            starts_at: point.starts_at,
            ends_at: point.ends_at,
        };
        let at = interval_midpoint(interval.starts_at, interval.ends_at);
        points.insert(
            at,
            trend_point(
                at,
                TrendReadoutValue::Percent(value),
                Some(bucket.sampled_at),
                Some(interval),
                !point.partial_reasons.is_empty(),
            ),
        );
    }
    points.into_values().collect()
}

/// Selects canonical weekly resets whose quota cycles overlap `bounds`.
///
/// Kept crate-visible so UI contract tests can lock reset-drift behavior to
/// the same implementation used by [`build_trends_report`].
#[cfg(test)]
pub(crate) fn weekly_resets_overlapping(
    history: &HistoryData,
    bounds: [DateTime<Utc>; 2],
) -> Vec<DateTime<Utc>> {
    weekly_resets_overlapping_as_of(history, bounds, DateTime::<Utc>::MAX_UTC)
}

pub(crate) fn weekly_resets_overlapping_as_of(
    history: &HistoryData,
    bounds: [DateTime<Utc>; 2],
    as_of: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    const RESET_DRIFT_SECONDS: i64 = 120;

    let mut candidates = history
        .quota_points
        .iter()
        .filter(|point| point.duration_mins == WEEKLY_WINDOW_MINUTES && point.observed_at <= as_of)
        .map(|point| (point.resets_at, point.observed_at))
        .chain(
            history
                .weekly_local_points
                .iter()
                .filter(|point| point.observed_at <= as_of)
                .map(|point| (point.resets_at, point.observed_at)),
        )
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|(reset, observed_at)| (*reset, *observed_at));

    let mut resets = Vec::<(DateTime<Utc>, DateTime<Utc>)>::new();
    let mut cluster_end = None;
    let mut representative = None::<(DateTime<Utc>, DateTime<Utc>)>;
    for (reset, observed_at) in candidates {
        let joins_cluster = cluster_end.is_some_and(|end| {
            reset <= datetime_saturating_add(end, Duration::seconds(RESET_DRIFT_SECONDS))
        });
        if !joins_cluster && let Some(previous) = representative.take() {
            resets.push(previous);
        }
        cluster_end = Some(reset);
        if representative.is_none_or(|current| (observed_at, reset) > (current.1, current.0)) {
            representative = Some((reset, observed_at));
        }
    }
    if let Some(last) = representative {
        resets.push(last);
    }
    resets.sort_by_key(|(reset, _)| *reset);

    resets
        .into_iter()
        .map(|(reset, _)| reset)
        .filter(|reset| {
            let starts_at =
                datetime_saturating_sub(*reset, Duration::minutes(WEEKLY_WINDOW_MINUTES));
            starts_at < bounds[1] && *reset > bounds[0]
        })
        .collect()
}

/// Builds an ordered remaining-quota series across every loaded reset cycle.
///
/// Kept crate-visible so UI contract tests do not duplicate transition logic.
#[cfg(test)]
pub(crate) fn remaining_trend(history: &HistoryData, duration_mins: i64) -> Vec<TrendPoint> {
    remaining_trend_as_of(history, duration_mins, DateTime::<Utc>::MAX_UTC)
}

pub(crate) fn remaining_trend_as_of(
    history: &HistoryData,
    duration_mins: i64,
    as_of: DateTime<Utc>,
) -> Vec<TrendPoint> {
    let mut points = history.remaining_series_as_of(duration_mins, as_of);
    // Keep real observations from every loaded cycle. A reset credit can start
    // a new cycle before the previous resets_at, so observed timestamps are
    // the honest transition boundary; rendering can still split recorder gaps.
    points.sort_by(|left, right| {
        left.observed_at
            .cmp(&right.observed_at)
            .then_with(|| left.resets_at.cmp(&right.resets_at))
    });
    points
        .into_iter()
        .map(|point| {
            trend_point(
                point.observed_at,
                TrendReadoutValue::Percent(point.remaining_percent),
                Some(point.observed_at),
                None,
                false,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::*;
    use crate::api_cost::API_PRICING_CATALOG_REVISION;
    use crate::domain::{Provenance, TokenUsage};
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, LocalHalfHourBucket,
        QuotaPoint, WeeklyLocalPoint,
    };

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn quota_point(
        observed_at: DateTime<Utc>,
        duration_mins: i64,
        resets_at: DateTime<Utc>,
        used_percent: f64,
    ) -> QuotaPoint {
        QuotaPoint {
            observed_at,
            limit_id: "codex".to_string(),
            duration_mins,
            resets_at,
            used_percent,
            remaining_percent: 100.0 - used_percent,
            provenance: Provenance::ServerSnapshot,
        }
    }

    fn bucket(
        starts_at: DateTime<Utc>,
        total_tokens: u64,
        estimated_cost_units: u128,
        api_long_context_extra_cost_units: Option<u128>,
        partial_reasons: &[&str],
    ) -> LocalHalfHourBucket {
        let ends_at = starts_at + Duration::minutes(LOCAL_BUCKET_MINUTES);
        LocalHalfHourBucket {
            starts_at,
            ends_at,
            sampled_at: ends_at,
            token_usage: TokenUsage {
                total_tokens,
                ..TokenUsage::default()
            },
            estimated_cost_units,
            api_long_context_extra_cost_units,
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: u64::from(total_tokens > 0),
            groups: Vec::new(),
            project_groups: Vec::new(),
            partial_reasons: partial_reasons.iter().map(ToString::to_string).collect(),
        }
    }

    fn history_fixture(now: DateTime<Utc>) -> HistoryData {
        let weekly_reset = now + Duration::days(3);
        let bounds = trend_day_bounds(now, 0);
        let mut history = HistoryData {
            quota_points: vec![
                quota_point(
                    now - Duration::hours(1),
                    300,
                    now + Duration::hours(4),
                    20.0,
                ),
                quota_point(now, 300, now + Duration::hours(4), 40.0),
                quota_point(
                    now - Duration::days(1),
                    WEEKLY_WINDOW_MINUTES,
                    weekly_reset,
                    10.0,
                ),
                quota_point(now, WEEKLY_WINDOW_MINUTES, weekly_reset, 25.0),
            ],
            half_hour_buckets: vec![
                bucket(bounds[1] - Duration::hours(2), 1_000, 100, Some(0), &[]),
                bucket(
                    bounds[1] - Duration::minutes(90),
                    2_000,
                    200,
                    Some(0),
                    &["fixture_partial"],
                ),
                bucket(bounds[1] - Duration::minutes(15), 3_000, 300, Some(0), &[]),
            ],
            warnings: vec!["fixture warning".to_string()],
            read_only: true,
            ..HistoryData::default()
        };
        // The last bucket is still open at `now`, so its exact recorder sample
        // is earlier than the interval end.
        history.half_hour_buckets.last_mut().unwrap().sampled_at = now;
        history
    }

    #[test]
    fn builds_tui_equivalent_series_readouts_and_health_metadata() {
        let now = at("2026-07-29T09:16:42Z");
        let report = build_trends_report(&history_fixture(now), now, 0, false);

        assert_eq!(report.schema_version, TRENDS_REPORT_SCHEMA_VERSION);
        assert_eq!(report.as_of, now);
        assert_eq!(report.day_offset, 0);
        assert_eq!(report.five_hour_remaining.len(), 2);
        assert_eq!(report.weekly_remaining.len(), 2);
        assert_eq!(report.weekly_tokens.len(), 4);
        assert_eq!(report.weekly_estimated.len(), 4);
        assert_eq!(report.half_hour_tokens.len(), 3);
        assert_eq!(report.half_hour_estimated.len(), 3);
        assert!(report.weekly_history_present);
        assert!(report.half_hour_history_present);
        assert_eq!(report.history_warnings, ["fixture warning"]);
        assert_eq!(report.history_warning_count, 1);
        assert!(report.history_read_only);
        assert!(!report.api_long_context_multiplier);

        assert_eq!(
            report.five_hour_remaining_readout,
            Some(TrendReadout {
                sampled_at: now,
                value: TrendReadoutValue::Percent(60.0),
                interval: None,
                partial: false,
            })
        );
        assert_eq!(
            report.weekly_remaining_readout.map(|readout| readout.value),
            Some(TrendReadoutValue::Percent(75.0))
        );
        assert_eq!(
            report.weekly_tokens_readout.map(|readout| readout.value),
            Some(TrendReadoutValue::Tokens(6_000))
        );
        assert_eq!(
            report.weekly_estimated_readout.map(|readout| readout.value),
            Some(TrendReadoutValue::Percent(25.0))
        );

        let partial = &report.half_hour_tokens[1];
        let expected_start = report.half_hour_bounds[1] - Duration::minutes(90);
        assert_eq!(
            partial.sampled_at,
            Some(expected_start + Duration::minutes(15))
        );
        assert_eq!(
            partial.interval,
            Some(TrendInterval {
                starts_at: expected_start,
                ends_at: expected_start + Duration::minutes(15),
            })
        );
        assert!(partial.partial);
        assert!(report.half_hour_estimated[1].partial);
    }

    #[test]
    fn aligns_the_selected_day_to_fifteen_minutes_and_filters_half_open() {
        let now = at("2026-07-29T12:07:00Z");
        let current_bounds = trend_day_bounds(now, 0);
        let previous_bounds = trend_day_bounds(now, 1);
        assert_eq!(current_bounds[1], at("2026-07-29T12:15:00Z"));
        assert_eq!(current_bounds[0], at("2026-07-28T12:15:00Z"));
        assert_eq!(previous_bounds[1], current_bounds[1] - Duration::days(1));

        let weekly_reset = now + Duration::days(3);
        let history = HistoryData {
            quota_points: vec![quota_point(now, WEEKLY_WINDOW_MINUTES, weekly_reset, 50.0)],
            half_hour_buckets: vec![
                bucket(previous_bounds[0], 100, 1, Some(0), &[]),
                bucket(current_bounds[0], 200, 2, Some(0), &[]),
                bucket(current_bounds[1], 300, 3, Some(0), &[]),
            ],
            ..HistoryData::default()
        };

        let current = build_trends_report(&history, now, 0, false);
        assert_eq!(
            current
                .half_hour_tokens
                .iter()
                .map(|point| point.readout_value)
                .collect::<Vec<_>>(),
            [TrendReadoutValue::Tokens(200)]
        );
        let previous = build_trends_report(&history, now, 1, false);
        assert_eq!(
            previous
                .half_hour_tokens
                .iter()
                .map(|point| point.readout_value)
                .collect::<Vec<_>>(),
            [TrendReadoutValue::Tokens(100)]
        );
    }

    #[test]
    fn long_context_reweights_only_estimated_series() {
        let now = at("2026-07-29T11:10:00Z");
        let reset = now + Duration::days(3);
        let first = now - Duration::minutes(70);
        let second = now - Duration::minutes(40);
        let history = HistoryData {
            quota_points: vec![quota_point(now, WEEKLY_WINDOW_MINUTES, reset, 40.0)],
            half_hour_buckets: vec![
                bucket(first, 200_000, 100, Some(0), &[]),
                bucket(second, 300_000, 100, Some(200), &[]),
            ],
            ..HistoryData::default()
        };

        let base = build_trends_report(&history, now, 0, false);
        let long = build_trends_report(&history, now, 0, true);
        let values =
            |points: &[TrendPoint]| points.iter().map(|point| point.value).collect::<Vec<_>>();
        let exact = |points: &[TrendPoint]| {
            points
                .iter()
                .map(|point| point.readout_value)
                .collect::<Vec<_>>()
        };

        assert_eq!(values(&base.weekly_estimated), [0.0, 20.0, 40.0]);
        assert_eq!(values(&long.weekly_estimated), [0.0, 10.0, 40.0]);
        assert_eq!(values(&base.half_hour_estimated), [20.0, 20.0]);
        assert_eq!(values(&long.half_hour_estimated), [10.0, 30.0]);
        assert_eq!(exact(&base.weekly_tokens), exact(&long.weekly_tokens));
        assert_eq!(exact(&base.half_hour_tokens), exact(&long.half_hour_tokens));
        assert!(!base.api_long_context_multiplier);
        assert!(long.api_long_context_multiplier);
    }

    #[test]
    fn fifteen_minute_estimates_use_their_own_overlapping_weekly_cycles() {
        let boundary = at("2026-07-23T12:00:00Z");
        let previous_reset = boundary;
        let current_reset = boundary + Duration::days(7);
        let now = boundary + Duration::hours(12);
        let history = HistoryData {
            quota_points: vec![
                quota_point(
                    boundary - Duration::hours(2),
                    WEEKLY_WINDOW_MINUTES,
                    previous_reset,
                    35.0,
                ),
                quota_point(
                    boundary - Duration::minutes(30),
                    WEEKLY_WINDOW_MINUTES,
                    previous_reset,
                    40.0,
                ),
                quota_point(
                    boundary + Duration::hours(1),
                    WEEKLY_WINDOW_MINUTES,
                    current_reset,
                    10.0,
                ),
                quota_point(now, WEEKLY_WINDOW_MINUTES, current_reset, 20.0),
            ],
            half_hour_buckets: vec![
                bucket(boundary - Duration::hours(1), 100, 100, Some(0), &[]),
                bucket(boundary + Duration::hours(1), 200, 200, Some(0), &[]),
            ],
            ..HistoryData::default()
        };

        let report = build_trends_report(&history, now, 0, false);
        assert_eq!(
            report
                .half_hour_estimated
                .iter()
                .map(|point| point.value)
                .collect::<Vec<_>>(),
            [40.0, 20.0]
        );
        assert!(
            report
                .half_hour_estimated
                .iter()
                .all(|point| !point.partial)
        );
    }

    #[test]
    fn serialization_is_camel_case_and_preserves_exact_token_readouts() {
        let now = at("2026-07-29T09:16:42Z");
        let mut history = history_fixture(now);
        let exact_tokens = 9_007_199_254_740_993;
        for bucket in &mut history.half_hour_buckets {
            bucket.token_usage.total_tokens = 0;
        }
        history
            .half_hour_buckets
            .last_mut()
            .unwrap()
            .token_usage
            .total_tokens = exact_tokens;

        let report = build_trends_report(&history, now, 0, false);
        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["schemaVersion"], TRENDS_REPORT_SCHEMA_VERSION);
        assert_eq!(value["asOf"], json!(now));
        assert_eq!(value["dayOffset"], 0);
        assert_eq!(value["historyWarnings"], json!(["fixture warning"]));
        assert_eq!(value["historyReadOnly"], true);
        assert!(value.get("fifteenMinuteTokens").is_some());
        assert!(value.get("halfHourTokens").is_none());
        let last = value["fifteenMinuteTokens"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(last["sampledAt"], json!(now));
        assert_eq!(last["readoutValue"]["kind"], "tokens");
        assert_eq!(
            last["readoutValue"]["value"],
            json!(exact_tokens.to_string())
        );
        assert_eq!(
            last["interval"]["startsAt"],
            json!(report.half_hour_bounds[1] - Duration::minutes(15))
        );
        assert_eq!(
            last["interval"]["endsAt"],
            json!(report.half_hour_bounds[1])
        );
        assert_eq!(last["partial"], false);

        let round_trip: TrendsReport = serde_json::from_value(value).unwrap();
        assert_eq!(round_trip, report);

        assert_eq!(
            serde_json::from_value::<TrendReadoutValue>(json!({
                "kind": "tokens",
                "value": 123
            }))
            .unwrap(),
            TrendReadoutValue::Tokens(123)
        );
    }

    #[test]
    fn expired_quota_and_synthetic_weekly_anchor_do_not_become_readouts() {
        let now = at("2026-07-29T09:16:42Z");
        let reset = now + Duration::days(3);
        let history = HistoryData {
            quota_points: vec![
                quota_point(now - Duration::minutes(1), 300, now, 90.0),
                quota_point(now, WEEKLY_WINDOW_MINUTES, reset, 25.0),
            ],
            ..HistoryData::default()
        };

        let report = build_trends_report(&history, now, 0, false);
        assert!(!report.five_hour_remaining.is_empty());
        assert_eq!(report.five_hour_remaining_readout, None);
        assert_eq!(
            report.weekly_remaining_readout.map(|readout| readout.value),
            Some(TrendReadoutValue::Percent(75.0))
        );
        assert_eq!(report.weekly_tokens.len(), 1);
        assert_eq!(report.weekly_tokens_readout, None);
        assert_eq!(report.weekly_estimated_readout, None);
    }

    #[test]
    fn report_excludes_every_observation_after_as_of() {
        let now = at("2026-07-29T12:00:00Z");
        let reset = now + Duration::days(3);
        let future_reset = now + Duration::days(8);
        let mut future_bucket = bucket(now - Duration::minutes(15), 9_000, 900, Some(0), &[]);
        future_bucket.sampled_at = now + Duration::hours(1);
        let history = HistoryData {
            quota_points: vec![
                quota_point(
                    now - Duration::minutes(1),
                    300,
                    now + Duration::hours(4),
                    20.0,
                ),
                quota_point(
                    now + Duration::hours(1),
                    300,
                    now + Duration::hours(4),
                    90.0,
                ),
                quota_point(
                    now - Duration::minutes(1),
                    WEEKLY_WINDOW_MINUTES,
                    reset,
                    25.0,
                ),
                quota_point(now + Duration::hours(1), WEEKLY_WINDOW_MINUTES, reset, 90.0),
                quota_point(
                    now + Duration::hours(2),
                    WEEKLY_WINDOW_MINUTES,
                    future_reset,
                    95.0,
                ),
            ],
            half_hour_buckets: vec![
                bucket(now - Duration::minutes(30), 1_000, 100, Some(0), &[]),
                future_bucket,
            ],
            weekly_local_points: vec![WeeklyLocalPoint {
                observed_at: now + Duration::hours(2),
                resets_at: future_reset,
                token_usage: TokenUsage {
                    total_tokens: 50_000,
                    ..TokenUsage::default()
                },
                estimated_cost_units: 5_000,
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: HISTORY_ESTIMATOR_REVISION,
                call_count: 1,
                partial_reasons: Vec::new(),
            }],
            ..HistoryData::default()
        };

        let report = build_trends_report(&history, now, 0, false);

        assert_eq!(report.five_hour_remaining.len(), 1);
        assert_eq!(report.weekly_remaining.len(), 1);
        assert_eq!(
            report.five_hour_remaining_readout.unwrap().value,
            TrendReadoutValue::Percent(80.0)
        );
        assert_eq!(
            report.weekly_remaining_readout.unwrap().value,
            TrendReadoutValue::Percent(75.0)
        );
        assert_eq!(report.half_hour_tokens.len(), 1);
        assert_eq!(
            report.half_hour_tokens[0].readout_value,
            TrendReadoutValue::Tokens(1_000)
        );
        assert!(
            report
                .weekly_tokens
                .iter()
                .filter_map(|point| point.sampled_at)
                .all(|sampled_at| sampled_at <= now)
        );
        assert_eq!(
            report.weekly_estimated_readout.unwrap().value,
            TrendReadoutValue::Percent(25.0)
        );
    }

    #[test]
    fn datetime_extremes_are_safe_for_trend_windows() {
        let minimum = DateTime::<Utc>::MIN_UTC;
        let maximum = DateTime::<Utc>::MAX_UTC;

        let minimum_bounds = trend_day_bounds(minimum, u16::MAX);
        assert_eq!(minimum_bounds[0], minimum);
        assert!(minimum_bounds[0] <= minimum_bounds[1]);
        let maximum_bounds = trend_day_bounds(maximum, 0);
        assert!(maximum_bounds[0] <= maximum_bounds[1]);
        assert!(maximum_bounds[1] <= maximum);

        let history = HistoryData::default();
        let minimum_report = build_trends_report(&history, minimum, u16::MAX, false);
        let maximum_report = build_trends_report(&history, maximum, 0, false);
        assert_eq!(minimum_report.as_of, minimum);
        assert_eq!(maximum_report.as_of, maximum);

        let midpoint = interval_midpoint(minimum, maximum);
        assert!(midpoint >= minimum && midpoint <= maximum);
    }
}
