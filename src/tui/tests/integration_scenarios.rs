use std::collections::HashSet;
use std::fs;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Modifier;

use crate::domain::{ApiCostAmount, PicoUsd, TokenUsage};
use crate::history::{HISTORY_ESTIMATOR_REVISION, LocalHalfHourBucket, LocalProjectUsageGroup};

use super::super::*;
use super::testkit::{ClickEdge, ControlId, TuiHarness, gallery_directory};

fn summary_timestamp(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

fn summary_group(
    thread_id: &str,
    parent_thread_id: Option<&str>,
    project_id: &str,
    project_label: &str,
    title: &str,
    source: &str,
    total_tokens: u64,
) -> LocalProjectUsageGroup {
    let input_tokens = total_tokens.saturating_mul(4) / 5;
    let output_tokens = total_tokens.saturating_sub(input_tokens);
    let pico_usd = u128::from(total_tokens).saturating_mul(1_000_000);
    let session_thread_id = parent_thread_id.unwrap_or(thread_id);
    let session_turn_id = format!("turn-{session_thread_id}");
    LocalProjectUsageGroup {
        thread_id: thread_id.to_string(),
        turn_id: Some(format!("turn-{thread_id}")),
        parent_thread_id: parent_thread_id.map(str::to_string),
        session_thread_id: Some(session_thread_id.to_string()),
        session_turn_id: Some(session_turn_id),
        message_preview: Some(format!("Review {project_label} usage")),
        turn_started_at: None,
        project_id: Some(project_id.to_string()),
        project_label: Some(project_label.to_string()),
        title: Some(title.to_string()),
        source: Some(source.to_string()),
        token_usage: TokenUsage {
            input_tokens,
            output_tokens,
            total_tokens,
            ..TokenUsage::default()
        },
        estimated_cost_units: u128::from(total_tokens).saturating_mul(3),
        api_long_context_extra_cost_units: Some(u128::from(total_tokens)),
        api_equivalent_cost: ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(pico_usd),
            maximum_pico_usd: PicoUsd::new(pico_usd),
            observed_samples: 1,
            priced_samples: 1,
            observed_tokens: total_tokens,
            priced_tokens: total_tokens,
        },
        call_count: 1,
    }
}

fn attributed_summary_group(
    mut group: LocalProjectUsageGroup,
    turn_id: &str,
    session_thread_id: &str,
    session_turn_id: &str,
    message_preview: &str,
    turn_started_at: DateTime<Utc>,
) -> LocalProjectUsageGroup {
    group.turn_id = Some(turn_id.to_string());
    group.session_thread_id = Some(session_thread_id.to_string());
    group.session_turn_id = Some(session_turn_id.to_string());
    group.message_preview = Some(message_preview.to_string());
    group.turn_started_at = Some(turn_started_at);
    group
}

fn summary_bucket(
    starts_at: DateTime<Utc>,
    groups: Vec<LocalProjectUsageGroup>,
) -> LocalHalfHourBucket {
    let mut token_usage = TokenUsage::default();
    let mut estimated_cost_units = 0_u128;
    let mut api_long_context_extra_cost_units = 0_u128;
    let mut api_equivalent_cost = ApiCostAmount::default();
    let mut call_count = 0_u64;
    for group in &groups {
        token_usage.add_assign(group.token_usage);
        estimated_cost_units = estimated_cost_units.saturating_add(group.estimated_cost_units);
        api_long_context_extra_cost_units = api_long_context_extra_cost_units
            .saturating_add(group.api_long_context_extra_cost_units.unwrap_or_default());
        api_equivalent_cost.add_assign(group.api_equivalent_cost);
        call_count = call_count.saturating_add(group.call_count);
    }
    LocalHalfHourBucket {
        starts_at,
        ends_at: starts_at + ChronoDuration::minutes(15),
        sampled_at: starts_at + ChronoDuration::minutes(15),
        token_usage,
        estimated_cost_units,
        api_long_context_extra_cost_units: Some(api_long_context_extra_cost_units),
        long_context_usage_unknown: false,
        estimator_revision: HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count,
        groups: Vec::new(),
        project_groups: groups,
        partial_reasons: Vec::new(),
    }
}

fn summary_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = TuiHarness::from_fixture("normal", width, height, theme);
    harness.app.history.half_hour_buckets = vec![
        summary_bucket(
            summary_timestamp("2026-07-09T06:00:00Z"),
            vec![attributed_summary_group(
                summary_group(
                    "alpha-root",
                    None,
                    "alpha-id",
                    "alpha-service",
                    "API billing dashboard",
                    "desktop",
                    120_000,
                ),
                "alpha-user-turn",
                "alpha-root",
                "alpha-user-turn",
                "Explain the API billing discrepancy",
                summary_timestamp("2026-07-09T05:55:00Z"),
            )],
        ),
        summary_bucket(
            summary_timestamp("2026-07-10T08:15:00Z"),
            vec![attributed_summary_group(
                summary_group(
                    "alpha-child",
                    Some("alpha-root"),
                    "alpha-id",
                    "alpha-service",
                    "Price catalog research",
                    "subagent",
                    70_000,
                ),
                "alpha-child-turn",
                "alpha-root",
                "alpha-user-turn",
                "Explain the API billing discrepancy",
                summary_timestamp("2026-07-09T05:55:00Z"),
            )],
        ),
        summary_bucket(
            summary_timestamp("2026-07-11T10:30:00Z"),
            vec![summary_group(
                "billing-root",
                None,
                "billing-id",
                "billing-cli",
                "Invoice reconciliation",
                "cli",
                90_000,
            )],
        ),
        summary_bucket(
            summary_timestamp("2026-07-12T03:45:00Z"),
            vec![summary_group(
                "docs-root",
                None,
                "docs-id",
                "docs-site",
                "Usage guide refresh",
                "desktop",
                40_000,
            )],
        ),
    ];
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('U'));
    harness
}

fn summary_many_projects_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = TuiHarness::from_fixture("normal", width, height, theme);
    let groups = (0_u64..8)
        .map(|index| {
            let thread = format!("project-{index}-thread");
            let project = format!("project-{index}-id");
            let label = format!("project-{index}");
            let title = format!("Project {index} task");
            summary_group(
                &thread,
                None,
                &project,
                &label,
                &title,
                "cli",
                (index + 1) * 10_000,
            )
        })
        .collect();
    harness.app.history.half_hour_buckets = vec![summary_bucket(
        summary_timestamp("2026-07-12T03:45:00Z"),
        groups,
    )];
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('U'));
    harness.key(KeyCode::Char('U'));
    harness
}

fn summary_mouse(harness: &mut TuiHarness, kind: MouseEventKind, column: u16, row: u16) -> bool {
    let handled = handle_mouse_event(
        &mut harness.app,
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    );
    harness.render();
    handled
}

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
    insta::assert_snapshot!(
        "tui_compact_tree_light_60x24",
        compact.frame().snapshot_text()
    );

    let mut diagnostics = TuiHarness::from_fixture("partial", 80, 24, Theme::Dark);
    diagnostics.key(KeyCode::Char('3'));
    insta::assert_snapshot!(
        "tui_partial_other_dark_80x24",
        diagnostics.frame().snapshot_text()
    );

    let mut settings = TuiHarness::from_fixture("normal", 80, 24, Theme::Dark);
    settings.key(KeyCode::Char('4'));
    insta::assert_snapshot!("tui_settings_dark_80x24", settings.frame().snapshot_text());

    for (name, width, height, theme) in [
        ("tui_summary_dark_120x40", 120, 40, Theme::Dark),
        ("tui_summary_light_120x40", 120, 40, Theme::Light),
        ("tui_summary_dark_60x24", 60, 24, Theme::Dark),
        ("tui_summary_light_60x24", 60, 24, Theme::Light),
    ] {
        insta::assert_snapshot!(
            name,
            summary_harness(width, height, theme)
                .frame()
                .snapshot_text()
        );
    }
    for (name, width, height, theme) in [
        ("tui_summary_turn_tree_dark_120x40", 120, 40, Theme::Dark),
        ("tui_summary_turn_tree_light_60x24", 60, 24, Theme::Light),
    ] {
        let mut summary = summary_harness(width, height, theme);
        summary.key(KeyCode::Enter);
        summary.key(KeyCode::Down);
        summary.key(KeyCode::Enter);
        insta::assert_snapshot!(name, summary.frame().snapshot_text());
    }

    for (name, width, height, theme) in [
        ("tui_summary_6h_dark_120x40", 120, 40, Theme::Dark),
        ("tui_summary_6h_light_60x24", 60, 24, Theme::Light),
    ] {
        let mut summary = summary_harness(width, height, theme);
        summary.key(KeyCode::Char('B'));
        summary.key(KeyCode::Char('B'));
        insta::assert_snapshot!(name, summary.frame().snapshot_text());
    }

    let many_projects = summary_many_projects_harness(120, 40, Theme::Dark);
    insta::assert_snapshot!(
        "tui_summary_top_six_other_dark_120x40",
        many_projects.frame().snapshot_text()
    );
    let mut all_projects = summary_many_projects_harness(120, 40, Theme::Dark);
    all_projects.key(KeyCode::Char('G'));
    insta::assert_snapshot!(
        "tui_summary_all_projects_dark_120x40",
        all_projects.frame().snapshot_text()
    );
}

#[test]
fn summary_30d_labels_known_history_and_removes_the_redundant_share_chart() {
    let mut harness = summary_harness(160, 40, Theme::Dark);
    harness.key(KeyCode::Char('M'));
    let frame = harness.frame().snapshot_text();

    assert!(frame.contains("known 320.0K"), "{frame}");
    assert!(frame.contains("0C/4P/27M"), "{frame}");
    assert!(frame.contains("LOWER BOUND"), "{frame}");
    assert!(frame.contains("C complete"), "{frame}");
    assert!(frame.contains("C/P/M M"), "{frame}");
    assert!(!frame.contains("Usage share"), "{frame}");
}

#[test]
fn summary_daily_status_strip_distinguishes_known_zero_partial_and_missing_dates() {
    for (width, theme) in [(120, Theme::Dark), (60, Theme::Light)] {
        let mut harness = TuiHarness::from_fixture("normal", width, 40, theme);
        let complete_day = summary_timestamp("2026-07-09T00:00:00Z");
        let mut buckets = (0_i64..96)
            .map(|index| {
                summary_bucket(
                    complete_day + ChronoDuration::minutes(index * 15),
                    Vec::new(),
                )
            })
            .collect::<Vec<_>>();
        buckets.push(summary_bucket(
            summary_timestamp("2026-07-10T12:00:00Z"),
            vec![summary_group(
                "partial-thread",
                None,
                "partial-project",
                "partial-project",
                "Partial day",
                "cli",
                80_000,
            )],
        ));
        harness.app.history.half_hour_buckets = buckets;
        harness.app.summary_cache = None;
        harness.key(KeyCode::Char('U'));
        harness.key(KeyCode::Char('7'));

        let frame = harness.frame().snapshot_text();
        assert!(frame.contains("1C/1P/6M"), "width={width}: {frame}");
        assert!(frame.contains("C/P/M MMMMCPMM"), "width={width}: {frame}");
    }
}

