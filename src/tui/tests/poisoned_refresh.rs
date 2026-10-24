use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;
use crate::tui::session_management::{DeletePhase, SessionDeleteDialog, SessionDeleteTarget};

#[test]
fn poisoned_history_exit_does_not_persist_staged_observation() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let history_root = directory.path().join("state/history-v1");
    let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
    let profile_lease = acquire_runtime_profile_lease(&runtime).unwrap();
    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 9, 0, 0).unwrap();
    let _ = prepare_tui_history_runtime(
        &mut runtime,
        &profile_lease,
        starts_at + ChronoDuration::minutes(20),
    );
    runtime
        .record_local_observation(
            &tui_runtime_test_observation(starts_at, 10),
            LocalObservationMode::Incremental,
        )
        .unwrap();
    let history_store = Arc::new(Mutex::new(TuiHistoryStore::runtime(
        runtime,
        Some(profile_lease),
        Vec::new(),
    )));
    let interrupted = catch_unwind(AssertUnwindSafe(|| {
        let mut store = history_store.lock().unwrap();
        store.stage(&tui_runtime_test_observation(starts_at, 900));
        panic!("interrupted while holding staged history");
    }));
    assert!(interrupted.is_err());
    assert!(history_store.is_poisoned());

    flush_staged_history_on_exit(&history_store, &PerfLog::default(), &EventLog::default());
    drop(history_store);

    let mut reader = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
    let persisted = reader
        .load_unified_history_since(starts_at - ChronoDuration::hours(1))
        .unwrap();
    assert_eq!(
        persisted.history.half_hour_buckets[0]
            .token_usage
            .total_tokens,
        10,
        "exit must not commit state held by an interrupted worker"
    );
}

#[test]
fn poisoned_initial_history_worker_stops_refresh_and_preserves_displayed_data() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let runtime = HistoryRuntime::new(
        directory.path().join("state/history-v1"),
        &codex_home,
        false,
    )
    .unwrap();
    let profile_lease = acquire_runtime_profile_lease(&runtime).unwrap();
    let mut store = TuiHistoryStore::deferred_runtime(runtime, profile_lease);
    store.set_deferred_runtime_preparation_hook(|| {
        panic!("interrupt actual initial worker while it holds history state");
    });
    let history = Arc::new(Mutex::new(store));
    let cache = Arc::new(Mutex::new(RolloutCache::new()));
    let events = directory.path().join("events.jsonl");
    let config = CollectConfig {
        codex_home,
        offline: true,
        ..CollectConfig::default()
    };
    let mut app = interaction_test_app(1, 1);
    install_empty_remote_sources_fixture(&mut app, directory.path());
    app.event_log = EventLog::open(Some(events.clone()), LogLevel::Debug, false);
    let displayed_tasks = app.snapshot.tasks.clone();
    let displayed_turns = app.snapshot.turns.clone();
    let tracker = StartupLoadProgressTracker::default();
    tracker.begin();
    app.attach_startup_progress(tracker);
    app.initial_bootstrap_pending = true;
    app.history_initialization_pending = true;
    let termination = TerminationSignal::for_test();
    let (refresh_sender, refresh_receiver) = mpsc::channel();
    let (resume_sender, resume_receiver) = mpsc::channel();
    let (remote_sender, remote_receiver) = mpsc::channel();
    let context = RunLoopContext {
        termination: &termination,
        refresh_sender: &refresh_sender,
        refresh_receiver: &refresh_receiver,
        resume_sender: &resume_sender,
        resume_receiver: &resume_receiver,
        remote_sender: &remote_sender,
        remote_receiver: &remote_receiver,
    };
    let mut worker = RefreshWorker::default();
    assert!(start_refresh_if_due(
        &mut app,
        &config,
        &context,
        &cache,
        &history,
        &mut worker,
    ));
    let completion = refresh_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("a panic must still deliver the worker completion");
    worker.join();
    assert!(history.is_poisoned());
    assert!(apply_refresh_completion(&mut app, completion));
    // The panic boundary identifies poisoned state in its completion. No
    // second worker starts, and the previous display remains inspectable.
    assert!(!start_refresh_if_due(
        &mut app,
        &config,
        &context,
        &cache,
        &history,
        &mut worker,
    ));
    assert!(worker.handle.is_none());
    assert!(app.refresh_stopped);
    assert!(!app.worker_running);
    assert!(!app.initial_bootstrap_pending);
    assert!(!app.history_initialization_pending);
    assert!(!app.history_source_loading);
    assert!(!app.history_source_query_pending);
    assert!(app.startup_progress_tracker.is_none());
    assert!(app.snapshot.partial);
    assert!(app.history.read_only);
    assert_eq!(app.snapshot.tasks, displayed_tasks);
    assert_eq!(app.snapshot.turns, displayed_turns);
    assert!(app.snapshot.warnings.iter().any(|warning| {
        warning.contains("history state was interrupted") && warning.contains("restart the TUI")
    }));

    let selection = app.history_source_selection.clone();
    app.request_history_source(HistorySourceSelection::Remote(
        "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
    ));
    app.force_history_source_refresh();
    assert_eq!(app.history_source_selection, selection);
    assert!(!app.history_source_query_pending);
    app.last_local_refresh = Instant::now() - LOCAL_REFRESH;
    app.next_account_refresh = Instant::now() - Duration::from_secs(1);
    assert!(!start_refresh_if_due(
        &mut app,
        &config,
        &context,
        &cache,
        &history,
        &mut worker,
    ));
    assert!(refresh_receiver.try_recv().is_err());
    assert!(next_run_loop_poll_timeout(&app, Instant::now(), true) > Duration::ZERO);
    assert!(
        std::fs::read_to_string(events)
            .unwrap()
            .contains("tui.refresh_stopped")
    );

    let failed = collect_initial_refresh_completion(
        &config,
        &cache,
        &history,
        0,
        &HistorySourceSelection::AllIncluded,
        None,
    );
    assert_eq!(
        failed.worker_failure,
        Some(RefreshWorkerFailure::HistoryStatePoisoned)
    );
    assert!(failed.result.is_none());
    flush_staged_history_on_exit(&history, &PerfLog::default(), &EventLog::default());
}

