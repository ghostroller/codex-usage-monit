use super::*;

enum HistoryBarrierOutcome {
    Complete,
    Panic,
}

struct InitialRefreshHarness {
    _directory: tempfile::TempDir,
    config: CollectConfig,
    app: App,
    cache: Arc<Mutex<RolloutCache>>,
    history: Arc<Mutex<TuiHistoryStore>>,
    worker: RefreshWorker,
    completions: Receiver<RefreshCompletion>,
    resume_history: mpsc::SyncSender<()>,
}

impl InitialRefreshHarness {
    fn start(outcome: HistoryBarrierOutcome) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let sessions = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let observed_at = Utc::now() - ChronoDuration::minutes(5);
        let events = [
            serde_json::json!({"timestamp":observed_at,"type":"session_meta","payload":{"id":RESUMABLE_THREAD_ID,"source":"desktop"}}),
            serde_json::json!({"timestamp":observed_at,"type":"event_msg","payload":{"type":"task_started","turn_id":"initial-turn"}}),
            serde_json::json!({"timestamp":observed_at+ChronoDuration::seconds(1),"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":17,"total_tokens":17},"last_token_usage":{"input_tokens":17,"total_tokens":17}}}}),
            serde_json::json!({"timestamp":observed_at+ChronoDuration::seconds(2),"type":"event_msg","payload":{"type":"task_complete","turn_id":"initial-turn"}}),
        ];
        std::fs::write(
            sessions.join("rollout-initial.jsonl"),
            events
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        std::fs::write(
            codex_home.join("session_index.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"id":RESUMABLE_THREAD_ID,"thread_name":"Initial preview task","updated_at":observed_at})
            ),
        )
        .unwrap();
        let runtime = HistoryRuntime::new(
            directory.path().join("state/history-v1"),
            &codex_home,
            false,
        )
        .unwrap();
        let profile_lease = acquire_runtime_profile_lease(&runtime).unwrap();
        let mut store = TuiHistoryStore::deferred_runtime(runtime, profile_lease);
        let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
        let (resume_history, resume_receiver) = mpsc::sync_channel(0);
        store.initial_history_load_hook = Some(Box::new(move || {
            entered_sender.send(()).unwrap();
            // This timeout only detects a deadlocked test. Success is proved
            // while this barrier remains closed, without an elapsed-time bound.
            resume_receiver
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            if matches!(outcome, HistoryBarrierOutcome::Panic) {
                panic!("interrupt initial history while its mutex is held");
            }
        }));
        let history = Arc::new(Mutex::new(store));
        let tracker = StartupLoadProgressTracker::default();
        let mut cache = RolloutCache::new();
        cache.set_startup_progress(tracker.clone());
        let cache = Arc::new(Mutex::new(cache));
        let config = CollectConfig {
            codex_home,
            offline: true,
            rollout_cache_dir: None,
            deleted_sessions_dir: Some(directory.path().join("deleted-sessions")),
            ..CollectConfig::default()
        };
        let mut app = App::new(initial_loading_result(&config), Theme::Dark);
        install_empty_remote_sources_fixture(&mut app, directory.path());
        app.attach_startup_progress(tracker);
        app.initial_bootstrap_pending = true;
        app.history_initialization_pending = true;
        app.history_source_loading = true;
        app.session_management.config = Some(config.clone());
        let termination = TerminationSignal::for_test();
        let (sender, completions) = mpsc::channel();
        let (resume_sender, resume_receiver) = mpsc::channel();
        let (remote_sender, remote_receiver) = mpsc::channel();
        let context = RunLoopContext {
            termination: &termination,
            refresh_sender: &sender,
            refresh_receiver: &completions,
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
        entered_receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        Self {
            _directory: directory,
            config,
            app,
            cache,
            history,
            worker,
            completions,
            resume_history,
        }
    }

    fn poll(&mut self) -> bool {
        poll_refresh_worker(
            &mut self.app,
            &self.config,
            &self.completions,
            &mut self.worker,
            &self.cache,
            &self.history,
        )
    }

    fn assert_preview(&mut self) {
        assert!(self.poll());
        self.app.poll_startup_progress(Instant::now());
        assert_eq!(self.app.snapshot.tasks.len(), 1);
        assert_eq!(self.app.snapshot.tasks[0].token_usage.total_tokens, 17);
        assert!(self.app.worker_running);
        assert!(self.app.history_initialization_pending);
        assert!(self.app.history_source_loading);
        assert!(self.app.startup_progress_tracker.is_some());
        assert_eq!(
            self.app.startup_progress.stage,
            StartupLoadStage::LoadingHistory
        );
        assert!(self.app.startup_progress_label(false).is_some());
        assert!(self.app.history.half_hour_buckets.is_empty());
        assert!(self.completions.try_recv().is_err());
        assert!(!self.worker.handle.as_ref().unwrap().is_finished());
        assert!(matches!(
            self.history.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
    }

    fn finish(&mut self) {
        self.resume_history.send(()).unwrap();
        // The test releases the barrier before joining; production UI polling
        // never waits for an unfinished worker.
        self.worker.handle.take().unwrap().join().unwrap();
        assert!(self.poll());
    }
}

#[test]
fn initial_snapshot_is_interactive_while_history_is_blocked_then_completes() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let mut harness = InitialRefreshHarness::start(HistoryBarrierOutcome::Complete);
    harness.assert_preview();
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut harness.app))
        .unwrap();
    assert_eq!(harness.app.snapshot.tasks[0].title, "Initial preview task");
    let rendered = buffer_rect_text(terminal.backend().buffer(), Rect::new(0, 0, 120, 32));
    // The task column may truncate the title after its project/source prefix.
    // Check the visible row as well as the complete model value above.
    assert!(rendered.contains("- | desktop"), "{rendered}");
    assert!(!handle_key_event(
        &mut harness.app,
        key_event(KeyCode::Char('U'))
    ));
    assert_eq!(harness.app.view, View::Summary);
    terminal
        .draw(|frame| render(frame, &mut harness.app))
        .unwrap();
    let overview = harness.app.view_tabs_hitbox.unwrap().tabs[View::Overview.index()];
    assert!(handle_mouse_event(
        &mut harness.app,
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            overview.x,
            overview.y
        ),
    ));
    assert_eq!(harness.app.view, View::Overview);
    assert!(harness.completions.try_recv().is_err());
    assert!(harness.app.history_initialization_pending);
    assert!(!harness.worker.handle.as_ref().unwrap().is_finished());

    harness.finish();

    assert!(!harness.app.worker_running);
    assert!(!harness.app.history_initialization_pending);
    assert!(!harness.app.history_source_loading);
    assert!(harness.app.startup_progress_tracker.is_none());
    assert_eq!(
        harness.app.startup_progress.stage,
        StartupLoadStage::Complete
    );
    assert_eq!(harness.app.snapshot.tasks[0].token_usage.total_tokens, 17);
    assert_eq!(
        harness
            .app
            .history
            .half_hour_buckets
            .iter()
            .map(|bucket| bucket.token_usage.total_tokens)
            .sum::<u64>(),
        17
    );
    assert!(harness.app.remote_source_history_store.is_some());
    assert!(!harness.app.history.read_only);
    let durable = harness
        .history
        .lock()
        .unwrap()
        .load_since_with_staged_selected(
            &HistorySourceSelection::AllIncluded,
            history_view_since(Utc::now()),
        );
    assert_eq!(
        durable
            .history
            .half_hour_buckets
            .iter()
            .map(|bucket| bucket.token_usage.total_tokens)
            .sum::<u64>(),
        17
    );
}