#[test]
fn summary_stacked_area_click_and_drag_inspect_exact_dates_without_hiding_gaps() {
    for (width, height, theme) in [
        (120, 40, Theme::Dark),
        (120, 40, Theme::Light),
        (60, 24, Theme::Dark),
        (60, 24, Theme::Light),
    ] {
        let mut harness = summary_harness(width, height, theme);
        harness.key(KeyCode::Char('7'));
        let hitbox = harness.app.summary_daily_hitbox.clone().unwrap();
        let first_date = hitbox.dates[0];
        let last_date = *hitbox.dates.last().unwrap();

        assert!(summary_mouse(
            &mut harness,
            MouseEventKind::Down(MouseButton::Left),
            hitbox.plot.x,
            hitbox.plot.y,
        ));
        assert_eq!(harness.app.summary_inspected_date, Some(first_date));
        assert!(harness.app.summary_daily_dragging);
        assert!(
            !summary_mouse(
                &mut harness,
                MouseEventKind::Drag(MouseButton::Left),
                hitbox.plot.x,
                hitbox.plot.y,
            ),
            "dragging within the same date must not request another full redraw"
        );
        assert_eq!(harness.app.summary_inspected_date, Some(first_date));
        let missing = harness.frame().snapshot_text();
        assert!(missing.contains("MISSING"), "{missing}");
        assert!(missing.contains("no local project evidence"), "{missing}");

        assert!(summary_mouse(
            &mut harness,
            MouseEventKind::Drag(MouseButton::Left),
            hitbox.plot.right().saturating_sub(1),
            hitbox.plot.y,
        ));
        assert_eq!(harness.app.summary_inspected_date, Some(last_date));
        let partial = harness.frame().snapshot_text();
        assert!(partial.contains("P lower bound"), "{partial}");

        assert!(summary_mouse(
            &mut harness,
            MouseEventKind::Up(MouseButton::Left),
            hitbox.plot.right().saturating_sub(1),
            hitbox.plot.y,
        ));
        assert!(!harness.app.summary_daily_dragging);
        assert_eq!(harness.app.summary_inspected_date, Some(last_date));
    }
}

#[test]
fn summary_stacked_area_column_mapping_never_reports_a_different_date_bucket() {
    let dates = (1..=4)
        .map(|day| {
            NaiveDate::from_ymd_opt(2026, 7, day)
                .expect("valid date")
                .and_hms_opt(0, 0, 0)
                .unwrap()
        })
        .collect::<Vec<_>>();
    for width in [4_u16, 9, 52] {
        let hitbox = SummaryDailyHitbox {
            plot: Rect::new(7, 3, width, 2),
            dates: dates.clone(),
        };
        for offset in 0..width {
            let expected =
                date_index_at_column(usize::from(offset), usize::from(width), dates.len())
                    .map(|index| dates[index]);
            assert_eq!(hitbox.date_at_column(7 + offset), expected);
        }
    }

    let narrow = SummaryDailyHitbox {
        plot: Rect::new(0, 0, 3, 1),
        dates,
    };
    assert!((0..3).all(|column| narrow.date_at_column(column).is_none()));
}

#[test]
fn summary_inspect_control_and_keyboard_navigation_cover_exact_dates() {
    for (width, height, theme) in [(120, 40, Theme::Dark), (60, 24, Theme::Light)] {
        let mut harness = summary_harness(width, height, theme);
        harness.assert_shortcut_distinct(ControlId::SummaryInspect);

        harness.key(KeyCode::Char('I'));
        assert_eq!(
            harness.app.summary_inspected_date,
            Some(summary_timestamp("2026-07-12T00:00:00Z").naive_utc())
        );
        assert_eq!(harness.app.view, View::Summary);
        harness.assert_shortcut_distinct(ControlId::SummaryInspect);

        harness.key(KeyCode::Left);
        assert_eq!(
            harness.app.summary_inspected_date,
            Some(summary_timestamp("2026-07-11T00:00:00Z").naive_utc())
        );
        harness.key(KeyCode::Char('['));
        assert_eq!(
            harness.app.summary_inspected_date,
            Some(summary_timestamp("2026-07-10T00:00:00Z").naive_utc())
        );
        harness.key(KeyCode::Home);
        assert_eq!(
            harness.app.summary_inspected_date,
            Some(summary_timestamp("2026-07-09T00:00:00Z").naive_utc())
        );
        harness.key(KeyCode::End);
        assert_eq!(
            harness.app.summary_inspected_date,
            Some(summary_timestamp("2026-07-12T00:00:00Z").naive_utc())
        );
        harness.key(KeyCode::Esc);
        assert_eq!(harness.app.summary_inspected_date, None);
        assert!(!harness.app.quit_confirmation_visible);
    }
}

#[test]
fn summary_stacked_area_inspection_preserves_the_date_across_metrics_and_clears_on_range_change() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let hitbox = harness.app.summary_daily_hitbox.clone().unwrap();
    let target_index = hitbox
        .dates
        .iter()
        .position(|date| *date == summary_timestamp("2026-07-09T00:00:00Z").naive_utc())
        .unwrap();
    let column = summary_date_column(hitbox.plot, target_index, hitbox.dates.len()).unwrap();
    assert!(summary_mouse(
        &mut harness,
        MouseEventKind::Down(MouseButton::Left),
        column,
        hitbox.plot.y,
    ));
    summary_mouse(
        &mut harness,
        MouseEventKind::Up(MouseButton::Left),
        column,
        hitbox.plot.y,
    );
    assert_eq!(
        harness.app.summary_inspected_date,
        Some(summary_timestamp("2026-07-09T00:00:00Z").naive_utc())
    );
    let tokens = harness.frame().snapshot_text();
    assert!(
        tokens.contains("07-09 · P lower bound · Total 120.0K"),
        "{tokens}"
    );

    harness.key(KeyCode::Char('A'));
    assert_eq!(
        harness.app.summary_inspected_date,
        Some(summary_timestamp("2026-07-09T00:00:00Z").naive_utc())
    );
    let api = harness.frame().snapshot_text();
    assert!(
        api.contains("07-09 · P lower bound · Total $0.1200"),
        "{api}"
    );

    harness.key(KeyCode::Char('M'));
    assert_eq!(harness.app.summary_inspected_date, None);
}

#[test]
fn summary_daily_hitbox_is_cleared_when_the_compact_layout_hides_the_chart() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let old = harness.app.summary_daily_hitbox.clone().unwrap();

    harness.resize(60, 12);
    assert!(harness.app.summary_daily_hitbox.is_none());
    harness.assert_shortcut_inactive(ControlId::SummaryInspect);
    harness.key(KeyCode::Char('I'));
    assert_eq!(harness.app.summary_inspected_date, None);
    assert!(!harness.click(ControlId::SummaryInspect, ClickEdge::Middle));
    assert!(!summary_mouse(
        &mut harness,
        MouseEventKind::Down(MouseButton::Left),
        old.plot.x,
        old.plot.y,
    ));
    assert_eq!(harness.app.summary_inspected_date, None);
}

#[test]
fn summary_inspection_is_inactive_when_the_compact_plot_aggregates_dates() {
    let mut harness = summary_harness(34, 24, Theme::Dark);
    harness.key(KeyCode::Char('M'));

    assert!(harness.app.summary_daily_hitbox.is_none());
    harness.assert_shortcut_inactive(ControlId::SummaryInspect);

    harness.app.summary_inspected_date =
        Some(summary_timestamp("2026-07-09T00:00:00Z").naive_utc());
    harness.render();
    assert_eq!(harness.app.summary_inspected_date, None);
    assert!(!harness.app.summary_daily_dragging);

    harness.key(KeyCode::Char('I'));
    assert_eq!(harness.app.summary_inspected_date, None);
    assert!(!harness.click(ControlId::SummaryInspect, ClickEdge::Middle));
}

#[test]
fn summary_inspection_is_inactive_without_a_nonzero_project_series() {
    let mut known_zero = TuiHarness::from_fixture("normal", 60, 24, Theme::Light);
    let mut group = summary_group(
        "zero-api-thread",
        None,
        "zero-api-project",
        "zero-api-project",
        "Known zero API equivalent",
        "cli",
        100_000,
    );
    group.api_equivalent_cost = ApiCostAmount {
        observed_samples: 1,
        priced_samples: 1,
        observed_tokens: 100_000,
        priced_tokens: 100_000,
        ..ApiCostAmount::default()
    };
    known_zero.app.history.half_hour_buckets = vec![summary_bucket(
        summary_timestamp("2026-07-12T03:45:00Z"),
        vec![group],
    )];
    known_zero.app.summary_cache = None;
    known_zero.key(KeyCode::Char('U'));
    known_zero.key(KeyCode::Char('A'));

    assert!(known_zero.app.summary_daily_hitbox.is_none());
    known_zero.assert_shortcut_inactive(ControlId::SummaryInspect);
    let frame = known_zero.frame().snapshot_text();
    assert!(frame.contains("no non-zero project usage"), "{frame}");
    known_zero.key(KeyCode::Char('I'));
    assert_eq!(known_zero.app.summary_inspected_date, None);
    assert!(!known_zero.click(ControlId::SummaryInspect, ClickEdge::Middle));

    let mut no_series = TuiHarness::from_fixture("normal", 60, 24, Theme::Dark);
    no_series.app.history.half_hour_buckets = vec![summary_bucket(
        summary_timestamp("2026-07-12T03:45:00Z"),
        Vec::new(),
    )];
    no_series.app.summary_cache = None;
    no_series.key(KeyCode::Char('U'));

    assert!(no_series.app.summary_daily_hitbox.is_none());
    no_series.assert_shortcut_inactive(ControlId::SummaryInspect);
    no_series.key(KeyCode::Char('I'));
    assert_eq!(no_series.app.summary_inspected_date, None);
    assert!(!no_series.click(ControlId::SummaryInspect, ClickEdge::Middle));
}

