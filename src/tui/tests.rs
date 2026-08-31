use super::*;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;

use crate::domain::{
    ApiCostAmount, ApiEquivalentCost, AttributionSummary, CollectionStats, LimitBucket,
    LimitWindow, ModelUsage, PicoUsd, RateLimitResetCredit, RateLimitResetCreditsSnapshot,
    SourceStatus, ThreadWindowUsage, TurnWindowUsage, UsageCall, WindowAnalysis, WindowDescriptor,
    WindowUsage,
};
use crate::history::{LocalHalfHourBucket, LocalProjectUsageGroup, QuotaPoint, WeeklyLocalPoint};
use crate::history_ownership::{HistoryOwnershipState, OwnershipManifestStatus};
use crate::remote_protocol::{
    GitRepositoryFingerprint, ProtocolRevisions, RemoteLiveSnapshot, RemoteLiveTask,
    RemoteLiveTurn, RemoteTokenUsage, SourceGeneration,
};
use crate::remote_sync::{RemoteSyncCompletion, RemoteSyncReport};
use crate::source_history::{
    LocalObservationMode, RedactionProfile, SourceBucketRecord, SourceHistoryRemoteBinding,
    SourceHistoryRemoteGenerationId, SourceKind, SourceMetadata,
};
use crate::source_identity::NodeId;
use chrono::TimeZone;
use ratatui::backend::TestBackend;

mod integration_scenarios;
mod testkit;

const RESUMABLE_THREAD_ID: &str = "019f52ac-7a9f-7fd1-8dda-e775ef950785";

#[cfg(windows)]
static WINDOWS_TERMINATION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn summary_estimate_formats_credit_rate_equivalents_instead_of_raw_units() {
    assert_eq!(format_estimated_credits(8_000_000, true), "~1.00 cr");
    assert_eq!(format_estimated_credits(800_000_000, true), "~100.0 cr");
    assert_eq!(format_estimated_credits(8_000_000_000, false), "~1.00Kcr");
}

#[test]
fn summary_api_chart_marks_a_non_aligned_range_start_as_a_lower_bound() {
    let starts_at = Utc::now();
    let window = SummaryWindow::new(starts_at, starts_at + ChronoDuration::days(1)).unwrap();
    let prepared = PreparedSummary {
        usage: UsageSummary {
            window,
            totals: SummaryMetrics::default(),
            days: Vec::new(),
            hours: Vec::new(),
            projects: Vec::new(),
        },
        range_note: None,
        represented_tokens: 0,
        available_tokens: 0,
        covered_buckets: 96,
        expected_buckets: 96,
        estimated_covered_tokens: 0,
        long_context_breakdown_complete: true,
        daily_coverage: BTreeMap::new(),
        hourly_coverage: BTreeMap::new(),
        partial_reasons: vec!["range_starts_within_15m_bucket".to_string()],
    };

    assert!(prepared.api_chart_is_lower_bound());
}

#[test]
fn summary_daily_coverage_distinguishes_complete_partial_and_missing_days() {
    assert_eq!(
        summary_daily_status_symbols(&[
            SummaryDailyState::Complete,
            SummaryDailyState::Missing,
            SummaryDailyState::Partial,
        ]),
        "CMP",
        "known zero-value days need a visible state distinct from missing days"
    );

    let date = NaiveDate::from_ymd_opt(2026, 8, 28).unwrap();
    let window = SummaryWindow::new(
        DateTime::parse_from_rfc3339("2026-08-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        DateTime::parse_from_rfc3339("2026-08-29T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    )
    .unwrap();
    let mut prepared = PreparedSummary {
        usage: UsageSummary {
            window,
            totals: SummaryMetrics::default(),
            days: Vec::new(),
            hours: Vec::new(),
            projects: Vec::new(),
        },
        range_note: None,
        represented_tokens: 0,
        available_tokens: 0,
        covered_buckets: 0,
        expected_buckets: 0,
        estimated_covered_tokens: 0,
        long_context_breakdown_complete: true,
        daily_coverage: BTreeMap::from([(
            date,
            SummaryDailyCoverage {
                expected_buckets: 96,
                covered_buckets: 96,
                ..SummaryDailyCoverage::default()
            },
        )]),
        hourly_coverage: BTreeMap::new(),
        partial_reasons: Vec::new(),
    };
    with_test_display_offset(FixedOffset::east_opt(0).unwrap(), || {
        assert_eq!(
            prepared.daily_state(
                date,
                SummaryMetrics::default(),
                SummaryMetric::Tokens,
                false
            ),
            SummaryDailyState::Complete
        );
    });

    prepared
        .daily_coverage
        .get_mut(&date)
        .unwrap()
        .covered_buckets = 48;
    with_test_display_offset(FixedOffset::east_opt(0).unwrap(), || {
        assert_eq!(
            prepared.daily_state(
                date,
                SummaryMetrics::default(),
                SummaryMetric::Tokens,
                false
            ),
            SummaryDailyState::Partial
        );
    });

    prepared
        .daily_coverage
        .get_mut(&date)
        .unwrap()
        .covered_buckets = 0;
    with_test_display_offset(FixedOffset::east_opt(0).unwrap(), || {
        assert_eq!(
            prepared.daily_state(
                date,
                SummaryMetrics::default(),
                SummaryMetric::Tokens,
                false
            ),
            SummaryDailyState::Missing
        );
    });
}

#[test]
fn rolling_summary_ranges_mark_first_and_last_local_days_partial() {
    with_test_display_offset(FixedOffset::east_opt(8 * 60 * 60).unwrap(), || {
        let query_now = DateTime::parse_from_rfc3339("2026-08-28T06:51:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for (days, expected_dates) in [(7, 8), (30, 31)] {
            let window = SummaryWindow::new(
                query_now - ChronoDuration::days(days),
                query_now + ChronoDuration::nanoseconds(1),
            )
            .unwrap();
            let mut daily_coverage = expected_summary_daily_coverage(window);
            for coverage in daily_coverage.values_mut() {
                coverage.covered_buckets = coverage.expected_buckets;
            }
            let dates = daily_coverage.keys().copied().collect::<Vec<_>>();
            assert_eq!(dates.len(), expected_dates);
            let prepared = PreparedSummary {
                usage: UsageSummary {
                    window,
                    totals: SummaryMetrics::default(),
                    days: Vec::new(),
                    hours: Vec::new(),
                    projects: Vec::new(),
                },
                range_note: None,
                represented_tokens: 0,
                available_tokens: 0,
                covered_buckets: daily_coverage
                    .values()
                    .map(|coverage| coverage.covered_buckets)
                    .sum(),
                expected_buckets: daily_coverage
                    .values()
                    .map(|coverage| coverage.expected_buckets)
                    .sum(),
                estimated_covered_tokens: 0,
                long_context_breakdown_complete: true,
                daily_coverage,
                hourly_coverage: BTreeMap::new(),
                partial_reasons: Vec::new(),
            };

            assert_eq!(
                prepared.daily_state(
                    dates[0],
                    SummaryMetrics::default(),
                    SummaryMetric::Tokens,
                    false
                ),
                SummaryDailyState::Partial
            );
            assert_eq!(
                prepared.daily_state(
                    *dates.last().unwrap(),
                    SummaryMetrics::default(),
                    SummaryMetric::Tokens,
                    false
                ),
                SummaryDailyState::Partial
            );
            assert!(dates[1..dates.len() - 1].iter().all(|date| {
                prepared.daily_state(
                    *date,
                    SummaryMetrics::default(),
                    SummaryMetric::Tokens,
                    false,
                ) == SummaryDailyState::Complete
            }));
        }
    });
}

#[test]
fn summary_chart_grains_preserve_hourly_totals_and_project_composition() {
    let starts_at = DateTime::parse_from_rfc3339("2026-08-28T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let ends_at = starts_at + ChronoDuration::days(1);
    let window = SummaryWindow::new(starts_at, ends_at).unwrap();
    let mut total = SummaryMetrics::default();
    let mut alpha_total = SummaryMetrics::default();
    let mut beta_total = SummaryMetrics::default();
    let mut hours = Vec::new();
    let mut alpha_hours = Vec::new();
    let mut beta_hours = Vec::new();
    let mut hourly_coverage = BTreeMap::new();
    for hour in 0_i64..24 {
        let starts_at = (window.starts_at + ChronoDuration::hours(hour)).naive_utc();
        let alpha = SummaryMetrics {
            token_usage: TokenUsage {
                input_tokens: (hour + 1) as u64,
                total_tokens: (hour + 1) as u64,
                ..TokenUsage::default()
            },
            ..SummaryMetrics::default()
        };
        let beta = SummaryMetrics {
            token_usage: TokenUsage {
                input_tokens: ((hour + 1) * 2) as u64,
                total_tokens: ((hour + 1) * 2) as u64,
                ..TokenUsage::default()
            },
            ..SummaryMetrics::default()
        };
        let mut combined = alpha;
        combined.add_assign(beta);
        total.add_assign(combined);
        alpha_total.add_assign(alpha);
        beta_total.add_assign(beta);
        hours.push(crate::summary::HourlySummary {
            starts_at,
            totals: combined,
        });
        alpha_hours.push(crate::summary::HourlySummary {
            starts_at,
            totals: alpha,
        });
        beta_hours.push(crate::summary::HourlySummary {
            starts_at,
            totals: beta,
        });
        hourly_coverage.insert(
            starts_at,
            SummaryDailyCoverage {
                expected_buckets: 4,
                covered_buckets: 4,
                available_tokens: combined.token_usage.total_tokens,
                represented_tokens: combined.token_usage.total_tokens,
                estimated_covered_tokens: combined.token_usage.total_tokens,
                ..SummaryDailyCoverage::default()
            },
        );
    }
    let project = |key: &str, totals: SummaryMetrics, hours: Vec<crate::summary::HourlySummary>| {
        ProjectSummary {
            key: key.to_string(),
            label: key.to_string(),
            cwd: None,
            totals,
            days: Vec::new(),
            hours,
            sessions: Vec::new(),
        }
    };
    let prepared = PreparedSummary {
        usage: UsageSummary {
            window,
            totals: total,
            days: Vec::new(),
            hours,
            projects: vec![
                project("alpha", alpha_total, alpha_hours),
                project("beta", beta_total, beta_hours),
            ],
        },
        range_note: None,
        represented_tokens: total.token_usage.total_tokens,
        available_tokens: total.token_usage.total_tokens,
        covered_buckets: 96,
        expected_buckets: 96,
        estimated_covered_tokens: total.token_usage.total_tokens,
        long_context_breakdown_complete: true,
        daily_coverage: BTreeMap::from([(
            starts_at.date_naive(),
            SummaryDailyCoverage {
                expected_buckets: 96,
                covered_buckets: 96,
                available_tokens: total.token_usage.total_tokens,
                represented_tokens: total.token_usage.total_tokens,
                estimated_covered_tokens: total.token_usage.total_tokens,
                ..SummaryDailyCoverage::default()
            },
        )]),
        hourly_coverage,
        partial_reasons: Vec::new(),
    };

    with_test_display_offset(FixedOffset::east_opt(0).unwrap(), || {
        for (grain, expected_buckets) in [
            (SummaryGrain::Day, 1),
            (SummaryGrain::Hours12, 2),
            (SummaryGrain::Hours6, 4),
            (SummaryGrain::Hours3, 8),
            (SummaryGrain::Hour, 24),
        ] {
            let chart = prepare_summary_chart(&prepared, grain);
            assert_eq!(chart.buckets.len(), expected_buckets, "{grain:?}");
            assert_eq!(
                chart
                    .buckets
                    .iter()
                    .map(|bucket| bucket.totals.token_usage.total_tokens)
                    .sum::<u64>(),
                total.token_usage.total_tokens,
                "{grain:?}"
            );
            for bucket in &chart.buckets {
                let project_total = chart
                    .project_values
                    .values()
                    .map(|values| {
                        values
                            .get(&bucket.starts_at)
                            .copied()
                            .unwrap_or_default()
                            .token_usage
                            .total_tokens
                    })
                    .sum::<u64>();
                assert_eq!(project_total, bucket.totals.token_usage.total_tokens);
                assert_eq!(
                    prepared.chart_bucket_state(bucket, grain, SummaryMetric::Tokens, false),
                    SummaryDailyState::Complete
                );
            }
        }
    });
}

#[test]
fn summary_hourly_coverage_handles_dst_fallback_and_spring_forward() {
    let local_hour = |timestamp: DateTime<Utc>, offset_hours: i64| {
        let local = (timestamp + ChronoDuration::hours(offset_hours)).naive_utc();
        local
            .date()
            .and_hms_opt(local.hour(), 0, 0)
            .expect("a shifted UTC timestamp remains a valid local hour")
    };

    let fallback_start = Utc.with_ymd_and_hms(2026, 11, 1, 4, 0, 0).unwrap();
    let fallback_transition = Utc.with_ymd_and_hms(2026, 11, 1, 6, 0, 0).unwrap();
    let fallback = expected_summary_coverage(
        SummaryWindow::new(fallback_start, fallback_start + ChronoDuration::hours(4)).unwrap(),
        |timestamp| {
            local_hour(
                timestamp,
                if timestamp < fallback_transition {
                    -4
                } else {
                    -5
                },
            )
        },
    );
    assert_eq!(
        fallback
            .values()
            .map(|coverage| coverage.expected_buckets)
            .sum::<usize>(),
        16
    );
    assert_eq!(fallback.len(), 3);
    assert_eq!(
        fallback[&NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(1, 0, 0)
            .unwrap()]
            .expected_buckets,
        8
    );

    let spring_start = Utc.with_ymd_and_hms(2026, 3, 8, 5, 0, 0).unwrap();
    let spring_transition = Utc.with_ymd_and_hms(2026, 3, 8, 7, 0, 0).unwrap();
    let spring = expected_summary_coverage(
        SummaryWindow::new(spring_start, spring_start + ChronoDuration::hours(4)).unwrap(),
        |timestamp| {
            local_hour(
                timestamp,
                if timestamp < spring_transition {
                    -5
                } else {
                    -4
                },
            )
        },
    );
    assert_eq!(
        spring
            .values()
            .map(|coverage| coverage.expected_buckets)
            .sum::<usize>(),
        16
    );
    assert_eq!(spring.len(), 4);
    assert!(
        !spring.contains_key(
            &NaiveDate::from_ymd_opt(2026, 3, 8)
                .unwrap()
                .and_hms_opt(2, 0, 0)
                .unwrap()
        )
    );
}

#[test]
fn summary_backfill_is_local_one_time_work_and_detects_incomplete_30d_history() {
    let mut config = CollectConfig {
        lookback_days: 7,
        max_files: 500,
        offline: false,
        ..CollectConfig::default()
    };
    let backfill = summary_backfill_config(&config);
    assert_eq!(backfill.lookback_days, SUMMARY_HISTORY_DAYS);
    assert_eq!(backfill.max_files, SUMMARY_BACKFILL_MAX_FILES);
    assert!(backfill.offline);

    config.lookback_days = 90;
    assert_eq!(
        summary_backfill_config(&config).lookback_days,
        SUMMARY_HISTORY_DAYS
    );

    let now = DateTime::parse_from_rfc3339("2026-08-28T12:07:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut starts_at = now - ChronoDuration::days(30) + ChronoDuration::minutes(8);
    let mut buckets = Vec::new();
    while starts_at <= now {
        buckets.push(LocalHalfHourBucket {
            starts_at,
            ends_at: starts_at + ChronoDuration::minutes(15),
            sampled_at: starts_at + ChronoDuration::minutes(15),
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 0,
            groups: Vec::new(),
            project_groups: Vec::new(),
            partial_reasons: Vec::new(),
        });
        starts_at += ChronoDuration::minutes(15);
    }
    let complete = HistoryData {
        half_hour_buckets: buckets,
        ..HistoryData::default()
    };
    assert!(!summary_history_backfill_needed(&complete, now));

    let mut failed_but_covered = complete.clone();
    failed_but_covered.summary_backfill_attempt_complete = Some(false);
    failed_but_covered.summary_backfill_attempted_at = Some(now - ChronoDuration::days(1));
    assert!(!summary_history_backfill_needed(&failed_but_covered, now));
    failed_but_covered.summary_backfill_attempted_at =
        Some(now - ChronoDuration::days(SUMMARY_BACKFILL_RETRY_DAYS + 1));
    assert!(summary_history_backfill_needed(&failed_but_covered, now));
    failed_but_covered.summary_backfill_attempted_at = Some(now + ChronoDuration::days(1));
    assert!(!summary_history_backfill_needed(&failed_but_covered, now));

    assert!(summary_history_backfill_needed(
        &complete,
        now + ChronoDuration::days(31)
    ));

    let mut incomplete = complete;
    incomplete.half_hour_buckets.pop();
    assert!(summary_history_backfill_needed(&incomplete, now));
    incomplete.summary_backfill_attempted_at = Some(now);
    assert!(!summary_history_backfill_needed(&incomplete, now));
    assert!(summary_history_backfill_needed(
        &incomplete,
        now + ChronoDuration::days(SUMMARY_BACKFILL_RETRY_DAYS + 1)
    ));
    incomplete.read_only = true;
    assert!(!summary_history_backfill_needed(
        &incomplete,
        now + ChronoDuration::days(SUMMARY_BACKFILL_RETRY_DAYS + 1)
    ));
}

#[test]
fn summary_backfill_scan_completeness_rejects_ambiguous_or_partial_sources() {
    let mut snapshot = mouse_test_app(0).snapshot;
    snapshot.stats = CollectionStats {
        discovered_files: 1,
        scanned_files: 1,
        ..CollectionStats::default()
    };
    snapshot.sources = vec![SourceStatus {
        source: "rollout_jsonl".to_string(),
        status: "ok".to_string(),
        as_of: snapshot.as_of,
        message: None,
    }];
    assert!(summary_backfill_scan_complete(&snapshot));

    snapshot.stats.ambiguous_token_resets = 1;
    assert!(!summary_backfill_scan_complete(&snapshot));
    snapshot.stats.ambiguous_token_resets = 0;
    snapshot.sources[0].status = "partial".to_string();
    assert!(!summary_backfill_scan_complete(&snapshot));
}

#[test]
fn summary_backfill_persists_only_buckets_with_usage_evidence() {
    let now = DateTime::parse_from_rfc3339("2026-08-28T12:07:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let first_usage = DateTime::parse_from_rfc3339("2026-08-26T10:15:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let call = UsageCall {
        timestamp: first_usage,
        thread_id: "thread-with-evidence".to_string(),
        turn_id: Some("turn-with-evidence".to_string()),
        usage_event_id: None,
        usage_event_identity_exact: false,
        model: Some("gpt-5.6-sol".to_string()),
        service_tier: None,
        tokens: TokenUsage {
            input_tokens: 10,
            total_tokens: 10,
            ..TokenUsage::default()
        },
        request_usage_exact: true,
    };
    let coverage_starts_at = now - ChronoDuration::days(SUMMARY_HISTORY_DAYS);
    let mut observation = HistoryObservation::from_sources_with_tasks_and_coverage(
        now,
        &[call],
        &[],
        &[],
        &[],
        Some(coverage_starts_at),
    );

    assert!(
        observation
            .half_hour_buckets
            .first()
            .is_some_and(|bucket| bucket.starts_at < first_usage)
    );
    retain_summary_backfill_evidence_buckets(&mut observation);
    assert_eq!(observation.half_hour_buckets.len(), 1);
    assert_eq!(
        observation
            .half_hour_buckets
            .first()
            .map(|bucket| bucket.starts_at),
        Some(first_usage)
    );
    assert!(
        observation
            .half_hour_buckets
            .first()
            .is_some_and(|bucket| bucket.call_count == 1)
    );

    let mut empty = HistoryObservation::from_sources_with_tasks_and_coverage(
        now,
        &[],
        &[],
        &[],
        &[],
        Some(coverage_starts_at),
    );
    assert!(!empty.half_hour_buckets.is_empty());
    retain_summary_backfill_evidence_buckets(&mut empty);
    assert!(empty.half_hour_buckets.is_empty());

    let mut spark_only = HistoryObservation {
        half_hour_buckets: vec![LocalHalfHourBucket {
            starts_at: first_usage,
            ends_at: first_usage + ChronoDuration::minutes(15),
            sampled_at: first_usage + ChronoDuration::minutes(15),
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 0,
            groups: Vec::new(),
            project_groups: vec![LocalProjectUsageGroup {
                thread_id: "spark-thread".to_string(),
                ..LocalProjectUsageGroup::default()
            }],
            partial_reasons: Vec::new(),
        }],
        ..HistoryObservation::default()
    };
    retain_summary_backfill_evidence_buckets(&mut spark_only);
    assert_eq!(spark_only.half_hour_buckets.len(), 1);
}

#[cfg(not(windows))]
#[test]
fn button_mouse_capture_keeps_click_drag_and_wide_coordinates_without_hover_events() {
    let mut output = Vec::new();
    enable_button_mouse_capture(&mut output).unwrap();

    let encoded = String::from_utf8(output).unwrap();
    assert!(encoded.contains("?1000h"));
    assert!(encoded.contains("?1002h"));
    assert!(encoded.contains("?1006h"));
    assert!(!encoded.contains("?1003h"));

    let mut output = Vec::new();
    disable_button_mouse_capture(&mut output).unwrap();
    let encoded = String::from_utf8(output).unwrap();
    assert!(encoded.contains("?1002l"));
    assert!(!encoded.contains("?1003l"));
}

#[test]
fn redraw_reasons_are_coalesced_for_one_logged_frame() {
    let mut reasons = RedrawReasons::default();
    assert!(reasons.is_empty());

    reasons.insert(RedrawReasons::INPUT);
    reasons.insert(RedrawReasons::SNAPSHOT);
    reasons.insert(RedrawReasons::NOTICE);
    assert_eq!(reasons.label(), "input+snapshot+notice");

    reasons.clear();
    assert!(reasons.is_empty());
}

#[test]
fn run_loop_poll_timeout_sleeps_until_work_and_checks_workers_promptly() {
    let mut app = mouse_test_app(1);
    let now = Instant::now();
    app.last_local_refresh = now.checked_sub(Duration::from_millis(500)).unwrap();

    assert_eq!(
        next_run_loop_poll_timeout(&app, now, false),
        Duration::from_millis(1_500)
    );

    app.worker_running = true;
    assert_eq!(
        next_run_loop_poll_timeout(&app, now, false),
        BACKGROUND_CHANNEL_POLL
    );

    app.worker_running = false;
    app.launching_threads.insert("opening".to_string());
    assert_eq!(
        next_run_loop_poll_timeout(&app, now, false),
        BACKGROUND_CHANNEL_POLL
    );

    app.launching_threads.clear();
    app.open_notice = Some(OpenNotice {
        message: "brief".to_string(),
        tone: OpenNoticeTone::Info,
        created_at: now.checked_sub(Duration::from_millis(7_900)).unwrap(),
    });
    assert_eq!(
        next_run_loop_poll_timeout(&app, now, false),
        Duration::from_millis(100)
    );
}

#[test]
fn termination_request_wakes_the_run_loop_without_terminal_io() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let termination = TerminationSignal::for_test();
    assert!(!termination.is_requested());
    assert_eq!(
        termination.poll_timeout(Duration::from_secs(30)),
        BACKGROUND_CHANNEL_POLL
    );

    termination.request_for_test();
    assert!(termination.is_requested());
}

#[cfg(windows)]
#[test]
fn windows_close_handler_waits_until_terminal_restoration() {
    use windows_sys::Win32::System::Console::CTRL_CLOSE_EVENT;

    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let termination = TerminationSignal::for_test();
    let handler = thread::spawn(|| unsafe { windows_console_ctrl_handler(CTRL_CLOSE_EVENT) });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !WINDOWS_TERMINATION_REQUESTED.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "console handler did not start");
        thread::yield_now();
    }
    assert!(
        !handler.is_finished(),
        "close handler returned before terminal restoration"
    );

    termination.mark_terminal_restored();
    assert_eq!(handler.join().unwrap(), 1);
}

#[test]
fn account_refresh_is_due_immediately_after_app_creation() {
    let app = mouse_test_app(1);
    let now = Instant::now();

    assert!(app.account_refresh_due(now));
    assert_eq!(next_run_loop_poll_timeout(&app, now, true), Duration::ZERO);
}

#[test]
fn reset_credit_fetch_status_distinguishes_initial_load_and_retry() {
    let mut app = mouse_test_app(1);
    let now = Instant::now();
    app.snapshot.sources = vec![SourceStatus {
        source: "app_server".to_string(),
        status: "stale".to_string(),
        as_of: Utc::now(),
        message: Some("no cached account snapshot".to_string()),
    }];

    assert_eq!(app.reset_credit_fetch_status(now), Some("loading"));
    app.account_refresh_retry_count = 1;
    assert_eq!(app.reset_credit_fetch_status(now), Some("retrying"));

    app.snapshot.rate_limit_reset_credits = Some(RateLimitResetCreditsSnapshot {
        available_count: 2,
        credits: None,
        provenance: Provenance::ServerSnapshot,
        as_of: Utc::now(),
    });
    assert_eq!(app.reset_credit_fetch_status(now), None);

    app.snapshot.rate_limit_reset_credits = Some(RateLimitResetCreditsSnapshot {
        available_count: 0,
        credits: None,
        provenance: Provenance::ServerSnapshot,
        as_of: Utc::now(),
    });
    assert_eq!(app.reset_credit_fetch_status(now), None);

    app.snapshot.sources[0].status = "partial".to_string();
    app.snapshot.sources[0].message = None;
    app.snapshot.rate_limit_reset_credits = None;
    app.snapshot.rate_limit_reset_credits_partial = true;
    assert_eq!(app.reset_credit_fetch_status(now), Some("retrying"));
    app.snapshot.rate_limit_reset_credits_partial = false;
    assert_eq!(app.reset_credit_fetch_status(now), None);
}

#[test]
fn incomplete_account_refreshes_back_off_before_returning_to_normal_period() {
    let mut app = mouse_test_app(1);
    let first_attempt = Instant::now();
    let missing_reset_snapshot = account_refresh_result("partial", None, true);
    app.schedule_next_account_refresh(&missing_reset_snapshot, first_attempt);
    assert_eq!(
        app.next_account_refresh
            .saturating_duration_since(first_attempt),
        Duration::from_secs(5)
    );

    let second_attempt = first_attempt + Duration::from_secs(5);
    let count_only_reset_snapshot = account_refresh_result(
        "ok",
        Some(RateLimitResetCreditsSnapshot {
            available_count: 2,
            credits: None,
            provenance: Provenance::ServerSnapshot,
            as_of: Utc::now(),
        }),
        false,
    );
    app.schedule_next_account_refresh(&count_only_reset_snapshot, second_attempt);
    assert_eq!(
        app.next_account_refresh
            .saturating_duration_since(second_attempt),
        ACCOUNT_REFRESH
    );
    assert_eq!(app.account_refresh_retry_count, 0);
}

#[test]
fn complete_account_refresh_uses_normal_period_and_resets_backoff() {
    let mut app = mouse_test_app(1);
    let failed_at = Instant::now();
    app.schedule_next_account_refresh(&account_refresh_result("error", None, true), failed_at);
    assert_eq!(app.account_refresh_retry_count, 1);

    let succeeded_at = failed_at + Duration::from_secs(5);
    let complete = account_refresh_result(
        // account/usage warnings make the aggregate source partial, but the
        // independently parsed rate limits and reset details are complete.
        "partial",
        Some(RateLimitResetCreditsSnapshot {
            available_count: 0,
            credits: Some(Vec::new()),
            provenance: Provenance::ServerSnapshot,
            as_of: Utc::now(),
        }),
        false,
    );
    app.schedule_next_account_refresh(&complete, succeeded_at);

    assert_eq!(app.account_refresh_retry_count, 0);
    assert_eq!(
        app.next_account_refresh
            .saturating_duration_since(succeeded_at),
        ACCOUNT_REFRESH
    );

    let without_reset_credit_support = account_refresh_result("partial", None, false);
    assert!(account_refresh_is_complete(&without_reset_credit_support));

    let truncated_reset_credit_details = account_refresh_result(
        "partial",
        Some(RateLimitResetCreditsSnapshot {
            available_count: 2,
            credits: Some(Vec::new()),
            provenance: Provenance::ServerSnapshot,
            as_of: Utc::now(),
        }),
        false,
    );
    assert!(account_refresh_is_complete(&truncated_reset_credit_details));
}

#[test]
fn bootstrap_history_defers_server_points_online_and_preserves_them_offline() {
    let now = Utc::now();
    let bucket = LocalHalfHourBucket {
        starts_at: now - ChronoDuration::minutes(15),
        ends_at: now,
        sampled_at: now,
        token_usage: TokenUsage {
            total_tokens: 42,
            ..TokenUsage::default()
        },
        estimated_cost_units: 7,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: 1,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 1,
        groups: Vec::new(),
        project_groups: Vec::new(),
        partial_reasons: Vec::new(),
    };
    let observation = HistoryObservation {
        observed_at: now,
        quota_points: vec![QuotaPoint {
            observed_at: now,
            limit_id: "codex".to_string(),
            duration_mins: 10_080,
            resets_at: now + ChronoDuration::days(1),
            used_percent: 25.0,
            remaining_percent: 75.0,
            provenance: Provenance::Stale,
        }],
        half_hour_buckets: vec![bucket.clone()],
        weekly_local_points: vec![WeeklyLocalPoint {
            observed_at: now,
            resets_at: now + ChronoDuration::days(1),
            token_usage: TokenUsage {
                total_tokens: 42,
                ..TokenUsage::default()
            },
            estimated_cost_units: 7,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: 1,
            call_count: 1,
            partial_reasons: vec!["weekly_window_stale".to_string()],
        }],
    };

    let mut failed_refresh = account_refresh_result("error", None, true);
    failed_refresh.history_observation = observation.clone();
    let deferred = collection_history_observation(&failed_refresh, false);

    assert_eq!(deferred.observed_at, now);
    assert!(deferred.quota_points.is_empty());
    assert_eq!(deferred.half_hour_buckets, vec![bucket]);
    assert!(deferred.weekly_local_points.is_empty());
    assert!(matches!(&deferred, Cow::Owned(_)));

    let offline = collection_history_observation(&failed_refresh, true);
    assert_eq!(offline.as_ref(), &observation);
    assert!(matches!(&offline, Cow::Borrowed(_)));
}

#[test]
fn mouse_moves_never_request_a_redraw_and_notices_expire_once() {
    let mut app = mouse_test_app(1);
    app.open_quit_confirmation();
    let moved = mouse_event(MouseEventKind::Moved, 1, 1);
    let handled = handle_mouse_event(&mut app, moved);
    assert!(
        handled,
        "the modal still consumes background mouse movement"
    );
    assert!(!mouse_event_requests_redraw(MouseEventKind::Moved, handled));
    assert!(mouse_event_requests_redraw(
        MouseEventKind::ScrollDown,
        true
    ));
    assert!(mouse_event_requests_redraw(
        MouseEventKind::Down(MouseButton::Left),
        false
    ));
    assert!(!mouse_event_requests_redraw(
        MouseEventKind::ScrollDown,
        false
    ));

    let now = Instant::now();
    app.open_notice = Some(OpenNotice {
        message: "expired".to_string(),
        tone: OpenNoticeTone::Info,
        created_at: now.checked_sub(OPEN_NOTICE_DURATION).unwrap(),
    });
    assert!(app.expire_open_notice_at(now));
    assert!(app.open_notice.is_none());
    assert!(!app.expire_open_notice_at(now));
}

fn mouse_test_app(task_count: usize) -> App {
    interaction_test_app(task_count, 0)
}

fn account_refresh_result(
    app_server_status: &str,
    reset_credits: Option<RateLimitResetCreditsSnapshot>,
    reset_credits_partial: bool,
) -> CollectionResult {
    let app = mouse_test_app(0);
    let now = Utc::now();
    let limits = (app_server_status != "error")
        .then(|| LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: None,
            plan_type: Some("test".to_string()),
            primary: None,
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        })
        .into_iter()
        .collect::<Vec<_>>();
    let mut snapshot = app.snapshot.clone();
    snapshot.sources = vec![SourceStatus {
        source: "app_server".to_string(),
        status: app_server_status.to_string(),
        as_of: now,
        message: None,
    }];
    snapshot.limits.clone_from(&limits);
    snapshot.rate_limit_reset_credits = reset_credits.clone();
    snapshot.rate_limit_reset_credits_partial = reset_credits_partial;
    CollectionResult {
        snapshot,
        account: AccountSnapshot {
            limits,
            rate_limit_reset_credits: reset_credits,
            rate_limit_reset_credits_partial: reset_credits_partial,
            ..AccountSnapshot::default()
        },
        history_observation: crate::history::HistoryObservation::default(),
        local_session_digests: Default::default(),
    }
}

fn interaction_test_app(task_count: usize, turns_per_task: usize) -> App {
    let now = chrono::Utc::now();
    let tasks = (0..task_count)
        .map(|index| TaskRecord {
            thread_id: format!("task-thread-{index}"),
            archived: false,
            title: format!("task {index}"),
            cwd: Some("/tmp/project".into()),
            source: Some("desktop".to_string()),
            parent_thread_id: None,
            created_at: Some(now),
            updated_at: Some(now),
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage {
                total_tokens: index as u64 + 1,
                ..TokenUsage::default()
            },
            turn_count: turns_per_task,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: Default::default(),
        })
        .collect();
    let mut turns = Vec::new();
    for task_index in 0..task_count {
        for turn_index in 0..turns_per_task {
            let token_base = (turn_index as u64 + 1) * 100;
            turns.push(TurnRecord {
                thread_id: format!("task-thread-{task_index}"),
                turn_id: format!("turn-{task_index}-{turn_index}"),
                model: Some(format!("model-{turn_index}")),
                reasoning_effort: Some("high".to_string()),
                service_tier: None,
                message_preview: Some(format!("message {task_index}/{turn_index}")),
                started_at: Some(now - chrono::Duration::seconds(turn_index as i64 + 2)),
                completed_at: Some(now - chrono::Duration::seconds(turn_index as i64)),
                duration_ms: Some(2_000),
                status: TurnStatus::Completed,
                token_usage: TokenUsage {
                    input_tokens: token_base / 2,
                    cached_input_tokens: token_base / 5,
                    cache_write_input_tokens: 0,
                    output_tokens: token_base / 3,
                    reasoning_output_tokens: token_base / 10,
                    total_tokens: token_base,
                },
                window_token_usage: TokenUsage {
                    input_tokens: token_base / 4,
                    cached_input_tokens: token_base / 10,
                    cache_write_input_tokens: 0,
                    output_tokens: token_base / 6,
                    reasoning_output_tokens: token_base / 20,
                    total_tokens: token_base / 2,
                },
                local_token_share_percent: turn_index as f64,
                estimated_quota_percent: turn_index as f64 / 10.0,
                quota_confidence: Confidence::Medium,
                api_equivalent_cost: Default::default(),
            });
        }
    }
    App::new(
        CollectionResult {
            snapshot: Snapshot {
                schema_version: 1,
                api_pricing: Default::default(),
                api_equivalent_cost: Default::default(),
                as_of: now,
                partial: false,
                codex_home: "/tmp/.codex".into(),
                sources: Vec::new(),
                limits: Vec::new(),
                rate_limit_reset_credits: None,
                rate_limit_reset_credits_partial: false,
                account_usage: None,
                tasks,
                turns,
                models: Vec::new(),
                attribution: AttributionSummary::default(),
                window_analyses: Vec::new(),
                stats: CollectionStats::default(),
                warnings: Vec::new(),
                errors: Vec::new(),
            },
            account: AccountSnapshot::default(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        Theme::Light,
    )
}

fn remote_live_fixture(
    received_at: DateTime<Utc>,
    range_complete: bool,
) -> SourceRemoteLiveSnapshot {
    let captured_at = received_at - ChronoDuration::minutes(1);
    let node_id: NodeId = "node-0123456789abcdef0123456789abcdef".parse().unwrap();
    SourceRemoteLiveSnapshot {
        source: SourceMetadata::new(node_id.clone(), SourceKind::Ssh, "dev-server").unwrap(),
        source_generation: SourceGeneration {
            node_id,
            generation: NonZeroU64::new(1).unwrap(),
        },
        revisions: crate::remote_agent::current_revisions(),
        redaction_profile: RedactionProfile::Redacted,
        live_revision: NonZeroU64::new(1).unwrap(),
        snapshot: RemoteLiveSnapshot {
            captured_at,
            tasks: vec![RemoteLiveTask {
                thread_id: "remote-thread".parse().unwrap(),
                parent_thread_id: None,
                observed_project_key: None,
                title_preview: None,
                created_at: Some(captured_at - ChronoDuration::minutes(1)),
                updated_at: captured_at,
                status: TaskStatus::Running,
                token_usage: RemoteTokenUsage {
                    input_tokens: 8,
                    output_tokens: 2,
                    total_tokens: 10,
                    ..RemoteTokenUsage::default()
                },
                turn_count: 1,
            }],
            turns: vec![RemoteLiveTurn {
                thread_id: "remote-thread".parse().unwrap(),
                turn_id: "turn-1".to_owned(),
                model: Some("gpt-5.6-sol".to_owned()),
                reasoning_effort: Some("high".to_owned()),
                service_tier: None,
                message_preview: None,
                started_at: Some(captured_at),
                completed_at: None,
                status: TurnStatus::InProgress,
                token_usage: RemoteTokenUsage {
                    input_tokens: 8,
                    output_tokens: 2,
                    total_tokens: 10,
                    ..RemoteTokenUsage::default()
                },
            }],
        },
        project_descriptors: Vec::new(),
        remote_observed_at: captured_at,
        received_at,
        range_complete,
        partial_reasons: (!range_complete)
            .then(|| "rollout_scan_incomplete".to_owned())
            .into_iter()
            .collect(),
        warning_codes: Vec::new(),
    }
}

#[test]
fn unchanged_remote_live_state_crosses_stale_boundary_once_including_error_refreshes() {
    let mut app = interaction_test_app(0, 0);
    let local_as_of = app.snapshot.as_of;
    let received_at = local_as_of + ChronoDuration::hours(1);
    let state = remote_live_fixture(received_at, true);
    let fresh_at = received_at + ChronoDuration::minutes(1);

    assert!(app.replace_remote_live_states_at(vec![state.clone()], fresh_at));
    assert_eq!(app.snapshot.as_of, local_as_of);
    assert_eq!(app.snapshot.sources[0].status, "ok");
    assert_eq!(app.snapshot.tasks[0].status, TaskStatus::Running);
    assert_eq!(
        app.snapshot.tasks[0].status_provenance,
        Provenance::Inferred
    );
    assert_eq!(app.snapshot.turns[0].status, TurnStatus::InProgress);
    assert!(!app.replace_remote_live_states_at(
        vec![state.clone()],
        received_at + ChronoDuration::minutes(15),
    ));

    let stale_at = received_at + ChronoDuration::minutes(16);
    assert!(app.record_remote_live_load_error_at("disk busy".to_owned(), stale_at));
    assert_eq!(app.snapshot.sources[0].status, "stale");
    assert_eq!(app.snapshot.tasks[0].status, TaskStatus::Stale);
    assert_eq!(app.snapshot.turns[0].status, TurnStatus::Stale);
    assert!(
        app.snapshot
            .warnings
            .iter()
            .any(|warning| warning.starts_with("remote live load failed:"))
    );
    assert!(!app.record_remote_live_load_error_at(
        "disk busy".to_owned(),
        stale_at + ChronoDuration::minutes(1),
    ));
    assert!(app.replace_remote_live_states_at(vec![state], stale_at + ChronoDuration::minutes(1),));
    assert!(!has_remote_live_load_error(&app.snapshot));
    let unchanged = app.remote_live_states.clone();
    assert!(!app.replace_remote_live_states_at(unchanged, stale_at + ChronoDuration::minutes(2),));
}

#[test]
fn remote_live_partial_error_and_source_removal_restore_local_quality() {
    let mut app = interaction_test_app(0, 0);
    let local_as_of = app.snapshot.as_of;
    let received_at = local_as_of + ChronoDuration::hours(1);
    let partial = remote_live_fixture(received_at, false);

    assert!(
        app.replace_remote_live_states_at(vec![partial], received_at + ChronoDuration::minutes(1),)
    );
    assert!(app.snapshot.partial);
    assert_eq!(app.snapshot.sources[0].status, "partial");

    let complete = remote_live_fixture(received_at + ChronoDuration::minutes(2), true);
    assert!(app.replace_remote_live_states_at(
        vec![complete.clone()],
        received_at + ChronoDuration::minutes(3),
    ));
    assert!(!app.snapshot.partial);
    assert_eq!(app.snapshot.sources[0].status, "ok");

    assert!(app.record_remote_live_load_error_at(
        "temporarily unreadable".to_owned(),
        received_at + ChronoDuration::minutes(4),
    ));
    assert!(app.snapshot.partial);
    // A successful read with identical state must clear the transient error
    // instead of taking the ordinary unchanged-state fast path.
    assert!(app.replace_remote_live_states_at(
        vec![complete],
        received_at + ChronoDuration::minutes(5),
    ));
    assert!(!app.snapshot.partial);
    assert!(!has_remote_live_load_error(&app.snapshot));

    assert!(
        app.replace_remote_live_states_at(Vec::new(), received_at + ChronoDuration::minutes(6),)
    );
    assert!(!app.snapshot.partial);
    assert!(app.snapshot.sources.is_empty());
    assert!(app.snapshot.tasks.is_empty());
    assert!(app.snapshot.turns.is_empty());
    assert_eq!(app.snapshot.as_of, local_as_of);
}

#[test]
fn collapsed_remote_subtree_uses_projected_window_tokens_not_live_cumulative_tokens() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::Week, 100, 100.0);
    app.local_snapshot = app.snapshot.clone();
    let received_at = app.snapshot.as_of + ChronoDuration::hours(1);
    let mut state = remote_live_fixture(received_at, true);
    let node_id = state.source.source_id().as_str().to_owned();
    let parent_id = state.snapshot.tasks[0].thread_id.clone();
    let mut child = state.snapshot.tasks[0].clone();
    child.thread_id = "remote-child".parse().unwrap();
    child.parent_thread_id = Some(parent_id);
    child.token_usage = RemoteTokenUsage {
        input_tokens: 16,
        output_tokens: 4,
        total_tokens: 20,
        ..RemoteTokenUsage::default()
    };
    child.turn_count = 0;
    state.snapshot.tasks.push(child);
    state
        .snapshot
        .tasks
        .sort_by(|left, right| left.thread_id.as_str().cmp(right.thread_id.as_str()));

    assert!(
        app.replace_remote_live_states_at(vec![state], received_at + ChronoDuration::minutes(1),)
    );
    let root_thread_id = remote_live_thread_id(&node_id, "remote-thread");
    let child_thread_id = remote_live_thread_id(&node_id, "remote-child");
    let root_index = app
        .snapshot
        .tasks
        .iter()
        .position(|task| task.thread_id == root_thread_id)
        .unwrap();
    let child_index = app
        .snapshot
        .tasks
        .iter()
        .position(|task| task.thread_id == child_thread_id)
        .unwrap();
    assert_eq!(
        task_usage_for_scope(
            &app.snapshot,
            WindowScope::Week,
            &app.snapshot.tasks[root_index],
        ),
        WindowUsage::default(),
        "analysis misses must not reinterpret a cumulative remote live counter as window usage",
    );
    let analysis = app
        .snapshot
        .window_analyses
        .iter_mut()
        .find(|analysis| analysis.duration_mins == WindowScope::Week.duration_mins())
        .unwrap();
    analysis.threads.extend([
        ThreadWindowUsage {
            thread_id: root_thread_id.clone(),
            usage: WindowUsage {
                token_usage: TokenUsage {
                    input_tokens: 8,
                    output_tokens: 2,
                    total_tokens: 10,
                    ..TokenUsage::default()
                },
                ..WindowUsage::default()
            },
        },
        ThreadWindowUsage {
            thread_id: child_thread_id.clone(),
            usage: WindowUsage {
                token_usage: TokenUsage {
                    input_tokens: 16,
                    output_tokens: 4,
                    total_tokens: 20,
                    ..TokenUsage::default()
                },
                ..WindowUsage::default()
            },
        },
    ]);
    let collapsed = TaskListRow {
        index: root_index,
        prefix: String::new(),
        depth: 0,
        has_children: true,
        collapsed: true,
        hidden_descendants: vec![child_index],
    };

    let usage = aggregate_task_row_usage(&app.snapshot, WindowScope::Week, &collapsed, true);
    assert_eq!(usage.token_usage.input_tokens, 24);
    assert_eq!(usage.token_usage.output_tokens, 6);
    assert_eq!(usage.token_usage.total_tokens, 30);
    assert_eq!(usage.quota_confidence, Confidence::Unknown);
}

fn remote_overview_history_fixture(as_of: DateTime<Utc>) -> RemoteOverviewHistory {
    let node: NodeId = "node-0123456789abcdef0123456789abcdef".parse().unwrap();
    let group = LocalProjectUsageGroup {
        thread_id: format!("remote-thread@{node}"),
        turn_id: Some(format!("turn-1@{node}")),
        session_thread_id: Some(format!("remote-thread@{node}")),
        message_preview: Some("remote prompt".to_owned()),
        project_label: Some("remote-project".to_owned()),
        title: Some("Remote history task".to_owned()),
        token_usage: TokenUsage {
            input_tokens: 24,
            output_tokens: 6,
            total_tokens: 30,
            ..TokenUsage::default()
        },
        estimated_cost_units: 30,
        api_equivalent_cost: exact_api_cost(30),
        call_count: 1,
        ..LocalProjectUsageGroup::default()
    };
    let history = HistoryData {
        half_hour_buckets: vec![LocalHalfHourBucket {
            starts_at: as_of - ChronoDuration::minutes(15),
            ends_at: as_of,
            sampled_at: as_of,
            token_usage: group.token_usage,
            estimated_cost_units: group.estimated_cost_units,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: 1,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 1,
            groups: Vec::new(),
            project_groups: vec![group],
            partial_reasons: Vec::new(),
        }],
        ..HistoryData::default()
    };
    RemoteOverviewHistory::from_unified(&history, [(node, "dev-server".to_owned())], as_of)
}

#[test]
fn remote_history_and_live_overlay_are_idempotent_and_never_double_count() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    add_window_analysis(&mut app, WindowScope::Week, 100, 100.0);
    app.local_snapshot = app.snapshot.clone();
    let history = remote_overview_history_fixture(app.snapshot.as_of);
    let received_at = app.snapshot.as_of + ChronoDuration::minutes(1);
    let live = remote_live_fixture(received_at, true);

    let mut snapshot = app.local_snapshot.clone();
    merge_remote_live_into_snapshot_at(
        &mut snapshot,
        std::slice::from_ref(&live),
        &history,
        received_at,
    );
    merge_remote_live_into_snapshot_at(
        &mut snapshot,
        std::slice::from_ref(&live),
        &history,
        received_at,
    );
    let remote_id = remote_live_thread_id(live.source.source_id().as_str(), "remote-thread");
    assert_eq!(
        snapshot
            .tasks
            .iter()
            .filter(|task| task.thread_id == remote_id)
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .turns
            .iter()
            .filter(|turn| turn.thread_id == remote_id && turn.turn_id == "turn-1")
            .count(),
        1
    );
    for duration in [300, 10_080] {
        let analysis = snapshot
            .window_analyses
            .iter()
            .find(|analysis| analysis.duration_mins == duration)
            .unwrap();
        let remote = analysis
            .threads
            .iter()
            .find(|usage| usage.thread_id == remote_id)
            .unwrap();
        assert_eq!(remote.usage.token_usage.total_tokens, 30);
        assert_eq!(
            analysis
                .threads
                .iter()
                .filter(|usage| usage.thread_id == remote_id)
                .count(),
            1
        );
    }

    merge_remote_live_into_snapshot_at(
        &mut snapshot,
        &[],
        &RemoteOverviewHistory::default(),
        received_at,
    );
    assert!(
        snapshot
            .tasks
            .iter()
            .all(|task| !task.thread_id.starts_with("remote:"))
    );
    assert!(
        snapshot
            .turns
            .iter()
            .all(|turn| !turn.thread_id.starts_with("remote:"))
    );
}

#[test]
fn unified_remote_replica_marker_trusts_history_parent_edge() {
    let mut app = interaction_test_app(2, 0);
    let parent = app.snapshot.tasks[0].clone();
    let mut child = app.snapshot.tasks[1].clone();
    child.source = Some("remote:replica".to_owned());
    child.parent_thread_id = Some(parent.thread_id.clone());
    app.trusted_remote_parent_edges
        .insert((child.thread_id.clone(), parent.thread_id.clone()));
    assert!(app.trusts_task_parent_edge(&child, &parent));
}

#[test]
fn logical_remote_live_overlay_keeps_host_label_tree_and_single_parent() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::Week, 100, 100.0);
    app.local_snapshot = app.snapshot.clone();
    let as_of = app.snapshot.as_of;
    let node: NodeId = "node-0123456789abcdef0123456789abcdef".parse().unwrap();
    let make_group = |thread: &str, parent: Option<&str>, tokens: u64| LocalProjectUsageGroup {
        thread_id: format!("logical-thread:{thread}"),
        turn_id: Some(format!("{thread}-turn@{node}")),
        parent_thread_id: parent.map(|parent| format!("logical-thread:{parent}")),
        session_thread_id: Some("logical-thread:parent".to_owned()),
        title: Some(thread.to_owned()),
        token_usage: TokenUsage {
            input_tokens: tokens,
            total_tokens: tokens,
            ..TokenUsage::default()
        },
        estimated_cost_units: u128::from(tokens),
        call_count: 1,
        ..LocalProjectUsageGroup::default()
    };
    let groups = vec![
        make_group("parent", None, 10),
        make_group("child", Some("parent"), 20),
    ];
    let history_data = HistoryData {
        half_hour_buckets: vec![LocalHalfHourBucket {
            starts_at: as_of - ChronoDuration::minutes(15),
            ends_at: as_of,
            sampled_at: as_of,
            token_usage: TokenUsage {
                input_tokens: 30,
                total_tokens: 30,
                ..TokenUsage::default()
            },
            estimated_cost_units: 30,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: 1,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 2,
            groups: Vec::new(),
            project_groups: groups,
            partial_reasons: Vec::new(),
        }],
        ..HistoryData::default()
    };
    let history = RemoteOverviewHistory::from_unified(
        &history_data,
        [(node.clone(), "dev-server".to_owned())],
        as_of,
    );
    let mut live = remote_live_fixture(as_of + ChronoDuration::minutes(1), true);
    live.snapshot.tasks[0].thread_id = "parent".parse().unwrap();
    let mut child = live.snapshot.tasks[0].clone();
    child.thread_id = "child".parse().unwrap();
    child.parent_thread_id = Some("parent".parse().unwrap());
    live.snapshot.tasks.push(child);

    let mut snapshot = app.local_snapshot.clone();
    let trusted_edges = merge_remote_live_into_snapshot_at(&mut snapshot, &[live], &history, as_of);
    let parent = snapshot
        .tasks
        .iter()
        .find(|task| task.thread_id == "parent")
        .unwrap();
    let child = snapshot
        .tasks
        .iter()
        .find(|task| task.thread_id == "child")
        .unwrap();
    assert_eq!(parent.source.as_deref(), Some("remote:dev-server"));
    assert_eq!(child.source.as_deref(), Some("remote:dev-server"));
    assert_eq!(child.parent_thread_id.as_deref(), Some("parent"));
    assert_eq!(
        snapshot
            .tasks
            .iter()
            .filter(|task| task.thread_id == "parent")
            .count(),
        1
    );
    assert!(trusted_edges.contains(&("child".to_owned(), "parent".to_owned())));

    app.snapshot = snapshot;
    app.trusted_remote_parent_edges = trusted_edges;
    app.task_list_mode = TaskListMode::Tree;
    let rows = app.filtered_task_rows_with_expanded(None);
    let child_index = app
        .snapshot
        .tasks
        .iter()
        .position(|task| task.thread_id == "child")
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|row| row.index == child_index)
            .unwrap()
            .depth,
        1
    );
}

fn install_remote_sources_fixture(
    app: &mut App,
    directory: &Path,
    now: DateTime<Utc>,
) -> (RemotesConfigStore, RemoteSyncHealthStore) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config_store = RemotesConfigStore::new(directory.join("config").join("remotes.json"));
    let mut transaction = config_store.begin_transaction().unwrap();
    transaction
        .apply(RemotesConfigMutation::add_host("dev", "dev-box"))
        .unwrap();
    transaction
        .apply(RemotesConfigMutation::pair_pin(
            "dev",
            SourceGeneration {
                node_id: "node-11111111111111111111111111111111".parse().unwrap(),
                generation: NonZeroU64::new(1).unwrap(),
            },
        ))
        .unwrap();
    transaction
        .apply(RemotesConfigMutation::enable_host("dev"))
        .unwrap();
    transaction
        .apply(RemotesConfigMutation::add_host("lab", "lab-box"))
        .unwrap();
    transaction
        .apply(RemotesConfigMutation::set_auto_sync_enabled(true))
        .unwrap();
    let config = config_store.commit(transaction).unwrap();

    let health_store = RemoteSyncHealthStore::new(directory.to_path_buf());
    health_store
        .record_success(
            "dev",
            config.host("dev").unwrap().expected_source(),
            now,
            &RemoteSyncReport {
                pages_committed: 2,
                changes_committed: 17,
                live_state_changed: false,
                response_bytes: 4_096,
                completion: RemoteSyncCompletion::Complete,
            },
            Some(now + ChronoDuration::minutes(1)),
        )
        .unwrap();
    health_store
        .record_failure(
            "lab",
            None,
            now - ChronoDuration::minutes(2),
            RemoteSyncErrorCategory::Transport,
            Some(now + ChronoDuration::minutes(3)),
        )
        .unwrap();

    app.remote_config_store = config_store.clone();
    app.remote_health_store = Some(health_store.clone());
    app.remote_bandwidth_store = Some(RemoteBandwidthBudgetStore::new(directory.to_path_buf()));
    app.local_redact_content = true;
    app.reload_remote_sources();
    (config_store, health_store)
}

fn install_empty_remote_sources_fixture(app: &mut App, directory: &Path) -> RemotesConfigStore {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config_store = RemotesConfigStore::new(directory.join("config").join("remotes.json"));
    let config = config_store.load_or_create().unwrap();
    app.remote_config_store = config_store.clone();
    app.remote_sources.config = Some(config);
    app.remote_sources.config_error = None;
    app.remote_sources.history_sources.clear();
    config_store
}

fn record_remote_bandwidth(app: &App, host_id: &str, recorded_at: DateTime<Utc>, bytes: usize) {
    let source = app
        .remote_sources
        .config
        .as_ref()
        .and_then(|config| config.host(host_id))
        .and_then(|host| host.expected_source())
        .cloned();
    let store = app.remote_bandwidth_store.as_ref().unwrap();
    let reservation = match store
        .begin_attempt(
            host_id,
            source.as_ref().map(|source| &source.node_id),
            recorded_at,
            RemoteBandwidthTransferKind::ManualOverride,
            bytes,
        )
        .unwrap()
    {
        crate::remote_bandwidth_budget::RemoteBandwidthAdmission::Granted(reservation) => {
            reservation
        }
        crate::remote_bandwidth_budget::RemoteBandwidthAdmission::Paused(pause) => {
            panic!("manual override unexpectedly paused: {pause:?}")
        }
    };
    store
        .complete_attempt(&reservation, recorded_at, bytes)
        .unwrap();
}

fn trend_history_fixture(now: DateTime<Utc>) -> HistoryData {
    let five_hour_reset = now + ChronoDuration::hours(4);
    let weekly_reset = now + ChronoDuration::days(3);
    let day_bounds = trend_day_bounds(now, 0);
    let bucket_starts = [
        day_bounds[1] - ChronoDuration::hours(2),
        day_bounds[1] - ChronoDuration::minutes(90),
        day_bounds[1] - ChronoDuration::minutes(15),
    ];
    HistoryData {
        quota_points: vec![
            QuotaPoint {
                observed_at: now - ChronoDuration::hours(1),
                limit_id: "codex".to_string(),
                duration_mins: 300,
                resets_at: five_hour_reset,
                used_percent: 20.0,
                remaining_percent: 80.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: now,
                limit_id: "codex".to_string(),
                duration_mins: 300,
                resets_at: five_hour_reset,
                used_percent: 40.0,
                remaining_percent: 60.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: now - ChronoDuration::days(1),
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: weekly_reset,
                used_percent: 10.0,
                remaining_percent: 90.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: now,
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: weekly_reset,
                used_percent: 25.0,
                remaining_percent: 75.0,
                provenance: Provenance::ServerSnapshot,
            },
        ],
        half_hour_buckets: bucket_starts
            .into_iter()
            .enumerate()
            .map(|(index, starts_at)| LocalHalfHourBucket {
                starts_at,
                ends_at: starts_at + ChronoDuration::minutes(15),
                sampled_at: now.min(starts_at + ChronoDuration::minutes(15)),
                token_usage: TokenUsage {
                    total_tokens: u64::try_from(index + 1).unwrap() * 1_000,
                    ..TokenUsage::default()
                },
                estimated_cost_units: u128::try_from(index + 1).unwrap() * 100,
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
                project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
                api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
                call_count: 1,
                groups: Vec::new(),
                project_groups: Vec::new(),
                partial_reasons: if index == 1 {
                    vec!["fixture_partial".to_string()]
                } else {
                    Vec::new()
                },
            })
            .collect(),
        weekly_local_points: Vec::new(),
        warnings: Vec::new(),
        read_only: false,
        summary_backfill_attempted_at: None,
        summary_backfill_attempt_complete: None,
    }
}

fn make_task_resumable(app: &mut App, index: usize, cwd: &std::path::Path) {
    let old_thread_id = app.snapshot.tasks[index].thread_id.clone();
    let thread_id = if index == 0 {
        RESUMABLE_THREAD_ID.to_string()
    } else {
        format!("019f52ac-7a9f-7fd1-8dda-e775ef95078{index}")
    };
    let task = &mut app.snapshot.tasks[index];
    task.thread_id = thread_id.clone();
    task.parent_thread_id = None;
    task.archived = false;
    task.cwd = Some(cwd.to_path_buf());
    task.source = Some("desktop".to_string());
    task.status = TaskStatus::Completed;
    for turn in &mut app.snapshot.turns {
        if turn.thread_id == old_thread_id {
            turn.thread_id = thread_id.clone();
        }
    }
    app.zellij_environment = true;
    app.open_config = OpenConfig::default();
    app.open_config_error = None;
    app.snapshot.codex_home = cwd.to_path_buf();
}

fn assert_resume_copy_command(command: &str) {
    #[cfg(not(windows))]
    {
        assert!(command.starts_with("CODEX_HOME="));
        assert!(command.contains(" codex resume --cd "));
    }
    #[cfg(windows)]
    {
        assert!(command.starts_with("& { param($codexHome) "));
        assert!(command.contains("$env:CODEX_HOME = $codexHome"));
        assert!(command.contains("& 'codex' 'resume' '--cd' "));
    }
    assert!(command.contains(RESUMABLE_THREAD_ID));
    assert!(!command.contains("PATH="));
    assert!(!command.contains("visible"));
}

#[test]
fn open_control_supports_keyboard_mouse_search_priority_and_compact_rendering() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(2, 2);
    make_task_resumable(&mut app, 0, temp.path());
    app.snapshot.tasks[0].title = "visible\u{202e}hidden".to_string();
    app.turns_default_visible = false;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let controls = app.task_controls_hitbox.unwrap();
    assert_eq!(controls.open_terminal.width, 7);
    assert!(controls.enter_turns.right() <= controls.open_terminal.x);
    assert!(controls.open_terminal.right() <= controls.toggle_tree.x);
    let shortcut =
        &terminal.backend().buffer()[(controls.open_terminal.x + 1, controls.open_terminal.y)];
    assert_eq!(shortcut.symbol(), "O");
    assert_eq!(shortcut.fg, app.theme.palette().accent);
    assert!(shortcut.modifier.contains(Modifier::BOLD));

    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    assert_eq!(
        app.resume_confirmation
            .as_ref()
            .map(|modal| modal.thread_id.as_str()),
        Some(RESUMABLE_THREAD_ID)
    );
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("Resume in new Codex terminal?"));
    assert!(content.contains("Open creates a new CLI frontend"));
    assert!(!content.contains('\u{202e}'));
    let copy = app.resume_confirmation_hitbox.unwrap().copy;
    assert!(!copy.is_empty());
    assert_eq!(
        terminal.backend().buffer()[(copy.x + 1, copy.y)].symbol(),
        "C"
    );
    assert_eq!(
        terminal.backend().buffer()[(copy.x + 1, copy.y)].fg,
        app.theme.palette().accent
    );
    handle_key_event(&mut app, key_event(KeyCode::Char('C')));
    let request = app.pending_clipboard.take().unwrap();
    assert_resume_copy_command(&request.text);
    app.apply_clipboard_result(
        request,
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "test failure")),
    );
    assert!(app.resume_confirmation.is_some());
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let failed_content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(failed_content.contains("Copy failed"));

    let selected = app.selected_task;
    let background_row = app.task_table_hitbox.unwrap().rows.y.saturating_add(1);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), 2, background_row),
    ));
    assert_eq!(app.selected_task, selected);
    let cancel = app.resume_confirmation_hitbox.unwrap().cancel;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            cancel.right() - 1,
            cancel.y,
        ),
    ));
    assert!(app.resume_confirmation.is_none());

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let open = app.task_controls_hitbox.unwrap().open_terminal;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            open.right() - 1,
            open.y,
        ),
    ));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let confirm = app.resume_confirmation_hitbox.unwrap().confirm;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            confirm.right() - 1,
            confirm.y,
        ),
    ));
    assert!(app.resume_confirmation.is_none());
    assert!(app.pending_resume.is_some());
    app.pending_resume = None;
    app.launching_threads.clear();

    app.begin_task_search();
    handle_key_event(&mut app, key_event(KeyCode::Char('o')));
    assert_eq!(app.task_search, "o");
    assert!(app.resume_confirmation.is_none());
    app.cancel_task_search();
    app.focus_turns();
    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    assert!(app.resume_confirmation.is_none());
    app.focus_tasks();

    app.snapshot.tasks[0].status = TaskStatus::Running;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let disabled = app.task_controls_hitbox.unwrap().open_terminal;
    assert_eq!(disabled, open);
    assert_eq!(
        terminal.backend().buffer()[(disabled.x + 1, disabled.y)].fg,
        app.theme.palette().muted
    );

    app.snapshot.tasks[0].status = TaskStatus::Completed;
    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        let mut compact_terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        compact_terminal
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let compact = app.task_controls_hitbox.unwrap().open_terminal;
        assert_eq!(compact.width, 3);
        assert_eq!(
            compact_terminal.backend().buffer()[(compact.x + 1, compact.y)].symbol(),
            "O"
        );
    }
}

#[test]
fn open_outside_zellij_copies_with_mouse_and_never_launches_on_enter() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(1, 0);
    make_task_resumable(&mut app, 0, temp.path());
    app.zellij_environment = false;
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();

    app.activate_open();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let hitbox = app.resume_confirmation_hitbox.unwrap();
    assert!(hitbox.confirm.is_empty());
    assert!(!hitbox.copy.is_empty());
    assert!(!hitbox.cancel.is_empty());
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("clipboard · run in another terminal"));
    assert!(content.contains("[C] Copy"));
    assert!(!content.contains("[↵] Open"));

    handle_key_event(&mut app, key_event(KeyCode::Enter));
    assert!(app.resume_confirmation.is_some());
    assert!(app.pending_resume.is_none());
    assert!(app.pending_clipboard.is_none());

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.copy.right() - 1,
            hitbox.copy.y,
        ),
    ));
    let request = app.pending_clipboard.take().unwrap();
    assert_eq!(request.thread_id, RESUMABLE_THREAD_ID);
    assert_resume_copy_command(&request.text);
    app.apply_clipboard_result(request, Ok(()));
    assert!(app.resume_confirmation.is_none());
    assert_eq!(
        app.open_notice.as_ref().unwrap().tone,
        OpenNoticeTone::Success
    );
    assert!(app.pending_resume.is_none());
}

#[test]
fn osc52_clipboard_writer_frames_base64_and_rejects_oversized_text() {
    let command = "codex resume --cd '/tmp/a b' 019f";
    let mut output = Vec::new();
    write_osc52_clipboard(&mut output, command).unwrap();
    assert!(output.starts_with(b"\x1b]52;c;"));
    assert_eq!(output.last(), Some(&b'\x07'));
    let encoded = &output[b"\x1b]52;c;".len()..output.len() - 1];
    assert_eq!(BASE64_STANDARD.decode(encoded).unwrap(), command.as_bytes());

    let oversized = "x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);
    let error = write_osc52_clipboard(&mut Vec::new(), &oversized).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    struct FailingWriter;
    impl io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        write_osc52_clipboard(&mut FailingWriter, command)
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[test]
fn resume_confirmation_revalidates_after_refresh_and_reuses_known_panes() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(2, 1);
    make_task_resumable(&mut app, 0, temp.path());
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    assert!(app.resume_confirmation.is_some());
    let mut snapshot = app.snapshot.clone();
    snapshot
        .tasks
        .iter_mut()
        .find(|task| task.thread_id == RESUMABLE_THREAD_ID)
        .unwrap()
        .status = TaskStatus::WaitingInput;
    snapshot.tasks.reverse();
    app.replace(
        CollectionResult {
            snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    assert!(app.pending_resume.is_none());
    assert!(app.launching_threads.is_empty());
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("Live attach is unavailable")
    );

    app.snapshot
        .tasks
        .iter_mut()
        .find(|task| task.thread_id == RESUMABLE_THREAD_ID)
        .unwrap()
        .status = TaskStatus::Completed;
    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    let first = app.pending_resume.take().unwrap();
    let ResumeLaunchRequest::Create { target, .. } = first else {
        panic!("first Open should create a pane");
    };
    assert_eq!(target.thread_id, RESUMABLE_THREAD_ID);
    assert!(app.launching_threads.contains(RESUMABLE_THREAD_ID));

    let pane_id = PaneId::parse("terminal_42").unwrap();
    app.apply_resume_completion(ResumeLaunchCompletion {
        thread_id: RESUMABLE_THREAD_ID.to_string(),
        result: Ok(ResumeLaunchOutcome::Created(pane_id.clone())),
    });
    assert_eq!(app.open_panes.get(RESUMABLE_THREAD_ID), Some(&pane_id));
    assert!(!app.launching_threads.contains(RESUMABLE_THREAD_ID));
    assert_eq!(
        app.open_notice.as_ref().unwrap().tone,
        OpenNoticeTone::Success
    );

    app.snapshot
        .tasks
        .iter_mut()
        .find(|task| task.thread_id == RESUMABLE_THREAD_ID)
        .unwrap()
        .status = TaskStatus::Running;
    assert!(app.open_control_available());
    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    let repeat = app.pending_resume.take().unwrap();
    let ResumeLaunchRequest::Focus {
        thread_id,
        pane_id: repeat_pane,
        ..
    } = repeat
    else {
        panic!("known pane should be focused even after the task becomes active");
    };
    assert_eq!(thread_id, RESUMABLE_THREAD_ID);
    assert_eq!(repeat_pane, pane_id);

    app.apply_resume_completion(ResumeLaunchCompletion {
        thread_id: RESUMABLE_THREAD_ID.to_string(),
        result: Ok(ResumeLaunchOutcome::Missing(pane_id.clone())),
    });
    assert!(!app.open_panes.contains_key(RESUMABLE_THREAD_ID));
    assert!(app.resume_confirmation.is_none());
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("press O again")
    );
    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("Live attach is unavailable")
    );

    app.snapshot
        .tasks
        .iter_mut()
        .find(|task| task.thread_id == RESUMABLE_THREAD_ID)
        .unwrap()
        .status = TaskStatus::Completed;
    app.open_panes
        .insert(RESUMABLE_THREAD_ID.to_string(), pane_id.clone());
    app.launching_threads.clear();
    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    let _ = app.pending_resume.take().unwrap();
    app.apply_resume_completion(ResumeLaunchCompletion {
        thread_id: RESUMABLE_THREAD_ID.to_string(),
        result: Ok(ResumeLaunchOutcome::Missing(pane_id)),
    });
    assert!(app.resume_confirmation.is_none());
    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    assert!(app.resume_confirmation.is_some());
}

#[test]
fn open_rejections_explain_config_runtime_and_task_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(1, 0);
    app.zellij_environment = true;
    handle_key_event(&mut app, key_event(KeyCode::Char('O')));
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("resumable thread id")
    );

    make_task_resumable(&mut app, 0, temp.path());
    app.zellij_environment = false;
    assert!(app.open_control_available());
    app.activate_open();
    assert!(app.resume_confirmation.is_some());
    app.close_resume_confirmation();

    app.zellij_environment = true;
    app.open_config.enabled = false;
    app.activate_open();
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("disabled")
    );

    app.open_config = OpenConfig::default();
    app.snapshot.tasks[0].source = Some("subagent".to_string());
    app.activate_open();
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("parent task")
    );

    app.snapshot.tasks[0].source = Some("desktop".to_string());
    app.snapshot.tasks[0].archived = true;
    app.activate_open();
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("Unarchive")
    );

    app.snapshot.tasks[0].archived = false;
    app.snapshot.tasks[0].cwd = None;
    app.activate_open();
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("working directory")
    );

    app.snapshot.tasks[0].cwd = Some(temp.path().join("removed"));
    app.activate_open();
    assert!(
        app.open_notice
            .as_ref()
            .unwrap()
            .message
            .contains("no longer exists")
    );
    assert!(app.resume_confirmation.is_none());
}

#[test]
fn resume_modal_disables_open_but_keeps_cancel_available_when_tiny() {
    let temp = tempfile::tempdir().unwrap();
    for (width, height) in [(8, 1), (12, 5), (24, 8), (60, 12)] {
        let mut app = interaction_test_app(1, 0);
        make_task_resumable(&mut app, 0, temp.path());
        app.activate_open();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hitbox = app.resume_confirmation_hitbox.unwrap();
        let can_confirm = !hitbox.confirm.is_empty();
        assert_eq!(can_confirm, width >= 46 && height >= 12);
        assert_eq!(!hitbox.copy.is_empty(), can_confirm);
        if can_confirm {
            assert!(!hitbox.cancel.is_empty());
        }
        for button in [hitbox.confirm, hitbox.copy, hitbox.cancel] {
            if !button.is_empty() {
                assert!(button.right() <= width);
                assert!(button.bottom() <= height);
            }
        }
        if !can_confirm {
            handle_key_event(&mut app, key_event(KeyCode::Enter));
            assert!(app.resume_confirmation.is_some());
            assert!(app.pending_resume.is_none());
            if (width, height) == (24, 8) {
                let content = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(content.contains("Resize terminal"));
                assert!(!content.contains("Cwd:"));
            }
            if hitbox.cancel.is_empty() {
                handle_key_event(&mut app, key_event(KeyCode::Esc));
            } else {
                assert!(handle_mouse_event(
                    &mut app,
                    mouse_event(
                        MouseEventKind::Down(MouseButton::Left),
                        hitbox.cancel.right() - 1,
                        hitbox.cancel.y,
                    ),
                ));
            }
        } else {
            handle_key_event(&mut app, key_event(KeyCode::Esc));
        }
        assert!(app.resume_confirmation.is_none());
    }
}

#[test]
fn resume_modal_preserves_the_checkout_tail_of_a_long_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp
        .path()
        .join("managed-worktrees-with-a-very-long-parent-directory-name".repeat(2))
        .join("checkout-alpha");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut app = interaction_test_app(1, 0);
    make_task_resumable(&mut app, 0, &cwd);
    app.activate_open();
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("checkout-alpha"));
    assert!(!app.resume_confirmation_hitbox.unwrap().confirm.is_empty());
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn key_event(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn set_task_metadata(app: &mut App, index: usize, title: &str, source: Option<&str>) {
    let task = &mut app.snapshot.tasks[index];
    task.title = title.to_string();
    task.source = source.map(str::to_string);
}

fn set_task_parent(app: &mut App, child: usize, parent: usize) {
    let parent_thread_id = app.snapshot.tasks[parent].thread_id.clone();
    let task = &mut app.snapshot.tasks[child];
    task.source = Some("subagent".to_string());
    task.parent_thread_id = Some(parent_thread_id);
}

fn set_remote_task_identity(
    app: &mut App,
    index: usize,
    node: &str,
    remote_thread_id: &str,
    parent_thread_id: Option<&str>,
) {
    let task = &mut app.snapshot.tasks[index];
    task.thread_id = format!("remote:{node}:{remote_thread_id}");
    task.parent_thread_id = parent_thread_id.map(|parent| format!("remote:{node}:{parent}"));
    task.source = Some("remote:dev-server".to_string());
}

fn expand_task_tree(app: &mut App) {
    app.task_list_mode = TaskListMode::Tree;
    let collapsible = app.filtered_collapsible_task_threads();
    app.expanded_task_threads.extend(collapsible);
}

fn render_models_content(snapshot: &Snapshot, width: u16, height: u16) -> String {
    render_models_content_for_scope(snapshot, WindowScope::FiveHours, width, height)
}

fn render_models_content_for_scope(
    snapshot: &Snapshot,
    scope: WindowScope,
    width: u16,
    height: u16,
) -> String {
    let mut app = App::new(
        CollectionResult {
            snapshot: snapshot.clone(),
            account: AccountSnapshot::default(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        Theme::Dark,
    );
    app.window_scope = scope;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| render_models(frame, frame.area(), &mut app))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn model_usage(model: &str, total_tokens: u64) -> ModelUsage {
    ModelUsage {
        model: model.to_string(),
        token_usage: TokenUsage {
            total_tokens,
            ..TokenUsage::default()
        },
        local_token_share_percent: 0.0,
        estimated_quota_percent: 0.0,
        quota_confidence: Confidence::Unknown,
        api_equivalent_cost: Default::default(),
    }
}

fn exact_api_cost(pico_usd: u128) -> ApiCostAmount {
    ApiCostAmount {
        minimum_pico_usd: PicoUsd::new(pico_usd),
        maximum_pico_usd: PicoUsd::new(pico_usd),
        observed_samples: 1,
        priced_samples: 1,
        observed_tokens: 100,
        priced_tokens: 100,
    }
}

fn ranged_api_cost(minimum_dollars: u128, maximum_dollars: u128) -> ApiCostAmount {
    const PICO_USD_PER_USD: u128 = 1_000_000_000_000;
    ApiCostAmount {
        minimum_pico_usd: PicoUsd::new(minimum_dollars * PICO_USD_PER_USD),
        maximum_pico_usd: PicoUsd::new(maximum_dollars * PICO_USD_PER_USD),
        observed_samples: 2,
        priced_samples: 1,
        observed_tokens: 200,
        priced_tokens: 100,
    }
}

fn add_window_analysis(
    app: &mut App,
    scope: WindowScope,
    total_tokens: u64,
    local_share_percent: f64,
) {
    let now = app.snapshot.as_of;
    let estimated_quota_percent = 23.0 * local_share_percent / 100.0;
    let usage = WindowUsage {
        token_usage: TokenUsage {
            input_tokens: total_tokens,
            total_tokens,
            ..TokenUsage::default()
        },
        local_token_share_percent: local_share_percent,
        estimated_quota_percent,
        quota_confidence: Confidence::Low,
        api_equivalent_cost: Default::default(),
    };
    let thread_id = app.snapshot.tasks[0].thread_id.clone();
    let turn_id = app
        .snapshot
        .turns
        .iter()
        .find(|turn| turn.thread_id == thread_id)
        .map(|turn| turn.turn_id.clone())
        .unwrap_or_else(|| "turn-0-0".to_string());
    app.snapshot
        .window_analyses
        .retain(|analysis| analysis.duration_mins != scope.duration_mins());
    app.snapshot.window_analyses.push(WindowAnalysis {
        duration_mins: scope.duration_mins(),
        attribution: AttributionSummary {
            window: Some(WindowDescriptor {
                limit_id: "codex".to_string(),
                label: scope.label().to_ascii_lowercase(),
                starts_at: now - chrono::Duration::minutes(scope.duration_mins() - 60),
                ends_at: now + chrono::Duration::hours(1),
                used_percent: 23.0,
            }),
            local_token_usage: usage.token_usage,
            observed_delta_percent: 0.0,
            estimated_assigned_percent: 0.0,
            proxy_projected_percent: 23.0,
            unattributed_percent: 23.0,
            attribution_coverage_percent: 0.0,
            external_activity_possible: true,
            confidence: Confidence::Low,
            method: "current_codex_gauge_credit_rate_weighted_proxy".to_string(),
            settled: true,
        },
        partial: false,
        partial_reasons: Vec::new(),
        threads: vec![ThreadWindowUsage {
            thread_id: thread_id.clone(),
            usage,
        }],
        turns: vec![TurnWindowUsage {
            thread_id,
            turn_id,
            usage,
        }],
        models: vec![ModelUsage {
            model: "gpt-window".to_string(),
            token_usage: usage.token_usage,
            local_token_share_percent: local_share_percent,
            estimated_quota_percent: usage.estimated_quota_percent,
            quota_confidence: usage.quota_confidence,
            api_equivalent_cost: Default::default(),
        }],
        api_equivalent_cost: Default::default(),
        api_pricing: Default::default(),
        api_long_context: None,
    });
}

fn set_window_entity_api_costs(
    app: &mut App,
    scope: WindowScope,
    thread_pico_usd: u128,
    turn_pico_usd: &[u128],
) {
    let thread_id = app.snapshot.tasks[0].thread_id.clone();
    let turn_ids = app
        .snapshot
        .turns
        .iter()
        .filter(|turn| turn.thread_id == thread_id)
        .map(|turn| turn.turn_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(turn_ids.len(), turn_pico_usd.len());
    let analysis = app
        .snapshot
        .window_analyses
        .iter_mut()
        .find(|analysis| analysis.duration_mins == scope.duration_mins())
        .unwrap();
    let template = analysis.turns[0].usage;
    analysis.threads[0].usage.api_equivalent_cost = exact_api_cost(thread_pico_usd);
    analysis.turns = turn_ids
        .into_iter()
        .zip(turn_pico_usd.iter().copied())
        .map(|(turn_id, amount)| TurnWindowUsage {
            thread_id: thread_id.clone(),
            turn_id,
            usage: WindowUsage {
                api_equivalent_cost: exact_api_cost(amount),
                ..template
            },
        })
        .collect();
}

fn set_window_reset(app: &mut App, scope: WindowScope, reset_at: chrono::DateTime<chrono::Utc>) {
    add_window_analysis(app, scope, 100, 100.0);
    let window = app
        .snapshot
        .window_analyses
        .iter_mut()
        .find(|analysis| analysis.duration_mins == scope.duration_mins())
        .and_then(|analysis| analysis.attribution.window.as_mut())
        .expect("window analysis");
    window.ends_at = reset_at;
    window.starts_at = reset_at - chrono::Duration::minutes(scope.duration_mins());
}

fn reset_credit(
    now: chrono::DateTime<chrono::Utc>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> RateLimitResetCredit {
    RateLimitResetCredit {
        granted_at: now - chrono::Duration::hours(1),
        expires_at,
        status: "available".to_string(),
        reset_type: "codexRateLimits".to_string(),
        title: Some("Reset Codex limits".to_string()),
        description: None,
    }
}

fn set_reset_credits(app: &mut App, available_count: u64, credits: Vec<RateLimitResetCredit>) {
    app.snapshot.rate_limit_reset_credits = Some(RateLimitResetCreditsSnapshot {
        available_count,
        credits: Some(credits),
        provenance: Provenance::ServerSnapshot,
        as_of: app.snapshot.as_of,
    });
    app.snapshot.rate_limit_reset_credits_partial = false;
}

fn add_spark_window_analysis(app: &mut App, scope: WindowScope, total_tokens: u64) {
    let mut analysis = app
        .snapshot
        .window_analyses
        .iter()
        .find(|analysis| analysis.duration_mins == scope.duration_mins())
        .cloned()
        .expect("base window analysis");
    let usage = WindowUsage {
        token_usage: TokenUsage {
            input_tokens: total_tokens,
            total_tokens,
            ..TokenUsage::default()
        },
        local_token_share_percent: 100.0,
        estimated_quota_percent: 1.25,
        quota_confidence: Confidence::Low,
        api_equivalent_cost: Default::default(),
    };
    let window = analysis.attribution.window.as_mut().unwrap();
    window.limit_id = "codex_bengalfox".to_string();
    window.used_percent = 7.0;
    window.ends_at += chrono::Duration::minutes(30);
    window.starts_at += chrono::Duration::minutes(30);
    analysis.attribution.local_token_usage = usage.token_usage;
    analysis.attribution.observed_delta_percent = 0.5;
    analysis.attribution.estimated_assigned_percent = 0.4;
    analysis.attribution.unattributed_percent = 6.6;
    analysis.attribution.attribution_coverage_percent = 5.7;
    analysis.attribution.confidence = Confidence::Low;
    analysis.threads[0].usage = usage;
    analysis.turns[0].usage = usage;
    analysis.models = vec![ModelUsage {
        model: "gpt-5.3-codex-spark".to_string(),
        token_usage: usage.token_usage,
        local_token_share_percent: 100.0,
        estimated_quota_percent: usage.estimated_quota_percent,
        quota_confidence: usage.quota_confidence,
        api_equivalent_cost: Default::default(),
    }];
    app.snapshot.window_analyses.push(analysis);
}

#[test]
fn quota_thresholds_are_distinct() {
    for theme in [Theme::Dark, Theme::Light] {
        let palette = theme.palette();
        assert_eq!(quota_color(50.0, theme), palette.success);
        assert_eq!(quota_color(75.0, theme), palette.warning);
        assert_eq!(quota_color(95.0, theme), palette.error);
    }
}

#[test]
fn reset_expiry_reminder_uses_strict_weekly_boundary() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    let weekly_reset = now + chrono::Duration::days(4);
    set_window_reset(&mut app, WindowScope::Week, weekly_reset);

    for (expires_at, expected) in [
        (Some(now - chrono::Duration::seconds(1)), false),
        (Some(now), false),
        (Some(now + chrono::Duration::seconds(1)), true),
        (Some(weekly_reset - chrono::Duration::seconds(1)), true),
        (Some(weekly_reset), false),
        (Some(weekly_reset + chrono::Duration::seconds(1)), false),
        (None, false),
    ] {
        set_reset_credits(&mut app, 1, vec![reset_credit(now, expires_at)]);
        let reminder = reset_expiry_reminder(&app.snapshot);
        assert_eq!(reminder.is_some(), expected, "expires_at={expires_at:?}");
        if expected {
            assert_eq!(
                reminder,
                Some(ResetExpiryReminder {
                    expires_at: expires_at.unwrap(),
                    weekly_reset_at: weekly_reset,
                })
            );
        }
    }
}

#[test]
fn reset_expiry_gauge_alert_preserves_the_exact_expiry_at_narrow_widths() {
    let now = chrono::Utc::now();
    let reminder = ResetExpiryReminder {
        expires_at: now + chrono::Duration::days(2) + chrono::Duration::seconds(47),
        weekly_reset_at: now + chrono::Duration::days(4) + chrono::Duration::seconds(29),
    };
    let local_expiry = reminder.expires_at.with_timezone(&Local);
    let expected_date = local_expiry.format("%Y-%m-%d").to_string();
    let expected_time = local_expiry.format("%H:%M:%S %:z").to_string();

    for width in [58, 38, 28, 18, 12, 1] {
        let lines = reset_expiry_gauge_alert_lines(reminder, width);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= usize::from(width.max(1)))
        );
        let joined = lines.join(" ");
        assert!(
            joined.contains(&expected_date) || lines.concat().contains(&expected_date),
            "width={width}: {joined}"
        );
        assert!(
            joined.contains(&expected_time) || lines.concat().contains(&expected_time),
            "width={width}: {joined}"
        );
    }
}

#[test]
fn reset_expiry_reminder_selects_the_earliest_reliable_available_credit() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    let weekly_reset = now + chrono::Duration::days(5);
    let earliest = now + chrono::Duration::days(2);
    set_window_reset(&mut app, WindowScope::Week, weekly_reset);

    let mut wrong_status = reset_credit(now, Some(now + chrono::Duration::hours(1)));
    wrong_status.status = "redeeming".to_string();
    let mut wrong_type = reset_credit(now, Some(now + chrono::Duration::hours(2)));
    wrong_type.reset_type = "futureResetType".to_string();
    let credits = vec![
        reset_credit(now, Some(now - chrono::Duration::minutes(1))),
        reset_credit(now, None),
        reset_credit(now, Some(weekly_reset + chrono::Duration::days(1))),
        reset_credit(now, Some(now + chrono::Duration::days(3))),
        reset_credit(now, Some(earliest)),
        wrong_status,
        wrong_type,
    ];
    set_reset_credits(&mut app, credits.len() as u64, credits);

    assert_eq!(
        reset_expiry_reminder(&app.snapshot),
        Some(ResetExpiryReminder {
            expires_at: earliest,
            weekly_reset_at: weekly_reset,
        })
    );

    app.snapshot
        .rate_limit_reset_credits
        .as_mut()
        .unwrap()
        .available_count += 1;
    assert_eq!(reset_expiry_reminder(&app.snapshot), None);
    app.snapshot
        .rate_limit_reset_credits
        .as_mut()
        .unwrap()
        .available_count -= 1;

    app.snapshot.rate_limit_reset_credits_partial = true;
    assert_eq!(reset_expiry_reminder(&app.snapshot), None);
    app.snapshot.rate_limit_reset_credits_partial = false;

    app.snapshot
        .rate_limit_reset_credits
        .as_mut()
        .unwrap()
        .provenance = Provenance::Stale;
    assert_eq!(reset_expiry_reminder(&app.snapshot), None);
}

#[test]
fn reset_expiry_reminder_requires_a_current_codex_week_window() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    let expires_at = now + chrono::Duration::days(1);
    set_reset_credits(&mut app, 1, vec![reset_credit(now, Some(expires_at))]);

    assert_eq!(reset_expiry_reminder(&app.snapshot), None);

    set_window_reset(
        &mut app,
        WindowScope::FiveHours,
        now + chrono::Duration::hours(2),
    );
    assert_eq!(reset_expiry_reminder(&app.snapshot), None);

    set_window_reset(&mut app, WindowScope::Week, now + chrono::Duration::days(4));
    let weekly_index = app
        .snapshot
        .window_analyses
        .iter()
        .position(|analysis| analysis.duration_mins == WindowScope::Week.duration_mins())
        .unwrap();
    app.snapshot.window_analyses[weekly_index]
        .attribution
        .window
        .as_mut()
        .unwrap()
        .limit_id = "codex_bengalfox".to_string();
    assert_eq!(reset_expiry_reminder(&app.snapshot), None);

    app.snapshot.window_analyses[weekly_index]
        .attribution
        .window
        .as_mut()
        .unwrap()
        .limit_id = "codex".to_string();
    app.snapshot.window_analyses[weekly_index]
        .attribution
        .window
        .as_mut()
        .unwrap()
        .ends_at = now;
    assert_eq!(reset_expiry_reminder(&app.snapshot), None);

    app.snapshot.window_analyses[weekly_index]
        .attribution
        .window
        .as_mut()
        .unwrap()
        .ends_at = now + chrono::Duration::days(4);
    app.snapshot.window_analyses[weekly_index]
        .partial_reasons
        .push("quota_window_stale".to_string());
    assert_eq!(reset_expiry_reminder(&app.snapshot), None);
}

#[test]
fn overview_orders_codex_quota_windows_before_other_buckets() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    app.snapshot.limits = vec![
        LimitBucket {
            limit_id: "base_model_inference".to_string(),
            limit_name: Some("GPT reserve".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(
                0.0,
                Some(10_080),
                Some(now + chrono::Duration::days(7)),
            )),
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
        LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: Some("Codex".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(
                12.0,
                Some(300),
                Some(now + chrono::Duration::hours(5)),
            )),
            secondary: Some(LimitWindow::new(
                34.0,
                Some(10_080),
                Some(now + chrono::Duration::days(6)),
            )),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
        LimitBucket {
            limit_id: "codex_bengalfox".to_string(),
            limit_name: Some("Codex Spark".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(
                0.0,
                Some(300),
                Some(now + chrono::Duration::hours(4)),
            )),
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
    ];

    let ordered = ordered_quota_windows(&app.snapshot);
    assert_eq!(
        ordered
            .iter()
            .map(|(bucket, window)| (bucket.limit_id.clone(), window.label()))
            .collect::<Vec<_>>(),
        vec![
            ("codex".to_string(), "5h".to_string()),
            ("codex".to_string(), "week".to_string()),
            ("base_model_inference".to_string(), "week".to_string()),
            ("codex_bengalfox".to_string(), "5h".to_string()),
        ]
    );

    let width = 200;
    let height = 5;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| render_limits(frame, frame.area(), &app.snapshot, app.theme))
        .unwrap();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 4); 4])
        .split(Rect::new(0, 0, width, height));
    let buffer = terminal.backend().buffer();
    for (column, expected_title) in columns.iter().zip([
        "5h · codex · SERVER",
        "week · codex · SERVER",
        "week · base_model_inference · SERVER",
        "5h · codex_bengalfox · SERVER",
    ]) {
        let text = buffer_rect_text(buffer, *column);
        assert!(
            text.contains(expected_title),
            "missing {expected_title:?} in reordered quota column: {text}"
        );
    }
}

#[test]
fn weekly_gauge_renders_exact_reset_expiry_reminder() {
    for theme in [Theme::Dark, Theme::Light] {
        for width in [40, 60, 80, 120] {
            let mut app = interaction_test_app(1, 1);
            app.theme = theme;
            let now = app.snapshot.as_of;
            let five_hour_reset = now + chrono::Duration::hours(2) + chrono::Duration::seconds(13);
            let weekly_reset = now + chrono::Duration::days(4) + chrono::Duration::seconds(29);
            let expires_at = now + chrono::Duration::days(2) + chrono::Duration::seconds(47);
            set_window_reset(&mut app, WindowScope::FiveHours, five_hour_reset);
            set_window_reset(&mut app, WindowScope::Week, weekly_reset);
            set_reset_credits(&mut app, 1, vec![reset_credit(now, Some(expires_at))]);
            app.snapshot.limits = vec![LimitBucket {
                limit_id: "codex".to_string(),
                limit_name: Some("Codex".to_string()),
                plan_type: Some("test".to_string()),
                primary: Some(LimitWindow::new(25.0, Some(300), Some(five_hour_reset))),
                secondary: Some(LimitWindow::new(40.0, Some(10_080), Some(weekly_reset))),
                credits: None,
                rate_limit_reached_type: None,
                provenance: Provenance::ServerSnapshot,
                as_of: now,
            }];

            let quota_height = overview_quota_height(&app.snapshot, width, 3);
            let mut terminal = Terminal::new(TestBackend::new(width, quota_height)).unwrap();
            terminal
                .draw(|frame| render_limits(frame, frame.area(), &app.snapshot, theme))
                .unwrap();
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .split(Rect::new(0, 0, width, quota_height));
            let buffer = terminal.backend().buffer();
            let five_hour_text = buffer_rect_text(buffer, columns[0]);
            let weekly_text = buffer_rect_text(buffer, columns[1]);
            let local_expiry = expires_at.with_timezone(&Local);
            let expected_date = local_expiry.format("%Y-%m-%d").to_string();
            let expected_time = local_expiry.format("%H:%M:%S %:z").to_string();

            assert!(!five_hour_text.contains(&expected_date));
            assert!(!five_hour_text.contains(&expected_time));
            assert!(
                weekly_text.contains(&expected_date),
                "missing expiry date at {width}x{quota_height}/{theme:?}: {weekly_text}"
            );
            assert!(
                weekly_text.contains(&expected_time),
                "missing expiry time at {width}x{quota_height}/{theme:?}: {weekly_text}"
            );
            assert!(
                weekly_text.contains("40/60%")
                    || weekly_text.contains("40%/60%")
                    || weekly_text.contains("40% used"),
                "missing weekly usage label at {width}x{quota_height}/{theme:?}: {weekly_text}"
            );
            assert!(!rect_has_foreground(
                buffer,
                columns[0],
                theme.palette().warning
            ));
            assert!(
                rect_has_foreground(buffer, columns[1], theme.palette().warning),
                "missing warning style at {width}x{quota_height}/{theme:?}"
            );
            let weekly_inner = panel("", theme).inner(columns[1]);
            let usage_y = weekly_inner.y + weekly_inner.height / 2;
            let usage_row = buffer_rect_row_text(buffer, weekly_inner, usage_y);
            assert!(
                usage_row.contains("40/60%")
                    || usage_row.contains("40%/60%")
                    || usage_row.contains("40% used"),
                "usage must stay centered at {width}x{quota_height}/{theme:?}: {usage_row}"
            );
            let warning_rows = rect_foreground_rows(buffer, columns[1], theme.palette().warning);
            assert_eq!(
                warning_rows.first().copied(),
                Some(usage_y + 1),
                "reminder must start below the centered usage row at {width}x{quota_height}/{theme:?}"
            );
        }
    }

    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    let weekly_reset = now + chrono::Duration::days(4);
    set_window_reset(&mut app, WindowScope::Week, weekly_reset);
    set_reset_credits(&mut app, 1, vec![reset_credit(now, Some(weekly_reset))]);
    app.snapshot.limits = vec![LimitBucket {
        limit_id: "codex".to_string(),
        limit_name: Some("Codex".to_string()),
        plan_type: Some("test".to_string()),
        primary: None,
        secondary: Some(LimitWindow::new(40.0, Some(10_080), Some(weekly_reset))),
        credits: None,
        rate_limit_reached_type: None,
        provenance: Provenance::ServerSnapshot,
        as_of: now,
    }];
    let mut terminal = Terminal::new(TestBackend::new(60, 5)).unwrap();
    terminal
        .draw(|frame| render_limits(frame, frame.area(), &app.snapshot, app.theme))
        .unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!content.contains("EXP"));
}

#[test]
fn quota_labels_invert_inside_the_filled_gauge_including_reset_credit_warning() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut app = interaction_test_app(1, 1);
        app.theme = theme;
        let now = app.snapshot.as_of;
        let five_hour_reset = now + chrono::Duration::hours(2);
        let weekly_reset = now + chrono::Duration::days(4);
        let expires_at = now + chrono::Duration::days(2);
        set_window_reset(&mut app, WindowScope::FiveHours, five_hour_reset);
        set_window_reset(&mut app, WindowScope::Week, weekly_reset);
        set_reset_credits(&mut app, 1, vec![reset_credit(now, Some(expires_at))]);
        app.snapshot.limits = vec![LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: Some("Codex".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(38.0, Some(300), Some(five_hour_reset))),
            secondary: Some(LimitWindow::new(42.0, Some(10_080), Some(weekly_reset))),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        }];

        let width = 120;
        let height = overview_quota_height(&app.snapshot, width, 3);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_limits(frame, frame.area(), &app.snapshot, theme))
            .unwrap();
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(Rect::new(0, 0, width, height));
        let buffer = terminal.backend().buffer();
        let palette = theme.palette();

        for (column, used_percent) in [(columns[0], 38.0), (columns[1], 42.0)] {
            let inner = panel("", theme).inner(column);
            let usage_y = inner.y + inner.height / 2;
            let gauge_color = quota_color(used_percent, theme);
            let filled_end =
                inner.x + (f64::from(inner.width) * used_percent / 100.0).round() as u16;

            assert!(
                row_has_text_style(
                    buffer,
                    usage_y,
                    inner.x,
                    filled_end,
                    palette.gauge_track,
                    gauge_color
                ),
                "covered usage text must invert at {theme:?}/{used_percent}%"
            );
            assert!(
                row_has_text_style(
                    buffer,
                    usage_y,
                    filled_end,
                    inner.right(),
                    gauge_color,
                    palette.gauge_track
                ),
                "uncovered usage text must keep the gauge colors at {theme:?}/{used_percent}%"
            );
        }

        let weekly_inner = panel("", theme).inner(columns[1]);
        let weekly_usage_y = weekly_inner.y + weekly_inner.height / 2;
        let weekly_warning_y = weekly_usage_y + 1;
        let weekly_color = quota_color(42.0, theme);
        let weekly_filled_end =
            weekly_inner.x + (f64::from(weekly_inner.width) * 0.42).round() as u16;
        assert!(
            row_has_text_style(
                buffer,
                weekly_warning_y,
                weekly_inner.x,
                weekly_filled_end,
                palette.gauge_track,
                weekly_color
            ),
            "covered reset-credit warning must invert at {theme:?}"
        );
        assert!(
            row_has_text_style(
                buffer,
                weekly_warning_y,
                weekly_filled_end,
                weekly_inner.right(),
                palette.warning,
                palette.gauge_track
            ),
            "uncovered reset-credit warning must retain warning color at {theme:?}"
        );
    }
}

#[test]
fn reset_expiry_reminder_only_marks_the_matching_codex_weekly_gauge() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    let five_hour_reset = now + chrono::Duration::hours(2);
    let weekly_reset = now + chrono::Duration::days(4);
    let expires_at = now + chrono::Duration::days(2);
    set_window_reset(&mut app, WindowScope::FiveHours, five_hour_reset);
    set_window_reset(&mut app, WindowScope::Week, weekly_reset);
    set_reset_credits(&mut app, 1, vec![reset_credit(now, Some(expires_at))]);
    app.snapshot.limits = vec![
        LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: Some("Codex".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(25.0, Some(300), Some(five_hour_reset))),
            secondary: Some(LimitWindow::new(40.0, Some(10_080), Some(weekly_reset))),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
        LimitBucket {
            limit_id: "codex_bengalfox".to_string(),
            limit_name: Some("Codex Spark".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(10.0, Some(10_080), Some(weekly_reset))),
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
    ];

    let width = 120;
    let mut terminal = Terminal::new(TestBackend::new(width, 5)).unwrap();
    terminal
        .draw(|frame| render_limits(frame, frame.area(), &app.snapshot, app.theme))
        .unwrap();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(Rect::new(0, 0, width, 5));
    let buffer = terminal.backend().buffer();

    assert!(!rect_has_foreground(
        buffer,
        columns[0],
        app.theme.palette().warning
    ));
    assert!(rect_has_foreground(
        buffer,
        columns[1],
        app.theme.palette().warning
    ));
    assert!(!rect_has_foreground(
        buffer,
        columns[2],
        app.theme.palette().warning
    ));
}

#[test]
fn overview_keeps_reset_expiry_reminder_inside_the_quota_panel() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    let five_hour_reset = now + chrono::Duration::hours(2);
    let weekly_reset = now + chrono::Duration::days(4);
    let expires_at = now + chrono::Duration::days(2) + chrono::Duration::seconds(47);
    set_window_reset(&mut app, WindowScope::FiveHours, five_hour_reset);
    set_window_reset(&mut app, WindowScope::Week, weekly_reset);
    set_reset_credits(&mut app, 1, vec![reset_credit(now, Some(expires_at))]);
    app.snapshot.limits = vec![LimitBucket {
        limit_id: "codex".to_string(),
        limit_name: Some("Codex".to_string()),
        plan_type: Some("test".to_string()),
        primary: Some(LimitWindow::new(25.0, Some(300), Some(five_hour_reset))),
        secondary: Some(LimitWindow::new(40.0, Some(10_080), Some(weekly_reset))),
        credits: None,
        rate_limit_reached_type: None,
        provenance: Provenance::ServerSnapshot,
        as_of: now,
    }];

    let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.task_table_hitbox.is_some());
    assert_eq!(overview_quota_height(&app.snapshot, 40, 3), 7);

    app.snapshot
        .warnings
        .push("app-server refresh failed: access denied".to_string());
    app.snapshot.sources = vec![SourceStatus {
        source: "app_server".to_string(),
        status: "partial".to_string(),
        as_of: now,
        message: None,
    }];
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(app.task_table_hitbox.is_some());
    assert!(content.contains("codex app-server failed · install CLI"));
    assert!(content.contains("Models ·"));
}

#[test]
fn overview_places_app_server_failure_between_sessions_and_models_at_all_sizes() {
    for (theme, width, height, expected) in [
        (
            Theme::Dark,
            80,
            40,
            "Unable to call codex app-server · try installing Codex CLI",
        ),
        (
            Theme::Light,
            40,
            24,
            "codex app-server failed · install CLI",
        ),
        (Theme::Dark, 24, 24, "app-server failed · CLI"),
    ] {
        let mut app = interaction_test_app(1, 1);
        app.theme = theme;
        app.snapshot.sources = vec![SourceStatus {
            source: "app_server".to_string(),
            status: "error".to_string(),
            as_of: app.snapshot.as_of,
            message: Some("failed to spawn codex app-server".to_string()),
        }];

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let buffer = terminal.backend().buffer();
        let area = Rect::new(0, 0, width, height);
        let warning_row = (0..height)
            .find(|row| buffer_rect_row_text(buffer, area, *row).contains(expected))
            .expect("warning row must be visible");
        let models_row = (0..height)
            .find(|row| buffer_rect_row_text(buffer, area, *row).contains("Models ·"))
            .expect("models panel must be visible");

        assert!(
            content.contains(expected),
            "missing CLI hint at {width}x{height}/{theme:?}: {content}"
        );
        assert_eq!(
            warning_row + 1,
            models_row,
            "notice must sit immediately above Models at {width}x{height}/{theme:?}"
        );
    }
}

#[test]
fn overview_keeps_app_server_failure_stable_across_local_only_refresh_state() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    app.snapshot
        .warnings
        .push("app-server refresh failed: access denied".to_string());
    app.snapshot.sources = vec![SourceStatus {
        source: "app_server".to_string(),
        status: "stale".to_string(),
        as_of: now,
        message: Some("no cached account snapshot".to_string()),
    }];
    app.snapshot.limits = vec![LimitBucket {
        limit_id: "codex".to_string(),
        limit_name: Some("Codex".to_string()),
        plan_type: Some("test".to_string()),
        primary: Some(LimitWindow::new(
            25.0,
            Some(300),
            Some(now + ChronoDuration::hours(2)),
        )),
        secondary: None,
        credits: None,
        rate_limit_reached_type: None,
        provenance: Provenance::Stale,
        as_of: now,
    }];

    assert!(app_server_call_failed(&app.snapshot));
    app.snapshot.sources[0].status = "partial".to_string();
    app.snapshot.sources[0].message = None;
    assert!(
        app_server_call_failed(&app.snapshot),
        "the persisted refresh warning must survive a local-only source-state rewrite"
    );
    assert_eq!(overview_quota_height(&app.snapshot, 80, 3), 3);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(content.contains("Unable to call codex app-server · try installing Codex CLI"));
}

#[test]
fn overview_does_not_report_the_initial_account_loading_state_as_an_error() {
    let mut app = interaction_test_app(0, 0);
    app.snapshot.sources = vec![SourceStatus {
        source: "app_server".to_string(),
        status: "stale".to_string(),
        as_of: app.snapshot.as_of,
        message: Some("no cached account snapshot".to_string()),
    }];

    assert!(!app_server_call_failed(&app.snapshot));
}

fn buffer_rect_text(buffer: &ratatui::buffer::Buffer, area: Rect) -> String {
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn buffer_rect_row_text(buffer: &ratatui::buffer::Buffer, area: Rect, y: u16) -> String {
    (area.x..area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn rect_has_foreground(buffer: &ratatui::buffer::Buffer, area: Rect, foreground: Color) -> bool {
    (area.y..area.bottom()).any(|y| {
        (area.x..area.right()).any(|x| {
            let cell = &buffer[(x, y)];
            cell.fg == foreground && !cell.symbol().trim().is_empty()
        })
    })
}

fn rect_foreground_rows(
    buffer: &ratatui::buffer::Buffer,
    area: Rect,
    foreground: Color,
) -> Vec<u16> {
    (area.y..area.bottom())
        .filter(|y| {
            (area.x..area.right()).any(|x| {
                let cell = &buffer[(x, *y)];
                cell.fg == foreground && !cell.symbol().trim().is_empty()
            })
        })
        .collect()
}

fn row_has_text_style(
    buffer: &ratatui::buffer::Buffer,
    y: u16,
    start: u16,
    end: u16,
    foreground: Color,
    background: Color,
) -> bool {
    (start..end).any(|x| {
        let cell = &buffer[(x, y)];
        !cell.symbol().trim().is_empty() && cell.fg == foreground && cell.bg == background
    })
}

#[test]
fn status_tones_have_distinct_subtle_backgrounds() {
    assert_eq!(task_status_tone(TaskStatus::Running), StatusTone::Active);
    assert_eq!(turn_status_tone(TurnStatus::Completed), StatusTone::Done);
    for theme in [Theme::Dark, Theme::Light] {
        assert_ne!(
            status_tone_style(StatusTone::Active, theme).bg,
            status_tone_style(StatusTone::Done, theme).bg
        );
        assert_ne!(
            status_tone_style(StatusTone::Waiting, theme).bg,
            status_tone_style(StatusTone::Failed, theme).bg
        );
    }
    assert_eq!(status_marker(StatusTone::Active), "R");
    assert_eq!(status_marker(StatusTone::Stopped), "X");
    assert_eq!(status_marker(StatusTone::Stale), "?");
}

#[test]
fn light_theme_uses_an_explicit_bright_canvas_and_toggles() {
    let palette = Theme::Light.palette();
    assert_eq!(palette.background, Color::Rgb(247, 249, 252));
    assert_eq!(palette.foreground, Color::Rgb(23, 32, 42));
    assert_eq!(palette.border, Color::Rgb(125, 137, 152));
    assert_ne!(palette.background, Theme::Dark.palette().background);
    assert_eq!(Theme::Dark.toggle(), Theme::Light);
    assert_eq!(Theme::Light.toggle(), Theme::Dark);
}

#[test]
fn saved_ui_state_restores_stable_menu_and_column_preferences_with_theme_override_priority() {
    let mut app = interaction_test_app(3, 1);
    set_task_metadata(&mut app, 1, "subagent task", Some("subagent"));
    let saved = UiState {
        theme: UiTheme::Light,
        view: UiView::Health,
        window_scope: UiWindowScope::Week,
        turns_visible: false,
        models_visible: false,
        api_long_context_multiplier: true,
        summary_range: UiSummaryRange::ThirtyDays,
        summary_grain: UiSummaryGrain::Hour,
        summary_metric: UiSummaryMetric::ApiEquivalent,
        summary_show_all_projects: true,
        table_columns: UiTableColumns {
            tokens: false,
            token_share: true,
            estimated_quota: false,
            api_equivalent: true,
        },
        task_list_mode: UiTaskListMode::Tree,
        task_source_filter: UiTaskSourceFilter::Subagent,
        ..UiState::default()
    };

    app.apply_ui_state(&saved, None);
    assert_eq!(app.ui_state(), saved);
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.selected_task, 1);

    app.task_search = "temporary query".to_string();
    app.expanded_task_threads
        .insert("task-thread-1".to_string());
    app.summary_expanded_nodes
        .insert("project:temporary".to_string());
    app.summary_selected_id = Some("project:temporary".to_string());
    app.summary_offset = 7;
    app.summary_inspected_date = NaiveDate::from_ymd_opt(2026, 8, 29)
        .unwrap()
        .and_hms_opt(12, 0, 0);
    assert_eq!(app.ui_state(), saved);

    app.apply_ui_state(&saved, Some(Theme::Dark));
    assert_eq!(app.theme, Theme::Dark);
    assert_eq!(app.ui_state().theme, UiTheme::Dark);
    assert_eq!(app.ui_state().table_columns, saved.table_columns);
}

#[test]
fn summary_preferences_survive_store_round_trip_while_navigation_resets() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = UiStateStore::new(directory.path().join("tui-state.json"));
    let mut app = interaction_test_app(3, 1);
    app.set_view(View::Summary);
    handle_key_event(&mut app, key_event(KeyCode::Char('M')));
    handle_key_event(&mut app, key_event(KeyCode::Char('A')));
    for _ in 0..4 {
        handle_key_event(&mut app, key_event(KeyCode::Char('B')));
    }
    app.summary_show_all_projects = true;
    app.summary_expanded_nodes
        .insert("project:temporary".to_string());
    app.summary_selected_id = Some("project:temporary".to_string());
    app.summary_offset = 7;
    app.summary_inspected_date = NaiveDate::from_ymd_opt(2026, 8, 29)
        .unwrap()
        .and_hms_opt(12, 0, 0);

    assert!(store.save(&app.ui_state()).unwrap());
    let loaded = store.load();
    let mut restarted = interaction_test_app(3, 1);
    restarted.apply_ui_state(&loaded, None);

    assert_eq!(restarted.view, View::Summary);
    assert_eq!(restarted.summary_range, SummaryRange::ThirtyDays);
    assert_eq!(restarted.summary_metric, SummaryMetric::ApiEquivalent);
    assert_eq!(restarted.summary_grain, SummaryGrain::Hour);
    assert!(restarted.summary_show_all_projects);
    assert!(restarted.summary_expanded_nodes.is_empty());
    assert_eq!(restarted.summary_selected_id, None);
    assert_eq!(restarted.summary_offset, 0);
    assert_eq!(restarted.summary_inspected_date, None);
}

#[test]
fn summary_and_trends_share_an_exact_persisted_source_selection() {
    let directory = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut app = interaction_test_app(1, 1);
    install_remote_sources_fixture(&mut app, directory.path(), now);
    let local_id: NodeId = "node-22222222222222222222222222222222".parse().unwrap();
    let remote_id: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
    app.history_local_source_id = Some(local_id.clone());

    assert_eq!(
        app.history_source_choices(),
        vec![
            HistorySourceSelection::AllIncluded,
            HistorySourceSelection::Local(local_id.clone()),
            HistorySourceSelection::Remote(remote_id.clone()),
        ]
    );

    app.cycle_history_source();
    assert_eq!(
        app.history_source_selection,
        HistorySourceSelection::Local(local_id.clone())
    );
    let local_generation = app.history_source_generation;
    app.cycle_history_source();
    assert_eq!(
        app.history_source_selection,
        HistorySourceSelection::Remote(remote_id.clone())
    );
    let remote_generation = app.history_source_generation;
    assert!(remote_generation > local_generation);

    let stale = TuiHistoryProjection {
        history: HistoryData::default(),
        selection: HistorySourceSelection::Local(local_id),
        status: Some(HistorySourceSelectionStatus::Applied),
        query_error: None,
    };
    assert!(!app.apply_history_projection(local_generation, stale));
    assert!(app.history_source_loading);
    assert_eq!(
        app.history_source_selection,
        HistorySourceSelection::Remote(remote_id.clone())
    );

    let current = TuiHistoryProjection {
        history: HistoryData::default(),
        selection: HistorySourceSelection::Remote(remote_id.clone()),
        status: Some(HistorySourceSelectionStatus::Unavailable(
            HistorySourceUnavailableReason::NotFound,
        )),
        query_error: None,
    };
    assert!(app.apply_history_projection(remote_generation, current));
    assert!(!app.history_source_loading);
    assert_eq!(
        app.history_source_status,
        Some(HistorySourceSelectionStatus::Unavailable(
            HistorySourceUnavailableReason::NotFound
        ))
    );
    assert_eq!(
        app.ui_state().history_source_selection,
        UiHistorySourceSelection::Remote { node_id: remote_id }
    );
}

#[test]
fn missing_saved_source_remains_desired_and_never_falls_back_to_all() {
    let mut app = interaction_test_app(1, 1);
    let missing_id: NodeId = "node-33333333333333333333333333333333".parse().unwrap();
    app.apply_ui_state(
        &UiState {
            history_source_selection: UiHistorySourceSelection::Remote {
                node_id: missing_id.clone(),
            },
            ..UiState::default()
        },
        None,
    );
    assert_eq!(
        app.history_source_selection,
        HistorySourceSelection::Remote(missing_id.clone())
    );
    assert_eq!(
        app.history_source_choices(),
        vec![
            HistorySourceSelection::AllIncluded,
            HistorySourceSelection::Remote(missing_id.clone()),
        ]
    );

    let unavailable = TuiHistoryProjection {
        history: HistoryData::default(),
        selection: HistorySourceSelection::Remote(missing_id.clone()),
        status: Some(HistorySourceSelectionStatus::Unavailable(
            HistorySourceUnavailableReason::NotFound,
        )),
        query_error: None,
    };
    assert!(app.apply_history_projection(0, unavailable));
    assert_eq!(
        app.history_source_applied_selection,
        HistorySourceSelection::Remote(missing_id)
    );
    assert!(app.history_source_scope_label().contains("UNAVAILABLE"));
}

#[test]
fn excluded_exact_source_is_labeled_as_inspect_only_instead_of_unavailable() {
    let mut app = interaction_test_app(1, 1);
    let source_id: NodeId = "node-55555555555555555555555555555555".parse().unwrap();
    app.history_source_selection = HistorySourceSelection::Remote(source_id.clone());
    app.history_source_applied_selection = HistorySourceSelection::Remote(source_id);
    app.history_source_status = Some(HistorySourceSelectionStatus::AppliedExcludedFromAggregates);

    assert!(
        app.history_source_scope_label()
            .contains("EXCLUDED (inspect only)")
    );
    assert!(app.history_source_compact_scope_label().ends_with(" EXCL"));
}

#[test]
fn detached_recorded_ssh_source_remains_in_the_source_cycle() {
    let mut app = interaction_test_app(1, 1);
    let detached_id: NodeId = "node-44444444444444444444444444444444".parse().unwrap();
    app.history_remote_sources = vec![(detached_id.clone(), "retired-build-host".to_string())];

    assert_eq!(
        app.history_source_choices(),
        vec![
            HistorySourceSelection::AllIncluded,
            HistorySourceSelection::Remote(detached_id.clone()),
        ]
    );
    app.cycle_history_source();
    assert_eq!(
        app.history_source_selection,
        HistorySourceSelection::Remote(detached_id.clone())
    );
    assert_eq!(
        app.history_source_label(&HistorySourceSelection::Remote(detached_id)),
        "SSH retired-build-host"
    );
}

#[test]
fn source_controls_are_keyboard_styled_and_whole_label_clickable_in_compact_themes() {
    let local_id: NodeId = "node-22222222222222222222222222222222".parse().unwrap();
    for (view, theme, width) in [
        (View::Summary, Theme::Dark, 120),
        (View::Summary, Theme::Light, 60),
        (View::Trends, Theme::Dark, 120),
        (View::Trends, Theme::Light, 60),
    ] {
        let mut app = interaction_test_app(1, 1);
        app.view = view;
        app.theme = theme;
        app.history_local_source_id = Some(local_id.clone());
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| render_at(frame, &mut app, Utc::now()))
            .unwrap();
        let hitbox = app.history_source_control_hitbox;
        assert!(!hitbox.is_empty(), "{view:?} {theme:?} width={width}");
        assert_eq!(
            terminal.backend().buffer()[(hitbox.x, hitbox.y)].symbol(),
            "["
        );
        assert_eq!(
            terminal.backend().buffer()[(hitbox.x + 1, hitbox.y)].symbol(),
            "S"
        );
        assert_eq!(
            terminal.backend().buffer()[(hitbox.x + 1, hitbox.y)].fg,
            theme.palette().accent
        );
        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('S'))));
        assert_eq!(
            app.history_source_selection,
            HistorySourceSelection::Local(local_id.clone())
        );
        app.history_source_selection = HistorySourceSelection::AllIncluded;
        app.history_source_loading = false;
        app.history_source_query_pending = false;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.right() - 1,
                hitbox.y,
            ),
        ));
        assert_eq!(
            app.history_source_selection,
            HistorySourceSelection::Local(local_id.clone())
        );
    }
}

#[test]
fn filtered_history_titles_name_the_scope_while_quota_is_explicitly_global() {
    let now = DateTime::parse_from_rfc3339("2026-08-29T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let local_id: NodeId = "node-22222222222222222222222222222222".parse().unwrap();
    let mut app = interaction_test_app(1, 1);
    app.history_local_source_id = Some(local_id.clone());
    app.request_history_source(HistorySourceSelection::Local(local_id.clone()));
    let generation = app.history_source_generation;
    assert!(app.apply_history_projection(
        generation,
        TuiHistoryProjection {
            history: trend_history_fixture(now),
            selection: HistorySourceSelection::Local(local_id),
            status: Some(HistorySourceSelectionStatus::Applied),
            query_error: None,
        },
    ));

    app.view = View::Trends;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    let trends = buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, 120, 40));
    assert!(trends.contains("Quota Remaining · GLOBAL"));
    assert!(trends.contains("Weekly Local Tokens"));
    assert!(trends.contains("source Local"));

    app.view = View::Summary;
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    let summary = buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, 120, 40));
    assert!(summary.contains("Usage tree · source Local"));
}

#[test]
fn remote_summary_never_overlays_same_id_local_task_metadata() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    app.snapshot.tasks[0].thread_id = "shared-thread".to_string();
    app.snapshot.tasks[0].title = "local title must not leak".to_string();
    app.snapshot.tasks[0].cwd = Some("/tmp/local-project".into());
    let mut bucket = tui_runtime_test_bucket(now - ChronoDuration::minutes(15), 42);
    bucket.project_groups[0] = LocalProjectUsageGroup {
        thread_id: "shared-thread".to_string(),
        project_id: Some("remote-project-id".to_string()),
        project_label: Some("remote-project".to_string()),
        title: Some("remote title".to_string()),
        source: Some("ssh".to_string()),
        token_usage: bucket.token_usage,
        estimated_cost_units: bucket.estimated_cost_units,
        api_long_context_extra_cost_units: bucket.api_long_context_extra_cost_units,
        api_equivalent_cost: ApiCostAmount::default(),
        call_count: 1,
        ..LocalProjectUsageGroup::default()
    };
    app.history.half_hour_buckets = vec![bucket];
    app.history_source_applied_selection =
        HistorySourceSelection::Remote("node-ffffffffffffffffffffffffffffffff".parse().unwrap());
    app.summary_range = SummaryRange::SevenDays;

    let prepared = prepare_summary(&app, now);
    let remote_project = prepared
        .usage
        .projects
        .iter()
        .find(|project| project.key == "remote-project-id")
        .expect("remote project remains attributed by remote history");
    assert_eq!(remote_project.label, "remote-project");
    assert_eq!(
        remote_project.sessions[0].title.as_deref(),
        Some("remote title")
    );
    assert!(
        prepared
            .usage
            .projects
            .iter()
            .all(|project| project.label != "local-project")
    );
}

#[test]
fn saved_tree_mode_starts_with_every_parent_collapsed() {
    let mut app = interaction_test_app(3, 1);
    set_task_parent(&mut app, 0, 2);
    app.selected_task = 0;
    let saved = UiState {
        task_list_mode: UiTaskListMode::Tree,
        ..UiState::default()
    };

    app.apply_ui_state(&saved, None);

    assert_eq!(app.task_list_mode, TaskListMode::Tree);
    assert!(app.expanded_task_threads.is_empty());
    assert!(app.all_filtered_task_threads_collapsed());
    assert_eq!(app.filtered_task_indices(), vec![2, 1]);
    assert_eq!(app.selected_task, 2);
}

#[test]
fn status_legend_compacts_at_the_horizontal_layout_breakpoint() {
    let compact = status_legend(Theme::Light, 46);
    let full = status_legend(Theme::Light, 80);
    let compact_text = compact
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(compact.width() <= 44);
    assert!(full.width() > compact.width());
    assert!(compact_text.contains("R:RUN"));
    assert!(compact_text.contains("?:STALE"));
}

#[test]
fn task_footer_prioritizes_the_legend_and_keeps_status_when_space_allows() {
    let status_width = 27;
    let (narrow, narrow_status) = task_footer_legend(Theme::Light, 46, status_width);
    let (split, split_status) = task_footer_legend(Theme::Light, 62, status_width);
    let (medium, medium_status) = task_footer_legend(Theme::Light, 80, status_width);
    let (wide, wide_status) = task_footer_legend(Theme::Light, 120, status_width);

    assert!(narrow.width() <= 44);
    assert!(!narrow_status);
    assert!(split.width() > narrow.width());
    assert!(split.width() <= 60);
    assert!(!split_status);
    assert!(medium.width() + 1 + usize::from(status_width) <= 78);
    assert!(medium_status);
    assert!(wide.width() > medium.width());
    assert!(wide_status);
}

#[test]
fn every_available_estimate_is_marked_as_approximate() {
    assert_eq!(format_estimated_quota(0.0, Confidence::Unknown), "-");
    assert_eq!(format_estimated_quota(99.0, Confidence::Unknown), "-");
    assert_eq!(format_estimated_quota(0.0, Confidence::Low), "~0.0%");
    assert_eq!(format_estimated_quota(2.26, Confidence::Low), "~2.3%");
    assert_eq!(format_estimated_quota(2.26, Confidence::Medium), "~2.3%");
    assert_eq!(format_estimated_quota(2.26, Confidence::High), "~2.3%");
}

#[test]
fn attribution_summary_explains_the_credit_rate_weighted_codex_share_formula() {
    let now = chrono::Utc::now();
    let attribution = AttributionSummary {
        window: Some(WindowDescriptor {
            limit_id: "codex".to_string(),
            label: "week".to_string(),
            starts_at: now - chrono::Duration::days(7),
            ends_at: now,
            used_percent: 34.0,
        }),
        local_token_usage: TokenUsage {
            total_tokens: 760_000_000,
            ..TokenUsage::default()
        },
        proxy_projected_percent: 34.0,
        confidence: Confidence::Low,
        ..AttributionSummary::default()
    };

    let compact =
        attribution_summary_lines(Some(&attribution), WindowScope::Week, false, &[], true);
    assert!(compact[1].contains("EST ~34.00pp"));
    assert!(compact[1].contains("codex gauge × credit-rate share"));
    assert!(compact[2].contains("Credit-rate-weighted quota proxy"));
    assert!(compact[2].contains("not server accounting"));
    assert!(!compact[2].contains("confidence"));

    let wide = attribution_summary_lines(Some(&attribution), WindowScope::Week, false, &[], false);
    assert!(wide[1].contains("~34.00pp estimated"));
    assert!(wide[1].contains("codex gauge × credit-rate share"));
    assert!(wide[2].contains("Credit-rate-weighted quota proxy"));
    assert!(wide[2].contains("not server per-task accounting"));
    assert!(!wide[2].contains("confidence"));
    assert!(!wide.join(" ").contains("evidence"));
    assert!(!wide.join(" ").contains("gap"));

    let unavailable = AttributionSummary {
        confidence: Confidence::Unknown,
        ..attribution
    };
    let unavailable_lines =
        attribution_summary_lines(Some(&unavailable), WindowScope::Week, false, &[], true);
    assert!(unavailable_lines[1].contains("EST -"));
    assert!(unavailable_lines[1].contains("estimate unavailable"));
    assert!(!unavailable_lines[1].contains("EST ~"));

    let no_window = AttributionSummary {
        window: None,
        confidence: Confidence::Low,
        ..unavailable
    };
    let no_window_lines =
        attribution_summary_lines(Some(&no_window), WindowScope::Week, false, &[], true);
    assert!(no_window_lines[1].contains("EST -"));
    assert!(no_window_lines[1].contains("no quota window"));
    assert!(!no_window_lines[1].contains("EST ~"));
}

#[test]
fn limit_window_labels_known_windows() {
    assert_eq!(LimitWindow::new(10.0, Some(300), None).label(), "5h");
    assert_eq!(LimitWindow::new(10.0, Some(10_080), None).label(), "week");
}

#[test]
fn models_panel_explains_missing_five_hour_with_weekly_data() {
    let mut app = interaction_test_app(0, 0);
    let now = app.snapshot.as_of;
    app.snapshot.limits = vec![LimitBucket {
        limit_id: "codex".to_string(),
        limit_name: None,
        plan_type: Some("test".to_string()),
        primary: Some(LimitWindow::new(
            23.0,
            Some(10_080),
            Some(now + chrono::Duration::days(6)),
        )),
        secondary: Some(LimitWindow::new(
            5.0,
            Some(1_440),
            Some(now + chrono::Duration::hours(20)),
        )),
        credits: None,
        rate_limit_reached_type: None,
        provenance: Provenance::ServerSnapshot,
        as_of: now,
    }];

    let content = render_models_content(&app.snapshot, 120, 8);

    assert!(content.contains("Models · 5h unavailable"));
    assert!(content.contains("weekly reset-cycle data remains available"));
}

#[test]
fn models_panel_distinguishes_an_empty_active_window() {
    let mut app = interaction_test_app(0, 0);
    let now = app.snapshot.as_of;
    app.snapshot.attribution.window = Some(WindowDescriptor {
        limit_id: "codex".to_string(),
        label: "5h".to_string(),
        starts_at: now - chrono::Duration::hours(1),
        ends_at: now + chrono::Duration::hours(4),
        used_percent: 0.0,
    });

    let content = render_models_content(&app.snapshot, 100, 8);

    assert!(content.contains("Models · 5h"));
    assert!(content.contains("No token usage in the current 5h window"));
    assert!(!content.contains("5h unavailable"));
}

#[test]
fn models_panel_prioritizes_token_usage_and_reports_clipping() {
    let mut app = interaction_test_app(0, 0);
    let now = app.snapshot.as_of;
    app.snapshot.attribution.window = Some(WindowDescriptor {
        limit_id: "codex".to_string(),
        label: "5h".to_string(),
        starts_at: now - chrono::Duration::hours(1),
        ends_at: now + chrono::Duration::hours(4),
        used_percent: 10.0,
    });
    app.snapshot.models = vec![
        model_usage("small-model", 10),
        model_usage("largest-model", 1_000),
        model_usage("medium-model", 100),
    ];

    let compact = render_models_content(&app.snapshot, 100, 7);
    assert!(compact.contains("Models · 5h · top 1/3"));
    assert!(compact.contains("largest-model"));
    assert!(!compact.contains("small-model"));
    assert!(!compact.contains("medium-model"));

    let expanded = render_models_content(&app.snapshot, 100, 10);
    let largest = expanded.find("largest-model").unwrap();
    let medium = expanded.find("medium-model").unwrap();
    let small = expanded.find("small-model").unwrap();
    assert!(largest < medium && medium < small);
    assert!(!expanded.contains("top 3/3"));
}
#[test]
fn models_panel_keeps_the_codex_share_formula_visible_at_eighty_columns() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 111, 100.0);
    app.snapshot.window_analyses[0].partial = true;
    app.snapshot.window_analyses[0]
        .partial_reasons
        .push("rollout_scan_incomplete".to_string());

    let content = render_models_content(&app.snapshot, 80, 8);

    assert!(content.contains("EST ~23.00pp"));
    assert!(content.contains("codex gauge × credit-rate share"));
    assert!(content.contains("Credit-rate-weighted quota proxy"));
    assert!(content.contains("not server accounting"));
    assert!(!content.contains("confidence"));
    assert!(content.contains("external"));
    assert!(content.contains("settled"));
    assert!(content.contains("partial"));
    assert!(content.contains("rollout_scan_incomplete"));
    assert!(!content.contains("CONF"));
    assert!(!content.contains("evidence"));
    assert!(!content.contains("gap"));
}

#[test]
fn compact_models_table_prioritizes_tokens_estimate_and_api_cost_without_confidence() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 111, 100.0);

    let content = render_models_content(&app.snapshot, 60, 12);

    assert!(content.contains("MODEL"));
    assert!(content.contains("TOKENS"));
    assert!(content.contains("API EQ."));
    assert!(!content.contains("TOKEN SHARE"));
    assert!(content.contains("EST. QUOTA"));
    assert!(content.contains("gpt-window"));
    assert!(content.contains("$0.0000"));
    assert!(!content.contains("CONF"));
}

#[test]
fn models_panel_shows_api_equivalent_summary_and_wide_cost_column() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 111, 100.0);
    let analysis = &mut app.snapshot.window_analyses[0];
    analysis.api_pricing = crate::api_cost::pricing_metadata();
    analysis.api_equivalent_cost = ApiEquivalentCost {
        amount: ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(1_234_500_000_000),
            maximum_pico_usd: PicoUsd::new(1_234_500_000_000),
            observed_samples: 2,
            priced_samples: 1,
            observed_tokens: 400,
            priced_tokens: 300,
        },
        partial_reasons: vec!["api_price_model_unknown".to_string()],
        model_breakdown: Vec::new(),
    };
    analysis.models[0].api_equivalent_cost = exact_api_cost(250_000_000_000);

    let wide = render_models_content(&app.snapshot, 120, 12);
    assert!(wide.contains("API equivalent $1.2345"));
    assert!(wide.contains("model calls only"));
    assert!(wide.contains("coverage 75.0%"));
    assert!(wide.contains("rates 2026-08-27"));
    assert!(wide.contains("API EQ."));
    assert!(wide.contains("$0.2500"));

    let compact = render_models_content(&app.snapshot, 80, 12);
    assert!(compact.contains("API equivalent $1.2345"));
    assert!(compact.contains("model calls only"));
    assert!(compact.contains("coverage 75.0%"));
    assert!(compact.contains("rates 2026-08-27"));
    assert!(compact.contains("API EQ."));
    assert!(compact.contains("$0.2500"));
}

#[test]
fn tui_ignores_bengalfox_analysis_for_rows_models_and_summary() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::Week, 150, 100.0);
    add_spark_window_analysis(&mut app, WindowScope::Week, 50);

    let task_usage = task_usage_for_scope(&app.snapshot, WindowScope::Week, &app.snapshot.tasks[0]);
    assert_eq!(task_usage.token_usage.total_tokens, 150);
    assert_eq!(task_usage.local_token_share_percent, 100.0);
    assert_eq!(task_usage.estimated_quota_percent, 23.0);
    assert_eq!(task_usage.quota_confidence, Confidence::Low);

    let models = models_for_scope(&app.snapshot, WindowScope::Week);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model, "gpt-window");

    let content = render_models_content_for_scope(&app.snapshot, WindowScope::Week, 120, 10);
    assert!(content.contains("Models · week"));
    assert!(content.contains("gpt-window"));
    assert!(!content.contains("codex_bengalfox"));
    assert!(!content.contains("gpt-5.3-codex-spark"));
    assert!(!content.contains("buckets"));

    app.snapshot.window_analyses.retain(|analysis| {
        analysis
            .attribution
            .window
            .as_ref()
            .is_some_and(|window| window.limit_id.eq_ignore_ascii_case("codex_bengalfox"))
    });
    assert!(models_for_scope(&app.snapshot, WindowScope::Week).is_empty());
    assert_eq!(
        task_usage_for_scope(&app.snapshot, WindowScope::Week, &app.snapshot.tasks[0]),
        WindowUsage::default()
    );
    let unavailable = render_models_content_for_scope(&app.snapshot, WindowScope::Week, 120, 10);
    assert!(unavailable.contains("Models · Week unavailable"));
    assert!(!unavailable.contains("gpt-5.3-codex-spark"));
}

#[test]
fn tui_ignores_legacy_five_hour_fields_without_a_codex_window() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    app.snapshot.attribution.window = Some(WindowDescriptor {
        limit_id: "codex_bengalfox".to_string(),
        label: "5h".to_string(),
        starts_at: now - chrono::Duration::hours(4),
        ends_at: now + chrono::Duration::hours(1),
        used_percent: 50.0,
    });
    app.snapshot.tasks[0].window_token_usage.total_tokens = 100;
    app.snapshot.tasks[0].estimated_quota_percent = 50.0;
    app.snapshot.models = vec![model_usage("gpt-5.3-codex-spark", 100)];

    assert_eq!(
        task_usage_for_scope(
            &app.snapshot,
            WindowScope::FiveHours,
            &app.snapshot.tasks[0]
        ),
        WindowUsage::default()
    );
    assert!(models_for_scope(&app.snapshot, WindowScope::FiveHours).is_empty());
    assert!(attribution_for_scope(&app.snapshot, WindowScope::FiveHours).is_none());
}

#[test]
fn models_panel_reports_missing_token_denominator_without_a_fake_estimate() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::Week, 0, 0.0);
    let analysis = &mut app.snapshot.window_analyses[0];
    analysis.attribution.proxy_projected_percent = 0.0;
    analysis.attribution.confidence = Confidence::Unknown;
    analysis.models.clear();

    let content = render_models_content_for_scope(&app.snapshot, WindowScope::Week, 120, 10);
    assert!(content.contains("token denominator"));
    assert!(content.contains("No token usage in the current week window"));
    assert!(!content.contains("EST ~"));
}
#[test]
fn task_hitbox_maps_only_its_visible_rows() {
    let hitbox = TableHitbox {
        viewport: Rect::new(9, 6, 32, 8),
        rows: Rect::new(10, 7, 30, 5),
        offset: 8,
        capacity: 5,
    };

    assert_eq!(hitbox.index_at(10, 7), Some(8));
    assert_eq!(hitbox.index_at(39, 11), Some(12));
    assert_eq!(hitbox.index_at(9, 7), None);
    assert_eq!(hitbox.index_at(40, 7), None);
    assert_eq!(hitbox.index_at(10, 6), None);
    assert_eq!(hitbox.index_at(10, 12), None);
    assert!(hitbox.contains_viewport(9, 6));
    assert!(!hitbox.contains_viewport(41, 6));
}

#[test]
fn viewport_offsets_scroll_and_reveal_with_clamped_bounds() {
    assert_eq!(scroll_offset(0, 20, 5, true, MOUSE_SCROLL_LINES), 3);
    assert_eq!(scroll_offset(14, 20, 5, true, MOUSE_SCROLL_LINES), 15);
    assert_eq!(scroll_offset(2, 20, 5, false, MOUSE_SCROLL_LINES), 0);
    assert_eq!(scroll_offset(9, 3, 5, true, MOUSE_SCROLL_LINES), 0);

    assert_eq!(reveal_offset(3, 2, 20, 5), 2);
    assert_eq!(reveal_offset(3, 7, 20, 5), 3);
    assert_eq!(reveal_offset(3, 8, 20, 5), 4);
    assert_eq!(reveal_offset(20, 19, 20, 5), 15);
}

#[test]
fn unicode_text_helpers_preserve_cursor_and_display_width_contracts() {
    let value = "a测b";
    assert_eq!(byte_index_at_grapheme(value, 0), 0);
    assert_eq!(byte_index_at_grapheme(value, 1), 1);
    assert_eq!(byte_index_at_grapheme(value, 2), 4);
    assert_eq!(byte_index_at_grapheme(value, usize::MAX), value.len());
    assert_eq!(grapheme_count("e\u{301}👩\u{200d}💻"), 2);
    assert_eq!(byte_index_at_grapheme("e\u{301}x", 1), "e\u{301}".len());

    assert_eq!(compact_search_text("abcdef测试", 5), "<测试");
    assert_eq!(compact_search_text("测试", 4), "测试");
    assert_eq!(
        compact_search_text("prefix👩\u{200d}💻", 3),
        "<👩\u{200d}💻"
    );
    assert_eq!(compact_search_text("prefixe\u{301}", 2), "<e\u{301}");
    assert_eq!(compact_search_text("测试", 0), "");

    assert_eq!(truncate_display_text("ab测试", 5), "ab测…");
    assert_eq!(truncate_display_text("👩\u{200d}💻ab", 3), "👩\u{200d}💻…");
    assert_eq!(truncate_display_text("unchanged", 32), "unchanged");
    assert_eq!(truncate_display_text("anything", 1), "…");
    assert_eq!(truncate_display_text("anything", 0), "");

    let middle = truncate_middle_display_text("/workspace/项目/src/main.rs", 12);
    assert_eq!(middle, "/wo…/main.rs");
    assert_eq!(UnicodeWidthStr::width(middle.as_str()), 12);
    assert_eq!(
        truncate_middle_display_text("abc👩\u{200d}💻z", 5),
        "a…👩\u{200d}💻z"
    );
    assert_eq!(truncate_middle_display_text("anything", 1), "…");
    assert_eq!(truncate_middle_display_text("anything", 0), "");

    assert_eq!(short_thread_id("019f52ac-remaining"), "019f52ac");
    assert_eq!(short_thread_id("short"), "short");

    assert_eq!(
        search_cursor_window("abc", 1, 0),
        (String::new(), String::new(), false)
    );
    let (before, after, visible) = search_cursor_window("ab测试cd", 4, 5);
    assert!(visible);
    assert_eq!(format!("{before}{after}"), "试cd");
    assert!(UnicodeWidthStr::width(before.as_str()) <= 4);
    assert!(UnicodeWidthStr::width(after.as_str()) <= 4);

    let (before, after, visible) = search_cursor_window("a👩\u{200d}💻b", 2, 4);
    assert!(visible);
    assert_eq!(before, "👩\u{200d}💻");
    assert_eq!(after, "b");
}

#[test]
fn scrollbar_geometry_maps_offsets_to_a_proportional_thumb() {
    let track = Rect::new(79, 7, 1, 10);
    assert!(scrollbar_geometry(track, 10, 10, 0).is_none());
    assert!(scrollbar_geometry(Rect::default(), 100, 10, 0).is_none());
    assert!(scrollbar_geometry(Rect::new(0, 0, 1, 1), 2, 1, 0).is_none());
    assert_eq!(
        scrollbar_geometry(Rect::new(0, 0, 1, 4), 5, 4, 0)
            .unwrap()
            .thumb
            .height,
        3
    );

    let top = scrollbar_geometry(track, 100, 20, 0).unwrap();
    assert_eq!(top.thumb, Rect::new(79, 7, 1, 2));
    assert_eq!(top.max_offset, 80);

    let middle = scrollbar_geometry(track, 100, 20, 40).unwrap();
    assert_eq!(middle.thumb, Rect::new(79, 11, 1, 2));

    let bottom = scrollbar_geometry(track, 100, 20, 80).unwrap();
    assert_eq!(bottom.thumb, Rect::new(79, 15, 1, 2));
    assert_eq!(scale_rounded(0, 80, 8), 0);
    assert_eq!(scale_rounded(4, 80, 8), 40);
    assert_eq!(scale_rounded(8, 80, 8), 80);
}

#[test]
fn turn_detail_allocation_never_reduces_table_capacity_as_height_grows() {
    let mut previous_capacity = 0;
    for height in 7_u16..=40 {
        let capacity = height
            .saturating_sub(turn_detail_height(height))
            .saturating_sub(3);
        assert!(
            capacity >= previous_capacity,
            "turn capacity shrank at height {height}"
        );
        previous_capacity = capacity;
    }
}

#[test]
fn task_filters_match_title_and_source_as_an_intersection() {
    let mut app = interaction_test_app(4, 2);
    set_task_metadata(&mut app, 0, "Alpha build", Some("desktop"));
    set_task_metadata(&mut app, 1, "beta review", Some("subagent"));
    set_task_metadata(&mut app, 2, "ALPHA tests", Some("cli"));
    set_task_metadata(&mut app, 3, "alpha archive", None);

    assert_eq!(app.filtered_task_indices(), vec![0, 1, 2, 3]);
    app.task_search = "alpha".to_string();
    app.reconcile_task_filter(true);
    assert_eq!(app.filtered_task_indices(), vec![0, 2, 3]);
    assert_eq!(app.selected_task, 0);

    app.set_task_source_filter(TaskSourceFilter::Cli);
    assert_eq!(app.filtered_task_indices(), vec![2]);
    assert_eq!(app.selected_task, 2);
    assert_eq!(app.selected_thread_id(), Some("task-thread-2"));
    assert_eq!(app.selected_task_turn_count(), 2);

    app.set_task_source_filter(TaskSourceFilter::Subagent);
    assert!(app.filtered_task_indices().is_empty());
    assert_eq!(app.selected_thread_id(), None);
    assert_eq!(app.selected_task_turn_count(), 0);

    app.set_task_source_filter(TaskSourceFilter::All);
    assert_eq!(app.filtered_task_indices(), vec![0, 2, 3]);
    assert_eq!(app.selected_task, 2);
    assert!(TaskSourceFilter::Desktop.matches(Some("vscode")));
}

#[test]
fn task_filter_matches_title_or_project_basename_but_not_parent_path() {
    let mut app = interaction_test_app(4, 1);
    set_task_metadata(&mut app, 0, "unrelated request", Some("desktop"));
    set_task_metadata(&mut app, 1, "Codex Usage Monitor review", Some("subagent"));
    set_task_metadata(&mut app, 2, "another request", Some("cli"));
    set_task_metadata(&mut app, 3, "root task", Some("desktop"));
    app.snapshot.tasks[0].cwd = Some("/work/codex-usage-monit".into());
    app.snapshot.tasks[1].cwd = Some("/tmp/other-project".into());
    app.snapshot.tasks[2].cwd = Some("/else/codex-usage-monit".into());
    app.snapshot.tasks[3].cwd = Some("/".into());

    app.task_search = "CODEX-USAGE-MONIT".to_string();
    app.reconcile_task_filter(true);
    assert_eq!(app.filtered_task_indices(), vec![0, 2]);

    app.task_search = "codex usage monitor".to_string();
    app.reconcile_task_filter(true);
    assert_eq!(app.filtered_task_indices(), vec![1]);

    app.task_search = "work".to_string();
    app.reconcile_task_filter(true);
    assert!(app.filtered_task_indices().is_empty());

    app.task_search = "codex-usage-monit".to_string();
    app.set_task_source_filter(TaskSourceFilter::Cli);
    assert_eq!(app.filtered_task_indices(), vec![2]);
}

#[test]
fn tree_rows_group_visible_subagents_by_subtree_recency_and_break_cycles() {
    let mut app = interaction_test_app(8, 1);
    set_task_parent(&mut app, 0, 3);
    set_task_parent(&mut app, 2, 5);
    set_task_parent(&mut app, 3, 5);
    app.snapshot.tasks[4].source = Some("subagent".to_string());
    app.snapshot.tasks[4].parent_thread_id = Some("missing-parent".to_string());
    set_task_parent(&mut app, 6, 7);
    set_task_parent(&mut app, 7, 6);
    expand_task_tree(&mut app);

    let rows = app.filtered_task_rows();
    assert_eq!(
        rows.iter().map(|row| row.index).collect::<Vec<_>>(),
        vec![5, 3, 0, 2, 1, 4, 7, 6]
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.prefix.as_str())
            .collect::<Vec<_>>(),
        vec!["", "├─ ", "│ └─ ", "└─ ", "", "", "", "└─ "]
    );

    app.task_source_filter = TaskSourceFilter::Subagent;
    let rows = app.filtered_task_rows();
    assert_eq!(
        rows.iter().map(|row| row.index).collect::<Vec<_>>(),
        vec![3, 0, 2, 4, 7, 6]
    );
    assert!(
        rows.iter()
            .all(|row| { app.snapshot.tasks[row.index].source.as_deref() == Some("subagent") })
    );
    assert_eq!(rows[0].prefix, "");
    assert_eq!(rows[1].prefix, "└─ ");

    app.task_source_filter = TaskSourceFilter::All;
    app.task_search = "task 0".to_string();
    let rows = app.filtered_task_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].index, 0);
    assert!(rows[0].prefix.is_empty());
}

#[test]
fn remote_tree_defaults_collapsed_and_expands_nested_same_node_edges() {
    const NODE: &str = "node-0123456789abcdef0123456789abcdef";
    const OTHER_NODE: &str = "node-fedcba9876543210fedcba9876543210";
    let mut app = interaction_test_app(4, 1);
    set_remote_task_identity(&mut app, 0, NODE, "grandchild", Some("child"));
    set_remote_task_identity(&mut app, 1, NODE, "child", Some("root"));
    set_remote_task_identity(&mut app, 2, OTHER_NODE, "other-root", None);
    set_remote_task_identity(&mut app, 3, NODE, "root", None);
    app.task_list_mode = TaskListMode::Tree;

    let collapsed = app.filtered_task_rows();
    assert_eq!(
        collapsed.iter().map(|row| row.index).collect::<Vec<_>>(),
        vec![3, 2]
    );
    assert!(collapsed[0].has_children);
    assert!(collapsed[0].collapsed);
    assert_eq!(collapsed[0].hidden_descendants, vec![1, 0]);

    assert!(app.set_task_collapsed(3, false));
    let root_expanded = app.filtered_task_rows();
    assert_eq!(
        root_expanded
            .iter()
            .map(|row| row.index)
            .collect::<Vec<_>>(),
        vec![3, 1, 2]
    );
    assert_eq!(root_expanded[1].depth, 1);
    assert!(root_expanded[1].collapsed);

    assert!(app.set_task_collapsed(1, false));
    let nested_expanded = app.filtered_task_rows();
    assert_eq!(
        nested_expanded
            .iter()
            .map(|row| row.index)
            .collect::<Vec<_>>(),
        vec![3, 1, 0, 2]
    );
    assert_eq!(
        nested_expanded
            .iter()
            .map(|row| row.depth)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 0]
    );

    app.selected_task = 0;
    app.selected_turn = 0;
    assert!(app.set_task_collapsed(3, true));
    assert_eq!(app.selected_task, 3);
    assert_eq!(
        app.selected_thread_id(),
        Some(format!("remote:{NODE}:root").as_str())
    );
}

#[test]
fn remote_tree_rejects_cross_node_malformed_and_missing_parent_edges() {
    const NODE_A: &str = "node-0123456789abcdef0123456789abcdef";
    const NODE_B: &str = "node-fedcba9876543210fedcba9876543210";
    let mut app = interaction_test_app(6, 1);

    set_remote_task_identity(&mut app, 0, NODE_A, "cross-node-child", None);
    set_remote_task_identity(&mut app, 3, NODE_B, "cross-node-parent", None);
    app.snapshot.tasks[0].parent_thread_id = Some(format!("remote:{NODE_B}:cross-node-parent"));

    set_remote_task_identity(&mut app, 1, NODE_A, "orphan", Some("missing-parent"));

    app.snapshot.tasks[2].thread_id = "remote:not-a-node:malformed-child".to_string();
    app.snapshot.tasks[2].parent_thread_id = Some(format!("remote:{NODE_A}:valid-parent"));
    app.snapshot.tasks[2].source = Some("remote:dev-server".to_string());
    set_remote_task_identity(&mut app, 4, NODE_A, "valid-parent", None);

    app.snapshot.tasks[5].parent_thread_id = Some(app.snapshot.tasks[4].thread_id.clone());
    app.snapshot.tasks[5].source = Some("desktop".to_string());

    expand_task_tree(&mut app);
    let rows = app.filtered_task_rows();
    assert_eq!(rows.len(), 6);
    assert!(rows.iter().all(|row| row.depth == 0));
    assert!(rows.iter().all(|row| !row.has_children));
}

#[test]
fn tree_collapse_hides_nested_rows_keeps_rank_and_promotes_filtered_orphans() {
    let mut app = interaction_test_app(6, 2);
    set_task_parent(&mut app, 0, 3);
    set_task_parent(&mut app, 2, 5);
    set_task_parent(&mut app, 3, 5);
    app.snapshot.tasks[4].source = Some("subagent".to_string());
    app.snapshot.tasks[4].parent_thread_id = Some("missing-parent".to_string());
    expand_task_tree(&mut app);

    let rows = app.filtered_task_rows();
    assert_eq!(
        rows.iter().map(|row| row.index).collect::<Vec<_>>(),
        vec![5, 3, 0, 2, 1, 4]
    );
    assert_eq!(rows.iter().find(|row| row.index == 0).unwrap().depth, 2);
    assert_eq!(rows.iter().find(|row| row.index == 4).unwrap().depth, 0);
    assert!(!task_display_label(&app.snapshot.tasks[0], true).contains("project |"));
    assert!(task_display_label(&app.snapshot.tasks[4], false).contains("project |"));

    app.selected_task = 0;
    app.selected_turn = 1;
    app.turn_offset = 1;
    assert!(app.set_task_collapsed(3, true));
    assert_eq!(app.selected_task, 3);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);
    assert_eq!(
        app.filtered_task_indices(),
        vec![5, 3, 2, 1, 4],
        "the hidden newest grandchild must still rank its branch first"
    );

    assert!(app.set_task_collapsed(5, true));
    assert_eq!(app.selected_task, 5);
    assert_eq!(app.filtered_task_indices(), vec![5, 1, 4]);

    app.task_search = "task 0".to_string();
    let rows = app.filtered_task_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].index, 0);
    assert_eq!(rows[0].depth, 0);
    assert!(!rows[0].has_children);
    assert!(task_display_label(&app.snapshot.tasks[0], false).contains("project |"));

    app.task_list_mode = TaskListMode::Flat;
    let flat = app.filtered_task_rows();
    assert_eq!(flat[0].depth, 0);
    assert!(task_display_label(&app.snapshot.tasks[0], false).contains("project |"));
}

#[test]
fn collapsed_tree_rows_aggregate_the_hidden_subtree_for_each_scope() {
    let mut app = interaction_test_app(3, 1);
    app.snapshot.attribution.window = Some(WindowDescriptor {
        limit_id: "codex".to_string(),
        label: "5h".to_string(),
        starts_at: app.snapshot.as_of - chrono::Duration::hours(4),
        ends_at: app.snapshot.as_of + chrono::Duration::hours(1),
        used_percent: 1.0,
    });
    set_task_parent(&mut app, 0, 2);
    set_task_parent(&mut app, 1, 0);
    let totals = [
        TokenUsage {
            input_tokens: 11,
            cached_input_tokens: 2,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 2,
            total_tokens: 20,
        },
        TokenUsage {
            input_tokens: 13,
            cached_input_tokens: 3,
            cache_write_input_tokens: 0,
            output_tokens: 10,
            reasoning_output_tokens: 4,
            total_tokens: 30,
        },
        TokenUsage {
            input_tokens: 4,
            cached_input_tokens: 1,
            cache_write_input_tokens: 0,
            output_tokens: 4,
            reasoning_output_tokens: 1,
            total_tokens: 10,
        },
    ];
    let five_hour_totals = [2, 3, 1];
    let shares = [20.0, 30.0, 10.0];
    let quotas = [0.2, 0.3, 0.1];
    let confidences = [Confidence::High, Confidence::Unknown, Confidence::Medium];
    for index in 0..3 {
        let task = &mut app.snapshot.tasks[index];
        task.token_usage = totals[index];
        task.window_token_usage = TokenUsage {
            input_tokens: u64::from(index != 0) + 1,
            total_tokens: five_hour_totals[index],
            ..TokenUsage::default()
        };
        task.local_token_share_percent = shares[index];
        task.estimated_quota_percent = quotas[index];
        task.quota_confidence = confidences[index];
        task.api_equivalent_cost = Some(exact_api_cost(
            u128::try_from(index + 1).unwrap() * 100_000_000_000,
        ));
    }
    let weekly_usages = [
        WindowUsage {
            token_usage: TokenUsage {
                total_tokens: 200,
                ..TokenUsage::default()
            },
            local_token_share_percent: 20.0,
            estimated_quota_percent: 2.0,
            quota_confidence: Confidence::High,
            api_equivalent_cost: exact_api_cost(400_000_000_000),
        },
        WindowUsage {
            token_usage: TokenUsage {
                total_tokens: 300,
                ..TokenUsage::default()
            },
            local_token_share_percent: 30.0,
            estimated_quota_percent: 3.0,
            quota_confidence: Confidence::Low,
            api_equivalent_cost: exact_api_cost(500_000_000_000),
        },
        WindowUsage {
            token_usage: TokenUsage {
                total_tokens: 100,
                ..TokenUsage::default()
            },
            local_token_share_percent: 10.0,
            estimated_quota_percent: 1.0,
            quota_confidence: Confidence::Medium,
            api_equivalent_cost: exact_api_cost(600_000_000_000),
        },
    ];
    app.snapshot.window_analyses.push(WindowAnalysis {
        duration_mins: WindowScope::Week.duration_mins(),
        attribution: AttributionSummary {
            window: Some(WindowDescriptor {
                limit_id: "codex".to_string(),
                label: "week".to_string(),
                starts_at: app.snapshot.as_of - chrono::Duration::days(5),
                ends_at: app.snapshot.as_of + chrono::Duration::days(2),
                used_percent: 10.0,
            }),
            ..AttributionSummary::default()
        },
        partial: false,
        partial_reasons: Vec::new(),
        threads: app
            .snapshot
            .tasks
            .iter()
            .zip(weekly_usages)
            .map(|(task, usage)| ThreadWindowUsage {
                thread_id: task.thread_id.clone(),
                usage,
            })
            .collect(),
        turns: Vec::new(),
        models: Vec::new(),
        api_equivalent_cost: Default::default(),
        api_pricing: Default::default(),
        api_long_context: None,
    });

    expand_task_tree(&mut app);
    app.selected_task = 2;
    assert!(app.set_task_collapsed(2, true));
    let rows = app.filtered_task_rows();
    let root = rows.iter().find(|row| row.index == 2).unwrap();
    assert_eq!(root.hidden_descendants, vec![0, 1]);

    let all_time = aggregate_task_row_usage(&app.snapshot, WindowScope::FiveHours, root, false);
    assert_eq!(all_time.token_usage.total_tokens, 60);
    assert_eq!(all_time.token_usage.input_tokens, 28);
    assert_eq!(all_time.token_usage.cached_input_tokens, 6);
    assert_eq!(all_time.token_usage.output_tokens, 19);
    assert_eq!(all_time.token_usage.reasoning_output_tokens, 7);

    let five_hours = aggregate_task_row_usage(&app.snapshot, WindowScope::FiveHours, root, true);
    assert_eq!(five_hours.token_usage.total_tokens, 6);
    assert_eq!(five_hours.local_token_share_percent, 60.0);
    assert!((five_hours.estimated_quota_percent - 0.6).abs() < f64::EPSILON);
    assert_eq!(five_hours.quota_confidence, Confidence::Unknown);
    assert_eq!(
        five_hours.api_equivalent_cost.minimum_pico_usd,
        PicoUsd::new(600_000_000_000)
    );

    let week = aggregate_task_row_usage(&app.snapshot, WindowScope::Week, root, true);
    assert_eq!(week.token_usage.total_tokens, 600);
    assert_eq!(week.local_token_share_percent, 60.0);
    assert_eq!(week.estimated_quota_percent, 6.0);
    assert_eq!(week.quota_confidence, Confidence::Low);
    assert_eq!(
        week.api_equivalent_cost.minimum_pico_usd,
        PicoUsd::new(1_500_000_000_000)
    );

    for task in &mut app.snapshot.tasks {
        task.estimated_quota_percent = 0.0;
        task.quota_confidence = Confidence::Low;
    }
    let known_zero = aggregate_task_row_usage(&app.snapshot, WindowScope::FiveHours, root, true);
    assert_eq!(known_zero.estimated_quota_percent, 0.0);
    assert_eq!(known_zero.quota_confidence, Confidence::Low);
    assert_eq!(
        format_estimated_quota(
            known_zero.estimated_quota_percent,
            known_zero.quota_confidence
        ),
        "~0.0%"
    );

    app.snapshot.tasks[1].quota_confidence = Confidence::Unknown;
    let unavailable = aggregate_task_row_usage(&app.snapshot, WindowScope::FiveHours, root, true);
    assert_eq!(unavailable.quota_confidence, Confidence::Unknown);
    assert_eq!(
        format_estimated_quota(
            unavailable.estimated_quota_percent,
            unavailable.quota_confidence
        ),
        "-"
    );

    assert!(app.set_task_collapsed(2, false));
    assert!(app.set_task_collapsed(0, true));
    let rows = app.filtered_task_rows();
    let root = rows.iter().find(|row| row.index == 2).unwrap();
    let child = rows.iter().find(|row| row.index == 0).unwrap();
    assert!(root.hidden_descendants.is_empty());
    assert_eq!(child.hidden_descendants, vec![1]);
    assert_eq!(
        aggregate_task_row_usage(&app.snapshot, WindowScope::FiveHours, child, true,)
            .token_usage
            .total_tokens,
        5
    );

    app.task_search = "task 2".to_string();
    let filtered_root = app.filtered_task_rows().pop().unwrap();
    assert!(filtered_root.hidden_descendants.is_empty());
    assert_eq!(
        aggregate_task_row_usage(&app.snapshot, WindowScope::FiveHours, &filtered_root, true,)
            .token_usage
            .total_tokens,
        1
    );

    app.task_search.clear();
    app.task_list_mode = TaskListMode::Flat;
    assert!(
        app.filtered_task_rows()
            .iter()
            .all(|row| row.hidden_descendants.is_empty())
    );
}

#[test]
fn collapsed_tree_root_renders_its_full_subtree_api_cost_range_and_unpriced_marker() {
    const PICO_USD_PER_USD: u128 = 1_000_000_000_000;

    let mut app = interaction_test_app(3, 0);
    set_task_metadata(&mut app, 2, "root pricing", Some("desktop"));
    set_task_parent(&mut app, 0, 2);
    set_task_parent(&mut app, 1, 0);
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);

    let analysis = app.snapshot.window_analyses.last_mut().unwrap();
    let template = analysis.threads[0].usage;
    let mut ranged_child = ranged_api_cost(2, 4);
    ranged_child.priced_samples = ranged_child.observed_samples;
    ranged_child.priced_tokens = ranged_child.observed_tokens;
    let unpriced_grandchild = ApiCostAmount {
        observed_samples: 1,
        observed_tokens: 100,
        ..ApiCostAmount::default()
    };
    analysis.threads = [
        (2, exact_api_cost(PICO_USD_PER_USD)),
        (0, ranged_child),
        (1, unpriced_grandchild),
    ]
    .into_iter()
    .map(|(task_index, api_equivalent_cost)| ThreadWindowUsage {
        thread_id: app.snapshot.tasks[task_index].thread_id.clone(),
        usage: WindowUsage {
            api_equivalent_cost,
            ..template
        },
    })
    .collect();

    expand_task_tree(&mut app);
    app.selected_task = 2;
    assert!(app.set_task_collapsed(2, true));

    let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
    terminal
        .draw(|frame| render_tasks(frame, frame.area(), &mut app, true))
        .unwrap();
    let rows = (0..12)
        .map(|y| buffer_rect_row_text(terminal.backend().buffer(), Rect::new(0, 0, 120, 12), y))
        .collect::<Vec<_>>();
    let root_row = rows
        .iter()
        .find(|row| row.contains("root pricing"))
        .expect("collapsed root row should render");

    assert!(root_row.contains("$3.0000–$5.0000+"), "{root_row}");
}

#[test]
fn tree_plus_minus_toggle_selected_parent_but_search_consumes_the_symbols() {
    let mut app = interaction_test_app(3, 2);
    set_task_parent(&mut app, 0, 2);
    app.task_list_mode = TaskListMode::Tree;
    app.selected_task = 2;

    assert!(!app.expanded_task_threads.contains("task-thread-2"));
    assert_eq!(app.filtered_task_indices(), vec![2, 1]);
    handle_key_event(&mut app, key_event(KeyCode::Char('+')));
    assert!(app.expanded_task_threads.contains("task-thread-2"));
    assert_eq!(app.filtered_task_indices(), vec![2, 0, 1]);

    handle_key_event(&mut app, key_event(KeyCode::Char('-')));
    assert!(!app.expanded_task_threads.contains("task-thread-2"));
    app.begin_task_search();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let marker = app
        .task_tree_marker_hitboxes
        .iter()
        .find(|marker| marker.task_index == 2)
        .unwrap();
    let shortcut = &terminal.backend().buffer()[(marker.area.x + 1, marker.area.y)];
    assert_eq!(shortcut.symbol(), "+");
    assert!(!shortcut.modifier.contains(Modifier::UNDERLINED));

    handle_key_event(&mut app, key_event(KeyCode::Char('-')));
    handle_key_event(&mut app, key_event(KeyCode::Char('+')));
    assert_eq!(app.task_search, "-+");
    assert!(!app.expanded_task_threads.contains("task-thread-2"));
}

#[test]
fn tree_marker_mouse_click_selects_once_and_has_stable_geometry_and_placeholder() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut app = interaction_test_app(4, 2);
        set_task_parent(&mut app, 0, 3);
        expand_task_tree(&mut app);
        app.theme = theme;
        app.selected_task = 1;
        app.selected_turn = 1;
        app.turn_offset = 1;
        app.focus = Focus::Turns;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let marker = app
            .task_tree_marker_hitboxes
            .iter()
            .find(|marker| marker.task_index == 3)
            .copied()
            .unwrap();
        assert_eq!(marker.area.width, TASK_TREE_MARKER_WIDTH);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(marker.area.x, marker.area.y)].symbol(), "[");
        assert_eq!(buffer[(marker.area.x + 1, marker.area.y)].symbol(), "-");
        assert_eq!(buffer[(marker.area.x + 2, marker.area.y)].symbol(), "]");

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                marker.area.right() - 1,
                marker.area.y,
            ),
        ));
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.selected_task, 3);
        assert_eq!(app.selected_turn, 0);
        assert_eq!(app.turn_offset, 0);
        assert!(!app.filtered_task_indices().contains(&0));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let collapsed = app
            .task_tree_marker_hitboxes
            .iter()
            .find(|marker| marker.task_index == 3)
            .copied()
            .unwrap();
        assert_eq!(collapsed.area, marker.area);
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(collapsed.area.x + 1, collapsed.area.y)].symbol(),
            "+"
        );
        assert!(
            buffer[(collapsed.area.x + 1, collapsed.area.y)]
                .modifier
                .contains(Modifier::UNDERLINED)
        );

        let leaf_position = app
            .filtered_task_indices()
            .iter()
            .position(|index| *index == 1)
            .unwrap();
        let viewport = app.task_table_hitbox.unwrap().viewport;
        let api_cost_width = api_cost_column_width(None, &[]);
        let task_column = *task_table_columns(
            viewport,
            task_visible_columns(app.table_columns, viewport.width, api_cost_width),
            api_cost_width,
        )
        .last()
        .unwrap();
        let leaf_y = app
            .task_table_hitbox
            .unwrap()
            .rows
            .y
            .saturating_add(u16::try_from(leaf_position).unwrap());
        assert!(
            (0..TASK_TREE_MARKER_WIDTH)
                .all(|offset| buffer[(task_column.x + offset, leaf_y)].symbol() == " ")
        );
    }
}

#[test]
fn compact_tree_marker_hitbox_matches_all_three_clickable_cells() {
    let mut app = interaction_test_app(4, 1);
    set_task_parent(&mut app, 0, 3);
    expand_task_tree(&mut app);
    app.turns_default_visible = false;
    app.selected_task = 3;
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();

    for (offset, expected_before) in [(0, "-"), (1, "+"), (2, "-")] {
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let marker = app
            .task_tree_marker_hitboxes
            .iter()
            .find(|marker| marker.task_index == 3)
            .copied()
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(marker.area.width, 3);
        assert_eq!(buffer[(marker.area.x, marker.area.y)].symbol(), "[");
        assert_eq!(
            buffer[(marker.area.x + 1, marker.area.y)].symbol(),
            expected_before
        );
        assert_eq!(buffer[(marker.area.x + 2, marker.area.y)].symbol(), "]");
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                marker.area.x + offset,
                marker.area.y,
            ),
        ));
        assert_eq!(
            app.expanded_task_threads.contains("task-thread-3"),
            expected_before == "+"
        );
    }
}

#[test]
fn tree_mode_and_refresh_move_a_newly_hidden_child_to_its_collapsed_parent() {
    let mut flat = interaction_test_app(4, 2);
    set_task_parent(&mut flat, 0, 3);
    flat.selected_task = 0;
    flat.selected_turn = 1;
    flat.turn_offset = 1;
    handle_key_event(&mut flat, key_event(KeyCode::Char('R')));
    assert_eq!(flat.task_list_mode, TaskListMode::Tree);
    assert_eq!(flat.selected_task, 3);
    assert_eq!(flat.selected_turn, 0);
    assert_eq!(flat.turn_offset, 0);
    assert!(!flat.filtered_task_indices().contains(&0));

    let mut refreshed = interaction_test_app(4, 2);
    refreshed.task_list_mode = TaskListMode::Tree;
    refreshed.selected_task = 0;
    refreshed.selected_turn = 1;
    refreshed.turn_offset = 1;
    let mut snapshot = refreshed.snapshot.clone();
    snapshot.tasks[0].source = Some("subagent".to_string());
    snapshot.tasks[0].parent_thread_id = Some("task-thread-3".to_string());
    refreshed.replace(
        CollectionResult {
            snapshot,
            account: refreshed.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert_eq!(refreshed.selected_task, 3);
    assert_eq!(refreshed.selected_turn, 0);
    assert_eq!(refreshed.turn_offset, 0);
    assert_eq!(refreshed.selected_thread_id(), Some("task-thread-3"));
    assert!(!refreshed.filtered_task_indices().contains(&0));
}

#[test]
fn refresh_retains_live_expansions_and_drops_removed_parent_state() {
    let mut app = interaction_test_app(4, 1);
    set_task_parent(&mut app, 0, 3);
    app.task_list_mode = TaskListMode::Tree;
    app.selected_task = 3;
    assert!(app.set_task_collapsed(3, false));
    let parent_id = app.snapshot.tasks[3].thread_id.clone();
    let child_id = app.snapshot.tasks[0].thread_id.clone();

    app.replace(
        CollectionResult {
            snapshot: app.snapshot.clone(),
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert!(app.expanded_task_threads.contains(&parent_id));
    assert!(
        app.filtered_task_indices()
            .iter()
            .any(|index| app.snapshot.tasks[*index].thread_id == child_id)
    );

    let mut snapshot = app.snapshot.clone();
    snapshot.tasks.retain(|task| task.thread_id != parent_id);
    app.replace(
        CollectionResult {
            snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert!(!app.expanded_task_threads.contains(&parent_id));
    let child = app
        .filtered_task_rows()
        .into_iter()
        .find(|row| app.snapshot.tasks[row.index].thread_id == child_id)
        .unwrap();
    assert_eq!(child.depth, 0);
    assert!(!child.has_children);
    assert!(task_display_label(&app.snapshot.tasks[child.index], false).contains("project |"));
}

#[test]
fn tree_keyboard_toggle_defaults_to_collapsed_and_search_consumes_the_shortcut() {
    let mut app = interaction_test_app(6, 2);
    set_task_parent(&mut app, 0, 4);
    app.selected_task = 0;
    app.selected_turn = 1;
    app.task_table_offset = 3;
    app.turn_offset = 1;

    handle_key_event(&mut app, key_event(KeyCode::Char('R')));
    assert_eq!(app.task_list_mode, TaskListMode::Tree);
    assert_eq!(app.selected_task, 4);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);
    assert_eq!(app.task_table_offset, 0);
    assert!(app.task_reveal_pending);

    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    handle_key_event(&mut app, key_event(KeyCode::Char('r')));
    assert_eq!(app.focus, Focus::TaskSearch);
    assert_eq!(app.task_search, "r");
    assert_eq!(app.task_list_mode, TaskListMode::Tree);
    handle_key_event(&mut app, key_event(KeyCode::Esc));

    app.focus = Focus::Turns;
    app.begin_turn_search();
    handle_key_event(&mut app, key_event(KeyCode::Char('R')));
    assert_eq!(app.focus, Focus::TurnSearch);
    assert_eq!(app.turn_search, "R");
    assert_eq!(app.task_list_mode, TaskListMode::Tree);
    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert_eq!(app.turn_search, "RE");
    assert!(app.expanded_task_threads.is_empty());
}

#[test]
fn collapse_all_toggles_nested_parents_and_moves_hidden_selection_to_its_root() {
    let mut app = interaction_test_app(6, 2);
    set_task_parent(&mut app, 0, 3);
    set_task_parent(&mut app, 3, 5);
    app.task_list_mode = TaskListMode::Tree;
    app.selected_task = 0;
    app.selected_turn = 1;
    app.turn_offset = 1;
    app.reconcile_task_filter(true);

    assert!(app.all_filtered_task_threads_collapsed());
    assert!(app.expanded_task_threads.is_empty());
    assert_eq!(app.selected_task, 5);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);
    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert!(app.expanded_task_threads.contains("task-thread-3"));
    assert!(app.expanded_task_threads.contains("task-thread-5"));
    assert!(!app.all_filtered_task_threads_collapsed());
    assert_eq!(app.filtered_task_rows().len(), 6);

    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert!(app.expanded_task_threads.is_empty());
    assert_eq!(app.selected_task, 5);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);
    assert!(app.all_filtered_task_threads_collapsed());

    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert!(app.expanded_task_threads.contains("task-thread-3"));
    assert!(app.expanded_task_threads.contains("task-thread-5"));
    assert_eq!(app.filtered_task_rows().len(), 6);

    assert!(app.set_task_collapsed(3, true));
    assert!(!app.all_filtered_task_threads_collapsed());
    let middle = app
        .filtered_task_rows()
        .into_iter()
        .find(|row| row.index == 3)
        .expect("the middle parent should reappear");
    assert!(middle.collapsed);
    assert_eq!(middle.hidden_descendants, vec![0]);

    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert!(app.all_filtered_task_threads_collapsed());
    assert!(app.expanded_task_threads.is_empty());
    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert!(app.expanded_task_threads.contains("task-thread-3"));
    assert!(app.expanded_task_threads.contains("task-thread-5"));
}

#[test]
fn collapse_all_toggle_only_changes_parents_in_the_current_filter() {
    let mut app = interaction_test_app(6, 1);
    set_task_parent(&mut app, 0, 2);
    set_task_parent(&mut app, 3, 5);
    set_task_metadata(&mut app, 0, "visible child", Some("subagent"));
    set_task_metadata(&mut app, 2, "visible parent", Some("desktop"));
    set_task_metadata(&mut app, 3, "hidden child", Some("subagent"));
    set_task_metadata(&mut app, 5, "hidden parent", Some("desktop"));
    app.task_list_mode = TaskListMode::Tree;
    app.expanded_task_threads
        .insert("task-thread-5".to_string());
    app.task_search = "visible".to_string();
    app.reconcile_task_filter(true);

    assert_eq!(
        app.filtered_collapsible_task_threads(),
        vec!["task-thread-2"]
    );
    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert!(app.expanded_task_threads.contains("task-thread-2"));
    assert!(app.expanded_task_threads.contains("task-thread-5"));

    handle_key_event(&mut app, key_event(KeyCode::Char('E')));
    assert!(!app.expanded_task_threads.contains("task-thread-2"));
    assert!(app.expanded_task_threads.contains("task-thread-5"));
}

#[test]
fn tree_control_is_fully_clickable_stable_and_muted_while_searching() {
    for (width, expected_width) in [(60, 3), (120, 7)] {
        let mut app = interaction_test_app(8, 1);
        set_task_parent(&mut app, 0, 4);
        app.turns_default_visible = false;
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let controls = app.task_controls_hitbox.unwrap();
        let initial = controls.toggle_tree;
        let initial_open = controls.open_terminal;
        let initial_collapse = controls.collapse_all;
        assert_eq!(initial.width, expected_width);
        assert_eq!(initial_collapse.width, if width == 60 { 3 } else { 11 });
        assert!(controls.enter_turns.right() <= initial_open.x);
        assert!(initial_open.right() <= initial.x);
        assert!(initial.right() <= initial_collapse.x);
        assert!(initial_collapse.right() <= controls.sources[0].x);
        assert_eq!(
            terminal.backend().buffer()[(initial.x + 1, initial.y)].symbol(),
            "R"
        );

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                initial.right() - 1,
                initial.y,
            ),
        ));
        assert_eq!(app.task_list_mode, TaskListMode::Tree);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let selected = app.task_controls_hitbox.unwrap().toggle_tree;
        let collapse = app.task_controls_hitbox.unwrap().collapse_all;
        assert_eq!(selected, initial);
        assert_eq!(collapse, initial_collapse);
        assert!(
            terminal.backend().buffer()[(selected.x + 1, selected.y)]
                .modifier
                .contains(Modifier::UNDERLINED)
        );
        let collapse_shortcut = &terminal.backend().buffer()[(collapse.x + 1, collapse.y)];
        assert_eq!(collapse_shortcut.fg, app.theme.palette().accent);
        assert!(collapse_shortcut.modifier.contains(Modifier::BOLD));
        assert!(!app.expanded_task_threads.contains("task-thread-4"));
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                collapse.right() - 1,
                collapse.y,
            ),
        ));
        assert!(app.expanded_task_threads.contains("task-thread-4"));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let collapse_control = app.task_controls_hitbox.unwrap().collapse_all;
        assert_eq!(collapse_control, initial_collapse);
        if width == 120 {
            let rendered = (collapse_control.x..collapse_control.right())
                .map(|x| {
                    terminal.backend().buffer()[(x, collapse_control.y)]
                        .symbol()
                        .to_string()
                })
                .collect::<String>();
            assert_eq!(rendered, "[E]Collapse");
        }
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                collapse_control.right() - 1,
                collapse_control.y,
            ),
        ));
        assert!(!app.expanded_task_threads.contains("task-thread-4"));

        handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                selected.right() - 1,
                selected.y,
            ),
        );
        app.begin_task_search();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let searching = app.task_controls_hitbox.unwrap().toggle_tree;
        let searching_collapse = app.task_controls_hitbox.unwrap().collapse_all;
        let shortcut = &terminal.backend().buffer()[(searching.x + 1, searching.y)];
        assert_eq!(searching, initial);
        assert_eq!(searching_collapse, initial_collapse);
        assert_eq!(shortcut.fg, app.theme.palette().muted);
        assert!(!shortcut.modifier.contains(Modifier::BOLD));
        assert!(!shortcut.modifier.contains(Modifier::UNDERLINED));
        let collapse_shortcut =
            &terminal.backend().buffer()[(searching_collapse.x + 1, searching_collapse.y)];
        assert_eq!(collapse_shortcut.fg, app.theme.palette().muted);
        assert!(!collapse_shortcut.modifier.contains(Modifier::BOLD));
    }
}

#[test]
fn filtered_task_navigation_clicks_and_scroll_use_visible_positions() {
    let mut app = interaction_test_app(30, 1);
    for index in 0..app.snapshot.tasks.len() {
        app.snapshot.tasks[index].source = Some(if index % 3 == 1 {
            "cli".to_string()
        } else {
            "desktop".to_string()
        });
    }
    app.set_task_source_filter(TaskSourceFilter::Cli);
    let filtered = app.filtered_task_indices();
    assert_eq!(filtered, vec![1, 4, 7, 10, 13, 16, 19, 22, 25, 28]);
    assert_eq!(app.selected_task, 1);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let hitbox = app.task_table_hitbox.expect("filtered rows should render");
    assert!(hitbox.capacity < filtered.len());
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            hitbox.viewport.x,
            hitbox.viewport.y,
        ),
    ));
    assert_eq!(app.task_table_offset, MOUSE_SCROLL_LINES);
    assert_eq!(app.selected_task, 1);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let scrolled = app.task_table_hitbox.expect("filtered rows should remain");
    let expected = filtered[scrolled.offset];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            scrolled.rows.x,
            scrolled.rows.y,
        ),
    ));
    assert_eq!(app.selected_task, expected);

    handle_key_event(&mut app, key_event(KeyCode::Down));
    assert_eq!(app.selected_task, filtered[scrolled.offset + 1]);
    handle_key_event(&mut app, key_event(KeyCode::Up));
    assert_eq!(app.selected_task, expected);
}

#[test]
fn tree_click_scroll_and_refresh_keep_flattened_positions_mapped_by_thread() {
    let mut app = interaction_test_app(30, 1);
    set_task_parent(&mut app, 0, 10);
    set_task_parent(&mut app, 1, 10);
    expand_task_tree(&mut app);
    assert_eq!(&app.filtered_task_indices()[..4], &[10, 0, 1, 2]);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = app.task_table_hitbox.unwrap().rows;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), rows.x, rows.y + 1,),
    ));
    assert_eq!(app.selected_task, 0);

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::ScrollDown, rows.x, rows.y),
    ));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let hitbox = app.task_table_hitbox.unwrap();
    let flattened = app.filtered_task_indices();
    let expected_clicked = flattened[hitbox.offset];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.y,
        ),
    ));
    assert_eq!(app.selected_task, expected_clicked);

    let viewport_thread_id = app.snapshot.tasks[flattened[hitbox.offset]]
        .thread_id
        .clone();
    let mut snapshot = app.snapshot.clone();
    let mut newest_child = snapshot.tasks[0].clone();
    newest_child.thread_id = "newest-tree-child".to_string();
    newest_child.parent_thread_id = Some("task-thread-10".to_string());
    snapshot.tasks.insert(0, newest_child);
    app.replace(
        CollectionResult {
            snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );

    let flattened = app.filtered_task_indices();
    assert_eq!(
        app.snapshot.tasks[flattened[app.task_table_offset]].thread_id,
        viewport_thread_id
    );
    assert_eq!(
        app.raw_selected_thread_id(),
        Some(viewport_thread_id.as_str())
    );
}

#[test]
fn source_shortcuts_cycle_and_reselecting_active_button_keeps_viewport() {
    let mut app = interaction_test_app(30, 1);
    for index in 0..app.snapshot.tasks.len() {
        app.snapshot.tasks[index].source = Some(
            match index % 3 {
                0 => "desktop",
                1 => "subagent",
                _ => "cli",
            }
            .to_string(),
        );
    }
    handle_key_event(&mut app, key_event(KeyCode::Char(']')));
    assert_eq!(app.task_source_filter, TaskSourceFilter::Desktop);
    handle_key_event(&mut app, key_event(KeyCode::Char(']')));
    assert_eq!(app.task_source_filter, TaskSourceFilter::Subagent);
    handle_key_event(&mut app, key_event(KeyCode::Char('[')));
    assert_eq!(app.task_source_filter, TaskSourceFilter::Desktop);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let task_hitbox = app.task_table_hitbox.unwrap();
    handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            task_hitbox.viewport.x,
            task_hitbox.viewport.y,
        ),
    );
    let scrolled_offset = app.task_table_offset;
    assert!(scrolled_offset > 0);
    let desktop = app.task_controls_hitbox.unwrap().sources[TaskSourceFilter::Desktop.index()];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            desktop.x,
            desktop.y,
        ),
    ));
    assert_eq!(app.task_table_offset, scrolled_offset);
    assert_eq!(app.focus, Focus::Tasks);
}

#[test]
fn shortcut_labels_highlight_real_bindings_and_direct_source_keys() {
    let mut app = interaction_test_app(8, 1);
    for (index, source) in [
        "desktop", "subagent", "cli", "desktop", "subagent", "cli", "desktop", "subagent",
    ]
    .into_iter()
    .enumerate()
    {
        app.snapshot.tasks[index].source = Some(source.to_string());
    }
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let palette = app.theme.palette();
    let buffer = terminal.backend().buffer();
    let controls = app.task_controls_hitbox.unwrap();
    assert_eq!(buffer[(controls.search.x, controls.search.y)].symbol(), "F");
    assert_eq!(
        buffer[(controls.search.x, controls.search.y)].fg,
        palette.accent
    );

    for filter in TaskSourceFilter::ALL {
        let button = controls.sources[filter.index()];
        let shortcut = &buffer[(button.x + 1, button.y)];
        assert_eq!(shortcut.symbol(), filter.shortcut().to_string());
        if filter == TaskSourceFilter::All {
            assert!(shortcut.modifier.contains(Modifier::UNDERLINED));
        } else {
            assert_eq!(shortcut.fg, palette.accent);
            assert!(shortcut.modifier.contains(Modifier::BOLD));
        }
    }

    let tabs = app.view_tabs_hitbox.unwrap();
    for view in View::ALL {
        let tab = tabs.tabs[view.index()];
        let shortcut = &buffer[(tab.x + 1, tab.y)];
        assert_eq!(shortcut.symbol(), view.shortcut().to_string());
        assert_eq!(shortcut.fg, palette.accent);
    }

    for (key, expected) in [
        ('d', TaskSourceFilter::Desktop),
        ('S', TaskSourceFilter::Subagent),
        ('c', TaskSourceFilter::Cli),
        ('A', TaskSourceFilter::All),
    ] {
        handle_key_event(&mut app, key_event(KeyCode::Char(key)));
        assert_eq!(app.task_source_filter, expected);
    }

    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    handle_key_event(&mut app, key_event(KeyCode::Char('d')));
    assert_eq!(app.focus, Focus::TaskSearch);
    assert_eq!(app.task_search, "d");
    assert_eq!(app.task_source_filter, TaskSourceFilter::All);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let controls = app.task_controls_hitbox.unwrap();
    assert_eq!(
        buffer[(controls.search.x, controls.search.y)].fg,
        palette.title
    );
    let all = controls.sources[TaskSourceFilter::All.index()];
    assert!(
        !buffer[(all.x + 1, all.y)]
            .modifier
            .contains(Modifier::UNDERLINED)
    );
    let desktop = controls.sources[TaskSourceFilter::Desktop.index()];
    assert_eq!(buffer[(desktop.x + 1, desktop.y)].fg, palette.muted);
    let overview = app.view_tabs_hitbox.unwrap().tabs[View::Overview.index()];
    assert_eq!(buffer[(overview.x + 1, overview.y)].fg, palette.muted);
}

#[test]
fn window_scope_shortcuts_and_mouse_switch_reset_cycle_data() {
    let mut app = interaction_test_app(3, 2);
    // Keep the scope-specific share/quota columns visible in the split Overview
    // layout; responsive defaults prioritize API EQ. at this width.
    app.table_columns.api_equivalent = false;
    add_window_analysis(&mut app, WindowScope::FiveHours, 111, 11.0);
    add_window_analysis(&mut app, WindowScope::Week, 777, 63.0);
    app.view = View::Overview;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let controls = app
        .window_controls_hitbox
        .expect("window controls should render");
    let palette = app.theme.palette();
    let buffer = terminal.backend().buffer();
    for scope in WindowScope::ALL {
        let button = controls.scopes[scope.index()];
        let shortcut = &buffer[(button.x + 1, button.y)];
        assert_eq!(shortcut.symbol(), scope.shortcut().to_string());
        if scope == WindowScope::FiveHours {
            assert!(shortcut.modifier.contains(Modifier::UNDERLINED));
        } else {
            assert_eq!(shortcut.fg, palette.accent);
            assert!(shortcut.modifier.contains(Modifier::BOLD));
        }
    }

    handle_key_event(&mut app, key_event(KeyCode::Char('W')));
    assert_eq!(app.window_scope, WindowScope::Week);
    assert_eq!(WindowScope::FiveHours.token_share_header(), "TOKEN5H%");
    assert_eq!(WindowScope::Week.token_share_header(), "TOKENWK%");
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("week reset cycle"));
    assert!(content.contains("Week-cycle tasks"));
    assert!(content.contains("TOKENWK%"));
    assert!(content.contains("777"));
    assert!(content.contains("63.0%"));
    assert!(content.contains("gpt-window"));

    let five_hours = app.window_controls_hitbox.unwrap().scopes[WindowScope::FiveHours.index()];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            five_hours.x,
            five_hours.y,
        ),
    ));
    assert_eq!(app.window_scope, WindowScope::FiveHours);

    handle_key_event(&mut app, key_event(KeyCode::Char('5')));
    assert_eq!(app.window_scope, WindowScope::FiveHours);
    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    handle_key_event(&mut app, key_event(KeyCode::Char('w')));
    assert_eq!(app.focus, Focus::TaskSearch);
    assert_eq!(app.task_search, "w");
    assert_eq!(app.window_scope, WindowScope::FiveHours);
}

#[test]
fn top_window_controls_keep_stable_compact_geometry() {
    for width in [8, 20, 44, 54, 64, 80, 120] {
        let mut app = interaction_test_app(1, 1);
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| {
                let controls = render_overview_controls(frame, frame.area(), &app);
                app.window_controls_hitbox = Some(controls);
            })
            .unwrap();
        let initial = app.window_controls_hitbox.unwrap();
        let tabs = view_tabs_hitbox(Rect::new(0, 0, width, 1));
        for button in [
            initial.toggle_turns,
            initial.toggle_models,
            initial.toggle_api_long_context,
        ]
        .into_iter()
        .chain(initial.scopes)
        {
            if !button.is_empty() {
                assert!(button.x >= tabs.rendered_right);
                assert!(button.right() <= width);
            }
        }

        app.turns_default_visible = false;
        app.models_visible = false;
        app.focus = Focus::TaskSearch;
        terminal
            .draw(|frame| {
                let controls = render_overview_controls(frame, frame.area(), &app);
                app.window_controls_hitbox = Some(controls);
            })
            .unwrap();
        assert_eq!(app.window_controls_hitbox.unwrap(), initial);

        if width >= 64 {
            assert!(!initial.toggle_turns.is_empty());
            assert!(!initial.toggle_models.is_empty());
            assert!(initial.scopes.iter().all(|button| !button.is_empty()));
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(initial.toggle_turns.x + 1, 0)].symbol(), "V");
            assert_eq!(buffer[(initial.toggle_models.x + 1, 0)].symbol(), "M");
            assert_eq!(buffer[(initial.scopes[0].x + 1, 0)].symbol(), "5");
            assert_eq!(buffer[(initial.scopes[1].x + 1, 0)].symbol(), "W");
        }
        if width >= 64 {
            assert!(!initial.toggle_api_long_context.is_empty());
            assert_eq!(
                terminal.backend().buffer()[(
                    initial.toggle_api_long_context.x + 1,
                    initial.toggle_api_long_context.y,
                )]
                    .symbol(),
                "L"
            );
        }
    }

    for width in 20..40 {
        let app = interaction_test_app(1, 1);
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        let mut controls = None;
        terminal
            .draw(|frame| controls = Some(render_overview_controls(frame, frame.area(), &app)))
            .unwrap();
        let controls_start = view_tabs_hitbox(Rect::new(0, 0, width, 1)).rendered_right;
        let rendered = (controls_start..width)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        let expected = match width.saturating_sub(controls_start) {
            0..=3 => "",
            4..=6 => " [V]",
            7..=9 => " [V][M]",
            10..=12 => " [V][M][5]",
            13..=15 => " [V][M][5][W]",
            _ => " [V][M][5][W][L]",
        };
        assert_eq!(rendered.trim_end(), expected, "width={width}");
        let controls = controls.unwrap();
        for (button, shortcut) in [
            (controls.toggle_turns, "V"),
            (controls.toggle_models, "M"),
            (controls.scopes[WindowScope::FiveHours.index()], "5"),
            (controls.scopes[WindowScope::Week.index()], "W"),
            (controls.toggle_api_long_context, "L"),
        ] {
            if !button.is_empty() {
                assert_eq!(button.width, 3);
                assert_eq!(
                    terminal.backend().buffer()[(button.x + 1, 0)].symbol(),
                    shortcut
                );
            }
        }
    }

    let mut app = interaction_test_app(1, 1);
    let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| {
            let controls = render_overview_controls(frame, frame.area(), &app);
            app.window_controls_hitbox = Some(controls);
        })
        .unwrap();
    let controls = app.window_controls_hitbox.unwrap();
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            controls.toggle_turns.right() - 1,
            controls.toggle_turns.y,
        ),
    ));
    assert!(!app.turns_default_visible);
    for (button, expected_scope) in [
        (
            controls.scopes[WindowScope::Week.index()],
            WindowScope::Week,
        ),
        (
            controls.scopes[WindowScope::FiveHours.index()],
            WindowScope::FiveHours,
        ),
    ] {
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                button.right() - 1,
                button.y,
            ),
        ));
        assert_eq!(app.window_scope, expected_scope);
    }
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            controls.toggle_models.right() - 1,
            controls.toggle_models.y,
        ),
    ));
    assert!(!app.models_visible);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            controls.toggle_api_long_context.right() - 1,
            controls.toggle_api_long_context.y,
        ),
    ));
    assert!(app.api_long_context_multiplier);
}

#[test]
fn api_long_context_toggle_uses_paired_estimates_and_keyboard_mouse_search_rules() {
    let mut app = interaction_test_app(1, 1);
    app.snapshot.turns[0].turn_id = "01a00b37-eb69-7f23-9c43-03cba436f012".to_string();
    app.snapshot.turns[0].message_preview = Some("visible message preview".to_string());
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    app.snapshot.api_pricing = crate::api_cost::pricing_metadata();
    let base = app.snapshot.window_analyses.first_mut().unwrap();
    base.api_pricing = crate::api_cost::pricing_metadata();
    base.api_equivalent_cost = ApiEquivalentCost {
        amount: exact_api_cost(1_234_500_000_000),
        partial_reasons: Vec::new(),
        model_breakdown: Vec::new(),
    };
    base.models[0].api_equivalent_cost = exact_api_cost(250_000_000_000);
    let mut partial_turn_cost = exact_api_cost(125_000_000_000);
    partial_turn_cost.maximum_pico_usd = PicoUsd::new(250_000_000_000);
    partial_turn_cost.observed_samples = 2;
    partial_turn_cost.observed_tokens = 200;
    base.turns[0].usage.api_equivalent_cost = partial_turn_cost;
    let mut api = base.clone();
    api.api_long_context = None;
    api.threads[0].usage.estimated_quota_percent = 17.0;
    api.turns[0].usage.estimated_quota_percent = 17.0;
    api.models[0].estimated_quota_percent = 17.0;
    api.api_equivalent_cost.amount = exact_api_cost(9_000_000_000_000);
    api.models[0].api_equivalent_cost = exact_api_cost(9_000_000_000_000);
    api.turns[0].usage.api_equivalent_cost = exact_api_cost(9_000_000_000_000);
    base.api_long_context = Some(Box::new(api));

    assert!(!app.api_long_context_multiplier);
    assert_eq!(
        task_usage_for_scope_with_api_long_context(
            &app.snapshot,
            WindowScope::FiveHours,
            &app.snapshot.tasks[0],
            false,
        )
        .estimated_quota_percent,
        23.0
    );
    assert_eq!(
        task_usage_for_scope_with_api_long_context(
            &app.snapshot,
            WindowScope::FiveHours,
            &app.snapshot.tasks[0],
            true,
        )
        .estimated_quota_percent,
        17.0
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let initial_content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(initial_content.contains("[L]EST Longx"));
    assert!(initial_content.contains("API equivalent $1.2345"));
    assert!(initial_content.contains("$0.2500"));
    assert!(initial_content.contains("share="));
    assert!(initial_content.contains(" · est="));
    assert!(initial_content.contains("api[5h]=$0.1250–$0.2500+ · cov=50.0%"));
    assert!(initial_content.contains("turn=01a00b37… · message=visible message preview"));
    let toggle = app.window_controls_hitbox.unwrap().toggle_api_long_context;
    let shortcut = &terminal.backend().buffer()[(toggle.x + 1, toggle.y)];
    assert_eq!(shortcut.symbol(), "L");
    assert_eq!(shortcut.fg, app.theme.palette().accent);

    handle_key_event(&mut app, key_event(KeyCode::Char('L')));
    assert!(app.api_long_context_multiplier);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("Models · 5h · EST Longx ON"));
    assert!(content.contains("API equivalent $1.2345"));
    assert!(content.contains("$0.2500"));
    assert!(content.contains("share="));
    assert!(content.contains(" · est="));
    assert!(content.contains("api[5h]=$0.1250–$0.2500+ · cov=50.0%"));
    assert!(!content.contains("$9.0000"));
    let toggle = app.window_controls_hitbox.unwrap().toggle_api_long_context;
    assert!(
        terminal.backend().buffer()[(toggle.x + 1, toggle.y)]
            .modifier
            .contains(Modifier::UNDERLINED)
    );

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            toggle.right() - 1,
            toggle.y,
        ),
    ));
    assert!(!app.api_long_context_multiplier);

    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    handle_key_event(&mut app, key_event(KeyCode::Char('l')));
    assert_eq!(app.task_search, "l");
    assert!(!app.api_long_context_multiplier);
    handle_key_event(&mut app, key_event(KeyCode::Esc));

    app.set_view(View::Trends);
    handle_key_event(&mut app, key_event(KeyCode::Char('L')));
    assert!(!app.api_long_context_multiplier);

    let mut compact = Terminal::new(TestBackend::new(60, 24)).unwrap();
    app.set_view(View::Overview);
    compact.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(
        !app.window_controls_hitbox
            .unwrap()
            .toggle_api_long_context
            .is_empty()
    );
}

#[test]
fn overview_shows_independent_task_and_turn_api_costs_for_the_selected_scope() {
    let mut app = interaction_test_app(1, 2);
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    set_window_entity_api_costs(
        &mut app,
        WindowScope::FiveHours,
        300_000_000_000,
        &[100_000_000_000, 200_000_000_000],
    );
    add_window_analysis(&mut app, WindowScope::Week, 400, 100.0);
    set_window_entity_api_costs(
        &mut app,
        WindowScope::Week,
        900_000_000_000,
        &[400_000_000_000, 500_000_000_000],
    );
    app.models_visible = false;

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let five_hours = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(five_hours.matches("API EQ.").count() >= 2);
    assert!(five_hours.contains("$0.3000"));
    assert!(five_hours.contains("$0.1000"));
    assert!(five_hours.contains("$0.2000"));

    app.toggle_api_long_context_multiplier();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let longx = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(longx.contains("$0.3000"));
    assert!(longx.contains("$0.1000"));
    assert!(longx.contains("$0.2000"));

    app.set_window_scope(WindowScope::Week);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let week = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(week.contains("$0.9000"));
    assert!(week.contains("$0.4000"));
    assert!(week.contains("$0.5000"));
    assert!(!week.contains("$0.3000"));

    app.table_columns.api_equivalent = false;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let hidden = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!hidden.contains("API EQ."));
}

#[test]
fn api_cost_columns_distinguish_an_unavailable_window_from_observed_zero() {
    let mut app = interaction_test_app(1, 1);
    app.models_visible = false;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let unavailable = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(unavailable.matches("API EQ.").count() >= 2);
    assert!(!unavailable.contains("$0.0000"));

    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let observed_zero = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(observed_zero.contains("$0.0000"));
}

#[test]
fn overview_maps_api_costs_to_each_thread_and_its_own_turns() {
    let mut app = interaction_test_app(2, 2);
    app.models_visible = false;
    app.snapshot.tasks[0].cwd = Some("/tmp/alpha".into());
    app.snapshot.tasks[1].cwd = Some("/tmp/beta".into());
    for (turn, message) in app
        .snapshot
        .turns
        .iter_mut()
        .zip(["m00", "m01", "m10", "m11"])
    {
        turn.message_preview = Some(message.to_string());
    }
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);

    let analysis = app.snapshot.window_analyses.last_mut().unwrap();
    let template = analysis.threads[0].usage;
    analysis.threads = app
        .snapshot
        .tasks
        .iter()
        .zip([300_000_000_000, 700_000_000_000])
        .map(|(task, amount)| ThreadWindowUsage {
            thread_id: task.thread_id.clone(),
            usage: WindowUsage {
                api_equivalent_cost: exact_api_cost(amount),
                ..template
            },
        })
        .collect();
    analysis.turns = app
        .snapshot
        .turns
        .iter()
        .zip([
            100_000_000_000,
            200_000_000_000,
            400_000_000_000,
            500_000_000_000,
        ])
        .map(|(turn, amount)| TurnWindowUsage {
            thread_id: turn.thread_id.clone(),
            turn_id: turn.turn_id.clone(),
            usage: WindowUsage {
                api_equivalent_cost: exact_api_cost(amount),
                ..template
            },
        })
        .collect();

    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = (0..40)
        .map(|y| buffer_rect_row_text(terminal.backend().buffer(), Rect::new(0, 0, 160, 40), y))
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| row.contains("alpha") && row.contains("$0.3000"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("beta") && row.contains("$0.7000"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("m00") && row.contains("$0.1000"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("m01") && row.contains("$0.2000"))
    );

    app.selected_task = 1;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = (0..40)
        .map(|y| buffer_rect_row_text(terminal.backend().buffer(), Rect::new(0, 0, 160, 40), y))
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| row.contains("m10") && row.contains("$0.4000"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("m11") && row.contains("$0.5000"))
    );
}

#[test]
fn api_cost_rows_mark_incomplete_local_coverage_as_a_lower_bound() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    let analysis = app.snapshot.window_analyses.last_mut().unwrap();
    analysis.partial = true;
    analysis.partial_reasons = vec!["rollout_scan_incomplete".to_string()];
    analysis.api_equivalent_cost.amount = exact_api_cost(500_000_000_000);
    analysis.threads[0].usage.api_equivalent_cost = exact_api_cost(300_000_000_000);
    analysis.turns[0].usage.api_equivalent_cost = ApiCostAmount::default();
    analysis.models[0].api_equivalent_cost = exact_api_cost(200_000_000_000);

    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let incomplete = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(incomplete.contains("$0.3000+"));
    assert!(incomplete.contains("$0.0000+"));
    assert!(incomplete.contains("$0.2000+"));

    let analysis = app.snapshot.window_analyses.last_mut().unwrap();
    analysis.partial_reasons = vec!["local_scan_disabled".to_string()];
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let disabled = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!disabled.contains("$0.3000"));
    assert!(!disabled.contains("$0.2000"));
}

#[test]
fn api_cost_columns_keep_large_range_and_lower_bound_markers_visible() {
    let mut app = interaction_test_app(1, 1);
    app.snapshot.tasks[0].cwd = Some("/tmp/alpha".into());
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    let analysis = app.snapshot.window_analyses.last_mut().unwrap();
    analysis.api_equivalent_cost.amount = ranged_api_cost(7_000, 8_000);
    analysis.threads[0].usage.api_equivalent_cost = ranged_api_cost(1_000, 2_000);
    analysis.turns[0].usage.api_equivalent_cost = ranged_api_cost(3_000, 4_000);
    analysis.models[0].api_equivalent_cost = ranged_api_cost(5_000, 6_000);

    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = (0..40)
        .map(|y| buffer_rect_row_text(terminal.backend().buffer(), Rect::new(0, 0, 160, 40), y))
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| { row.contains("alpha") && row.contains("$1000.00–$2000.00+") })
    );
    assert!(
        rows.iter()
            .any(|row| { row.contains("model-0") && row.contains("$3000.00–$4000.00+") })
    );
    assert!(
        rows.iter()
            .any(|row| { row.contains("gpt-window") && row.contains("$5000.00–$6000.00+") })
    );
}

#[test]
fn api_cost_columns_keep_precision_breakpoint_values_visible() {
    const ONE_THOUSAND_DOLLARS: u128 = 1_000_000_000_000_000;
    const JUST_BELOW_ONE_THOUSAND_DOLLARS: u128 = 999_999_900_000_000;

    let mut app = interaction_test_app(1, 1);
    app.snapshot.tasks[0].cwd = Some("/tmp/alpha".into());
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    let analysis = app.snapshot.window_analyses.last_mut().unwrap();
    analysis.api_equivalent_cost.amount = exact_api_cost(ONE_THOUSAND_DOLLARS);
    analysis.threads[0].usage.api_equivalent_cost = exact_api_cost(JUST_BELOW_ONE_THOUSAND_DOLLARS);
    analysis.turns[0].usage.api_equivalent_cost = exact_api_cost(JUST_BELOW_ONE_THOUSAND_DOLLARS);
    analysis.models[0].api_equivalent_cost = exact_api_cost(JUST_BELOW_ONE_THOUSAND_DOLLARS);

    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = (0..40)
        .map(|y| buffer_rect_row_text(terminal.backend().buffer(), Rect::new(0, 0, 160, 40), y))
        .collect::<Vec<_>>();
    for identity in ["alpha", "model-0", "gpt-window"] {
        assert!(
            rows.iter()
                .any(|row| row.contains(identity) && row.contains("$999.9999")),
            "missing untruncated precision-boundary amount for {identity}"
        );
    }
}

#[test]
fn table_column_preferences_apply_to_tasks_turns_and_models() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    let mut terminal = Terminal::new(TestBackend::new(200, 40)).unwrap();

    let render_content = |app: &mut App, terminal: &mut Terminal<TestBackend>| {
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    let visible = render_content(&mut app, &mut terminal);
    for header in [
        "TOKENS",
        "TOKEN5H%",
        "TOKEN%",
        "TOKEN SHARE",
        "EST.Q5H",
        "EST.Q",
        "EST. QUOTA",
        "API EQ.",
    ] {
        assert!(visible.contains(header), "missing {header}");
    }

    app.table_columns.tokens = false;
    let without_tokens = render_content(&mut app, &mut terminal);
    assert!(!without_tokens.contains("TOKENS"));
    app.table_columns.tokens = true;

    app.table_columns.token_share = false;
    let without_share = render_content(&mut app, &mut terminal);
    for header in ["TOKEN5H%", "TOKEN%", "TOKEN SHARE"] {
        assert!(!without_share.contains(header), "unexpected {header}");
    }
    app.table_columns.token_share = true;

    app.table_columns.estimated_quota = false;
    let without_estimate = render_content(&mut app, &mut terminal);
    for header in ["EST.Q5H", "EST.Q", "EST. QUOTA"] {
        assert!(!without_estimate.contains(header), "unexpected {header}");
    }
    app.table_columns.estimated_quota = true;

    app.table_columns.api_equivalent = false;
    let without_api = render_content(&mut app, &mut terminal);
    assert!(!without_api.contains("API EQ."));
}

#[test]
fn hidden_tokens_column_keeps_task_and_turn_status_markers_in_identity_columns() {
    for (width, height) in [(120, 40), (60, 24)] {
        let mut app = interaction_test_app(3, 3);
        app.models_visible = false;
        app.table_columns.tokens = false;

        for (task, (status, project)) in app.snapshot.tasks.iter_mut().zip([
            (TaskStatus::Running, "run-task"),
            (TaskStatus::Completed, "done-task"),
            (TaskStatus::Interrupted, "stop-task"),
        ]) {
            task.status = status;
            task.cwd = Some(format!("/tmp/{project}").into());
        }
        for (turn, (status, message)) in app.snapshot.turns.iter_mut().take(3).zip([
            (TurnStatus::InProgress, "run"),
            (TurnStatus::Completed, "done"),
            (TurnStatus::Interrupted, "stop"),
        ]) {
            turn.status = status;
            turn.message_preview = Some(message.to_string());
        }

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let task_rows = app.task_table_hitbox.expect("task rows should render").rows;
        let turn_rows = app.turn_table_hitbox.expect("turn rows should render").rows;
        assert!(task_rows.height >= 3, "{width}x{height} task rows");
        assert!(turn_rows.height >= 3, "{width}x{height} turn rows");

        let buffer = terminal.backend().buffer();
        let row_texts = |rows: Rect| {
            (0..3)
                .map(|offset| {
                    (rows.x..rows.right())
                        .map(|x| buffer[(x, rows.y + offset)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let header_text = |rows: Rect| {
            (rows.x..rows.right())
                .map(|x| buffer[(x, rows.y.saturating_sub(1))].symbol())
                .collect::<String>()
        };
        let task_lines = row_texts(task_rows);
        let turn_lines = row_texts(turn_rows);

        assert!(!header_text(task_rows).contains("TOKENS"));
        assert!(!header_text(turn_rows).contains("TOKENS"));
        for expected in ["R run-task", "D done-task", "X stop-task"] {
            assert!(
                task_lines.iter().any(|line| line.contains(expected)),
                "{width}x{height} missing {expected:?} from task identity rows: {task_lines:?}"
            );
        }
        for expected in ["R run", "D done", "X stop"] {
            assert!(
                turn_lines.iter().any(|line| line.contains(expected)),
                "{width}x{height} missing {expected:?} from turn identity rows: {turn_lines:?}"
            );
        }
    }
}

#[test]
fn models_panel_visibility_uses_keyboard_mouse_and_search_priority() {
    let mut app = interaction_test_app(3, 2);
    add_window_analysis(&mut app, WindowScope::FiveHours, 111, 11.0);
    add_window_analysis(&mut app, WindowScope::Week, 777, 63.0);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let visible_capacity = app.task_table_hitbox.unwrap().capacity;
    let model_button = app.window_controls_hitbox.unwrap().toggle_models;
    let shortcut = &terminal.backend().buffer()[(model_button.x + 1, model_button.y)];
    assert_eq!(shortcut.symbol(), "M");
    assert!(shortcut.modifier.contains(Modifier::UNDERLINED));
    assert!(app.window_controls_hitbox.is_some());

    handle_key_event(&mut app, key_event(KeyCode::Char('M')));
    assert!(!app.models_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let hidden_button = app.window_controls_hitbox.unwrap().toggle_models;
    assert_eq!(hidden_button, model_button);
    let hidden_shortcut = &terminal.backend().buffer()[(hidden_button.x + 1, hidden_button.y)];
    assert_eq!(hidden_shortcut.fg, app.theme.palette().accent);
    assert!(hidden_shortcut.modifier.contains(Modifier::BOLD));
    assert!(!hidden_shortcut.modifier.contains(Modifier::UNDERLINED));
    assert!(app.window_controls_hitbox.is_some());
    assert!(app.task_table_hitbox.unwrap().capacity > visible_capacity);
    let hidden = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!hidden.contains("Attribution"));
    assert!(!hidden.contains("gpt-window"));

    let week = app.window_controls_hitbox.unwrap().scopes[WindowScope::Week.index()];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            week.right() - 1,
            week.y,
        ),
    ));
    assert_eq!(app.window_scope, WindowScope::Week);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let model_button = app.window_controls_hitbox.unwrap().toggle_models;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            model_button.right() - 1,
            model_button.y,
        ),
    ));
    assert!(app.models_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.window_controls_hitbox.is_some());
    let visible_week_button = app.window_controls_hitbox.unwrap().toggle_models;
    let visible = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(visible.contains("Attribution"));

    app.begin_task_search();
    handle_key_event(&mut app, key_event(KeyCode::Char('m')));
    assert_eq!(app.task_search, "m");
    assert!(app.models_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let searching = app.window_controls_hitbox.unwrap().toggle_models;
    let searching_shortcut = &terminal.backend().buffer()[(searching.x + 1, searching.y)];
    let searching_bracket = &terminal.backend().buffer()[(searching.x, searching.y)];
    assert_eq!(searching, visible_week_button);
    assert_eq!(searching_shortcut.fg, searching_bracket.fg);
    assert_eq!(searching_shortcut.bg, searching_bracket.bg);
    assert_eq!(searching_shortcut.modifier, searching_bracket.modifier);
    assert!(!searching_shortcut.modifier.contains(Modifier::UNDERLINED));
    app.cancel_task_search();

    app.set_view(View::Health);
    handle_key_event(&mut app, key_event(KeyCode::Char('M')));
    assert!(app.models_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.window_controls_hitbox.is_none());
}

#[test]
fn missing_selected_reset_cycle_is_explicitly_unavailable() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
    app.view = View::Overview;
    app.window_scope = WindowScope::Week;
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("Week reset cycle unavailable"));
    assert!(content.contains("Models · Week unavailable"));
    assert!(!content.contains("No token usage in the current Week window"));
}

#[test]
fn window_partial_marker_follows_the_selected_scope() {
    let mut app = interaction_test_app(1, 1);
    add_window_analysis(&mut app, WindowScope::FiveHours, 100, 40.0);
    add_window_analysis(&mut app, WindowScope::Week, 250, 100.0);
    let weekly = app
        .snapshot
        .window_analyses
        .iter_mut()
        .find(|analysis| analysis.duration_mins == WindowScope::Week.duration_mins())
        .unwrap();
    weekly.partial = true;
    weekly
        .partial_reasons
        .push("rollout_lookback_incomplete".to_string());
    app.snapshot.partial = true;
    app.view = View::Overview;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let five_hour = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!five_hour.contains(" · partial"));

    app.window_scope = WindowScope::Week;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let weekly = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(weekly.contains(" · partial"));
    assert!(weekly.contains("~23.00pp estimated"));
    assert!(weekly.contains("~23.0%"));
}

#[test]
fn source_buttons_search_hitbox_and_empty_results_are_safe() {
    for (width, height) in [(60, 24), (80, 24), (100, 30), (120, 40)] {
        let mut app = interaction_test_app(8, 2);
        for (index, source) in [
            "desktop", "subagent", "cli", "desktop", "subagent", "cli", "desktop", "subagent",
        ]
        .into_iter()
        .enumerate()
        {
            app.snapshot.tasks[index].source = Some(source.to_string());
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let controls = app
            .task_controls_hitbox
            .expect("task controls should render");
        assert!(controls.sources.iter().all(|area| !area.is_empty()));
        assert!(!controls.search.is_empty());
        for pair in controls.sources.windows(2) {
            assert!(pair[0].right() <= pair[1].x);
        }
        assert!(controls.sources[3].right() <= controls.search.x);

        let subagent = controls.sources[TaskSourceFilter::Subagent.index()];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                subagent.x,
                subagent.y,
            ),
        ));
        assert_eq!(app.task_source_filter, TaskSourceFilter::Subagent);
        assert!(
            app.filtered_task_indices()
                .iter()
                .all(|index| app.snapshot.tasks[*index].source.as_deref() == Some("subagent"))
        );

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let controls = app.task_controls_hitbox.unwrap();
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                controls.search.x,
                controls.search.y,
            ),
        ));
        assert_eq!(app.focus, Focus::TaskSearch);
        for character in "no such task".chars() {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }
        assert!(app.filtered_task_indices().is_empty());
        assert_eq!(app.selected_thread_id(), None);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.task_table_hitbox.is_none());
        assert!(app.turn_table_hitbox.is_none());
        let clear = app.task_controls_hitbox.unwrap().clear_search;
        assert!(!clear.is_empty());
        let clear_shortcut = &terminal.backend().buffer()[(clear.x + 1, clear.y)];
        assert_eq!(clear_shortcut.fg, app.theme.palette().muted);
        assert!(!clear_shortcut.modifier.contains(Modifier::BOLD));
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                clear.right() - 1,
                clear.y,
            ),
        ));
        assert!(app.task_search.is_empty());
        assert!(!app.filtered_task_indices().is_empty());

        app.view = View::Health;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.task_controls_hitbox.is_none());
    }
}

#[test]
fn window_task_controls_compact_before_filter_hitboxes_are_clipped() {
    for scope in WindowScope::ALL {
        for query in ["", "task"] {
            let mut app = interaction_test_app(8, 2);
            app.view = View::Overview;
            app.window_scope = scope;
            app.task_search = query.to_string();
            app.task_search_before_edit = app.task_search.clone();
            app.task_search_cursor = grapheme_count(&app.task_search);
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();

            let controls = app.task_controls_hitbox.unwrap();
            assert!(controls.enter_turns.right() <= controls.toggle_tree.x);
            assert!(controls.toggle_tree.right() <= controls.collapse_all.x);
            assert!(controls.collapse_all.right() <= controls.sources[0].x);
            for pair in controls.sources.windows(2) {
                assert!(pair[0].right() <= pair[1].x);
            }
            assert!(controls.sources[3].right() <= controls.search.x);
            assert!(controls.sources.iter().all(|button| !button.is_empty()));
            assert!(!controls.search.is_empty());

            let buffer = terminal.backend().buffer();
            for filter in TaskSourceFilter::ALL {
                let button = controls.sources[filter.index()];
                assert_eq!(
                    buffer[(button.x + 1, button.y)].symbol(),
                    filter.shortcut().to_string()
                );
            }
            assert_eq!(buffer[(controls.search.x, controls.search.y)].symbol(), "F");
            if query.is_empty() {
                assert!(controls.clear_search.is_empty());
            } else {
                assert!(!controls.clear_search.is_empty());
                assert_eq!(controls.clear_search.width, 5);
                assert_eq!(
                    buffer[(controls.clear_search.x, controls.clear_search.y)].symbol(),
                    "["
                );
                assert_eq!(
                    buffer[(controls.clear_search.x + 1, controls.clear_search.y)].symbol(),
                    "D"
                );
                let shortcut = &buffer[(controls.clear_search.x + 1, controls.clear_search.y)];
                assert_eq!(shortcut.fg, app.theme.palette().accent);
                assert!(shortcut.modifier.contains(Modifier::BOLD));
                assert!(controls.search.right() <= controls.clear_search.x);
            }
        }
    }
}

#[test]
fn filter_clear_controls_reserve_query_space_across_layout_boundaries() {
    let mut app = interaction_test_app(8, 4);
    app.task_search = "task".to_string();
    app.task_search_before_edit.clone_from(&app.task_search);
    app.turn_search = "model".to_string();
    app.turn_search_before_edit.clone_from(&app.turn_search);

    for scope in WindowScope::ALL {
        app.window_scope = scope;
        for width in 60..=140 {
            let (_, controls) = task_panel_block(
                Rect::new(0, 0, width, 10),
                &app,
                true,
                app.filtered_task_indices().len(),
            );
            assert_eq!(controls.clear_search.width, 5, "task width={width}");
            let query_start = controls.search.x.saturating_add("Filter:".len() as u16);
            assert!(
                controls.clear_search.x.saturating_sub(query_start)
                    >= FILTER_CLEAR_GAP_WIDTH + FILTER_MIN_QUERY_WIDTH,
                "task width={width} scope={scope:?}"
            );
        }

        let title = match scope {
            WindowScope::FiveHours => "Turns · 5h cycle",
            WindowScope::Week => "Turns · Week cycle",
        };
        for width in 40..=80 {
            let (_, controls) = turn_panel_block(Rect::new(0, 0, width, 10), &app, title);
            assert_eq!(controls.clear_search.width, 5, "turn width={width}");
            let query_start = controls.search.x.saturating_add("Filter:".len() as u16);
            assert!(
                controls.clear_search.x.saturating_sub(query_start)
                    >= FILTER_CLEAR_GAP_WIDTH + FILTER_MIN_QUERY_WIDTH,
                "turn width={width} scope={scope:?}"
            );
        }
    }
}

#[test]
fn search_input_has_priority_over_global_shortcuts_and_supports_cancel() {
    let mut app = interaction_test_app(3, 1);
    let initial_theme = app.theme;
    let initial_view = app.view;
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('/'))));
    assert_eq!(app.focus, Focus::TaskSearch);

    for character in ['q', 't', '1', 'U', 'j', '测'] {
        assert!(!handle_key_event(
            &mut app,
            key_event(KeyCode::Char(character)),
        ));
    }
    assert_eq!(app.task_search, "qt1Uj测");
    assert_eq!(app.theme, initial_theme);
    assert_eq!(app.view, initial_view);
    assert_eq!(app.selected_task, 0);

    handle_key_event(&mut app, key_event(KeyCode::Left));
    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    assert_eq!(app.task_search, "qt1U测");
    handle_key_event(&mut app, key_event(KeyCode::Esc));
    assert_eq!(app.focus, Focus::Tasks);
    assert!(app.task_search.is_empty());

    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    for character in "task 2".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.task_search, "task 2");
    assert_eq!(app.selected_task, 2);
}

#[test]
fn escape_uses_confirmation_while_ctrl_c_and_q_exit_directly() {
    let mut app = interaction_test_app(2, 1);
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Esc)));
    assert!(app.quit_confirmation_visible);

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('2'))));
    assert_eq!(app.view, View::Overview);
    assert!(app.quit_confirmation_visible);
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Esc)));
    assert!(!app.quit_confirmation_visible);

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Esc)));
    assert!(handle_key_event(&mut app, key_event(KeyCode::Enter)));
    app.open_quit_confirmation();
    assert!(handle_key_event(&mut app, key_event(KeyCode::Char('q'))));

    app.open_quit_confirmation();
    assert!(handle_key_event(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ));
    app.close_quit_confirmation();
    assert!(handle_key_event(&mut app, key_event(KeyCode::Char('q'))));

    app.begin_task_search();
    assert!(handle_key_event(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ));
    handle_key_event(&mut app, key_event(KeyCode::Char('x')));
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Esc)));
    assert_eq!(app.focus, Focus::Tasks);
    assert!(!app.quit_confirmation_visible);
}

#[test]
fn quit_confirmation_renders_and_blocks_background_mouse_input() {
    for (theme, width, height) in [(Theme::Dark, 80, 24), (Theme::Light, 12, 9)] {
        let mut app = interaction_test_app(3, 2);
        app.theme = theme;
        app.open_quit_confirmation();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let hitbox = app
            .quit_confirmation_hitbox
            .expect("quit controls should render");
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(hitbox.confirm.x + 1, hitbox.confirm.y)].symbol(),
            "↵"
        );
        assert_eq!(
            buffer[(hitbox.confirm.x + 1, hitbox.confirm.y)].fg,
            theme.palette().accent
        );
        assert_eq!(buffer[(hitbox.cancel.x + 1, hitbox.cancel.y)].symbol(), "E");

        if width == 80 {
            let tabs = app.view_tabs_hitbox.unwrap();
            let overview = tabs.tabs[View::Overview.index()];
            assert_eq!(
                buffer[(overview.x + 1, overview.y)].fg,
                theme.palette().muted
            );
            let models = app.window_controls_hitbox.unwrap().toggle_models;
            assert!(
                !buffer[(models.x + 1, models.y)]
                    .modifier
                    .contains(Modifier::UNDERLINED)
            );
            let filter = app.task_controls_hitbox.unwrap().search;
            assert_eq!(buffer[(filter.x, filter.y)].fg, theme.palette().muted);
        }

        let health = app.view_tabs_hitbox.unwrap().tabs[View::Health.index()];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), health.x, health.y),
        ));
        assert_eq!(app.view, View::Overview);
        assert!(app.quit_confirmation_visible);

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.cancel.right() - 1,
                hitbox.cancel.y,
            ),
        ));
        assert!(!app.quit_confirmation_visible);
        assert!(!app.quit_requested);

        app.open_quit_confirmation();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let confirm = app.quit_confirmation_hitbox.unwrap().confirm;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                confirm.right() - 1,
                confirm.y,
            ),
        ));
        assert!(app.quit_requested);
    }
}

#[test]
fn quit_confirmation_never_renders_partial_buttons_in_tiny_terminals() {
    for width in 3..=12 {
        let mut terminal = Terminal::new(TestBackend::new(width, 7)).unwrap();
        let mut hitbox = None;
        terminal
            .draw(|frame| {
                hitbox = Some(render_quit_confirmation(frame, frame.area(), Theme::Dark));
            })
            .unwrap();
        let hitbox = hitbox.unwrap();
        assert_eq!(!hitbox.confirm.is_empty(), width >= 5, "width={width}");
        assert_eq!(!hitbox.cancel.is_empty(), width >= 11, "width={width}");
        if !hitbox.confirm.is_empty() {
            assert_eq!(hitbox.confirm.width, 3);
            assert!(hitbox.confirm.right() <= width);
            assert_eq!(
                terminal.backend().buffer()[(hitbox.confirm.x + 1, hitbox.confirm.y)].symbol(),
                "↵"
            );
        }
        if !hitbox.cancel.is_empty() {
            assert_eq!(hitbox.cancel.width, 5);
            assert!(hitbox.cancel.right() <= width);
            assert_eq!(
                terminal.backend().buffer()[(hitbox.cancel.x + 1, hitbox.cancel.y)].symbol(),
                "E"
            );
        }
    }
}

#[test]
fn delete_clears_the_focused_filter_only_after_editing_finishes() {
    let mut app = interaction_test_app(2, 4);
    app.task_search = "task".to_string();
    app.task_search_before_edit.clone_from(&app.task_search);
    app.reconcile_task_filter(true);
    app.focus_turns();
    app.turn_search = "model-2".to_string();
    app.turn_search_before_edit.clone_from(&app.turn_search);
    app.reconcile_turn_filter(true, None);

    handle_key_event(&mut app, key_event(KeyCode::Delete));
    assert!(app.turn_search.is_empty());
    assert_eq!(app.task_search, "task");
    assert_eq!(app.selected_task_turn_count(), 4);

    app.focus_tasks();
    handle_key_event(&mut app, key_event(KeyCode::Delete));
    assert!(app.task_search.is_empty());

    app.task_search = "task".to_string();
    app.task_search_before_edit.clone_from(&app.task_search);
    app.begin_task_search();
    handle_key_event(&mut app, key_event(KeyCode::Home));
    handle_key_event(&mut app, key_event(KeyCode::Delete));
    assert_eq!(app.task_search, "ask");
    assert_eq!(app.focus, Focus::TaskSearch);
}

#[test]
fn returning_to_tasks_clears_turn_filter_and_keeps_task_filter() {
    let mut app = interaction_test_app(2, 4);
    app.task_search = "task".to_string();
    app.task_search_before_edit.clone_from(&app.task_search);
    app.reconcile_task_filter(true);
    app.focus_turns();
    app.turn_search = "model-2".to_string();
    app.turn_search_before_edit.clone_from(&app.turn_search);
    app.reconcile_turn_filter(true, None);
    assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-0-2");

    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.task_search, "task");
    assert!(app.turn_search.is_empty());
    assert!(app.turn_search_before_edit.is_empty());
    assert!(app.turn_search_restore_turn_id.is_none());
    assert_eq!(app.turn_search_restore_offset, 0);
    assert_eq!(app.selected_task_turn_count(), 4);
    assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-0-2");

    app.focus_turns();
    app.turn_search = "model-1".to_string();
    app.turn_search_before_edit.clone_from(&app.turn_search);
    app.reconcile_turn_filter(true, None);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let task_rows = app.task_table_hitbox.unwrap().rows;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            task_rows.x,
            task_rows.y,
        ),
    ));
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.task_search, "task");
    assert!(app.turn_search.is_empty());

    app.focus_turns();
    app.turn_search = "model-3".to_string();
    app.reconcile_turn_filter(true, None);
    app.begin_task_search();
    assert_eq!(app.focus, Focus::TaskSearch);
    assert_eq!(app.task_search, "task");
    assert!(app.turn_search.is_empty());
}

#[test]
fn turn_filter_is_independent_and_consumes_shortcuts_while_editing() {
    let mut app = interaction_test_app(2, 4);
    app.task_search = "task".to_string();
    app.task_search_before_edit = app.task_search.clone();
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    assert_eq!(app.focus, Focus::Turns);

    handle_key_event(&mut app, key_event(KeyCode::Char('f')));
    assert_eq!(app.focus, Focus::TurnSearch);
    for character in "model-2".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    assert_eq!(app.task_search, "task");
    assert_eq!(app.selected_task_turn_count(), 1);
    assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-0-2");

    let initial_view = app.view;
    let initial_visibility = app.turns_default_visible;
    for character in ['v', '1'] {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    assert_eq!(app.view, initial_view);
    assert_eq!(app.turns_default_visible, initial_visibility);
    assert_eq!(app.turn_search, "model-2v1");

    handle_key_event(&mut app, key_event(KeyCode::Esc));
    assert_eq!(app.focus, Focus::Turns);
    assert!(app.turn_search.is_empty());
    assert_eq!(app.selected_task_turn_count(), 4);

    app.snapshot.turns[2].service_tier = Some("priority".to_string());
    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    for character in "fa".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    assert_eq!(app.focus, Focus::Turns);
    assert_eq!(app.selected_task_turn_count(), 1);
    assert!(app.selected_turn_record().unwrap().is_fast());
    assert_eq!(app.task_search, "task");
}

#[test]
fn turn_filter_mouse_controls_work_while_temporarily_expanded() {
    let mut app = interaction_test_app(1, 4);
    app.toggle_turns_default_visibility();
    app.focus_turns();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let controls = app.turn_controls_hitbox.unwrap();
    assert!(!controls.back_tasks.is_empty());
    assert!(!controls.search.is_empty());
    assert!(controls.back_tasks.right() <= controls.search.x);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            controls.search.x,
            controls.search.y,
        ),
    ));
    assert_eq!(app.focus, Focus::TurnSearch);
    assert!(app.turns_temporarily_visible);

    for character in "model-2".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    assert_eq!(app.selected_task_turn_count(), 1);
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    assert_eq!(app.focus, Focus::Turns);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let clear = app.turn_controls_hitbox.unwrap().clear_search;
    assert!(!clear.is_empty());
    let clear_shortcut = &terminal.backend().buffer()[(clear.x + 1, clear.y)];
    assert_eq!(clear_shortcut.fg, app.theme.palette().accent);
    assert!(clear_shortcut.modifier.contains(Modifier::BOLD));
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            clear.right() - 1,
            clear.y,
        ),
    ));
    assert!(app.turn_search.is_empty());
    assert_eq!(app.selected_task_turn_count(), 4);
    assert_eq!(app.focus, Focus::Turns);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let back = app.turn_controls_hitbox.unwrap().back_tasks;
    assert!(!back.is_empty());
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), back.x, back.y),
    ));
    assert_eq!(app.focus, Focus::Tasks);
    assert!(!app.turns_temporarily_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_none());
}

#[test]
fn clicking_turn_filter_clear_commits_the_editor_before_esc() {
    let mut app = interaction_test_app(1, 4);
    app.turn_search = "model-".to_string();
    app.reconcile_turn_filter(true, None);
    app.focus_turns();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let search = app.turn_controls_hitbox.unwrap().search;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), search.x, search.y),
    ));
    assert_eq!(app.focus, Focus::TurnSearch);
    assert_eq!(app.turn_search_before_edit, "model-");
    handle_key_event(&mut app, key_event(KeyCode::Char('2')));
    assert_eq!(app.turn_search, "model-2");
    assert_eq!(app.selected_task_turn_count(), 1);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let clear = app.turn_controls_hitbox.unwrap().clear_search;
    let clear_shortcut = &terminal.backend().buffer()[(clear.x + 1, clear.y)];
    assert_eq!(clear_shortcut.fg, app.theme.palette().muted);
    assert!(!clear_shortcut.modifier.contains(Modifier::BOLD));
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), clear.x, clear.y),
    ));
    assert_eq!(app.focus, Focus::Turns);
    assert!(app.turn_search.is_empty());
    assert!(app.turn_search_before_edit.is_empty());
    assert!(app.turn_search_restore_turn_id.is_none());
    assert_eq!(app.selected_task_turn_count(), 4);

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Esc)));
    assert!(app.quit_confirmation_visible);
    assert_eq!(app.focus, Focus::Turns);
    assert!(app.turn_search.is_empty());
}

#[test]
fn filtered_turn_rows_keep_selection_detail_mouse_and_scroll_in_sync() {
    let mut app = interaction_test_app(1, 8);
    app.focus_turns();
    app.turn_search = "model-6".to_string();
    app.reconcile_turn_filter(true, None);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let hitbox = app.turn_table_hitbox.unwrap();
    assert_eq!(app.selected_task_turn_count(), 1);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.y,
        ),
    ));
    assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-0-6");
    let selected_before = app.selected_turn;
    handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            hitbox.viewport.x,
            hitbox.viewport.y,
        ),
    );
    assert_eq!(app.selected_turn, selected_before);
    assert_eq!(app.turn_offset, 0);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("turn=turn-0-6"));
    assert!(content.contains("message=message 0/6"));
    assert!(content.contains("est="));
    assert!(!content.contains("confidence="));

    app.turn_search = "no matching turn".to_string();
    app.reconcile_turn_filter(true, None);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("No matching turns"));
    assert!(!content.contains("No turns for selected task"));
}

#[test]
fn turns_visibility_and_focus_buttons_share_the_keyboard_state_machine() {
    let mut app = interaction_test_app(1, 3);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_some());

    let toggle = app.window_controls_hitbox.unwrap().toggle_turns;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), toggle.x, toggle.y),
    ));
    assert!(!app.turns_default_visible);
    assert!(!app.turns_temporarily_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_none());
    assert_eq!(app.task_table_hitbox.unwrap().viewport.width, 118);

    let enter = app.task_controls_hitbox.unwrap().enter_turns;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), enter.x, enter.y),
    ));
    assert_eq!(app.focus, Focus::Turns);
    assert!(app.turns_temporarily_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_some());

    let back = app.turn_controls_hitbox.unwrap().back_tasks;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), back.x, back.y),
    ));
    assert_eq!(app.focus, Focus::Tasks);
    assert!(!app.turns_temporarily_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_none());

    handle_key_event(&mut app, key_event(KeyCode::Char('v')));
    assert!(app.turns_default_visible);
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    assert_eq!(app.focus, Focus::Tasks);
    assert!(app.turns_visible());
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_some());
}

#[test]
fn status_legend_stays_under_tasks_when_turns_are_hidden() {
    fn row_text(terminal: &Terminal<TestBackend>, width: u16, row: u16) -> String {
        (0..width)
            .map(|column| terminal.backend().buffer()[(column, row)].symbol())
            .collect()
    }

    fn assert_has_legend(text: &str) {
        for (marker, label) in [
            ("R", "RUN"),
            ("W", "WAIT"),
            ("D", "DONE"),
            ("X", "STOP"),
            ("F", "FAIL"),
            ("?", "STALE"),
        ] {
            assert!(
                text.contains(&format!("{marker}:{label}"))
                    || text.contains(&format!("{marker} {label}")),
                "missing {marker}/{label} legend in: {text}"
            );
        }
    }

    for (width, height) in [(60, 24), (80, 24), (100, 30), (120, 40)] {
        let mut app = interaction_test_app(1, 1);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let task_footer = app.task_table_hitbox.unwrap().viewport.bottom();
        let turn_footer = app.turn_table_hitbox.unwrap().viewport.bottom();
        assert_has_legend(&row_text(&terminal, width, task_footer));
        let turn_footer_text = row_text(&terminal, width, turn_footer);
        assert!(!turn_footer_text.contains("R:RUN"));
        assert!(!turn_footer_text.contains("R RUN"));

        app.toggle_turns_default_visibility();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_none());
        let task_footer = app.task_table_hitbox.unwrap().viewport.bottom();
        let task_footer_text = row_text(&terminal, width, task_footer);
        assert_has_legend(&task_footer_text);
        if width == 120 {
            assert!(task_footer_text.contains("EXACT/H"));
        }
    }
}

#[test]
fn opening_turns_reveals_a_task_after_the_compact_viewport_shrinks() {
    for open_temporarily in [false, true] {
        let mut app = interaction_test_app(30, 1);
        app.toggle_turns_default_visibility();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let hidden_hitbox = app.task_table_hitbox.unwrap();
        let target = hidden_hitbox.capacity - 1;
        assert!(app.select_task(target, true));
        assert_eq!(app.task_table_offset, 0);
        assert!(!app.task_reveal_pending);

        let key = if open_temporarily {
            KeyCode::Enter
        } else {
            KeyCode::Char('v')
        };
        handle_key_event(&mut app, key_event(key));
        assert!(app.task_reveal_pending);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let visible_hitbox = app.task_table_hitbox.unwrap();
        assert!(visible_hitbox.capacity < hidden_hitbox.capacity);
        assert!(target >= visible_hitbox.offset);
        assert!(target < visible_hitbox.offset + visible_hitbox.capacity);
        assert!(!app.task_reveal_pending);
        assert_eq!(
            app.focus,
            if open_temporarily {
                Focus::Turns
            } else {
                Focus::Tasks
            }
        );
        assert_eq!(app.turns_temporarily_visible, open_temporarily);
        assert_eq!(app.turns_default_visible, !open_temporarily);
    }
}

#[test]
fn compact_task_controls_keep_focus_visibility_and_filter_hitboxes_separate() {
    let mut app = interaction_test_app(4, 2);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let controls = app.task_controls_hitbox.unwrap();
    assert!(!controls.enter_turns.is_empty());
    assert!(!controls.open_terminal.is_empty());
    assert!(!controls.toggle_tree.is_empty());
    assert!(!controls.collapse_all.is_empty());
    assert!(!controls.search.is_empty());
    assert!(controls.enter_turns.right() <= controls.open_terminal.x);
    assert!(controls.open_terminal.right() <= controls.toggle_tree.x);
    assert!(controls.toggle_tree.right() <= controls.collapse_all.x);
    assert!(controls.collapse_all.right() <= controls.sources[0].x);
    for pair in controls.sources.windows(2) {
        assert!(pair[0].right() <= pair[1].x);
    }
    assert!(controls.sources[3].right() <= controls.search.x);

    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(controls.enter_turns.x, controls.enter_turns.y)].symbol(),
        ENTER_FOCUS_HINT
    );
    assert_eq!(
        buffer[(controls.toggle_tree.x + 1, controls.toggle_tree.y)].symbol(),
        "R"
    );
    assert_eq!(
        buffer[(controls.open_terminal.x + 1, controls.open_terminal.y)].symbol(),
        "O"
    );
    assert_eq!(
        buffer[(controls.collapse_all.x + 1, controls.collapse_all.y)].symbol(),
        "E"
    );
    assert_eq!(buffer[(controls.search.x, controls.search.y)].symbol(), "F");

    let top_controls = app.window_controls_hitbox.unwrap();
    let top_turns = top_controls.toggle_turns;
    let top_models = top_controls.toggle_models;
    assert_eq!(buffer[(top_turns.x + 1, top_turns.y)].symbol(), "V");
    assert_eq!(buffer[(top_models.x + 1, top_models.y)].symbol(), "M");
    assert!(top_turns.right() <= top_models.x);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            top_models.right() - 1,
            top_models.y,
        ),
    ));
    assert!(!app.models_visible);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert_eq!(
        app.window_controls_hitbox.unwrap().toggle_models,
        top_models
    );
}

#[test]
fn temporary_turns_close_on_task_focus_and_view_changes() {
    let mut app = interaction_test_app(2, 2);
    app.toggle_turns_default_visibility();
    app.focus_turns();
    assert!(app.turns_temporarily_visible);
    app.focus_tasks();
    assert!(!app.turns_temporarily_visible);

    app.focus_turns();
    app.set_view(View::Health);
    assert_eq!(app.focus, Focus::Tasks);
    assert!(!app.turns_temporarily_visible);

    app.set_view(View::Overview);
    app.focus_turns();
    app.set_view(View::Overview);
    assert_eq!(app.focus, Focus::Turns);
    assert!(app.turns_temporarily_visible);
}

#[test]
fn clicking_the_selected_task_closes_temporarily_visible_turns() {
    let mut app = interaction_test_app(2, 2);
    app.toggle_turns_default_visibility();
    app.focus_turns();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let task_rows = app.task_table_hitbox.unwrap().rows;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            task_rows.x,
            task_rows.y,
        ),
    ));
    assert_eq!(app.selected_task, 0);
    assert_eq!(app.focus, Focus::Tasks);
    assert!(!app.turns_temporarily_visible);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_none());
}

#[test]
fn clicking_a_selected_task_again_matches_enter_navigation() {
    for turns_default_visible in [true, false] {
        let mut mouse_app = interaction_test_app(2, 2);
        let mut keyboard_app = interaction_test_app(2, 2);
        if !turns_default_visible {
            mouse_app.toggle_turns_default_visibility();
            keyboard_app.toggle_turns_default_visibility();
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut mouse_app))
            .unwrap();
        let task_rows = mouse_app.task_table_hitbox.unwrap().rows;
        let second_row = task_rows.y + 1;

        assert!(handle_mouse_event(
            &mut mouse_app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                task_rows.x,
                second_row,
            ),
        ));
        assert_eq!(mouse_app.selected_task, 1);
        assert_eq!(mouse_app.focus, Focus::Tasks);

        handle_key_event(&mut keyboard_app, key_event(KeyCode::Down));
        handle_key_event(&mut keyboard_app, key_event(KeyCode::Enter));
        assert!(handle_mouse_event(
            &mut mouse_app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                task_rows.x,
                second_row,
            ),
        ));
        assert_eq!(mouse_app.focus, keyboard_app.focus);
        assert_eq!(mouse_app.selected_task, keyboard_app.selected_task);
        assert_eq!(
            mouse_app.turns_temporarily_visible,
            keyboard_app.turns_temporarily_visible
        );
        assert_eq!(mouse_app.turns_visible(), keyboard_app.turns_visible());
    }
}

#[test]
fn repeated_task_click_respects_search_and_empty_turn_boundaries() {
    let mut searching = interaction_test_app(1, 1);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut searching))
        .unwrap();
    let selected_row = searching.task_table_hitbox.unwrap().rows;
    handle_key_event(&mut searching, key_event(KeyCode::Char('/')));
    assert_eq!(searching.focus, Focus::TaskSearch);
    assert!(handle_mouse_event(
        &mut searching,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            selected_row.x,
            selected_row.y,
        ),
    ));
    assert_eq!(searching.focus, Focus::Tasks);
    assert!(handle_mouse_event(
        &mut searching,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            selected_row.x,
            selected_row.y,
        ),
    ));
    assert_eq!(searching.focus, Focus::Turns);

    let mut no_turns = interaction_test_app(2, 0);
    no_turns.toggle_turns_default_visibility();
    terminal.draw(|frame| render(frame, &mut no_turns)).unwrap();
    let rows = no_turns.task_table_hitbox.unwrap().rows;
    for _ in 0..2 {
        assert!(handle_mouse_event(
            &mut no_turns,
            mouse_event(MouseEventKind::Down(MouseButton::Left), rows.x, rows.y + 1,),
        ));
    }
    assert_eq!(no_turns.selected_task, 1);
    assert_eq!(no_turns.focus, Focus::Tasks);
    assert!(!no_turns.turns_visible());
}

#[test]
fn clicking_the_task_scrollbar_closes_temporarily_visible_turns() {
    let mut app = interaction_test_app(30, 2);
    app.toggle_turns_default_visibility();
    app.focus_turns();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let scrollbar = app.task_scrollbar_hitbox.unwrap();
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.thumb.x,
            scrollbar.thumb.y,
        ),
    ));
    assert_eq!(app.focus, Focus::Tasks);
    assert!(!app.turns_temporarily_visible);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.turn_table_hitbox.is_none());
}

#[test]
fn fast_turns_are_badged_without_changing_ordinary_turns() {
    let mut app = interaction_test_app(1, 2);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let ordinary = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!ordinary.contains("FAST"));

    app.snapshot.turns[1].service_tier = Some("priority".to_string());
    app.focus_turns();
    app.select_turn(1, true);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let fast = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(fast.contains("FAST"));
    assert!(fast.contains("model-0"));
    assert!(fast.contains("model-1"));
    let turn_hitbox = app.turn_table_hitbox.unwrap();
    let fast_row_y = turn_hitbox.rows.y + u16::try_from(1 - turn_hitbox.offset).unwrap();
    let fast_row = (turn_hitbox.rows.x..turn_hitbox.rows.right())
        .map(|x| terminal.backend().buffer()[(x, fast_row_y)].symbol())
        .collect::<String>();
    assert!(fast_row.contains("model-1 FAST"));
    assert!(!fast_row.contains("FAST model-1"));

    let mut compact_terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    app.turn_reveal_pending = true;
    compact_terminal
        .draw(|frame| render(frame, &mut app))
        .unwrap();
    let compact_hitbox = app.turn_table_hitbox.unwrap();
    let compact_fast_row_y =
        compact_hitbox.rows.y + u16::try_from(1 - compact_hitbox.offset).unwrap();
    let compact_fast_row = (compact_hitbox.rows.x..compact_hitbox.rows.right())
        .map(|x| compact_terminal.backend().buffer()[(x, compact_fast_row_y)].symbol())
        .collect::<String>();
    assert!(
        compact_fast_row.contains("high/model-1 FAST"),
        "compact row was {compact_fast_row:?}"
    );

    app.snapshot.turns[1].model = Some("gpt-5.6-codex-super-long".to_string());
    app.turn_reveal_pending = true;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let wide_hitbox = app.turn_table_hitbox.unwrap();
    let wide_row_y = wide_hitbox.rows.y + u16::try_from(1 - wide_hitbox.offset).unwrap();
    let wide_row = (wide_hitbox.rows.x..wide_hitbox.rows.right())
        .map(|x| terminal.backend().buffer()[(x, wide_row_y)].symbol())
        .collect::<String>();
    assert!(wide_row.contains("… FAST"), "wide row was {wide_row:?}");

    app.turn_reveal_pending = true;
    compact_terminal
        .draw(|frame| render(frame, &mut app))
        .unwrap();
    let compact_hitbox = app.turn_table_hitbox.unwrap();
    let compact_row_y = compact_hitbox.rows.y + u16::try_from(1 - compact_hitbox.offset).unwrap();
    let compact_row = (compact_hitbox.rows.x..compact_hitbox.rows.right())
        .map(|x| compact_terminal.backend().buffer()[(x, compact_row_y)].symbol())
        .collect::<String>();
    assert!(
        compact_row.contains("… FAST"),
        "compact long-model row was {compact_row:?}"
    );
}

#[test]
fn search_cancel_restores_task_turn_and_viewports() {
    let mut app = interaction_test_app(8, 4);
    app.selected_task = 6;
    app.selected_turn = 3;
    app.task_table_offset = 4;
    app.turn_offset = 2;

    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    for character in "task 1".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    assert_eq!(app.selected_task, 1);
    assert_eq!(app.selected_turn, 0);
    handle_key_event(&mut app, key_event(KeyCode::Esc));

    assert_eq!(app.focus, Focus::Tasks);
    assert!(app.task_search.is_empty());
    assert_eq!(app.selected_task, 6);
    assert_eq!(app.selected_turn, 3);
    assert_eq!(app.task_table_offset, 4);
    assert_eq!(app.turn_offset, 2);
}

#[test]
fn long_unicode_search_keeps_the_cursor_visible_in_narrow_panels() {
    let (before, after, cursor_visible) =
        search_cursor_window("abcdefghijklmnopqrstuvwxyz测试", 28, 6);
    assert!(cursor_visible);
    assert!(UnicodeWidthStr::width(before.as_str()) + UnicodeWidthStr::width(after.as_str()) < 6);
    assert!(after.is_empty());

    let mut app = interaction_test_app(4, 1);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    for character in "abcdefghijklmnopqrstuvwxyz测试".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }

    for key in [KeyCode::End, KeyCode::Home] {
        handle_key_event(&mut app, key_event(key));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let search = app.task_controls_hitbox.unwrap().search;
        let buffer = terminal.backend().buffer();
        assert!((search.x..search.right()).any(|x| buffer[(x, search.y)].symbol() == "▌"));
    }
}

#[test]
fn task_and_turn_search_edit_complete_grapheme_clusters() {
    let mut app = interaction_test_app(2, 2);
    app.begin_task_search();
    for character in ['a', 'b'] {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    handle_key_event(&mut app, key_event(KeyCode::Left));
    handle_key_event(&mut app, key_event(KeyCode::Char('\u{301}')));
    assert_eq!(app.task_search, "a\u{301}b");
    assert_eq!(app.task_search_cursor, 1);

    handle_key_event(&mut app, key_event(KeyCode::End));
    assert_eq!(app.task_search_cursor, 2);
    handle_key_event(&mut app, key_event(KeyCode::Home));
    handle_key_event(&mut app, key_event(KeyCode::Right));
    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    assert_eq!(app.task_search, "b");
    assert_eq!(app.task_search_cursor, 0);
    handle_key_event(&mut app, key_event(KeyCode::Delete));
    assert!(app.task_search.is_empty());

    handle_key_event(&mut app, key_event(KeyCode::Enter));
    app.focus_turns();
    app.begin_turn_search();
    for character in ['👩', '\u{200d}', '💻', 'x'] {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    assert_eq!(app.turn_search, "👩\u{200d}💻x");
    assert_eq!(app.turn_search_cursor, 2);

    handle_key_event(&mut app, key_event(KeyCode::Left));
    handle_key_event(&mut app, key_event(KeyCode::Delete));
    assert_eq!(app.turn_search, "👩\u{200d}💻");
    assert_eq!(app.turn_search_cursor, 1);
    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    assert!(app.turn_search.is_empty());
    assert_eq!(app.turn_search_cursor, 0);

    app.turn_search = "👩💻".to_string();
    app.turn_search_cursor = grapheme_count(&app.turn_search);
    handle_key_event(&mut app, key_event(KeyCode::Left));
    handle_key_event(&mut app, key_event(KeyCode::Char('\u{200d}')));
    assert_eq!(app.turn_search, "👩\u{200d}💻");
    assert_eq!(app.turn_search_cursor, 1);
    handle_key_event(&mut app, key_event(KeyCode::Char('x')));
    assert_eq!(app.turn_search, "👩\u{200d}💻x");
    assert_eq!(app.turn_search_cursor, 2);
}

#[test]
fn keyboard_focus_moves_between_tasks_and_turns_and_reveals_selection() {
    let mut app = interaction_test_app(2, 30);
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.selected_task, 0);
    assert_eq!(app.selected_turn, 0);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    handle_key_event(&mut app, key_event(KeyCode::Down));
    assert_eq!(app.selected_task, 1);
    assert_eq!(app.selected_turn, 0);

    handle_key_event(&mut app, key_event(KeyCode::Enter));
    assert_eq!(app.focus, Focus::Turns);
    handle_key_event(&mut app, key_event(KeyCode::End));
    assert_eq!(app.selected_task, 1);
    assert_eq!(app.selected_turn, 29);
    assert!(app.turn_offset > 0);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let turn_hitbox = app.turn_table_hitbox.expect("turn rows should render");
    assert!(app.selected_turn >= turn_hitbox.offset);
    assert!(app.selected_turn < turn_hitbox.offset + turn_hitbox.capacity);
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("30/30"));
    let turn_offset = app.turn_offset;
    assert!(!app.turn_reveal_pending);

    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.selected_turn, 29);
    assert_eq!(app.turn_offset, turn_offset);
    assert!(!app.turn_reveal_pending);
    handle_key_event(&mut app, key_event(KeyCode::Up));
    assert_eq!(app.selected_task, 0);
    assert_eq!(app.selected_turn, 0);

    let mut no_turns = interaction_test_app(1, 0);
    handle_key_event(&mut no_turns, key_event(KeyCode::Enter));
    assert_eq!(no_turns.focus, Focus::Tasks);
}

#[test]
fn health_view_ignores_hidden_panel_navigation() {
    let mut app = interaction_test_app(3, 3);
    handle_key_event(&mut app, key_event(KeyCode::Enter));
    handle_key_event(&mut app, key_event(KeyCode::Down));
    assert_eq!(app.focus, Focus::Turns);
    assert_eq!(app.selected_turn, 1);
    app.view = View::Health;
    let before = (
        app.selected_task,
        app.selected_turn,
        app.task_table_offset,
        app.turn_offset,
    );

    for key in [
        KeyCode::Down,
        KeyCode::Up,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageDown,
        KeyCode::PageUp,
        KeyCode::Backspace,
    ] {
        handle_key_event(&mut app, key_event(key));
    }
    assert_eq!(
        (
            app.selected_task,
            app.selected_turn,
            app.task_table_offset,
            app.turn_offset
        ),
        before
    );
    assert_eq!(app.focus, Focus::Turns);
}

#[test]
fn refresh_preserves_viewport_rows_and_leaves_empty_turn_focus() {
    let mut app = interaction_test_app(50, 30);
    app.selected_task = 20;
    app.selected_turn = 10;
    app.task_table_offset = 10;
    app.turn_offset = 5;
    app.focus = Focus::Turns;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let selected_thread = app.raw_selected_thread_id().unwrap().to_string();
    let selected_turn = app.selected_turn_record().unwrap().turn_id.clone();
    let task_top = app.filtered_task_indices()[app.task_table_offset];
    let task_top_id = app.snapshot.tasks[task_top].thread_id.clone();
    let turn_top_id = app
        .snapshot
        .turns
        .iter()
        .filter(|turn| turn.thread_id == selected_thread)
        .nth(app.turn_offset)
        .unwrap()
        .turn_id
        .clone();

    let mut snapshot = app.snapshot.clone();
    let mut inserted_task = snapshot.tasks[0].clone();
    inserted_task.thread_id = "inserted-task".to_string();
    inserted_task.title = "inserted task".to_string();
    snapshot.tasks.insert(0, inserted_task);
    let mut inserted_turn = snapshot
        .turns
        .iter()
        .find(|turn| turn.thread_id == selected_thread)
        .unwrap()
        .clone();
    inserted_turn.turn_id = "inserted-turn".to_string();
    snapshot.turns.insert(0, inserted_turn);
    app.replace(
        CollectionResult {
            snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );

    assert_eq!(app.raw_selected_thread_id(), Some(selected_thread.as_str()));
    assert_eq!(app.selected_turn_record().unwrap().turn_id, selected_turn);
    assert_eq!(
        app.snapshot.tasks[app.filtered_task_indices()[app.task_table_offset]].thread_id,
        task_top_id
    );
    assert_eq!(
        app.snapshot
            .turns
            .iter()
            .filter(|turn| turn.thread_id == selected_thread)
            .nth(app.turn_offset)
            .unwrap()
            .turn_id,
        turn_top_id
    );

    let mut no_turns = app.snapshot.clone();
    no_turns
        .turns
        .retain(|turn| turn.thread_id != selected_thread);
    app.replace(
        CollectionResult {
            snapshot: no_turns,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.selected_task_turn_count(), 0);
}

#[test]
fn refresh_keeps_a_top_task_viewport_following_new_rows() {
    let mut app = interaction_test_app(50, 1);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let selected_thread = app.raw_selected_thread_id().unwrap().to_string();
    let capacity = app.task_table_hitbox.unwrap().capacity;
    let mut snapshot = app.snapshot.clone();
    for index in 0..capacity + 2 {
        let mut inserted_task = snapshot.tasks[0].clone();
        inserted_task.thread_id = format!("newest-task-{index}");
        inserted_task.title = format!("newest task {index}");
        snapshot.tasks.insert(0, inserted_task);
    }
    let newest_thread = format!("newest-task-{}", capacity + 1);

    app.replace(
        CollectionResult {
            snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    assert_eq!(app.task_table_offset, 0);
    assert_eq!(app.raw_selected_thread_id(), Some(selected_thread.as_str()));
    let hitbox = app.task_table_hitbox.expect("tasks should remain visible");
    let first_visible = app.filtered_task_indices()[hitbox.offset];
    assert_eq!(app.snapshot.tasks[first_visible].thread_id, newest_thread);
    let selected_position = app
        .filtered_task_indices()
        .iter()
        .position(|index| *index == app.selected_task)
        .unwrap();
    assert!(selected_position >= hitbox.capacity);
}

#[test]
fn refresh_restores_filtered_turn_selection_and_viewport_by_id() {
    let mut app = interaction_test_app(1, 30);
    for (index, turn) in app.snapshot.turns.iter_mut().enumerate() {
        turn.model = Some(if index % 2 == 0 { "keep" } else { "drop" }.to_string());
    }
    app.turn_search = "keep".to_string();
    app.selected_turn = 10;
    app.turn_offset = 5;
    app.focus = Focus::Turns;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let selected_turn_id = app.selected_turn_record().unwrap().turn_id.clone();
    let viewport_turn_id = app.snapshot.turns[app.filtered_turn_indices()[app.turn_offset]]
        .turn_id
        .clone();
    let mut snapshot = app.snapshot.clone();
    let mut inserted = snapshot.turns[0].clone();
    inserted.turn_id = "inserted-matching-turn".to_string();
    snapshot.turns.insert(0, inserted);

    app.replace(
        CollectionResult {
            snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );

    assert_eq!(
        app.selected_turn_record().unwrap().turn_id,
        selected_turn_id
    );
    assert_eq!(
        app.snapshot.turns[app.filtered_turn_indices()[app.turn_offset]].turn_id,
        viewport_turn_id
    );
}

#[test]
fn refresh_reveals_a_previously_visible_selection_after_reordering() {
    let mut app = interaction_test_app(50, 30);
    app.selected_task = 20;
    app.selected_turn = 10;
    app.task_table_offset = 10;
    app.turn_offset = 5;
    app.focus = Focus::Turns;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let selected_thread = app.raw_selected_thread_id().unwrap().to_string();
    let selected_turn = app.selected_turn_record().unwrap().turn_id.clone();
    let mut snapshot = app.snapshot.clone();
    let task_index = snapshot
        .tasks
        .iter()
        .position(|task| task.thread_id == selected_thread)
        .unwrap();
    let task = snapshot.tasks.remove(task_index);
    snapshot.tasks.insert(0, task);
    let turn_index = snapshot
        .turns
        .iter()
        .position(|turn| turn.turn_id == selected_turn)
        .unwrap();
    let turn = snapshot.turns.remove(turn_index);
    let first_thread_turn = snapshot
        .turns
        .iter()
        .position(|turn| turn.thread_id == selected_thread)
        .unwrap();
    snapshot.turns.insert(first_thread_turn, turn);

    app.replace(
        CollectionResult {
            snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    assert_eq!(app.raw_selected_thread_id(), Some(selected_thread.as_str()));
    assert_eq!(app.selected_turn_record().unwrap().turn_id, selected_turn);
    let task_position = app
        .filtered_task_indices()
        .iter()
        .position(|index| *index == app.selected_task)
        .unwrap();
    let task_hitbox = app.task_table_hitbox.unwrap();
    assert!(task_position >= task_hitbox.offset);
    assert!(task_position < task_hitbox.offset + task_hitbox.capacity);
    let turn_hitbox = app.turn_table_hitbox.unwrap();
    assert!(app.selected_turn >= turn_hitbox.offset);
    assert!(app.selected_turn < turn_hitbox.offset + turn_hitbox.capacity);
}

#[test]
fn focused_panel_uses_the_vivid_selection_marker() {
    let mut app = interaction_test_app(1, 2);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let palette = app.theme.palette();
    let task_markers = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .filter(|cell| cell.symbol() == "▌" && cell.fg == palette.accent)
        .count();
    assert_eq!(task_markers, 1);

    handle_key_event(&mut app, key_event(KeyCode::Enter));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol() == "▏" && cell.fg == palette.muted)
    );
    assert_eq!(
        buffer
            .content()
            .iter()
            .filter(|cell| cell.symbol() == "▌" && cell.fg == palette.accent)
            .count(),
        1
    );
}

#[test]
fn focus_transition_hints_follow_available_keyboard_actions_without_layout_shift() {
    let mut app = interaction_test_app(1, 2);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let palette = app.theme.palette();
    let task_controls = app.task_controls_hitbox.unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol() == ENTER_FOCUS_HINT && cell.fg == palette.accent)
    );
    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| cell.symbol() != BACK_FOCUS_HINT)
    );

    handle_key_event(&mut app, key_event(KeyCode::Enter));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| cell.symbol() != ENTER_FOCUS_HINT)
    );
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol() == BACK_FOCUS_HINT && cell.fg == palette.accent)
    );
    assert_eq!(app.task_controls_hitbox.unwrap(), task_controls);

    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| { cell.symbol() != ENTER_FOCUS_HINT && cell.symbol() != BACK_FOCUS_HINT })
    );
    assert_eq!(app.task_controls_hitbox.unwrap(), task_controls);

    let mut no_turns = interaction_test_app(1, 0);
    terminal.draw(|frame| render(frame, &mut no_turns)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.symbol() != ENTER_FOCUS_HINT)
    );
}

#[test]
fn view_tabs_use_rendered_padding_and_support_mouse_switching() {
    let mut app = interaction_test_app(3, 2);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let tabs = app.view_tabs_hitbox.expect("view tabs should render");
    assert_eq!(tabs.tabs[View::Overview.index()], Rect::new(0, 0, 12, 1));
    assert_eq!(tabs.tabs[View::Trends.index()], Rect::new(15, 0, 10, 1));
    assert_eq!(tabs.tabs[View::Summary.index()], Rect::new(28, 0, 11, 1));
    assert_eq!(tabs.tabs[View::Health.index()], Rect::new(42, 0, 9, 1));
    assert_eq!(tabs.tabs[View::Settings.index()], Rect::new(54, 0, 12, 1));
    assert_eq!(tabs.rendered_right, 66);

    let divider = tabs.tabs[View::Overview.index()].right();
    assert!(!handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), divider, 0),
    ));
    assert_eq!(app.view, View::Overview);
    assert!(!handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            tabs.tabs[View::Health.index()].x,
            0,
        ),
    ));

    handle_key_event(&mut app, key_event(KeyCode::Char('/')));
    handle_key_event(&mut app, key_event(KeyCode::Char('q')));
    assert_eq!(app.focus, Focus::TaskSearch);
    let health = tabs.tabs[View::Health.index()];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), health.x, health.y),
    ));
    assert_eq!(app.view, View::Health);
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.task_search, "q");
    assert!(handle_key_event(&mut app, key_event(KeyCode::Char('q'))));

    for width in [8, 20, 80] {
        let area = Rect::new(0, 0, width, 1);
        let hitbox = view_tabs_hitbox(area);
        assert!(
            hitbox
                .tabs
                .iter()
                .all(|tab| tab.x >= area.x && tab.right() <= area.right())
        );
    }

    let mut narrow = interaction_test_app(3, 2);
    let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
    terminal.draw(|frame| render(frame, &mut narrow)).unwrap();
    let tabs = narrow.view_tabs_hitbox.unwrap();
    assert!(!tabs.tabs[View::Overview.index()].is_empty());
    assert!(!tabs.tabs[View::Trends.index()].is_empty());
    assert!(tabs.tabs[View::Summary.index()].is_empty());
    assert_eq!(tabs.rendered_right, 20);
    assert!(!handle_mouse_event(
        &mut narrow,
        mouse_event(MouseEventKind::Down(MouseButton::Left), 19, 0),
    ));
    assert_eq!(narrow.view, View::Overview);

    for theme in [Theme::Dark, Theme::Light] {
        for width in 21..=25 {
            let mut app = interaction_test_app(3, 2);
            app.theme = theme;
            let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let summary = app.view_tabs_hitbox.unwrap().tabs[View::Summary.index()];
            if width < 25 {
                assert!(summary.is_empty(), "theme={theme:?} width={width}");
            } else {
                assert_eq!(summary, Rect::new(19, 0, 6, 1));
                let rendered = (summary.x..summary.right())
                    .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                    .collect::<String>();
                assert_eq!(rendered, " U Sum");
                assert_eq!(
                    terminal.backend().buffer()[(20, 0)].fg,
                    theme.palette().accent
                );
            }
        }
    }

    for column in 20..25 {
        let mut app = interaction_test_app(3, 2);
        let mut terminal = Terminal::new(TestBackend::new(25, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), column, 0),
        ));
        assert_eq!(app.view, View::Summary, "column={column}");
    }
}

#[test]
fn title_hitboxes_require_the_complete_control_label() {
    let area = Rect::new(0, 0, 29, 5);
    assert_eq!(title_hitbox(area, 25, 3), Rect::new(25, 0, 3, 1));
    assert!(title_hitbox(area, 26, 3).is_empty());
    assert!(title_hitbox(area, 28, 1).is_empty());
}

#[test]
fn trends_view_uses_responsive_panels_and_btop_controls() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut app = interaction_test_app(3, 2);
        app.theme = theme;
        app.set_view(View::Trends);
        let selected_task = app.selected_task;
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Quota Remaining"));
        assert!(!content.contains("Weekly Local Tokens"));
        assert!(content.contains("No history recorded yet"));
        assert!(app.task_table_hitbox.is_none());
        assert!(app.turn_table_hitbox.is_none());
        let controls = app.trend_controls_hitbox.expect("trend controls");
        assert!(controls.sections.iter().all(|area| !area.is_empty()));
        assert!(controls.previous_day.is_empty());
        assert!(controls.next_day.is_empty());
        assert!(controls.now.is_empty());

        let remaining = controls.sections[TrendSection::Remaining.index()];
        let shortcut = &terminal.backend().buffer()[(remaining.x + 1, remaining.y)];
        assert_eq!(shortcut.symbol(), "R");
        assert!(shortcut.modifier.contains(Modifier::UNDERLINED));
        assert_eq!(shortcut.bg, theme.palette().accent);

        for section in TrendSection::ALL {
            let button = controls.sections[section.index()];
            for x in [button.x, button.x + button.width / 2, button.right() - 1] {
                assert!(handle_mouse_event(
                    &mut app,
                    mouse_event(MouseEventKind::Down(MouseButton::Left), x, button.y),
                ));
                assert_eq!(app.trend_section, section);
            }
        }
        handle_key_event(&mut app, key_event(KeyCode::Char('W')));
        assert_eq!(app.trend_section, TrendSection::Weekly);
        handle_key_event(&mut app, key_event(KeyCode::Char('H')));
        assert_eq!(app.trend_section, TrendSection::HalfHour);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("15m Local Tokens"));
        assert!(content.contains("15m ~EST Usage"));
        assert!(!content.contains("Weekly Local Tokens"));

        let controls = app.trend_controls_hitbox.expect("15-minute controls");
        assert!(!controls.previous_day.is_empty());
        assert!(!controls.next_day.is_empty());
        assert!(!controls.now.is_empty());
        for x in [
            controls.previous_day.x,
            controls.previous_day.x + controls.previous_day.width / 2,
            controls.previous_day.right() - 1,
        ] {
            assert!(handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    x,
                    controls.previous_day.y,
                ),
            ));
        }
        assert_eq!(app.trend_day_offset, 3);
        handle_key_event(&mut app, key_event(KeyCode::Char(']')));
        assert_eq!(app.trend_day_offset, 2);
        handle_key_event(&mut app, key_event(KeyCode::Char('[')));
        handle_key_event(&mut app, key_event(KeyCode::Char('N')));
        assert_eq!(app.trend_day_offset, 0);
        for _ in 0..16 {
            handle_key_event(&mut app, key_event(KeyCode::Char('[')));
        }
        assert_eq!(
            app.trend_day_offset,
            u16::try_from(HISTORY_VIEW_DAYS - 1).unwrap()
        );
        handle_key_event(&mut app, key_event(KeyCode::Char('N')));

        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        handle_key_event(&mut app, key_event(KeyCode::Char('j')));
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.selected_task, selected_task);
    }

    let mut app = interaction_test_app(1, 1);
    app.trend_section = TrendSection::Weekly;
    app.set_view(View::Trends);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    for title in [
        "Quota Remaining",
        "Weekly Local Tokens",
        "Weekly ~EST Usage",
        "15m Local Tokens",
        "15m ~EST Usage",
    ] {
        assert!(content.contains(title), "missing {title}: {content}");
    }
    let controls = app.trend_controls_hitbox.expect("wide trend controls");
    assert!(controls.sections.iter().all(|area| area.is_empty()));
    assert!(!controls.previous_day.is_empty());
    handle_key_event(&mut app, key_event(KeyCode::Char('R')));
    assert_eq!(app.trend_section, TrendSection::Weekly);
}

#[test]
fn trend_inspect_control_is_whole_label_clickable_and_esc_exits_inspection() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T09:16:42Z")
        .unwrap()
        .with_timezone(&Utc);

    for theme in [Theme::Dark, Theme::Light] {
        let mut app = interaction_test_app(1, 1);
        app.theme = theme;
        app.replace_history(trend_history_fixture(now));
        app.set_view(View::Trends);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render_at(frame, &mut app, now))
            .unwrap();

        let inspect = app.trend_controls_hitbox.expect("trend controls").inspect;
        assert_eq!(
            buffer_rect_text(terminal.backend().buffer(), inspect),
            "[I]Inspect"
        );
        let shortcut = &terminal.backend().buffer()[(inspect.x + 1, inspect.y)];
        assert_eq!(shortcut.symbol(), "I");
        assert_eq!(shortcut.fg, theme.palette().accent);
        assert!(shortcut.modifier.contains(Modifier::BOLD));

        for x in [
            inspect.x,
            inspect.x + inspect.width / 2,
            inspect.right() - 1,
        ] {
            let before = app.trend_inspect_mode;
            assert!(handle_mouse_event(
                &mut app,
                mouse_event(MouseEventKind::Down(MouseButton::Left), x, inspect.y),
            ));
            assert_ne!(app.trend_inspect_mode, before);
        }
        assert!(app.trend_inspect_mode);
        assert!(app.trend_inspection.is_some());
        terminal
            .draw(|frame| render_at(frame, &mut app, now))
            .unwrap();
        let selected_inspect = app
            .trend_controls_hitbox
            .expect("selected trend controls")
            .inspect;
        assert_eq!(selected_inspect, inspect);
        let selected_shortcut =
            &terminal.backend().buffer()[(selected_inspect.x + 1, selected_inspect.y)];
        assert_eq!(selected_shortcut.bg, theme.palette().accent);
        assert_eq!(selected_shortcut.fg, theme.palette().background);
        assert!(selected_shortcut.modifier.contains(Modifier::UNDERLINED));

        assert!(!handle_key_event(&mut app, key_event(KeyCode::Esc)));
        assert!(!app.trend_inspect_mode);
        assert!(app.trend_inspection.is_none());
        assert!(!app.quit_confirmation_visible);

        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('I'))));
        assert!(app.trend_inspect_mode);
        assert!(app.trend_inspection.is_some());
        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('i'))));
        assert!(!app.trend_inspect_mode);
        assert!(app.trend_inspection.is_none());

        let mut compact = interaction_test_app(1, 1);
        compact.theme = theme;
        compact.replace_history(trend_history_fixture(now));
        compact.set_view(View::Trends);
        let mut compact_terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        compact_terminal
            .draw(|frame| render_at(frame, &mut compact, now))
            .unwrap();
        let compact_inspect = compact
            .trend_controls_hitbox
            .expect("compact trend controls")
            .inspect;
        assert_eq!(compact_inspect.width, 3);
        assert_eq!(
            buffer_rect_text(compact_terminal.backend().buffer(), compact_inspect),
            "[I]"
        );
        for x in compact_inspect.x..compact_inspect.right() {
            let before = compact.trend_inspect_mode;
            assert!(handle_mouse_event(
                &mut compact,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    x,
                    compact_inspect.y,
                ),
            ));
            assert_ne!(compact.trend_inspect_mode, before);
        }
        assert!(compact.trend_inspect_mode);
    }
}

#[test]
fn trend_inspect_keyboard_steps_samples_and_visible_panels() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T09:16:42Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(1, 1);
    app.replace_history(trend_history_fixture(now));
    app.set_view(View::Trends);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();

    handle_key_event(&mut app, key_event(KeyCode::Char('i')));
    assert_eq!(
        app.trend_inspection,
        Some(TrendInspection {
            panel: TrendPanelId::Remaining,
            at: now,
        })
    );
    handle_key_event(&mut app, key_event(KeyCode::Left));
    assert_eq!(
        app.trend_inspection,
        Some(TrendInspection {
            panel: TrendPanelId::Remaining,
            at: now - ChronoDuration::hours(1),
        })
    );
    handle_key_event(&mut app, key_event(KeyCode::Home));
    assert_eq!(
        app.trend_inspection,
        Some(TrendInspection {
            panel: TrendPanelId::Remaining,
            at: now - ChronoDuration::days(1),
        })
    );
    handle_key_event(&mut app, key_event(KeyCode::Right));
    assert_eq!(
        app.trend_inspection,
        Some(TrendInspection {
            panel: TrendPanelId::Remaining,
            at: now - ChronoDuration::hours(1),
        })
    );
    handle_key_event(&mut app, key_event(KeyCode::End));
    assert_eq!(
        app.trend_inspection,
        Some(TrendInspection {
            panel: TrendPanelId::Remaining,
            at: now,
        })
    );

    handle_key_event(&mut app, key_event(KeyCode::Up));
    assert_eq!(
        app.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::Remaining)
    );
    for panel in [
        TrendPanelId::WeeklyTokens,
        TrendPanelId::WeeklyEstimated,
        TrendPanelId::LocalTokens,
        TrendPanelId::LocalEstimated,
    ] {
        handle_key_event(&mut app, key_event(KeyCode::Down));
        assert_eq!(
            app.trend_inspection.map(|inspection| inspection.panel),
            Some(panel)
        );
    }
    handle_key_event(&mut app, key_event(KeyCode::Down));
    assert_eq!(
        app.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::LocalEstimated)
    );
    handle_key_event(&mut app, key_event(KeyCode::Up));
    assert_eq!(
        app.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::LocalTokens)
    );

    let mut compact = interaction_test_app(1, 1);
    compact.replace_history(trend_history_fixture(now));
    compact.set_view(View::Trends);
    compact.trend_section = TrendSection::Weekly;
    let mut compact_terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    compact_terminal
        .draw(|frame| render_at(frame, &mut compact, now))
        .unwrap();
    handle_key_event(&mut compact, key_event(KeyCode::Char('i')));
    assert_eq!(
        compact.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::WeeklyTokens)
    );
    handle_key_event(&mut compact, key_event(KeyCode::Up));
    assert_eq!(
        compact.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::WeeklyTokens)
    );
    handle_key_event(&mut compact, key_event(KeyCode::Down));
    assert_eq!(
        compact.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::WeeklyEstimated)
    );
    handle_key_event(&mut compact, key_event(KeyCode::Down));
    assert_eq!(
        compact.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::WeeklyEstimated)
    );
    handle_key_event(&mut compact, key_event(KeyCode::Up));
    assert_eq!(
        compact.trend_inspection.map(|inspection| inspection.panel),
        Some(TrendPanelId::WeeklyTokens)
    );
}

#[test]
fn trend_inspect_mouse_click_drag_and_release_retain_the_selected_sample() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T09:16:42Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(1, 1);
    app.replace_history(trend_history_fixture(now));
    app.set_view(View::Trends);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();

    let chart = app
        .trend_chart_hitboxes
        .iter()
        .find(|hitbox| hitbox.panel == TrendPanelId::Remaining)
        .cloned()
        .expect("remaining chart hitbox");
    let row = chart.plot.bottom() - 1;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), chart.plot.x, row,),
    ));
    assert!(app.trend_inspect_mode);
    assert_eq!(
        app.trend_inspection,
        Some(TrendInspection {
            panel: TrendPanelId::Remaining,
            at: now - ChronoDuration::days(1),
        })
    );
    assert_eq!(
        app.trend_drag,
        Some(TrendDrag {
            panel: TrendPanelId::Remaining
        })
    );

    let before_move = app.trend_inspection;
    let moved = mouse_event(
        MouseEventKind::Moved,
        chart.plot.x + chart.plot.width / 2,
        row,
    );
    let handled = handle_mouse_event(&mut app, moved);
    assert!(!handled);
    assert!(!mouse_event_requests_redraw(MouseEventKind::Moved, handled));
    assert_eq!(app.trend_inspection, before_move);

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            chart.plot.right() - 1,
            row,
        ),
    ));
    assert_eq!(
        app.trend_inspection,
        Some(TrendInspection {
            panel: TrendPanelId::Remaining,
            at: now,
        })
    );
    let retained = app.trend_inspection;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            chart.plot.right() - 1,
            row,
        ),
    ));
    assert!(app.trend_drag.is_none());
    assert_eq!(app.trend_inspection, retained);
}

#[test]
fn inspected_quarter_hour_bar_shows_exact_tokens_and_bucket_interval() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T09:16:42Z")
        .unwrap()
        .with_timezone(&Utc);
    let exact_tokens = 9_007_199_254_740_993;
    let mut history = trend_history_fixture(now);
    history
        .half_hour_buckets
        .last_mut()
        .unwrap()
        .token_usage
        .total_tokens = exact_tokens;

    for theme in [Theme::Dark, Theme::Light] {
        let mut app = interaction_test_app(1, 1);
        app.theme = theme;
        app.replace_history(history.clone());
        app.set_view(View::Trends);
        app.trend_section = TrendSection::HalfHour;
        let mut terminal = Terminal::new(TestBackend::new(119, 40)).unwrap();
        let content = with_test_display_offset(FixedOffset::east_opt(0).unwrap(), || {
            terminal
                .draw(|frame| render_at(frame, &mut app, now))
                .unwrap();
            handle_key_event(&mut app, key_event(KeyCode::Char('i')));
            terminal
                .draw(|frame| render_at(frame, &mut app, now))
                .unwrap();
            buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, 119, 40))
        });

        assert_eq!(
            app.trend_inspection.map(|inspection| inspection.panel),
            Some(TrendPanelId::LocalTokens)
        );
        assert!(
            content.contains("9,007,199,254,740,993"),
            "exact token readout was lost for {theme:?}:\n{content}"
        );
        assert!(
            content.contains("07-29 09:15–09:30"),
            "15-minute interval was missing for {theme:?}:\n{content}"
        );
        assert!(content.contains("Inspect"), "{content}");
    }
}

#[test]
fn trends_render_recorded_samples_gaps_partial_state_and_day_windows() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    let mut history = trend_history_fixture(now);
    history.warnings.push("fixture warning".to_string());
    history.read_only = true;
    app.replace_history(history);
    app.set_view(View::Trends);

    let bounds = trend_day_bounds(now, 0);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    for expected in [
        "Quota Remaining",
        "Weekly Local Tokens",
        "Weekly ~EST Usage",
        "15m Local Tokens",
        "15m ~EST Usage",
        "samples",
        "gaps",
        "PARTIAL",
        "READ-ONLY",
    ] {
        assert!(content.contains(expected), "missing {expected}: {content}");
    }
    assert!(!content.contains("No history recorded yet"));
    for time in [
        format_local_time(bounds[0], "%m-%d %H:%M"),
        format_local_time(bounds[1], "%m-%d %H:%M"),
    ] {
        assert!(content.contains(&time), "missing time {time}: {content}");
    }

    app.trend_section = TrendSection::HalfHour;
    let mut compact = Terminal::new(TestBackend::new(60, 24)).unwrap();
    compact
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    handle_key_event(&mut app, key_event(KeyCode::Char('[')));
    assert_eq!(app.trend_day_offset, 1);
    compact
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    let previous_day = compact
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(previous_day.contains("No history recorded yet"));
}

#[test]
fn line_trend_readouts_show_exact_values_at_their_real_sample_times() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T09:16:42Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut history = trend_history_fixture(now);
    let weekly_observed_at = now - ChronoDuration::seconds(7);
    history
        .quota_points
        .iter_mut()
        .find(|point| point.duration_mins == 10_080 && point.observed_at == now)
        .unwrap()
        .observed_at = weekly_observed_at;

    let mut app = interaction_test_app(1, 1);
    app.replace_history(history);
    app.set_view(View::Trends);
    let data = prepare_trend_data_at(&app, now);

    assert_eq!(
        data.five_hour_remaining_readout,
        Some(TrendReadout {
            sampled_at: now,
            value: TrendReadoutValue::Percent(60.0),
            interval: None,
            partial: false,
        })
    );
    assert_eq!(
        data.weekly_remaining_readout,
        Some(TrendReadout {
            sampled_at: weekly_observed_at,
            value: TrendReadoutValue::Percent(75.0),
            interval: None,
            partial: false,
        })
    );
    assert_eq!(
        data.weekly_tokens_readout.map(|readout| readout.value),
        Some(TrendReadoutValue::Tokens(6_000))
    );
    assert_eq!(
        data.weekly_estimated_readout.map(|readout| readout.value),
        Some(TrendReadoutValue::Percent(25.0))
    );

    let offset = FixedOffset::east_opt(0).unwrap();
    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let content = with_test_display_offset(offset, || {
            terminal
                .draw(|frame| render_at(frame, &mut app, now))
                .unwrap();
            buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, 120, 40))
        });
        for exact_readout in [
            "5h 60% @ 07-29 09:16:42",
            "Week 75% @ 07-29 09:16:35",
            "Tokens 6,000 @ 07-29 09:16:42",
            "~EST 25% @ 07-29 09:16:42",
        ] {
            assert!(
                content.contains(exact_readout),
                "missing {exact_readout} for {theme:?}:\n{content}"
            );
        }
    }

    let exact_tokens = 9_007_199_254_740_993;
    for bucket in &mut app.history.half_hour_buckets {
        bucket.token_usage.total_tokens = 0;
    }
    app.history
        .half_hour_buckets
        .last_mut()
        .unwrap()
        .token_usage
        .total_tokens = exact_tokens;
    assert_eq!(
        prepare_trend_data_at(&app, now)
            .weekly_tokens_readout
            .map(|readout| readout.value),
        Some(TrendReadoutValue::Tokens(exact_tokens))
    );
}

#[test]
fn half_hour_bar_charts_omit_the_current_value_row() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T09:16:42Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(1, 1);
    app.replace_history(trend_history_fixture(now));
    app.set_view(View::Trends);
    app.trend_section = TrendSection::HalfHour;

    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    let content = with_test_display_offset(FixedOffset::east_opt(0).unwrap(), || {
        terminal
            .draw(|frame| render_at(frame, &mut app, now))
            .unwrap();
        buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, 60, 24))
    });
    assert!(content.contains("15m Local Tokens"), "{content}");
    assert!(content.contains("15m ~EST Usage"), "{content}");
    assert!(content.contains("samples"), "{content}");
    assert!(!content.contains("As of"), "{content}");
    assert!(!content.contains("Tokens 3,000 @"), "{content}");
    assert!(!content.contains("~EST 12.5% @"), "{content}");
}

#[test]
fn fifteen_minute_bars_render_a_full_day_of_96_samples() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let bounds = trend_day_bounds(now, 0);
    let weekly_reset = now + ChronoDuration::days(3);
    let buckets = (0..96)
        .map(|index| {
            let starts_at = bounds[0] + ChronoDuration::minutes(i64::from(index) * 15);
            LocalHalfHourBucket {
                starts_at,
                ends_at: starts_at + ChronoDuration::minutes(15),
                sampled_at: starts_at + ChronoDuration::minutes(15),
                token_usage: TokenUsage {
                    total_tokens: u64::try_from(index + 1).unwrap() * 100,
                    ..TokenUsage::default()
                },
                estimated_cost_units: u128::try_from(index + 1).unwrap(),
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
                project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
                api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
                call_count: 1,
                groups: Vec::new(),
                project_groups: Vec::new(),
                partial_reasons: Vec::new(),
            }
        })
        .collect::<Vec<_>>();
    let mut app = interaction_test_app(1, 1);
    app.history = HistoryData {
        quota_points: vec![QuotaPoint {
            observed_at: now,
            limit_id: "codex".to_string(),
            duration_mins: 10_080,
            resets_at: weekly_reset,
            used_percent: 50.0,
            remaining_percent: 50.0,
            provenance: Provenance::ServerSnapshot,
        }],
        half_hour_buckets: buckets,
        ..HistoryData::default()
    };
    app.set_view(View::Trends);
    app.trend_section = TrendSection::HalfHour;

    let data = prepare_trend_data_at(&app, now);
    assert_eq!(data.half_hour_tokens.len(), 96);
    assert_eq!(data.half_hour_estimated.len(), 96);
    assert_eq!(
        data.half_hour_tokens[0].at,
        bounds[0] + ChronoDuration::minutes(7) + ChronoDuration::seconds(30)
    );

    for (width, height) in [(60, 24), (120, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_at(frame, &mut app, now))
            .unwrap();
        let content = buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, width, height));
        assert!(
            content.contains("15m Local Tokens · 96 samples"),
            "{content}"
        );
        assert!(content.contains("15m ~EST Usage · 96 samples"), "{content}");
    }

    app.history.half_hour_buckets.remove(48);
    let gapped = prepare_trend_data_at(&app, now);
    assert_eq!(gapped.half_hour_tokens.len(), 95);
    assert_eq!(gapped.half_hour_estimated.len(), 95);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    let content = buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, 120, 40));
    assert!(
        content.contains("15m Local Tokens · 95 samples · 1 gaps"),
        "{content}"
    );
    assert!(
        content.contains("15m ~EST Usage · 95 samples · 1 gaps"),
        "{content}"
    );
}

#[test]
fn api_long_context_toggle_reweights_weekly_and_fifteen_minute_estimates_without_changing_tokens() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T11:10:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let weekly_reset = now + ChronoDuration::days(3);
    let call = |timestamp, thread_id: &str, input_tokens| UsageCall {
        timestamp,
        thread_id: thread_id.to_string(),
        turn_id: Some(format!("{thread_id}-turn")),
        usage_event_id: None,
        usage_event_identity_exact: false,
        model: Some("gpt-5.6-luna".to_string()),
        service_tier: None,
        tokens: TokenUsage {
            input_tokens,
            total_tokens: input_tokens,
            ..TokenUsage::default()
        },
        request_usage_exact: true,
    };
    let calls = vec![
        call(now - ChronoDuration::minutes(70), "short", 200_000),
        call(now - ChronoDuration::minutes(40), "long", 300_000),
    ];
    let limits = vec![LimitBucket {
        limit_id: "codex".to_string(),
        limit_name: Some("Codex".to_string()),
        plan_type: Some("test".to_string()),
        primary: Some(LimitWindow::new(40.0, Some(10_080), Some(weekly_reset))),
        secondary: None,
        credits: None,
        rate_limit_reached_type: None,
        provenance: Provenance::ServerSnapshot,
        as_of: now,
    }];
    let observation = HistoryObservation::from_sources(now, &calls, &limits, &[]);
    let mut app = interaction_test_app(1, 1);
    app.replace_history(HistoryData {
        quota_points: observation.quota_points,
        half_hour_buckets: observation.half_hour_buckets,
        weekly_local_points: observation.weekly_local_points,
        ..HistoryData::default()
    });
    app.set_view(View::Trends);

    let off = prepare_trend_data_at(&app, now);
    app.toggle_api_long_context_multiplier();
    let on = prepare_trend_data_at(&app, now);
    app.toggle_api_long_context_multiplier();
    let off_again = prepare_trend_data_at(&app, now);

    let values = |points: &[TrendPoint]| points.iter().map(|point| point.value).collect::<Vec<_>>();
    let token_values = |points: &[TrendPoint]| {
        points
            .iter()
            .map(|point| point.readout_value)
            .collect::<Vec<_>>()
    };
    assert_eq!(values(&off.weekly_estimated), [0.0, 16.0, 40.0, 40.0]);
    assert_eq!(values(&on.weekly_estimated), [0.0, 10.0, 40.0, 40.0]);
    assert_eq!(
        values(&off.half_hour_estimated),
        [16.0, 24.0],
        "base weighting should split the 40% gauge in a 2:3 ratio"
    );
    assert_eq!(
        values(&on.half_hour_estimated),
        [10.0, 30.0],
        "API weighting should double only the verified long request"
    );
    assert_eq!(
        values(&off.weekly_estimated),
        values(&off_again.weekly_estimated)
    );
    assert_eq!(
        values(&off.half_hour_estimated),
        values(&off_again.half_hour_estimated)
    );
    assert_eq!(
        token_values(&off.weekly_tokens),
        [
            TrendReadoutValue::Tokens(0),
            TrendReadoutValue::Tokens(200_000),
            TrendReadoutValue::Tokens(500_000),
            TrendReadoutValue::Tokens(500_000),
        ]
    );
    assert_eq!(
        token_values(&off.weekly_tokens),
        token_values(&on.weekly_tokens)
    );
    assert_eq!(
        token_values(&off.weekly_tokens),
        token_values(&off_again.weekly_tokens)
    );
    assert_eq!(
        token_values(&off.half_hour_tokens),
        [
            TrendReadoutValue::Tokens(200_000),
            TrendReadoutValue::Tokens(300_000),
        ]
    );
    assert_eq!(
        token_values(&off.half_hour_tokens),
        token_values(&on.half_hour_tokens)
    );
    assert_eq!(
        token_values(&off.half_hour_tokens),
        token_values(&off_again.half_hour_tokens)
    );
}

#[test]
fn trend_readout_formatting_keeps_exact_integer_tokens_and_compact_decimals() {
    assert_eq!(format_exact_token_count(0), "0");
    assert_eq!(
        format_exact_token_count(9_007_199_254_740_993),
        "9,007,199,254,740,993"
    );
    assert_eq!(
        format_exact_token_count(u64::MAX),
        "18,446,744,073,709,551,615"
    );
    assert_eq!(
        format_trend_readout_value(TrendReadoutValue::Percent(12.5)),
        "12.5%"
    );
    assert_eq!(
        format_trend_readout_value(TrendReadoutValue::Percent(12.345)),
        "12.35%"
    );
    assert_eq!(
        format_trend_readout_value(TrendReadoutValue::Percent(f64::NAN)),
        "—"
    );
}

#[test]
fn trend_readouts_reject_expired_quota_and_the_synthetic_weekly_anchor() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T09:16:42Z")
        .unwrap()
        .with_timezone(&Utc);
    let weekly_reset = now + ChronoDuration::days(3);
    let mut app = interaction_test_app(1, 1);
    app.history = HistoryData {
        quota_points: vec![
            QuotaPoint {
                observed_at: now - ChronoDuration::minutes(1),
                limit_id: "codex".to_string(),
                duration_mins: 300,
                resets_at: now,
                used_percent: 90.0,
                remaining_percent: 10.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: now,
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: weekly_reset,
                used_percent: 25.0,
                remaining_percent: 75.0,
                provenance: Provenance::ServerSnapshot,
            },
        ],
        ..HistoryData::default()
    };

    let data = prepare_trend_data_at(&app, now);

    assert!(!data.five_hour_remaining.is_empty());
    assert_eq!(data.five_hour_remaining_readout, None);
    assert_eq!(
        data.weekly_remaining_readout.map(|readout| readout.value),
        Some(TrendReadoutValue::Percent(75.0))
    );
    assert_eq!(data.weekly_tokens.len(), 1);
    assert_eq!(data.weekly_tokens_readout, None);
    assert_eq!(data.weekly_estimated_readout, None);
}

#[test]
fn weekly_readout_keeps_a_real_sample_at_the_exact_cycle_start() {
    let weekly_reset = DateTime::parse_from_rfc3339("2026-08-05T09:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let cycle_start = weekly_reset - ChronoDuration::days(7);
    let now = cycle_start + ChronoDuration::minutes(1);
    let mut app = interaction_test_app(1, 1);
    app.history = HistoryData {
        quota_points: vec![QuotaPoint {
            observed_at: now,
            limit_id: "codex".to_string(),
            duration_mins: 10_080,
            resets_at: weekly_reset,
            used_percent: 25.0,
            remaining_percent: 75.0,
            provenance: Provenance::ServerSnapshot,
        }],
        weekly_local_points: vec![WeeklyLocalPoint {
            observed_at: cycle_start,
            resets_at: weekly_reset,
            token_usage: TokenUsage {
                total_tokens: 123,
                ..TokenUsage::default()
            },
            estimated_cost_units: 100,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
            call_count: 1,
            partial_reasons: Vec::new(),
        }],
        ..HistoryData::default()
    };

    let data = prepare_trend_data_at(&app, now);

    assert_eq!(
        data.weekly_tokens_readout,
        Some(TrendReadout {
            sampled_at: cycle_start,
            value: TrendReadoutValue::Tokens(123),
            interval: None,
            partial: false,
        })
    );
    assert_eq!(
        data.weekly_estimated_readout.map(|readout| readout.value),
        Some(TrendReadoutValue::Percent(25.0))
    );

    app.history.quota_points.clear();
    let uncalibrated = prepare_trend_data_at(&app, now);
    assert!(uncalibrated.weekly_history_present);
    assert_eq!(
        uncalibrated
            .weekly_tokens_readout
            .map(|readout| readout.value),
        Some(TrendReadoutValue::Tokens(123))
    );
    assert!(uncalibrated.weekly_estimated.is_empty());
    assert_eq!(uncalibrated.weekly_estimated_readout, None);
}

#[test]
fn latest_local_bucket_window_uses_now_and_15_minute_alignment() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T12:07:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(1, 1);
    app.snapshot.as_of = now - ChronoDuration::hours(6);
    let bucket = |starts_at, total_tokens| LocalHalfHourBucket {
        starts_at,
        ends_at: starts_at + ChronoDuration::minutes(15),
        sampled_at: starts_at + ChronoDuration::minutes(15),
        token_usage: TokenUsage {
            total_tokens,
            ..TokenUsage::default()
        },
        estimated_cost_units: u128::from(total_tokens),
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 1,
        groups: Vec::new(),
        project_groups: Vec::new(),
        partial_reasons: Vec::new(),
    };
    app.history.half_hour_buckets = vec![
        bucket(
            DateTime::parse_from_rfc3339("2026-07-28T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            100,
        ),
        bucket(
            DateTime::parse_from_rfc3339("2026-07-29T11:45:00Z")
                .unwrap()
                .with_timezone(&Utc),
            200,
        ),
    ];

    let data = prepare_trend_data_at(&app, now);

    assert_eq!(data.half_hour_bounds, trend_day_bounds(now, 0));
    assert_eq!(
        data.half_hour_bounds[1],
        DateTime::parse_from_rfc3339("2026-07-29T12:15:00Z")
            .unwrap()
            .with_timezone(&Utc)
    );
    assert_ne!(
        data.half_hour_bounds,
        trend_day_bounds(app.snapshot.as_of, 0)
    );
    assert_eq!(
        data.half_hour_tokens
            .iter()
            .map(|point| point.value)
            .collect::<Vec<_>>(),
        [200.0]
    );
    assert_eq!(
        data.half_hour_tokens[0].at,
        DateTime::parse_from_rfc3339("2026-07-29T11:52:30Z")
            .unwrap()
            .with_timezone(&Utc)
    );
}

#[test]
fn history_view_cutoff_uses_15_minute_alignment() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T12:20:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        history_view_since(now),
        DateTime::parse_from_rfc3339("2026-06-28T12:15:00Z")
            .unwrap()
            .with_timezone(&Utc)
    );
}

fn tui_runtime_test_bucket(starts_at: DateTime<Utc>, tokens: u64) -> LocalHalfHourBucket {
    let ends_at = starts_at + ChronoDuration::minutes(15);
    LocalHalfHourBucket {
        starts_at,
        ends_at,
        sampled_at: ends_at,
        token_usage: TokenUsage {
            input_tokens: tokens,
            total_tokens: tokens,
            ..TokenUsage::default()
        },
        estimated_cost_units: u128::from(tokens),
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
        call_count: 1,
        groups: Vec::new(),
        project_groups: vec![LocalProjectUsageGroup {
            thread_id: "thread-1".to_string(),
            project_id: Some("project-1".to_string()),
            project_label: Some("project".to_string()),
            token_usage: TokenUsage {
                input_tokens: tokens,
                total_tokens: tokens,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(tokens),
            api_equivalent_cost: ApiCostAmount::default(),
            call_count: 1,
            ..LocalProjectUsageGroup::default()
        }],
        partial_reasons: Vec::new(),
    }
}

fn tui_runtime_test_observation(starts_at: DateTime<Utc>, tokens: u64) -> HistoryObservation {
    HistoryObservation {
        observed_at: starts_at + ChronoDuration::minutes(20),
        half_hour_buckets: vec![tui_runtime_test_bucket(starts_at, tokens)],
        ..HistoryObservation::default()
    }
}

#[test]
fn canonical_tui_history_runtime_activates_v2_and_aggregates_remote_history() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut runtime = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 9, 0, 0).unwrap();
    let profile_lease = acquire_tui_history_profile_lease(&runtime).unwrap();
    let warnings = match prepare_tui_history_runtime(
        &mut runtime,
        &profile_lease,
        starts_at + ChronoDuration::minutes(20),
    ) {
        TuiHistoryRuntimePreparation::Ready(warnings) => warnings,
        TuiHistoryRuntimePreparation::LegacyFallback(warnings) => {
            panic!("unexpected legacy fallback: {warnings:?}")
        }
    };
    assert!(
        warnings.is_empty(),
        "unexpected setup warnings: {warnings:?}"
    );
    runtime
        .record_local_observation(
            &tui_runtime_test_observation(starts_at, 10),
            LocalObservationMode::Incremental,
        )
        .unwrap();

    let active = match runtime.ownership().load_manifest().unwrap() {
        OwnershipManifestStatus::Initialized(manifest) => manifest,
        OwnershipManifestStatus::Uninitialized => panic!("ownership must be initialized"),
    };
    assert_eq!(active.state(), HistoryOwnershipState::V2Active);
    let remote_id: NodeId = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
    {
        let lease = runtime.ownership().acquire_writer_lease().unwrap();
        let authority = runtime
            .ownership()
            .authorize_v2_write(&lease, &active)
            .unwrap();
        let writer = runtime.source_history().writer(&authority).unwrap();
        writer
            .save_source_metadata(
                &SourceMetadata::new_with_redaction_profile(
                    remote_id.clone(),
                    SourceKind::Ssh,
                    "remote",
                    runtime.redaction_profile(),
                )
                .unwrap(),
            )
            .unwrap();
        let generation: SourceHistoryRemoteGenerationId =
            "ingest-gen-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .unwrap();
        let one = NonZeroU32::new(1).unwrap();
        let binding = SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: remote_id.clone(),
                generation: NonZeroU64::new(1).unwrap(),
            },
            ProtocolRevisions {
                history_format: one,
                metric: one,
                estimator: one,
                project_breakdown: one,
                api_pricing_catalog: one,
            },
        )
        .unwrap();
        writer
            .ensure_remote_history_generation(
                &remote_id,
                runtime.redaction_profile(),
                &generation,
                &binding,
            )
            .unwrap();
        let mut remote_bucket = tui_runtime_test_bucket(starts_at, 20);
        remote_bucket.project_groups[0].thread_id = "thread-remote".to_owned();
        writer
            .apply_remote_history_generation_page(
                &remote_id,
                runtime.redaction_profile(),
                &generation,
                &binding,
                &[SourceBucketRecord::upsert(1, remote_bucket).unwrap()],
                &[],
            )
            .unwrap();
        writer
            .activate_remote_history_generation(
                &remote_id,
                runtime.redaction_profile(),
                None,
                &generation,
                &binding,
                starts_at + ChronoDuration::minutes(20),
            )
            .unwrap();
        writer.validate().unwrap();
    }

    let mut store = TuiHistoryStore::runtime(runtime, Some(profile_lease), Vec::new());
    let history = store.load_since_with_staged(starts_at - ChronoDuration::hours(1));
    assert_eq!(history.half_hour_buckets.len(), 1);
    assert_eq!(
        history.half_hour_buckets[0].token_usage.total_tokens, 30,
        "the TUI projection must add the persisted local and SSH source slices"
    );
}

#[test]
fn recent_legacy_recorder_defers_tui_v2_cutover() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut runtime = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
    let mut status = RecorderStatusFile::started_with_interval(
        now,
        runtime.legacy_history().namespace().to_string(),
        60,
    );
    status.record_success(now);
    crate::service::write_recorder_status(
        &default_status_file(runtime.legacy_history().history_root().unwrap()),
        &status,
    )
    .unwrap();

    let profile_lease = acquire_tui_history_profile_lease(&runtime).unwrap();
    let warnings = match prepare_tui_history_runtime(&mut runtime, &profile_lease, now) {
        TuiHistoryRuntimePreparation::Ready(warnings) => warnings,
        TuiHistoryRuntimePreparation::LegacyFallback(warnings) => {
            panic!("legacy status should retain the fenced V1 runtime: {warnings:?}")
        }
    };
    assert!(warnings.iter().any(|warning| {
        warning.contains("source-aware history cutover deferred while legacy recorder")
    }));
    let manifest = match runtime.ownership().load_manifest().unwrap() {
        OwnershipManifestStatus::Initialized(manifest) => manifest,
        OwnershipManifestStatus::Uninitialized => panic!("ownership must be initialized"),
    };
    assert_eq!(manifest.state(), HistoryOwnershipState::V1Active);
}

#[test]
fn busy_cooperating_recorder_lock_keeps_tui_on_legacy_without_migrating() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
    let profile_lease = acquire_tui_history_profile_lease(&runtime).unwrap();
    let _recorder_guard =
        match crate::service::try_acquire_recorder_instance_lock(&history_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
        };

    let warnings = match prepare_tui_history_runtime(&mut runtime, &profile_lease, Utc::now()) {
        TuiHistoryRuntimePreparation::LegacyFallback(warnings) => warnings,
        TuiHistoryRuntimePreparation::Ready(warnings) => {
            panic!("busy recorder must force legacy fallback: {warnings:?}")
        }
    };
    assert!(
        warnings
            .iter()
            .any(|warning| { warning.contains("cutover deferred while the recorder is active") })
    );
    assert_eq!(
        runtime.ownership().load_manifest().unwrap(),
        OwnershipManifestStatus::Uninitialized,
        "a busy current recorder must prevent even the start of migration"
    );
}

#[test]
fn same_profile_recorder_on_v2_allows_tui_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
    let profile_lease = acquire_tui_history_profile_lease(&runtime).unwrap();
    let warnings = match prepare_tui_history_runtime(&mut runtime, &profile_lease, Utc::now()) {
        TuiHistoryRuntimePreparation::Ready(warnings) => warnings,
        TuiHistoryRuntimePreparation::LegacyFallback(warnings) => {
            panic!("an active V2 manifest must remain queryable: {warnings:?}")
        }
    };
    assert!(warnings.is_empty());
    let _recorder_profile_lease = acquire_tui_history_profile_lease(&runtime).unwrap();
    let _recorder_guard =
        match crate::service::try_acquire_recorder_instance_lock(&history_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
        };

    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 10, 0, 0).unwrap();
    let mut store = TuiHistoryStore::runtime(runtime, Some(profile_lease), warnings);
    store.stage(&tui_runtime_test_observation(starts_at, 51));
    assert!(matches!(
        store.flush_staged_if_due(Duration::ZERO).unwrap(),
        Some(HistoryRuntimeWriteReport::V2(_))
    ));
    let history = store.load_since_with_staged(starts_at - ChronoDuration::hours(1));
    assert_eq!(history.half_hour_buckets.len(), 1);
    assert_eq!(history.half_hour_buckets[0].token_usage.total_tokens, 51);
}

#[test]
fn same_profile_recorder_does_not_block_tui_reconcile_or_backfill_marker() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
    let profile_lease = acquire_tui_history_profile_lease(&runtime).unwrap();
    let warnings = match prepare_tui_history_runtime(&mut runtime, &profile_lease, Utc::now()) {
        TuiHistoryRuntimePreparation::Ready(warnings) => warnings,
        TuiHistoryRuntimePreparation::LegacyFallback(warnings) => {
            panic!("unexpected legacy fallback: {warnings:?}")
        }
    };
    let mut store = TuiHistoryStore::runtime(runtime, Some(profile_lease), warnings);
    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 11, 0, 0).unwrap();
    let observed_at = starts_at + ChronoDuration::minutes(20);
    store.stage_full_observation(&tui_runtime_test_observation(starts_at, 73));

    let _recorder_profile_lease = acquire_tui_history_profile_lease(match &store.backend {
        TuiHistoryBackend::Runtime(runtime) => runtime,
        TuiHistoryBackend::LegacyFallback(_) => unreachable!(),
    })
    .unwrap();
    let _recorder_guard =
        match crate::service::try_acquire_recorder_instance_lock(&history_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test recorder lock was unexpectedly busy"),
        };
    assert!(matches!(
        store
            .flush_staged_reconcile(starts_at - ChronoDuration::hours(1), observed_at)
            .unwrap(),
        Some(HistoryRuntimeWriteReport::V2(_))
    ));
    let marker = store
        .mark_summary_backfill_attempt(observed_at, true)
        .unwrap();
    assert_eq!(marker.completed_at, observed_at);
    assert!(marker.complete);
}

#[test]
fn tui_revalidates_profile_lease_before_each_runtime_write() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut runtime = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
    let profile_lease = acquire_tui_history_profile_lease(&runtime).unwrap();
    match prepare_tui_history_runtime(&mut runtime, &profile_lease, Utc::now()) {
        TuiHistoryRuntimePreparation::Ready(_) => {}
        TuiHistoryRuntimePreparation::LegacyFallback(warnings) => {
            panic!("unexpected legacy fallback: {warnings:?}")
        }
    }
    let marker = profile_lease
        .lock_path()
        .parent()
        .unwrap()
        .join("active-profile.json");
    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 11, 30, 0).unwrap();
    let mut store = TuiHistoryStore::runtime(runtime, Some(profile_lease), Vec::new());
    store.stage(&tui_runtime_test_observation(starts_at, 19));
    std::fs::remove_file(marker).unwrap();

    let error = store.flush_staged().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(store.profile_lease.is_none());
    let history = store.load_since_with_staged(starts_at - ChronoDuration::hours(1));
    assert!(history.read_only);
    assert!(
        history
            .warnings
            .iter()
            .any(|warning| { warning.contains("profile lease could not be revalidated") })
    );
}

#[test]
fn opposite_profile_keeps_tui_runtime_read_only_without_losing_v2_history() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut preview_runtime =
        HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
    let preview_lease = acquire_tui_history_profile_lease(&preview_runtime).unwrap();
    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
    match prepare_tui_history_runtime(&mut preview_runtime, &preview_lease, Utc::now()) {
        TuiHistoryRuntimePreparation::Ready(_) => {}
        TuiHistoryRuntimePreparation::LegacyFallback(warnings) => {
            panic!("unexpected legacy fallback: {warnings:?}")
        }
    }
    preview_runtime
        .record_local_observation(
            &tui_runtime_test_observation(starts_at, 11),
            LocalObservationMode::Incremental,
        )
        .unwrap();
    drop(preview_lease);

    let redacted_runtime = HistoryRuntime::new(history_root, &codex_home, true).unwrap();
    let _opposite_profile_lease = acquire_tui_history_profile_lease(&redacted_runtime).unwrap();
    let selection_error = acquire_tui_history_profile_lease(&preview_runtime).unwrap_err();
    assert_eq!(selection_error.kind(), io::ErrorKind::WouldBlock);

    let mut store = TuiHistoryStore::runtime(
        preview_runtime,
        None,
        vec![format!(
            "{TUI_HISTORY_PROFILE_BUSY_WARNING}: {selection_error}"
        )],
    );
    store.stage(&tui_runtime_test_observation(starts_at, 99));
    assert!(store.flush_staged().unwrap().is_none());
    let marker_error = store
        .mark_summary_backfill_attempt(starts_at, true)
        .unwrap_err();
    assert_eq!(marker_error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(marker_error.to_string(), TUI_HISTORY_PROFILE_BUSY_WARNING);

    let history = store.load_since_with_staged(starts_at - ChronoDuration::hours(1));
    assert!(history.read_only);
    assert_eq!(history.half_hour_buckets.len(), 1);
    assert_eq!(history.half_hour_buckets[0].token_usage.total_tokens, 11);
    assert!(
        history
            .warnings
            .iter()
            .any(|warning| { warning.starts_with(TUI_HISTORY_PROFILE_BUSY_WARNING) })
    );
}

#[test]
fn legacy_tui_fallback_keeps_live_history_when_persistence_is_unavailable() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("not-a-directory");
    std::fs::write(&history_root, b"occupied").unwrap();
    let legacy = HistoryStore::new(history_root, &codex_home);
    let mut store =
        TuiHistoryStore::legacy_fallback(legacy, vec!["source-aware test fallback".to_string()]);
    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 9, 0, 0).unwrap();

    let mut observation = tui_runtime_test_observation(starts_at, 42);
    observation.quota_points.push(QuotaPoint {
        observed_at: starts_at,
        limit_id: "codex".to_string(),
        duration_mins: 300,
        resets_at: starts_at + ChronoDuration::hours(5),
        used_percent: 20.0,
        remaining_percent: 80.0,
        provenance: Provenance::ServerSnapshot,
    });
    let (history, _) = stage_and_load_history(
        &mut store,
        &observation,
        &[],
        &LocalSessionDigestEvidence::default(),
        starts_at + ChronoDuration::minutes(20),
        &PerfLog::default(),
        true,
    );

    assert_eq!(history.half_hour_buckets.len(), 1);
    assert_eq!(history.half_hour_buckets[0].token_usage.total_tokens, 42);
    assert!(
        history
            .warnings
            .iter()
            .any(|warning| warning.contains("source-aware test fallback"))
    );
    assert!(
        history
            .warnings
            .iter()
            .any(|warning| warning.contains("history persistence failed"))
    );

    let source_id: NodeId = "node-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".parse().unwrap();
    let exact = store.load_since_with_staged_selected(
        &HistorySourceSelection::Remote(source_id),
        starts_at - ChronoDuration::hours(1),
    );
    assert!(exact.history.half_hour_buckets.is_empty());
    assert!(exact.history.weekly_local_points.is_empty());
    assert_eq!(exact.history.quota_points, observation.quota_points);
    assert!(exact.history.read_only);
    assert!(matches!(
        exact.status,
        Some(HistorySourceSelectionStatus::Unavailable(
            HistorySourceUnavailableReason::UnsupportedByLegacy
        ))
    ));
    assert!(
        exact
            .history
            .warnings
            .iter()
            .any(|warning| warning.contains("source-aware test fallback"))
    );
}

#[test]
fn summary_does_not_mix_api_amounts_from_an_outdated_catalog() {
    let amount = ApiCostAmount {
        minimum_pico_usd: crate::domain::PicoUsd::new(10),
        maximum_pico_usd: crate::domain::PicoUsd::new(20),
        observed_samples: 2,
        priced_samples: 2,
        observed_tokens: 300,
        priced_tokens: 300,
    };

    assert_eq!(
        summary_api_cost_for_catalog(amount, crate::api_cost::API_PRICING_CATALOG_REVISION),
        amount
    );
    assert_eq!(
        summary_api_cost_for_catalog(
            amount,
            crate::api_cost::API_PRICING_CATALOG_REVISION.saturating_sub(1),
        ),
        ApiCostAmount {
            observed_samples: 2,
            observed_tokens: 300,
            ..ApiCostAmount::default()
        }
    );
}

#[test]
fn recorder_health_treats_future_heartbeat_as_recent_during_clock_rollback() {
    let now = DateTime::parse_from_rfc3339("2026-07-29T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(1, 1);
    app.snapshot.as_of = now - ChronoDuration::hours(2);
    let mut status = RecorderStatusFile::started_with_interval(
        now - ChronoDuration::hours(1),
        "test-history".to_string(),
        60,
    );
    status.record_success(now - ChronoDuration::minutes(1));
    assert!(
        status.heartbeat_is_recent(app.snapshot.as_of),
        "an apparent future heartbeat is ambiguous clock rollback and must fail closed"
    );
    app.recorder_health.status = Some(status);

    assert!(recorder_panel_status_at(&app, now).starts_with("running "));
    assert!(
        recorder_panel_status_at(&app, now + ChronoDuration::minutes(13)).starts_with("stale ")
    );
}

#[test]
fn half_hour_estimates_merge_cross_reset_cycles_and_restore_older_days() {
    let boundary = DateTime::parse_from_rfc3339("2026-07-23T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let previous_reset = boundary;
    let current_reset = boundary + ChronoDuration::days(7);
    let cross_reset_now = boundary + ChronoDuration::hours(12);
    let bucket = |starts_at, cost_units| LocalHalfHourBucket {
        starts_at,
        ends_at: starts_at + ChronoDuration::minutes(15),
        sampled_at: starts_at + ChronoDuration::minutes(15),
        token_usage: TokenUsage {
            total_tokens: u64::try_from(cost_units).unwrap(),
            ..TokenUsage::default()
        },
        estimated_cost_units: cost_units,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 1,
        groups: Vec::new(),
        project_groups: Vec::new(),
        partial_reasons: Vec::new(),
    };
    let mut app = interaction_test_app(1, 1);
    app.history = HistoryData {
        quota_points: vec![
            QuotaPoint {
                observed_at: boundary - ChronoDuration::hours(2),
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: previous_reset,
                used_percent: 35.0,
                remaining_percent: 65.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: boundary - ChronoDuration::minutes(30),
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: previous_reset,
                used_percent: 40.0,
                remaining_percent: 60.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: boundary + ChronoDuration::hours(1),
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: current_reset,
                used_percent: 10.0,
                remaining_percent: 90.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: cross_reset_now,
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: current_reset,
                used_percent: 20.0,
                remaining_percent: 80.0,
                provenance: Provenance::ServerSnapshot,
            },
        ],
        half_hour_buckets: vec![
            bucket(boundary - ChronoDuration::hours(1), 100),
            bucket(boundary + ChronoDuration::hours(1), 200),
        ],
        ..HistoryData::default()
    };

    let cross_reset = prepare_trend_data_at(&app, cross_reset_now);
    assert_eq!(
        cross_reset
            .half_hour_estimated
            .iter()
            .map(|point| point.value)
            .collect::<Vec<_>>(),
        [40.0, 20.0]
    );
    assert!(
        cross_reset
            .half_hour_estimated
            .iter()
            .all(|point| !point.partial)
    );

    app.trend_day_offset = 1;
    let older_day = prepare_trend_data_at(&app, boundary + ChronoDuration::hours(24));
    assert_eq!(
        older_day
            .half_hour_estimated
            .iter()
            .map(|point| point.value)
            .collect::<Vec<_>>(),
        [40.0]
    );
    assert!(!older_day.half_hour_estimated[0].partial);
}

#[test]
fn overlapping_early_reset_uses_the_new_weekly_cycle_for_15m_estimates() {
    let transition = DateTime::parse_from_rfc3339("2026-07-29T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let now = transition + ChronoDuration::minutes(20);
    let old_reset = transition + ChronoDuration::days(3);
    let new_reset = transition + ChronoDuration::days(7);
    let bucket = |starts_at, sampled_at| LocalHalfHourBucket {
        starts_at,
        ends_at: starts_at + ChronoDuration::minutes(15),
        sampled_at,
        token_usage: TokenUsage {
            total_tokens: 1_000,
            ..TokenUsage::default()
        },
        estimated_cost_units: 100,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 1,
        groups: Vec::new(),
        project_groups: Vec::new(),
        partial_reasons: Vec::new(),
    };
    let mut app = interaction_test_app(1, 1);
    app.history = HistoryData {
        quota_points: vec![
            QuotaPoint {
                observed_at: transition - ChronoDuration::minutes(5),
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: old_reset,
                used_percent: 80.0,
                remaining_percent: 20.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: now,
                limit_id: "codex".to_string(),
                duration_mins: 10_080,
                resets_at: new_reset,
                used_percent: 10.0,
                remaining_percent: 90.0,
                provenance: Provenance::ServerSnapshot,
            },
        ],
        half_hour_buckets: vec![
            bucket(transition - ChronoDuration::minutes(15), transition),
            bucket(transition, transition + ChronoDuration::minutes(15)),
        ],
        ..HistoryData::default()
    };

    let data = prepare_trend_data_at(&app, now);

    assert_eq!(
        data.half_hour_estimated
            .iter()
            .map(|point| point.value)
            .collect::<Vec<_>>(),
        [40.0, 10.0]
    );

    app.history.quota_points.pop();
    app.history.weekly_local_points = vec![WeeklyLocalPoint {
        observed_at: now,
        resets_at: new_reset,
        token_usage: TokenUsage {
            total_tokens: 1_000,
            ..TokenUsage::default()
        },
        estimated_cost_units: 100,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
        call_count: 1,
        partial_reasons: Vec::new(),
    }];
    app.trend_day_offset = 1;
    let viewed_at = now + ChronoDuration::days(1);
    let uncalibrated = prepare_trend_data_at(&app, viewed_at);
    assert_eq!(
        uncalibrated
            .half_hour_estimated
            .iter()
            .map(|point| point.value)
            .collect::<Vec<_>>(),
        [40.0]
    );
}

#[test]
fn weekly_reset_dedup_is_order_independent_for_bridging_candidates() {
    let base_reset = DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let observed_at = base_reset - ChronoDuration::days(1);
    let resets = [
        base_reset,
        base_reset + ChronoDuration::seconds(120),
        base_reset + ChronoDuration::seconds(240),
    ];
    let candidates = resets
        .into_iter()
        .enumerate()
        .map(|(index, resets_at)| QuotaPoint {
            observed_at: observed_at
                + if index == 1 {
                    ChronoDuration::minutes(3)
                } else {
                    ChronoDuration::minutes(i64::try_from(index).unwrap())
                },
            limit_id: "codex".to_string(),
            duration_mins: 10_080,
            resets_at,
            used_percent: 25.0,
            remaining_percent: 75.0,
            provenance: Provenance::ServerSnapshot,
        })
        .collect::<Vec<_>>();
    let bounds = [
        base_reset - ChronoDuration::hours(1),
        base_reset + ChronoDuration::hours(1),
    ];

    for order in [[0, 2, 1], [2, 0, 1], [1, 0, 2], [2, 1, 0]] {
        let history = HistoryData {
            quota_points: order
                .into_iter()
                .map(|index| candidates[index].clone())
                .collect(),
            ..HistoryData::default()
        };

        assert_eq!(
            weekly_resets_overlapping(&history, bounds),
            vec![resets[1]],
            "unexpected representative for input order {order:?}"
        );
    }
}

#[test]
fn weekly_trends_connect_confirmed_zero_plateaus_and_keep_true_gaps() {
    let mut app = interaction_test_app(1, 1);
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let aligned_now = DateTime::from_timestamp(
        app.snapshot.as_of.timestamp().div_euclid(bucket_seconds) * bucket_seconds,
        0,
    )
    .unwrap();
    let reset = aligned_now + ChronoDuration::days(3);
    let start = reset - ChronoDuration::days(7);
    let first_at = start + ChronoDuration::minutes(30);
    let second_at = start + ChronoDuration::minutes(120);
    let zero_bucket = |starts_at| LocalHalfHourBucket {
        starts_at,
        ends_at: starts_at + ChronoDuration::minutes(15),
        sampled_at: starts_at + ChronoDuration::minutes(15),
        token_usage: TokenUsage::default(),
        estimated_cost_units: 0,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 0,
        groups: Vec::new(),
        project_groups: Vec::new(),
        partial_reasons: Vec::new(),
    };
    let history = HistoryData {
        quota_points: vec![QuotaPoint {
            observed_at: second_at,
            limit_id: "codex".to_string(),
            duration_mins: 10_080,
            resets_at: reset,
            used_percent: 40.0,
            remaining_percent: 60.0,
            provenance: Provenance::ServerSnapshot,
        }],
        half_hour_buckets: vec![
            zero_bucket(start + ChronoDuration::minutes(30)),
            zero_bucket(start + ChronoDuration::minutes(45)),
            zero_bucket(start + ChronoDuration::minutes(60)),
        ],
        weekly_local_points: vec![
            WeeklyLocalPoint {
                observed_at: first_at,
                resets_at: reset,
                token_usage: TokenUsage {
                    total_tokens: 10,
                    ..TokenUsage::default()
                },
                estimated_cost_units: 100,
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
                call_count: 1,
                partial_reasons: Vec::new(),
            },
            WeeklyLocalPoint {
                observed_at: second_at,
                resets_at: reset,
                token_usage: TokenUsage {
                    total_tokens: 20,
                    ..TokenUsage::default()
                },
                estimated_cost_units: 200,
                api_long_context_extra_cost_units: Some(0),
                long_context_usage_unknown: false,
                estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
                call_count: 2,
                partial_reasons: Vec::new(),
            },
        ],
        ..HistoryData::default()
    };
    app.replace_history(history.clone());
    app.set_view(View::Trends);
    app.trend_section = TrendSection::Weekly;

    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let connected = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!connected.contains("gaps"), "{connected}");

    let mut missing = history;
    missing.half_hour_buckets.pop();
    app.replace_history(missing);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let gapped = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(gapped.contains("1 gaps"), "{gapped}");
}

#[test]
fn trend_chart_canvas_uses_the_active_theme_background() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut app = interaction_test_app(1, 1);
        let now = app.snapshot.as_of;
        app.theme = theme;
        app.replace_history(trend_history_fixture(now));
        app.set_view(View::Trends);

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer.content().iter().all(|cell| cell.bg != Color::Reset),
            "trend canvas leaked the terminal default background for {theme:?}"
        );
        assert!(
            buffer
                .content()
                .iter()
                .filter(|cell| cell.bg == theme.palette().background)
                .count()
                > buffer.content().len() / 2,
            "trend canvas did not use the {theme:?} background"
        );
        for data_color in [theme.palette().accent, theme.palette().warning] {
            assert!(
                buffer.content().iter().any(|cell| {
                    !cell.symbol().trim().is_empty()
                        && cell.fg == data_color
                        && cell.bg == theme.palette().background
                }),
                "trend marks did not preserve the {theme:?} background"
            );
        }
    }
}

#[test]
fn trend_segments_keep_single_points_and_split_missing_intervals() {
    let start = DateTime::parse_from_rfc3339("2026-07-28T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let points = vec![
        TrendPoint {
            at: start,
            value: 1.0,
            readout_value: TrendReadoutValue::Percent(1.0),
            sampled_at: Some(start),
            interval: None,
            partial: false,
        },
        TrendPoint {
            at: start + ChronoDuration::minutes(5),
            value: 2.0,
            readout_value: TrendReadoutValue::Percent(2.0),
            sampled_at: Some(start + ChronoDuration::minutes(5)),
            interval: None,
            partial: false,
        },
        TrendPoint {
            at: start + ChronoDuration::minutes(25),
            value: 3.0,
            readout_value: TrendReadoutValue::Percent(3.0),
            sampled_at: Some(start + ChronoDuration::minutes(25)),
            interval: None,
            partial: true,
        },
    ];
    let (segments, gaps) = prepare_trend_segments(
        &points,
        TrendGraphKind::Line {
            maximum_gap: ChronoDuration::minutes(10),
        },
    );
    assert_eq!(gaps, 1);
    assert_eq!(segments.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);

    let (single, gaps) = prepare_trend_segments(
        &points[..1],
        TrendGraphKind::Line {
            maximum_gap: ChronoDuration::minutes(10),
        },
    );
    assert_eq!(gaps, 0);
    assert_eq!(single.iter().map(Vec::len).collect::<Vec<_>>(), [1]);

    let (bars, gaps) = prepare_trend_segments(
        &points,
        TrendGraphKind::Bar {
            expected_step: ChronoDuration::minutes(5),
        },
    );
    assert_eq!(gaps, 3);
    assert_eq!(bars[0].len(), 3);
}

#[test]
fn quota_remaining_trends_retain_previous_reset_cycles_and_show_observed_jumps() {
    let boundary = DateTime::parse_from_rfc3339("2026-07-29T04:10:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let quota_point = |duration_mins, observed_at, resets_at, remaining_percent| QuotaPoint {
        observed_at,
        limit_id: "codex".to_string(),
        duration_mins,
        resets_at,
        used_percent: 100.0 - remaining_percent,
        remaining_percent,
        provenance: Provenance::ServerSnapshot,
    };
    let mut app = interaction_test_app(1, 1);
    app.history = HistoryData {
        // Deliberately keep the input out of order so the chart contract does
        // not depend on how a caller assembled HistoryData.
        quota_points: vec![
            quota_point(
                300,
                boundary + ChronoDuration::minutes(5),
                boundary + ChronoDuration::minutes(300),
                99.0,
            ),
            quota_point(
                10_080,
                boundary - ChronoDuration::minutes(5),
                boundary,
                12.0,
            ),
            quota_point(300, boundary - ChronoDuration::minutes(5), boundary, 8.0),
            quota_point(
                10_080,
                boundary + ChronoDuration::minutes(5),
                boundary + ChronoDuration::minutes(10_080),
                100.0,
            ),
        ],
        ..HistoryData::default()
    };

    let data = prepare_trend_data_at(&app, boundary + ChronoDuration::minutes(5));
    assert_eq!(
        data.five_hour_remaining
            .iter()
            .map(|point| point.value)
            .collect::<Vec<_>>(),
        [8.0, 99.0]
    );
    assert_eq!(
        data.weekly_remaining
            .iter()
            .map(|point| point.value)
            .collect::<Vec<_>>(),
        [12.0, 100.0]
    );

    let (segments, gaps) = prepare_trend_segments(
        &data.five_hour_remaining,
        TrendGraphKind::Line {
            maximum_gap: ChronoDuration::minutes(15),
        },
    );
    assert_eq!(gaps, 0);
    assert_eq!(segments.len(), 1);
    assert_eq!(
        segments[0]
            .iter()
            .map(|(_, value)| *value)
            .collect::<Vec<_>>(),
        [8.0, 99.0]
    );
}

#[test]
fn quota_remaining_reset_does_not_bridge_a_recorder_outage() {
    let boundary = DateTime::parse_from_rfc3339("2026-07-29T04:10:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let history = HistoryData {
        quota_points: vec![
            QuotaPoint {
                observed_at: boundary - ChronoDuration::minutes(10),
                limit_id: "codex".to_string(),
                duration_mins: 300,
                resets_at: boundary,
                used_percent: 94.0,
                remaining_percent: 6.0,
                provenance: Provenance::ServerSnapshot,
            },
            QuotaPoint {
                observed_at: boundary + ChronoDuration::minutes(20),
                limit_id: "codex".to_string(),
                duration_mins: 300,
                resets_at: boundary + ChronoDuration::minutes(300),
                used_percent: 3.0,
                remaining_percent: 97.0,
                provenance: Provenance::ServerSnapshot,
            },
        ],
        ..HistoryData::default()
    };
    let points = remaining_trend(&history, 300);

    let (segments, gaps) = prepare_trend_segments(
        &points,
        TrendGraphKind::Line {
            maximum_gap: ChronoDuration::minutes(15),
        },
    );
    assert_eq!(points.len(), 2);
    assert_eq!(gaps, 1);
    assert_eq!(segments.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1]);
}

#[test]
fn trends_controls_clip_only_whole_buttons_in_tiny_terminals() {
    for (width, height) in [(40, 12), (20, 8), (8, 3)] {
        let mut app = interaction_test_app(1, 1);
        app.set_view(View::Trends);
        app.trend_section = TrendSection::HalfHour;
        app.trend_day_offset = 2;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let controls = app.trend_controls_hitbox.expect("trend controls");
        for control in controls.sections.into_iter().chain([
            controls.inspect,
            controls.previous_day,
            controls.next_day,
            controls.now,
        ]) {
            if !control.is_empty() {
                assert_eq!(control.width, 3);
                assert!(control.right() <= width);
                assert!(control.bottom() <= height);
            }
        }
        if controls.now.is_empty() {
            handle_key_event(&mut app, key_event(KeyCode::Char('N')));
            assert_eq!(app.trend_day_offset, 2);
        }
    }
}

fn project_mapping_observation(
    source_digit: char,
    project_digit: char,
    label: Option<&str>,
) -> crate::project_mapping::ProjectObservation {
    let source_id: NodeId = format!("node-{}", source_digit.to_string().repeat(32))
        .parse()
        .unwrap();
    let observed_project_key = format!(
        "opk-hmac-sha256-v1-{}",
        project_digit.to_string().repeat(64)
    )
    .parse()
    .unwrap();
    crate::project_mapping::ProjectObservation::new(
        crate::project_mapping::SourceObservedProject::new(source_id, observed_project_key),
    )
    .with_display_label(label.map(|label| label.parse().unwrap()))
    .with_git_evidence(
        Some(
            format!("git-sha256-v1-{}", "a".repeat(64))
                .parse::<GitRepositoryFingerprint>()
                .unwrap(),
        ),
        Some("workspace".to_owned()),
    )
    .unwrap()
}

fn project_mapping_observation_without_git(
    source_digit: char,
    project_digit: char,
    label: &str,
) -> crate::project_mapping::ProjectObservation {
    let source_id: NodeId = format!("node-{}", source_digit.to_string().repeat(32))
        .parse()
        .unwrap();
    let observed_project_key = format!(
        "opk-hmac-sha256-v1-{}",
        project_digit.to_string().repeat(64)
    )
    .parse()
    .unwrap();
    crate::project_mapping::ProjectObservation::new(
        crate::project_mapping::SourceObservedProject::new(source_id, observed_project_key),
    )
    .with_display_label(Some(label.parse().unwrap()))
}

fn install_project_mapping_fixture(app: &mut App, directory: &Path) -> ProjectMappingStore {
    let store = ProjectMappingStore::new(directory.join("config/project-mappings.json"));
    let initial = store.load_or_create().unwrap();
    let first = store
        .resolve_or_create(
            initial.revision(),
            project_mapping_observation('1', '1', Some("shared-project")),
        )
        .unwrap();
    store
        .resolve_or_create(
            first.mappings().revision(),
            project_mapping_observation('2', '2', Some("shared-project")),
        )
        .unwrap();
    app.project_mapping_store = store.clone();
    app.reload_project_mappings();
    store
}

#[test]
fn settings_project_mappings_render_and_keyboard_actions_use_explicit_cas() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let store = install_project_mapping_fixture(&mut app, directory.path());
    app.view = View::Settings;
    app.selected_setting = app.project_mapping_selection_base();

    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        for (width, height) in [(60, 14), (100, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(content.contains("Project mappings"));
            assert!(content.contains("SUG"));
            assert!(content.contains("shared-project"));
            let controls = app.settings_controls_hitbox.as_ref().unwrap();
            assert!(controls.project_accept_enabled);
            assert!(!controls.project_accept.is_empty());
            let shortcut = &terminal.backend().buffer()
                [(controls.project_accept.x + 1, controls.project_accept.y)];
            assert_eq!(shortcut.symbol(), "J");
            assert_eq!(shortcut.fg, theme.palette().accent);
            assert!(shortcut.modifier.contains(Modifier::BOLD));
        }
    }

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('j'))));
    let merged = store.load().unwrap();
    assert_eq!(merged.logical_projects().len(), 1);
    assert_eq!(
        merged.logical_projects()[0].display_label().as_str(),
        "shared-project"
    );
    assert!(merged.merge_suggestions().is_empty());
    assert!(
        app.project_mappings
            .status
            .as_deref()
            .is_some_and(|status| { status.contains("Merged 2 instances as shared-project") })
    );

    let logical_index = app
        .project_mappings
        .rows
        .iter()
        .position(|row| matches!(row, ProjectMappingSettingsRow::LogicalProject { .. }))
        .unwrap();
    app.selected_setting = app.project_mapping_selection_base() + logical_index;
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(
        app.settings_controls_hitbox
            .as_ref()
            .unwrap()
            .project_split_enabled
    );
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('x'))));
    assert!(store.load().unwrap().logical_projects().is_empty());
}

#[test]
fn settings_project_mapping_rows_and_actions_are_whole_label_clickable() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let store = install_project_mapping_fixture(&mut app, directory.path());
    app.view = View::Settings;
    app.selected_setting = 0;
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let controls = app.settings_controls_hitbox.clone().unwrap();
    let suggestion_row = controls.project_rows[0];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            suggestion_row.right() - 1,
            suggestion_row.y,
        ),
    ));
    assert_eq!(app.selected_setting, app.project_mapping_selection_base());
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let accept = app
        .settings_controls_hitbox
        .as_ref()
        .unwrap()
        .project_accept;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            accept.right() - 1,
            accept.y,
        ),
    ));
    assert_eq!(store.load().unwrap().logical_projects().len(), 1);

    let instance_index = app
        .project_mappings
        .rows
        .iter()
        .position(|row| matches!(row, ProjectMappingSettingsRow::MappedInstance { .. }))
        .unwrap();
    app.selected_setting = app.project_mapping_selection_base() + instance_index;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let split = app.settings_controls_hitbox.as_ref().unwrap().project_split;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            split.right() - 1,
            split.y,
        ),
    ));
    let mappings = store.load().unwrap();
    assert_eq!(mappings.logical_projects().len(), 1);
    assert_eq!(
        mappings
            .instances()
            .iter()
            .filter(|instance| instance.logical_project_id().is_some())
            .count(),
        1
    );
}

#[test]
fn settings_unmapped_projects_support_explicit_multi_select_merge_by_keyboard_and_mouse() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProjectMappingStore::new(directory.path().join("config/project-mappings.json"));
    let initial = store.load_or_create().unwrap();
    let first = store
        .resolve_or_create(
            initial.revision(),
            project_mapping_observation_without_git('1', '1', "manual-project"),
        )
        .unwrap();
    store
        .resolve_or_create(
            first.mappings().revision(),
            project_mapping_observation_without_git('2', '2', "manual-project"),
        )
        .unwrap();

    let mut app = interaction_test_app(0, 0);
    app.project_mapping_store = store.clone();
    app.reload_project_mappings();
    app.view = View::Settings;
    assert_eq!(app.project_mappings.rows.len(), 2);
    assert!(
        app.project_mappings
            .rows
            .iter()
            .all(|row| matches!(row, ProjectMappingSettingsRow::UnmappedInstance { .. }))
    );

    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        app.selected_setting = app.project_mapping_selection_base();
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("FREE"));
        let controls = app.settings_controls_hitbox.as_ref().unwrap();
        assert!(controls.project_toggle_enabled);
        assert!(!controls.project_toggle.is_empty());
        let shortcut = &terminal.backend().buffer()
            [(controls.project_toggle.x + 1, controls.project_toggle.y)];
        assert_eq!(shortcut.symbol(), "T");
        assert_eq!(shortcut.fg, theme.palette().accent);
        assert!(shortcut.modifier.contains(Modifier::BOLD));
    }

    app.theme = Theme::Dark;
    app.selected_setting = app.project_mapping_selection_base();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('t'))));
    assert_eq!(app.project_mappings.selected_instances.len(), 1);

    app.selected_setting += 1;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let toggle = app
        .settings_controls_hitbox
        .as_ref()
        .unwrap()
        .project_toggle;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            toggle.right() - 1,
            toggle.y,
        ),
    ));
    assert_eq!(app.project_mappings.selected_instances.len(), 2);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let controls = app.settings_controls_hitbox.as_ref().unwrap();
    assert!(controls.project_merge_enabled);
    let merge = controls.project_merge;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            merge.right() - 1,
            merge.y,
        ),
    ));
    let mappings = store.load().unwrap();
    assert_eq!(mappings.logical_projects().len(), 1);
    assert_eq!(
        mappings.logical_projects()[0].display_label().as_str(),
        "manual-project"
    );
    assert_eq!(
        mappings
            .instances()
            .iter()
            .filter(|instance| instance.logical_project_id().is_some())
            .count(),
        2
    );

    // Re-open the same explicit mapping so the visible M binding is also
    // exercised through the keyboard (the first merge used its whole mouse
    // hitbox above).
    let instance_ids = mappings
        .instances()
        .iter()
        .map(|instance| instance.instance_id().clone())
        .collect::<Vec<_>>();
    store
        .split_instances(mappings.revision(), &instance_ids)
        .unwrap();
    app.reload_project_mappings();
    app.selected_setting = app.project_mapping_selection_base();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('t'))));
    app.selected_setting += 1;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let toggle = app
        .settings_controls_hitbox
        .as_ref()
        .unwrap()
        .project_toggle;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            toggle.right() - 1,
            toggle.y,
        ),
    ));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('m'))));
    assert_eq!(store.load().unwrap().logical_projects().len(), 1);
}

#[test]
fn settings_project_mapping_stale_revision_reloads_without_merging() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let store = install_project_mapping_fixture(&mut app, directory.path());
    app.view = View::Settings;
    app.selected_setting = app.project_mapping_selection_base();
    let stale_revision = app.project_mappings.mappings.as_ref().unwrap().revision();
    store
        .resolve_or_create(stale_revision, project_mapping_observation('3', '3', None))
        .unwrap();

    app.accept_selected_project_merge();
    assert!(store.load().unwrap().logical_projects().is_empty());
    assert_eq!(
        app.project_mappings.mappings.as_ref().unwrap().revision(),
        stale_revision + 1
    );
    assert_eq!(
        app.project_mappings.status.as_deref(),
        Some("Project merge failed (busy)")
    );
}

#[test]
fn settings_project_mapping_merge_without_labels_uses_a_stable_short_instance_id() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let store = ProjectMappingStore::new(directory.path().join("config/project-mappings.json"));
    let initial = store.load_or_create().unwrap();
    let first = store
        .resolve_or_create(
            initial.revision(),
            project_mapping_observation('1', '1', None),
        )
        .unwrap();
    store
        .resolve_or_create(
            first.mappings().revision(),
            project_mapping_observation('2', '2', None),
        )
        .unwrap();
    app.project_mapping_store = store.clone();
    app.reload_project_mappings();
    app.selected_setting = app.project_mapping_selection_base();
    let proposed = match app.selected_project_mapping_row().unwrap() {
        ProjectMappingSettingsRow::Suggestion { proposed_label, .. } => {
            proposed_label.as_str().to_owned()
        }
        _ => unreachable!(),
    };
    assert_eq!(proposed.chars().count(), 8);
    assert!(
        proposed
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );

    app.accept_selected_project_merge();
    assert_eq!(
        store.load().unwrap().logical_projects()[0]
            .display_label()
            .as_str(),
        proposed
    );
}

#[test]
fn settings_project_mapping_load_errors_are_sanitized() {
    let directory = tempfile::tempdir().unwrap();
    let blocked = directory.path().join("private-project-path");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let mut app = interaction_test_app(0, 0);
    app.project_mapping_store = ProjectMappingStore::new(blocked.join("project-mappings.json"));
    app.reload_project_mappings();
    let error = app.project_mappings.error.as_deref().unwrap();
    assert!(error.starts_with("local-state/"));
    assert!(!error.contains("private-project-path"));

    app.view = View::Settings;
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("local-state/"));
    assert!(!content.contains("private-project-path"));
}

#[test]
fn settings_remote_sources_render_across_compact_light_and_dark_layouts() {
    let directory = tempfile::tempdir().unwrap();
    let now = DateTime::parse_from_rfc3339("2026-08-31T08:15:30Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), now);
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();

    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        for (width, height) in [(60, 12), (100, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(content.contains("Remote sources"));
            assert!(content.contains("Automatic sync: On"));
            assert!(content.contains("dev"));
            assert!(content.contains("yes"));
            assert!(content.contains("redacted"));
            assert!(content.contains("Budget 0 B · OK"));

            let controls = app.settings_controls_hitbox.as_ref().unwrap();
            for (area, shortcut) in [
                (controls.remote_global, "G"),
                (controls.remote_new, "N"),
                (controls.remote_edit, "E"),
                (controls.remote_remove, "D"),
                (controls.remote_enable, "H"),
                (controls.remote_test, "C"),
                (controls.remote_sync, "S"),
            ] {
                assert!(!area.is_empty(), "missing {shortcut} at {width}x{height}");
                let cell = &terminal.backend().buffer()[(area.x + 1, area.y)];
                assert_eq!(cell.symbol(), shortcut);
                assert_eq!(cell.fg, theme.palette().accent);
                assert!(cell.modifier.contains(Modifier::BOLD));
            }
        }
    }
}

#[test]
fn empty_remote_panel_is_keyboard_and_mouse_reachable_in_wide_and_compact_layouts() {
    for (width, height) in [(60, 12), (100, 24)] {
        let directory = tempfile::tempdir().unwrap();
        let mut app = interaction_test_app(0, 0);
        let config_store = install_empty_remote_sources_fixture(&mut app, directory.path());
        app.view = View::Settings;
        app.selected_setting = SettingItem::ALL.len() - 1;

        // The synthetic empty-panel position is reachable through ordinary
        // Settings navigation; no invisible global shortcut is required.
        assert!(!handle_key_event(&mut app, key_event(KeyCode::Down)));
        assert_eq!(app.selected_setting, SettingItem::ALL.len());

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let controls = app.settings_controls_hitbox.clone().unwrap();
        for (area, shortcut) in [(controls.remote_global, "G"), (controls.remote_new, "N")] {
            assert!(!area.is_empty(), "missing {shortcut} at {width}x{height}");
            let shortcut_cell = &terminal.backend().buffer()[(area.x + 1, area.y)];
            assert_eq!(shortcut_cell.symbol(), shortcut);
            assert_eq!(shortcut_cell.fg, app.theme.palette().accent);
            assert!(shortcut_cell.modifier.contains(Modifier::BOLD));
        }

        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('g'))));
        assert!(config_store.load().unwrap().auto_sync_enabled());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('n'))));
        assert!(app.remote_editor.is_some());

        // A fresh app proves the complete visible labels remain clickable,
        // including the compact `[G]` / `[N]` forms.
        let mouse_directory = tempfile::tempdir().unwrap();
        let mut mouse_app = interaction_test_app(0, 0);
        let mouse_store =
            install_empty_remote_sources_fixture(&mut mouse_app, mouse_directory.path());
        mouse_app.view = View::Settings;
        mouse_app.selected_setting = SettingItem::ALL.len();
        let mut mouse_terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        mouse_terminal
            .draw(|frame| render(frame, &mut mouse_app))
            .unwrap();
        let mouse_controls = mouse_app.settings_controls_hitbox.clone().unwrap();
        assert!(handle_mouse_event(
            &mut mouse_app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                mouse_controls.remote_global.right() - 1,
                mouse_controls.remote_global.y,
            ),
        ));
        assert!(mouse_store.load().unwrap().auto_sync_enabled());
        mouse_terminal
            .draw(|frame| render(frame, &mut mouse_app))
            .unwrap();
        let new_area = mouse_app
            .settings_controls_hitbox
            .as_ref()
            .unwrap()
            .remote_new;
        assert!(handle_mouse_event(
            &mut mouse_app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                new_area.right() - 1,
                new_area.y,
            ),
        ));
        assert!(mouse_app.remote_editor.is_some());
    }
}

#[cfg(unix)]
#[test]
fn dropping_running_remote_action_worker_terminates_and_reaps_its_process_tree() {
    let directory = tempfile::tempdir().unwrap();
    let primary_pid_file = directory.path().join("primary.pid");
    let descendant_pid_file = directory.path().join("descendant.pid");
    let mut command = Command::new("sh");
    command
        .env("PRIMARY_PID_FILE", &primary_pid_file)
        .env("DESCENDANT_PID_FILE", &descendant_pid_file)
        .args([
            "-c",
            "echo $$ > \"$PRIMARY_PID_FILE\"; sleep 30 & echo $! > \"$DESCENDANT_PID_FILE\"; wait",
        ]);

    let cancellation = RemoteActionCancellation::default();
    let worker_cancellation = cancellation.clone();
    let worker = thread::spawn(move || {
        let _ = run_cancellable_remote_action_command(command, &worker_cancellation);
    });
    let guard = RemoteActionWorkerGuard {
        cancellation,
        worker: Some(worker),
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    while (!primary_pid_file.is_file() || !descendant_pid_file.is_file())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    let primary_pid: libc::pid_t = std::fs::read_to_string(&primary_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let descendant_pid: libc::pid_t = std::fs::read_to_string(&descendant_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(primary_pid, 0) }, 0);
    assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, 0);

    // This is the same Drop path taken when the TUI run loop returns on q,
    // Ctrl-C, a signal, or an input error.
    drop(guard);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && (unsafe { libc::kill(primary_pid, 0) } == 0
            || unsafe { libc::kill(descendant_pid, 0) } == 0)
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(primary_pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[cfg(unix)]
#[test]
fn real_isolated_tui_helper_accepts_its_parent_process_tree_contract() {
    const PROBE_MODE_ENV: &str = "CODEX_USAGE_MONIT_TEST_TUI_HELPER_CONTRACT_PROBE";
    const PROBE_TOKEN_ENV: &str = "CODEX_USAGE_MONIT_TEST_TUI_HELPER_REQUESTED_TOKEN";

    if std::env::var_os(PROBE_MODE_ENV).is_some() {
        let requested = std::env::var(PROBE_TOKEN_ENV).unwrap();
        assert!(
            crate::remote_transport::tui_process_tree_inheritance_is_authorized(Some(&requested)),
            "the real child should observe its TUI parent, fresh process group, and matching capability"
        );
        return;
    }

    let executable = std::env::current_exe().unwrap();
    let contract = TuiProcessTreeInheritanceContract::generate().unwrap();
    let mut command = Command::new(executable);
    command
        .args([
            "--exact",
            "tui::tests::real_isolated_tui_helper_accepts_its_parent_process_tree_contract",
            "--nocapture",
        ])
        .env(PROBE_MODE_ENV, "1")
        .env(PROBE_TOKEN_ENV, contract.token_for_test());
    contract.apply_environment_for_test(&mut command);

    let output =
        run_cancellable_remote_action_command(command, &RemoteActionCancellation::default())
            .unwrap();
    assert!(
        output.status.success(),
        "real helper contract probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn remote_ui_helper_inherits_the_outer_cancellation_tree() {
    let config = CollectConfig {
        codex_home: PathBuf::from("/tmp/codex-home"),
        ..CollectConfig::default()
    };
    let request = RemoteUiActionRequest {
        kind: RemoteUiActionKind::Sync,
        host_id: "devbox".to_owned(),
        config_revision: 7,
    };

    let command =
        remote_ui_action_command(Path::new("/tmp/codex-usage-monit"), &config, &request).unwrap();
    let args = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        args.first().map(String::as_str),
        Some("--inherit-remote-process-tree")
    );
    let capability = args.get(1).expect("the TUI helper capability is missing");
    assert_eq!(capability.len(), 64);
    let environment = command
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(ToOwned::to_owned),
            )
        })
        .collect::<HashMap<_, _>>();
    let environment_capability = environment
        .get("CODEX_USAGE_MONIT_TUI_PROCESS_TREE_TOKEN")
        .and_then(Option::as_deref);
    let environment_parent = environment
        .get("CODEX_USAGE_MONIT_TUI_PROCESS_TREE_PARENT")
        .and_then(Option::as_deref);
    assert!(
        crate::remote_transport::validate_tui_process_tree_inheritance(
            Some(capability),
            environment_capability,
            environment_parent,
            Some(std::process::id()),
            true,
        )
    );
    assert!(args.windows(2).any(|pair| pair == ["remote", "sync"]));
    assert!(args.windows(2).any(|pair| pair == ["sync", "devbox"]));
}

#[test]
fn settings_shortcuts_are_isolated_to_the_selected_panel_in_wide_and_compact_layouts() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    install_project_mapping_fixture(&mut app, directory.path());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();

    let display_before = (
        app.theme,
        app.turns_default_visible,
        app.models_visible,
        app.api_long_context_multiplier,
        app.table_columns,
    );
    let config_before = config_store.load().unwrap();
    let mut compact = Terminal::new(TestBackend::new(60, 12)).unwrap();
    compact.draw(|frame| render(frame, &mut app)).unwrap();
    let compact_controls = app.settings_controls_hitbox.clone().unwrap();
    assert!(!compact_controls.remote_edit.is_empty());
    assert_eq!(
        compact.backend().buffer()[(
            compact_controls.remote_edit.x + 1,
            compact_controls.remote_edit.y
        )]
            .fg,
        app.theme.palette().accent
    );
    for item in SettingItem::ALL {
        let area = compact_controls.rows[item.index()];
        if !area.is_empty() {
            assert_eq!(
                compact.backend().buffer()[(area.x + 3, area.y)].fg,
                app.theme.palette().muted,
                "main shortcut {} stayed active while a remote row had focus",
                item.shortcut()
            );
        }
    }

    for code in [
        KeyCode::Char('t'),
        KeyCode::Char('v'),
        KeyCode::Char('m'),
        KeyCode::Char('l'),
        KeyCode::Char('k'),
        KeyCode::Char('p'),
        KeyCode::Char('a'),
        KeyCode::Enter,
        KeyCode::Char(' '),
    ] {
        assert!(!handle_key_event(&mut app, key_event(code)));
    }
    assert_eq!(
        (
            app.theme,
            app.turns_default_visible,
            app.models_visible,
            app.api_long_context_multiplier,
            app.table_columns,
        ),
        display_before
    );
    assert_eq!(
        config_store.load().unwrap().config_revision(),
        config_before.config_revision(),
        "Enter and Space must not hide an enable/include action"
    );
    assert_eq!(
        config_store
            .load()
            .unwrap()
            .host("dev")
            .unwrap()
            .sync_enabled(),
        config_before.host("dev").unwrap().sync_enabled()
    );

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('e'))));
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.mode),
        Some(RemoteEditorMode::Edit),
        "E must remain the visible remote Edit binding"
    );
    assert_eq!(
        app.table_columns.estimated_quota,
        display_before.4.estimated_quota
    );
    app.cancel_remote_editor();

    app.selected_setting = app.project_mapping_selection_base();
    let mut wide = Terminal::new(TestBackend::new(100, 24)).unwrap();
    wide.draw(|frame| render(frame, &mut app)).unwrap();
    let wide_controls = app.settings_controls_hitbox.clone().unwrap();
    for item in SettingItem::ALL {
        let area = wide_controls.rows[item.index()];
        assert!(!area.is_empty());
        assert_eq!(
            wide.backend().buffer()[(area.x + 3, area.y)].fg,
            app.theme.palette().muted,
            "main shortcut {} stayed active while a project row had focus",
            item.shortcut()
        );
    }
    assert_eq!(
        wide.backend().buffer()[(
            wide_controls.remote_global.x + 1,
            wide_controls.remote_global.y
        )]
            .fg,
        app.theme.palette().muted,
        "remote keyboard controls must be inactive outside the remote panel"
    );
    assert_eq!(
        wide.backend().buffer()[(
            wide_controls.project_accept.x + 1,
            wide_controls.project_accept.y
        )]
            .fg,
        app.theme.palette().accent
    );

    for shortcut in ['t', 'v', 'm', 'l', 'k', 'p', 'e', 'a', 'g', 'n'] {
        assert!(!handle_key_event(
            &mut app,
            key_event(KeyCode::Char(shortcut))
        ));
    }
    assert_eq!(
        (
            app.theme,
            app.turns_default_visible,
            app.models_visible,
            app.api_long_context_multiplier,
            app.table_columns,
        ),
        display_before
    );
    assert!(app.remote_editor.is_none());
    assert!(app.pending_remote_action.is_none());
    assert_eq!(
        config_store.load().unwrap().config_revision(),
        config_before.config_revision()
    );

    // Keyboard focus only affects the shortcut binding. The entire rendered
    // control remains a valid mouse target, including its non-shortcut tail.
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            wide_controls.remote_global.right() - 1,
            wide_controls.remote_global.y,
        ),
    ));
    assert!(!config_store.load().unwrap().auto_sync_enabled());
    assert_eq!(app.selected_setting, app.project_mapping_selection_base());
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('g'))));
    assert!(!config_store.load().unwrap().auto_sync_enabled());

    let api_row = wide_controls.rows[SettingItem::ApiEquivalent.index()];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            api_row.right() - 1,
            api_row.y,
        ),
    ));
    assert_ne!(
        app.table_columns.api_equivalent,
        display_before.4.api_equivalent
    );
}

#[test]
fn settings_remote_source_policy_is_distinct_clickable_and_uses_offline_cli() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    let attached_id: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
    let attached = SourceMetadata::new_with_redaction_profile(
        attached_id,
        SourceKind::Ssh,
        "dev",
        crate::source_history::RedactionProfile::Redacted,
    )
    .unwrap();
    let detached_id: NodeId = "node-22222222222222222222222222222222".parse().unwrap();
    let mut detached = SourceMetadata::new_with_redaction_profile(
        detached_id.clone(),
        SourceKind::Ssh,
        "archived lab",
        crate::source_history::RedactionProfile::Redacted,
    )
    .unwrap();
    detached.set_detached(true);
    detached.set_include_in_aggregates(false);
    let history_store = SourceHistoryStore::new(
        directory.path().join("source-history"),
        "0123456789abcdef".parse().unwrap(),
    );
    history_store.save_source_metadata(&attached).unwrap();
    history_store.save_source_metadata(&detached).unwrap();
    app.remote_source_history_store = Some(history_store);
    app.reload_remote_sources_with_history(true);
    assert_eq!(app.remote_sources.history_sources.len(), 2);
    app.view = View::Settings;
    app.selected_setting =
        SettingItem::ALL.len() + app.remote_sources.config.as_ref().unwrap().hosts().len();

    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        for (width, height) in [(60, 14), (100, 24)] {
            let mut render_terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            render_terminal
                .draw(|frame| render(frame, &mut app))
                .unwrap();
            let controls = app.settings_controls_hitbox.as_ref().unwrap();
            assert!(controls.remote_purge_enabled);
            assert!(!controls.remote_purge.is_empty());
            assert!(controls.remote_pair.is_empty());
            let shortcut = &render_terminal.backend().buffer()
                [(controls.remote_purge.x + 1, controls.remote_purge.y)];
            assert_eq!(shortcut.symbol(), "P");
            assert_eq!(shortcut.fg, theme.palette().accent);
            assert!(shortcut.modifier.contains(Modifier::BOLD));
        }
    }
    app.theme = Theme::Light;

    let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("archived"));
    assert!(content.contains("detached"));
    assert!(content.contains("AGG"));

    let controls = app.settings_controls_hitbox.clone().unwrap();
    assert!(controls.remote_include_enabled);
    assert!(!controls.remote_include.is_empty());
    assert!(controls.remote_purge_enabled);
    assert!(!controls.remote_purge.is_empty());
    assert!(controls.remote_pair.is_empty());
    let shortcut =
        &terminal.backend().buffer()[(controls.remote_include.x + 1, controls.remote_include.y)];
    assert_eq!(shortcut.symbol(), "I");
    assert_eq!(shortcut.fg, app.theme.palette().accent);
    assert!(shortcut.modifier.contains(Modifier::BOLD));

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('i'))));
    let request = app.pending_remote_action.clone().unwrap();
    assert_eq!(request.kind, RemoteUiActionKind::Include);
    assert_eq!(request.host_id, detached_id.as_str());
    let mut command = Command::new("codex-usage-monit");
    append_remote_ui_action_args(&mut command, &request);
    assert_eq!(
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        vec![
            "remote".to_owned(),
            "source".to_owned(),
            "include".to_owned(),
            detached_id.to_string(),
        ]
    );
    assert_eq!(config_store.load().unwrap().hosts().len(), 2);

    app.pending_remote_action = None;
    app.remote_action_running = None;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let include = app
        .settings_controls_hitbox
        .as_ref()
        .unwrap()
        .remote_include;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            include.right() - 1,
            include.y,
        ),
    ));
    assert_eq!(
        app.pending_remote_action
            .as_ref()
            .map(|request| &request.kind),
        Some(&RemoteUiActionKind::Include)
    );

    app.pending_remote_action = None;
    app.remote_action_running = None;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('p'))));
    assert_eq!(
        app.remote_purge_confirmation
            .as_ref()
            .map(|confirmation| &confirmation.source_id),
        Some(&detached_id)
    );
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let popup_content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(popup_content.contains("Purge retained source history?"));
    let confirmation = app.remote_purge_confirmation_hitbox.unwrap();
    assert!(!confirmation.confirm.is_empty());
    assert!(!confirmation.cancel.is_empty());
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            confirmation.cancel.right() - 1,
            confirmation.cancel.y,
        ),
    ));
    assert!(app.remote_purge_confirmation.is_none());

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let purge = app.settings_controls_hitbox.as_ref().unwrap().remote_purge;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            purge.right() - 1,
            purge.y,
        ),
    ));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let confirmation = app.remote_purge_confirmation_hitbox.unwrap();
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            confirmation.confirm.right() - 1,
            confirmation.confirm.y,
        ),
    ));
    let request = app.pending_remote_action.as_ref().unwrap();
    assert_eq!(request.kind, RemoteUiActionKind::Purge);
    assert_eq!(request.host_id, detached_id.as_str());
    let mut command = Command::new("codex-usage-monit");
    append_remote_ui_action_args(&mut command, request);
    assert_eq!(
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        vec![
            "remote".to_owned(),
            "source".to_owned(),
            "purge".to_owned(),
            detached_id.to_string(),
        ]
    );
}

#[test]
fn settings_remote_purge_rechecks_that_the_source_is_still_detached() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    let source_id: NodeId = "node-22222222222222222222222222222222".parse().unwrap();
    let mut source = SourceMetadata::new_with_redaction_profile(
        source_id.clone(),
        SourceKind::Ssh,
        "archived lab",
        crate::source_history::RedactionProfile::Redacted,
    )
    .unwrap();
    source.set_detached(true);
    let history_store = SourceHistoryStore::new(
        directory.path().join("source-history"),
        "0123456789abcdef".parse().unwrap(),
    );
    history_store.save_source_metadata(&source).unwrap();
    app.remote_source_history_store = Some(history_store.clone());
    app.reload_remote_sources_with_history(true);
    app.view = View::Settings;
    app.selected_setting =
        SettingItem::ALL.len() + app.remote_sources.config.as_ref().unwrap().hosts().len();

    app.begin_remote_purge_confirmation();
    assert_eq!(
        app.remote_purge_confirmation
            .as_ref()
            .map(|confirmation| &confirmation.source_id),
        Some(&source_id)
    );
    history_store
        .update_source_metadata(&source_id, |metadata| {
            metadata.set_detached(false);
            Ok(())
        })
        .unwrap();

    app.confirm_remote_purge();
    assert!(app.pending_remote_action.is_none());
    assert!(app.remote_purge_confirmation.is_none());
    assert_eq!(
        app.remote_action_status.as_deref(),
        Some("Source changed; purge was not started")
    );
}

#[test]
fn settings_remote_keyboard_mutations_persist_and_actions_are_single_host() {
    let directory = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), now);
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('g'))));
    assert!(!config_store.load().unwrap().auto_sync_enabled());
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('h'))));
    assert!(
        !config_store
            .load()
            .unwrap()
            .host("dev")
            .unwrap()
            .sync_enabled()
    );

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('c'))));
    let test = app.pending_remote_action.as_ref().unwrap();
    assert_eq!(test.kind, RemoteUiActionKind::Test);
    assert_eq!(test.host_id, "dev");
    app.pending_remote_action = None;
    app.remote_action_running = None;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('s'))));
    let sync = app.pending_remote_action.as_ref().unwrap();
    assert_eq!(sync.kind, RemoteUiActionKind::Sync);
    assert_eq!(sync.host_id, "dev");
    assert_eq!(config_store.load().unwrap().hosts().len(), 2);
}

#[test]
fn settings_remote_control_geometry_is_stable_while_an_action_is_running() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let active = app.settings_controls_hitbox.clone().unwrap();

    app.remote_action_running = Some(RemoteUiActionRequest {
        kind: RemoteUiActionKind::Test,
        host_id: "dev".to_owned(),
        config_revision: app
            .remote_sources
            .config
            .as_ref()
            .unwrap()
            .config_revision(),
    });
    app.remote_action_status = Some("远程连接测试运行中".to_owned());
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let inactive = app.settings_controls_hitbox.clone().unwrap();
    assert_eq!(active.remote_global, inactive.remote_global);
    assert_eq!(active.remote_new, inactive.remote_new);
    assert_eq!(active.remote_edit, inactive.remote_edit);
    assert_eq!(active.remote_pair, inactive.remote_pair);
    assert_eq!(active.remote_remove, inactive.remote_remove);
    assert_eq!(active.remote_enable, inactive.remote_enable);
    assert_eq!(active.remote_test, inactive.remote_test);
    assert_eq!(active.remote_sync, inactive.remote_sync);
    assert_eq!(active.remote_include, inactive.remote_include);
    assert!(!inactive.remote_global_enabled);
    assert!(!inactive.remote_new_enabled);
    assert!(!inactive.remote_edit_enabled);
    assert!(!inactive.remote_pair_enabled);
    assert!(!inactive.remote_remove_enabled);
    assert!(!inactive.remote_enable_enabled);
    assert!(!inactive.remote_test_enabled);
    assert!(!inactive.remote_sync_enabled);
    assert!(!inactive.remote_include_enabled);
    for area in [
        inactive.remote_global,
        inactive.remote_new,
        inactive.remote_edit,
        inactive.remote_pair,
        inactive.remote_remove,
        inactive.remote_enable,
        inactive.remote_test,
        inactive.remote_sync,
        inactive.remote_include,
    ] {
        assert_eq!(
            terminal.backend().buffer()[(area.x + 1, area.y)].fg,
            app.theme.palette().muted
        );
    }
}

#[test]
fn settings_remote_config_controls_respect_running_and_pending_action_gates() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let before = config_store.load().unwrap();
    assert!(before.host("dev").unwrap().sync_enabled());
    app.remote_action_running = Some(RemoteUiActionRequest {
        kind: RemoteUiActionKind::Test,
        host_id: "dev".to_owned(),
        config_revision: before.config_revision(),
    });
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let disabled = app.settings_controls_hitbox.clone().unwrap();
    assert!(!disabled.remote_global_enabled);
    assert!(!disabled.remote_new_enabled);
    assert!(!disabled.remote_edit_enabled);
    assert!(!disabled.remote_remove_enabled);
    assert!(!disabled.remote_enable_enabled);
    for code in [
        KeyCode::Char('g'),
        KeyCode::Char('n'),
        KeyCode::Char('e'),
        KeyCode::Char('d'),
        KeyCode::Char('h'),
        KeyCode::Enter,
        KeyCode::Char(' '),
    ] {
        assert!(!handle_key_event(&mut app, key_event(code)));
        let unchanged = config_store.load().unwrap();
        assert_eq!(unchanged.config_revision(), before.config_revision());
        assert!(unchanged.auto_sync_enabled());
        assert!(unchanged.host("dev").unwrap().sync_enabled());
    }
    for area in [
        disabled.remote_global,
        disabled.remote_new,
        disabled.remote_edit,
        disabled.remote_remove,
        disabled.remote_enable,
    ] {
        assert!(!handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                area.right() - 1,
                area.y,
            ),
        ));
        assert_eq!(
            config_store.load().unwrap().config_revision(),
            before.config_revision()
        );
    }

    app.remote_action_running = None;
    app.selected_setting = SettingItem::ALL.len();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let stale_active = app.settings_controls_hitbox.clone().unwrap();
    app.pending_remote_action = Some(RemoteUiActionRequest {
        kind: RemoteUiActionKind::Sync,
        host_id: "dev".to_owned(),
        config_revision: before.config_revision(),
    });
    // The state may become busy after a frame is drawn. Method-level guards
    // must still reject stale active keyboard and mouse hitboxes.
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('g'))));
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('n'))));
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('e'))));
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('d'))));
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('h'))));
    for (label, area) in [
        ("global", stale_active.remote_global),
        ("new", stale_active.remote_new),
        ("edit", stale_active.remote_edit),
        ("remove", stale_active.remote_remove),
        ("enable", stale_active.remote_enable),
    ] {
        assert!(
            handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    area.right() - 1,
                    area.y,
                ),
            ),
            "{label} stale hitbox"
        );
    }
    assert_eq!(
        config_store.load().unwrap().config_revision(),
        before.config_revision()
    );

    app.pending_remote_action = None;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(!handle_key_event(&mut app, key_event(KeyCode::Enter)));
    assert!(
        config_store
            .load()
            .unwrap()
            .host("dev")
            .unwrap()
            .sync_enabled()
    );
}

#[test]
fn settings_remote_mouse_uses_whole_control_and_host_hitboxes() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let controls = app.settings_controls_hitbox.clone().unwrap();

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            controls.remote_global.right() - 1,
            controls.remote_global.y,
        ),
    ));
    assert!(!config_store.load().unwrap().auto_sync_enabled());

    let first_host = controls.remote_hosts[0];
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            first_host.right() - 1,
            first_host.y,
        ),
    ));
    assert_eq!(app.selected_remote_host_id().as_deref(), Some("dev"));

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            controls.remote_test.right() - 1,
            controls.remote_test.y,
        ),
    ));
    assert_eq!(
        app.pending_remote_action
            .as_ref()
            .map(|request| request.host_id.as_str()),
        Some("dev")
    );
}

#[test]
fn settings_remote_editor_consumes_printable_shortcuts_before_global_bindings() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let before = config_store.load().unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('n'))));
    assert!(app.remote_editor.is_some());
    for character in "node4GHCSqJX".chars() {
        assert!(!handle_key_event(
            &mut app,
            key_event(KeyCode::Char(character))
        ));
    }
    assert_eq!(app.view, View::Settings);
    assert_eq!(app.remote_editor.as_ref().unwrap().host_id, "node4GHCSqJX");
    assert_eq!(
        config_store.load().unwrap().config_revision(),
        before.config_revision()
    );

    handle_key_event(&mut app, key_event(KeyCode::Tab));
    for character in "ssh-GHCS4JX".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    assert_eq!(app.remote_editor.as_ref().unwrap().ssh_host, "ssh-GHCS4JX");
    handle_key_event(&mut app, key_event(KeyCode::Tab));
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.field),
        Some(RemoteEditorField::AgentExecutable)
    );
    assert_eq!(
        app.remote_editor
            .as_ref()
            .map(|editor| editor.agent_executable.as_str()),
        Some(DEFAULT_REMOTE_AGENT_EXECUTABLE)
    );
    handle_key_event(&mut app, key_event(KeyCode::BackTab));
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.field),
        Some(RemoteEditorField::SshHost)
    );
    handle_key_event(&mut app, key_event(KeyCode::Tab));
    handle_key_event(&mut app, key_event(KeyCode::Home));
    for character in "~/.local/bin/".chars() {
        handle_key_event(&mut app, key_event(KeyCode::Char(character)));
    }
    assert_eq!(
        app.remote_editor.as_ref().unwrap().agent_executable,
        "~/.local/bin/codex-usage-monit"
    );
    handle_key_event(&mut app, key_event(KeyCode::Tab));
    let redacted = app.remote_editor.as_ref().unwrap().redact_content;
    handle_key_event(&mut app, key_event(KeyCode::Char(' ')));
    assert_ne!(app.remote_editor.as_ref().unwrap().redact_content, redacted);
    handle_key_event(&mut app, key_event(KeyCode::Esc));
    assert!(app.remote_editor.is_none());
    assert_eq!(
        config_store.load().unwrap().config_revision(),
        before.config_revision()
    );
}

#[test]
fn settings_remote_add_and_edit_stage_exact_single_host_cli_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let revision = config_store.load().unwrap().config_revision();

    app.begin_remote_add();
    for character in "new_host".chars() {
        app.insert_remote_editor_character(character);
    }
    app.cycle_remote_editor_field(true);
    for character in "new-box".chars() {
        app.insert_remote_editor_character(character);
    }
    app.cycle_remote_editor_field(true);
    app.move_remote_editor_cursor(false, Some(false));
    for character in "/opt/".chars() {
        app.insert_remote_editor_character(character);
    }
    app.cycle_remote_editor_field(true);
    app.toggle_remote_editor_content();
    app.submit_remote_editor();
    let request = app.pending_remote_action.as_ref().unwrap();
    assert_eq!(request.host_id, "new_host");
    assert_eq!(request.config_revision, revision);
    assert!(matches!(
        &request.kind,
        RemoteUiActionKind::Add {
            ssh_host,
            agent_executable,
            redact_content: false,
        } if ssh_host == "new-box" && agent_executable == "/opt/codex-usage-monit"
    ));
    let mut command = Command::new("codex-usage-monit");
    append_remote_ui_action_args(&mut command, request);
    let args = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        args,
        vec![
            "remote".to_owned(),
            "add".to_owned(),
            "new_host".to_owned(),
            "--expected-revision".to_owned(),
            revision.to_string(),
            "--ssh-host".to_owned(),
            "new-box".to_owned(),
            "--agent-executable".to_owned(),
            "/opt/codex-usage-monit".to_owned(),
            "--redact-content".to_owned(),
            "false".to_owned(),
        ]
    );
    assert!(!args.iter().any(|argument| argument == "--all"));

    app.pending_remote_action = None;
    app.remote_action_running = None;
    app.begin_selected_remote_edit();
    let editor = app.remote_editor.as_ref().unwrap();
    assert_eq!(editor.mode, RemoteEditorMode::Edit);
    assert_eq!(editor.host_id, "dev");
    assert_eq!(editor.field, RemoteEditorField::SshHost);
    assert_eq!(editor.ssh_host, "dev-box");
    assert_eq!(editor.agent_executable, DEFAULT_REMOTE_AGENT_EXECUTABLE);
    app.submit_remote_editor();
    assert!(matches!(
        &app.pending_remote_action.as_ref().unwrap().kind,
        RemoteUiActionKind::Edit {
            ssh_host,
            agent_executable,
            ..
        } if ssh_host == "dev-box" && agent_executable == DEFAULT_REMOTE_AGENT_EXECUTABLE
    ));
}

#[test]
fn settings_remote_crud_mouse_controls_and_remove_confirmation_work_in_compact_layout() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len() + 1;
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let controls = app.settings_controls_hitbox.clone().unwrap();
    for area in [
        controls.remote_new,
        controls.remote_edit,
        controls.remote_pair,
        controls.remote_remove,
        controls.remote_enable,
        controls.remote_test,
        controls.remote_sync,
    ] {
        assert!(!area.is_empty());
        assert!(area.right() <= 60);
    }

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            controls.remote_new.right() - 1,
            controls.remote_new.y,
        ),
    ));
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.mode),
        Some(RemoteEditorMode::Add)
    );
    app.cancel_remote_editor();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let pair = app.settings_controls_hitbox.as_ref().unwrap().remote_pair;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            pair.right() - 1,
            pair.y,
        ),
    ));
    let pair_request = app.pending_remote_action.as_ref().unwrap();
    assert_eq!(pair_request.host_id, "lab");
    assert!(matches!(&pair_request.kind, RemoteUiActionKind::Pair));
    app.pending_remote_action = None;
    app.remote_action_running = None;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let edit = app.settings_controls_hitbox.as_ref().unwrap().remote_edit;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            edit.right() - 1,
            edit.y,
        ),
    ));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let editor_hitbox = app.remote_editor_hitbox.unwrap();
    assert!(!editor_hitbox.ssh_host.is_empty());
    assert!(!editor_hitbox.agent_executable.is_empty());
    assert!(!editor_hitbox.content.is_empty());
    assert!(!editor_hitbox.next.is_empty());
    assert!(!editor_hitbox.save.is_empty());
    assert!(!editor_hitbox.cancel.is_empty());
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            editor_hitbox.next.right() - 1,
            editor_hitbox.next.y,
        ),
    ));
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.field),
        Some(RemoteEditorField::AgentExecutable)
    );
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            editor_hitbox.content.right() - 1,
            editor_hitbox.content.y,
        ),
    ));
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.field),
        Some(RemoteEditorField::Content)
    );
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            editor_hitbox.cancel.right() - 1,
            editor_hitbox.cancel.y,
        ),
    ));
    assert!(app.remote_editor.is_none());

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let remove = app.settings_controls_hitbox.as_ref().unwrap().remote_remove;
    handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            remove.right() - 1,
            remove.y,
        ),
    );
    assert_eq!(
        app.remote_remove_confirmation
            .as_ref()
            .map(|confirmation| confirmation.host_id.as_str()),
        Some("lab")
    );
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let confirmation = app.remote_remove_confirmation_hitbox.unwrap();
    assert!(!confirmation.confirm.is_empty());
    assert!(!confirmation.cancel.is_empty());
    handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            confirmation.confirm.right() - 1,
            confirmation.confirm.y,
        ),
    );
    let request = app.pending_remote_action.as_ref().unwrap();
    assert_eq!(request.host_id, "lab");
    assert!(matches!(&request.kind, RemoteUiActionKind::Remove));
    let mut command = Command::new("codex-usage-monit");
    append_remote_ui_action_args(&mut command, request);
    let args = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        args,
        vec![
            "remote".to_owned(),
            "remove".to_owned(),
            "lab".to_owned(),
            "--expected-revision".to_owned(),
            request.config_revision.to_string(),
        ]
    );
    assert!(!args.iter().any(|argument| argument == "--keep-included"));
}

#[test]
fn settings_remote_completion_is_applied_only_to_the_matching_request() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let revision = config_store.load().unwrap().config_revision();
    app.request_remote_action(RemoteUiActionKind::Test);
    let running = app.remote_action_running.clone().unwrap();
    app.apply_remote_action_completion(RemoteUiActionCompletion {
        request: RemoteUiActionRequest {
            kind: RemoteUiActionKind::Test,
            host_id: "lab".to_owned(),
            config_revision: revision,
        },
        result: Err("command failed".to_owned()),
    });
    assert_eq!(app.remote_action_running.as_ref(), Some(&running));
    assert!(
        app.remote_action_status
            .as_deref()
            .is_some_and(|status| status.contains("started"))
    );

    app.apply_remote_action_completion(RemoteUiActionCompletion {
        request: running,
        result: Ok(RemoteUiActionOutcome::Complete),
    });
    assert!(app.remote_action_running.is_none());
    assert!(
        app.remote_action_status
            .as_deref()
            .is_some_and(|status| status.contains("completed for dev"))
    );
}

#[test]
fn settings_remote_editor_next_control_is_clickable_in_terse_layout() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    app.begin_selected_remote_edit();
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.field),
        Some(RemoteEditorField::SshHost)
    );

    let mut terminal = Terminal::new(TestBackend::new(36, 12)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let hitbox = app.remote_editor_hitbox.unwrap();
    assert_eq!(hitbox.next.width, UnicodeWidthStr::width("[Tab]") as u16);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.next.right() - 1,
            hitbox.next.y,
        ),
    ));
    assert_eq!(
        app.remote_editor.as_ref().map(|editor| editor.field),
        Some(RemoteEditorField::AgentExecutable)
    );
}

#[test]
fn settings_remote_editor_refuses_a_stale_config_revision() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    let (config_store, _) = install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    app.begin_selected_remote_edit();
    let original_revision = app.remote_editor.as_ref().unwrap().config_revision;
    config_store
        .update(
            original_revision,
            RemotesConfigMutation::set_auto_sync_enabled(false),
        )
        .unwrap();

    app.submit_remote_editor();
    assert!(app.pending_remote_action.is_none());
    assert!(app.remote_action_running.is_none());
    assert!(
        app.remote_editor
            .as_ref()
            .and_then(|editor| editor.validation_error.as_deref())
            .is_some_and(|error| error.contains("config changed"))
    );
}

#[test]
fn other_view_shows_sanitized_per_host_remote_sync_health_fields() {
    let directory = tempfile::tempdir().unwrap();
    let now = DateTime::parse_from_rfc3339("2026-08-31T08:15:30Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), now);
    app.view = View::Health;
    let mut terminal = Terminal::new(TestBackend::new(180, 40)).unwrap();
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(content.contains("Remote sync health"));
    assert!(content.contains("dev  configured=yes"));
    assert!(content.contains("result=success"));
    assert!(content.contains("completion=complete"));
    assert!(content.contains("pages=2"));
    assert!(content.contains("changes=17"));
    assert!(content.contains("bytes=4096"));
    assert!(content.contains("failures=0"));
    assert!(content.contains("lab  configured=yes"));
    assert!(content.contains("result=failure"));
    assert!(content.contains("error=transport"));
    assert!(content.contains("budget rolling=0 B (0 B)"));
    assert!(content.contains("soft=ok/150.0 MiB"));
    assert!(content.contains("hard=ok/250.0 MiB"));
    assert!(content.contains(&local_full_time_label(Some(now), "never")));
    assert!(content.contains(&local_full_time_label(
        Some(now + ChronoDuration::minutes(1)),
        "-"
    )));
    assert!(!content.contains("dev-box"));
    assert!(!content.contains("lab-box"));
}

#[test]
fn remote_health_reload_detects_service_updates_without_snapshot_changes() {
    let directory = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut app = interaction_test_app(0, 0);
    let (config_store, health_store) =
        install_remote_sources_fixture(&mut app, directory.path(), now);
    let config = config_store.load().unwrap();
    let source = config.host("dev").unwrap().expected_source().unwrap();

    assert!(!app.reload_remote_sources());
    health_store
        .record_failure(
            "dev",
            Some(source),
            now + ChronoDuration::minutes(1),
            RemoteSyncErrorCategory::Transport,
            Some(now + ChronoDuration::minutes(5)),
        )
        .unwrap();
    assert!(app.reload_remote_sources());
    assert!(!app.reload_remote_sources());
}

#[test]
fn other_view_renders_budget_pause_as_distinct_health_state_with_exact_resume() {
    let directory = tempfile::tempdir().unwrap();
    let now = DateTime::parse_from_rfc3339("2026-08-31T08:15:30Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut app = interaction_test_app(0, 0);
    let (config_store, health_store) =
        install_remote_sources_fixture(&mut app, directory.path(), now);
    let config = config_store.load().unwrap();
    let source = config.host("dev").unwrap().expected_source().unwrap();
    let paused_at = now + ChronoDuration::minutes(2);
    let resume_at = now + ChronoDuration::hours(24);
    health_store
        .record_pause(
            "dev",
            Some(source),
            paused_at,
            crate::remote_bandwidth_budget::RemoteBandwidthBudgetLevel::Hard,
            Some(resume_at),
        )
        .unwrap();
    assert!(app.reload_remote_sources());

    app.view = View::Health;
    let mut terminal = Terminal::new(TestBackend::new(240, 40)).unwrap();
    terminal
        .draw(|frame| render_at(frame, &mut app, now))
        .unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("result=budget-paused"));
    assert!(content.contains("failures=0"));
    assert!(content.contains(&local_full_time_label(Some(paused_at), "-")));
    assert!(content.contains(&local_full_time_label(Some(resume_at), "-")));
}

#[test]
fn remote_bandwidth_reload_detects_changes_and_renders_soft_hard_resume_times() {
    let directory = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), now);

    assert!(!app.reload_remote_sources());
    record_remote_bandwidth(&app, "dev", Utc::now(), 160 * 1024 * 1024);
    assert!(app.reload_remote_sources());
    assert!(!app.reload_remote_sources());
    let soft = remote_bandwidth_status(&app, "dev").unwrap();
    assert_eq!(soft.rolling_bytes, Some(160 * 1024 * 1024));
    assert_eq!(soft.soft, RemoteBandwidthThresholdStatus::Paused);
    assert_eq!(soft.hard, RemoteBandwidthThresholdStatus::Ok);
    let soft_resume = soft.resume_at.expect("soft resume time");

    app.view = View::Health;
    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        for (width, height) in [(80, 24), (180, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_remote_sync_health(frame, frame.area(), &app))
                .unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(content.contains("budget rolling=167772160 B (160.0 MiB)"));
            assert!(content.contains("soft=paused/150.0 MiB"));
            assert!(content.contains("hard=ok/250.0 MiB"));
            assert!(content.contains(&local_full_time_label(Some(soft_resume), "-")));
            assert!(!content.contains("dev-box"));
        }
    }

    record_remote_bandwidth(&app, "dev", Utc::now(), 100 * 1024 * 1024);
    assert!(app.reload_remote_sources());
    let hard = remote_bandwidth_status(&app, "dev").unwrap();
    assert_eq!(hard.rolling_bytes, Some(260 * 1024 * 1024));
    assert_eq!(hard.soft, RemoteBandwidthThresholdStatus::Paused);
    assert_eq!(hard.hard, RemoteBandwidthThresholdStatus::Paused);
    assert!(hard.resume_at.is_some());

    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("Budget 260.0 MiB · HARD PAUSED"));
}

#[test]
fn remote_bandwidth_refresh_is_a_read_only_batch_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), Utc::now());
    let budget_directory = directory.path().join("remote-bandwidth-v1");

    // Observing a fresh store must not create even the budget directory or
    // stable lock file.
    assert!(!budget_directory.exists());
    assert!(!app.reload_remote_sources());
    assert!(!budget_directory.exists());

    record_remote_bandwidth(&app, "dev", Utc::now(), 8 * 1024);
    let ledger_path = budget_directory.join("ledger.json");
    let before = std::fs::read(&ledger_path).unwrap();
    let before_json: serde_json::Value = serde_json::from_slice(&before).unwrap();
    let before_last_observed = before_json.get("lastObservedAt").cloned();
    let before_modified = std::fs::metadata(&ledger_path).unwrap().modified().unwrap();
    let mut before_names = std::fs::read_dir(&budget_directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    before_names.sort();

    assert!(app.reload_remote_sources());
    for _ in 0..4 {
        assert!(!app.reload_remote_sources());
    }
    assert_eq!(
        remote_bandwidth_status(&app, "dev").and_then(|status| status.rolling_bytes),
        Some(8 * 1024)
    );

    let after = std::fs::read(&ledger_path).unwrap();
    let after_json: serde_json::Value = serde_json::from_slice(&after).unwrap();
    let mut after_names = std::fs::read_dir(&budget_directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    after_names.sort();
    assert_eq!(
        after, before,
        "refresh must not atomically rewrite the ledger"
    );
    assert_eq!(
        after_json.get("lastObservedAt"),
        before_last_observed.as_ref()
    );
    assert_eq!(
        std::fs::metadata(&ledger_path).unwrap().modified().unwrap(),
        before_modified
    );
    assert_eq!(
        after_names, before_names,
        "refresh must not create temp files"
    );
}

#[test]
fn unpaired_remote_bandwidth_uses_only_the_local_host_key() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = interaction_test_app(0, 0);
    install_remote_sources_fixture(&mut app, directory.path(), Utc::now());

    record_remote_bandwidth(&app, "lab", Utc::now(), 4 * 1024);
    assert!(app.reload_remote_sources());
    let status = remote_bandwidth_status(&app, "lab").expect("unpaired host budget");
    assert_eq!(status.rolling_bytes, Some(4 * 1024));
    assert_eq!(status.soft, RemoteBandwidthThresholdStatus::Ok);
    assert_eq!(status.hard, RemoteBandwidthThresholdStatus::Ok);

    app.view = View::Settings;
    app.selected_setting = SettingItem::ALL.len() + 1;
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("Budget 4.0 KiB · OK"));

    let ledger = std::fs::read_to_string(
        directory
            .path()
            .join("remote-bandwidth-v1")
            .join("ledger.json"),
    )
    .unwrap();
    assert!(ledger.contains("\"hostId\": \"lab\""));
    assert!(!ledger.contains("lab-box"));
}

#[test]
fn other_view_lists_every_reset_time_and_available_reset_credits() {
    let mut app = interaction_test_app(1, 1);
    let now = app.snapshot.as_of;
    app.snapshot.sources = ["rollout_jsonl", "app_server"]
        .into_iter()
        .map(|source| SourceStatus {
            source: source.to_string(),
            status: "ok".to_string(),
            as_of: now,
            message: None,
        })
        .collect();
    let primary_reset = now + chrono::Duration::hours(2);
    let secondary_reset = now + chrono::Duration::days(4);
    let expired_reset = now - chrono::Duration::hours(1);
    let first_credit_granted = now - chrono::Duration::hours(3);
    let first_credit_expires = now + chrono::Duration::days(7);
    let second_credit_granted = now - chrono::Duration::minutes(45);
    let second_credit_expires = now + chrono::Duration::days(14);
    let third_credit_granted = now - chrono::Duration::minutes(5);
    app.snapshot.limits = vec![
        LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: Some("Codex".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(25.0, Some(300), Some(primary_reset))),
            secondary: Some(LimitWindow::new(40.0, Some(10_080), Some(secondary_reset))),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
        LimitBucket {
            limit_id: "reviews".to_string(),
            limit_name: Some("Reviews".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(60.0, Some(1_440), Some(expired_reset))),
            secondary: Some(LimitWindow::new(0.0, Some(60), None)),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
    ];
    app.snapshot.rate_limit_reset_credits = Some(RateLimitResetCreditsSnapshot {
        available_count: 4,
        credits: Some(vec![
            RateLimitResetCredit {
                granted_at: first_credit_granted,
                expires_at: Some(first_credit_expires),
                status: "available".to_string(),
                reset_type: "codexRateLimits".to_string(),
                title: Some("First reset".to_string()),
                description: None,
            },
            RateLimitResetCredit {
                granted_at: second_credit_granted,
                expires_at: Some(second_credit_expires),
                status: "redeeming".to_string(),
                reset_type: "unknown".to_string(),
                title: None,
                description: Some("Second reset".to_string()),
            },
            RateLimitResetCredit {
                granted_at: third_credit_granted,
                expires_at: None,
                status: "ok\u{7}x".to_string(),
                reset_type: "futureResetType".to_string(),
                title: Some("A\u{1b}B".to_string()),
                description: None,
            },
        ]),
        provenance: Provenance::ServerSnapshot,
        as_of: now,
    });
    app.view = View::Health;

    let expected_resets = [primary_reset, secondary_reset, expired_reset].map(|reset| {
        reset
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string()
    });
    let expected_credit_resets = [first_credit_expires, second_credit_expires].map(|time| {
        time.with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string()
    });
    let credit_granted_times = [
        first_credit_granted,
        second_credit_granted,
        third_credit_granted,
    ];
    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        for (width, height) in [(60, 24), (80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();

            assert!(content.contains("Other"));
            assert!(!content.contains("Data health"));
            assert!(content.contains("Resets"));
            assert!(content.contains("4 available"));
            assert!(content.contains("SERVER"));
            assert!(content.contains("DETAILS 3/4"));
            assert!(!content.contains("SHOWING"));
            assert!(!content.contains("PARTIAL DETAILS"));
            assert!(content.contains("GRANTED"));
            assert!(content.contains("RESET TIME (LOCAL)"));
            assert!(!content.contains("EXPIRES"));
            assert!(content.matches("codex").count() >= 2);
            assert!(content.matches("reviews").count() >= 2);
            for expected in &expected_resets {
                assert!(
                    content.contains(expected),
                    "missing exact reset {expected} at {width}x{height}/{theme:?}: {content}"
                );
            }
            for expected in &expected_credit_resets {
                assert!(
                    content.contains(expected),
                    "missing exact credit reset {expected} at {width}x{height}/{theme:?}: {content}"
                );
            }
            let expected_grants = credit_granted_times.map(|time| {
                time.with_timezone(&Local)
                    .format(if width < 80 {
                        "%m-%d %H:%M"
                    } else {
                        "%Y-%m-%d %H:%M:%S %:z"
                    })
                    .to_string()
            });
            for expected in &expected_grants {
                assert!(
                    content.contains(expected),
                    "missing credit grant {expected} at {width}x{height}/{theme:?}: {content}"
                );
            }
            assert!(content.contains("available"));
            assert!(content.contains("redeeming"));
            assert!(content.contains("unknown"));
            assert!(content.contains("A B"));
            assert!(content.contains("ok x"));
            assert!(content.contains("never"));
            assert!(content.contains("unavailable"));
            assert!(!content.contains('\u{1b}'));
            assert!(!content.contains('\u{7}'));
            if width < 80 {
                for slot in ["P/5h", "S/week", "P/1440m", "S/60m"] {
                    assert!(content.contains(slot));
                }
            } else {
                assert!(content.contains("First reset"));
                assert!(content.matches("primary").count() >= 2);
                assert!(content.matches("secondary").count() >= 2);
            }
        }
    }
}

#[test]
fn other_view_caps_many_credit_rows_and_keeps_diagnostics_intact() {
    let mut app = interaction_test_app(0, 0);
    let now = app.snapshot.as_of;
    app.snapshot.sources = ["rollout_jsonl", "app_server"]
        .into_iter()
        .map(|source| SourceStatus {
            source: source.to_string(),
            status: "ok".to_string(),
            as_of: now,
            message: None,
        })
        .collect();
    let primary_reset = now + chrono::Duration::hours(2);
    let secondary_reset = now + chrono::Duration::hours(3);
    let review_primary_reset = now + chrono::Duration::hours(4);
    let review_secondary_reset = now + chrono::Duration::hours(5);
    app.snapshot.limits = vec![
        LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: Some("Codex".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(25.0, Some(300), Some(primary_reset))),
            secondary: Some(LimitWindow::new(40.0, Some(10_080), Some(secondary_reset))),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
        LimitBucket {
            limit_id: "reviews".to_string(),
            limit_name: Some("Reviews".to_string()),
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(
                10.0,
                Some(1_440),
                Some(review_primary_reset),
            )),
            secondary: Some(LimitWindow::new(
                20.0,
                Some(60),
                Some(review_secondary_reset),
            )),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        },
    ];
    let credit_times = (0..8)
        .map(|index| {
            (
                now - chrono::Duration::minutes(index),
                now + chrono::Duration::days(index + 1),
            )
        })
        .collect::<Vec<_>>();
    app.snapshot.rate_limit_reset_credits = Some(RateLimitResetCreditsSnapshot {
        available_count: 8,
        credits: Some(
            credit_times
                .iter()
                .enumerate()
                .map(|(index, (granted_at, expires_at))| RateLimitResetCredit {
                    granted_at: *granted_at,
                    expires_at: Some(*expires_at),
                    status: "available".to_string(),
                    reset_type: "codexRateLimits".to_string(),
                    title: Some(format!("credit-{index}")),
                    description: None,
                })
                .collect(),
        ),
        provenance: Provenance::ServerSnapshot,
        as_of: now,
    });
    app.view = View::Health;

    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(content.contains("SHOWING 4/8"));
    assert!(content.contains("WINDOWS 3/4"));
    for (index, (granted_at, expires_at)) in credit_times.iter().take(4).enumerate() {
        assert!(content.contains(&format!("credit-{index}")));
        assert!(
            content.contains(
                &granted_at
                    .with_timezone(&Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            )
        );
        assert!(
            content.contains(
                &expires_at
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S %:z")
                    .to_string()
            )
        );
    }
    for index in 4..8 {
        assert!(!content.contains(&format!("credit-{index}")));
    }
    for reset in [primary_reset, secondary_reset, review_primary_reset] {
        assert!(
            content.contains(
                &reset
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S %:z")
                    .to_string()
            )
        );
    }
    assert!(
        !content.contains(
            &review_secondary_reset
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        )
    );
    assert!(content.contains("Diagnostics"));
    assert!(content.contains("No collection issues"));
}

#[test]
fn other_view_reports_when_reset_data_is_unavailable() {
    let mut app = interaction_test_app(0, 0);
    app.view = View::Health;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(content.contains("Other"));
    assert!(content.contains("Resets"));
    assert!(content.contains("credits unavailable"));
    assert!(content.contains("No reset-window data"));

    app.snapshot.rate_limit_reset_credits = Some(RateLimitResetCreditsSnapshot {
        available_count: 2,
        credits: None,
        provenance: Provenance::ServerSnapshot,
        as_of: app.snapshot.as_of,
    });
    app.account_refresh_retry_count = 1;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("2 available"));
    assert!(!content.contains("RETRYING"));
    assert!(content.contains("DETAILS UNAVAILABLE"));
    assert!(!content.contains("SHOWING"));
}

#[test]
fn other_view_distinguishes_zero_stale_and_partial_reset_credits() {
    let mut app = interaction_test_app(0, 0);
    app.snapshot.rate_limit_reset_credits = Some(RateLimitResetCreditsSnapshot {
        available_count: 0,
        credits: Some(Vec::new()),
        provenance: Provenance::Stale,
        as_of: app.snapshot.as_of,
    });
    app.snapshot.rate_limit_reset_credits_partial = true;
    app.view = View::Health;
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(content.contains("0 available"));
    assert!(content.contains("STALE"));
    assert!(content.contains("PARTIAL"));
    assert!(!content.contains("credits unavailable"));

    app.snapshot.rate_limit_reset_credits = None;
    app.snapshot.rate_limit_reset_credits_partial = true;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("credits unavailable"));
    assert!(content.contains("PARTIAL"));
}

#[test]
fn mouse_focus_and_wheel_stay_independent_and_backspace_reveals_task() {
    let mut app = interaction_test_app(30, 5);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let turn_hitbox = app.turn_table_hitbox.unwrap();
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            turn_hitbox.rows.x,
            turn_hitbox.rows.y,
        ),
    ));
    assert_eq!(app.focus, Focus::Turns);

    let task_hitbox = app.task_table_hitbox.unwrap();
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            task_hitbox.viewport.x,
            task_hitbox.viewport.y,
        ),
    ));
    assert_eq!(app.focus, Focus::Turns);
    assert_eq!(app.selected_task, 0);
    assert!(app.task_table_offset > 0);

    handle_key_event(&mut app, key_event(KeyCode::Backspace));
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.task_table_offset, 0);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let tasks = app.task_table_hitbox.unwrap();
    assert!(app.selected_task >= tasks.offset);
    assert!(app.selected_task < tasks.offset + tasks.capacity);
}

#[test]
fn keyboard_task_selection_in_health_is_revealed_on_return() {
    let mut app = mouse_test_app(50);
    app.selected_task = 40;
    app.task_table_offset = 26;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    app.view = View::Health;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    app.select_first_task();
    assert!(app.task_reveal_pending);
    app.view = View::Overview;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let overview = app.task_table_hitbox.expect("task rows should be visible");
    assert_eq!(app.selected_task, 0);
    assert_eq!(overview.offset, 0);
    assert!(!app.task_reveal_pending);

    app.view = View::Health;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    app.select_last_task();
    assert!(app.task_reveal_pending);
    app.view = View::Overview;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let window = app.task_table_hitbox.expect("task rows should be visible");
    assert_eq!(app.selected_task, 49);
    assert!(app.selected_task >= window.offset);
    assert!(app.selected_task < window.offset + usize::from(window.rows.height));
    assert!(!app.task_reveal_pending);
}

#[test]
fn task_and_turn_scrollbars_drag_without_changing_selection_or_wheel_behavior() {
    let mut app = interaction_test_app(30, 30);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let task_bar = app.task_scrollbar_hitbox.expect("task scrollbar");
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            task_bar.thumb.x,
            task_bar.thumb.y,
        ),
    ));
    assert_eq!(app.task_table_offset, 0);
    assert_eq!(app.selected_task, 0);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            0,
            task_bar.track.bottom().saturating_add(10),
        ),
    ));
    assert_eq!(app.task_table_offset, task_bar.max_offset);
    assert_eq!(app.selected_task, 0);
    assert_eq!(app.focus, Focus::Tasks);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Up(MouseButton::Left), 0, 0),
    ));
    let dragged_offset = app.task_table_offset;
    assert!(!handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Drag(MouseButton::Left), 0, 0),
    ));
    assert_eq!(app.task_table_offset, dragged_offset);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let task_bar = app.task_scrollbar_hitbox.expect("task scrollbar");
    assert_eq!(task_bar.thumb.bottom(), task_bar.track.bottom());
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::ScrollUp, task_bar.track.x, task_bar.track.y,),
    ));
    assert_eq!(
        app.task_table_offset,
        dragged_offset.saturating_sub(MOUSE_SCROLL_LINES)
    );
    assert_eq!(app.selected_task, 0);

    app.begin_task_search();
    let turn_bar = app.turn_scrollbar_hitbox.expect("turn scrollbar");
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            turn_bar.track.x,
            turn_bar.track.bottom().saturating_sub(1),
        ),
    ));
    assert_eq!(app.focus, Focus::Turns);
    assert_eq!(app.turn_offset, turn_bar.max_offset);
    assert_eq!(app.selected_turn, 0);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            turn_bar.track.x,
            turn_bar.track.bottom().saturating_sub(1),
        ),
    ));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let turn_bar = app.turn_scrollbar_hitbox.expect("turn scrollbar");
    assert_eq!(turn_bar.thumb.bottom(), turn_bar.track.bottom());
}

#[test]
fn horizontal_drag_on_a_scrollbar_thumb_does_not_quantize_the_offset() {
    let mut app = mouse_test_app(100);
    app.task_table_offset = 44;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let scrollbar = app.task_scrollbar_hitbox.expect("task scrollbar");

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.thumb.x,
            scrollbar.thumb.y,
        ),
    ));
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            scrollbar.thumb.x.saturating_sub(10),
            scrollbar.thumb.y,
        ),
    ));
    assert_eq!(app.task_table_offset, 44);

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            0,
            scrollbar.track.bottom().saturating_sub(1),
        ),
    ));
    assert!(app.task_table_offset > 44);
}

#[test]
fn filtered_task_scrollbar_keeps_row_clicks_mapped_to_absolute_tasks() {
    let mut app = interaction_test_app(30, 1);
    for index in 0..30 {
        app.snapshot.tasks[index].source =
            Some(if index % 2 == 0 { "cli" } else { "desktop" }.to_string());
    }
    app.set_task_source_filter(TaskSourceFilter::Cli);
    let filtered = app.filtered_task_indices();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let scrollbar = app.task_scrollbar_hitbox.expect("filtered task scrollbar");

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.track.x,
            scrollbar.track.bottom().saturating_sub(1),
        ),
    ));
    assert_eq!(app.task_table_offset, scrollbar.max_offset);
    handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            scrollbar.track.x,
            scrollbar.track.bottom().saturating_sub(1),
        ),
    );
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = app.task_table_hitbox.unwrap().rows;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), rows.x, rows.y),
    ));
    assert_eq!(app.selected_task, filtered[scrollbar.max_offset]);
}

#[test]
fn mouse_wheel_routes_by_table_and_keeps_selection_stable() {
    let mut app = interaction_test_app(30, 20);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let task_hitbox = app.task_table_hitbox.expect("task rows should be visible");
    let turn_hitbox = app.turn_table_hitbox.expect("turn rows should be visible");

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            task_hitbox.viewport.x,
            task_hitbox.viewport.y,
        ),
    ));
    assert_eq!(app.task_table_offset, MOUSE_SCROLL_LINES);
    assert_eq!(app.selected_task, 0);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollUp,
            task_hitbox.viewport.x,
            task_hitbox.viewport.y,
        ),
    ));
    assert_eq!(app.task_table_offset, 0);
    handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            task_hitbox.viewport.x,
            task_hitbox.viewport.y,
        ),
    );

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            turn_hitbox.viewport.x,
            turn_hitbox.viewport.y,
        ),
    ));
    assert_eq!(app.turn_offset, MOUSE_SCROLL_LINES);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.selected_task, 0);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollUp,
            turn_hitbox.viewport.x,
            turn_hitbox.viewport.y,
        ),
    ));
    assert_eq!(app.turn_offset, 0);
    handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            turn_hitbox.viewport.x,
            turn_hitbox.viewport.y,
        ),
    );

    assert!(!handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            turn_hitbox.viewport.x,
            turn_hitbox.viewport.bottom().saturating_add(1),
        ),
    ));
    assert!(!handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollLeft,
            turn_hitbox.viewport.x,
            turn_hitbox.viewport.y,
        ),
    ));

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let task_hitbox = app.task_table_hitbox.expect("task rows should be visible");
    assert_eq!(task_hitbox.offset, MOUSE_SCROLL_LINES);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            task_hitbox.rows.x,
            task_hitbox.rows.y,
        ),
    ));
    assert_eq!(app.selected_task, MOUSE_SCROLL_LINES);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let turn_hitbox = app.turn_table_hitbox.expect("turn rows should be visible");
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            turn_hitbox.viewport.x,
            turn_hitbox.viewport.y,
        ),
    ));
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let turn_hitbox = app.turn_table_hitbox.expect("turn rows should be visible");
    let target_row = turn_hitbox.rows.y.saturating_add(1);
    let target_index = turn_hitbox.offset + 1;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            turn_hitbox.rows.x,
            target_row,
        ),
    ));
    assert_eq!(app.selected_turn, target_index);

    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::ScrollDown,
            turn_hitbox.viewport.x,
            turn_hitbox.viewport.y,
        ),
    ));
    assert_eq!(app.selected_turn, target_index);
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains(&format!("turn=turn-3-{target_index}")));
}

#[test]
fn turn_click_maps_scrolled_rows_across_sizes() {
    for (width, height) in [(80, 24), (100, 30), (120, 40)] {
        let mut app = interaction_test_app(1, 30);
        app.turn_offset = 10;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hitbox = app.turn_table_hitbox.expect("turn rows should be visible");
        assert!(hitbox.offset > 0);
        let relative = usize::from(hitbox.rows.height > 1);
        let target = hitbox.offset + relative;

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y + relative as u16,
            ),
        ));
        assert_eq!(app.selected_turn, target);
        assert_eq!(app.turn_offset, hitbox.offset);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Turn detail"));
        assert!(content.contains(&format!("{}/30", target + 1)));
    }
}

#[test]
fn refresh_preserves_selected_turn_by_id_and_falls_back_when_removed() {
    let mut app = interaction_test_app(1, 3);
    app.selected_turn = 1;
    let selected_id = app.selected_turn_record().unwrap().turn_id.clone();

    let mut inserted_snapshot = app.snapshot.clone();
    let mut inserted = inserted_snapshot.turns[0].clone();
    inserted.turn_id = "newest-turn".to_string();
    inserted_snapshot.turns.insert(0, inserted);
    app.replace(
        CollectionResult {
            snapshot: inserted_snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert_eq!(app.selected_turn, 2);
    assert_eq!(app.selected_turn_record().unwrap().turn_id, selected_id);

    let mut removed_snapshot = app.snapshot.clone();
    removed_snapshot
        .turns
        .retain(|turn| turn.turn_id != selected_id);
    app.turn_offset = 2;
    app.replace(
        CollectionResult {
            snapshot: removed_snapshot,
            account: app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);
    assert_ne!(app.selected_turn_record().unwrap().turn_id, selected_id);

    let replacement = interaction_test_app(2, 10);
    let mut replacement_snapshot = replacement.snapshot;
    replacement_snapshot
        .tasks
        .retain(|task| task.thread_id == "task-thread-1");
    replacement_snapshot
        .turns
        .retain(|turn| turn.thread_id == "task-thread-1");
    app.task_table_offset = 7;
    app.turn_offset = 4;
    app.selected_turn = 1;
    app.replace(
        CollectionResult {
            snapshot: replacement_snapshot,
            account: replacement.account,
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert_eq!(app.selected_task, 0);
    assert_eq!(app.task_table_offset, 0);
    assert_eq!(app.selected_turn, 0);
    assert_eq!(app.turn_offset, 0);
    assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-1-0");
}

#[test]
fn task_click_uses_rendered_scroll_offset_without_jumping() {
    for (width, height) in [(80, 24), (100, 30), (120, 40)] {
        let mut app = mouse_test_app(50);
        app.selected_task = 40;
        app.task_table_offset = 35;
        app.turn_offset = 5;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hitbox = app.task_table_hitbox.expect("task rows should be visible");
        assert!(hitbox.offset > 0, "expected a scrolled task table");

        let target = hitbox.offset;
        assert_ne!(target, app.selected_task);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y,
            ),
        ));
        assert_eq!(app.selected_task, target);
        assert_eq!(app.turn_offset, 0);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rerendered = app
            .task_table_hitbox
            .expect("task rows should remain visible");
        assert_eq!(rerendered.offset, hitbox.offset);
        assert_eq!(app.task_table_offset, hitbox.offset);
    }
}

#[test]
fn task_scroll_offset_refills_rows_after_resize_and_models_visibility_change() {
    let mut app = mouse_test_app(50);
    app.view = View::Overview;
    app.selected_task = 40;
    app.task_table_offset = 39;

    let mut narrow = Terminal::new(TestBackend::new(80, 24)).unwrap();
    narrow.draw(|frame| render(frame, &mut app)).unwrap();
    let narrow_hitbox = app.task_table_hitbox.expect("task rows should be visible");
    assert_eq!(narrow_hitbox.rows.height, 2);
    assert!(narrow_hitbox.offset > 30);

    let mut wide = Terminal::new(TestBackend::new(120, 40)).unwrap();
    wide.draw(|frame| render(frame, &mut app)).unwrap();
    let wide_hitbox = app.task_table_hitbox.expect("task rows should be visible");
    assert_eq!(wide_hitbox.rows.height, 21);
    assert!(wide_hitbox.offset <= 31);
    assert!(app.selected_task >= wide_hitbox.offset);
    assert!(app.selected_task < wide_hitbox.offset + usize::from(wide_hitbox.rows.height));

    app.toggle_models_visibility();
    wide.draw(|frame| render(frame, &mut app)).unwrap();
    let overview_hitbox = app.task_table_hitbox.expect("task rows should be visible");
    assert_eq!(overview_hitbox.rows.height, 31);
    assert!(overview_hitbox.offset <= 19);
    assert!(app.selected_task >= overview_hitbox.offset);
    assert!(app.selected_task < overview_hitbox.offset + usize::from(overview_hitbox.rows.height));
}

#[test]
fn task_click_ignores_non_rows_non_left_clicks_and_stale_views() {
    let mut app = mouse_test_app(3);
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let hitbox = app.task_table_hitbox.expect("task rows should be visible");

    for event in [
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.y - 1,
        ),
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x - 1,
            hitbox.rows.y,
        ),
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.bottom(),
        ),
        mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            hitbox.rows.x,
            hitbox.rows.y + 1,
        ),
        mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.y + 1,
        ),
    ] {
        assert!(!handle_mouse_event(&mut app, event));
        assert_eq!(app.selected_task, 0);
    }

    app.turn_offset = 7;
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.y,
        ),
    ));
    assert_eq!(app.turn_offset, 7);
    assert!(handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.y + 1,
        ),
    ));
    assert_eq!(app.selected_task, 1);
    assert_eq!(app.turn_offset, 0);

    app.view = View::Health;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert!(app.task_table_hitbox.is_none());
    assert!(app.turn_table_hitbox.is_none());
    assert!(!handle_mouse_event(
        &mut app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            hitbox.rows.x,
            hitbox.rows.y,
        ),
    ));
    assert_eq!(app.selected_task, 1);
}

#[test]
fn renders_all_views_at_common_terminal_sizes() {
    for (width, height) in [(80, 24), (120, 40)] {
        let now = chrono::Utc::now();
        let snapshot = Snapshot {
            schema_version: 1,
            api_pricing: Default::default(),
            api_equivalent_cost: Default::default(),
            as_of: now,
            partial: false,
            codex_home: "/tmp/.codex".into(),
            sources: Vec::new(),
            limits: vec![LimitBucket {
                limit_id: "codex".to_string(),
                limit_name: None,
                plan_type: Some("test".to_string()),
                primary: Some(LimitWindow::new(
                    25.0,
                    Some(300),
                    Some(now + chrono::Duration::hours(2)),
                )),
                secondary: Some(LimitWindow::new(
                    40.0,
                    Some(10_080),
                    Some(now + chrono::Duration::days(4)),
                )),
                credits: None,
                rate_limit_reached_type: None,
                provenance: Provenance::ServerSnapshot,
                as_of: now,
            }],
            rate_limit_reset_credits: None,
            rate_limit_reset_credits_partial: false,
            account_usage: None,
            tasks: vec![TaskRecord {
                thread_id: "task-thread".to_string(),
                archived: false,
                title: "task".to_string(),
                cwd: Some("/tmp/project".into()),
                source: Some("desktop".to_string()),
                parent_thread_id: None,
                created_at: Some(now),
                updated_at: Some(now),
                status: TaskStatus::Completed,
                status_provenance: Provenance::LocalExact,
                status_confidence: Confidence::High,
                token_usage: TokenUsage {
                    total_tokens: 42,
                    ..TokenUsage::default()
                },
                turn_count: 1,
                window_token_usage: TokenUsage {
                    total_tokens: 42,
                    ..TokenUsage::default()
                },
                local_token_share_percent: 100.0,
                estimated_quota_percent: 1.0,
                quota_confidence: Confidence::Low,
                api_equivalent_cost: Default::default(),
            }],
            turns: vec![TurnRecord {
                thread_id: "task-thread".to_string(),
                turn_id: "turn-1".to_string(),
                model: Some("gpt-5.6-sol".to_string()),
                reasoning_effort: Some("ultra".to_string()),
                service_tier: None,
                message_preview: Some("inspect message preview".to_string()),
                started_at: Some(now),
                completed_at: Some(now),
                duration_ms: Some(1),
                status: crate::domain::TurnStatus::Completed,
                token_usage: TokenUsage {
                    total_tokens: 42,
                    ..TokenUsage::default()
                },
                window_token_usage: TokenUsage {
                    total_tokens: 42,
                    ..TokenUsage::default()
                },
                local_token_share_percent: 100.0,
                estimated_quota_percent: 1.0,
                quota_confidence: Confidence::Low,
                api_equivalent_cost: Default::default(),
            }],
            models: Vec::new(),
            attribution: AttributionSummary {
                window: Some(WindowDescriptor {
                    limit_id: "codex".to_string(),
                    label: "5h".to_string(),
                    starts_at: now - chrono::Duration::hours(3),
                    ends_at: now + chrono::Duration::hours(2),
                    used_percent: 25.0,
                }),
                ..AttributionSummary::default()
            },
            window_analyses: Vec::new(),
            stats: CollectionStats::default(),
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        let result = CollectionResult {
            snapshot,
            account: AccountSnapshot::default(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        };

        for theme in [Theme::Dark, Theme::Light] {
            let mut app = App::new(result.clone(), theme);
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();

            for view in View::ALL {
                app.view = view;
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                if theme == Theme::Light {
                    let palette = theme.palette();
                    let cell_count = usize::from(width) * usize::from(height);
                    assert!(
                        buffer
                            .content()
                            .iter()
                            .filter(|cell| cell.bg == palette.background)
                            .count()
                            > cell_count / 2
                    );
                    assert!(
                        buffer
                            .content()
                            .iter()
                            .filter(|cell| cell.fg == palette.border)
                            .count()
                            > 10
                    );
                }
                if view == View::Overview {
                    let content = buffer
                        .content()
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(content.contains("ultra"));
                    assert!(
                        content.contains("gpt-5.6-sol"),
                        "missing full model at {width}x{height} in {view:?}/{theme:?}: {content}"
                    );
                    assert!(content.contains("inspect"));
                    assert!(content.contains("Turn detail"));
                    assert!(content.contains("total=42"));
                    assert!(content.contains("1ms"));
                    assert!(content.contains("D 42"));
                    assert!(content.contains("TOKEN"));
                    assert!(content.contains("API EQ."));
                    assert!(content.contains("DONE"));
                    assert!(content.contains("STALE"));
                    assert!(!content.contains("STATE EVIDENCE"));

                    if theme == Theme::Light {
                        let palette = theme.palette();
                        let done = status_tone_style(StatusTone::Done, theme);
                        assert!(
                            buffer
                                .content()
                                .iter()
                                .any(|cell| cell.bg == palette.gauge_track)
                        );
                        assert!(buffer.content().iter().any(|cell| {
                            cell.fg == palette.accent
                                && Some(cell.bg) == done.bg
                                && !cell.symbol().trim().is_empty()
                        }));
                        for tone in [
                            StatusTone::Active,
                            StatusTone::Waiting,
                            StatusTone::Done,
                            StatusTone::Stopped,
                            StatusTone::Failed,
                            StatusTone::Stale,
                        ] {
                            let style = status_tone_style(tone, theme);
                            assert!(buffer.content().iter().any(|cell| {
                                Some(cell.fg) == style.fg && Some(cell.bg) == style.bg
                            }));
                        }
                    }
                } else if view == View::Trends {
                    let content = buffer
                        .content()
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(content.contains("Quota Remaining"));
                    if width >= 120 && height >= 30 {
                        assert!(content.contains("Weekly Local Tokens"));
                        assert!(content.contains("15m Local Tokens"));
                    }
                } else if view == View::Health {
                    let content = buffer
                        .content()
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(content.contains("Other"));
                    assert!(content.contains("Resets"));
                } else if view == View::Settings {
                    let content = buffer
                        .content()
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(content.contains("Table columns"));
                    assert!(content.contains("API equivalent"));
                }
            }

            if theme == Theme::Light {
                let light_background = theme.palette().background;
                app.toggle_theme();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let dark_background = Theme::Dark.palette().background;
                assert!(
                    terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .any(|cell| cell.bg == dark_background)
                );
                assert!(
                    terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .all(|cell| cell.bg != light_background)
                );
            }
        }
    }
}
