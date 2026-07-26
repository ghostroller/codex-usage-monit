use std::fs;

use crossterm::event::KeyCode;

use super::super::*;
use super::testkit::{ClickEdge, ControlId, TuiHarness, gallery_directory};

#[test]
fn snapshot_fixtures_render_with_fixed_utc_labels() {
    let normal = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    let normal_frame = normal.frame().snapshot_text();
    assert!(normal_frame.contains("codex-usage-monit | desktop"));
    assert!(normal_frame.contains("07-12"));
    assert!(normal_frame.contains("+00:00"));

    let empty = TuiHarness::from_fixture("empty", 80, 24, Theme::Light);
    assert!(empty.frame().snapshot_text().contains("no tasks"));

    let partial = TuiHarness::from_fixture("partial", 80, 24, Theme::Dark);
    assert!(partial.state().ui_state.view == UiView::Overview);
}

#[test]
fn semantic_frames_cover_full_compact_and_diagnostic_layouts() {
    let overview = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    insta::assert_snapshot!("tui_overview_dark_120x40", overview.frame().snapshot_text());

    let mut compact = TuiHarness::from_fixture("normal", 60, 24, Theme::Light);
    compact.key(KeyCode::Char('R'));
    compact.key(KeyCode::Char('E'));
    insta::assert_snapshot!(
        "tui_compact_tree_light_60x24",
        compact.frame().snapshot_text()
    );

    let mut diagnostics = TuiHarness::from_fixture("partial", 80, 24, Theme::Dark);
    diagnostics.key(KeyCode::Char('2'));
    insta::assert_snapshot!(
        "tui_partial_other_dark_80x24",
        diagnostics.frame().snapshot_text()
    );
}

#[test]
fn keyboard_and_mouse_controls_have_equivalent_state_transitions() {
    for (width, height) in [(120, 40), (60, 24)] {
        for (key, control) in [
            (KeyCode::Char('v'), ControlId::ToggleTurns),
            (KeyCode::Char('m'), ControlId::ToggleModels),
            (KeyCode::Char('W'), ControlId::ScopeWeek),
            (KeyCode::Char('2'), ControlId::ViewOther),
            (KeyCode::Char('R'), ControlId::ToggleTree),
            (KeyCode::Char('D'), ControlId::SourceDesktop),
        ] {
            let mut keyboard = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            let mut mouse = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            assert!(
                !mouse.control_rect(control).is_empty(),
                "{control:?} missing at {width}x{height}"
            );

            keyboard.key(key);
            assert!(mouse.click(control, ClickEdge::Middle));

            assert_eq!(
                keyboard.state(),
                mouse.state(),
                "{control:?} differs at {width}x{height}"
            );
            assert_eq!(
                keyboard.frame(),
                mouse.frame(),
                "{control:?} frame differs at {width}x{height}"
            );
        }
    }
}

#[test]
fn shortcut_graphemes_are_visually_distinct_when_the_binding_is_active() {
    for (width, height) in [(120, 40), (60, 24)] {
        let harness = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
        for control in [
            ControlId::ViewOverview,
            ControlId::ViewOther,
            ControlId::ToggleTurns,
            ControlId::ToggleModels,
            ControlId::ScopeFiveHours,
            ControlId::ScopeWeek,
            ControlId::SourceAll,
            ControlId::SourceDesktop,
            ControlId::SourceSubagent,
            ControlId::SourceCli,
            ControlId::TaskSearch,
            ControlId::EnterTurns,
            ControlId::OpenTerminal,
            ControlId::ToggleTree,
        ] {
            harness.assert_shortcut_distinct(control);
        }

        let mut tree = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
        tree.key(KeyCode::Char('R'));
        tree.assert_shortcut_distinct(ControlId::ToggleTree);
        tree.assert_shortcut_distinct(ControlId::CollapseAll);
    }
}

