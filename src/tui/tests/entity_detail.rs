use super::testkit::{TuiHarness, gallery_directory};
use super::*;

const SIZES: [(u16, u16); 4] = [(120, 40), (80, 24), (60, 24), (32, 14)];

fn detail_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    TuiHarness::from_snapshot(interaction_test_app(3, 2).snapshot, width, height, theme)
}

fn popup_text(app: &App) -> String {
    app.entity_detail
        .as_ref()
        .expect("details must be open")
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry(harness: &TuiHarness, focus: Focus) -> Rect {
    match focus {
        Focus::Tasks => harness.app.task_controls_hitbox.unwrap().details,
        Focus::Turns => harness.app.turn_controls_hitbox.unwrap().details,
        _ => panic!("expected a table focus"),
    }
}

fn click_at(harness: &mut TuiHarness, area: Rect, column: u16) -> bool {
    let handled = handle_mouse_event(
        &mut harness.app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), column, area.y),
    );
    harness.render();
    handled
}

fn mouse_at(harness: &mut TuiHarness, kind: MouseEventKind, column: u16, row: u16) -> bool {
    let handled = handle_mouse_event(&mut harness.app, mouse_event(kind, column, row));
    harness.render();
    handled
}

fn assert_binding(harness: &TuiHarness, area: Rect, key: &str, active: bool) {
    assert!(!area.is_empty(), "{key} control must be visible");
    let binding = key
        .chars()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    let mut bindings = 0;
    for column in area.x..area.right() {
        let (symbol, foreground, modifier) = harness.cell_style(column, area.y);
        let index = column.saturating_sub(area.x.saturating_add(1));
        if column > area.x && usize::from(index) < binding.len() {
            bindings += 1;
            assert_eq!(symbol, binding[usize::from(index)]);
            assert_eq!(foreground == harness.app.theme.palette().accent, active);
            assert_eq!(modifier.contains(Modifier::BOLD), active);
        } else {
            assert_ne!(
                foreground,
                harness.app.theme.palette().accent,
                "accents {symbol:?}"
            );
        }
    }
    assert_eq!(
        bindings,
        binding.len(),
        "all exact shortcut graphemes in {area:?}"
    );
}

fn open(harness: &mut TuiHarness) {
    assert!(!harness.key(KeyCode::F(2)));
    assert!(harness.app.entity_detail.is_some());
}

fn open_with_lines(harness: &mut TuiHarness, lines: Vec<Line<'static>>) {
    assert!(!handle_key_event(
        &mut harness.app,
        key_event(KeyCode::F(2))
    ));
    harness
        .app
        .entity_detail
        .as_mut()
        .expect("details must open")
        .lines = lines;
    // Install the long fixture before the first render builds its wrapped-line cache.
    harness.render();
}

#[test]
fn entity_detail_entries_style_active_bindings_and_keep_search_geometry() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            for focus in [Focus::Tasks, Focus::Turns] {
                let mut harness = detail_harness(width, height, theme);
                if focus == Focus::Turns {
                    harness.app.focus_turns();
                }
                harness.render();
                let initial = entry(&harness, focus);
                assert_binding(&harness, initial, "F2", true);
                let other = if focus == Focus::Tasks {
                    Focus::Turns
                } else {
                    Focus::Tasks
                };
                assert_binding(&harness, entry(&harness, other), "F2", false);

                // Selecting another item preserves the entire clickable label.
                harness.key(KeyCode::Down);
                assert_eq!(entry(&harness, focus), initial);

                match focus {
                    Focus::Tasks => harness.app.begin_task_search(),
                    Focus::Turns => harness.app.begin_turn_search(),
                    _ => unreachable!(),
                }
                harness.render();
                assert_eq!(entry(&harness, focus), initial);
                assert_binding(&harness, initial, "F2", false);
                harness.key(KeyCode::Char('d'));
                assert!(harness.app.entity_detail.is_none());
                let query = if focus == Focus::Tasks {
                    &harness.app.task_search
                } else {
                    &harness.app.turn_search
                };
                assert_eq!(query, "d");
                harness.key(KeyCode::F(2));
                assert!(harness.app.entity_detail.is_none());
            }
        }
    }
}

#[test]
fn entity_detail_entries_and_popup_controls_are_whole_label_clickable() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            for focus in [Focus::Tasks, Focus::Turns] {
                let mut harness = detail_harness(width, height, theme);
                if focus == Focus::Turns {
                    harness.app.focus_turns();
                }
                harness.render();
                let control = entry(&harness, focus);
                for column in control.x..control.right() {
                    assert!(click_at(&mut harness, control, column));
                    assert!(harness.app.entity_detail.is_some());
                    harness.key(KeyCode::Esc);
                    assert!(harness.app.entity_detail.is_none());
                    assert_eq!(entry(&harness, focus), control);
                }

                // Enough independent lines to exercise both scroll buttons at every size.
                let lines = (0..100)
                    .map(|index| Line::from(format!("detail row {index}")))
                    .collect();
                open_with_lines(&mut harness, lines);
                let hitbox = harness.app.entity_detail_hitbox.unwrap();
                for column in hitbox.down.x..hitbox.down.right() {
                    harness.key(KeyCode::Home);
                    assert!(click_at(&mut harness, hitbox.down, column));
                    assert!(harness.app.entity_detail.as_ref().unwrap().offset > 0);
                }
                for column in hitbox.up.x..hitbox.up.right() {
                    harness.key(KeyCode::End);
                    let before = harness.app.entity_detail.as_ref().unwrap().offset;
                    assert!(click_at(&mut harness, hitbox.up, column));
                    assert!(harness.app.entity_detail.as_ref().unwrap().offset < before);
                }
                for column in hitbox.back.x..hitbox.back.right() {
                    assert!(click_at(&mut harness, hitbox.back, column));
                    assert!(harness.app.entity_detail.is_none());
                    open(&mut harness);
                }
            }
        }
    }
}

