use super::testkit::TuiHarness;
use super::*;
use crate::deleted_sessions::{DeletedSessionRecord, DeletionStatus};
use crate::tui::session_management::{DELETE_ACTION_SECTION, DeleteCompletion, DeletePhase};

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
        .apply_deleted_session_records(crate::tui::session_management::load_records(
            &config, &cache,
        ));
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
        .apply_deleted_session_records(crate::tui::session_management::load_records(
            &config, &cache,
        ));
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
        let root = rows.last().unwrap();
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
        assert!(harness.app.filtered_task_rows().last().unwrap().collapsed);
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
    let root = harness.app.filtered_task_rows().pop().unwrap();
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
    let root = rows.last().unwrap();
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
    harness.app.restore_overview_refresh_anchor(anchor);
    assert_eq!(harness.app.selected_task, DELETED_TASK_ROOT);
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