#[test]
fn initial_snapshot_survives_poison_without_publishing_history() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let mut harness = InitialRefreshHarness::start(HistoryBarrierOutcome::Panic);
    harness.assert_preview();
    let displayed_tasks = harness.app.snapshot.tasks.clone();
    let displayed_turns = harness.app.snapshot.turns.clone();
    harness.finish();

    assert!(harness.history.is_poisoned());
    assert!(harness.app.refresh_stopped);
    assert!(!harness.app.worker_running);
    assert!(!harness.app.history_initialization_pending);
    assert_eq!(harness.app.snapshot.tasks, displayed_tasks);
    assert_eq!(harness.app.snapshot.turns, displayed_turns);
    assert!(harness.app.history.half_hour_buckets.is_empty());
    assert!(harness.app.history.read_only);
    assert!(harness.app.remote_source_history_store.is_none());
    assert!(
        harness
            .app
            .snapshot
            .warnings
            .iter()
            .any(|warning| warning.contains("history state was interrupted"))
    );
}

#[test]
fn initial_snapshot_and_final_completion_in_one_poll_finish_history() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let mut harness = InitialRefreshHarness::start(HistoryBarrierOutcome::Complete);
    // Leave the preview queued until the terminal completion is also queued.
    // Polling must apply them in order and finish, never restore loading state.
    harness.finish();
    assert_eq!(harness.app.snapshot.tasks[0].token_usage.total_tokens, 17);
    assert!(!harness.app.worker_running);
    assert!(!harness.app.history_initialization_pending);
    assert!(!harness.app.history_source_loading);
    assert_eq!(
        harness.app.startup_progress.stage,
        StartupLoadStage::Complete
    );
    assert!(harness.app.startup_progress_tracker.is_none());
    assert!(harness.app.remote_source_history_store.is_some());
    assert!(harness.worker.initial_snapshot.is_none());
    assert!(!harness.poll());
}

#[test]
fn initial_snapshot_shutdown_detaches_without_waiting_for_blocked_history() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let mut harness = InitialRefreshHarness::start(HistoryBarrierOutcome::Complete);
    harness.assert_preview();
    assert!(handle_key_event(
        &mut harness.app,
        key_event(KeyCode::Char('q'))
    ));
    harness.worker.detach();
    flush_staged_history_on_exit(&harness.history, &PerfLog::default(), &EventLog::default());
    assert!(harness.worker.handle.is_none());
    assert!(harness.worker.initial_snapshot.is_none());
    assert!(!harness.history.is_poisoned());
    assert!(harness.completions.try_recv().is_err());
    assert!(matches!(
        harness.history.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    // Cleanup only after the exact detach/exit-flush path returned while the
    // worker still held history state. No timing assertion substitutes for it.
    harness.resume_history.send(()).unwrap();
    harness
        .completions
        .recv_timeout(Duration::from_secs(30))
        .unwrap();
}

#[test]
fn initial_snapshot_survives_a_retryable_bootstrap_failure() {
    let directory = tempfile::tempdir().unwrap();
    let config = CollectConfig {
        offline: true,
        ..CollectConfig::default()
    };
    let mut app = interaction_test_app(1, 1);
    install_empty_remote_sources_fixture(&mut app, directory.path());
    app.history_initialization_pending = true;
    app.worker_running = true;
    let tasks = app.snapshot.tasks.clone();
    let turns = app.snapshot.turns.clone();
    apply_refresh_completion(&mut app, initial_refresh_panic_completion(&config));
    assert_eq!(app.snapshot.tasks, tasks);
    assert_eq!(app.snapshot.turns, turns);
    assert!(app.initial_bootstrap_pending);
    assert!(app.history_initialization_pending);
    assert!(app.refresh_retry_not_before.is_some());
    assert!(!app.worker_running);
    assert!(!app.refresh_stopped);
}
