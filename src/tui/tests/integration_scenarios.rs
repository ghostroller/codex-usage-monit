use std::fs;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use crossterm::event::KeyCode;

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
    LocalProjectUsageGroup {
        thread_id: thread_id.to_string(),
        parent_thread_id: parent_thread_id.map(str::to_string),
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
            vec![summary_group(
                "alpha-root",
                None,
                "alpha-id",
                "alpha-service",
                "API billing dashboard",
                "desktop",
                120_000,
            )],
        ),
        summary_bucket(
            summary_timestamp("2026-07-10T08:15:00Z"),
            vec![summary_group(
                "alpha-child",
                Some("alpha-root"),
                "alpha-id",
                "alpha-service",
                "Price catalog research",
                "subagent",
                70_000,
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
    assert!(frame.contains("days M"), "{frame}");
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
        assert!(frame.contains("days MMMMCPMM"), "width={width}: {frame}");
    }
}

#[test]
fn summary_backfill_status_does_not_move_control_hitboxes() {
    for (width, expected) in [(120, "BACKFILLING 30d HISTORY"), (60, "BACKFILL 30d")] {
        let mut harness = summary_harness(width, 24, Theme::Dark);
        let controls = [
            ControlId::SummaryRangeCycle,
            ControlId::SummaryRangeSevenDays,
            ControlId::SummaryRangeThirtyDays,
            ControlId::SummaryMetricTokens,
            ControlId::SummaryMetricEstimated,
            ControlId::SummaryMetricApiEquivalent,
            ControlId::SummaryLongContext,
            ControlId::SummaryToggle,
        ];
        let before = controls.map(|control| harness.control_rect(control));
        harness.app.summary_backfill_running = true;
        harness.render();
        let after = controls.map(|control| harness.control_rect(control));

        assert_eq!(before, after);
        assert!(harness.frame().snapshot_text().contains(expected));
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
        (KeyCode::Char('L'), ControlId::SummaryLongContext),
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
fn summary_controls_clip_only_whole_buttons_in_tiny_terminals() {
    for (width, height) in [(40, 12), (20, 8), (8, 3)] {
        let harness = summary_harness(width, height, Theme::Dark);
        let controls = harness.app.summary_controls_hitbox.unwrap();
        for control in controls
            .ranges
            .into_iter()
            .chain(controls.metrics)
            .chain([controls.toggle_long_context, controls.toggle_selected])
        {
            if !control.is_empty() {
                assert_eq!(control.width, 3, "width={width}");
                assert!(control.right() <= width, "width={width}");
                assert!(control.bottom() <= height, "height={height}");
            }
        }
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

    let frame = harness.frame().snapshot_text();
    assert!(frame.contains("Build the integration test harnes"));
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
fn summary_tree_defaults_collapsed_and_enter_expands_project_then_task() {
    let mut harness = summary_harness(120, 40, Theme::Dark);
    let rows = harness.app.summary_rows();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.id.starts_with("project:")));
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
        "a collapsed task row should include its descendants"
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
    let task_expanded = harness.app.summary_rows();
    assert_eq!(task_expanded.len(), 5);
    assert_eq!(task_expanded[0].metrics.token_usage.total_tokens, 190_000);
    assert_eq!(
        task_expanded[1].metrics.token_usage.total_tokens, 120_000,
        "an expanded task row should show only its own usage"
    );
    assert_eq!(task_expanded[2].metrics.token_usage.total_tokens, 70_000);
    assert!(
        harness
            .frame()
            .snapshot_text()
            .contains("Price catalog research")
    );
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
