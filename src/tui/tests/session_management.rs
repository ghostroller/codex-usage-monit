use super::testkit::TuiHarness;
use super::*;
use crate::deleted_sessions::{DeletedSessionRecord, DeletionStatus};
use crate::tui::session_management::{
    DELETE_ACTION_SECTION, DeleteCompletion, DeletePhase, SessionRecordLoadError,
};

const SIZES: [(u16, u16); 4] = [(120, 40), (80, 24), (60, 24), (32, 14)];

fn harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness =
        TuiHarness::from_snapshot(interaction_test_app(4, 2).snapshot, width, height, theme);
    harness.app.session_management.config = Some(CollectConfig {
        codex_home: harness.app.snapshot.codex_home.clone(),
        codex_bin: Some(PathBuf::from("/nonexistent/ui-deletion-fixture")),
        ..CollectConfig::default()
    });
    harness
}

fn record(task: &TaskRecord, at: DateTime<Utc>, status: DeletionStatus) -> DeletedSessionRecord {
    DeletedSessionRecord {
        thread_id: task.thread_id.clone(),
        title: task.title.clone(),
        parent_thread_id: task.parent_thread_id.clone(),
        cwd: task.cwd.clone(),
        source: task.source.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        captured_at: at,
        deleted_at: Some(at),
        status,
        calls: Vec::new(),
        turns: Vec::new(),
        agent_interactions: Vec::new(),
    }
}

fn install_deleted(harness: &mut TuiHarness) {
    let at = harness.app.snapshot.as_of;
    let records = vec![
        record(
            &harness.app.snapshot.tasks[1],
            at - ChronoDuration::minutes(1),
            DeletionStatus::Deleted,
        ),
        record(&harness.app.snapshot.tasks[2], at, DeletionStatus::Deleted),
        record(&harness.app.snapshot.tasks[3], at, DeletionStatus::Unknown),
    ];
    harness.app.apply_deleted_session_records(Ok(records));
    harness.app.reconcile_task_filter(false);
    harness.render();
}

#[test]
fn session_management_restored_raw_session_returns_to_the_normal_list() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let config = CollectConfig {
        codex_home: home.clone(),
        deleted_sessions_dir: Some(temporary.path().join("ledger")),
        offline: true,
        ..CollectConfig::default()
    };
    let mut harness = harness(120, 40, Theme::Dark);
    let task = harness.app.snapshot.tasks[1].clone();
    let captured_at = Utc::now() - ChronoDuration::minutes(1);
    crate::deleted_sessions::prepare_delete(
        &config,
        "restore-case",
        &[record(&task, captured_at, DeletionStatus::Pending)],
    )
    .unwrap();
    crate::deleted_sessions::set_deletion_outcome(
        &config,
        "restore-case",
        &task.thread_id,
        DeletionStatus::Deleted,
        Utc::now(),
    )
    .unwrap();
    let cache = Arc::new(Mutex::new(RolloutCache::new()));
    cache.lock().unwrap().scan(&config, Utc::now()).unwrap();
    harness
        .app
        .apply_deleted_session_records(Ok(crate::tui::session_management::load_records(
            &config, &cache,
        )
        .unwrap()));
    assert!(harness.app.is_deleted_task(&task));
    let restored = home
        .join("sessions")
        .join(format!("rollout-{}.jsonl", task.thread_id));
    std::fs::write(
        restored,
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp":Utc::now(), "type":"session_meta",
                "payload":{"id":task.thread_id,"source":"cli"},
            })
        ),
    )
    .unwrap();
    cache.lock().unwrap().invalidate_after_session_mutation();
    cache.lock().unwrap().scan(&config, Utc::now()).unwrap();
    harness
        .app
        .apply_deleted_session_records(Ok(crate::tui::session_management::load_records(
            &config, &cache,
        )
        .unwrap()));
    assert!(!harness.app.is_deleted_task(&task));
    assert!(
        harness
            .app
            .filtered_task_rows()
            .iter()
            .all(|row| row.kind != TaskRowKind::DeletedRoot)
    );
    assert_eq!(
        crate::deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .len(),
        1,
        "restoration changes display, not the durable deletion audit"
    );
}

fn reconciliation_fixture(
    offline: bool,
) -> (tempfile::TempDir, CollectConfig, Arc<Mutex<RolloutCache>>) {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let config = CollectConfig {
        codex_home: home,
        codex_bin: Some(PathBuf::from(
            "/nonexistent/read-only-reconciliation-fixture",
        )),
        deleted_sessions_dir: Some(temporary.path().join("ledger")),
        event_log: EventLog::open(
            Some(temporary.path().join("events.jsonl")),
            LogLevel::Debug,
            false,
        ),
        offline,
        ..CollectConfig::default()
    };
    let app = interaction_test_app(4, 0);
    let at = app.snapshot.as_of;
    crate::deleted_sessions::prepare_delete(
        &config,
        "readonly-ui",
        &[
            record(&app.snapshot.tasks[1], at, DeletionStatus::Pending),
            record(&app.snapshot.tasks[2], at, DeletionStatus::Pending),
        ],
    )
    .unwrap();
    for (thread, status) in [
        ("task-thread-1", DeletionStatus::Deleted),
        ("task-thread-2", DeletionStatus::Unknown),
    ] {
        crate::deleted_sessions::set_deletion_outcome(&config, "readonly-ui", thread, status, at)
            .unwrap();
    }
    (temporary, config, Arc::new(Mutex::new(RolloutCache::new())))
}