#[test]
fn entity_detail_modal_scrolls_without_running_background_shortcuts() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = detail_harness(width, height, theme);
            harness.app.selected_task = 1;
            harness.app.selected_turn = 1;
            harness.app.focus_turns();
            harness.render();
            let initial = harness.state();
            let lines = (0..100)
                .map(|index| Line::from(format!("detail row {index}")))
                .collect();
            open_with_lines(&mut harness, lines);
            let controls = harness.app.entity_detail_hitbox.unwrap();
            for (area, binding, active) in [
                (controls.up, "↑", false),
                (controls.down, "↓", true),
                (controls.back, "←", true),
            ] {
                assert_binding(&harness, area, binding, active);
            }
            assert!(!harness.app.shortcuts_active());
            for code in ['1', 'U', 'T', 'R', 'V', 'd'] {
                assert!(!harness.key(KeyCode::Char(code)));
                assert_eq!(harness.app.theme, theme);
                assert_eq!(harness.state(), initial);
                assert!(!harness.app.quit_requested);
            }

            harness.key(KeyCode::Down);
            assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 1);
            harness.key(KeyCode::PageDown);
            assert!(harness.app.entity_detail.as_ref().unwrap().offset > 1);
            harness.key(KeyCode::End);
            let max_offset = harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .line_count
                .saturating_sub(usize::from(controls.content.height));
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                max_offset
            );
            assert_binding(&harness, controls.up, "↑", true);
            assert_binding(&harness, controls.down, "↓", false);
            harness.key(KeyCode::Down);
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                max_offset
            );
            harness.key(KeyCode::PageUp);
            assert!(harness.app.entity_detail.as_ref().unwrap().offset < max_offset);
            harness.key(KeyCode::Home);
            harness.key(KeyCode::Up);
            assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 0);
            let after = harness.app.entity_detail_hitbox.unwrap();
            assert_eq!(after.content, controls.content);
            assert_eq!(after.up, controls.up);
            assert_eq!(after.down, controls.down);
            assert_eq!(after.back, controls.back);
            harness.key(KeyCode::Left);
            assert!(harness.app.entity_detail.is_none());
            assert_eq!(harness.state(), initial);
            open(&mut harness);
            assert!(!harness.key(KeyCode::Char('q')));
            assert!(harness.app.entity_detail.is_none());
            assert!(!harness.app.quit_requested);
            assert_eq!(harness.state(), initial);
        }
    }
}

#[test]
fn entity_detail_wheel_and_drag_reach_the_last_line_at_compact_sizes() {
    for (width, height) in SIZES {
        let mut harness = detail_harness(width, height, Theme::Dark);
        let lines = (0..100)
            .map(|index| Line::from(format!("scroll detail row {index:03}")))
            .collect();
        open_with_lines(&mut harness, lines);
        let hitbox = harness.app.entity_detail_hitbox.unwrap();
        let scrollbar = hitbox.scrollbar.expect("long details require a scrollbar");
        assert_eq!(
            scrollbar.max_offset,
            100 - usize::from(hitbox.content.height)
        );
        assert_eq!(scrollbar.track.y, hitbox.content.y);
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::ScrollDown,
            hitbox.content.x,
            hitbox.content.y
        ));
        assert!(harness.app.entity_detail.as_ref().unwrap().offset > 0);
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::ScrollUp,
            hitbox.content.x,
            hitbox.content.y
        ));
        assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 0);
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.thumb.x,
            scrollbar.thumb.y
        ));
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::Drag(MouseButton::Left),
            scrollbar.track.x,
            scrollbar.track.bottom() - 1
        ));
        mouse_at(
            &mut harness,
            MouseEventKind::Up(MouseButton::Left),
            scrollbar.track.x,
            scrollbar.track.bottom() - 1,
        );
        assert_eq!(
            harness.app.entity_detail.as_ref().unwrap().offset,
            scrollbar.max_offset
        );
        assert!(
            harness
                .frame()
                .snapshot_text()
                .contains("scroll detail row 099")
        );
        assert!(harness.app.scroll_drag.is_none());
    }
}

