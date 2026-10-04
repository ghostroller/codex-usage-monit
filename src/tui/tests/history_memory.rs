use super::*;

#[test]
fn projection_cache_shares_records_with_app_while_diagnostics_remain_independent() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 8, 30, 9, 20, 0).unwrap();
    let mut runtime = HistoryRuntime::new(
        directory.path().join("state/history-v1"),
        &codex_home,
        false,
    )
    .unwrap();
    let lease = acquire_runtime_profile_lease(&runtime).unwrap();
    prepare_tui_history_runtime(&mut runtime, &lease, now);
    let mut observation = tui_runtime_test_observation(now - ChronoDuration::minutes(20), 10);
    observation.quota_points.push(QuotaPoint {
        observed_at: now,
        limit_id: "codex".to_owned(),
        duration_mins: 10_080,
        resets_at: now + ChronoDuration::days(6),
        used_percent: 20.0,
        remaining_percent: 80.0,
        provenance: Provenance::ServerSnapshot,
    });
    observation.weekly_local_points.push(WeeklyLocalPoint {
        observed_at: now,
        resets_at: now + ChronoDuration::days(6),
        token_usage: TokenUsage {
            total_tokens: 10,
            ..TokenUsage::default()
        },
        estimated_cost_units: 10,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: HISTORY_ESTIMATOR_REVISION,
        call_count: 1,
        partial_reasons: Vec::new(),
    });
    runtime
        .record_local_observation(&observation, LocalObservationMode::Incremental)
        .unwrap();
    let mut store = TuiHistoryStore::runtime(runtime, Some(lease), Vec::new());
    let selection = HistorySourceSelection::AllIncluded;
    let projection = store.load_since_with_staged_selected(&selection, history_view_since(now));
    assert!(projection.query_error.is_none());
    assert!(!projection.history.quota_points.is_empty());
    assert!(!projection.history.half_hour_buckets.is_empty());
    assert!(!projection.history.weekly_local_points.is_empty());
    let cached = store
        .projection_cache
        .as_ref()
        .unwrap()
        .projection
        .history
        .clone();

    let mut app = interaction_test_app(1, 1);
    assert!(app.apply_history_projection(app.history_source_generation, projection));
    assert_eq!(
        app.history.quota_points.as_ptr(),
        cached.quota_points.as_ptr()
    );
    assert_eq!(
        app.history.half_hour_buckets.as_ptr(),
        cached.half_hour_buckets.as_ptr()
    );
    assert_eq!(
        app.history.weekly_local_points.as_ptr(),
        cached.weekly_local_points.as_ptr()
    );

    // Cached records stay shared when a later delivery gains diagnostics and
    // loses write permission. Neither change belongs to the cached data slice.
    store
        .setup_warnings
        .push("injected setup warning".to_owned());
    drop(store.profile_lease.take());
    let mut delivery = store.clone_cached_projection().unwrap();
    assert!(delivery.history.read_only);
    assert!(
        delivery
            .history
            .warnings
            .iter()
            .any(|warning| warning == "injected setup warning")
    );
    assert!(!cached.read_only);
    assert!(!app.history.read_only);
    assert!(
        !app.history
            .warnings
            .iter()
            .any(|warning| warning == "injected setup warning")
    );
    assert_eq!(
        delivery.history.half_hour_buckets.as_ptr(),
        cached.half_hour_buckets.as_ptr()
    );
    assert_eq!(
        delivery.history.quota_points.as_ptr(),
        cached.quota_points.as_ptr()
    );
    assert_eq!(
        delivery.history.weekly_local_points.as_ptr(),
        cached.weekly_local_points.as_ptr()
    );

    let original_total = cached.half_hour_buckets[0].token_usage.total_tokens;
    delivery.history.half_hour_buckets[0]
        .token_usage
        .total_tokens = original_total + 1;
    delivery.history.half_hour_buckets[0].project_groups[0].title =
        Some("changed title".to_owned());
    assert_eq!(
        cached.half_hour_buckets[0].token_usage.total_tokens,
        original_total
    );
    assert_eq!(
        app.history.half_hour_buckets[0].token_usage.total_tokens,
        original_total
    );
    assert_ne!(
        delivery.history.half_hour_buckets.as_ptr(),
        cached.half_hour_buckets.as_ptr()
    );
    assert_ne!(
        cached.half_hour_buckets[0].project_groups[0]
            .title
            .as_deref(),
        Some("changed title")
    );
    // Copying the edited bucket series must not copy the untouched series.
    assert_eq!(
        delivery.history.quota_points.as_ptr(),
        cached.quota_points.as_ptr()
    );
    assert_eq!(
        delivery.history.weekly_local_points.as_ptr(),
        cached.weekly_local_points.as_ptr()
    );
}

#[test]
fn shared_history_preserves_non_reflexive_quota_equality() {
    let now = Utc.with_ymd_and_hms(2026, 8, 30, 9, 20, 0).unwrap();
    let mut history = trend_history_fixture(now);
    history.quota_points[0].used_percent = f64::NAN;
    let cloned = history.clone();
    assert_eq!(history.quota_points.as_ptr(), cloned.quota_points.as_ptr());
    assert_ne!(
        history, cloned,
        "sharing records must not change float comparison semantics"
    );
}
