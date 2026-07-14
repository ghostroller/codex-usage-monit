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
        parent_thread_id: None,
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
        service_tier: None,
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
    model: Option<&str>,
    total_tokens: u64,
) -> UsageCall {
    UsageCall {
        timestamp,
        thread_id: thread_id.to_string(),
        turn_id: Some(turn_id.to_string()),
        model: model.map(str::to_string),
        tokens: tokens(total_tokens),
    }
}

fn limit_with_id(
    limit_id: &str,
    as_of: DateTime<Utc>,
    used_percent: f64,
    duration_mins: i64,
    resets_at: DateTime<Utc>,
) -> LimitBucket {
    LimitBucket {
        limit_id: limit_id.to_string(),
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

fn codex_limit(
    as_of: DateTime<Utc>,
    used_percent: f64,
    duration_mins: i64,
    resets_at: DateTime<Utc>,
) -> LimitBucket {
    limit_with_id("codex", as_of, used_percent, duration_mins, resets_at)
}

fn observation(
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
fn estimates_each_entity_from_codex_gauge_and_local_token_share() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let limits = vec![codex_limit(now, 40.0, 300, reset)];
    let calls = vec![
        call(at(8, 59), "old", "old-turn", Some("gpt-old"), 900),
        call(at(10, 0), "a", "a-turn", Some("gpt-a"), 100),
        call(at(11, 0), "a", "a-turn", Some("gpt-a"), 200),
        call(at(11, 30), "b", "b-turn", Some("gpt-b"), 100),
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
        analyze_current_window(&mut tasks, &mut turns, &calls, &[], &limits, now);

    assert_eq!(summary.local_token_usage, tokens(400));
    assert_close(summary.observed_delta_percent, 0.0);
    assert_close(summary.estimated_assigned_percent, 0.0);
    assert_close(summary.proxy_projected_percent, 40.0);
    assert_close(summary.unattributed_percent, 40.0);
    assert_eq!(summary.confidence, Confidence::Low);
    assert!(summary.settled);
    assert_eq!(summary.method, "current_codex_gauge_token_share_proxy");

    assert_eq!(tasks[0].window_token_usage, tokens(300));
    assert_close(tasks[0].local_token_share_percent, 75.0);
    assert_close(tasks[0].estimated_quota_percent, 30.0);
    assert_eq!(tasks[0].quota_confidence, Confidence::Low);
    assert_eq!(tasks[1].window_token_usage, tokens(100));
    assert_close(tasks[1].local_token_share_percent, 25.0);
    assert_close(tasks[1].estimated_quota_percent, 10.0);
    assert_eq!(tasks[2].window_token_usage, TokenUsage::default());
    assert_close(tasks[2].estimated_quota_percent, 0.0);
    assert_eq!(tasks[2].quota_confidence, Confidence::Unknown);

    assert_close(turns[0].estimated_quota_percent, 30.0);
    assert_close(turns[1].estimated_quota_percent, 10.0);
    assert_close(turns[2].estimated_quota_percent, 0.0);

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model, "gpt-a");
    assert_eq!(models[0].token_usage, tokens(300));
    assert_close(models[0].local_token_share_percent, 75.0);
    assert_close(models[0].estimated_quota_percent, 30.0);
    assert_eq!(models[0].quota_confidence, Confidence::Low);
    assert_eq!(models[1].model, "gpt-b");
    assert_close(models[1].estimated_quota_percent, 10.0);
}

#[test]
fn observations_do_not_change_the_simple_estimate() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let limits = vec![codex_limit(now, 34.0, 300, reset)];
    let calls = vec![
        call(at(11, 0), "a", "a-turn", Some("gpt-a"), 100),
        call(at(11, 30), "b", "b-turn", Some("gpt-b"), 300),
    ];
    let observations = vec![
        observation(at(10, 0), 99.0, 300, reset + Duration::minutes(30)),
        observation(at(11, 0), 3.0, 300, reset),
        observation(at(11, 59), 33.0, 300, reset),
    ];

    let without = analyze_windows(&[], &[], &calls, &[], &limits, now);
    let with = analyze_windows(&[], &[], &calls, &observations, &limits, now);

    assert_eq!(with, without);
    assert_close(with[0].threads[0].usage.estimated_quota_percent, 8.5);
    assert_close(with[0].threads[1].usage.estimated_quota_percent, 25.5);
}

#[test]
fn running_or_stale_tasks_only_change_the_settled_flag() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let limits = vec![codex_limit(now, 20.0, 300, reset)];
    let calls = vec![call(at(11, 59), "a", "turn", Some("gpt-a"), 100)];

    for status in [TaskStatus::Running, TaskStatus::Stale] {
        let mut tasks = vec![task("a", status)];
        let mut turns = vec![turn("a", "turn")];
        let (_, summary) =
            analyze_current_window(&mut tasks, &mut turns, &calls, &[], &limits, now);

        assert!(!summary.settled);
        assert_eq!(summary.confidence, Confidence::Low);
        assert_close(tasks[0].estimated_quota_percent, 20.0);
        assert_eq!(tasks[0].quota_confidence, Confidence::Low);
    }
}