#[test]
fn entity_detail_keeps_full_unicode_title_id_and_path_available_by_scrolling() {
    let mut harness = detail_harness(60, 24, Theme::Dark);
    let full_title = format!("{} TITLE-END", "会话内容 👩‍💻 ".repeat(24));
    let full_id = format!("thread-{}-ID-END", "0123456789abcdef".repeat(5));
    let full_path = format!("/tmp/{}/PATH-END", "目录workspace/".repeat(14));
    let old_id = harness.app.snapshot.tasks[0].thread_id.clone();
    harness.app.snapshot.tasks[0].title = full_title.clone();
    harness.app.snapshot.tasks[0].thread_id = full_id.clone();
    harness.app.snapshot.tasks[0].cwd = Some(full_path.clone().into());
    for turn in &mut harness.app.snapshot.turns {
        if turn.thread_id == old_id {
            turn.thread_id = full_id.clone();
        }
    }
    harness.render();
    open(&mut harness);
    let source = popup_text(&harness.app);
    assert!(source.contains(&full_title));
    assert!(source.contains(&full_id));
    assert!(source.contains(&full_path));
    let mut visible = String::new();
    loop {
        let viewport = harness.app.entity_detail_hitbox.unwrap().content;
        for row in viewport.y..viewport.bottom() {
            let text = (viewport.x..viewport.right())
                .map(|column| harness.cell_style(column, row).0)
                .collect::<String>();
            visible.push_str(text.trim_end());
        }
        let before = harness.app.entity_detail.as_ref().unwrap().offset;
        harness.key(KeyCode::Down);
        if harness.app.entity_detail.as_ref().unwrap().offset == before {
            break;
        }
    }
    for marker in ["TITLE-END", "ID-END", "PATH-END"] {
        assert!(
            visible.contains(marker),
            "full {marker} must be reachable without truncation"
        );
    }
    for theme in [Theme::Dark, Theme::Light] {
        harness.app.theme = theme;
        for (width, height) in [(8, 5), (3, 2), (2, 1), (1, 1)] {
            harness.resize(width, height);
            harness.key(KeyCode::End);
            harness.key(KeyCode::Home);
            assert!(harness.app.entity_detail.is_some());
        }
    }
    harness.key(KeyCode::Esc);
    assert!(harness.app.entity_detail.is_none());
}

#[test]
fn entity_detail_turn_shows_exact_token_components_and_api_pricing_coverage() {
    for (width, height, theme, gallery_name) in [
        (120, 40, Theme::Dark, "entity-detail-dark-120x40"),
        (60, 24, Theme::Light, "entity-detail-light-60x24"),
    ] {
        let mut harness = detail_harness(width, height, theme);
        let usage = TokenUsage {
            input_tokens: 1_234,
            cached_input_tokens: 234,
            cache_write_input_tokens: 135,
            output_tokens: 432,
            reasoning_output_tokens: 123,
            unclassified_tokens: 34,
            total_tokens: 1_700,
        };
        harness.app.snapshot.turns[0].token_usage = usage;
        harness.app.snapshot.turns[0].service_tier = Some("fast".to_string());
        harness.app.snapshot.turns[0].message_preview = Some("visible user preview".to_string());
        add_window_analysis(&mut harness.app, WindowScope::FiveHours, 1_700, 40.0);
        let analysis = &mut harness.app.snapshot.window_analyses[0];
        analysis.turns[0].usage.token_usage = usage;
        analysis.turns[0].usage.api_equivalent_cost = ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(1_000_000_000_000),
            maximum_pico_usd: PicoUsd::new(2_000_000_000_000),
            observed_samples: 4,
            priced_samples: 2,
            observed_tokens: 1_700,
            priced_tokens: 850,
        };
        harness.app.focus_turns();
        harness.render();
        open(&mut harness);
        let content = popup_text(&harness.app);
        for text in [
            "visible user preview",
            "turn-0-0",
            "model-0",
            "high",
            "fast",
            "Cache write input",
            "Cached input",
            "Reasoning output",
            "135",
            "234",
            "123",
            "API equivalent",
            "Priced token coverage",
            "50.00%",
            "Usage samples",
            "18.96%",
            "10.94%",
            "28.47%",
            "Input / total: 72.59%",
            "Output / total: 25.41%",
        ] {
            assert!(content.contains(text), "missing {text:?}: {content}");
        }
        let directory = gallery_directory();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{gallery_name}.svg")),
            harness.frame().to_svg(gallery_name),
        )
        .unwrap();
    }
}

#[test]
fn entity_detail_derived_turn_statistics_keep_duration_and_count_meanings_distinct() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    let turns = &mut harness.app.snapshot.turns[..2];
    turns[0].status = TurnStatus::Completed;
    turns[0].duration_ms = Some(2_345);
    turns[1].status = TurnStatus::Failed;
    turns[1].duration_ms = Some(9_876);
    turns[1].token_usage = TokenUsage {
        input_tokens: 4_321,
        total_tokens: 4_321,
        ..TokenUsage::default()
    };
    harness.render();
    open(&mut harness);
    let content = popup_text(&harness.app);
    for text in [
        "Turns completed: 1",
        "Turns failed: 1",
        "Completed turn duration sum: 2345 ms",
        "Largest known turn: turn-0-1 (4321 tokens)",
        "Longest completed turn: turn-0-0 (2345 ms",
    ] {
        assert!(
            content.contains(text),
            "missing statistic {text:?}: {content}"
        );
    }
    assert!(!content.contains("Completed turn duration sum: 12221"));
    harness.key(KeyCode::Esc);
    let captured_at = harness.app.snapshot.as_of;
    harness.app.snapshot.turns[0].status = TurnStatus::InProgress;
    harness.app.snapshot.turns[0].started_at =
        Some(captured_at - ChronoDuration::milliseconds(3_210));
    harness.app.snapshot.turns[0].completed_at = None;
    harness.app.snapshot.turns[0].duration_ms = None;
    harness.app.snapshot.turns[0].token_usage = TokenUsage::default();
    harness.app.focus_turns();
    harness.render();
    open(&mut harness);
    let content = popup_text(&harness.app);
    assert!(content.contains("Elapsed at capture: 3210 ms"));
    assert!(content.contains("Cache read / input: unavailable (zero denominator)"));
    assert!(content.contains("Reasoning / output: unavailable (zero denominator)"));
    assert!(content.contains("Input / total: unavailable (zero denominator)"));
    assert!(content.contains("Output / total: unavailable (zero denominator)"));
}