#[test]
fn summary_backfill_status_does_not_move_control_hitboxes() {
    for width in [120, 60] {
        let mut harness = summary_harness(width, 24, Theme::Dark);
        let controls = [
            ControlId::SummaryRangeCycle,
            ControlId::SummaryRangeSevenDays,
            ControlId::SummaryRangeThirtyDays,
            ControlId::SummaryMetricTokens,
            ControlId::SummaryMetricEstimated,
            ControlId::SummaryMetricApiEquivalent,
            ControlId::SummaryBucketGrain,
            ControlId::SummaryAllProjects,
            ControlId::SummaryLongContext,
            ControlId::SummaryInspect,
            ControlId::SummaryToggle,
            ControlId::SummaryCollapseAll,
        ];
        let before = controls.map(|control| harness.control_rect(control));
        harness.app.summary_backfill_running = true;
        harness.render();
        let after = controls.map(|control| harness.control_rect(control));

        assert_eq!(before, after);
        let frame = harness.frame().snapshot_text();
        assert!(frame.contains("BACKFILL"), "width={width}: {frame}");
    }
}

#[test]
fn keyboard_and_mouse_controls_have_equivalent_state_transitions() {
    for (width, height) in [(120, 40), (60, 24)] {
        for (key, control) in [
            (KeyCode::Char('v'), ControlId::ToggleTurns),
            (KeyCode::Char('m'), ControlId::ToggleModels),
            (KeyCode::Char('W'), ControlId::ScopeWeek),
            (KeyCode::Char('2'), ControlId::ViewTrends),
            (KeyCode::Char('U'), ControlId::ViewSummary),
            (KeyCode::Char('3'), ControlId::ViewOther),
            (KeyCode::Char('4'), ControlId::ViewSettings),
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
            ControlId::ViewTrends,
            ControlId::ViewSummary,
            ControlId::ViewOther,
            ControlId::ViewSettings,
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

            let mut settings = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            assert!(settings.click(ControlId::ViewSettings, edge));
            assert_eq!(settings.state().ui_state.view, UiView::Settings);

            let mut trends = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            assert!(trends.click(ControlId::ViewTrends, edge));
            assert_eq!(trends.state().ui_state.view, UiView::Trends);

            let mut summary = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            assert!(summary.click(ControlId::ViewSummary, edge));
            assert_eq!(summary.state().ui_state.view, UiView::Summary);

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
fn summary_controls_have_keyboard_mouse_parity_and_whole_label_hitboxes() {
    let controls = [
        (KeyCode::Char('C'), ControlId::SummaryRangeCycle),
        (KeyCode::Char('7'), ControlId::SummaryRangeSevenDays),
        (KeyCode::Char('M'), ControlId::SummaryRangeThirtyDays),
        (KeyCode::Char('K'), ControlId::SummaryMetricTokens),
        (KeyCode::Char('E'), ControlId::SummaryMetricEstimated),
        (KeyCode::Char('A'), ControlId::SummaryMetricApiEquivalent),
        (KeyCode::Char('B'), ControlId::SummaryBucketGrain),
        (KeyCode::Char('L'), ControlId::SummaryLongContext),
        (KeyCode::Char('I'), ControlId::SummaryInspect),
        (KeyCode::Enter, ControlId::SummaryToggle),
    ];

    for (width, height, theme) in [(120, 40, Theme::Dark), (60, 24, Theme::Light)] {
        let harness = summary_harness(width, height, theme);
        for control in controls.map(|(_, control)| control) {
            harness.assert_shortcut_distinct(control);
        }

        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            for (key, control) in controls {
                let mut keyboard = summary_harness(width, height, theme);
                let mut mouse = summary_harness(width, height, theme);
                match control {
                    ControlId::SummaryRangeCycle => {
                        keyboard.key(KeyCode::Char('7'));
                        mouse.key(KeyCode::Char('7'));
                    }
                    ControlId::SummaryMetricTokens => {
                        keyboard.key(KeyCode::Char('E'));
                        mouse.key(KeyCode::Char('E'));
                    }
                    _ => {}
                }

                keyboard.key(key);
                assert!(mouse.click(control, edge));
                assert_eq!(
                    keyboard.state(),
                    mouse.state(),
                    "{control:?} state differs at {width}x{height} {edge:?}"
                );
                assert_eq!(
                    keyboard.frame(),
                    mouse.frame(),
                    "{control:?} frame differs at {width}x{height} {edge:?}"
                );
            }
        }
    }
}

#[test]
fn summary_all_projects_toggle_has_keyboard_mouse_parity_and_stable_hitboxes() {
    for (width, height, theme) in [(120, 40, Theme::Dark), (60, 24, Theme::Light)] {
        let mut inactive = summary_harness(width, height, theme);
        inactive.assert_shortcut_inactive(ControlId::SummaryAllProjects);
        inactive.key(KeyCode::Char('G'));
        assert!(!inactive.app.summary_show_all_projects);
        assert!(!inactive.click(ControlId::SummaryAllProjects, ClickEdge::Middle));

        let base = summary_many_projects_harness(width, height, theme);
        base.assert_shortcut_distinct(ControlId::SummaryAllProjects);
        let original_rect = base.control_rect(ControlId::SummaryAllProjects);

        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            let mut keyboard = summary_many_projects_harness(width, height, theme);
            let mut mouse = summary_many_projects_harness(width, height, theme);
            keyboard.key(KeyCode::Char('G'));
            assert!(mouse.click(ControlId::SummaryAllProjects, edge));

            assert!(keyboard.app.summary_show_all_projects);
            assert_eq!(keyboard.state(), mouse.state());
            assert_eq!(keyboard.frame(), mouse.frame());
            assert_eq!(
                keyboard.control_rect(ControlId::SummaryAllProjects),
                original_rect
            );
            keyboard.assert_shortcut_distinct(ControlId::SummaryAllProjects);
        }
    }
}

#[test]
fn summary_all_projects_preference_survives_a_range_where_it_is_inapplicable() {
    let mut harness = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    let now = harness.app.snapshot.as_of;
    let older_groups = (0_u64..7)
        .map(|index| {
            summary_group(
                &format!("older-{index}-thread"),
                None,
                &format!("older-{index}-id"),
                &format!("older-{index}"),
                &format!("Older project {index}"),
                "cli",
                (index + 1) * 10_000,
            )
        })
        .collect();
    harness.app.history.half_hour_buckets = vec![
        summary_bucket(now - ChronoDuration::days(20), older_groups),
        summary_bucket(
            now - ChronoDuration::minutes(15),
            vec![summary_group(
                "recent-thread",
                None,
                "recent-id",
                "recent",
                "Recent project",
                "cli",
                10_000,
            )],
        ),
    ];
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('U'));
    harness.key(KeyCode::Char('M'));
    assert_eq!(
        harness
            .app
            .summary_cache
            .as_ref()
            .unwrap()
            .prepared
            .usage
            .projects
            .len(),
        8
    );
    harness.key(KeyCode::Char('G'));
    assert!(harness.app.summary_show_all_projects);

    harness.key(KeyCode::Char('C'));

    assert!(
        harness
            .app
            .summary_cache
            .as_ref()
            .unwrap()
            .prepared
            .usage
            .projects
            .len()
            <= SUMMARY_STACKED_PROJECT_LIMIT
    );
    assert!(harness.app.summary_show_all_projects);
    harness.assert_shortcut_inactive(ControlId::SummaryAllProjects);

    harness.key(KeyCode::Char('M'));
    assert!(harness.app.summary_show_all_projects);
    assert!(harness.frame().snapshot_text().contains("all projects"));
}

#[test]
fn summary_daily_readout_prioritizes_selection_and_other_and_reserves_omission_count() {
    let harness = summary_many_projects_harness(120, 40, Theme::Light);
    let cache = harness.app.summary_cache.as_ref().unwrap();
    let prepared = &cache.prepared;
    let chart = &cache.chart;
    let colors = summary_project_colors(&prepared.usage, &harness.app.history, harness.app.theme);
    let series = summary_project_series(&harness.app, prepared, chart, &colors);
    let index = chart
        .buckets
        .iter()
        .position(|bucket| bucket.starts_at.date() == NaiveDate::from_ymd_opt(2026, 7, 12).unwrap())
        .unwrap();
    let state = prepared.chart_bucket_state(
        &chart.buckets[index],
        chart.grain,
        harness.app.summary_metric,
        harness.app.api_long_context_multiplier,
    );
    let selected = "project-2-id";
    let line = summary_daily_readout_line(
        chart,
        &series,
        Some(selected),
        index,
        state,
        90,
        &harness.app,
    );
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let selected_at = text.find("project-2").unwrap();
    let other_at = text.find("Other").unwrap();
    let omitted_at = text.rfind("+").unwrap();
    assert!(selected_at < other_at && other_at < omitted_at, "{text}");

    let selected_span = line
        .spans
        .iter()
        .position(|span| span.content.contains("project-2"))
        .unwrap();
    assert_eq!(line.spans[selected_span - 1].content.as_ref(), "■ ");
    assert_eq!(
        line.spans[selected_span - 1].style.fg,
        Some(colors[selected])
    );
    assert_eq!(
        line.spans[selected_span].style.fg,
        Some(Theme::Light.palette().foreground)
    );
    assert!(
        line.spans[selected_span]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );

    let compact = summary_daily_readout_line(
        chart,
        &series,
        Some(selected),
        index,
        state,
        48,
        &harness.app,
    );
    let compact_text = compact
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(compact_text.contains("■ project-2 30.0K"), "{compact_text}");
    assert!(compact_text.contains("+6"), "{compact_text}");
}

#[test]
fn summary_open_history_bucket_is_partial_and_dst_overlap_is_explicit() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let open = &mut harness.app.history.half_hour_buckets[1];
    open.sampled_at = open.starts_at + ChronoDuration::minutes(5);
    let open_starts_at = open.starts_at;
    let prepared = prepare_summary(&harness.app, harness.app.snapshot.as_of);
    assert!(
        prepared
            .partial_reasons
            .iter()
            .any(|reason| reason == "history_bucket_open")
    );
    let chart = prepare_summary_chart(&prepared, SummaryGrain::Hour);
    let open_local_hour = display_local_hour(open_starts_at);
    let open_bucket = chart
        .buckets
        .iter()
        .find(|bucket| bucket.starts_at == open_local_hour)
        .unwrap();
    assert_eq!(
        prepared.chart_bucket_state(
            open_bucket,
            SummaryGrain::Hour,
            SummaryMetric::Tokens,
            false
        ),
        SummaryDailyState::Partial
    );
    let mut closed_history = harness.app.history.clone();
    closed_history.half_hour_buckets[1].sampled_at = closed_history.half_hour_buckets[1].ends_at;
    assert!(!summary_history_inputs_eq(
        &harness.app.history,
        &closed_history
    ));

    let repeated_hour = SummaryChartData {
        grain: SummaryGrain::Hour,
        buckets: vec![SummaryChartBucket {
            starts_at: NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(1, 0, 0)
                .unwrap(),
            totals: SummaryMetrics::default(),
            coverage: SummaryDailyCoverage {
                expected_buckets: 8,
                ..SummaryDailyCoverage::default()
            },
        }],
        project_values: HashMap::new(),
    };
    let readout = summary_daily_readout_line(
        &repeated_hour,
        &[],
        None,
        0,
        SummaryDailyState::Missing,
        80,
        &harness.app,
    );
    let text = readout
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("11-01 01:00 (DST overlap)"), "{text}");
}

