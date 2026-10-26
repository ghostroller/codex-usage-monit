use chrono::TimeZone;

use super::*;
use crate::domain::Provenance;

fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, hour, minute, 0)
        .single()
        .unwrap()
}

fn quota(observed_at: DateTime<Utc>, resets_at: DateTime<Utc>, used_percent: f64) -> QuotaPoint {
    QuotaPoint {
        observed_at,
        limit_id: "codex".to_string(),
        duration_mins: WEEKLY_WINDOW_MINUTES,
        resets_at,
        used_percent,
        remaining_percent: 100.0 - used_percent,
        provenance: Provenance::ServerSnapshot,
    }
}

fn weekly(observed_at: DateTime<Utc>, resets_at: DateTime<Utc>, total: u64) -> WeeklyLocalPoint {
    WeeklyLocalPoint {
        observed_at,
        resets_at,
        token_usage: TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        },
        estimated_cost_units: u128::from(total),
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::current_history_estimator_revision(),
        call_count: 1,
        partial_reasons: Vec::new(),
    }
}

fn source(weekly_local_points: Vec<WeeklyLocalPoint>) -> SourceSlice {
    SourceSlice {
        metadata: SourceMetadata::new_with_redaction_profile(
            "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
            SourceKind::Local,
            "local",
            crate::source_history::RedactionProfile::Redacted,
        )
        .unwrap(),
        buckets: Vec::new(),
        weekly_local_points,
    }
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
        estimator_revision: crate::history::current_history_estimator_revision(),
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 1,
        groups: Vec::new(),
        project_groups: Vec::new(),
        partial_reasons: Vec::new(),
    }
}

#[test]
fn rolling_reset_with_a_nonzero_local_call_does_not_create_a_seventeenth_cycle() {
    let stable_reset = at(14, 4, 1) + Duration::seconds(30);
    let rolling_reset = stable_reset - Duration::seconds(232);
    let observed_at = stable_reset - Duration::days(7) + Duration::seconds(7);
    let point = weekly(observed_at, rolling_reset, 108_253);
    let sources = vec![source(vec![point.clone()])];
    let mut quota_points = (0_i64..16)
        .map(|index| {
            let reset = stable_reset + Duration::days(index * 2);
            quota(reset - Duration::days(6), reset, 1.0)
        })
        .collect::<Vec<_>>();
    quota_points.push(quota(
        rolling_reset - Duration::days(7) - Duration::seconds(1),
        rolling_reset,
        0.0,
    ));

    let resets = canonical_weekly_resets(&sources, &quota_points).unwrap();
    assert_eq!(resets.len(), 16);
    assert!(resets.contains(&stable_reset));
    assert!(!resets.contains(&rolling_reset));
    assert_eq!(sources[0].weekly_local_points, vec![point]);
}

#[test]
fn authoritative_reset_cycles_are_not_limited_to_sixteen() {
    let first = at(8, 0, 0);
    let account = (0_i64..64)
        .map(|index| quota(at(1, 0, 0), first + Duration::seconds(index * 121), 1.0))
        .collect::<Vec<_>>();
    let resets = canonical_weekly_resets(&[], &account).unwrap();
    assert_eq!(resets.len(), 64);
}

#[test]
fn authoritative_cycles_232_seconds_apart_are_preserved() {
    let first = at(14, 4, 1) + Duration::seconds(30);
    let second = first + Duration::seconds(232);
    let account = vec![
        quota(at(7, 5, 0), first, 1.0),
        quota(at(7, 5, 1), second, 2.0),
        quota(first - Duration::days(7), first, 0.0),
    ];
    let sources = vec![source(vec![weekly(at(7, 6, 0), first, 10)])];

    assert_eq!(
        canonical_weekly_resets(&sources, &account).unwrap(),
        vec![first, second]
    );
}

#[test]
fn source_only_cycles_without_rolling_quota_evidence_are_preserved() {
    let observed_at = at(1, 0, 0);
    let first_reset = at(8, 0, 0);
    let source = source(
        (0_i64..32)
            .map(|index| weekly(observed_at, first_reset + Duration::minutes(index * 3), 10))
            .collect(),
    );

    let resets = canonical_weekly_resets(&[source], &[]).unwrap();
    assert_eq!(resets.len(), 32);
}