#[test]
fn session_management_live_raw_unknown_receipt_does_not_repeat_inventory_reads() {
    let (_temporary, config, cache) = reconciliation_fixture(false);
    std::fs::write(
        config.codex_home.join("sessions/live.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({"timestamp":Utc::now(),"type":"session_meta",
            "payload":{"id":"task-thread-2","source":"cli"}})
        ),
    )
    .unwrap();
    cache.lock().unwrap().scan(&config, Utc::now()).unwrap();
    assert!(cache.lock().unwrap().has_live_thread("task-thread-2"));
    let records =
        crate::tui::session_management::load_records_with_reconcile(&config, &cache, |_| {
            panic!("a visible raw session already proves it is not deleted")
        })
        .unwrap();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .any(|record| record.thread_id == "task-thread-2"
                && record.status == DeletionStatus::Unknown)
    );
}

#[test]
fn session_management_offline_record_worker_never_requests_reconciliation() {
    let (_temporary, config, cache) = reconciliation_fixture(true);
    let records =
        crate::tui::session_management::load_records_with_reconcile(&config, &cache, |_| {
            panic!("offline loading must not start an App Server or any reconciliation")
        })
        .unwrap();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .any(|record| record.status == DeletionStatus::Unknown)
    );
    let mut app = interaction_test_app(4, 0);
    app.apply_deleted_session_records(Ok(records));
    assert!(app.is_deleted_task(&app.snapshot.tasks[1]));
    assert!(!app.is_deleted_task(&app.snapshot.tasks[2]));
}

fn poison_rollout_cache(cache: &Arc<Mutex<RolloutCache>>) {
    let cache = Arc::clone(cache);
    assert!(
        thread::spawn(move || {
            let mut cache = cache.lock().unwrap();
            cache.invalidate_after_session_mutation();
            panic!("interrupt session cache mutation while holding its lock");
        })
        .join()
        .is_err()
    );
}

#[test]
fn session_management_poisoned_cache_refuses_online_and_offline_record_loading() {
    for offline in [false, true] {
        let (_temporary, config, cache) = reconciliation_fixture(offline);
        poison_rollout_cache(&cache);
        let error =
            crate::tui::session_management::load_records_with_reconcile(&config, &cache, |_| {
                panic!("a poisoned cache cannot authorize reconciliation")
            })
            .unwrap_err();
        assert_eq!(error, SessionRecordLoadError::RolloutCachePoisoned);
        let stored = crate::deleted_sessions::load_deleted_sessions(&config).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].status, DeletionStatus::Deleted);
        assert_eq!(stored[1].status, DeletionStatus::Unknown);
    }
}

#[test]
fn session_management_reconciliation_poison_refuses_every_callback_outcome() {
    for reconciliation in [
        Ok(vec!["task-thread-2".to_owned()]),
        Ok(Vec::new()),
        Err("fixture inventory is incomplete".to_owned()),
    ] {
        let (_temporary, config, cache) = reconciliation_fixture(false);
        let error =
            crate::tui::session_management::load_records_with_reconcile(&config, &cache, |_| {
                assert!(
                    cache.try_lock().is_ok(),
                    "RPCs must not hold the cache lock"
                );
                poison_rollout_cache(&cache);
                reconciliation
            })
            .unwrap_err();
        assert_eq!(error, SessionRecordLoadError::RolloutCachePoisoned);
        assert!(cache.is_poisoned());
        let stored = crate::deleted_sessions::load_deleted_sessions(&config).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].status, DeletionStatus::Deleted);
        assert_eq!(stored[1].status, DeletionStatus::Unknown);
    }
}

#[test]
fn session_management_ledger_failure_remains_distinct_from_cache_poison() {
    let (temporary, mut config, cache) = reconciliation_fixture(false);
    let invalid_root = temporary.path().join("ledger-file");
    std::fs::write(&invalid_root, b"not a ledger directory").unwrap();
    config.deleted_sessions_dir = Some(invalid_root);
    let error =
        crate::tui::session_management::load_records_with_reconcile(&config, &cache, |_| {
            panic!("an unreadable ledger cannot authorize reconciliation")
        })
        .unwrap_err();
    assert!(matches!(error, SessionRecordLoadError::Ledger(_)));
    assert!(!cache.is_poisoned());
}

#[test]
fn session_management_reconciliation_error_keeps_confirmed_records_and_deduplicates_warning() {
    let (temporary, config, cache) = reconciliation_fixture(false);
    for _ in 0..2 {
        let records =
            crate::tui::session_management::load_records_with_reconcile(&config, &cache, |_| {
                assert!(
                    cache.try_lock().is_ok(),
                    "the cache mutex must not be held across read-only RPCs"
                );
                Err("fixture inventory is incomplete".into())
            })
            .unwrap();
        let mut app = interaction_test_app(4, 0);
        app.apply_deleted_session_records(Ok(records));
        assert!(
            app.is_deleted_task(&app.snapshot.tasks[1]),
            "a failed recheck cannot undo a different confirmed deletion"
        );
        assert!(
            !app.is_deleted_task(&app.snapshot.tasks[2]),
            "uncertainty is not a successful deletion"
        );
    }
    let events = std::fs::read_to_string(temporary.path().join("events.jsonl")).unwrap();
    let warnings = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["event"] == "session_delete.reconciliation")
        .count();
    assert_eq!(
        warnings, 1,
        "repeated inventory failure is one observed warning"
    );
    let stored = crate::deleted_sessions::load_deleted_sessions(&config).unwrap();
    assert!(
        stored
            .iter()
            .any(|record| record.thread_id == "task-thread-1"
                && record.status == DeletionStatus::Deleted)
    );
    assert!(
        stored
            .iter()
            .any(|record| record.thread_id == "task-thread-2"
                && record.status == DeletionStatus::Unknown)
    );
}

