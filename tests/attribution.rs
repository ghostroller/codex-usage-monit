use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};
use codex_usage_monit::attribution::{analyze_current_window, analyze_windows};
use codex_usage_monit::domain::{
    Confidence, LimitBucket, LimitWindow, Provenance, RateObservation, TaskRecord, TaskStatus,
    TokenUsage, TurnRecord, TurnStatus, UsageCall,
};

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    on(12, hour, minute)
}

fn on(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, day, hour, minute, 0)
        .single()
        .unwrap()
}

fn tokens(total_tokens: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens,
        total_tokens,
        ..TokenUsage::default()
    }
}

fn task(thread_id: &str, status: TaskStatus) -> TaskRecord {
    TaskRecord {
        thread_id: thread_id.to_string(),
        title: thread_id.to_string(),
        cwd: Some(PathBuf::from("/tmp/project")),
        source: Some("test".to_string()),
        created_at: None,
        updated_at: None,
        status,
        status_provenance: Provenance::LocalExact,
        status_confidence: Confidence::High,
        token_usage: TokenUsage::default(),
        turn_count: 1,
        window_token_usage: tokens(999),
        local_token_share_percent: 99.0,
        estimated_quota_percent: 99.0,
        quota_confidence: Confidence::High,
    }
}

fn turn(thread_id: &str, turn_id: &str) -> TurnRecord {
    TurnRecord {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        model: None,
        reasoning_effort: None,
        message_preview: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        status: TurnStatus::Completed,
        token_usage: TokenUsage::default(),
        window_token_usage: tokens(999),
        local_token_share_percent: 99.0,
        estimated_quota_percent: 99.0,
        quota_confidence: Confidence::High,
    }
}

fn call(
    timestamp: DateTime<Utc>,
    thread_id: &str,
    turn_id: &str,
    model: &str,
    total_tokens: u64,
) -> UsageCall {
    UsageCall {
        timestamp,
        thread_id: thread_id.to_string(),
        turn_id: Some(turn_id.to_string()),
        model: Some(model.to_string()),
        tokens: tokens(total_tokens),
    }
}

fn limit(
    as_of: DateTime<Utc>,
    used_percent: f64,
    duration_mins: i64,
    resets_at: DateTime<Utc>,
) -> LimitBucket {
    LimitBucket {
        limit_id: "codex".to_string(),
        limit_name: None,
        plan_type: None,
        primary: Some(LimitWindow::new(
            used_percent,
            Some(duration_mins),
            Some(resets_at),
        )),
        secondary: None,
        credits: None,
        rate_limit_reached_type: None,
        provenance: Provenance::ServerSnapshot,
        as_of,
    }
}

fn observation(
    timestamp: DateTime<Utc>,
    used_percent: f64,
    resets_at: DateTime<Utc>,
) -> RateObservation {
    observation_for(timestamp, used_percent, 300, resets_at)
}