#[test]
fn summary_light_project_body_uses_foreground_while_swatch_keeps_project_color() {
    let harness = summary_many_projects_harness(120, 40, Theme::Light);
    let second = &harness.app.summary_bar_hitboxes[1];
    let project_color = harness.app.summary_project_colors[&second.project_key];
    let (swatch, swatch_color, swatch_modifiers) = harness.cell_style(second.area.x, second.area.y);
    let (_, label_color, label_modifiers) =
        harness.cell_style(second.area.x.saturating_add(2), second.area.y);

    assert_eq!(swatch, "■");
    assert_eq!(swatch_color, project_color);
    assert!(swatch_modifiers.is_empty());
    assert_eq!(label_color, Theme::Light.palette().foreground);
    assert!(!label_modifiers.contains(Modifier::BOLD));
    assert_eq!(
        summary_project_body_style(Theme::Dark, project_color, false).fg,
        Some(project_color)
    );

    let tree = harness.app.summary_table_hitbox.unwrap();
    let tree_row = harness.app.summary_rows()[1].clone();
    let tree_project = tree_row.id.strip_prefix("project:").unwrap();
    let tree_color = harness.app.summary_project_colors[tree_project];
    let tree_y = tree.rows.y.saturating_add(1);
    let tree_swatch_x = (tree.rows.x..tree.rows.right())
        .find(|column| {
            let (symbol, color, _) = harness.cell_style(*column, tree_y);
            symbol == "■" && color == tree_color
        })
        .expect("project row should expose its stable color as a swatch");
    let (_, tree_label_color, tree_label_modifiers) =
        harness.cell_style(tree_swatch_x.saturating_add(2), tree_y);
    assert_eq!(tree_label_color, Theme::Light.palette().foreground);
    assert!(tree_label_modifiers.contains(Modifier::BOLD));
}

#[test]
fn summary_selected_project_row_keeps_its_stable_swatch_color() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = summary_many_projects_harness(120, 40, theme);
        for movement in 0..2 {
            if movement > 0 {
                harness.key(KeyCode::Down);
            }
            let rows = harness.app.summary_rows();
            let selected = harness.app.summary_selected_index(&rows);
            let row = &rows[selected];
            let project_key = row.id.strip_prefix("project:").unwrap();
            let expected_color = harness.app.summary_project_colors[project_key];
            let table = harness.app.summary_table_hitbox.unwrap();
            let row_y = table.rows.y.saturating_add(
                u16::try_from(selected.saturating_sub(table.offset)).unwrap_or(u16::MAX),
            );
            let (_, swatch_color, _) = (table.rows.x..table.rows.right())
                .find_map(|column| {
                    let cell = harness.cell_style(column, row_y);
                    (cell.0 == "■").then_some(cell)
                })
                .expect("selected project row should render a color swatch");
            assert_eq!(swatch_color, expected_color, "theme={theme:?}");
        }
    }
}

#[test]
fn summary_stacked_area_defaults_to_top_six_plus_other_and_can_show_every_project() {
    let mut harness = summary_many_projects_harness(120, 40, Theme::Dark);
    let cache = harness.app.summary_cache.as_ref().unwrap();
    let prepared = &cache.prepared;
    let chart = &cache.chart;
    let colors = summary_project_colors(&prepared.usage, &harness.app.history, harness.app.theme);
    assert_eq!(colors.values().copied().collect::<HashSet<_>>().len(), 8);
    let assigned = colors.values().copied().collect::<Vec<_>>();
    for (index, left) in assigned.iter().copied().enumerate() {
        for right in assigned.iter().copied().skip(index + 1) {
            assert!(
                summary_color_distance_squared(left, right)
                    >= SUMMARY_PROJECT_COLOR_MIN_DISTANCE_SQUARED,
                "project colors {left:?} and {right:?} are too similar"
            );
        }
    }
    let stable_color = colors["project-0-id"];
    let compacted = summary_project_series(&harness.app, prepared, chart, &colors);
    assert_eq!(compacted.len(), SUMMARY_STACKED_PROJECT_LIMIT + 1);
    assert_eq!(compacted.last().unwrap().label, "Other");
    for index in 0..chart.buckets.len() {
        let stacked_total = compacted
            .iter()
            .map(|series| {
                summary_project_metrics_at(series, chart, index)
                    .token_usage
                    .total_tokens
            })
            .fold(0_u64, u64::saturating_add);
        assert_eq!(
            stacked_total,
            chart.buckets[index].totals.token_usage.total_tokens
        );
    }
    assert!(harness.frame().snapshot_text().contains("Top 6 + Other"));

    harness.key(KeyCode::Char('G'));
    let cache = harness.app.summary_cache.as_ref().unwrap();
    let prepared = &cache.prepared;
    let chart = &cache.chart;
    let all_colors =
        summary_project_colors(&prepared.usage, &harness.app.history, harness.app.theme);
    let all = summary_project_series(&harness.app, prepared, chart, &all_colors);
    assert_eq!(all.len(), 8);
    assert!(all.iter().all(|series| series.project_key.is_some()));

    harness.key(KeyCode::Char('A'));
    let prepared = &harness.app.summary_cache.as_ref().unwrap().prepared;
    let api_colors =
        summary_project_colors(&prepared.usage, &harness.app.history, harness.app.theme);
    assert_eq!(api_colors["project-0-id"], stable_color);

    for theme in [Theme::Dark, Theme::Light] {
        let keys = prepared
            .usage
            .projects
            .iter()
            .map(|project| project.key.as_str())
            .collect::<Vec<_>>();
        let themed = assign_summary_project_colors(keys, theme);
        let themed_colors = themed.values().copied().collect::<Vec<_>>();
        for (index, left) in themed_colors.iter().copied().enumerate() {
            assert!(
                summary_color_distance_squared(left, theme.palette().background)
                    >= SUMMARY_PROJECT_COLOR_MIN_DISTANCE_SQUARED
            );
            for right in themed_colors.iter().copied().skip(index + 1) {
                assert!(
                    summary_color_distance_squared(left, right)
                        >= SUMMARY_PROJECT_COLOR_MIN_DISTANCE_SQUARED
                );
            }
        }
        let forward = assign_summary_project_colors(["project-207", "project-326"], theme);
        let reverse = assign_summary_project_colors(["project-326", "project-207"], theme);
        assert_eq!(forward, reverse);
        assert!(
            summary_color_distance_squared(forward["project-207"], forward["project-326"])
                >= SUMMARY_PROJECT_COLOR_MIN_DISTANCE_SQUARED
        );
    }
}

#[test]
fn summary_30d_hour_chart_keeps_each_projects_observations_sparse() {
    let mut harness = summary_many_projects_harness(120, 40, Theme::Dark);
    harness.key(KeyCode::Char('M'));
    for _ in 0..4 {
        harness.key(KeyCode::Char('B'));
    }

    let chart = &harness.app.summary_cache.as_ref().unwrap().chart;
    assert_eq!(chart.grain, SummaryGrain::Hour);
    assert!(chart.buckets.len() >= 30 * 24);
    assert_eq!(chart.project_values.len(), 8);
    assert!(
        chart
            .project_values
            .values()
            .all(|values| values.len() == 1)
    );
}

#[test]
fn summary_stacked_area_omits_projects_that_are_zero_for_the_selected_metric() {
    let mut harness = summary_many_projects_harness(120, 40, Theme::Dark);
    let zero_api_project = harness.app.history.half_hour_buckets[0].project_groups[0]
        .project_id
        .clone()
        .unwrap();
    let zero_api_group = &mut harness.app.history.half_hour_buckets[0].project_groups[0];
    zero_api_group.api_equivalent_cost = ApiCostAmount {
        observed_samples: 1,
        observed_tokens: zero_api_group.token_usage.total_tokens,
        ..ApiCostAmount::default()
    };
    harness.app.summary_metric = SummaryMetric::ApiEquivalent;
    harness.app.summary_show_all_projects = true;
    let prepared = prepare_summary(&harness.app, harness.app.snapshot.as_of);
    let chart = prepare_summary_chart(&prepared, harness.app.summary_grain);
    let colors = summary_project_colors(&prepared.usage, &harness.app.history, harness.app.theme);

    let series = summary_project_series(&harness.app, &prepared, &chart, &colors);

    assert_eq!(series.len(), 7);
    assert!(
        series.iter().all(|candidate| {
            candidate.project_key.as_deref() != Some(zero_api_project.as_str())
        })
    );
    assert!(series.iter().all(|candidate| {
        chart.buckets.iter().enumerate().any(|(index, _)| {
            SummaryMetric::ApiEquivalent
                .value(summary_project_metrics_at(candidate, &chart, index), false)
                > 0
        })
    }));
}

#[test]
fn summary_all_projects_uses_the_selected_metrics_nonzero_project_count() {
    let mut harness = summary_many_projects_harness(120, 40, Theme::Dark);
    harness.key(KeyCode::Char('G'));
    assert!(harness.app.summary_show_all_projects);

    let bucket = &mut harness.app.history.half_hour_buckets[0];
    for group in bucket.project_groups.iter_mut().take(3) {
        group.api_equivalent_cost = ApiCostAmount {
            observed_samples: 1,
            observed_tokens: group.token_usage.total_tokens,
            ..ApiCostAmount::default()
        };
    }
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('A'));

    assert!(harness.app.summary_show_all_projects);
    harness.assert_shortcut_inactive(ControlId::SummaryAllProjects);
    let frame = harness.frame().snapshot_text();
    assert!(frame.contains("Project mix · API EQ. · 1d local"));
    assert!(frame.contains("all projects"));
    assert!(!frame.contains("Top 6 + Other"));
    harness.key(KeyCode::Char('G'));
    assert!(harness.app.summary_show_all_projects);

    harness.key(KeyCode::Char('K'));
    harness.key(KeyCode::Char('G'));
    assert!(!harness.app.summary_show_all_projects);
}