#[test]
fn session_management_readonly_recovery_reloads_ledger_without_repeating_a_completed_operation() {
    let (_temporary, config, cache) = reconciliation_fixture(false);
    let records =
        crate::tui::session_management::load_records_with_reconcile(&config, &cache, |config| {
            assert!(cache.try_lock().is_ok());
            crate::deleted_sessions::set_deletion_outcome(
                config,
                "readonly-ui",
                "task-thread-2",
                DeletionStatus::Deleted,
                Utc::now(),
            )
            .unwrap();
            Ok(vec!["task-thread-2".into()])
        })
        .unwrap();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record.status == DeletionStatus::Deleted)
    );
    let mut app = interaction_test_app(4, 0);
    app.apply_deleted_session_records(Ok(records));
    assert!(app.is_deleted_task(&app.snapshot.tasks[1]));
    assert!(app.is_deleted_task(&app.snapshot.tasks[2]));
    let records =
        crate::tui::session_management::load_records_with_reconcile(&config, &cache, |_| {
            panic!("all recorded outcomes are confirmed; no further reconciliation is necessary")
        })
        .unwrap();
    assert_eq!(records.len(), 2);
}

#[test]
fn session_management_same_named_deleted_sessions_keep_distinct_uuid_tails_visible() {
    let ids = [
        "019a1234-1234-7000-8000-000000111111",
        "019a1234-1234-7000-8000-000000222222",
    ];
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = harness(width, height, theme);
            for (index, id) in ids.into_iter().enumerate() {
                harness.app.snapshot.tasks[index + 1].thread_id = id.into();
                harness.app.snapshot.tasks[index + 1].title = "[redacted]".into();
            }
            install_deleted(&mut harness);
            harness.app.select_task(DELETED_TASK_ROOT, true);
            harness.key(KeyCode::Enter);
            for (index, tail) in [(2, "222222"), (1, "111111")] {
                harness.app.select_task(index, true);
                harness.render();
                let frame = harness.frame().snapshot_text();
                assert!(
                    frame.contains(tail),
                    "identifier tail must be visible at {width}x{height}: {frame}"
                );
            }
            harness.key(KeyCode::F(2));
            let detail = harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
                .collect::<String>();
            assert!(
                detail.contains(ids[0]),
                "details retain the complete raw identifier"
            );
        }
    }
}

fn click(harness: &mut TuiHarness, column: u16, row: u16) {
    assert!(handle_mouse_event(
        &mut harness.app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    ));
    harness.render();
}

fn open_deletable(harness: &mut TuiHarness) {
    assert!(harness.app.select_task(0, true));
    harness.app.focus = Focus::Tasks;
    assert!(harness.app.open_entity_detail());
    harness.render();
    assert!(harness.app.entity_delete_available());
}

fn ready_confirmation(harness: &mut TuiHarness) {
    open_deletable(harness);
    assert!(harness.app.request_session_delete());
    let target = harness
        .app
        .entity_detail
        .as_ref()
        .unwrap()
        .delete_target
        .as_ref()
        .unwrap()
        .thread_id
        .clone();
    let prepared = crate::session_delete::prepared_session_deletion_for_test(
        &target,
        harness.app.snapshot.as_of,
    );
    let (sender, receiver) = mpsc::channel();
    sender
        .send(DeleteCompletion::Prepared(Ok(Box::new(prepared))))
        .unwrap();
    harness.app.session_management.receiver = Some(receiver);
    assert!(harness.app.poll_session_delete());
    harness.render();
    assert!(matches!(
        harness
            .app
            .session_management
            .dialog
            .as_ref()
            .unwrap()
            .phase,
        DeletePhase::Confirm(_)
    ));
}

#[test]
fn session_management_deleted_root_sorts_by_latest_deletion_time() {
    for mode in [TaskListMode::Flat, TaskListMode::Tree] {
        for (offset_minutes, expected) in [
            (1, [DELETED_TASK_ROOT, 0, 3]),
            (0, [0, DELETED_TASK_ROOT, 3]),
            (-3, [0, DELETED_TASK_ROOT, 3]),
            (-10, [0, 3, DELETED_TASK_ROOT]),
        ] {
            let mut harness = harness(80, 24, Theme::Dark);
            harness.app.task_list_mode = mode;
            let at = harness.app.snapshot.as_of;
            for (index, age) in [(0, 0), (1, 100), (2, 120), (3, 5)] {
                harness.app.snapshot.tasks[index].updated_at =
                    Some(at - ChronoDuration::minutes(age));
            }
            let latest_deletion = at + ChronoDuration::minutes(offset_minutes);
            harness.app.apply_deleted_session_records(Ok(vec![
                record(
                    &harness.app.snapshot.tasks[1],
                    latest_deletion - ChronoDuration::minutes(1),
                    DeletionStatus::Deleted,
                ),
                record(
                    &harness.app.snapshot.tasks[2],
                    latest_deletion,
                    DeletionStatus::Deleted,
                ),
            ]));
            harness.app.reconcile_task_filter(false);
            let indices = |harness: &TuiHarness| {
                harness
                    .app
                    .filtered_task_rows()
                    .iter()
                    .map(|row| row.index)
                    .collect::<Vec<_>>()
            };
            assert_eq!(indices(&harness), expected);
            harness.app.select_task(DELETED_TASK_ROOT, true);
            harness.key(KeyCode::Enter);
            let mut expanded = expected.to_vec();
            let root = expanded
                .iter()
                .position(|index| *index == DELETED_TASK_ROOT)
                .unwrap();
            expanded.splice(root + 1..root + 1, [2, 1]);
            assert_eq!(
                indices(&harness),
                expanded,
                "deleted children stay with the root in newest-deletion order"
            );
            harness.key(KeyCode::Enter);
            assert_eq!(indices(&harness), expected);
        }
    }
}