#[test]
fn entity_detail_longx_changes_quota_projection_without_changing_api_cost() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    add_window_analysis(&mut harness.app, WindowScope::FiveHours, 111, 33.0);
    let base = &mut harness.app.snapshot.window_analyses[0];
    base.turns[0].usage.api_equivalent_cost = exact_api_cost(1_000_000_000_000);
    let mut alternative = base.clone();
    alternative.turns[0].usage.estimated_quota_percent = 99.0;
    alternative.turns[0].usage.api_equivalent_cost = exact_api_cost(9_000_000_000_000);
    base.api_long_context = Some(Box::new(alternative));
    harness.app.focus_turns();
    harness.render();
    open(&mut harness);
    let without_longx = popup_text(&harness.app);
    assert!(without_longx.contains("Own API equivalent: $1.0000"));
    harness.key(KeyCode::Esc);
    harness.app.api_long_context_multiplier = true;
    harness.render();
    open(&mut harness);
    let with_longx = popup_text(&harness.app);
    assert!(with_longx.contains("Own API equivalent: $1.0000"));
    assert!(!with_longx.contains("Own API equivalent: $9.0000"));
    assert_ne!(
        without_longx
            .lines()
            .find(|line| line.starts_with("Own estimated quota:")),
        with_longx
            .lines()
            .find(|line| line.starts_with("Own estimated quota:")),
    );
}

#[test]
fn entity_detail_preserves_desktop_filter_binding_and_requires_unmodified_f2() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    harness.app.task_source_filter = TaskSourceFilter::All;
    harness.key(KeyCode::Char('d'));
    assert_eq!(harness.app.task_source_filter, TaskSourceFilter::Desktop);
    assert!(harness.app.entity_detail.is_none());
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        assert!(!handle_key_event(
            &mut harness.app,
            KeyEvent::new(KeyCode::F(2), modifiers)
        ));
        assert!(harness.app.entity_detail.is_none());
    }
    open(&mut harness);
    assert_eq!(harness.app.task_source_filter, TaskSourceFilter::Desktop);
}

#[test]
fn entity_detail_session_aggregation_is_stable_when_subagent_tree_expands() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    set_task_parent(&mut harness.app, 1, 0);
    set_task_parent(&mut harness.app, 2, 1);
    for (task, total) in harness.app.snapshot.tasks.iter_mut().zip([113, 229, 337]) {
        task.token_usage = TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        };
    }
    harness.app.task_list_mode = TaskListMode::Tree;
    harness.app.selected_task = 0;
    harness.render();
    open(&mut harness);
    let collapsed = popup_text(&harness.app);
    for label in [
        "Own cumulative tokens",
        "Delegated cumulative tokens",
        "Total cumulative tokens",
    ] {
        assert!(collapsed.contains(label));
    }
    for total in ["113", "566", "679"] {
        assert!(
            collapsed.contains(total),
            "missing aggregation {total}: {collapsed}"
        );
    }
    harness.key(KeyCode::Esc);
    expand_task_tree(&mut harness.app);
    harness.render();
    open(&mut harness);
    assert_eq!(
        popup_text(&harness.app),
        collapsed,
        "details use stable own/delegated totals"
    );
}

fn historical_summary_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = detail_harness(width, height, theme);
    harness.app.snapshot.tasks.clear();
    harness.app.snapshot.turns.clear();
    let starts_at = harness.app.snapshot.as_of - ChronoDuration::hours(1);
    let groups = [
        ("historical-root", None, Some("historical-turn"), 101),
        ("historical-root", None, None, 202),
        ("historical-child", Some("historical-root"), None, 303),
    ]
    .into_iter()
    .map(|(thread, parent, turn, total)| LocalProjectUsageGroup {
        thread_id: thread.to_string(),
        turn_id: turn.map(str::to_string),
        parent_thread_id: parent.map(str::to_string),
        session_thread_id: Some("historical-root".to_string()),
        session_turn_id: turn.map(str::to_string),
        message_preview: turn.map(|_| "Historical prompt".to_string()),
        turn_started_at: turn.map(|_| starts_at),
        project_id: Some("historical-project".to_string()),
        project_label: Some("history-workspace".to_string()),
        title: Some("Historical title".to_string()),
        source: Some(
            if parent.is_some() {
                "subagent"
            } else {
                "desktop"
            }
            .to_string(),
        ),
        token_usage: TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        },
        call_count: 1,
        ..LocalProjectUsageGroup::default()
    })
    .collect::<Vec<_>>();
    harness.app.history.half_hour_buckets = vec![LocalHalfHourBucket {
        starts_at,
        ends_at: starts_at + ChronoDuration::minutes(15),
        sampled_at: starts_at + ChronoDuration::minutes(15),
        token_usage: TokenUsage {
            input_tokens: 606,
            total_tokens: 606,
            ..TokenUsage::default()
        },
        estimated_cost_units: 0,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 3,
        groups: Vec::new(),
        project_groups: groups,
        partial_reasons: Vec::new(),
    }]
    .into();
    harness.app.summary_range = SummaryRange::SevenDays;
    harness.app.summary_cache = None;
    harness.app.set_view(View::Summary);
    harness.render();
    harness
}