fn observation_for(
    timestamp: DateTime<Utc>,
    used_percent: f64,
    duration_mins: i64,
    resets_at: DateTime<Utc>,
) -> RateObservation {
    RateObservation {
        timestamp,
        thread_id: "source-thread".to_string(),
        turn_id: None,
        limit_id: "codex".to_string(),
        primary: Some(LimitWindow::new(
            used_percent,
            Some(duration_mins),
            Some(resets_at),
        )),
        secondary: None,
        provenance: Provenance::LocalExact,
    }
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn aggregates_exact_tokens_and_estimates_observed_deltas() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let limits = vec![limit(now, 40.0, 300, resets_at)];
    let observations = vec![
        observation(at(11, 54), 30.0, resets_at + Duration::seconds(90)),
        observation(at(11, 57), 34.0, resets_at + Duration::seconds(90)),
    ];
    let calls = vec![
        call(at(9, 30), "old", "old-turn", "gpt-old", 400),
        call(at(11, 55), "a", "a-turn", "gpt-a", 100),
        call(at(11, 56), "b", "b-turn", "gpt-b", 300),
        call(at(11, 58), "a", "a-turn", "gpt-a", 200),
    ];
    let mut tasks = vec![
        task("a", TaskStatus::Completed),
        task("b", TaskStatus::Completed),
        task("old", TaskStatus::Completed),
    ];
    let mut turns = vec![
        turn("a", "a-turn"),
        turn("b", "b-turn"),
        turn("old", "old-turn"),
    ];

    let (models, summary) =
        analyze_current_window(&mut tasks, &mut turns, &calls, &observations, &limits, now);

    assert_eq!(summary.local_token_usage, tokens(1_000));
    assert_close(summary.observed_delta_percent, 10.0);
    assert_close(summary.estimated_assigned_percent, 10.0);
    assert_close(summary.unattributed_percent, 30.0);
    assert_eq!(summary.confidence, Confidence::Medium);
    assert!(summary.settled);
    assert_eq!(summary.method, "observed_delta_token_proportional");

    assert_eq!(tasks[0].window_token_usage, tokens(300));
    assert_close(tasks[0].local_token_share_percent, 30.0);
    assert_close(tasks[0].estimated_quota_percent, 7.0);
    assert_eq!(tasks[0].quota_confidence, Confidence::Medium);
    assert_close(tasks[1].estimated_quota_percent, 3.0);
    assert_close(tasks[2].local_token_share_percent, 40.0);
    assert_close(tasks[2].estimated_quota_percent, 0.0);
    assert_eq!(tasks[2].quota_confidence, Confidence::Unknown);

    assert_close(turns[0].estimated_quota_percent, 7.0);
    assert_close(turns[1].estimated_quota_percent, 3.0);
    assert_close(turns[2].estimated_quota_percent, 0.0);

    assert_eq!(models.len(), 3);
    assert_eq!(models[0].model, "gpt-a");
    assert_eq!(models[0].token_usage, tokens(300));
    assert_close(models[0].local_token_share_percent, 30.0);
    assert_close(models[0].estimated_quota_percent, 7.0);
    assert_eq!(models[0].quota_confidence, Confidence::Medium);
    assert_eq!(models[1].model, "gpt-b");
    assert_close(models[1].estimated_quota_percent, 3.0);
    assert_eq!(models[2].model, "gpt-old");
    assert_close(models[2].estimated_quota_percent, 0.0);
}

#[test]
fn active_tasks_keep_quota_confidence_low() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let limits = vec![limit(now, 20.0, 300, resets_at)];
    let observations = vec![observation(at(11, 58), 18.0, resets_at)];
    let calls = vec![call(at(11, 59), "a", "turn", "gpt-a", 100)];
    let mut tasks = vec![task("a", TaskStatus::Running)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) =
        analyze_current_window(&mut tasks, &mut turns, &calls, &observations, &limits, now);

    assert_close(summary.estimated_assigned_percent, 2.0);
    assert_eq!(summary.confidence, Confidence::Low);
    assert!(!summary.settled);
    assert_eq!(tasks[0].quota_confidence, Confidence::Low);
    assert_ne!(summary.confidence, Confidence::High);
}

#[test]
fn stale_tasks_do_not_claim_a_settled_window() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let limits = vec![limit(now, 20.0, 300, resets_at)];
    let observations = vec![observation(at(11, 58), 18.0, resets_at)];
    let calls = vec![call(at(11, 59), "a", "turn", "gpt-a", 100)];
    let mut tasks = vec![task("a", TaskStatus::Stale)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) =
        analyze_current_window(&mut tasks, &mut turns, &calls, &observations, &limits, now);

    assert!(!summary.settled);
    assert_eq!(summary.confidence, Confidence::Low);
}

#[test]
fn observations_outside_reset_drift_are_not_used() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let limits = vec![limit(now, 20.0, 300, resets_at)];
    let observations = vec![observation(
        at(11, 0),
        18.0,
        resets_at + Duration::seconds(121),
    )];
    let calls = vec![call(at(11, 30), "a", "turn", "gpt-a", 100)];
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) =
        analyze_current_window(&mut tasks, &mut turns, &calls, &observations, &limits, now);

    assert_close(summary.observed_delta_percent, 0.0);
    assert_close(summary.estimated_assigned_percent, 0.0);
    assert_close(summary.unattributed_percent, 20.0);
    assert_eq!(summary.confidence, Confidence::Unknown);
    assert_eq!(summary.method, "local_tokens_only");
    assert_close(tasks[0].local_token_share_percent, 100.0);
}