#[test]
fn session_management_deleted_root_never_splits_a_recent_normal_subtree() {
    let mut harness = harness(120, 40, Theme::Dark);
    harness.app.snapshot = interaction_test_app(5, 0).snapshot;
    harness.app.task_list_mode = TaskListMode::Tree;
    let at = harness.app.snapshot.as_of;
    for (index, age) in [(0, 0), (1, 8), (2, 10), (3, 20), (4, 30)] {
        harness.app.snapshot.tasks[index].updated_at = Some(at - ChronoDuration::minutes(age));
    }
    let parent = harness.app.snapshot.tasks[3].thread_id.clone();
    harness.app.snapshot.tasks[0].parent_thread_id = Some(parent.clone());
    harness.app.snapshot.tasks[0].source = Some("subagent".into());
    harness.app.apply_deleted_session_records(Ok(vec![
        record(
            &harness.app.snapshot.tasks[1],
            at - ChronoDuration::minutes(6),
            DeletionStatus::Deleted,
        ),
        record(
            &harness.app.snapshot.tasks[4],
            at - ChronoDuration::minutes(5),
            DeletionStatus::Deleted,
        ),
    ]));
    for normal_expanded in [false, true] {
        harness.app.expanded_task_threads.clear();
        if normal_expanded {
            harness.app.expanded_task_threads.insert(parent.clone());
        }
        for deleted_expanded in [false, true] {
            harness.app.session_management.expanded = deleted_expanded;
            let mut expected = vec![3];
            if normal_expanded {
                expected.push(0);
            }
            expected.push(DELETED_TASK_ROOT);
            if deleted_expanded {
                expected.extend([4, 1]);
            }
            expected.push(2);
            assert_eq!(
                harness
                    .app
                    .filtered_task_rows()
                    .iter()
                    .map(|row| row.index)
                    .collect::<Vec<_>>(),
                expected,
                "the recent child ranks its complete normal branch ahead of the deletion group"
            );
        }
    }
}

#[test]
fn session_management_deleted_root_sort_uses_matching_children_after_filtering() {
    let mut harness = harness(80, 24, Theme::Dark);
    let at = harness.app.snapshot.as_of;
    harness.app.snapshot.tasks[0].title = "matched new task".into();
    harness.app.snapshot.tasks[3].title = "matched old task".into();
    harness.app.snapshot.tasks[3].updated_at = Some(at - ChronoDuration::minutes(5));
    harness.app.snapshot.tasks[1].title = "hidden newest deletion".into();
    harness.app.snapshot.tasks[2].title = "matched old deletion".into();
    harness.app.apply_deleted_session_records(Ok(vec![
        record(
            &harness.app.snapshot.tasks[1],
            at + ChronoDuration::minutes(1),
            DeletionStatus::Deleted,
        ),
        record(
            &harness.app.snapshot.tasks[2],
            at - ChronoDuration::minutes(10),
            DeletionStatus::Deleted,
        ),
    ]));
    assert_eq!(
        harness.app.filtered_task_rows()[0].kind,
        TaskRowKind::DeletedRoot
    );
    harness.app.task_search = "matched".into();
    let rows = harness.app.filtered_task_rows();
    assert_eq!(
        rows.iter().map(|row| row.index).collect::<Vec<_>>(),
        [0, 3, DELETED_TASK_ROOT]
    );
    assert_eq!(rows[2].hidden_descendants, [2]);
}

#[test]
fn session_management_deleted_root_is_independent_of_flat_tree_and_counts_usage_once() {
    for mode in [TaskListMode::Flat, TaskListMode::Tree] {
        let mut harness = harness(120, 40, Theme::Dark);
        harness.app.task_list_mode = mode;
        install_deleted(&mut harness);
        let rows = harness.app.filtered_task_rows();
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == TaskRowKind::Session)
                .count(),
            2
        );
        let root = rows
            .iter()
            .find(|row| row.kind == TaskRowKind::DeletedRoot)
            .unwrap();
        assert_eq!(root.kind, TaskRowKind::DeletedRoot);
        assert_eq!(root.index, DELETED_TASK_ROOT);
        assert!(root.collapsed);
        assert_eq!(root.hidden_descendants.len(), 2);
        let before = harness
            .app
            .snapshot
            .tasks
            .iter()
            .map(|task| task.token_usage.total_tokens)
            .sum::<u64>();
        let usage =
            aggregate_task_row_usage(&harness.app.snapshot, WindowScope::FiveHours, root, false);
        assert_eq!(
            usage.token_usage.total_tokens, 5,
            "each deleted session contributes only its own counters"
        );
        harness.app.select_task(DELETED_TASK_ROOT, true);
        harness.key(KeyCode::Enter);
        let rows = harness.app.filtered_task_rows();
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == TaskRowKind::DeletedRoot)
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == TaskRowKind::DeletedSession)
                .map(|row| row.index)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(
            harness
                .app
                .snapshot
                .tasks
                .iter()
                .map(|task| task.token_usage.total_tokens)
                .sum::<u64>(),
            before
        );
        assert!(
            harness.app.selected_task_record().is_none(),
            "the root is never a real task"
        );
        assert!(!harness.app.entity_detail_available());
        assert!(!harness.app.open_control_available());
        assert_eq!(harness.app.selected_task_raw_turn_count(), 0);
        harness.app.activate_open();
        assert!(harness.app.pending_resume.is_none());
        harness.key(KeyCode::Enter);
        assert!(
            harness
                .app
                .filtered_task_rows()
                .iter()
                .find(|row| row.kind == TaskRowKind::DeletedRoot)
                .unwrap()
                .collapsed
        );
    }
}