#[test]
fn poisoned_rollout_cache_rejects_initial_local_and_backfill_collection() {
    let directory = tempfile::tempdir().unwrap();
    let config = CollectConfig {
        codex_home: directory.path().to_path_buf(),
        offline: true,
        ..CollectConfig::default()
    };
    let cache = Arc::new(Mutex::new(RolloutCache::new()));
    let worker_cache = Arc::clone(&cache);
    let worker_config = config.clone();
    assert!(
        thread::spawn(move || {
            let mut cache = worker_cache.lock().unwrap();
            collect_snapshot_cached(&worker_config, None, false, &mut cache);
            panic!("interrupt worker after changing its rollout cache");
        })
        .join()
        .is_err()
    );
    assert!(cache.is_poisoned());
    for force_materialization in [false, true] {
        let inputs = RefreshCollectionInputs::Local {
            cached_account: AccountSnapshot::default(),
            force_materialization,
        };
        assert!(matches!(
            inputs.collect(&config, &cache),
            Err(RefreshWorkerFailure::RolloutCachePoisoned)
        ));
    }
    assert_eq!(
        with_summary_backfill_cache(&cache, |_| panic!("must not fork poisoned cache")),
        Err::<(), _>(RefreshWorkerFailure::RolloutCachePoisoned),
    );
    let history = Arc::new(Mutex::new(TuiHistoryStore::memory_fallback(
        HistoryStore::memory_only(&config.codex_home, false),
        Vec::new(),
    )));
    let completion = collect_initial_refresh_completion(
        &config,
        &cache,
        &history,
        0,
        &HistorySourceSelection::AllIncluded,
        None,
    );
    assert_eq!(
        completion.worker_failure,
        Some(RefreshWorkerFailure::RolloutCachePoisoned)
    );
    assert!(completion.result.is_none());
    assert!(
        !history.is_poisoned(),
        "cache failure must not poison unrelated history"
    );
}