#[test]
fn quota_decrease_in_same_window_disables_estimation() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let mut limits = vec![limit(now, 3.0, 300, resets_at)];
    limits[0].provenance = Provenance::Stale;
    let observations = vec![
        observation(at(10, 0), 91.0, resets_at),
        observation(at(11, 0), 93.0, resets_at),
    ];
    let calls = vec![call(at(10, 30), "a", "turn", "gpt-a", 100)];
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) =
        analyze_current_window(&mut tasks, &mut turns, &calls, &observations, &limits, now);

    assert_eq!(summary.local_token_usage, tokens(100));
    assert_close(summary.observed_delta_percent, 0.0);
    assert_close(summary.estimated_assigned_percent, 0.0);
    assert_close(summary.unattributed_percent, 3.0);
    assert_eq!(summary.confidence, Confidence::Unknown);
    assert_eq!(summary.method, "quota_discontinuity_local_tokens_only");
    assert_close(tasks[0].local_token_share_percent, 100.0);
    assert_close(tasks[0].estimated_quota_percent, 0.0);
}

#[test]
fn monotonic_samples_after_a_quota_correction_restore_low_confidence_estimation() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let limits = vec![limit(now, 4.0, 300, resets_at)];
    let observations = vec![
        observation(at(11, 54), 90.0, resets_at),
        observation(at(11, 56), 3.0, resets_at),
        observation(at(11, 58), 4.0, resets_at),
    ];
    let calls = vec![call(at(11, 57), "a", "turn", "gpt-a", 100)];
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) =
        analyze_current_window(&mut tasks, &mut turns, &calls, &observations, &limits, now);

    assert_close(summary.observed_delta_percent, 1.0);
    assert_close(summary.estimated_assigned_percent, 1.0);
    assert_close(summary.unattributed_percent, 3.0);
    assert_eq!(summary.confidence, Confidence::Low);
    assert_eq!(
        summary.method,
        "post_discontinuity_observed_delta_token_proportional"
    );
    assert_close(tasks[0].estimated_quota_percent, 1.0);
}

#[test]
fn long_snapshot_gaps_remain_unattributed() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let limits = vec![limit(now, 20.0, 300, resets_at)];
    let observations = vec![observation(at(10, 0), 10.0, resets_at)];
    let calls = vec![call(at(11, 0), "a", "turn", "gpt-a", 100)];
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) =
        analyze_current_window(&mut tasks, &mut turns, &calls, &observations, &limits, now);

    assert_close(summary.observed_delta_percent, 10.0);
    assert_close(summary.estimated_assigned_percent, 0.0);
    assert_close(summary.unattributed_percent, 20.0);
    assert_eq!(summary.confidence, Confidence::Unknown);
}

#[test]
fn server_history_ignores_a_disagreeing_rollout_quota_stream() {
    let now = at(12, 0);
    let resets_at = at(14, 0);
    let limits = vec![limit(now, 4.0, 300, resets_at)];
    let local = observation(at(11, 55), 95.0, resets_at);
    let mut server = observation(at(11, 56), 3.0, resets_at);
    server.provenance = Provenance::ServerSnapshot;
    let calls = vec![call(at(11, 59), "a", "turn", "gpt-a", 100)];
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) = analyze_current_window(
        &mut tasks,
        &mut turns,
        &calls,
        &[local, server],
        &limits,
        now,
    );

    assert_close(summary.observed_delta_percent, 1.0);
    assert_close(summary.estimated_assigned_percent, 1.0);
    assert_eq!(summary.method, "observed_delta_token_proportional");
}

#[test]
fn selects_only_a_current_five_hour_window() {
    let now = at(12, 0);
    let weekly_reset = now + Duration::days(2);
    let five_hour_reset = at(14, 0);
    let mut weekly = limit(now, 60.0, 10_080, weekly_reset);
    weekly.limit_id = "weekly".to_string();
    let mut five_hour = limit(now, 10.0, 300, five_hour_reset);
    five_hour.limit_id = "five-hour".to_string();
    let mut tasks = Vec::new();
    let mut turns = Vec::new();

    let (_, five_hour_summary) = analyze_current_window(
        &mut tasks,
        &mut turns,
        &[],
        &[],
        &[weekly.clone(), five_hour],
        now,
    );
    assert_eq!(five_hour_summary.window.unwrap().limit_id, "five-hour");

    let weekly_call = call(at(11, 59), "weekly-task", "weekly-turn", "gpt-week", 100);
    let (weekly_models, weekly_only_summary) =
        analyze_current_window(&mut tasks, &mut turns, &[weekly_call], &[], &[weekly], now);
    assert!(weekly_only_summary.window.is_none());
    assert!(weekly_models.is_empty());
}