#[test]
fn session_management_local_scoped_copies_are_grouped_once_and_foreign_identity_stays_separate() {
    let mut harness = harness(120, 40, Theme::Dark);
    install_deleted(&mut harness);
    let local: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
    harness.app.history_local_source_id = Some(local.clone());
    let mut copy = harness.app.snapshot.tasks[1].clone();
    copy.thread_id = format!("{}@{}", copy.thread_id, local.as_str());
    harness.app.snapshot.tasks.push(copy.clone());
    let mut foreign = copy;
    foreign.thread_id = "task-thread-1@node-22222222222222222222222222222222".into();
    assert!(!harness.app.is_deleted_task(&foreign));
    harness.app.snapshot.tasks.push(foreign);
    let root = harness
        .app
        .filtered_task_rows()
        .into_iter()
        .find(|row| row.kind == TaskRowKind::DeletedRoot)
        .unwrap();
    assert_eq!(root.kind, TaskRowKind::DeletedRoot);
    assert_eq!(root.hidden_descendants.len(), 2);
    assert_eq!(
        aggregate_task_row_usage(&harness.app.snapshot, WindowScope::FiveHours, &root, false)
            .token_usage
            .total_tokens,
        5
    );
    harness.app.snapshot.tasks.remove(1);
    let rows = harness.app.filtered_task_rows();
    let root = rows
        .iter()
        .find(|row| row.kind == TaskRowKind::DeletedRoot)
        .unwrap();
    assert_eq!(
        root.hidden_descendants.len(),
        2,
        "the known local scoped row remains grouped after raw-row eviction"
    );
}

#[test]
fn session_management_deleted_filters_and_selection_survive_refresh_without_treating_unknown_as_deleted()
 {
    let mut harness = harness(80, 24, Theme::Dark);
    install_deleted(&mut harness);
    assert!(!harness.app.is_deleted_task(&harness.app.snapshot.tasks[3]));
    harness.app.select_task(DELETED_TASK_ROOT, true);
    let anchor = harness.app.overview_refresh_anchor();
    harness.app.snapshot.tasks.reverse();
    let latest_deletion = harness.app.snapshot.as_of + ChronoDuration::minutes(1);
    for record in harness.app.session_management.deleted.values_mut() {
        record.deleted_at = Some(latest_deletion);
    }
    harness.app.restore_overview_refresh_anchor(anchor);
    assert_eq!(harness.app.selected_task, DELETED_TASK_ROOT);
    assert_eq!(
        harness.app.filtered_task_rows()[0].kind,
        TaskRowKind::DeletedRoot
    );
    harness.app.task_search = "task 2".into();
    harness.app.reconcile_task_filter(false);
    let rows = harness.app.filtered_task_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].hidden_descendants.len(), 1);
    harness.app.set_task_source_filter(TaskSourceFilter::Cli);
    assert!(harness.app.filtered_task_rows().is_empty());
    harness.app.set_task_source_filter(TaskSourceFilter::All);
    harness.app.task_search = "no matching deleted record".into();
    harness.app.reconcile_task_filter(false);
    assert!(harness.app.filtered_task_rows().is_empty());
}

#[test]
fn session_management_root_whole_label_mouse_and_shortcut_styles_work_in_compact_themes() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = harness(width, height, theme);
            install_deleted(&mut harness);
            harness.app.select_task(DELETED_TASK_ROOT, true);
            harness.render();
            let marker = *harness
                .app
                .task_tree_marker_hitboxes
                .iter()
                .find(|marker| marker.task_index == DELETED_TASK_ROOT)
                .unwrap();
            let (symbol, foreground, modifiers) =
                harness.cell_style(marker.area.x + 1, marker.area.y);
            assert_eq!(symbol, "↵");
            assert_eq!(foreground, theme.palette().accent);
            assert!(modifiers.contains(Modifier::BOLD));
            for offset in 0..marker.area.width {
                let marker = *harness
                    .app
                    .task_tree_marker_hitboxes
                    .iter()
                    .find(|marker| marker.task_index == DELETED_TASK_ROOT)
                    .unwrap();
                click(
                    &mut harness,
                    marker.area.x + offset.min(marker.area.width - 1),
                    marker.area.y,
                );
                assert!(harness.app.session_management.expanded);
                harness.key(KeyCode::Enter);
                assert!(!harness.app.session_management.expanded);
            }
            harness.key(KeyCode::Enter);
            let leaf = harness
                .app
                .filtered_task_rows()
                .iter()
                .find(|row| row.kind == TaskRowKind::DeletedSession)
                .unwrap()
                .index;
            harness.app.select_task(leaf, true);
            harness.key(KeyCode::Enter);
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().title,
                "Deleted session usage"
            );
            assert!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .delete_target
                    .is_none()
            );
            assert!(!harness.app.entity_detail_loading());
            let body = harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
                .collect::<String>();
            assert!(body.contains("Deleted by monit"));
            assert!(body.contains("Usage"));
            assert!(!body.contains("/tmp/project"));
            assert!(!body.contains("Message preview"));
            harness.key(KeyCode::Esc);
            assert_eq!(harness.app.selected_task, leaf);
            harness.app.focus_turns();
            assert_eq!(harness.app.focus, Focus::Tasks);
            harness.app.activate_open();
            assert!(harness.app.pending_resume.is_none());
        }
    }
}