#[test]
fn entity_detail_summary_requires_entity_selection_and_preserves_history_only_metadata() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = historical_summary_harness(width, height, theme);
            let rows = harness.app.summary_rows();
            let project = rows
                .iter()
                .find(|row| row.kind == SummaryRowKind::Project)
                .unwrap();
            harness.app.summary_selected_id = Some(project.id.clone());
            harness.render();
            let entry = harness.app.summary_controls_hitbox.unwrap().details;
            assert_binding(&harness, entry, "F2", false);
            harness.key(KeyCode::F(2));
            assert!(harness.app.entity_detail.is_none());
            click_at(&mut harness, entry, entry.right() - 1);
            assert!(harness.app.entity_detail.is_none());

            harness
                .app
                .summary_expanded_nodes
                .insert(project.id.clone());
            harness.render();
            let rows = harness.app.summary_rows();
            let session = rows
                .iter()
                .find(|row| row.kind == SummaryRowKind::Session)
                .unwrap();
            let session_id = session.id.clone();
            harness.app.summary_selected_id = Some(session_id.clone());
            harness.render();
            assert_eq!(harness.app.summary_controls_hitbox.unwrap().details, entry);
            assert_binding(&harness, entry, "F2", true);
            open(&mut harness);
            let content = popup_text(&harness.app);
            assert!(content.contains("Historical title"));
            assert!(content.contains("historical-root"));
            assert!(content.contains("Status: unavailable"));
            assert!(content.contains("606"));
            assert!(
                content.contains("Largest known user turn (range): historical-turn (101 tokens)")
            );
            assert!(content.contains("Largest priced user-turn subtotal (range): unavailable"));
            harness.key(KeyCode::Esc);
            assert_eq!(
                harness.app.summary_selected_id.as_deref(),
                Some(session_id.as_str())
            );

            harness.app.summary_expanded_nodes.insert(session_id);
            harness.render();
            let rows = harness.app.summary_rows();
            for row in rows.iter().filter(|row| row.kind == SummaryRowKind::Turn) {
                harness.app.summary_selected_id = Some(row.id.clone());
                harness.render();
                open(&mut harness);
                let content = popup_text(&harness.app);
                assert!(content.contains("Model: unavailable"));
                if row.label.starts_with("Unassigned") {
                    assert!(content.contains(&row.label));
                    assert!(content.contains("Turn ID: unavailable"));
                    assert!(!content.contains("Turn ID: historical-turn"));
                } else {
                    assert!(content.contains("historical-turn"));
                    assert!(content.contains("Historical prompt"));
                }
                harness.key(KeyCode::Esc);
            }
        }
    }
}

#[test]
fn entity_detail_summary_rankings_exclude_unassigned_and_unpriced_turns() {
    let mut harness = historical_summary_harness(80, 24, Theme::Dark);
    let bucket = &mut harness.app.history.half_hour_buckets[0];
    bucket.project_groups[0].turn_started_at = Some(bucket.starts_at - ChronoDuration::hours(1));
    bucket.project_groups[0].api_equivalent_cost = ApiCostAmount {
        observed_samples: 1,
        observed_tokens: 101,
        ..ApiCostAmount::default()
    };
    // This unassigned row is larger and more recent than either known user turn.
    bucket.project_groups[1].turn_started_at = Some(bucket.starts_at);
    let latest_known = bucket.starts_at - ChronoDuration::minutes(5);
    let mut priced_zero = bucket.project_groups[0].clone();
    priced_zero.turn_id = Some("priced-zero-turn".to_string());
    priced_zero.session_turn_id = priced_zero.turn_id.clone();
    priced_zero.turn_started_at = Some(latest_known);
    priced_zero.message_preview = Some("Known zero-priced user turn".to_string());
    priced_zero.token_usage = TokenUsage {
        input_tokens: 55,
        total_tokens: 55,
        ..TokenUsage::default()
    };
    priced_zero.api_equivalent_cost = ApiCostAmount {
        observed_samples: 1,
        priced_samples: 1,
        observed_tokens: 55,
        priced_tokens: 55,
        ..ApiCostAmount::default()
    };
    bucket.token_usage.add_assign(priced_zero.token_usage);
    bucket.call_count += 1;
    bucket.project_groups.push(priced_zero);
    harness.app.summary_cache = None;
    harness.render();
    let rows = harness.app.summary_rows();
    let project = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Project)
        .unwrap();
    harness
        .app
        .summary_expanded_nodes
        .insert(project.id.clone());
    harness.render();
    let rows = harness.app.summary_rows();
    let session = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Session)
        .unwrap();
    harness.app.summary_selected_id = Some(session.id.clone());
    harness.render();
    open(&mut harness);
    let content = popup_text(&harness.app);
    assert!(content.contains("Largest known user turn (range): historical-turn (101 tokens)"));
    assert!(content.contains("Largest priced user-turn subtotal (range): priced-zero-turn ("));
    assert!(!content.contains("Largest priced user-turn subtotal (range): historical-turn"));
    assert!(content.contains(&format!(
        "Latest known user-turn start: {}",
        latest_known.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S %:z")
    )));
}