#[test]
fn restored_all_projects_preference_survives_partial_data_until_projects_grow() {
    let mut harness = summary_many_projects_harness(120, 40, Theme::Dark);
    let all_groups = harness.app.history.half_hour_buckets[0]
        .project_groups
        .clone();
    harness.app.history.half_hour_buckets[0]
        .project_groups
        .truncate(3);
    harness.app.summary_cache = None;
    let saved = UiState {
        view: UiView::Summary,
        summary_show_all_projects: true,
        ..harness.app.ui_state()
    };
    harness.app.apply_ui_state(&saved, None);

    harness.render();
    let initial = harness.frame().snapshot_text();
    assert!(harness.app.summary_show_all_projects);
    harness.assert_shortcut_inactive(ControlId::SummaryAllProjects);
    assert!(initial.contains("all projects"));

    harness.app.history.half_hour_buckets[0].project_groups = all_groups;
    harness.app.summary_cache = None;
    harness.render();
    let complete = harness.frame().snapshot_text();
    assert!(harness.app.summary_show_all_projects);
    harness.assert_shortcut_distinct(ControlId::SummaryAllProjects);
    assert!(complete.contains("all projects"));
    assert!(!complete.contains("Top 6 + Other"));
}

#[test]
fn summary_cache_survives_refreshes_that_do_not_change_summary_inputs() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let next_as_of = harness.app.snapshot.as_of + ChronoDuration::seconds(2);
    let mut snapshot = harness.app.snapshot.clone();
    snapshot.as_of = next_as_of;
    harness.app.replace(
        CollectionResult {
            snapshot,
            account: harness.app.account.clone(),
            history_observation: HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );

    assert_eq!(
        harness
            .app
            .summary_cache
            .as_ref()
            .map(|cache| cache.snapshot_as_of),
        Some(next_as_of)
    );

    let mut marker_changed = harness.app.history.clone();
    marker_changed.summary_backfill_attempted_at = Some(next_as_of);
    harness.app.replace_history(marker_changed);
    assert!(harness.app.summary_cache.is_some());
}

#[test]
fn summary_cache_is_invalidated_by_live_metadata_or_bucket_usage_changes() {
    let mut metadata = summary_harness(120, 40, Theme::Dark);
    let mut snapshot = metadata.app.snapshot.clone();
    snapshot.as_of += ChronoDuration::seconds(2);
    snapshot.tasks[0].title.push_str(" renamed");
    metadata.app.replace(
        CollectionResult {
            snapshot,
            account: metadata.app.account.clone(),
            history_observation: HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    assert!(metadata.app.summary_cache.is_none());

    let mut usage = summary_harness(120, 40, Theme::Dark);
    let mut changed = usage.app.history.clone();
    changed.half_hour_buckets[0].token_usage.total_tokens += 1;
    changed.half_hour_buckets[0].project_groups[0]
        .token_usage
        .total_tokens += 1;
    usage.app.replace_history(changed);
    assert!(usage.app.summary_cache.is_none());
}

#[test]
fn summary_project_colors_stay_stable_across_range_subsets() {
    let mut harness = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    let mut collision_pair = ["project-207", "project-326"];
    collision_pair.sort_unstable_by_key(|project| summary_project_hash(project));
    let [older_project, recent_project] = collision_pair;
    assert!(
        summary_color_distance_squared(
            summary_project_color(older_project, harness.app.theme),
            summary_project_color(recent_project, harness.app.theme),
        ) < SUMMARY_PROJECT_COLOR_MIN_DISTANCE_SQUARED,
        "fixture must exercise a pair that requires collision resolution"
    );

    let now = harness.app.snapshot.as_of;
    harness.app.history.half_hour_buckets = vec![
        summary_bucket(
            now - ChronoDuration::days(20),
            vec![summary_group(
                "older-thread",
                None,
                older_project,
                "older-project",
                "Older task",
                "cli",
                20_000,
            )],
        ),
        summary_bucket(
            now - ChronoDuration::minutes(15),
            vec![summary_group(
                "recent-thread",
                None,
                recent_project,
                "recent-project",
                "Recent task",
                "cli",
                10_000,
            )],
        ),
    ];

    let mut prepared_for = |range| {
        harness.app.summary_range = range;
        prepare_summary(&harness.app, now)
    };
    let cycle = prepared_for(SummaryRange::Cycle);
    let seven_days = prepared_for(SummaryRange::SevenDays);
    let thirty_days = prepared_for(SummaryRange::ThirtyDays);
    for prepared in [&cycle, &seven_days, &thirty_days] {
        assert!(
            prepared
                .usage
                .projects
                .iter()
                .any(|project| project.key == recent_project)
        );
    }
    assert_eq!(cycle.usage.projects.len(), 1);
    assert_eq!(seven_days.usage.projects.len(), 1);
    assert_eq!(thirty_days.usage.projects.len(), 2);

    let scoped_seven = assign_summary_project_colors(
        seven_days
            .usage
            .projects
            .iter()
            .map(|project| project.key.as_str()),
        harness.app.theme,
    );
    let scoped_thirty = assign_summary_project_colors(
        thirty_days
            .usage
            .projects
            .iter()
            .map(|project| project.key.as_str()),
        harness.app.theme,
    );
    assert_ne!(
        scoped_seven[recent_project], scoped_thirty[recent_project],
        "fixture must reproduce the old range-dependent assignment"
    );

    let colors = [&cycle, &seven_days, &thirty_days].map(|prepared| {
        summary_project_colors(&prepared.usage, &harness.app.history, harness.app.theme)
            [recent_project]
    });
    assert_eq!(colors[0], colors[1]);
    assert_eq!(colors[1], colors[2]);

    let older_bucket = harness.app.history.half_hour_buckets[0].clone();
    let recent_bucket = harness.app.history.half_hour_buckets[1].clone();
    let mut refreshed = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    refreshed.app.summary_project_colors.clear();
    refreshed.app.replace_history(HistoryData {
        half_hour_buckets: vec![recent_bucket.clone()],
        ..HistoryData::default()
    });
    refreshed.key(KeyCode::Char('U'));
    let color_before_refresh = refreshed.app.summary_project_colors[recent_project];
    refreshed.app.replace_history(HistoryData {
        half_hour_buckets: vec![older_bucket, recent_bucket],
        ..HistoryData::default()
    });
    refreshed.render();
    assert_eq!(
        refreshed.app.summary_project_colors[recent_project],
        color_before_refresh
    );
}

#[test]
fn summary_controls_clip_only_whole_buttons_in_tiny_terminals() {
    for (width, height) in [(40, 12), (20, 8), (8, 3)] {
        let harness = summary_harness(width, height, Theme::Dark);
        let controls = harness.app.summary_controls_hitbox.unwrap();
        for control in controls.ranges.into_iter().chain(controls.metrics).chain([
            controls.bucket_grain,
            controls.toggle_all_projects,
            controls.toggle_long_context,
            controls.inspect,
            controls.toggle_selected,
            controls.collapse_all,
        ]) {
            if !control.is_empty() {
                assert!(matches!(control.width, 3 | 6), "width={width}");
                assert!(control.right() <= width, "width={width}");
                assert!(control.bottom() <= height, "height={height}");
            }
        }
    }
}

#[test]
fn summary_controls_keep_collapse_all_visible_at_standard_and_full_label_widths() {
    for (width, expected_width) in [(80, 3), (100, 3), (109, 3), (118, 11)] {
        let harness = summary_harness(width, 24, Theme::Dark);
        for control in [
            ControlId::SummaryRangeCycle,
            ControlId::SummaryRangeSevenDays,
            ControlId::SummaryRangeThirtyDays,
            ControlId::SummaryMetricTokens,
            ControlId::SummaryMetricEstimated,
            ControlId::SummaryMetricApiEquivalent,
            ControlId::SummaryBucketGrain,
            ControlId::SummaryAllProjects,
            ControlId::SummaryLongContext,
            ControlId::SummaryInspect,
            ControlId::SummaryToggle,
            ControlId::SummaryCollapseAll,
        ] {
            assert!(
                !harness.control_rect(control).is_empty(),
                "{control:?} missing at width {width}"
            );
        }
        assert_eq!(
            harness.control_rect(ControlId::SummaryCollapseAll).width,
            expected_width
        );
    }
}

#[test]
fn summary_bucket_grain_cycles_with_keyboard_and_mouse_and_keeps_a_stable_hitbox() {
    for (width, height, theme) in [(120, 40, Theme::Dark), (60, 24, Theme::Light)] {
        let mut harness = summary_harness(width, height, theme);
        let stable_rect = harness.control_rect(ControlId::SummaryBucketGrain);
        assert_eq!(stable_rect.width, 6);
        harness.assert_shortcut_distinct(ControlId::SummaryBucketGrain);

        let mut point_counts = vec![
            harness
                .app
                .summary_cache
                .as_ref()
                .unwrap()
                .chart
                .buckets
                .len(),
        ];
        for expected in [
            SummaryGrain::Hours12,
            SummaryGrain::Hours6,
            SummaryGrain::Hours3,
            SummaryGrain::Hour,
            SummaryGrain::Day,
        ] {
            harness.key(KeyCode::Char('B'));
            assert_eq!(harness.app.summary_grain, expected);
            assert_eq!(
                harness.app.summary_cache.as_ref().unwrap().chart.grain,
                expected
            );
            assert_eq!(
                harness.control_rect(ControlId::SummaryBucketGrain),
                stable_rect
            );
            let frame = harness.frame().snapshot_text();
            assert!(
                frame.contains(&format!("{} local", expected.label())),
                "{frame}"
            );
            assert!(
                frame.contains(&format!("[B]{}", expected.control_suffix())),
                "{frame}"
            );
            point_counts.push(
                harness
                    .app
                    .summary_cache
                    .as_ref()
                    .unwrap()
                    .chart
                    .buckets
                    .len(),
            );
        }
        assert!(point_counts[..5].windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(point_counts.first(), point_counts.last());
    }

    for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
        let mut keyboard = summary_harness(60, 24, Theme::Dark);
        let mut mouse = summary_harness(60, 24, Theme::Dark);
        keyboard.key(KeyCode::Char('B'));
        assert!(mouse.click(ControlId::SummaryBucketGrain, edge));
        assert_eq!(keyboard.state(), mouse.state());
        assert_eq!(keyboard.frame(), mouse.frame());
    }
}

#[test]
fn summary_subday_chart_shows_local_time_and_disables_exact_inspect_when_compressed() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    harness.key(KeyCode::Char('B'));
    let frame = harness.frame().snapshot_text();
    assert!(frame.contains("12h local"), "{frame}");
    assert!(frame.contains("07-12 00:00"), "{frame}");

    harness.key(KeyCode::Char('M'));
    for _ in 0..3 {
        harness.key(KeyCode::Char('B'));
    }
    assert_eq!(harness.app.summary_grain, SummaryGrain::Hour);
    assert!(
        harness
            .app
            .summary_cache
            .as_ref()
            .unwrap()
            .chart
            .buckets
            .len()
            > 700
    );
    assert!(harness.app.summary_daily_hitbox.is_none());
    harness.assert_shortcut_inactive(ControlId::SummaryInspect);
}

#[test]
fn summary_collapse_all_has_keyboard_mouse_parity_and_stable_whole_label_hitbox() {
    for (width, height, theme) in [(120, 40, Theme::Dark), (60, 24, Theme::Light)] {
        let mut inactive = summary_harness(width, height, theme);
        let inactive_rect = inactive.control_rect(ControlId::SummaryCollapseAll);
        assert!(!inactive_rect.is_empty(), "missing at {width}x{height}");
        inactive.assert_shortcut_inactive(ControlId::SummaryCollapseAll);
        let selected_before = inactive.app.summary_selected_id.clone();
        inactive.key(KeyCode::Char('X'));
        assert!(inactive.app.summary_expanded_nodes.is_empty());
        assert_eq!(inactive.app.summary_selected_id, selected_before);
        assert!(!inactive.click(ControlId::SummaryCollapseAll, ClickEdge::Middle));

        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            let mut keyboard = summary_harness(width, height, theme);
            let mut mouse = summary_harness(width, height, theme);
            for harness in [&mut keyboard, &mut mouse] {
                harness.key(KeyCode::Enter);
                harness.key(KeyCode::Down);
                harness.key(KeyCode::Enter);
                harness.key(KeyCode::Down);
                assert_eq!(harness.app.summary_expanded_nodes.len(), 2);
                assert_eq!(
                    harness.app.summary_selected_id.as_deref(),
                    Some("turn:alpha-root:alpha-user-turn")
                );
                assert_eq!(
                    harness.control_rect(ControlId::SummaryCollapseAll),
                    inactive_rect,
                    "collapse-all hitbox moved at {width}x{height}"
                );
                harness.assert_shortcut_distinct(ControlId::SummaryCollapseAll);
            }

            keyboard.key(KeyCode::Char('X'));
            assert!(mouse.click(ControlId::SummaryCollapseAll, edge));

            assert_eq!(keyboard.state(), mouse.state());
            assert_eq!(keyboard.frame(), mouse.frame());
            for harness in [&keyboard, &mouse] {
                assert!(harness.app.summary_expanded_nodes.is_empty());
                assert!(
                    harness
                        .app
                        .summary_rows()
                        .iter()
                        .all(|row| row.kind == SummaryRowKind::Project && row.collapsed)
                );
                assert_eq!(
                    harness.app.summary_selected_id.as_deref(),
                    Some("project:alpha-id")
                );
                harness.assert_shortcut_inactive(ControlId::SummaryCollapseAll);
            }
        }
    }
}