#[test]
fn session_management_delete_entry_is_local_session_only_and_printable_shortcuts_obey_focus() {
    for status in [
        TaskStatus::Running,
        TaskStatus::WaitingApproval,
        TaskStatus::WaitingInput,
        TaskStatus::Unknown,
        TaskStatus::Stale,
    ] {
        let mut harness = harness(80, 24, Theme::Dark);
        harness.app.snapshot.tasks[0].status = status;
        harness.key(KeyCode::F(2));
        assert!(!harness.app.entity_delete_available(), "{status:?}");
        assert!(harness.app.entity_detail_hitbox.unwrap().delete.is_empty());
    }
    let mut harness = harness(80, 24, Theme::Dark);
    harness.app.local_snapshot.tasks.clear();
    harness.key(KeyCode::F(2));
    assert!(!harness.app.entity_delete_available());
    harness.key(KeyCode::Esc);
    harness.app.local_snapshot = harness.app.snapshot.clone();
    harness.app.focus_turns();
    harness.key(KeyCode::F(2));
    assert!(
        !harness.app.entity_delete_available(),
        "turn detail cannot delete its session implicitly"
    );
    harness.key(KeyCode::Esc);
    harness.app.focus_tasks();
    harness.app.begin_task_search();
    harness.key(KeyCode::Char('x'));
    assert_eq!(harness.app.task_search, "x");
    assert!(!harness.app.session_delete_visible());
}

fn stop_session_management_refresh(harness: &mut TuiHarness) {
    harness.app.record_refresh_worker_failure(
        RefreshWorkerFailure::RolloutCachePoisoned,
        Instant::now(),
        Utc::now(),
    );
    assert!(harness.app.refresh_stopped);
}

#[test]
fn session_management_stopped_refresh_disables_delete_entry_and_stale_confirmation() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in [(80, 24), (32, 14)] {
            let mut entry = harness(width, height, theme);
            open_deletable(&mut entry);
            let delete = entry.app.entity_detail_hitbox.unwrap().delete;
            stop_session_management_refresh(&mut entry);
            entry.render();
            assert!(!entry.app.entity_delete_available());
            let (symbol, foreground, modifiers) = entry.cell_style(delete.x + 1, delete.y);
            assert_eq!(symbol, "X");
            assert_ne!(foreground, theme.palette().accent);
            assert!(!modifiers.contains(Modifier::BOLD));
            entry.key(KeyCode::Char('x'));
            click(&mut entry, delete.x, delete.y);
            assert!(!entry.app.request_session_delete());
            assert!(
                !entry
                    .app
                    .toggle_entity_detail_section(DELETE_ACTION_SECTION)
            );
            assert!(!entry.app.session_delete_visible());
            assert!(entry.app.session_management.receiver.is_none());

            let mut disabled = harness(width, height, theme);
            ready_confirmation(&mut disabled);
            let confirm = disabled.app.session_delete_hitbox.unwrap().confirm;
            stop_session_management_refresh(&mut disabled);
            disabled.render();
            assert!(matches!(
                disabled
                    .app
                    .session_management
                    .dialog
                    .as_ref()
                    .unwrap()
                    .phase,
                DeletePhase::Confirm(_)
            ));
            assert!(
                disabled
                    .app
                    .session_delete_hitbox
                    .unwrap()
                    .confirm
                    .is_empty()
            );
            let (symbol, foreground, modifiers) = disabled.cell_style(confirm.x + 1, confirm.y);
            assert_eq!(symbol, "↵");
            assert_ne!(foreground, theme.palette().accent);
            assert!(!modifiers.contains(Modifier::BOLD));

            for use_mouse in [false, true] {
                let mut confirmation = harness(width, height, theme);
                ready_confirmation(&mut confirmation);
                let confirm = confirmation.app.session_delete_hitbox.unwrap().confirm;
                stop_session_management_refresh(&mut confirmation);
                // Deliberately use the old hitbox before another render.
                if use_mouse {
                    click(&mut confirmation, confirm.x, confirm.y);
                } else {
                    confirmation.key(KeyCode::Enter);
                }
                assert!(matches!(
                    confirmation
                        .app
                        .session_management
                        .dialog
                        .as_ref()
                        .unwrap()
                        .phase,
                    DeletePhase::Failed(_)
                ));
                assert!(confirmation.app.session_management.receiver.is_none());
                confirmation.render();
                assert!(
                    confirmation
                        .app
                        .session_delete_hitbox
                        .unwrap()
                        .confirm
                        .is_empty()
                );
                let (symbol, foreground, modifiers) =
                    confirmation.cell_style(confirm.x + 1, confirm.y);
                assert_eq!(symbol, "↵");
                assert_ne!(foreground, theme.palette().accent);
                assert!(!modifiers.contains(Modifier::BOLD));
            }
        }
    }
}

