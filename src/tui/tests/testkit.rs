use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ControlId {
    ViewOverview,
    ViewTrends,
    ViewSummary,
    ViewOther,
    ViewSettings,
    ToggleTurns,
    ToggleModels,
    ScopeFiveHours,
    ScopeWeek,
    HistorySource,
    SummaryRangeCycle,
    SummaryRangeSevenDays,
    SummaryRangeThirtyDays,
    SummaryMetricTokens,
    SummaryMetricEstimated,
    SummaryMetricApiEquivalent,
    SummaryBucketGrain,
    SummaryAllProjects,
    SummaryLongContext,
    SummaryInspect,
    SummaryToggle,
    SummaryCollapseAll,
    SourceAll,
    SourceDesktop,
    SourceSubagent,
    SourceCli,
    TaskSearch,
    TaskSearchClear,
    EnterTurns,
    OpenTerminal,
    ToggleTree,
    CollapseAll,
    BackTasks,
    TurnSearch,
    TurnSearchClear,
    SettingTheme,
    SettingTurns,
    SettingModels,
    SettingLongContext,
    SettingTokens,
    SettingTokenShare,
    SettingEstimatedQuota,
    SettingApiEquivalent,
    QuitConfirm,
    QuitCancel,
    ResumeConfirm,
    ResumeCopy,
    ResumeCancel,
}

