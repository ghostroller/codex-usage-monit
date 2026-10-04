use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::json;

use super::*;

fn backfill_fixture(directory: &Path) -> CollectConfig {
    let sessions = directory.join("sessions");
    fs::create_dir(&sessions).unwrap();
    for (name, days, thread_id, total) in [
        ("recent", 1, RESUMABLE_THREAD_ID, 100),
        ("older", 20, "019f52ac-7a9f-7fd1-8dda-e775ef950786", 200),
    ] {
        let observed_at = Utc::now() - ChronoDuration::days(days);
        let timestamp = observed_at.to_rfc3339();
        let records = [
            json!({"timestamp": timestamp, "type": "session_meta", "payload": {
                "id": thread_id, "cwd": "/backfill-memory", "source": "cli"
            }}),
            json!({"timestamp": timestamp, "type": "event_msg", "payload": {
                "type": "task_started", "turn_id": name
            }}),
            json!({"timestamp": timestamp, "type": "turn_context", "payload": {
                "turn_id": name, "model": "gpt-5.6-sol"
            }}),
            json!({"timestamp": timestamp, "type": "token_usage_record", "payload": {
                "thread_id": thread_id, "turn_id": name,
                "response_id": name, "usage": {
                    "input_tokens": total - 20, "output_tokens": 20,
                    "total_tokens": total
                }
            }}),
            json!({"timestamp": timestamp, "type": "event_msg", "payload": {
                "type": "task_complete", "turn_id": name
            }}),
        ];
        let mut contents = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        contents.push('\n');
        let path = sessions.join(format!("rollout-{name}.jsonl"));
        fs::write(&path, contents).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(observed_at.into())
            .unwrap();
    }
    CollectConfig {
        codex_home: directory.to_path_buf(),
        offline: true,
        ..CollectConfig::default()
    }
}

#[test]
fn summary_backfill_memory_releases_live_lock_and_preserves_recent_selection() {
    let directory = tempfile::tempdir().unwrap();
    let config = backfill_fixture(directory.path());
    let cache = Arc::new(Mutex::new(RolloutCache::new()));
    let recent = collect_snapshot_cached(&config, None, false, &mut cache.lock().unwrap());
    assert_eq!(recent.snapshot.tasks.len(), 1);
    assert_eq!(recent.snapshot.tasks[0].token_usage.total_tokens, 100);
    let metrics = cache.lock().unwrap().metrics();
    assert_eq!(metrics.selected_files, 1);

    let backfill = with_summary_backfill_cache(&cache, |backfill_cache| {
        assert!(
            cache.try_lock().is_ok(),
            "the scan must not hold the live lock"
        );
        let result = collect_snapshot_cached(
            &summary_backfill_config(&config),
            None,
            false,
            backfill_cache,
        );
        assert_eq!(backfill_cache.metrics().selected_files, 2);
        assert_eq!(backfill_cache.last_refresh().reused_files, 1);
        assert_eq!(backfill_cache.last_refresh().full_parsed_files, 1);
        result
    });
    assert_eq!(backfill.snapshot.tasks.len(), 2);
    assert_eq!(backfill.snapshot.turns.len(), 2);
    assert_eq!(
        backfill
            .snapshot
            .tasks
            .iter()
            .map(|task| task.token_usage.total_tokens)
            .sum::<u64>(),
        300
    );
    assert_eq!(cache.lock().unwrap().metrics(), metrics);

    let recent = collect_snapshot_cached(&config, None, false, &mut cache.lock().unwrap());
    assert_eq!(recent.snapshot.tasks.len(), 1);
    assert_eq!(recent.snapshot.tasks[0].thread_id, RESUMABLE_THREAD_ID);
    assert_eq!(recent.snapshot.tasks[0].token_usage.total_tokens, 100);
    let refresh = cache.lock().unwrap().last_refresh();
    assert_eq!(refresh.reused_files, 1);
    assert_eq!(refresh.full_parsed_files, 0);
}

#[test]
fn summary_backfill_memory_panic_leaves_live_cache_usable() {
    let directory = tempfile::tempdir().unwrap();
    let config = backfill_fixture(directory.path());
    let cache = Arc::new(Mutex::new(RolloutCache::new()));
    collect_snapshot_cached(&config, None, false, &mut cache.lock().unwrap());
    let metrics = cache.lock().unwrap().metrics();

    let result = catch_unwind(AssertUnwindSafe(|| {
        with_summary_backfill_cache(&cache, |backfill_cache| {
            assert!(cache.try_lock().is_ok());
            collect_snapshot_cached(
                &summary_backfill_config(&config),
                None,
                false,
                backfill_cache,
            );
            panic!("failed backfill after widening the scan");
        });
    }));
    assert!(result.is_err());
    assert_eq!(cache.lock().unwrap().metrics(), metrics);
    let recent = collect_snapshot_cached(&config, None, false, &mut cache.lock().unwrap());
    assert_eq!(recent.snapshot.tasks.len(), 1);
    assert_eq!(recent.snapshot.tasks[0].token_usage.total_tokens, 100);
    assert_eq!(cache.lock().unwrap().last_refresh().reused_files, 1);
}

#[test]
fn summary_backfill_memory_scheduling_and_quit_do_not_wait_for_live_cache() {
    #[cfg(windows)]
    let _signal_test_guard = WINDOWS_TERMINATION_TEST_LOCK.lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let config = backfill_fixture(directory.path());
    let cache = Arc::new(Mutex::new(RolloutCache::new()));
    let history = Arc::new(Mutex::new(TuiHistoryStore::memory_fallback(
        HistoryStore::memory_only(&config.codex_home, false),
        Vec::new(),
    )));
    // Hold the cache until the UI has both scheduled the worker and quit. A
    // synchronous fork or joining a blocked worker cannot reach this handshake.
    let guard = cache.lock().unwrap();
    let worker_cache = Arc::clone(&cache);
    let (ready_sender, ready_receiver) = mpsc::channel();
    let scheduler = thread::spawn(move || {
        let mut app = App::new(initial_loading_result(&config), Theme::Dark);
        app.view = View::Summary;
        app.summary_range = SummaryRange::ThirtyDays;
        app.summary_backfill_pending = true;
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
        let started = start_refresh_if_due(
            &mut app,
            &config,
            &context,
            &worker_cache,
            &history,
            &mut worker,
        );
        let quit = handle_key_event(&mut app, key_event(KeyCode::Char('q')));
        drop(worker);
        ready_sender
            .send((
                started,
                app.summary_backfill_running,
                quit,
                refresh_receiver,
            ))
            .unwrap();
    });
    let ready = ready_receiver.recv_timeout(Duration::from_secs(5));
    // Also unblock and clean up on regression, before inspecting the result.
    drop(guard);
    scheduler.join().unwrap();
    let (started, backfill_running, quit, completion) =
        ready.expect("UI scheduling and quit must finish while the cache remains locked");
    assert!(started);
    assert!(backfill_running);
    assert!(quit);
    let completion = completion.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(completion.summary_backfill);
    assert_eq!(completion.worker_failure, None);
}