#[test]
fn analyzes_five_hour_and_weekly_reset_cycles_without_overwriting() {
    let now = at(12, 0);
    let five_hour_reset = at(14, 0);
    let weekly_reset = on(14, 12, 0);
    let mut five_hour = limit(now, 10.0, 300, five_hour_reset);
    five_hour.limit_id = "five-hour".to_string();
    let mut weekly = limit(now, 35.0, 10_080, weekly_reset);
    weekly.limit_id = "weekly".to_string();
    let calls = vec![
        call(on(7, 11, 59), "ignored", "ignored-turn", "gpt-old", 900),
        call(on(7, 12, 0), "a", "a-turn", "gpt-a", 200),
        call(at(8, 0), "b", "b-turn", "gpt-b", 300),
        call(at(10, 0), "a", "a-turn", "gpt-a", 100),
        call(at(11, 0), "b", "b-turn", "gpt-b", 400),
    ];
    let tasks = vec![
        task("a", TaskStatus::Completed),
        task("b", TaskStatus::Completed),
    ];
    let turns = vec![turn("a", "a-turn"), turn("b", "b-turn")];

    let analyses = analyze_windows(&tasks, &turns, &calls, &[], &[weekly, five_hour], now);

    assert_eq!(analyses.len(), 2);
    let five_hour = analyses
        .iter()
        .find(|analysis| analysis.duration_mins == 300)
        .unwrap();
    let weekly = analyses
        .iter()
        .find(|analysis| analysis.duration_mins == 10_080)
        .unwrap();
    assert_eq!(
        five_hour.attribution.window.as_ref().unwrap().starts_at,
        at(9, 0)
    );
    assert_eq!(
        weekly.attribution.window.as_ref().unwrap().starts_at,
        on(7, 12, 0)
    );
    assert_eq!(five_hour.attribution.local_token_usage, tokens(500));
    assert_eq!(weekly.attribution.local_token_usage, tokens(1_000));

    let five_hour_a = five_hour
        .threads
        .iter()
        .find(|thread| thread.thread_id == "a")
        .unwrap();
    let weekly_a = weekly
        .threads
        .iter()
        .find(|thread| thread.thread_id == "a")
        .unwrap();
    assert_close(five_hour_a.usage.local_token_share_percent, 20.0);
    assert_close(weekly_a.usage.local_token_share_percent, 30.0);
    assert_eq!(five_hour_a.usage.token_usage, tokens(100));
    assert_eq!(weekly_a.usage.token_usage, tokens(300));

    let mut legacy_tasks = tasks;
    let mut legacy_turns = turns;
    let (_, legacy) = analyze_current_window(
        &mut legacy_tasks,
        &mut legacy_turns,
        &calls,
        &[],
        &[
            limit(now, 35.0, 10_080, weekly_reset),
            limit(now, 10.0, 300, five_hour_reset),
        ],
        now,
    );
    assert_eq!(legacy.window.unwrap().label, "5h");
    assert_eq!(legacy_tasks[0].window_token_usage, tokens(100));
    assert_close(legacy_tasks[0].local_token_share_percent, 20.0);
}

#[test]
fn weekly_analysis_estimates_only_its_matching_reset_epoch() {
    let now = at(12, 0);
    let reset = on(14, 12, 0);
    let mut weekly_limit = limit(now, 20.0, 10_080, reset);
    weekly_limit.secondary = weekly_limit.primary.take();
    let mut observations = vec![
        observation_for(at(11, 55), 99.0, 10_080, reset + Duration::days(7)),
        observation_for(at(11, 58), 18.0, 10_080, reset),
    ];
    for observation in &mut observations {
        observation.secondary = observation.primary.take();
    }
    let calls = vec![call(at(11, 59), "a", "turn", "gpt-a", 100)];
    let tasks = vec![task("a", TaskStatus::Completed)];
    let turns = vec![turn("a", "turn")];

    let analyses = analyze_windows(&tasks, &turns, &calls, &observations, &[weekly_limit], now);

    assert_eq!(analyses.len(), 1);
    let weekly = &analyses[0];
    assert_eq!(weekly.duration_mins, 10_080);
    assert_close(weekly.attribution.observed_delta_percent, 2.0);
    assert_close(weekly.attribution.estimated_assigned_percent, 2.0);
    assert_close(weekly.threads[0].usage.estimated_quota_percent, 2.0);
    assert_eq!(weekly.threads[0].usage.quota_confidence, Confidence::Medium);
}