impl ControlId {
    pub(super) const fn binding(self) -> &'static str {
        match self {
            Self::ViewOverview => "1",
            Self::ViewTrends => "2",
            Self::ViewSummary => "U",
            Self::ViewOther => "3",
            Self::ViewSettings => "4",
            Self::ToggleTurns => "V",
            Self::ToggleModels => "M",
            Self::ScopeFiveHours => "5",
            Self::ScopeWeek => "W",
            Self::HistorySource => "S",
            Self::SummaryRangeCycle => "C",
            Self::SummaryRangeSevenDays => "7",
            Self::SummaryRangeThirtyDays => "M",
            Self::SummaryMetricTokens => "K",
            Self::SummaryMetricEstimated => "E",
            Self::SummaryMetricApiEquivalent => "A",
            Self::SummaryBucketGrain => "B",
            Self::SummaryAllProjects => "G",
            Self::SummaryLongContext => "L",
            Self::SummaryInspect => "I",
            Self::SummaryToggle => "↵",
            Self::SummaryCollapseAll => "X",
            Self::SourceAll => "A",
            Self::SourceDesktop => "D",
            Self::SourceSubagent => "S",
            Self::SourceCli => "C",
            Self::TaskSearch | Self::TurnSearch => "F",
            Self::TaskSearchClear | Self::TurnSearchClear => "Del",
            Self::EnterTurns | Self::ResumeConfirm | Self::QuitConfirm => "↵",
            Self::OpenTerminal => "O",
            Self::ToggleTree => "R",
            Self::CollapseAll => "E",
            Self::BackTasks => "←",
            Self::QuitCancel | Self::ResumeCancel => "Esc",
            Self::ResumeCopy => "C",
            Self::SettingTheme => "T",
            Self::SettingTurns => "V",
            Self::SettingModels => "M",
            Self::SettingLongContext => "L",
            Self::SettingTokens => "K",
            Self::SettingTokenShare => "P",
            Self::SettingEstimatedQuota => "E",
            Self::SettingApiEquivalent => "A",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HarnessState {
    pub(super) ui_state: UiState,
    pub(super) focus: Focus,
    pub(super) selected_thread_id: Option<String>,
    pub(super) selected_turn_id: Option<String>,
    pub(super) task_search: String,
    pub(super) turn_search: String,
    pub(super) task_offset: usize,
    pub(super) turn_offset: usize,
    pub(super) summary_range: SummaryRange,
    pub(super) summary_grain: SummaryGrain,
    pub(super) summary_metric: SummaryMetric,
    pub(super) summary_show_all_projects: bool,
    pub(super) summary_inspected_date: Option<NaiveDateTime>,
    pub(super) summary_selected_id: Option<String>,
    pub(super) summary_offset: usize,
    pub(super) quit_confirmation: bool,
    pub(super) resume_confirmation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StyleRun {
    row: u16,
    column: u16,
    width: u16,
    text: String,
    foreground: Color,
    background: Color,
    modifiers: Modifier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ControlFrame {
    id: ControlId,
    binding: &'static str,
    area: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SemanticFrame {
    width: u16,
    height: u16,
    rows: Vec<String>,
    style_runs: Vec<StyleRun>,
    controls: Vec<ControlFrame>,
}

impl SemanticFrame {
    pub(super) fn snapshot_text(&self) -> String {
        let mut output = String::new();
        writeln!(&mut output, "size={}x{}", self.width, self.height).unwrap();
        writeln!(&mut output, "rows:").unwrap();
        for (row, text) in self.rows.iter().enumerate() {
            writeln!(&mut output, "{row:02}|{}", text.trim_end()).unwrap();
        }
        writeln!(&mut output, "styles:").unwrap();
        let base_background = self
            .style_runs
            .first()
            .map(|run| run.background)
            .unwrap_or(Color::Reset);
        for run in &self.style_runs {
            if run.text.trim().is_empty()
                && run.background == base_background
                && run.modifiers.is_empty()
            {
                continue;
            }
            let text = serde_json::to_string(&run.text).unwrap();
            writeln!(
                &mut output,
                "{:02}:{:03}+{:03} fg={} bg={} mods={} text={}",
                run.row,
                run.column,
                run.width,
                color_label(run.foreground),
                color_label(run.background),
                modifier_label(run.modifiers),
                text
            )
            .unwrap();
        }
        writeln!(&mut output, "controls:").unwrap();
        for control in &self.controls {
            writeln!(
                &mut output,
                "{:?} binding={} rect={},{},{},{}",
                control.id,
                control.binding,
                control.area.x,
                control.area.y,
                control.area.width,
                control.area.height
            )
            .unwrap();
        }
        output
    }

    pub(super) fn to_svg(&self, title: &str) -> String {
        const CELL_WIDTH: u16 = 9;
        const CELL_HEIGHT: u16 = 18;
        const BASELINE: u16 = 14;
        const PADDING: u16 = 12;

        let pixel_width = self
            .width
            .saturating_mul(CELL_WIDTH)
            .saturating_add(PADDING * 2);
        let pixel_height = self
            .height
            .saturating_mul(CELL_HEIGHT)
            .saturating_add(PADDING * 2);
        let mut svg = String::new();
        writeln!(
            &mut svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{pixel_width}\" height=\"{pixel_height}\" viewBox=\"0 0 {pixel_width} {pixel_height}\">"
        )
        .unwrap();
        writeln!(&mut svg, "<title>{}</title>", xml_escape(title)).unwrap();
        writeln!(
            &mut svg,
            "<rect width=\"100%\" height=\"100%\" fill=\"#101216\" rx=\"8\"/>"
        )
        .unwrap();

        for run in &self.style_runs {
            let x = PADDING.saturating_add(run.column.saturating_mul(CELL_WIDTH));
            let y = PADDING.saturating_add(run.row.saturating_mul(CELL_HEIGHT));
            let width = run.width.saturating_mul(CELL_WIDTH);
            writeln!(
                &mut svg,
                "<rect x=\"{x}\" y=\"{y}\" width=\"{width}\" height=\"{CELL_HEIGHT}\" fill=\"{}\"/>",
                color_hex(run.background, "#101216")
            )
            .unwrap();
        }

        let buffer_rows = &self.rows;
        for (row, text) in buffer_rows.iter().enumerate() {
            let mut column = 0usize;
            for symbol in text.chars() {
                let width = UnicodeWidthChar::width(symbol).unwrap_or(0).max(1);
                if !symbol.is_whitespace() {
                    let run = self.style_at(row as u16, column as u16);
                    let x = usize::from(PADDING) + column * usize::from(CELL_WIDTH);
                    let y = usize::from(PADDING)
                        + row * usize::from(CELL_HEIGHT)
                        + usize::from(BASELINE);
                    let weight = if run.is_some_and(|run| run.modifiers.contains(Modifier::BOLD)) {
                        "700"
                    } else {
                        "400"
                    };
                    let decoration =
                        if run.is_some_and(|run| run.modifiers.contains(Modifier::UNDERLINED)) {
                            " text-decoration=\"underline\""
                        } else {
                            ""
                        };
                    let foreground = run
                        .map(|run| color_hex(run.foreground, "#dadde3"))
                        .unwrap_or_else(|| "#dadde3".to_string());
                    writeln!(
                        &mut svg,
                        "<text x=\"{x}\" y=\"{y}\" fill=\"{foreground}\" font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\" font-size=\"14\" font-weight=\"{weight}\"{decoration}>{}</text>",
                        xml_escape(&symbol.to_string())
                    )
                    .unwrap();
                }
                column = column.saturating_add(width);
            }
        }
        svg.push_str("</svg>\n");
        svg
    }

    fn style_at(&self, row: u16, column: u16) -> Option<&StyleRun> {
        self.style_runs.iter().find(|run| {
            run.row == row && column >= run.column && column < run.column.saturating_add(run.width)
        })
    }
}

pub(super) struct TuiHarness {
    pub(super) app: App,
    terminal: Terminal<TestBackend>,
    _mapping_directory: tempfile::TempDir,
}

impl TuiHarness {
    pub(super) fn from_fixture(fixture: &str, width: u16, height: u16, theme: Theme) -> Self {
        let contents = match fixture {
            "normal" => include_str!("../../../tests/fixtures/snapshots/normal.json"),
            "empty" => include_str!("../../../tests/fixtures/snapshots/empty.json"),
            "partial" => include_str!("../../../tests/fixtures/snapshots/partial.json"),
            _ => panic!("unknown TUI fixture: {fixture}"),
        };
        let snapshot =
            serde_json::from_str::<Snapshot>(contents).expect("TUI fixture must deserialize");
        Self::from_snapshot(snapshot, width, height, theme)
    }

    pub(super) fn from_snapshot(snapshot: Snapshot, width: u16, height: u16, theme: Theme) -> Self {
        let mut app = App::new(
            CollectionResult {
                snapshot,
                account: AccountSnapshot::default(),
                history_observation: crate::history::HistoryObservation::default(),
                local_session_digests: Default::default(),
            },
            theme,
        );
        let mapping_directory =
            tempfile::tempdir().expect("test mapping directory must initialize");
        app.project_mapping_store = ProjectMappingStore::new(
            mapping_directory
                .path()
                .join("config/project-mappings.json"),
        );
        let terminal =
            Terminal::new(TestBackend::new(width, height)).expect("test terminal must initialize");
        let mut harness = Self {
            app,
            terminal,
            _mapping_directory: mapping_directory,
        };
        harness.render();
        harness
    }

    pub(super) fn render(&mut self) {
        self.render_at(self.app.snapshot.as_of);
    }

    pub(super) fn render_at(&mut self, now: DateTime<Utc>) {
        let app = &mut self.app;
        with_test_display_offset(FixedOffset::east_opt(0).unwrap(), || {
            self.terminal
                .draw(|frame| super::super::render_at(frame, app, now))
                .expect("test frame must render");
        });
    }

    pub(super) fn key(&mut self, code: KeyCode) -> bool {
        let should_quit = handle_key_event(&mut self.app, KeyEvent::new(code, KeyModifiers::NONE));
        self.render();
        should_quit
    }

    pub(super) fn click(&mut self, control: ControlId, edge: ClickEdge) -> bool {
        let area = self.control_rect(control);
        assert!(!area.is_empty(), "{control:?} is not visible");
        let column = match edge {
            ClickEdge::Start => area.x,
            ClickEdge::Middle => area.x + area.width.saturating_sub(1) / 2,
            ClickEdge::End => area.right().saturating_sub(1),
        };
        let handled = handle_mouse_event(
            &mut self.app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        self.render();
        handled
    }

    pub(super) fn resize(&mut self, width: u16, height: u16) {
        self.terminal.backend_mut().resize(width, height);
        self.terminal
            .resize(Rect::new(0, 0, width, height))
            .expect("test terminal must resize");
        self.render();
    }

    pub(super) fn state(&self) -> HarnessState {
        HarnessState {
            ui_state: self.app.ui_state(),
            focus: self.app.focus,
            selected_thread_id: self
                .app
                .selected_task_record()
                .map(|task| task.thread_id.clone()),
            selected_turn_id: self
                .app
                .selected_turn_record()
                .map(|turn| turn.turn_id.clone()),
            task_search: self.app.task_search.clone(),
            turn_search: self.app.turn_search.clone(),
            task_offset: self.app.task_table_offset,
            turn_offset: self.app.turn_offset,
            summary_range: self.app.summary_range,
            summary_grain: self.app.summary_grain,
            summary_metric: self.app.summary_metric,
            summary_show_all_projects: self.app.summary_show_all_projects,
            summary_inspected_date: self.app.summary_inspected_date,
            summary_selected_id: self.app.summary_selected_id.clone(),
            summary_offset: self.app.summary_offset,
            quit_confirmation: self.app.quit_confirmation_visible,
            resume_confirmation: self.app.resume_confirmation.is_some(),
        }
    }

    pub(super) fn frame(&self) -> SemanticFrame {
        SemanticFrame::from_buffer(self.terminal.backend().buffer(), self.visible_controls())
    }

    pub(super) fn cell_style(&self, column: u16, row: u16) -> (String, Color, Modifier) {
        let cell = &self.terminal.backend().buffer()[(column, row)];
        (cell.symbol().to_string(), cell.fg, cell.modifier)
    }

    pub(super) fn control_rect(&self, control: ControlId) -> Rect {
        match control {
            ControlId::ViewOverview => self
                .app
                .view_tabs_hitbox
                .map(|hitbox| hitbox.tabs[View::Overview.index()])
                .unwrap_or_default(),
            ControlId::ViewTrends => self
                .app
                .view_tabs_hitbox
                .map(|hitbox| hitbox.tabs[View::Trends.index()])
                .unwrap_or_default(),
            ControlId::ViewSummary => self
                .app
                .view_tabs_hitbox
                .map(|hitbox| hitbox.tabs[View::Summary.index()])
                .unwrap_or_default(),
            ControlId::ViewOther => self
                .app
                .view_tabs_hitbox
                .map(|hitbox| hitbox.tabs[View::Health.index()])
                .unwrap_or_default(),
            ControlId::ViewSettings => self
                .app
                .view_tabs_hitbox
                .map(|hitbox| hitbox.tabs[View::Settings.index()])
                .unwrap_or_default(),
            ControlId::ToggleTurns => self
                .app
                .window_controls_hitbox
                .map(|hitbox| hitbox.toggle_turns)
                .unwrap_or_default(),
            ControlId::ToggleModels => self
                .app
                .window_controls_hitbox
                .map(|hitbox| hitbox.toggle_models)
                .unwrap_or_default(),
            ControlId::ScopeFiveHours => self
                .app
                .window_controls_hitbox
                .map(|hitbox| hitbox.scopes[WindowScope::FiveHours.index()])
                .unwrap_or_default(),
            ControlId::ScopeWeek => self
                .app
                .window_controls_hitbox
                .map(|hitbox| hitbox.scopes[WindowScope::Week.index()])
                .unwrap_or_default(),
            ControlId::HistorySource => self.app.history_source_control_hitbox,
            ControlId::SummaryRangeCycle => self.summary_range_rect(SummaryRange::Cycle),
            ControlId::SummaryRangeSevenDays => self.summary_range_rect(SummaryRange::SevenDays),
            ControlId::SummaryRangeThirtyDays => self.summary_range_rect(SummaryRange::ThirtyDays),
            ControlId::SummaryMetricTokens => self.summary_metric_rect(SummaryMetric::Tokens),
            ControlId::SummaryMetricEstimated => self.summary_metric_rect(SummaryMetric::Estimated),
            ControlId::SummaryMetricApiEquivalent => {
                self.summary_metric_rect(SummaryMetric::ApiEquivalent)
            }
            ControlId::SummaryBucketGrain => self
                .app
                .summary_controls_hitbox
                .map(|hitbox| hitbox.bucket_grain)
                .unwrap_or_default(),
            ControlId::SummaryAllProjects => self
                .app
                .summary_controls_hitbox
                .map(|hitbox| hitbox.toggle_all_projects)
                .unwrap_or_default(),
            ControlId::SummaryLongContext => self
                .app
                .summary_controls_hitbox
                .map(|hitbox| hitbox.toggle_long_context)
                .unwrap_or_default(),
            ControlId::SummaryInspect => self
                .app
                .summary_controls_hitbox
                .map(|hitbox| hitbox.inspect)
                .unwrap_or_default(),
            ControlId::SummaryToggle => self
                .app
                .summary_controls_hitbox
                .map(|hitbox| hitbox.toggle_selected)
                .unwrap_or_default(),
            ControlId::SummaryCollapseAll => self
                .app
                .summary_controls_hitbox
                .map(|hitbox| hitbox.collapse_all)
                .unwrap_or_default(),
            ControlId::SourceAll => self.task_source_rect(0),
            ControlId::SourceDesktop => self.task_source_rect(1),
            ControlId::SourceSubagent => self.task_source_rect(2),
            ControlId::SourceCli => self.task_source_rect(3),
            ControlId::TaskSearch => self
                .app
                .task_controls_hitbox
                .map(|hitbox| hitbox.search)
                .unwrap_or_default(),
            ControlId::TaskSearchClear => self
                .app
                .task_controls_hitbox
                .map(|hitbox| hitbox.clear_search)
                .unwrap_or_default(),
            ControlId::EnterTurns => self
                .app
                .task_controls_hitbox
                .map(|hitbox| hitbox.enter_turns)
                .unwrap_or_default(),
            ControlId::OpenTerminal => self
                .app
                .task_controls_hitbox
                .map(|hitbox| hitbox.open_terminal)
                .unwrap_or_default(),
            ControlId::ToggleTree => self
                .app
                .task_controls_hitbox
                .map(|hitbox| hitbox.toggle_tree)
                .unwrap_or_default(),
            ControlId::CollapseAll => self
                .app
                .task_controls_hitbox
                .map(|hitbox| hitbox.collapse_all)
                .unwrap_or_default(),
            ControlId::BackTasks => self
                .app
                .turn_controls_hitbox
                .map(|hitbox| hitbox.back_tasks)
                .unwrap_or_default(),
            ControlId::TurnSearch => self
                .app
                .turn_controls_hitbox
                .map(|hitbox| hitbox.search)
                .unwrap_or_default(),
            ControlId::TurnSearchClear => self
                .app
                .turn_controls_hitbox
                .map(|hitbox| hitbox.clear_search)
                .unwrap_or_default(),
            ControlId::SettingTheme => self.setting_rect(SettingItem::Theme),
            ControlId::SettingTurns => self.setting_rect(SettingItem::Turns),
            ControlId::SettingModels => self.setting_rect(SettingItem::Models),
            ControlId::SettingLongContext => self.setting_rect(SettingItem::ApiLongContext),
            ControlId::SettingTokens => self.setting_rect(SettingItem::Tokens),
            ControlId::SettingTokenShare => self.setting_rect(SettingItem::TokenShare),
            ControlId::SettingEstimatedQuota => self.setting_rect(SettingItem::EstimatedQuota),
            ControlId::SettingApiEquivalent => self.setting_rect(SettingItem::ApiEquivalent),
            ControlId::QuitConfirm => self
                .app
                .quit_confirmation_hitbox
                .map(|hitbox| hitbox.confirm)
                .unwrap_or_default(),
            ControlId::QuitCancel => self
                .app
                .quit_confirmation_hitbox
                .map(|hitbox| hitbox.cancel)
                .unwrap_or_default(),
            ControlId::ResumeConfirm => self
                .app
                .resume_confirmation_hitbox
                .map(|hitbox| hitbox.confirm)
                .unwrap_or_default(),
            ControlId::ResumeCopy => self
                .app
                .resume_confirmation_hitbox
                .map(|hitbox| hitbox.copy)
                .unwrap_or_default(),
            ControlId::ResumeCancel => self
                .app
                .resume_confirmation_hitbox
                .map(|hitbox| hitbox.cancel)
                .unwrap_or_default(),
        }
    }

    pub(super) fn assert_shortcut_distinct(&self, control: ControlId) {
        let area = self.control_rect(control);
        assert!(!area.is_empty(), "{control:?} is not visible");
        let binding = control.binding();
        let palette = self.app.theme.palette();
        let cell = (area.x..area.right())
            .map(|column| &self.terminal.backend().buffer()[(column, area.y)])
            .find(|cell| cell.symbol() == binding)
            .unwrap_or_else(|| panic!("{control:?} does not render binding {binding}"));
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "{control:?} shortcut weight"
        );
        assert!(
            cell.fg == palette.accent
                || (cell.bg == palette.accent && cell.modifier.contains(Modifier::UNDERLINED)),
            "{control:?} shortcut must use the accent foreground or an underlined inverse accent"
        );
    }

    pub(super) fn assert_shortcut_inactive(&self, control: ControlId) {
        let area = self.control_rect(control);
        assert!(!area.is_empty(), "{control:?} is not visible");
        let binding = control.binding();
        let palette = self.app.theme.palette();
        let cell = (area.x..area.right())
            .map(|column| &self.terminal.backend().buffer()[(column, area.y)])
            .find(|cell| cell.symbol() == binding)
            .unwrap_or_else(|| panic!("{control:?} does not render binding {binding}"));
        assert_ne!(cell.fg, palette.accent, "{control:?} shortcut color");
        assert!(
            !cell.modifier.contains(Modifier::UNDERLINED),
            "{control:?} inactive shortcut must not be underlined"
        );
    }

    fn task_source_rect(&self, index: usize) -> Rect {
        self.app
            .task_controls_hitbox
            .map(|hitbox| hitbox.sources[index])
            .unwrap_or_default()
    }

    fn summary_range_rect(&self, range: SummaryRange) -> Rect {
        self.app
            .summary_controls_hitbox
            .map(|hitbox| hitbox.ranges[range.index()])
            .unwrap_or_default()
    }

    fn summary_metric_rect(&self, metric: SummaryMetric) -> Rect {
        self.app
            .summary_controls_hitbox
            .map(|hitbox| hitbox.metrics[metric.index()])
            .unwrap_or_default()
    }

    fn setting_rect(&self, item: SettingItem) -> Rect {
        self.app
            .settings_controls_hitbox
            .as_ref()
            .map(|hitbox| hitbox.rows[item.index()])
            .unwrap_or_default()
    }

    fn visible_controls(&self) -> Vec<ControlFrame> {
        let mut controls = [
            ControlId::ViewOverview,
            ControlId::ViewTrends,
            ControlId::ViewSummary,
            ControlId::ViewOther,
            ControlId::ViewSettings,
            ControlId::ToggleTurns,
            ControlId::ToggleModels,
            ControlId::ScopeFiveHours,
            ControlId::ScopeWeek,
            ControlId::HistorySource,
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
            ControlId::SourceAll,
            ControlId::SourceDesktop,
            ControlId::SourceSubagent,
            ControlId::SourceCli,
            ControlId::TaskSearch,
            ControlId::TaskSearchClear,
            ControlId::EnterTurns,
            ControlId::OpenTerminal,
            ControlId::ToggleTree,
            ControlId::CollapseAll,
            ControlId::BackTasks,
            ControlId::TurnSearch,
            ControlId::TurnSearchClear,
            ControlId::SettingTheme,
            ControlId::SettingTurns,
            ControlId::SettingModels,
            ControlId::SettingLongContext,
            ControlId::SettingTokens,
            ControlId::SettingTokenShare,
            ControlId::SettingEstimatedQuota,
            ControlId::SettingApiEquivalent,
            ControlId::QuitConfirm,
            ControlId::QuitCancel,
            ControlId::ResumeConfirm,
            ControlId::ResumeCopy,
            ControlId::ResumeCancel,
        ]
        .into_iter()
        .filter_map(|id| {
            let area = self.control_rect(id);
            (!area.is_empty()).then_some(ControlFrame {
                id,
                binding: id.binding(),
                area,
            })
        })
        .collect::<Vec<_>>();
        controls.sort_by_key(|control| control.id);
        controls
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ClickEdge {
    Start,
    Middle,
    End,
}

impl SemanticFrame {
    fn from_buffer(buffer: &Buffer, controls: Vec<ControlFrame>) -> Self {
        let width = buffer.area.width;
        let height = buffer.area.height;
        let rows = (0..height)
            .map(|row| display_row(buffer, row))
            .collect::<Vec<_>>();
        let mut style_runs = Vec::new();
        for row in 0..height {
            let mut start = 0;
            while start < width {
                let first = &buffer[(start, row)];
                let mut end = start + 1;
                while end < width {
                    let cell = &buffer[(end, row)];
                    if cell.fg != first.fg || cell.bg != first.bg || cell.modifier != first.modifier
                    {
                        break;
                    }
                    end += 1;
                }
                let text = (start..end)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>();
                style_runs.push(StyleRun {
                    row,
                    column: start,
                    width: end - start,
                    text,
                    foreground: first.fg,
                    background: first.bg,
                    modifiers: first.modifier,
                });
                start = end;
            }
        }
        Self {
            width,
            height,
            rows,
            style_runs,
            controls,
        }
    }
}

pub(super) fn gallery_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("tui-gallery")
}

fn display_row(buffer: &Buffer, row: u16) -> String {
    let mut output = String::new();
    let mut hidden = 0usize;
    for column in 0..buffer.area.width {
        let symbol = buffer[(column, row)].symbol();
        if hidden == 0 {
            output.push_str(symbol);
        }
        hidden = hidden.max(UnicodeWidthStr::width(symbol)).saturating_sub(1);
    }
    output
}

fn color_label(color: Color) -> String {
    match color {
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        Color::Indexed(index) => format!("idx({index})"),
        value => format!("{value:?}").to_ascii_lowercase(),
    }
}

fn modifier_label(modifier: Modifier) -> String {
    let mut labels = Vec::new();
    for (flag, label) in [
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINED, "underline"),
        (Modifier::SLOW_BLINK, "slow-blink"),
        (Modifier::RAPID_BLINK, "rapid-blink"),
        (Modifier::REVERSED, "reversed"),
        (Modifier::HIDDEN, "hidden"),
        (Modifier::CROSSED_OUT, "crossed-out"),
    ] {
        if modifier.contains(flag) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        "none".to_string()
    } else {
        labels.join("+")
    }
}

fn color_hex(color: Color, reset: &str) -> String {
    match color {
        Color::Reset => reset.to_string(),
        Color::Black => "#000000".to_string(),
        Color::Red => "#aa0000".to_string(),
        Color::Green => "#00aa00".to_string(),
        Color::Yellow => "#aa5500".to_string(),
        Color::Blue => "#0000aa".to_string(),
        Color::Magenta => "#aa00aa".to_string(),
        Color::Cyan => "#00aaaa".to_string(),
        Color::Gray => "#aaaaaa".to_string(),
        Color::DarkGray => "#555555".to_string(),
        Color::LightRed => "#ff5555".to_string(),
        Color::LightGreen => "#55ff55".to_string(),
        Color::LightYellow => "#ffff55".to_string(),
        Color::LightBlue => "#5555ff".to_string(),
        Color::LightMagenta => "#ff55ff".to_string(),
        Color::LightCyan => "#55ffff".to_string(),
        Color::White => "#ffffff".to_string(),
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        Color::Indexed(_) => "#888888".to_string(),
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