#[test]
fn entity_detail_summary_uses_selected_project_when_one_session_spans_projects() {
    let mut harness = historical_summary_harness(120, 40, Theme::Dark);
    let bucket = &mut harness.app.history.half_hour_buckets[0];
    for (index, group) in bucket.project_groups.iter_mut().enumerate() {
        // Project grouping belongs to emitting threads. Two branches can share
        // one root session while retaining their distinct observed projects.
        group.thread_id = format!("historical-emitter-{index}");
        group.parent_thread_id = Some("historical-root".to_string());
        group.source = Some("subagent".to_string());
        group.project_id = Some(if index == 0 { "a-small" } else { "z-large" }.to_string());
        group.project_label = group.project_id.clone();
        group.api_equivalent_cost = exact_api_cost(if index == 0 {
            100_000_000_000_000
        } else {
            1_000_000_000_000
        });
    }
    harness.app.summary_metric = SummaryMetric::ApiEquivalent;
    harness.app.summary_cache = None;
    harness.render();
    let projects = &harness
        .app
        .summary_cache
        .as_ref()
        .unwrap()
        .prepared
        .usage
        .projects;
    assert_eq!(
        projects.len(),
        2,
        "two emitting projects share the same root session"
    );
    assert!(projects.iter().all(|project| {
        project
            .sessions
            .iter()
            .any(|session| session.thread_id == "historical-root")
    }));
    assert_eq!(
        projects[0].label, "z-large",
        "persisted summary is ordered by tokens"
    );
    let rows = harness.app.summary_rows();
    let project = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Project)
        .unwrap();
    assert_eq!(
        project.label, "a-small",
        "the displayed tree is ordered by API amount"
    );
    harness
        .app
        .summary_expanded_nodes
        .insert(project.id.clone());
    harness.render();
    let rows = harness.app.summary_rows();
    let session = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Session)
        .unwrap();
    assert_eq!(session.metrics.token_usage.total_tokens, 101);
    harness.app.summary_selected_id = Some(session.id.clone());
    harness.render();
    open(&mut harness);
    let content = popup_text(&harness.app);
    assert!(content.contains("Project: a-small"));
    assert!(content.contains("Total range tokens: 101"));
    assert!(!content.contains("Project: z-large"));
    assert!(!content.contains("Total range tokens: 505"));
}