#[test]
fn poisoned_refresh_stops_post_delete_refresh_even_with_a_visible_dialog() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let config = CollectConfig {
        codex_home: directory.path().to_path_buf(),
        offline: true,
        ..CollectConfig::default()
    };
    for poison_history in [false, true] {
        for show_dialog in [false, true] {
            let cache = Arc::new(Mutex::new(RolloutCache::new()));
            let history = Arc::new(Mutex::new(TuiHistoryStore::memory_fallback(
                HistoryStore::memory_only(&config.codex_home, false),
                Vec::new(),
            )));
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    if poison_history {
                        let _guard = history.lock().unwrap();
                        panic!("interrupt history before the post-delete refresh");
                    } else {
                        let _guard = cache.lock().unwrap();
                        panic!("interrupt cache before the post-delete refresh");
                    }
                }))
                .is_err()
            );
            let mut app = interaction_test_app(1, 1);
            let displayed_tasks = app.snapshot.tasks.clone();
            let displayed_turns = app.snapshot.turns.clone();
            app.session_management.force_local_refresh = true;
            app.refresh_retry_not_before = Some(Instant::now() + Duration::from_secs(60));
            if show_dialog {
                app.session_management.dialog = Some(SessionDeleteDialog {
                    target: SessionDeleteTarget {
                        thread_id: "deleted-thread".to_string(),
                        title: "Deleted session".to_string(),
                        codex_home: config.codex_home.clone(),
                    },
                    phase: DeletePhase::Failed("delete failed".to_string()),
                });
            }
            let termination = TerminationSignal::for_test();
            let (refresh_sender, refresh_receiver) = mpsc::channel();
            let (resume_sender, resume_receiver) = mpsc::channel();
            let (remote_sender, remote_receiver) = mpsc::channel();
            let context = RunLoopContext {
                termination: &termination,
                refresh_sender: &refresh_sender,
                refresh_receiver: &refresh_receiver,
                resume_sender: &resume_sender,
                resume_receiver: &resume_receiver,
                remote_sender: &remote_sender,
                remote_receiver: &remote_receiver,
            };
            let mut worker = RefreshWorker::default();
            assert!(start_refresh_if_due(
                &mut app,
                &config,
                &context,
                &cache,
                &history,
                &mut worker,
            ));
            assert!(app.refresh_stopped);
            assert!(!app.worker_running);
            assert!(worker.handle.is_none());
            assert!(refresh_receiver.try_recv().is_err());
            assert_eq!(app.snapshot.tasks, displayed_tasks);
            assert_eq!(app.snapshot.turns, displayed_turns);
            assert!(!start_refresh_if_due(
                &mut app,
                &config,
                &context,
                &cache,
                &history,
                &mut worker,
            ));
        }
    }
}

#[test]
fn stopped_refresh_source_control_keeps_geometry_without_an_active_shortcut() {
    for (view, theme, width) in [
        (View::Summary, Theme::Dark, 120),
        (View::Summary, Theme::Light, 60),
        (View::Trends, Theme::Dark, 120),
        (View::Trends, Theme::Light, 60),
    ] {
        let mut app = interaction_test_app(1, 1);
        app.view = view;
        app.theme = theme;
        app.history_local_source_id =
            Some("node-22222222222222222222222222222222".parse().unwrap());
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let active_hitbox = app.history_source_control_hitbox;
        assert!(!active_hitbox.is_empty());
        app.record_refresh_worker_failure(
            RefreshWorkerFailure::HistoryStatePoisoned,
            Instant::now(),
            Utc::now(),
        );
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hitbox = app.history_source_control_hitbox;
        assert_eq!(hitbox, active_hitbox);
        let shortcut = &terminal.backend().buffer()[(hitbox.x + 1, hitbox.y)];
        assert_eq!(shortcut.symbol(), "S");
        assert_ne!(shortcut.fg, theme.palette().accent);
        assert!(!shortcut.modifier.contains(Modifier::BOLD));
        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('S'))));
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.right() - 1,
                hitbox.y
            ),
        ));
        assert_eq!(
            app.history_source_selection,
            HistorySourceSelection::AllIncluded
        );
        assert!(!app.history_source_loading);
        assert!(!app.history_source_query_pending);
    }
}