#[test]
fn superseded_rolling_baseline_is_not_moved_into_a_later_cycle() {
    let stable_reset = at(14, 4, 1) + Duration::seconds(30);
    let rolling_reset = stable_reset - Duration::seconds(232);
    let observed_at = stable_reset - Duration::days(7) + Duration::seconds(7);
    let old_baseline = weekly(observed_at, rolling_reset, 108_253);
    let mut source = source(vec![old_baseline.clone()]);
    source.buckets.push(bucket(at(7, 4, 15), 10));
    let account = vec![
        quota(
            rolling_reset - Duration::days(7) - Duration::seconds(1),
            rolling_reset,
            0.0,
        ),
        quota(at(7, 5, 0), stable_reset, 1.0),
    ];
    let resets = canonical_weekly_resets(std::slice::from_ref(&source), &account).unwrap();
    let points = aggregate_source_weekly_points_for_resets(
        std::slice::from_ref(&source),
        &resets,
        at(7, 0, 0),
    )
    .unwrap();

    assert_eq!(resets, vec![stable_reset]);
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].resets_at, stable_reset);
    assert_eq!(points[0].token_usage.total_tokens, 10);
    assert!(
        points[0]
            .partial_reasons
            .iter()
            .any(|reason| reason == "local_weekly_from_buckets_lower_bound")
    );
    assert_eq!(source.weekly_local_points, vec![old_baseline]);
}

#[test]
fn a_future_zero_gauge_does_not_reclassify_an_earlier_source_baseline() {
    let rolling_reset = at(14, 4, 0);
    let stable_reset = rolling_reset + Duration::seconds(232);
    let source = source(vec![weekly(at(7, 3, 59), rolling_reset, 10)]);
    let account = vec![
        quota(at(7, 4, 0), rolling_reset, 0.0),
        quota(at(7, 5, 0), stable_reset, 1.0),
    ];

    assert_eq!(
        canonical_weekly_resets(&[source], &account).unwrap(),
        vec![rolling_reset, stable_reset]
    );
}

#[test]
fn a_stable_zero_gauge_source_cycle_survives_a_later_distinct_cycle() {
    let first_reset = at(8, 0, 0);
    let later_reset = at(9, 0, 0);
    let source = source(vec![weekly(at(1, 0, 11), first_reset, 10)]);
    let account = vec![
        quota(at(1, 0, 0), first_reset, 0.0),
        quota(at(1, 0, 5), first_reset, 0.0),
        quota(at(1, 0, 10), first_reset, 0.0),
        quota(at(2, 0, 1), later_reset, 1.0),
    ];

    assert_eq!(
        canonical_weekly_resets(&[source], &account).unwrap(),
        vec![first_reset, later_reset]
    );
}

#[test]
fn more_than_sixteen_cycles_project_without_discarding_samples() {
    let first = at(8, 0, 0);
    let account = (0_i64..32)
        .map(|index| {
            let reset = first + Duration::minutes(index * 3);
            quota(reset - Duration::days(7) + Duration::hours(1), reset, 1.0)
        })
        .collect::<Vec<_>>();
    let sources = vec![source(
        account
            .iter()
            .map(|point| weekly(point.observed_at, point.resets_at, 10))
            .collect(),
    )];

    let (points, work) =
        aggregate_source_weekly_points_with_work(&sources, &account, at(1, 0, 0)).unwrap();
    assert_eq!(work.reset_cycles, 32);
    assert_eq!(work.weekly_advances, 32);
    assert_eq!(points.len(), 32);
    assert!(
        points
            .iter()
            .all(|point| point.token_usage.total_tokens == 10)
    );
}

#[test]
fn cycle_source_plan_budget_is_checked_before_allocating_the_cross_product() {
    let resets = (0_i64..5)
        .map(|index| at(8, 0, 0) + Duration::days(index))
        .collect::<Vec<_>>();
    let sources = vec![source(Vec::new())];

    let error = with_weekly_projection_budget(9, || {
        aggregate_source_weekly_points_for_resets(&sources, &resets, at(1, 0, 0))
    })
    .unwrap_err();
    assert!(error.to_string().contains("cycle/source plans"));
}

#[test]
fn weekly_budget_counts_index_memberships_and_source_evaluations() {
    let resets = vec![at(8, 0, 0)];
    let mut sources = vec![source(Vec::new())];
    sources[0].buckets.push(bucket(at(1, 0, 0), 10));

    let membership_error = with_weekly_projection_budget(4, || {
        aggregate_source_weekly_points_for_resets(&sources, &resets, at(1, 0, 0))
    })
    .unwrap_err();
    assert!(membership_error.to_string().contains("indexed memberships"));

    let evaluation_error = with_weekly_projection_budget(6, || {
        aggregate_source_weekly_points_for_resets(&sources, &resets, at(1, 0, 0))
    })
    .unwrap_err();
    assert!(evaluation_error.to_string().contains("source evaluations"));
    assert_eq!(
        aggregate_source_weekly_points_for_resets(&sources, &resets, at(1, 0, 0))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn reset_candidate_budget_is_scoped_and_restored_after_unwind() {
    let account = vec![quota(at(1, 0, 0), at(8, 0, 0), 1.0)];
    let error =
        with_weekly_projection_budget(0, || canonical_weekly_resets(&[], &account)).unwrap_err();
    assert!(error.to_string().contains("account reset candidates"));

    let panic = std::panic::catch_unwind(|| {
        with_weekly_projection_budget(0, || panic!("test budget restoration"));
    });
    assert!(panic.is_err());
    assert_eq!(canonical_weekly_resets(&[], &account).unwrap().len(), 1);
}