#[test]
fn entity_detail_local_enrichment_updates_the_modal_and_keeps_remote_content_separate() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut snapshot = interaction_test_app(1, 1).snapshot;
    snapshot.codex_home = directory.path().to_path_buf();
    let captured = snapshot.as_of;
    let thread_id = snapshot.tasks[0].thread_id.clone();
    let turn_id = snapshot.turns[0].turn_id.clone();
    let record = |timestamp: DateTime<Utc>, kind: &str, payload: serde_json::Value| {
        serde_json::json!({"timestamp": timestamp, "type": kind, "payload": payload}).to_string()
    };
    let full_message = format!(
        "{} COMPLETE-LOCAL-MESSAGE",
        "long recorded message ".repeat(20)
    );
    let log = [
        record(captured - ChronoDuration::seconds(2), "session_meta", serde_json::json!({"id": thread_id})),
        record(captured - ChronoDuration::seconds(1), "event_msg", serde_json::json!({"type": "user_message", "turn_id": turn_id, "message": full_message})),
        record(captured + ChronoDuration::seconds(1), "event_msg", serde_json::json!({"type": "user_message", "turn_id": turn_id, "message": "FUTURE-MESSAGE-MUST-NOT-APPEAR"})),
    ].join("\n");
    std::fs::write(sessions.join(format!("rollout-{thread_id}.jsonl")), log).unwrap();
    let mut harness = TuiHarness::from_snapshot(snapshot.clone(), 80, 24, Theme::Dark);
    harness.app.focus_turns();
    harness.render();
    open(&mut harness);
    assert!(popup_text(&harness.app).contains("Recorded details: loading"));
    let now = Instant::now();
    harness.app.last_local_refresh = now;
    assert_eq!(
        next_run_loop_poll_timeout(&harness.app, now, false),
        BACKGROUND_CHANNEL_POLL
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !harness.app.poll_entity_detail() {
        assert!(
            Instant::now() < deadline,
            "local enrichment did not complete"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    harness.render();
    let content = popup_text(&harness.app);
    assert!(content.contains("Recorded details (at capture)"));
    assert!(content.contains(&full_message));
    assert!(!content.contains("FUTURE-MESSAGE-MUST-NOT-APPEAR"));
    assert!(harness.app.entity_detail.is_some());
    harness.key(KeyCode::Esc);
    let now = Instant::now();
    harness.app.last_local_refresh = now;
    assert!(next_run_loop_poll_timeout(&harness.app, now, false) > BACKGROUND_CHANNEL_POLL);

    snapshot.tasks[0].source = Some("remote:other-machine".to_string());
    let mut remote = TuiHarness::from_snapshot(snapshot, 80, 24, Theme::Dark);
    remote.app.focus_turns();
    remote.render();
    open(&mut remote);
    assert!(!remote.app.poll_entity_detail());
    assert!(popup_text(&remote.app).contains("no unambiguous local rollout association"));
    assert!(!popup_text(&remote.app).contains("COMPLETE-LOCAL-MESSAGE"));
}

#[test]
fn entity_detail_reads_local_summary_sessions_that_have_aged_out_of_overview() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut harness = historical_summary_harness(80, 24, Theme::Dark);
    harness.app.snapshot.codex_home = directory.path().to_path_buf();
    harness.app.local_snapshot.tasks.clear();
    let local_id: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
    harness.app.history_local_source_id = Some(local_id.clone());
    let range = harness
        .app
        .summary_cache
        .as_ref()
        .unwrap()
        .prepared
        .usage
        .window;
    let captured = harness.app.snapshot.as_of;
    let message = |time: DateTime<Utc>, id: &str, body: &str| {
        serde_json::json!({
            "timestamp": time,
            "type": "event_msg",
            "payload": {
                "type": "item_completed", "thread_id": "historical-root", "turn_id": "historical-turn",
                "item": {"type": "UserMessage", "id": id, "content": [{"text": body}]}
            }
        }).to_string()
    };
    let body = "HISTORICAL-CONTENT-OUTSIDE-OVERVIEW-WITHIN-SUMMARY-RANGE";
    let log = [
        serde_json::json!({"timestamp": range.starts_at - ChronoDuration::seconds(2), "type": "session_meta", "payload": {"id": "historical-root"}}).to_string(),
        message(range.starts_at - ChronoDuration::seconds(1), "before", "BEFORE-SUMMARY-RANGE-MUST-NOT-APPEAR"),
        message(captured - ChronoDuration::minutes(30), "within", body),
        message(captured.max(range.ends_at) + ChronoDuration::hours(2), "future", "AFTER-CAPTURE-MUST-NOT-APPEAR"),
        message(range.ends_at, "exact-end", "EXACT-RANGE-END-MUST-NOT-APPEAR"),
    ].join("\n");
    std::fs::write(sessions.join("rollout-historical-root.jsonl"), log).unwrap();

    let rows = harness.app.summary_rows();
    let project_id = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Project)
        .unwrap()
        .id
        .clone();
    harness.app.summary_expanded_nodes.insert(project_id);
    harness.render();
    let rows = harness.app.summary_rows();
    let session_id = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Session)
        .unwrap()
        .id
        .clone();
    harness
        .app
        .summary_expanded_nodes
        .insert(session_id.clone());
    harness.render();
    let rows = harness.app.summary_rows();
    let turn_id = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Turn && row.label == "Historical prompt")
        .unwrap()
        .id
        .clone();

    for scope in [
        HistorySourceSelection::Local(local_id),
        HistorySourceSelection::AllIncluded,
    ] {
        harness.app.history_source_applied_selection = scope.clone();
        harness.app.history_source_selection = scope;
        for selected in [&session_id, &turn_id] {
            harness.app.summary_selected_id = Some(selected.clone());
            harness.render();
            // A capture can happen after a historical report has ended; its end stays exclusive.
            harness.app.summary_cache.as_mut().unwrap().snapshot_as_of =
                range.ends_at + ChronoDuration::hours(1);
            open(&mut harness);
            assert!(
                harness.app.entity_detail_loading(),
                "local history supplies an exact owner even after Overview expires"
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while !harness.app.poll_entity_detail() {
                assert!(
                    Instant::now() < deadline,
                    "historical detail loading did not complete"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            harness.render();
            let content = popup_text(&harness.app);
            assert!(content.contains(body));
            assert!(!content.contains("BEFORE-SUMMARY-RANGE-MUST-NOT-APPEAR"));
            assert!(!content.contains("AFTER-CAPTURE-MUST-NOT-APPEAR"));
            assert!(!content.contains("EXACT-RANGE-END-MUST-NOT-APPEAR"));
            harness.key(KeyCode::Esc);
        }
    }

    harness.app.history_remote_sources = vec![(
        "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
        "other-machine".to_string(),
    )];
    harness.app.history_source_applied_selection = HistorySourceSelection::AllIncluded;
    harness.app.summary_selected_id = Some(turn_id);
    harness.render();
    open(&mut harness);
    assert!(!harness.app.entity_detail_loading());
    assert!(!harness.app.poll_entity_detail());
    assert!(popup_text(&harness.app).contains("no unambiguous local rollout association"));
    assert!(!popup_text(&harness.app).contains(body));
}

#[test]
fn entity_detail_restores_only_known_local_scopes_for_historical_summary_ids() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let local_id: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
    let other_id: NodeId = "node-22222222222222222222222222222222".parse().unwrap();
    let body = "SCOPED-HISTORY-READS-RAW-LOCAL-OWNER-AND-TURN";

    for (case, session_allowed, turn_allowed) in [
        ("local", true, true),
        ("other-session", false, false),
        ("other-turn", true, false),
        ("logical", false, false),
    ] {
        for (scope_name, scope, scope_allowed) in [
            (
                "Local",
                HistorySourceSelection::Local(local_id.clone()),
                true,
            ),
            ("AllIncluded", HistorySourceSelection::AllIncluded, true),
            (
                "different Local node",
                HistorySourceSelection::Local(other_id.clone()),
                false,
            ),
        ] {
            let mut harness = historical_summary_harness(80, 24, Theme::Dark);
            harness.app.snapshot.codex_home = directory.path().to_path_buf();
            harness.app.local_snapshot.tasks.clear();
            assert!(harness.app.snapshot.tasks.is_empty());
            assert!(harness.app.local_snapshot.tasks.is_empty());
            harness.app.history_local_source_id = Some(local_id.clone());
            harness.app.history_remote_sources.clear();
            harness.app.history_source_applied_selection = scope.clone();
            harness.app.history_source_selection = scope;

            // Match scope_project_groups: thread, parent and session IDs share
            // their source suffix; both exact turn IDs are scoped separately.
            let thread_source = if case == "other-session" {
                &other_id
            } else {
                &local_id
            };
            let turn_source = if case == "other-turn" {
                &other_id
            } else {
                &local_id
            };
            let scoped_thread = |raw: &str| {
                if case == "logical" && raw == "historical-root" {
                    format!("logical-thread:{raw}")
                } else {
                    format!("{raw}@{}", thread_source.as_str())
                }
            };
            let scoped_turn = |raw: &str| format!("{raw}@{}", turn_source.as_str());
            let expected_thread = scoped_thread("historical-root");
            let expected_turn = scoped_turn("historical-turn");
            let mut bucket = harness.app.history.half_hour_buckets[0].clone();
            for group in &mut bucket.project_groups {
                group.thread_id = scoped_thread(&group.thread_id);
                group.parent_thread_id = group.parent_thread_id.as_deref().map(scoped_thread);
                group.session_thread_id = group.session_thread_id.as_deref().map(scoped_thread);
                group.turn_id = group.turn_id.as_deref().map(scoped_turn);
                group.session_turn_id = group.session_turn_id.as_deref().map(scoped_turn);
            }
            harness.app.history.half_hour_buckets = vec![bucket].into();
            harness.app.summary_cache = None;
            harness.render();

            // The rollout retains raw IDs; using the Summary IDs directly
            // would miss both the owner and its exact user turn.
            let captured = harness.app.snapshot.as_of;
            let log = [
                serde_json::json!({
                    "timestamp": captured - ChronoDuration::hours(1),
                    "type": "session_meta", "payload": {"id": "historical-root"}
                }).to_string(),
                serde_json::json!({
                    "timestamp": captured - ChronoDuration::minutes(30),
                    "type": "event_msg",
                    "payload": {
                        "type": "item_completed", "thread_id": "historical-root", "turn_id": "historical-turn",
                        "item": {"type": "UserMessage", "id": "scoped-history-message", "content": [{"text": body}]}
                    }
                }).to_string(),
            ].join("\n");
            std::fs::write(sessions.join("rollout-historical-root.jsonl"), log).unwrap();

            let project = harness
                .app
                .summary_rows()
                .into_iter()
                .find(|row| row.kind == SummaryRowKind::Project)
                .unwrap();
            harness.app.summary_expanded_nodes.insert(project.id);
            harness.render();
            let session = harness
                .app
                .summary_rows()
                .into_iter()
                .find(|row| row.kind == SummaryRowKind::Session)
                .unwrap();
            harness
                .app
                .summary_expanded_nodes
                .insert(session.id.clone());
            harness.render();
            let turn = harness
                .app
                .summary_rows()
                .into_iter()
                .find(|row| row.kind == SummaryRowKind::Turn && row.label == "Historical prompt")
                .unwrap();

            for (selected, allowed, is_turn) in [
                (&session.id, session_allowed, false),
                (&turn.id, turn_allowed, true),
            ] {
                harness.app.summary_selected_id = Some(selected.clone());
                harness.render();
                open(&mut harness);
                let allowed = allowed && scope_allowed;
                assert_eq!(
                    harness.app.entity_detail_loading(),
                    allowed,
                    "{case}, {scope_name}, turn={is_turn}"
                );
                if allowed {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !harness.app.poll_entity_detail() {
                        assert!(
                            Instant::now() < deadline,
                            "scoped history loading did not complete: {case}, {scope_name}, turn={is_turn}"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    harness.render();
                    assert!(
                        popup_text(&harness.app).contains(body),
                        "{case}, {scope_name}, turn={is_turn}"
                    );
                } else {
                    assert!(!harness.app.poll_entity_detail());
                    assert!(
                        popup_text(&harness.app)
                            .contains("no unambiguous local rollout association")
                    );
                    assert!(!popup_text(&harness.app).contains(body));
                }
                let content = popup_text(&harness.app);
                assert!(content.contains(&format!("Thread ID: {expected_thread}")));
                if is_turn {
                    assert!(content.contains(&format!("Turn ID: {expected_turn}")));
                }
                harness.key(KeyCode::Esc);
            }
        }
    }
}

#[test]
fn entity_detail_refresh_keeps_the_open_identity_and_frozen_values() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    harness.app.selected_task = 1;
    harness.app.selected_turn = 1;
    harness.app.focus_turns();
    harness.render();
    open(&mut harness);
    let original = popup_text(&harness.app);
    let title = harness.app.entity_detail.as_ref().unwrap().title.clone();
    let mut snapshot = harness.app.snapshot.clone();
    snapshot.tasks.reverse();
    snapshot.turns.reverse();
    for task in &mut snapshot.tasks {
        task.title = "new snapshot title".to_string();
    }
    for turn in &mut snapshot.turns {
        turn.message_preview = Some("new snapshot message".to_string());
    }
    snapshot.as_of += ChronoDuration::seconds(30);
    harness.app.replace(
        CollectionResult {
            snapshot,
            account: harness.app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    harness.render();
    assert_eq!(harness.app.entity_detail.as_ref().unwrap().title, title);
    assert_eq!(popup_text(&harness.app), original);
    assert!(original.contains("turn-1-1"));
    assert!(!original.contains("new snapshot message"));
    let selected = harness.app.selected_turn_record().unwrap().turn_id.clone();
    harness.key(KeyCode::Esc);
    assert_eq!(
        harness.app.selected_turn_record().unwrap().turn_id,
        selected
    );
}