#[test]
fn whole_labels_are_clickable_at_both_layout_breakpoints() {
    for (width, height) in [(120, 40), (60, 24)] {
        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            let mut tab = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            assert!(tab.click(ControlId::ViewOther, edge));
            assert_eq!(tab.state().ui_state.view, UiView::Health);

            let mut toggle = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            assert!(toggle.state().ui_state.models_visible);
            assert!(toggle.click(ControlId::ToggleModels, edge));
            assert!(!toggle.state().ui_state.models_visible);

            let mut source = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            assert!(source.click(ControlId::SourceDesktop, edge));
            assert_eq!(
                source.state().ui_state.task_source_filter,
                UiTaskSourceFilter::Desktop
            );
        }
    }
}

#[test]
fn compact_search_consumes_printable_global_shortcuts() {
    let mut harness = TuiHarness::from_fixture("normal", 60, 24, Theme::Dark);
    harness.key(KeyCode::Char('/'));
    harness.key(KeyCode::Char('2'));
    harness.key(KeyCode::Char('V'));
    harness.key(KeyCode::Char('W'));

    let state = harness.state();
    assert_eq!(state.focus, Focus::TaskSearch);
    assert_eq!(state.task_search, "2VW");
    assert_eq!(state.ui_state.view, UiView::Overview);
    assert!(state.ui_state.turns_visible);
    assert_eq!(state.ui_state.window_scope, UiWindowScope::FiveHours);

    harness.key(KeyCode::Esc);
    assert_eq!(harness.state().focus, Focus::Tasks);
    assert!(harness.state().task_search.is_empty());
}

#[test]
fn resize_reflows_without_losing_selection_or_control_bindings() {
    let mut harness = TuiHarness::from_fixture("normal", 120, 40, Theme::Light);
    harness.key(KeyCode::Down);
    let selected = harness.state().selected_thread_id;

    harness.resize(60, 24);

    assert_eq!(harness.state().selected_thread_id, selected);
    assert_eq!(
        harness.frame().snapshot_text().lines().next(),
        Some("size=60x24")
    );
    harness.assert_shortcut_distinct(ControlId::ViewOther);
    harness.assert_shortcut_distinct(ControlId::TaskSearch);
}

#[test]
fn svg_gallery_is_generated_from_the_same_semantic_frames() {
    let directory = gallery_directory();
    fs::create_dir_all(&directory).unwrap();

    let mut scenarios = Vec::new();
    scenarios.push((
        "overview-dark-120x40",
        TuiHarness::from_fixture("normal", 120, 40, Theme::Dark),
    ));
    scenarios.push((
        "overview-light-80x24",
        TuiHarness::from_fixture("normal", 80, 24, Theme::Light),
    ));
    scenarios.push((
        "overview-compact-60x24",
        TuiHarness::from_fixture("normal", 60, 24, Theme::Dark),
    ));

    let mut tree = TuiHarness::from_fixture("normal", 80, 24, Theme::Dark);
    tree.key(KeyCode::Char('R'));
    tree.key(KeyCode::Char('E'));
    scenarios.push(("tree-collapsed-80x24", tree));

    let mut search = TuiHarness::from_fixture("normal", 80, 24, Theme::Light);
    search.key(KeyCode::Char('/'));
    search.key(KeyCode::Char('布'));
    search.key(KeyCode::Char('局'));
    scenarios.push(("task-search-unicode-80x24", search));

    let mut other = TuiHarness::from_fixture("normal", 80, 24, Theme::Dark);
    other.key(KeyCode::Char('2'));
    scenarios.push(("other-normal-80x24", other));

    scenarios.push((
        "overview-empty-80x24",
        TuiHarness::from_fixture("empty", 80, 24, Theme::Dark),
    ));

    let mut partial = TuiHarness::from_fixture("partial", 80, 24, Theme::Light);
    partial.key(KeyCode::Char('2'));
    scenarios.push(("other-partial-80x24", partial));

    let mut quit = TuiHarness::from_fixture("normal", 60, 24, Theme::Dark);
    quit.key(KeyCode::Esc);
    scenarios.push(("quit-confirmation-60x24", quit));

    for (name, harness) in scenarios {
        let svg = harness.frame().to_svg(name);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("<text"));
        fs::write(directory.join(format!("{name}.svg")), svg).unwrap();
    }
}