#[test]
fn an_empty_local_denominator_keeps_estimates_unavailable() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let analyses = analyze_windows(
        &[],
        &[],
        &[],
        &[],
        &[codex_limit(now, 27.0, 300, reset)],
        now,
    );

    assert_eq!(analyses.len(), 1);
    let analysis = &analyses[0];
    assert!(analysis.threads.is_empty());
    assert!(analysis.turns.is_empty());
    assert!(analysis.models.is_empty());
    assert_eq!(
        analysis.attribution.local_token_usage,
        TokenUsage::default()
    );
    assert_close(analysis.attribution.proxy_projected_percent, 0.0);
    assert_eq!(analysis.attribution.confidence, Confidence::Unknown);
    assert_eq!(
        analysis.attribution.method,
        "codex_gauge_without_local_tokens"
    );
}

#[test]
fn excludes_only_the_exact_spark_model_case_insensitively() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let calls = vec![
        call(
            at(11, 0),
            "spark-a",
            "spark-a-turn",
            Some("gpt-5.3-codex-spark"),
            500,
        ),
        call(
            at(11, 10),
            "spark-b",
            "spark-b-turn",
            Some("  GPT-5.3-CODEX-SPARK  "),
            500,
        ),
        call(
            at(11, 20),
            "regular",
            "regular-turn",
            Some("gpt-5.3-codex"),
            200,
        ),
        call(
            at(11, 30),
            "spark-preview",
            "preview-turn",
            Some("gpt-5.3-codex-spark-preview"),
            100,
        ),
    ];

    let analyses = analyze_windows(
        &[],
        &[],
        &calls,
        &[],
        &[codex_limit(now, 30.0, 300, reset)],
        now,
    );

    let analysis = &analyses[0];
    assert_eq!(analysis.attribution.local_token_usage, tokens(300));
    assert_eq!(analysis.threads.len(), 2);
    assert!(
        analysis
            .threads
            .iter()
            .all(|row| !row.thread_id.starts_with("spark-") || row.thread_id == "spark-preview")
    );
    assert_eq!(analysis.models.len(), 2);
    assert!(
        analysis
            .models
            .iter()
            .any(|row| row.model == "gpt-5.3-codex")
    );
    assert!(
        analysis
            .models
            .iter()
            .any(|row| row.model == "gpt-5.3-codex-spark-preview")
    );
    assert_close(
        analysis
            .models
            .iter()
            .map(|row| row.estimated_quota_percent)
            .sum(),
        30.0,
    );
}

#[test]
fn missing_and_other_model_names_remain_in_the_codex_denominator() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let calls = vec![
        call(at(11, 0), "unknown", "unknown-turn", None, 100),
        call(at(11, 15), "blank", "blank-turn", Some("   "), 100),
        call(at(11, 30), "other", "other-turn", Some("custom-model"), 300),
    ];

    let analysis = analyze_windows(
        &[],
        &[],
        &calls,
        &[],
        &[codex_limit(now, 40.0, 300, reset)],
        now,
    )
    .remove(0);

    assert_eq!(analysis.attribution.local_token_usage, tokens(500));
    let unknown = analysis
        .models
        .iter()
        .find(|row| row.model == "unknown")
        .unwrap();
    let other = analysis
        .models
        .iter()
        .find(|row| row.model == "custom-model")
        .unwrap();
    assert_close(unknown.local_token_share_percent, 40.0);
    assert_close(unknown.estimated_quota_percent, 16.0);
    assert_close(other.local_token_share_percent, 60.0);
    assert_close(other.estimated_quota_percent, 24.0);
}

#[test]
fn only_codex_limit_buckets_are_analyzed() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let calls = vec![call(at(11, 0), "a", "turn", Some("gpt-a"), 100)];
    let limits = vec![
        limit_with_id("codex_bengalfox", now, 70.0, 300, reset),
        limit_with_id("five-hour", now, 60.0, 300, reset),
        codex_limit(now, 20.0, 300, reset),
    ];

    let analyses = analyze_windows(&[], &[], &calls, &[], &limits, now);

    assert_eq!(analyses.len(), 1);
    assert_eq!(
        analyses[0].attribution.window.as_ref().unwrap().limit_id,
        "codex"
    );
    assert_close(analyses[0].threads[0].usage.estimated_quota_percent, 20.0);

    let without_codex = analyze_windows(&[], &[], &calls, &[], &limits[..2], now);
    assert!(without_codex.is_empty());
}