#[test]
fn session_management_stopped_refresh_refuses_queued_preparation_and_execution() {
    for execute in [false, true] {
        let mut harness = harness(80, 24, Theme::Dark);
        if execute {
            ready_confirmation(&mut harness);
            harness.key(KeyCode::Enter);
        } else {
            open_deletable(&mut harness);
            assert!(harness.app.request_session_delete());
        }
        stop_session_management_refresh(&mut harness);
        harness.app.session_management.force_local_refresh = true;
        harness.app.start_session_delete_work();
        assert!(matches!(
            harness
                .app
                .session_management
                .dialog
                .as_ref()
                .unwrap()
                .phase,
            DeletePhase::Failed(_)
        ));
        assert!(harness.app.session_management.receiver.is_none());
        assert!(!harness.app.session_management.force_local_refresh);
        harness.app.start_session_delete_work();
        assert!(harness.app.session_management.receiver.is_none());
    }
}

#[test]
fn session_management_stopped_refresh_never_confirms_a_late_prepared_result() {
    for dispatch_before_result in [false, true] {
        let mut harness = harness(80, 24, Theme::Dark);
        open_deletable(&mut harness);
        assert!(harness.app.request_session_delete());
        let dialog = harness.app.session_management.dialog.as_mut().unwrap();
        let prepared = crate::session_delete::prepared_session_deletion_for_test(
            &dialog.target.thread_id,
            harness.app.snapshot.as_of,
        );
        dialog.phase = DeletePhase::Preparing;
        let (sender, receiver) = mpsc::channel();
        harness.app.session_management.receiver = Some(receiver);
        stop_session_management_refresh(&mut harness);
        if dispatch_before_result {
            harness.app.start_session_delete_work();
            assert!(
                sender
                    .send(DeleteCompletion::Prepared(Ok(Box::new(prepared))))
                    .is_err()
            );
            assert!(!harness.app.poll_session_delete());
        } else {
            sender
                .send(DeleteCompletion::Prepared(Ok(Box::new(prepared))))
                .unwrap();
            assert!(harness.app.poll_session_delete());
        }
        assert!(matches!(
            harness
                .app
                .session_management
                .dialog
                .as_ref()
                .unwrap()
                .phase,
            DeletePhase::Failed(_)
        ));
        harness.app.confirm_session_delete();
        harness.app.start_session_delete_work();
        assert!(harness.app.session_management.receiver.is_none());
        assert!(!harness.app.session_management.force_local_refresh);
    }
}

#[test]
fn session_management_stopped_refresh_preserves_executing_receipts_without_retry() {
    for outcome in [
        Some(Ok(crate::session_delete::DeletionOutcome {
            deleted_threads: vec!["task-thread-0".into()],
        })),
        Some(Err(
            "Deletion result is unknown; no automatic retry".to_owned()
        )),
        None,
    ] {
        let success = matches!(outcome, Some(Ok(_)));
        let mut harness = harness(80, 24, Theme::Dark);
        ready_confirmation(&mut harness);
        harness.key(KeyCode::Enter);
        harness
            .app
            .session_management
            .dialog
            .as_mut()
            .unwrap()
            .phase = DeletePhase::Executing;
        let (sender, receiver) = mpsc::channel();
        harness.app.session_management.receiver = Some(receiver);
        stop_session_management_refresh(&mut harness);
        harness.app.start_session_delete_work();
        harness.app.cancel_session_delete();
        assert!(harness.app.session_management.receiver.is_some());
        assert!(matches!(
            harness
                .app
                .session_management
                .dialog
                .as_ref()
                .unwrap()
                .phase,
            DeletePhase::Executing
        ));
        if let Some(outcome) = outcome {
            sender.send(DeleteCompletion::Executed(outcome)).unwrap();
        }
        drop(sender);
        assert!(harness.app.poll_session_delete());
        assert!(harness.app.session_management.receiver.is_none());
        assert!(!harness.app.session_management.force_local_refresh);
        assert!(harness.app.summary_cache.is_none());
        if success {
            assert!(!harness.app.session_delete_visible());
        } else {
            assert!(matches!(
                harness
                    .app
                    .session_management
                    .dialog
                    .as_ref()
                    .unwrap()
                    .phase,
                DeletePhase::Failed(_)
            ));
        }
        harness.app.start_session_delete_work();
        assert!(harness.app.session_management.receiver.is_none());
    }
}

#[test]
fn session_management_delete_footer_whole_labels_freeze_target_and_wait_for_refresh() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = harness(width, height, theme);
            open_deletable(&mut harness);
            let area = harness.app.entity_detail_hitbox.unwrap().delete;
            assert!(!area.is_empty());
            let (symbol, foreground, modifiers) = harness.cell_style(area.x + 1, area.y);
            assert_eq!(symbol, "X");
            assert_eq!(foreground, theme.palette().accent);
            assert!(modifiers.contains(Modifier::BOLD));
            for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
                handle_key_event(
                    &mut harness.app,
                    KeyEvent::new(KeyCode::Char('x'), modifier),
                );
                assert!(!harness.app.session_delete_visible());
            }
            for offset in 0..area.width {
                click(&mut harness, area.x + offset, area.y);
                let original = harness
                    .app
                    .session_management
                    .dialog
                    .as_ref()
                    .unwrap()
                    .target
                    .thread_id
                    .clone();
                assert!(matches!(
                    harness
                        .app
                        .session_management
                        .dialog
                        .as_ref()
                        .unwrap()
                        .phase,
                    DeletePhase::QueuedPrepare
                ));
                harness.app.selected_task = 1;
                harness.app.worker_running = true;
                harness.app.start_session_delete_work();
                assert!(harness.app.session_management.receiver.is_none());
                assert_eq!(
                    harness
                        .app
                        .session_management
                        .dialog
                        .as_ref()
                        .unwrap()
                        .target
                        .thread_id,
                    original
                );
                harness.key(KeyCode::Enter);
                assert!(matches!(
                    harness
                        .app
                        .session_management
                        .dialog
                        .as_ref()
                        .unwrap()
                        .phase,
                    DeletePhase::QueuedPrepare
                ));
                harness.key(KeyCode::Esc);
                harness.app.worker_running = false;
                harness.app.selected_task = 0;
                harness.render();
            }
            assert!(
                harness
                    .app
                    .toggle_entity_detail_section(DELETE_ACTION_SECTION)
            );
            assert!(harness.app.session_delete_visible());
            harness.key(KeyCode::Esc);
        }
    }
}