#[test]
fn summary_wide_layout_balances_the_top_panels_and_gives_the_bottom_chart_full_width() {
    for theme in [Theme::Dark, Theme::Light] {
        let harness = summary_harness(120, 40, theme);
        let tree = harness.app.summary_table_hitbox.unwrap().viewport;
        let bars = harness.app.summary_bar_hitboxes.first().unwrap().area;
        let plot = harness.app.summary_daily_hitbox.as_ref().unwrap().plot;

        assert_eq!(tree.width, bars.width);
        assert_eq!(bars.x.saturating_sub(tree.x), 60);
        assert!(
            plot.x < bars.x,
            "plot must begin in the left half: {plot:?}"
        );
        assert!(plot.right() >= bars.x + bars.width, "{plot:?}");
        assert!(plot.y > tree.bottom().saturating_sub(1), "{plot:?}");
    }
}

#[test]
fn summary_cycle_marks_the_rolling_fallback_when_weekly_server_data_is_unavailable() {
    let mut harness = TuiHarness::from_fixture("empty", 80, 24, Theme::Dark);
    harness.key(KeyCode::Char('U'));
    let frame = harness.frame().snapshot_text();

    assert!(frame.contains("Cycle (7d fallback)"));
    assert!(frame.contains("PARTIAL"));
}

#[test]
fn summary_rolling_window_advances_when_the_snapshot_clock_is_frozen() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    harness.key(KeyCode::Char('7'));
    let snapshot_as_of = harness.app.snapshot.as_of;
    let initial_bucket = harness.app.summary_cache.as_ref().unwrap().query_bucket;
    assert!(!harness.app.summary_rows().is_empty());

    let later = snapshot_as_of + ChronoDuration::days(8) + ChronoDuration::minutes(16);
    harness.render_at(later);
    let cache = harness.app.summary_cache.as_ref().unwrap();

    assert_ne!(cache.query_bucket, initial_bucket);
    assert_eq!(
        cache.prepared.usage.window.ends_at,
        later + ChronoDuration::nanoseconds(1)
    );
    assert!(harness.app.summary_rows().is_empty());
}

#[test]
fn summary_cycle_falls_back_after_a_frozen_server_window_expires() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let after_weekly_reset = summary_timestamp("2026-07-16T04:31:00Z");

    harness.render_at(after_weekly_reset);

    assert!(
        harness
            .frame()
            .snapshot_text()
            .contains("Cycle (7d fallback)")
    );
}

#[test]
fn summary_cycle_cache_expires_inside_the_same_fifteen_minute_bucket() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let before_reset = harness.app.snapshot.as_of;
    let reset_at = before_reset + ChronoDuration::minutes(5);
    let weekly = harness
        .app
        .snapshot
        .window_analyses
        .iter_mut()
        .find(|analysis| analysis.duration_mins == WindowScope::Week.duration_mins())
        .unwrap();
    weekly.attribution.window.as_mut().unwrap().ends_at = reset_at;
    harness.app.summary_cache = None;

    harness.render_at(before_reset);
    let initial_bucket = harness.app.summary_cache.as_ref().unwrap().query_bucket;
    assert_eq!(
        harness
            .app
            .summary_cache
            .as_ref()
            .unwrap()
            .prepared
            .range_note,
        None
    );

    harness.render_at(reset_at + ChronoDuration::seconds(1));
    let cache = harness.app.summary_cache.as_ref().unwrap();
    assert_eq!(cache.query_bucket, initial_bucket);
    assert_eq!(cache.prepared.range_note, Some("7d fallback"));
    assert!(
        harness
            .frame()
            .snapshot_text()
            .contains("Cycle (7d fallback)")
    );
}

#[test]
fn summary_disambiguates_same_basename_projects_without_showing_paths() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    harness.app.history.half_hour_buckets = vec![summary_bucket(
        summary_timestamp("2026-07-12T03:45:00Z"),
        vec![
            summary_group("first", None, "first-id", "repo", "First", "cli", 2_000),
            summary_group("second", None, "second-id", "repo", "Second", "cli", 1_000),
        ],
    )];
    harness.app.summary_cache = None;
    harness.render();

    let rows = harness.app.summary_rows();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].label, rows[1].label);
    assert!(rows.iter().all(|row| row.label.starts_with("repo · ")));
    assert!(rows.iter().all(|row| !row.label.contains('/')));
}

#[test]
fn summary_ignores_project_metadata_from_outside_the_selected_range() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    harness.app.history.half_hour_buckets = vec![
        summary_bucket(
            summary_timestamp("2026-06-01T00:00:00Z"),
            vec![summary_group(
                "same-thread",
                None,
                "old-id",
                "old-project",
                "Old title",
                "cli",
                1_000,
            )],
        ),
        summary_bucket(
            summary_timestamp("2026-07-12T03:45:00Z"),
            vec![summary_group(
                "same-thread",
                None,
                "current-id",
                "current-project",
                "Current title",
                "cli",
                2_000,
            )],
        ),
    ];
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('7'));

    let rows = harness.app.summary_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "project:current-id");
    assert_eq!(rows[0].label, "current-project");
}

#[test]
fn summary_live_task_metadata_overrides_stale_history_title() {
    let mut harness = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    harness.app.history.half_hour_buckets = vec![summary_bucket(
        summary_timestamp("2026-07-12T03:45:00Z"),
        vec![summary_group(
            "019f52ac-7a9f-7fd1-8dda-e775ef950785",
            None,
            "project-id",
            "old-label",
            "Untitled task",
            "cli",
            2_000,
        )],
    )];
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('U'));
    harness.key(KeyCode::Enter);

    let rows = harness.app.summary_rows();
    assert_eq!(rows[1].label, "Build the integration test harness");
    let frame = harness.frame().snapshot_text();
    assert!(frame.contains("Build the integration"));
    assert!(!frame.contains("Untitled task"));
}

#[test]
fn summary_api_range_and_lower_bound_marker_remain_visible() {
    for width in [120, 60] {
        let mut harness = TuiHarness::from_fixture("normal", width, 24, Theme::Dark);
        let mut group = summary_group(
            "api-thread",
            None,
            "api-project",
            "api-project",
            "API range",
            "cli",
            2_000,
        );
        group.api_equivalent_cost = ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(123_400_000_000),
            maximum_pico_usd: PicoUsd::new(567_800_000_000),
            observed_samples: 2,
            priced_samples: 1,
            observed_tokens: 2_000,
            priced_tokens: 1_000,
        };
        harness.app.history.half_hour_buckets = vec![summary_bucket(
            summary_timestamp("2026-07-12T03:45:00Z"),
            vec![group],
        )];
        harness.app.summary_cache = None;
        harness.key(KeyCode::Char('U'));
        harness.key(KeyCode::Char('A'));

        let frame = harness.frame().snapshot_text();
        assert!(
            frame.contains("$0.1234–$0.5678+"),
            "API range was clipped at width {width}: {frame}"
        );
    }
}

#[test]
fn summary_daily_marks_a_fully_covered_non_exact_api_range_as_a_lower_bound() {
    let mut harness = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    let now = harness.app.snapshot.as_of;
    let (window, _) = SummaryRange::SevenDays.window(&harness.app.snapshot, now);
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let mut starts_at_seconds = window
        .starts_at
        .timestamp()
        .div_euclid(bucket_seconds)
        .saturating_mul(bucket_seconds);
    let aligned = DateTime::from_timestamp(starts_at_seconds, 0).unwrap();
    if aligned < window.starts_at {
        starts_at_seconds = starts_at_seconds.saturating_add(bucket_seconds);
    }
    let mut starts_at = DateTime::from_timestamp(starts_at_seconds, 0).unwrap();
    let mut buckets = Vec::new();
    let mut inserted_usage = false;
    while starts_at < window.ends_at {
        let groups = if inserted_usage {
            Vec::new()
        } else {
            inserted_usage = true;
            let mut group = summary_group(
                "api-range-thread",
                None,
                "api-range-project",
                "api-range-project",
                "Fully priced API range",
                "cli",
                2_000,
            );
            group.api_equivalent_cost = ApiCostAmount {
                minimum_pico_usd: PicoUsd::new(123_400_000_000),
                maximum_pico_usd: PicoUsd::new(567_800_000_000),
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: 2_000,
                priced_tokens: 2_000,
            };
            vec![group]
        };
        buckets.push(summary_bucket(starts_at, groups));
        starts_at += ChronoDuration::minutes(LOCAL_BUCKET_MINUTES);
    }
    assert!(inserted_usage);
    harness.app.history = HistoryData {
        half_hour_buckets: buckets,
        ..HistoryData::default()
    };
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('U'));
    harness.key(KeyCode::Char('7'));
    harness.key(KeyCode::Char('A'));

    let cache = harness.app.summary_cache.as_ref().unwrap();
    let prepared = &cache.prepared;
    assert_eq!(prepared.covered_buckets, prepared.expected_buckets);
    assert_eq!(prepared.represented_tokens, prepared.available_tokens);
    assert!(prepared.partial(SummaryMetric::ApiEquivalent, false));
    assert_eq!(
        prepared.coverage_state(SummaryMetric::ApiEquivalent, false),
        SummaryDailyState::Partial
    );
    assert!(prepared.api_chart_is_lower_bound());
    assert!(summary_daily_is_lower_bound(
        prepared,
        &cache.chart,
        SummaryMetric::ApiEquivalent,
        false,
    ));
    assert!(harness.frame().snapshot_text().contains("LOWER BOUND"));
}

