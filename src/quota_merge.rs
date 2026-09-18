//! Deterministic projection of observations explicitly assigned to one account.
//! Raw observations remain source-owned; this projection is never exported.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::history::QuotaPoint;

const SAMPLE_SECONDS: i64 = 300;
const RESET_DRIFT_SECONDS: i64 = 120;

/// Union coverage without summing or averaging account-wide percentages.
/// Reset clusters are anchored to their earliest reset, rather than chained
/// through successive near matches. Sorting before clustering makes both the
/// result and the tie-break independent of source/synchronization order.
pub(crate) fn merge_quota_points(mut points: Vec<QuotaPoint>) -> Vec<QuotaPoint> {
    points.sort_by(|a, b| {
        a.limit_id
            .to_ascii_lowercase()
            .cmp(&b.limit_id.to_ascii_lowercase())
            .then(a.duration_mins.cmp(&b.duration_mins))
            .then(a.resets_at.cmp(&b.resets_at))
            .then(a.observed_at.cmp(&b.observed_at))
            .then(a.used_percent.total_cmp(&b.used_percent))
            .then(b.remaining_percent.total_cmp(&a.remaining_percent))
            .then(a.limit_id.cmp(&b.limit_id))
            .then_with(|| provenance_rank(a.provenance).cmp(&provenance_rank(b.provenance)))
    });
    let mut merged = BTreeMap::<(String, i64, DateTime<Utc>, i64), QuotaPoint>::new();
    let mut cycle: Option<(String, i64, DateTime<Utc>)> = None;
    for mut point in points {
        let limit = point.limit_id.to_ascii_lowercase();
        let anchor = match &cycle {
            Some((id, duration, reset))
                if *id == limit
                    && *duration == point.duration_mins
                    && point.resets_at.signed_duration_since(*reset).num_seconds()
                        <= RESET_DRIFT_SECONDS =>
            {
                *reset
            }
            _ => {
                cycle = Some((limit.clone(), point.duration_mins, point.resets_at));
                point.resets_at
            }
        };
        point.resets_at = anchor;
        let key = (
            limit,
            point.duration_mins,
            anchor,
            point.observed_at.timestamp().div_euclid(SAMPLE_SECONDS),
        );
        match merged.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(point);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let old = entry.get();
                let newer = point
                    .observed_at
                    .cmp(&old.observed_at)
                    .then(point.used_percent.total_cmp(&old.used_percent))
                    .then(old.remaining_percent.total_cmp(&point.remaining_percent))
                    .then(point.limit_id.cmp(&old.limit_id))
                    .then(provenance_rank(point.provenance).cmp(&provenance_rank(old.provenance)));
                if newer.is_gt() {
                    entry.insert(point);
                }
            }
        }
    }
    let mut result = merged.into_values().collect::<Vec<_>>();
    result.sort_by(|a, b| {
        a.observed_at
            .cmp(&b.observed_at)
            .then(a.duration_mins.cmp(&b.duration_mins))
            .then(a.limit_id.cmp(&b.limit_id))
            .then(a.resets_at.cmp(&b.resets_at))
    });
    result
}

fn provenance_rank(provenance: crate::domain::Provenance) -> u8 {
    use crate::domain::Provenance::*;
    match provenance {
        Unknown => 0,
        Estimated => 1,
        Inferred => 2,
        Stale => 3,
        LocalExact => 4,
        ServerSnapshot => 5,
        Live => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provenance;
    use chrono::Duration;

    fn point(minutes: i64, remaining: f64, reset_offset: i64) -> QuotaPoint {
        let base = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        QuotaPoint {
            observed_at: base + Duration::minutes(minutes),
            resets_at: base + Duration::hours(5) + Duration::seconds(reset_offset),
            duration_mins: 300,
            limit_id: "codex".into(),
            used_percent: 100.0 - remaining,
            remaining_percent: remaining,
            provenance: Provenance::ServerSnapshot,
        }
    }

    #[test]
    fn quota_merge_unions_coverage_and_uses_newest_sample_without_averaging() {
        let merged = merge_quota_points(vec![
            point(0, 95.0, 0),
            point(2, 94.8, 30),
            point(5, 90.0, 0),
            point(120, 20.0, 0),
        ]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].remaining_percent, 94.8);
        assert_eq!(merged[0].observed_at, point(2, 94.8, 30).observed_at);
        assert_eq!(merged[0].resets_at, point(0, 95.0, 0).resets_at);
        assert_eq!(
            merged[2].observed_at - merged[1].observed_at,
            Duration::minutes(115)
        );
    }

    #[test]
    fn quota_merge_is_order_independent_and_duplicate_safe() {
        let input = vec![
            point(0, 95.0, 0),
            point(2, 94.0, 30),
            point(2, 93.0, 60),
            point(5, 90.0, 120),
            point(6, 89.0, 240),
        ];
        let expected = merge_quota_points(input.clone());
        let mut reversed = input.clone();
        reversed.reverse();
        assert_eq!(expected, merge_quota_points(reversed));
        assert_eq!(
            expected,
            merge_quota_points((0..3).flat_map(|_| input.clone()).collect())
        );
        assert_eq!(expected[0].remaining_percent, 93.0);
        // 0 -> 120 -> 240 seconds must not chain into a single reset.
        assert_eq!(expected.len(), 3);
        assert_ne!(expected[1].resets_at, expected[2].resets_at);
    }

    #[test]
    fn quota_merge_keeps_early_resets_and_distinct_limits() {
        let mut other_limit = point(1, 50.0, 0);
        other_limit.limit_id = "other".into();
        let merged = merge_quota_points(vec![point(0, 5.0, 0), point(1, 100.0, 3600), other_limit]);
        assert_eq!(merged.len(), 3);
        assert!(merged.iter().any(|p| p.remaining_percent == 100.0));
    }
}