#[test]
fn session_management_confirm_requires_prepared_result_and_a_second_explicit_action() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = harness(width, height, theme);
            ready_confirmation(&mut harness);
            let area = harness.app.session_delete_hitbox.unwrap().confirm;
            assert!(!area.is_empty(), "{width}x{height}");
            let (symbol, foreground, modifiers) = harness.cell_style(area.x + 1, area.y);
            assert_eq!(symbol, "↵");
            assert_eq!(foreground, theme.palette().accent);
            assert!(modifiers.contains(Modifier::BOLD));
            let original = harness
                .app
                .session_management
                .dialog
                .as_ref()
                .unwrap()
                .target
                .thread_id
                .clone();
            harness.app.selected_task = 1;
            handle_key_event(
                &mut harness.app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            );
            assert!(matches!(
                harness
                    .app
                    .session_management
                    .dialog
                    .as_ref()
                    .unwrap()
                    .phase,
                DeletePhase::Confirm(_)
            ));
            harness.key(KeyCode::Enter);
            let DeletePhase::QueuedExecute(prepared) = &harness
                .app
                .session_management
                .dialog
                .as_ref()
                .unwrap()
                .phase
            else {
                panic!("confirmation must queue, not synchronously execute")
            };
            assert_eq!(prepared.root_thread_id, original);
            assert!(
                harness.app.entity_detail.is_none(),
                "the original content reader must be cancelled before execution"
            );
            assert!(harness.app.session_management.receiver.is_none());
        }
    }
}

#[test]
fn session_management_confirm_and_cancel_mouse_cover_the_entire_label() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut initial = harness(width, height, theme);
            ready_confirmation(&mut initial);
            let controls = initial.app.session_delete_hitbox.unwrap();
            for offset in 0..controls.confirm.width {
                let mut harness = harness(width, height, theme);
                ready_confirmation(&mut harness);
                let area = harness.app.session_delete_hitbox.unwrap().confirm;
                click(&mut harness, area.x + offset, area.y);
                assert!(matches!(
                    harness
                        .app
                        .session_management
                        .dialog
                        .as_ref()
                        .unwrap()
                        .phase,
                    DeletePhase::QueuedExecute(_)
                ));
            }
            for offset in 0..controls.cancel.width {
                let mut harness = harness(width, height, theme);
                ready_confirmation(&mut harness);
                let area = harness.app.session_delete_hitbox.unwrap().cancel;
                click(&mut harness, area.x + offset, area.y);
                assert!(!harness.app.session_delete_visible());
                assert!(harness.app.entity_detail.is_some());
            }
        }
    }
}

#[test]
fn session_management_result_refreshes_on_success_and_never_retries_uncertain_deletion() {
    for success in [true, false] {
        let mut harness = harness(80, 24, Theme::Dark);
        ready_confirmation(&mut harness);
        harness.key(KeyCode::Enter);
        harness
            .app
            .session_management
            .dialog
            .as_mut()
            .unwrap()
            .phase = DeletePhase::Executing;
        let (sender, receiver) = mpsc::channel();
        sender
            .send(DeleteCompletion::Executed(if success {
                Ok(crate::session_delete::DeletionOutcome {
                    deleted_threads: vec!["task-thread-0".into()],
                })
            } else {
                Err("Deletion result is unknown; no automatic retry".into())
            }))
            .unwrap();
        harness.app.session_management.receiver = Some(receiver);
        assert!(harness.app.poll_session_delete());
        assert!(harness.app.session_management.force_local_refresh);
        assert!(harness.app.summary_cache.is_none());
        assert!(harness.app.entity_detail.is_none());
        if success {
            assert!(!harness.app.session_delete_visible());
        } else {
            assert!(matches!(
                harness
                    .app
                    .session_management
                    .dialog
                    .as_ref()
                    .unwrap()
                    .phase,
                DeletePhase::Failed(_)
            ));
            harness.key(KeyCode::Enter);
            harness.key(KeyCode::Char('x'));
            assert!(harness.app.session_management.receiver.is_none());
            assert!(harness.app.session_management.deleted.is_empty());
        }
    }
}

#[test]
fn session_management_tiny_terminals_do_not_offer_unreadable_confirmation() {
    for (width, height) in [(1, 1), (8, 4), (16, 8)] {
        let mut harness = harness(width, height, Theme::Light);
        open_deletable(&mut harness);
        assert!(harness.app.request_session_delete());
        harness.render();
        harness.key(KeyCode::Enter);
        assert!(matches!(
            harness
                .app
                .session_management
                .dialog
                .as_ref()
                .unwrap()
                .phase,
            DeletePhase::QueuedPrepare
        ));
    }
}