#[test]
fn matching_reset_epoch_survives_primary_secondary_slot_changes() {
    let now = at(12, 0);
    let reset = on(14, 12, 0);
    let tasks = vec![task("a", TaskStatus::Completed)];
    let turns = vec![turn("a", "turn")];
    let calls = vec![call(at(11, 59), "a", "turn", "gpt-a", 100)];

    for current_is_primary in [true, false] {
        let mut current = limit(now, 20.0, 10_080, reset);
        let mut previous = observation_for(at(11, 58), 18.0, 10_080, reset);
        if current_is_primary {
            previous.secondary = previous.primary.take();
        } else {
            current.secondary = current.primary.take();
        }

        let analyses = analyze_windows(&tasks, &turns, &calls, &[previous], &[current], now);

        assert_eq!(analyses.len(), 1);
        let weekly = &analyses[0];
        assert_close(weekly.attribution.observed_delta_percent, 2.0);
        assert_close(weekly.attribution.estimated_assigned_percent, 2.0);
        assert_close(weekly.threads[0].usage.estimated_quota_percent, 2.0);
        assert_eq!(weekly.threads[0].usage.quota_confidence, Confidence::Medium);
    }
}

#[test]
fn same_duration_prefers_codex_bucket_and_serializes_window_usage() {
    let now = at(12, 0);
    let reset = on(14, 12, 0);
    let mut codex = limit(now, 20.0, 10_080, reset);
    codex.limit_id = "codex".to_string();
    codex.provenance = Provenance::Stale;
    let mut secondary = limit(now, 5.0, 10_080, reset);
    secondary.limit_id = "codex-secondary".to_string();
    let unsupported = limit(now, 3.0, 1_440, now + Duration::hours(12));
    let tasks = vec![task("a", TaskStatus::Completed)];
    let turns = vec![turn("a", "turn")];
    let calls = vec![call(at(11, 0), "a", "turn", "gpt-a", 100)];

    let analyses = analyze_windows(
        &tasks,
        &turns,
        &calls,
        &[],
        &[secondary, unsupported, codex],
        now,
    );

    assert_eq!(analyses.len(), 1);
    assert_eq!(
        analyses[0].attribution.window.as_ref().unwrap().limit_id,
        "codex"
    );
    let value = serde_json::to_value(&analyses[0]).unwrap();
    assert_eq!(value["durationMins"], 10_080);
    assert_eq!(value["threads"][0]["threadId"], "a");
    assert_eq!(
        value["threads"][0]["usage"]["tokenUsage"]["totalTokens"],
        100
    );

    let mut alphabetic_first = limit(now, 8.0, 10_080, reset);
    alphabetic_first.limit_id = "alpha".to_string();
    alphabetic_first.provenance = Provenance::Stale;
    let mut server = limit(now, 9.0, 10_080, reset);
    server.limit_id = "zeta".to_string();
    let preferred = analyze_windows(
        &tasks,
        &turns,
        &calls,
        &[],
        &[alphabetic_first, server],
        now,
    );
    assert_eq!(
        preferred[0].attribution.window.as_ref().unwrap().limit_id,
        "zeta"
    );
}

#[test]
fn expired_windows_are_not_analyzed_as_current() {
    let now = at(12, 0);
    let expired = limit(at(9, 0), 80.0, 300, at(10, 0));
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) = analyze_current_window(&mut tasks, &mut turns, &[], &[], &[expired], now);

    assert!(summary.window.is_none());
    assert_eq!(summary.method, "unavailable");
}

#[test]
fn a_window_is_expired_immediately_at_its_reset_time() {
    let now = at(12, 0);
    let expired = limit(now, 80.0, 300, now);
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (_, summary) = analyze_current_window(&mut tasks, &mut turns, &[], &[], &[expired], now);

    assert!(summary.window.is_none());
}

#[test]
fn no_limits_clears_previous_attribution_values() {
    let mut tasks = vec![task("a", TaskStatus::Completed)];
    let mut turns = vec![turn("a", "turn")];

    let (models, summary) =
        analyze_current_window(&mut tasks, &mut turns, &[], &[], &[], at(12, 0));

    assert!(models.is_empty());
    assert!(summary.window.is_none());
    assert!(summary.settled);
    assert_eq!(summary.confidence, Confidence::Unknown);
    assert_eq!(tasks[0].window_token_usage, TokenUsage::default());
    assert_close(tasks[0].local_token_share_percent, 0.0);
    assert_close(tasks[0].estimated_quota_percent, 0.0);
    assert_eq!(tasks[0].quota_confidence, Confidence::Unknown);
    assert_eq!(turns[0].window_token_usage, TokenUsage::default());
    assert_close(turns[0].local_token_share_percent, 0.0);
}