#[test]
fn duplicate_codex_buckets_choose_the_newest_authoritative_candidate() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let calls = vec![call(at(11, 0), "a", "turn", Some("gpt-a"), 100)];
    let mut stale = codex_limit(now, 90.0, 300, reset);
    stale.provenance = Provenance::Stale;
    stale.as_of = now + Duration::minutes(1);
    let old_server = codex_limit(now - Duration::hours(1), 80.0, 300, reset);
    let newest_server = codex_limit(now, 20.0, 300, reset);

    let analyses = analyze_windows(
        &[],
        &[],
        &calls,
        &[],
        &[stale.clone(), old_server, newest_server],
        now,
    );

    assert_eq!(analyses.len(), 1);
    assert_close(
        analyses[0]
            .attribution
            .window
            .as_ref()
            .unwrap()
            .used_percent,
        20.0,
    );
    assert_close(analyses[0].threads[0].usage.estimated_quota_percent, 20.0);
    assert!(!analyses[0].partial);

    let stale_only = analyze_windows(&[], &[], &calls, &[], &[stale], now);
    assert!(stale_only[0].partial);
    assert!(
        stale_only[0]
            .partial_reasons
            .contains(&"quota_window_stale".to_string())
    );
    assert_close(stale_only[0].threads[0].usage.estimated_quota_percent, 90.0);
}

#[test]
fn five_hour_and_weekly_cycles_use_independent_codex_denominators() {
    let now = at(12, 0);
    let five_hour_reset = at(14, 0);
    let weekly_reset = on(14, 12, 0);
    let limits = vec![
        codex_limit(now, 20.0, 300, five_hour_reset),
        codex_limit(now, 50.0, 10_080, weekly_reset),
    ];
    let calls = vec![
        call(
            on(7, 11, 59),
            "too-old",
            "too-old-turn",
            Some("gpt-old"),
            900,
        ),
        call(on(8, 8, 0), "week-only", "week-turn", Some("gpt-week"), 300),
        call(at(10, 0), "recent", "recent-turn", Some("gpt-recent"), 100),
    ];

    let analyses = analyze_windows(&[], &[], &calls, &[], &limits, now);

    assert_eq!(analyses.len(), 2);
    let five_hour = analyses
        .iter()
        .find(|analysis| analysis.duration_mins == 300)
        .unwrap();
    let weekly = analyses
        .iter()
        .find(|analysis| analysis.duration_mins == 10_080)
        .unwrap();
    assert_eq!(five_hour.attribution.local_token_usage, tokens(100));
    assert_eq!(weekly.attribution.local_token_usage, tokens(400));
    assert_eq!(five_hour.threads.len(), 1);
    assert_close(five_hour.threads[0].usage.estimated_quota_percent, 20.0);

    let weekly_recent = weekly
        .threads
        .iter()
        .find(|row| row.thread_id == "recent")
        .unwrap();
    let weekly_old = weekly
        .threads
        .iter()
        .find(|row| row.thread_id == "week-only")
        .unwrap();
    assert_close(weekly_recent.usage.local_token_share_percent, 25.0);
    assert_close(weekly_recent.usage.estimated_quota_percent, 12.5);
    assert_close(weekly_old.usage.local_token_share_percent, 75.0);
    assert_close(weekly_old.usage.estimated_quota_percent, 37.5);
}

#[test]
fn each_complete_entity_partition_sums_to_codex_used_percent() {
    let now = at(12, 0);
    let reset = at(14, 0);
    let calls = vec![
        call(at(10, 0), "a", "a-1", Some("gpt-a"), 100),
        call(at(10, 30), "a", "a-2", Some("gpt-b"), 200),
        call(at(11, 0), "b", "b-1", Some("gpt-a"), 300),
        call(at(11, 30), "c", "c-1", None, 400),
    ];

    let analysis = analyze_windows(
        &[],
        &[],
        &calls,
        &[],
        &[codex_limit(now, 37.0, 300, reset)],
        now,
    )
    .remove(0);

    assert_close(
        analysis
            .threads
            .iter()
            .map(|row| row.usage.estimated_quota_percent)
            .sum(),
        37.0,
    );
    assert_close(
        analysis
            .turns
            .iter()
            .map(|row| row.usage.estimated_quota_percent)
            .sum(),
        37.0,
    );
    assert_close(
        analysis
            .models
            .iter()
            .map(|row| row.estimated_quota_percent)
            .sum(),
        37.0,
    );
}

#[test]
fn expired_windows_are_not_analyzed_as_current() {
    let now = at(12, 0);
    let expired = codex_limit(at(9, 0), 80.0, 300, at(10, 0));
    let at_reset = codex_limit(now, 80.0, 300, now);

    assert!(analyze_windows(&[], &[], &[], &[], &[expired], now).is_empty());
    assert!(analyze_windows(&[], &[], &[], &[], &[at_reset], now).is_empty());
}

#[test]
fn no_current_codex_window_clears_legacy_projection_fields() {
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
    assert_close(turns[0].estimated_quota_percent, 0.0);
    assert_eq!(turns[0].quota_confidence, Confidence::Unknown);
}