#[test]
fn summary_api_marks_equal_sample_but_partial_token_pricing_as_a_lower_bound() {
    let harness = summary_harness(120, 40, Theme::Dark);
    let mut prepared = prepare_summary(&harness.app, harness.app.snapshot.as_of);
    prepared.partial_reasons.clear();
    prepared.available_tokens = 2_000;
    prepared.represented_tokens = 2_000;
    prepared.covered_buckets = 1;
    prepared.expected_buckets = 1;
    prepared.usage.totals.api_equivalent_cost = ApiCostAmount {
        minimum_pico_usd: PicoUsd::new(123_400_000_000),
        maximum_pico_usd: PicoUsd::new(123_400_000_000),
        observed_samples: 1,
        priced_samples: 1,
        observed_tokens: 2_000,
        priced_tokens: 1_000,
    };

    assert!(prepared.partial(SummaryMetric::ApiEquivalent, false));
    assert!(prepared.api_chart_is_lower_bound());
    assert!(prepared.coverage_percent(SummaryMetric::ApiEquivalent) < 100.0);
}

#[test]
fn summary_estimate_excludes_buckets_from_an_old_estimator_revision() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let outdated = &mut harness.app.history.half_hour_buckets[0];
    outdated.estimator_revision = HISTORY_ESTIMATOR_REVISION.saturating_sub(1);
    let expected_units = harness.app.history.half_hour_buckets[1..]
        .iter()
        .flat_map(|bucket| &bucket.project_groups)
        .map(|group| group.estimated_cost_units)
        .fold(0_u128, u128::saturating_add);
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('E'));

    let prepared = &harness.app.summary_cache.as_ref().unwrap().prepared;
    assert_eq!(prepared.usage.totals.estimated_cost_units, expected_units);
    assert!(
        prepared
            .partial_reasons
            .iter()
            .any(|reason| reason == "estimator_revision_changed")
    );
    assert!(prepared.coverage_percent(SummaryMetric::Estimated) < 100.0);
}

#[test]
fn summary_does_not_consume_an_unknown_project_breakdown_revision() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    harness.app.history.half_hour_buckets.truncate(1);
    let bucket = &mut harness.app.history.half_hour_buckets[0];
    let available_tokens = bucket.token_usage.total_tokens;
    bucket.project_breakdown_revision = HISTORY_PROJECT_BREAKDOWN_REVISION.saturating_add(1);
    harness.app.summary_cache = None;
    harness.render();

    let prepared = &harness.app.summary_cache.as_ref().unwrap().prepared;
    assert_eq!(prepared.available_tokens, available_tokens);
    assert_eq!(prepared.represented_tokens, 0);
    assert_eq!(prepared.usage.totals.token_usage.total_tokens, 0);
    assert!(
        prepared
            .partial_reasons
            .iter()
            .any(|reason| reason == "project_breakdown_unavailable")
    );
}

#[test]
fn summary_longx_does_not_treat_spark_only_metadata_as_unknown_est_usage() {
    let mut harness = TuiHarness::from_fixture("normal", 120, 40, Theme::Dark);
    let group = LocalProjectUsageGroup {
        thread_id: "spark-thread".to_string(),
        project_id: Some("spark-project".to_string()),
        project_label: Some("spark-project".to_string()),
        title: Some("Spark task".to_string()),
        source: Some("desktop".to_string()),
        api_long_context_extra_cost_units: None,
        call_count: 1,
        ..LocalProjectUsageGroup::default()
    };
    harness.app.history.half_hour_buckets = vec![summary_bucket(
        summary_timestamp("2026-07-12T03:45:00Z"),
        vec![group],
    )];
    harness.app.summary_cache = None;
    harness.key(KeyCode::Char('U'));
    harness.key(KeyCode::Char('E'));
    harness.key(KeyCode::Char('L'));

    assert!(
        harness
            .app
            .summary_cache
            .as_ref()
            .unwrap()
            .prepared
            .long_context_breakdown_complete
    );
}

#[test]
fn summary_tree_defaults_collapsed_and_enter_expands_project_then_session() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let alpha = harness
        .app
        .summary_cache
        .as_ref()
        .unwrap()
        .prepared
        .usage
        .projects
        .iter()
        .find(|project| project.key == "alpha-id")
        .unwrap();
    assert_eq!(alpha.sessions.len(), 1);
    assert_eq!(alpha.sessions[0].turns.len(), 1);
    assert_eq!(alpha.sessions[0].totals.token_usage.total_tokens, 190_000);
    assert_eq!(
        alpha.sessions[0].turns[0].totals.token_usage.total_tokens,
        190_000
    );
    assert_eq!(
        alpha.sessions[0].turns[0].message_preview.as_deref(),
        Some("Explain the API billing discrepancy")
    );

    let rows = harness.app.summary_rows();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.id.starts_with("project:")));
    assert!(rows.iter().all(|row| row.kind == SummaryRowKind::Project));
    assert!(rows.iter().all(|row| row.collapsed));
    assert_eq!(rows[0].label, "alpha-service");
    assert_eq!(rows[0].metrics.token_usage.total_tokens, 190_000);
    assert!(
        !harness
            .frame()
            .snapshot_text()
            .contains("API billing dashboard")
    );

    harness.key(KeyCode::Enter);
    let project_expanded = harness.app.summary_rows();
    assert_eq!(project_expanded.len(), 4);
    assert_eq!(
        project_expanded[0].metrics.token_usage.total_tokens,
        190_000
    );
    assert_eq!(
        project_expanded[1].metrics.token_usage.total_tokens, 190_000,
        "a collapsed session row should include its descendants"
    );
    assert!(
        harness
            .frame()
            .snapshot_text()
            .contains("API billing dashboard")
    );
    assert!(
        !harness
            .frame()
            .snapshot_text()
            .contains("Price catalog research")
    );

    harness.key(KeyCode::Down);
    harness.key(KeyCode::Enter);
    let session_expanded = harness.app.summary_rows();
    assert_eq!(session_expanded.len(), 5);
    assert_eq!(
        session_expanded[0].metrics.token_usage.total_tokens,
        190_000
    );
    assert_eq!(
        session_expanded[1].metrics.token_usage.total_tokens, 190_000,
        "an expanded session row should retain the session aggregate"
    );
    assert_eq!(
        session_expanded[2].metrics.token_usage.total_tokens,
        190_000
    );
    assert_eq!(session_expanded[2].id, "turn:alpha-root:alpha-user-turn");
    let expanded = harness.frame().snapshot_text();
    assert!(expanded.contains("TURN Explain"));
    assert!(!expanded.contains("[ ] TURN"));
    assert!(!expanded.contains("Price catalog research"));
    assert!(!expanded.contains("SUB "));
    assert_eq!(session_expanded[0].kind, SummaryRowKind::Project);
    assert_eq!(session_expanded[1].kind, SummaryRowKind::Session);
    assert_eq!(session_expanded[2].kind, SummaryRowKind::Turn);
}

#[test]
fn summary_tree_keeps_direct_and_delegated_unassigned_turn_rows_distinct() {
    for (width, height, theme) in [(120, 40, Theme::Dark), (60, 24, Theme::Light)] {
        let mut direct = summary_group(
            "root",
            None,
            "alpha-id",
            "alpha-service",
            "Root session",
            "desktop",
            10_000,
        );
        direct.turn_id = None;
        direct.session_turn_id = None;
        direct.message_preview = Some("preview must not mask the direct fallback".to_string());

        let mut delegated_a = summary_group(
            "child-a",
            Some("root"),
            "alpha-id",
            "alpha-service",
            "Delegated A",
            "subagent",
            20_000,
        );
        delegated_a.session_turn_id = None;
        delegated_a.message_preview =
            Some("preview must not mask the delegated fallback".to_string());
        let mut delegated_b = summary_group(
            "child-b",
            Some("root"),
            "alpha-id",
            "alpha-service",
            "Delegated B",
            "subagent",
            30_000,
        );
        delegated_b.session_turn_id = None;

        let mut harness = TuiHarness::from_fixture("normal", width, height, theme);
        harness.app.history.half_hour_buckets = vec![summary_bucket(
            summary_timestamp("2026-07-12T03:45:00Z"),
            vec![direct, delegated_a, delegated_b],
        )];
        harness.app.summary_cache = None;
        harness.key(KeyCode::Char('U'));
        harness.key(KeyCode::Enter);
        harness.key(KeyCode::Down);
        harness.key(KeyCode::Enter);

        let rows = harness.app.summary_rows();
        let direct = rows
            .iter()
            .find(|row| row.id == "turn-unassigned-session:root")
            .unwrap();
        assert_eq!(direct.label, "Unassigned session usage");
        assert_eq!(direct.metrics.token_usage.total_tokens, 10_000);
        let delegated = rows
            .iter()
            .find(|row| row.id == "turn-unassigned-delegated:root")
            .unwrap();
        assert_eq!(delegated.label, "Unassigned delegated usage");
        assert_eq!(delegated.metrics.token_usage.total_tokens, 50_000);
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SummaryRowKind::Turn)
                .count(),
            2
        );

        let frame = harness.frame().snapshot_text();
        assert!(frame.contains("Unassigned"), "{frame}");
        assert!(!frame.contains("preview must not mask"), "{frame}");
    }
}

#[test]
fn summary_tree_plus_minus_match_the_highlighted_marker_shortcuts() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let marker = harness.app.summary_tree_marker_hitboxes[0].area;
    let (symbol, _, modifier) = harness.cell_style(marker.x + 1, marker.y);
    assert_eq!(symbol, "+");
    assert!(modifier.contains(Modifier::BOLD | Modifier::UNDERLINED));

    harness.key(KeyCode::Char('+'));
    assert_eq!(harness.app.summary_rows().len(), 4);
    let marker = harness.app.summary_tree_marker_hitboxes[0].area;
    assert_eq!(harness.cell_style(marker.x + 1, marker.y).0, "-");

    harness.key(KeyCode::Char('-'));
    assert_eq!(harness.app.summary_rows().len(), 3);
    let marker = harness.app.summary_tree_marker_hitboxes[0].area;
    assert_eq!(harness.cell_style(marker.x + 1, marker.y).0, "+");
}

#[test]
fn summary_tree_markers_are_whole_mouse_targets_for_project_and_session() {
    for (width, height, theme) in [
        (120, 40, Theme::Dark),
        (120, 40, Theme::Light),
        (60, 24, Theme::Dark),
        (60, 24, Theme::Light),
    ] {
        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            let mut harness = summary_harness(width, height, theme);
            let project_marker = harness.app.summary_tree_marker_hitboxes[0].area;
            let x = match edge {
                ClickEdge::Start => project_marker.x,
                ClickEdge::Middle => project_marker.x + project_marker.width / 2,
                ClickEdge::End => project_marker.right() - 1,
            };
            assert!(summary_mouse(
                &mut harness,
                MouseEventKind::Down(MouseButton::Left),
                x,
                project_marker.y,
            ));
            assert_eq!(harness.app.summary_rows().len(), 4);
            assert_eq!(
                harness.app.summary_selected_id.as_deref(),
                Some("project:alpha-id")
            );

            let session_marker = harness.app.summary_tree_marker_hitboxes[1].area;
            assert!(summary_mouse(
                &mut harness,
                MouseEventKind::Down(MouseButton::Left),
                session_marker.x + session_marker.width / 2,
                session_marker.y,
            ));
            let rows = harness.app.summary_rows();
            assert_eq!(rows.len(), 5);
            assert_eq!(rows[2].kind, SummaryRowKind::Turn);
            assert_eq!(
                harness.app.summary_selected_id.as_deref(),
                Some("thread:alpha-root")
            );
        }
    }
}

#[test]
fn summary_tree_labels_projects_sessions_and_turns_without_color_only_cues() {
    for (width, height, theme) in [
        (120, 40, Theme::Dark),
        (120, 40, Theme::Light),
        (60, 24, Theme::Dark),
        (60, 24, Theme::Light),
    ] {
        let mut harness = summary_harness(width, height, theme);
        let collapsed = harness.frame().snapshot_text();
        assert!(
            collapsed.contains("TYPE · PROJECT / SESSION / TURN"),
            "{collapsed}"
        );
        assert!(
            collapsed.contains("[+] PROJ ■ alpha-service"),
            "{collapsed}"
        );

        harness.key(KeyCode::Enter);
        harness.key(KeyCode::Down);
        harness.key(KeyCode::Enter);
        let expanded = harness.frame().snapshot_text();
        assert!(expanded.contains("PROJ ■ alpha-service"), "{expanded}");
        assert!(expanded.contains("SESS API billing"), "{expanded}");
        assert!(expanded.contains("TURN Explain"), "{expanded}");
        assert!(!expanded.contains("[ ] TURN"), "{expanded}");
        assert!(!expanded.contains("SUB "), "{expanded}");
    }
}

#[test]
fn summary_toggle_is_inactive_on_leaf_rows_for_keyboard_and_mouse() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    harness.key(KeyCode::Enter);
    harness.key(KeyCode::Down);
    harness.key(KeyCode::Enter);
    harness.key(KeyCode::Down);

    let selected = harness.app.summary_selected_id.clone().unwrap();
    assert!(
        harness
            .app
            .summary_rows()
            .iter()
            .any(|row| row.id == selected && !row.has_children)
    );
    harness.assert_shortcut_inactive(ControlId::SummaryToggle);

    let rows_before = harness
        .app
        .summary_rows()
        .into_iter()
        .map(|row| (row.id, row.collapsed))
        .collect::<Vec<_>>();
    harness.key(KeyCode::Enter);
    assert_eq!(
        harness
            .app
            .summary_rows()
            .into_iter()
            .map(|row| (row.id, row.collapsed))
            .collect::<Vec<_>>(),
        rows_before
    );
    assert!(!harness.click(ControlId::SummaryToggle, ClickEdge::Middle));
    assert_eq!(
        harness
            .app
            .summary_rows()
            .into_iter()
            .map(|row| (row.id, row.collapsed))
            .collect::<Vec<_>>(),
        rows_before
    );
}

#[test]
fn summary_page_scroll_is_not_undone_by_the_selected_row() {
    let mut harness = summary_harness(60, 24, Theme::Dark);
    let groups = (0_u64..24)
        .map(|index| {
            summary_group(
                &format!("thread-{index}"),
                None,
                &format!("project-{index}"),
                &format!("project-{index:02}"),
                &format!("Task {index}"),
                "cli",
                1_000 + index,
            )
        })
        .collect();
    harness.app.history.half_hour_buckets = vec![summary_bucket(
        summary_timestamp("2026-07-12T03:45:00Z"),
        groups,
    )];
    harness.app.summary_cache = None;
    harness.render();

    assert_eq!(harness.app.summary_rows().len(), 24);
    assert_eq!(harness.state().summary_offset, 0);
    harness.key(KeyCode::PageDown);
    let scrolled = harness.state().summary_offset;
    assert!(scrolled > 0);

    harness.render();
    assert_eq!(harness.state().summary_offset, scrolled);
}

#[test]
fn settings_rows_have_keyboard_mouse_parity_and_whole_row_hitboxes() {
    for (width, height) in [(120, 40), (60, 24)] {
        for (key, control) in [
            (KeyCode::Char('T'), ControlId::SettingTheme),
            (KeyCode::Char('V'), ControlId::SettingTurns),
            (KeyCode::Char('M'), ControlId::SettingModels),
            (KeyCode::Char('L'), ControlId::SettingLongContext),
            (KeyCode::Char('K'), ControlId::SettingTokens),
            (KeyCode::Char('P'), ControlId::SettingTokenShare),
            (KeyCode::Char('E'), ControlId::SettingEstimatedQuota),
            (KeyCode::Char('A'), ControlId::SettingApiEquivalent),
        ] {
            let mut keyboard = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            let mut mouse = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            keyboard.key(KeyCode::Char('4'));
            mouse.key(KeyCode::Char('4'));
            keyboard.key(key);
            assert!(mouse.click(control, ClickEdge::End));
            assert_eq!(keyboard.state(), mouse.state(), "{control:?} state");
            assert_eq!(keyboard.frame(), mouse.frame(), "{control:?} frame");
        }

        let mut settings = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
        settings.key(KeyCode::Char('4'));
        for control in [
            ControlId::SettingTheme,
            ControlId::SettingTurns,
            ControlId::SettingModels,
            ControlId::SettingLongContext,
            ControlId::SettingTokens,
            ControlId::SettingTokenShare,
            ControlId::SettingEstimatedQuota,
            ControlId::SettingApiEquivalent,
        ] {
            settings.assert_shortcut_distinct(control);
            assert!(!settings.control_rect(control).is_empty());
        }

        settings.key(KeyCode::Esc);
        assert!(settings.state().quit_confirmation);
        for control in [
            ControlId::SettingTheme,
            ControlId::SettingTurns,
            ControlId::SettingModels,
            ControlId::SettingLongContext,
            ControlId::SettingTokens,
            ControlId::SettingTokenShare,
            ControlId::SettingEstimatedQuota,
            ControlId::SettingApiEquivalent,
        ] {
            settings.assert_shortcut_inactive(control);
        }
    }
}

#[test]
fn compact_settings_scrolls_to_keep_keyboard_selection_visible() {
    let mut settings = TuiHarness::from_fixture("normal", 60, 10, Theme::Dark);
    settings.key(KeyCode::Char('4'));
    assert!(
        settings
            .control_rect(ControlId::SettingApiEquivalent)
            .is_empty()
    );

    settings.key(KeyCode::End);
    assert_eq!(
        settings.app.selected_setting,
        SettingItem::ALL.len(),
        "an empty Remote panel owns the last synthetic focus position"
    );
    let remote_controls = settings
        .app
        .settings_controls_hitbox
        .as_ref()
        .expect("settings controls should render");
    assert!(!remote_controls.remote_global.is_empty());
    assert!(settings.frame().snapshot_text().contains("Remote sources"));
    assert!(settings.control_rect(ControlId::SettingTheme).is_empty());

    settings.key(KeyCode::Up);
    assert_eq!(
        settings.app.selected_setting,
        SettingItem::ApiEquivalent.index()
    );
    assert!(
        !settings
            .control_rect(ControlId::SettingApiEquivalent)
            .is_empty()
    );
    assert!(settings.frame().snapshot_text().contains("API equivalent"));

    let before = settings.app.table_columns.api_equivalent;
    settings.key(KeyCode::Enter);
    assert_ne!(settings.app.table_columns.api_equivalent, before);
    assert!(settings.click(ControlId::SettingApiEquivalent, ClickEdge::End));
    assert_eq!(settings.app.table_columns.api_equivalent, before);

    settings.key(KeyCode::Home);
    assert_eq!(settings.app.selected_setting, SettingItem::Theme.index());
    assert!(!settings.control_rect(ControlId::SettingTheme).is_empty());
}

#[test]
fn compact_search_consumes_printable_global_shortcuts() {
    let mut harness = TuiHarness::from_fixture("normal", 60, 24, Theme::Dark);
    harness.key(KeyCode::Char('/'));
    harness.key(KeyCode::Char('2'));
    harness.key(KeyCode::Char('U'));
    harness.key(KeyCode::Char('V'));
    harness.key(KeyCode::Char('W'));

    let state = harness.state();
    assert_eq!(state.focus, Focus::TaskSearch);
    assert_eq!(state.task_search, "2UVW");
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
    harness.assert_shortcut_distinct(ControlId::ViewTrends);
    harness.assert_shortcut_distinct(ControlId::ViewSummary);
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
    scenarios.push(("tree-collapsed-80x24", tree));

    let mut search = TuiHarness::from_fixture("normal", 80, 24, Theme::Light);
    search.key(KeyCode::Char('/'));
    search.key(KeyCode::Char('布'));
    search.key(KeyCode::Char('局'));
    scenarios.push(("task-search-unicode-80x24", search));

    let mut other = TuiHarness::from_fixture("normal", 80, 24, Theme::Dark);
    other.key(KeyCode::Char('3'));
    scenarios.push(("other-normal-80x24", other));

    scenarios.push((
        "overview-empty-80x24",
        TuiHarness::from_fixture("empty", 80, 24, Theme::Dark),
    ));

    let mut partial = TuiHarness::from_fixture("partial", 80, 24, Theme::Light);
    partial.key(KeyCode::Char('3'));
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
