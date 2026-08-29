use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
#[cfg(test)]
use chrono::FixedOffset;
use chrono::{
    DateTime, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, Timelike, Utc,
};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
#[cfg(windows)]
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Clear, Dataset, Gauge, GraphType, HighlightSpacing,
    Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod geometry;
mod stacked_area;
mod text;

use geometry::{reveal_offset, scale_rounded, scroll_offset, scrollbar_geometry};
use stacked_area::{StackedArea, StackedAreaSeries, StackedAreaState, date_index_at_column};
use text::{
    byte_index_at_char, compact_search_text, search_cursor_window, short_thread_id,
    truncate_display_text, truncate_middle_display_text,
};

use crate::api_cost::{API_PRICING_CATALOG_REVISION, format_api_cost_amount, format_pico_usd};
use crate::attribution::ESTIMATED_COST_UNITS_PER_CREDIT;
use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, ApiCostAmount, AttributionSummary, Confidence, ModelUsage, Provenance,
    Snapshot, TaskRecord, TaskStatus, TokenUsage, TurnRecord, TurnStatus, WindowAnalysis,
    WindowUsage, terminal_safe_text,
};
use crate::history::{
    HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, HistoryData,
    HistoryObservation, HistoryStore, LOCAL_BUCKET_MINUTES,
};
use crate::open_config::{OpenConfig, OpenConfigStore};
use crate::perf::{HistoryMetrics, PerfLog};
use crate::rollout::RolloutCache;
use crate::service::{RecorderStatusFile, default_status_file, read_recorder_status};
use crate::session_launch::{
    FocusResult, LaunchContext, LaunchResult, PaneId, ResumeTarget, ZellijOptions,
    check_eligibility, check_eligibility_without_cwd_probe, execute_zellij_launch,
    focus_existing_pane, prepare_resume_copy_command, prepare_zellij_focus, prepare_zellij_launch,
    render_resume_command,
};
use crate::snapshot::{
    CollectionResult, collect_snapshot_cached, collect_snapshot_cached_if_changed,
};
use crate::summary::{
    ProjectSummary, SessionSummary, SummaryMetrics, SummarySample, SummaryTurnKey, SummaryWindow,
    TurnSummary, UsageSummary, summarize_samples_with_local_time,
};
use crate::ui_state::{
    UiState, UiStateStore, UiTableColumns, UiTaskListMode, UiTaskSourceFilter, UiTheme, UiView,
    UiWindowScope,
};

const LOCAL_REFRESH: Duration = Duration::from_secs(2);
const ACCOUNT_REFRESH: Duration = Duration::from_secs(45);
const ACCOUNT_REFRESH_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_secs(5), Duration::from_secs(10)];
const HISTORY_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const HISTORY_VIEW_DAYS: i64 = 8;
const SUMMARY_HISTORY_DAYS: i64 = 31;
const SUMMARY_BACKFILL_MAX_FILES: usize = 5_000;
const SUMMARY_BACKFILL_RETRY_DAYS: i64 = 7;
const BACKGROUND_CHANNEL_POLL: Duration = Duration::from_millis(100);
const MOUSE_SCROLL_LINES: usize = 3;
const PAGE_SCROLL_LINES: usize = 5;
const OPEN_NOTICE_DURATION: Duration = Duration::from_secs(8);
const TAB_PADDING: &str = " ";
const TAB_DIVIDER: &str = " | ";
const ENTER_FOCUS_HINT: &str = "↵";
const BACK_FOCUS_HINT: &str = "←";
const CLEAR_FILTER_LABEL: &str = "[Del]";
const FILTER_CLEAR_GAP_WIDTH: u16 = 1;
const FILTER_MIN_QUERY_WIDTH: u16 = 1;
const RESUME_CONFIRM_MIN_INNER_WIDTH: u16 = 44;
const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024;
const TASK_TOKENS_WIDTH: u16 = 10;
const TASK_TOKEN_SHARE_WIDTH: u16 = 8;
const TASK_QUOTA_WIDTH: u16 = 8;
const TASK_COLUMN_SPACING: u16 = 1;
const TASK_HIGHLIGHT_WIDTH: u16 = 1;
const TASK_TREE_MARKER_WIDTH: u16 = 3;
const TURN_MODEL_WIDTH: u16 = 16;
const TURN_COMPACT_MODEL_WIDTH: u16 = 17;
const TURN_EFFORT_WIDTH: u16 = 7;
const TURN_MESSAGE_WIDTH: u16 = 14;
const TURN_COMPACT_MESSAGE_WIDTH: u16 = 8;
const TURN_TOKENS_WIDTH: u16 = 9;
const TURN_TOKEN_SHARE_WIDTH: u16 = 7;
const TURN_QUOTA_WIDTH: u16 = 7;
const MODEL_TOKENS_WIDTH: u16 = 12;
const MODEL_TOKEN_SHARE_WIDTH: u16 = 12;
const MODEL_QUOTA_WIDTH: u16 = 12;
const SUMMARY_STACKED_PROJECT_LIMIT: usize = 6;
const SUMMARY_PROJECT_COLOR_CANDIDATES: usize = 24;
const SUMMARY_PROJECT_COLOR_MIN_DISTANCE_SQUARED: u32 = 5_000;
const MAX_DEBUG_STARTUP_CELLS: u32 = 500_000;

#[cfg(test)]
thread_local! {
    static TEST_DISPLAY_OFFSET: std::cell::Cell<Option<FixedOffset>> =
        const { std::cell::Cell::new(None) };
}

fn format_local_time(value: DateTime<Utc>, format: &str) -> String {
    #[cfg(test)]
    if let Some(offset) = TEST_DISPLAY_OFFSET.with(std::cell::Cell::get) {
        return value.with_timezone(&offset).format(format).to_string();
    }

    value.with_timezone(&Local).format(format).to_string()
}

fn display_local_date(value: DateTime<Utc>) -> chrono::NaiveDate {
    #[cfg(test)]
    if let Some(offset) = TEST_DISPLAY_OFFSET.with(std::cell::Cell::get) {
        return value.with_timezone(&offset).date_naive();
    }

    value.with_timezone(&Local).date_naive()
}

fn display_local_datetime(value: DateTime<Utc>) -> NaiveDateTime {
    #[cfg(test)]
    if let Some(offset) = TEST_DISPLAY_OFFSET.with(std::cell::Cell::get) {
        return value.with_timezone(&offset).naive_local();
    }

    value.with_timezone(&Local).naive_local()
}

fn display_local_hour(value: DateTime<Utc>) -> NaiveDateTime {
    let value = display_local_datetime(value);
    value
        .date()
        .and_hms_opt(value.hour(), 0, 0)
        .unwrap_or(value)
}

fn local_time_is_midnight(value: DateTime<Utc>) -> bool {
    value.timestamp_subsec_nanos() == 0 && format_local_time(value, "%H:%M:%S") == "00:00:00"
}

fn summary_day_is_partial_window_edge(window: SummaryWindow, date: NaiveDate) -> bool {
    let starts_on_date = display_local_date(window.starts_at) == date;
    let ends_on_date = window
        .ends_at
        .checked_sub_signed(ChronoDuration::nanoseconds(1))
        .is_some_and(|last| display_local_date(last) == date);
    (starts_on_date && !local_time_is_midnight(window.starts_at))
        || (ends_on_date && !local_time_is_midnight(window.ends_at))
}

fn summary_bucket_is_partial_window_edge(
    window: SummaryWindow,
    starts_at: NaiveDateTime,
    grain: SummaryGrain,
) -> bool {
    let local_start = display_local_datetime(window.starts_at);
    let local_end = display_local_datetime(window.ends_at);
    let local_last = window
        .ends_at
        .checked_sub_signed(ChronoDuration::nanoseconds(1))
        .map(display_local_datetime)
        .unwrap_or(local_end);
    let bucket_end = starts_at
        .checked_add_signed(ChronoDuration::hours(i64::from(grain.hours())))
        .unwrap_or(starts_at);
    (grain.bucket_start(local_start) == starts_at && local_start != starts_at)
        || (grain.bucket_start(local_last) == starts_at && local_end != bucket_end)
}

#[cfg(test)]
struct TestDisplayOffsetGuard(Option<FixedOffset>);

#[cfg(test)]
impl Drop for TestDisplayOffsetGuard {
    fn drop(&mut self) {
        TEST_DISPLAY_OFFSET.with(|current| current.set(self.0));
    }
}

#[cfg(test)]
fn with_test_display_offset<T>(offset: FixedOffset, render: impl FnOnce() -> T) -> T {
    let _guard =
        TestDisplayOffsetGuard(TEST_DISPLAY_OFFSET.with(|current| current.replace(Some(offset))));
    render()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug)]
struct Palette {
    background: Color,
    foreground: Color,
    muted: Color,
    accent: Color,
    border: Color,
    title: Color,
    gauge_track: Color,
    success: Color,
    warning: Color,
    error: Color,
}

impl Theme {
    fn toggle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    fn palette(self) -> Palette {
        match self {
            Self::Dark => Palette {
                background: Color::Rgb(18, 20, 23),
                foreground: Color::Rgb(218, 222, 228),
                muted: Color::Rgb(126, 134, 145),
                accent: Color::Rgb(63, 185, 192),
                border: Color::Rgb(75, 82, 92),
                title: Color::Rgb(244, 246, 248),
                gauge_track: Color::Rgb(32, 36, 42),
                success: Color::Rgb(74, 222, 128),
                warning: Color::Rgb(250, 204, 21),
                error: Color::Rgb(248, 113, 113),
            },
            Self::Light => Palette {
                background: Color::Rgb(247, 249, 252),
                foreground: Color::Rgb(23, 32, 42),
                muted: Color::Rgb(95, 107, 122),
                accent: Color::Rgb(0, 108, 117),
                border: Color::Rgb(125, 137, 152),
                title: Color::Rgb(23, 32, 42),
                gauge_track: Color::Rgb(230, 234, 240),
                success: Color::Rgb(22, 121, 74),
                warning: Color::Rgb(138, 89, 0),
                error: Color::Rgb(180, 35, 24),
            },
        }
    }

    fn base_style(self) -> Style {
        let palette = self.palette();
        Style::default()
            .fg(palette.foreground)
            .bg(palette.background)
    }
}

impl From<UiTheme> for Theme {
    fn from(value: UiTheme) -> Self {
        match value {
            UiTheme::Dark => Self::Dark,
            UiTheme::Light => Self::Light,
        }
    }
}

impl From<Theme> for UiTheme {
    fn from(value: Theme) -> Self {
        match value {
            Theme::Dark => Self::Dark,
            Theme::Light => Self::Light,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Overview,
    Trends,
    Summary,
    Health,
    Settings,
}

impl From<UiView> for View {
    fn from(value: UiView) -> Self {
        match value {
            UiView::Overview => Self::Overview,
            UiView::Trends => Self::Trends,
            UiView::Summary => Self::Summary,
            UiView::Health => Self::Health,
            UiView::Settings => Self::Settings,
        }
    }
}

impl From<View> for UiView {
    fn from(value: View) -> Self {
        match value {
            View::Overview => Self::Overview,
            View::Trends => Self::Trends,
            View::Summary => Self::Summary,
            View::Health => Self::Health,
            View::Settings => Self::Settings,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingItem {
    Theme,
    Turns,
    Models,
    ApiLongContext,
    Tokens,
    TokenShare,
    EstimatedQuota,
    ApiEquivalent,
}

impl SettingItem {
    const ALL: [Self; 8] = [
        Self::Theme,
        Self::Turns,
        Self::Models,
        Self::ApiLongContext,
        Self::Tokens,
        Self::TokenShare,
        Self::EstimatedQuota,
        Self::ApiEquivalent,
    ];

    fn index(self) -> usize {
        match self {
            Self::Theme => 0,
            Self::Turns => 1,
            Self::Models => 2,
            Self::ApiLongContext => 3,
            Self::Tokens => 4,
            Self::TokenShare => 5,
            Self::EstimatedQuota => 6,
            Self::ApiEquivalent => 7,
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Theme => 'T',
            Self::Turns => 'V',
            Self::Models => 'M',
            Self::ApiLongContext => 'L',
            Self::Tokens => 'K',
            Self::TokenShare => 'P',
            Self::EstimatedQuota => 'E',
            Self::ApiEquivalent => 'A',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::Turns => "Turns panel",
            Self::Models => "Models panel",
            Self::ApiLongContext => "EST Longx",
            Self::Tokens => "Tokens",
            Self::TokenShare => "Token share",
            Self::EstimatedQuota => "Estimated quota",
            Self::ApiEquivalent => "API equivalent",
        }
    }

    fn from_shortcut(value: char) -> Option<Self> {
        let shortcut = value.to_ascii_uppercase();
        Self::ALL
            .into_iter()
            .find(|item| item.shortcut() == shortcut)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TrendSection {
    #[default]
    Remaining,
    Weekly,
    HalfHour,
}

impl TrendSection {
    const ALL: [Self; 3] = [Self::Remaining, Self::Weekly, Self::HalfHour];

    fn label(self) -> &'static str {
        match self {
            Self::Remaining => "Remaining",
            Self::Weekly => "Weekly",
            Self::HalfHour => "15-minute",
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Remaining => 'R',
            Self::Weekly => 'W',
            Self::HalfHour => 'H',
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Remaining => 0,
            Self::Weekly => 1,
            Self::HalfHour => 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TrendPoint {
    at: DateTime<Utc>,
    value: f64,
    readout_value: TrendReadoutValue,
    sampled_at: Option<DateTime<Utc>>,
    interval: Option<TrendInterval>,
    partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrendInterval {
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TrendReadoutValue {
    Percent(f64),
    Tokens(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TrendReadout {
    sampled_at: DateTime<Utc>,
    value: TrendReadoutValue,
    interval: Option<TrendInterval>,
    partial: bool,
}

#[derive(Clone, Copy)]
struct TrendSeries<'a> {
    name: &'static str,
    points: &'a [TrendPoint],
    readout: Option<TrendReadout>,
    color: Color,
}

#[derive(Clone, Copy)]
enum TrendValueKind {
    Percent,
    Tokens,
}

#[derive(Clone, Copy, Debug)]
enum TrendGraphKind {
    Line { maximum_gap: chrono::Duration },
    Bar { expected_step: chrono::Duration },
}

impl TrendGraphKind {
    fn selection_tolerance(self) -> chrono::Duration {
        match self {
            Self::Line { maximum_gap } => maximum_gap,
            Self::Bar { expected_step } => expected_step,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrendPanelId {
    Remaining,
    WeeklyTokens,
    WeeklyEstimated,
    LocalTokens,
    LocalEstimated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrendInspection {
    panel: TrendPanelId,
    at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrendDrag {
    panel: TrendPanelId,
}

#[derive(Clone, Debug)]
struct TrendChartHitbox {
    panel: TrendPanelId,
    plot: Rect,
    legend: Option<Rect>,
    x_bounds: [f64; 2],
    graph_kind: TrendGraphKind,
    inspectable_times: Vec<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug)]
struct TrendChartGeometry {
    plot: Rect,
    legend: Option<Rect>,
}

impl TrendPoint {
    fn readout(self) -> Option<TrendReadout> {
        self.sampled_at.map(|sampled_at| TrendReadout {
            sampled_at,
            value: self.readout_value,
            interval: self.interval,
            partial: self.partial,
        })
    }
}

impl TrendChartHitbox {
    fn contains(&self, column: u16, row: u16) -> bool {
        rect_contains(self.plot, column, row)
            && !self
                .legend
                .is_some_and(|legend| rect_contains(legend, column, row))
    }

    fn has_inspectable_points(&self) -> bool {
        !self.inspectable_times.is_empty()
    }

    fn earliest_inspection(&self) -> Option<TrendInspection> {
        self.inspectable_times
            .first()
            .copied()
            .map(|at| TrendInspection {
                panel: self.panel,
                at,
            })
    }

    fn latest_inspection(&self) -> Option<TrendInspection> {
        self.inspectable_times
            .last()
            .copied()
            .map(|at| TrendInspection {
                panel: self.panel,
                at,
            })
    }

    fn nearest_inspection(&self, at: DateTime<Utc>) -> Option<TrendInspection> {
        let tolerance_ms = self
            .graph_kind
            .selection_tolerance()
            .num_milliseconds()
            .unsigned_abs();
        self.inspectable_times
            .iter()
            .copied()
            .map(|candidate| {
                let distance = (candidate - at).num_milliseconds().unsigned_abs();
                (distance, candidate)
            })
            .filter(|(distance, _)| *distance <= tolerance_ms)
            .min_by_key(|(distance, candidate)| (*distance, *candidate))
            .map(|(_, at)| TrendInspection {
                panel: self.panel,
                at,
            })
    }

    fn step_inspection(&self, current: DateTime<Utc>, forward: bool) -> Option<TrendInspection> {
        let at = if forward {
            self.inspectable_times
                .iter()
                .copied()
                .find(|candidate| *candidate > current)
                .or_else(|| self.inspectable_times.last().copied())
        } else {
            self.inspectable_times
                .iter()
                .rev()
                .copied()
                .find(|candidate| *candidate < current)
                .or_else(|| self.inspectable_times.first().copied())
        }?;
        Some(TrendInspection {
            panel: self.panel,
            at,
        })
    }

    fn inspection_at_column(&self, column: u16) -> Option<TrendInspection> {
        if self.plot.is_empty() {
            return None;
        }
        let column = column.clamp(self.plot.x, self.plot.right().saturating_sub(1));
        let fraction = if self.plot.width <= 1 {
            0.5
        } else {
            f64::from(column.saturating_sub(self.plot.x)) / f64::from(self.plot.width - 1)
        };
        let target = self.x_bounds[0] + fraction * (self.x_bounds[1] - self.x_bounds[0]);
        let target = DateTime::from_timestamp(target.round() as i64, 0)?;
        self.nearest_inspection(target)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WindowScope {
    #[default]
    FiveHours,
    Week,
}

impl From<UiWindowScope> for WindowScope {
    fn from(value: UiWindowScope) -> Self {
        match value {
            UiWindowScope::FiveHours => Self::FiveHours,
            UiWindowScope::Week => Self::Week,
        }
    }
}

impl From<WindowScope> for UiWindowScope {
    fn from(value: WindowScope) -> Self {
        match value {
            WindowScope::FiveHours => Self::FiveHours,
            WindowScope::Week => Self::Week,
        }
    }
}

impl WindowScope {
    const ALL: [Self; 2] = [Self::FiveHours, Self::Week];

    fn index(self) -> usize {
        match self {
            Self::FiveHours => 0,
            Self::Week => 1,
        }
    }

    fn duration_mins(self) -> i64 {
        match self {
            Self::FiveHours => 300,
            Self::Week => 10_080,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FiveHours => "5h",
            Self::Week => "Week",
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::FiveHours => '5',
            Self::Week => 'W',
        }
    }

    fn token_share_header(self) -> &'static str {
        match self {
            Self::FiveHours => "TOKEN5H%",
            Self::Week => "TOKENWK%",
        }
    }

    fn quota_header(self) -> &'static str {
        match self {
            Self::FiveHours => "EST.Q5H",
            Self::Week => "EST.QWK",
        }
    }

    fn task_title(self) -> &'static str {
        match self {
            Self::FiveHours => "5h tasks",
            Self::Week => "Week-cycle tasks",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SummaryRange {
    #[default]
    Cycle,
    SevenDays,
    ThirtyDays,
}

impl SummaryRange {
    const ALL: [Self; 3] = [Self::Cycle, Self::SevenDays, Self::ThirtyDays];

    fn index(self) -> usize {
        match self {
            Self::Cycle => 0,
            Self::SevenDays => 1,
            Self::ThirtyDays => 2,
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Cycle => 'C',
            Self::SevenDays => '7',
            Self::ThirtyDays => 'M',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cycle => "Cycle",
            Self::SevenDays => "7d",
            Self::ThirtyDays => "30d",
        }
    }

    fn window(
        self,
        snapshot: &Snapshot,
        query_now: DateTime<Utc>,
    ) -> (SummaryWindow, Option<&'static str>) {
        let query_now = query_now.max(snapshot.as_of);
        let ends_at = query_now
            .checked_add_signed(ChronoDuration::nanoseconds(1))
            .unwrap_or(query_now);
        let (starts_at, fallback_note) = match self {
            Self::Cycle => {
                let starts_at = window_analysis(snapshot, WindowScope::Week)
                    .filter(|analysis| {
                        !analysis
                            .partial_reasons
                            .iter()
                            .any(|reason| reason == "quota_window_stale")
                    })
                    .and_then(|analysis| analysis.attribution.window.as_ref())
                    .filter(|window| window.starts_at <= query_now && query_now < window.ends_at)
                    .map(|window| window.starts_at);
                starts_at.map_or_else(
                    || (query_now - ChronoDuration::days(7), Some("7d fallback")),
                    |starts_at| (starts_at, None),
                )
            }
            Self::SevenDays => (query_now - ChronoDuration::days(7), None),
            Self::ThirtyDays => (query_now - ChronoDuration::days(30), None),
        };
        (
            SummaryWindow::new(starts_at.min(query_now), ends_at)
                .expect("summary ranges always have a positive duration"),
            fallback_note,
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SummaryGrain {
    #[default]
    Day,
    Hours12,
    Hours6,
    Hours3,
    Hour,
}

impl SummaryGrain {
    const ALL: [Self; 5] = [
        Self::Day,
        Self::Hours12,
        Self::Hours6,
        Self::Hours3,
        Self::Hour,
    ];

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn index(self) -> usize {
        match self {
            Self::Day => 0,
            Self::Hours12 => 1,
            Self::Hours6 => 2,
            Self::Hours3 => 3,
            Self::Hour => 4,
        }
    }

    fn hours(self) -> u32 {
        match self {
            Self::Day => 24,
            Self::Hours12 => 12,
            Self::Hours6 => 6,
            Self::Hours3 => 3,
            Self::Hour => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Day => "1d",
            Self::Hours12 => "12h",
            Self::Hours6 => "6h",
            Self::Hours3 => "3h",
            Self::Hour => "1h",
        }
    }

    /// Three cells for every value keeps the whole `[B]...` hitbox stable as
    /// the selected grain changes.
    fn control_suffix(self) -> &'static str {
        match self {
            Self::Day => " 1d",
            Self::Hours12 => "12h",
            Self::Hours6 => " 6h",
            Self::Hours3 => " 3h",
            Self::Hour => " 1h",
        }
    }

    fn bucket_start(self, hour: NaiveDateTime) -> NaiveDateTime {
        let bucket_hour = match self {
            Self::Day => 0,
            _ => hour.hour().div_euclid(self.hours()) * self.hours(),
        };
        hour.date().and_hms_opt(bucket_hour, 0, 0).unwrap_or(hour)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SummaryMetric {
    #[default]
    Tokens,
    Estimated,
    ApiEquivalent,
}

impl SummaryMetric {
    const ALL: [Self; 3] = [Self::Tokens, Self::Estimated, Self::ApiEquivalent];

    fn index(self) -> usize {
        match self {
            Self::Tokens => 0,
            Self::Estimated => 1,
            Self::ApiEquivalent => 2,
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Tokens => 'K',
            Self::Estimated => 'E',
            Self::ApiEquivalent => 'A',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Tokens",
            Self::Estimated => "~EST CR.",
            Self::ApiEquivalent => "API EQ.",
        }
    }

    fn value(self, metrics: SummaryMetrics, api_long_context: bool) -> u128 {
        match self {
            Self::Tokens => u128::from(metrics.token_usage.total_tokens),
            Self::Estimated => metrics.estimated_units(api_long_context),
            Self::ApiEquivalent => metrics.api_equivalent_cost.minimum_pico_usd.value(),
        }
    }

    fn share_percent(
        self,
        metrics: SummaryMetrics,
        total: SummaryMetrics,
        api_long_context: bool,
    ) -> f64 {
        percent_u128(
            self.value(metrics, api_long_context),
            self.value(total, api_long_context),
        )
    }
}

fn window_analysis(snapshot: &Snapshot, scope: WindowScope) -> Option<&WindowAnalysis> {
    snapshot.window_analyses.iter().find(|analysis| {
        analysis.duration_mins == scope.duration_mins()
            && analysis
                .attribution
                .window
                .as_ref()
                .is_some_and(|window| window.limit_id.trim().eq_ignore_ascii_case("codex"))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApiCostWindowState {
    Unavailable,
    NoLocalData,
    Incomplete,
    Complete,
}

fn api_cost_window_state(analysis: Option<&WindowAnalysis>) -> ApiCostWindowState {
    let Some(analysis) = analysis else {
        return ApiCostWindowState::Unavailable;
    };
    if analysis
        .partial_reasons
        .iter()
        .any(|reason| reason == "local_scan_disabled")
    {
        return ApiCostWindowState::NoLocalData;
    }
    let rollout_incomplete = analysis
        .partial_reasons
        .iter()
        .any(|reason| reason.starts_with("rollout_"));
    if !rollout_incomplete {
        return ApiCostWindowState::Complete;
    }
    let amount = analysis.api_equivalent_cost.amount;
    if amount.observed_samples == 0 && amount.observed_tokens == 0 {
        ApiCostWindowState::NoLocalData
    } else {
        ApiCostWindowState::Incomplete
    }
}

fn format_scoped_api_cost_amount(state: ApiCostWindowState, cost: ApiCostAmount) -> String {
    match state {
        ApiCostWindowState::Unavailable | ApiCostWindowState::NoLocalData => "-".to_string(),
        ApiCostWindowState::Complete => format_api_cost_amount(cost),
        ApiCostWindowState::Incomplete => {
            let mut formatted = format_api_cost_amount(cost);
            if formatted != "-" && !formatted.ends_with('+') {
                formatted.push('+');
            }
            formatted
        }
    }
}

fn api_cost_column_width(analysis: Option<&WindowAnalysis>, visible_values: &[String]) -> u16 {
    // The window total keeps the width stable for ordinary amounts. Visible
    // values also participate because special decimal formatting can make a
    // numerically smaller entity wider than the total.
    let total_width = analysis
        .map(|analysis| {
            UnicodeWidthStr::width(
                format_scoped_api_cost_amount(
                    api_cost_window_state(Some(analysis)),
                    analysis.api_equivalent_cost.amount,
                )
                .as_str(),
            )
        })
        .unwrap_or(0);
    let value_width = visible_values
        .iter()
        .map(|value| UnicodeWidthStr::width(value.as_str()))
        .max()
        .unwrap_or(0);
    u16::try_from(
        total_width
            .max(value_width)
            .max(UnicodeWidthStr::width("API EQ.")),
    )
    .unwrap_or(u16::MAX)
}

fn window_analysis_with_api_long_context(
    snapshot: &Snapshot,
    scope: WindowScope,
    api_long_context: bool,
) -> Option<&WindowAnalysis> {
    let analysis = window_analysis(snapshot, scope)?;
    if api_long_context {
        analysis.api_long_context.as_deref().or(Some(analysis))
    } else {
        Some(analysis)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResetExpiryReminder {
    expires_at: chrono::DateTime<chrono::Utc>,
    weekly_reset_at: chrono::DateTime<chrono::Utc>,
}

fn reset_expiry_reminder(snapshot: &Snapshot) -> Option<ResetExpiryReminder> {
    let reset_credits = snapshot.rate_limit_reset_credits.as_ref()?;
    if reset_credits.available_count == 0
        || snapshot.rate_limit_reset_credits_partial
        || !matches!(
            reset_credits.provenance,
            Provenance::Live | Provenance::ServerSnapshot
        )
        || reset_credits.details_are_truncated()
    {
        return None;
    }
    let credits = reset_credits.credits.as_deref()?;

    let weekly_analysis = window_analysis(snapshot, WindowScope::Week)?;
    if weekly_analysis
        .partial_reasons
        .iter()
        .any(|reason| reason == "quota_window_stale")
    {
        return None;
    }
    let weekly_reset_at = weekly_analysis.attribution.window.as_ref()?.ends_at;
    if weekly_reset_at <= snapshot.as_of {
        return None;
    }

    let expires_at = credits
        .iter()
        .filter(|credit| {
            credit.status.trim().eq_ignore_ascii_case("available")
                && credit
                    .reset_type
                    .trim()
                    .eq_ignore_ascii_case("codexRateLimits")
        })
        .filter_map(|credit| credit.expires_at)
        .filter(|expires_at| *expires_at > snapshot.as_of)
        .min()?;

    (expires_at < weekly_reset_at).then_some(ResetExpiryReminder {
        expires_at,
        weekly_reset_at,
    })
}

fn has_legacy_codex_window(snapshot: &Snapshot) -> bool {
    snapshot
        .attribution
        .window
        .as_ref()
        .is_some_and(|window| window.limit_id.trim().eq_ignore_ascii_case("codex"))
}

#[cfg(test)]
fn attribution_for_scope(snapshot: &Snapshot, scope: WindowScope) -> Option<&AttributionSummary> {
    attribution_for_scope_with_api_long_context(snapshot, scope, false)
}

fn attribution_for_scope_with_api_long_context(
    snapshot: &Snapshot,
    scope: WindowScope,
    api_long_context: bool,
) -> Option<&AttributionSummary> {
    window_analysis_with_api_long_context(snapshot, scope, api_long_context)
        .map(|analysis| &analysis.attribution)
        .or_else(|| {
            (scope == WindowScope::FiveHours && has_legacy_codex_window(snapshot))
                .then_some(&snapshot.attribution)
        })
}

#[cfg(test)]
fn task_usage_for_scope(snapshot: &Snapshot, scope: WindowScope, task: &TaskRecord) -> WindowUsage {
    task_usage_for_scope_with_api_long_context(snapshot, scope, task, false)
}

fn task_usage_for_scope_with_api_long_context(
    snapshot: &Snapshot,
    scope: WindowScope,
    task: &TaskRecord,
    api_long_context: bool,
) -> WindowUsage {
    if let Some(analysis) = window_analysis_with_api_long_context(snapshot, scope, api_long_context)
    {
        return analysis
            .threads
            .iter()
            .find(|usage| usage.thread_id == task.thread_id)
            .map(|usage| usage.usage)
            .unwrap_or_default();
    }

    if scope == WindowScope::FiveHours && has_legacy_codex_window(snapshot) {
        WindowUsage {
            token_usage: task.window_token_usage,
            local_token_share_percent: task.local_token_share_percent,
            estimated_quota_percent: task.estimated_quota_percent,
            quota_confidence: task.quota_confidence,
            api_equivalent_cost: task.api_equivalent_cost.unwrap_or_default(),
        }
    } else {
        WindowUsage::default()
    }
}

fn turn_usage_for_scope_with_api_long_context(
    snapshot: &Snapshot,
    scope: WindowScope,
    turn: &TurnRecord,
    api_long_context: bool,
) -> WindowUsage {
    if let Some(analysis) = window_analysis_with_api_long_context(snapshot, scope, api_long_context)
    {
        return analysis
            .turns
            .iter()
            .find(|usage| usage.thread_id == turn.thread_id && usage.turn_id == turn.turn_id)
            .map(|usage| usage.usage)
            .unwrap_or_default();
    }

    if scope == WindowScope::FiveHours && has_legacy_codex_window(snapshot) {
        WindowUsage {
            token_usage: turn.window_token_usage,
            local_token_share_percent: turn.local_token_share_percent,
            estimated_quota_percent: turn.estimated_quota_percent,
            quota_confidence: turn.quota_confidence,
            api_equivalent_cost: turn.api_equivalent_cost.unwrap_or_default(),
        }
    } else {
        WindowUsage::default()
    }
}

fn task_record_usage(
    snapshot: &Snapshot,
    scope: WindowScope,
    task: &TaskRecord,
    window_only: bool,
    api_long_context: bool,
) -> WindowUsage {
    if window_only {
        task_usage_for_scope_with_api_long_context(snapshot, scope, task, api_long_context)
    } else {
        WindowUsage {
            token_usage: task.token_usage,
            local_token_share_percent: task.local_token_share_percent,
            estimated_quota_percent: task.estimated_quota_percent,
            quota_confidence: task.quota_confidence,
            api_equivalent_cost: Default::default(),
        }
    }
}

#[cfg(test)]
fn aggregate_task_row_usage(
    snapshot: &Snapshot,
    scope: WindowScope,
    row: &TaskListRow,
    window_only: bool,
) -> WindowUsage {
    aggregate_task_row_usage_with_api_long_context(snapshot, scope, row, window_only, false)
}

fn aggregate_task_row_usage_with_api_long_context(
    snapshot: &Snapshot,
    scope: WindowScope,
    row: &TaskListRow,
    window_only: bool,
    api_long_context: bool,
) -> WindowUsage {
    let Some(task) = snapshot.tasks.get(row.index) else {
        return WindowUsage::default();
    };
    if window_only
        && !row.hidden_descendants.is_empty()
        && let Some(analysis) =
            window_analysis_with_api_long_context(snapshot, scope, api_long_context)
    {
        let thread_ids = std::iter::once(row.index)
            .chain(row.hidden_descendants.iter().copied())
            .filter_map(|index| snapshot.tasks.get(index))
            .map(|task| task.thread_id.as_str())
            .collect::<HashSet<_>>();
        let mut aggregate = WindowUsage::default();
        let mut quota_confidence = None;
        for thread in analysis
            .threads
            .iter()
            .filter(|thread| thread_ids.contains(thread.thread_id.as_str()))
        {
            aggregate.token_usage.add_assign(thread.usage.token_usage);
            aggregate.local_token_share_percent += thread.usage.local_token_share_percent;
            aggregate.estimated_quota_percent += thread.usage.estimated_quota_percent;
            aggregate
                .api_equivalent_cost
                .add_assign(thread.usage.api_equivalent_cost);
            if quota_estimate_participates(&thread.usage) {
                quota_confidence = Some(match quota_confidence {
                    None => thread.usage.quota_confidence,
                    Some(current) => {
                        weakest_quota_confidence(current, thread.usage.quota_confidence)
                    }
                });
            }
        }
        aggregate.quota_confidence = quota_confidence.unwrap_or(Confidence::Unknown);
        return aggregate;
    }

    let mut aggregate = task_record_usage(snapshot, scope, task, window_only, api_long_context);
    if row.hidden_descendants.is_empty() {
        return aggregate;
    }
    let parent_confidence = aggregate.quota_confidence;
    let mut quota_confidence =
        quota_estimate_participates(&aggregate).then_some(aggregate.quota_confidence);
    for index in row.hidden_descendants.iter().copied() {
        let Some(descendant) = snapshot.tasks.get(index) else {
            continue;
        };
        let usage = task_record_usage(snapshot, scope, descendant, window_only, api_long_context);
        aggregate.token_usage.add_assign(usage.token_usage);
        aggregate.local_token_share_percent += usage.local_token_share_percent;
        aggregate.estimated_quota_percent += usage.estimated_quota_percent;
        aggregate
            .api_equivalent_cost
            .add_assign(usage.api_equivalent_cost);
        if quota_estimate_participates(&usage) {
            quota_confidence = Some(match quota_confidence {
                None => usage.quota_confidence,
                Some(current) => weakest_quota_confidence(current, usage.quota_confidence),
            });
        }
    }
    aggregate.quota_confidence = quota_confidence.unwrap_or(parent_confidence);
    aggregate
}

fn quota_estimate_participates(usage: &WindowUsage) -> bool {
    !usage.token_usage.is_zero()
        || usage.estimated_quota_percent > 0.0
        || usage.quota_confidence != Confidence::Unknown
}

fn weakest_quota_confidence(left: Confidence, right: Confidence) -> Confidence {
    use Confidence::{High, Low, Medium, Unknown};
    match (left, right) {
        (Unknown, _) | (_, Unknown) => Unknown,
        (Low, _) | (_, Low) => Low,
        (Medium, _) | (_, Medium) => Medium,
        (High, High) => High,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Focus {
    #[default]
    Tasks,
    Turns,
    TaskSearch,
    TurnSearch,
}

impl Focus {
    fn is_search(self) -> bool {
        matches!(self, Self::TaskSearch | Self::TurnSearch)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TaskSourceFilter {
    #[default]
    All,
    Desktop,
    Subagent,
    Cli,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TaskListMode {
    #[default]
    Flat,
    Tree,
}

impl From<UiTaskListMode> for TaskListMode {
    fn from(value: UiTaskListMode) -> Self {
        match value {
            UiTaskListMode::Flat => Self::Flat,
            UiTaskListMode::Tree => Self::Tree,
        }
    }
}

impl From<TaskListMode> for UiTaskListMode {
    fn from(value: TaskListMode) -> Self {
        match value {
            TaskListMode::Flat => Self::Flat,
            TaskListMode::Tree => Self::Tree,
        }
    }
}

impl TaskListMode {
    fn toggle(self) -> Self {
        match self {
            Self::Flat => Self::Tree,
            Self::Tree => Self::Flat,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TaskListRow {
    index: usize,
    prefix: String,
    depth: usize,
    has_children: bool,
    collapsed: bool,
    hidden_descendants: Vec<usize>,
}

impl TaskSourceFilter {
    const ALL: [Self; 4] = [Self::All, Self::Desktop, Self::Subagent, Self::Cli];

    fn index(self) -> usize {
        match self {
            Self::All => 0,
            Self::Desktop => 1,
            Self::Subagent => 2,
            Self::Cli => 3,
        }
    }

    fn label(self, compact: bool) -> &'static str {
        match (self, compact) {
            (Self::All, true) => "A",
            (Self::All, false) => "All",
            (Self::Desktop, true) => "D",
            (Self::Desktop, false) => "Desktop",
            (Self::Subagent, true) => "S",
            (Self::Subagent, false) => "Subagent",
            (Self::Cli, true) => "C",
            (Self::Cli, false) => "CLI",
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::All => 'A',
            Self::Desktop => 'D',
            Self::Subagent => 'S',
            Self::Cli => 'C',
        }
    }

    fn matches(self, source: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Desktop => source.is_some_and(|source| {
                source.eq_ignore_ascii_case("desktop") || source.eq_ignore_ascii_case("vscode")
            }),
            Self::Subagent => source.is_some_and(|source| source.eq_ignore_ascii_case("subagent")),
            Self::Cli => source.is_some_and(|source| source.eq_ignore_ascii_case("cli")),
        }
    }
}

impl From<UiTaskSourceFilter> for TaskSourceFilter {
    fn from(value: UiTaskSourceFilter) -> Self {
        match value {
            UiTaskSourceFilter::All => Self::All,
            UiTaskSourceFilter::Desktop => Self::Desktop,
            UiTaskSourceFilter::Subagent => Self::Subagent,
            UiTaskSourceFilter::Cli => Self::Cli,
        }
    }
}

impl From<TaskSourceFilter> for UiTaskSourceFilter {
    fn from(value: TaskSourceFilter) -> Self {
        match value {
            TaskSourceFilter::All => Self::All,
            TaskSourceFilter::Desktop => Self::Desktop,
            TaskSourceFilter::Subagent => Self::Subagent,
            TaskSourceFilter::Cli => Self::Cli,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusTone {
    Active,
    Waiting,
    Done,
    Stopped,
    Failed,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TableHitbox {
    viewport: Rect,
    rows: Rect,
    offset: usize,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TaskControlsHitbox {
    sources: [Rect; 4],
    search: Rect,
    clear_search: Rect,
    enter_turns: Rect,
    open_terminal: Rect,
    toggle_tree: Rect,
    collapse_all: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TaskTreeMarkerHitbox {
    area: Rect,
    task_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SummaryTreeMarkerHitbox {
    area: Rect,
    node_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SummaryBarHitbox {
    area: Rect,
    project_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SummaryDailyHitbox {
    plot: Rect,
    dates: Vec<NaiveDateTime>,
}

impl SummaryDailyHitbox {
    fn exact(plot: Rect, dates: Vec<NaiveDateTime>) -> Option<Self> {
        if plot.is_empty() {
            return None;
        }
        date_index_at_column(0, usize::from(plot.width), dates.len())?;
        Some(Self { plot, dates })
    }

    fn contains(&self, column: u16, row: u16) -> bool {
        rect_contains(self.plot, column, row)
    }

    fn date_at_column(&self, column: u16) -> Option<NaiveDateTime> {
        if self.plot.is_empty() {
            return None;
        }
        let column = column.clamp(self.plot.x, self.plot.right().saturating_sub(1));
        let index = date_index_at_column(
            usize::from(column.saturating_sub(self.plot.x)),
            usize::from(self.plot.width),
            self.dates.len(),
        )?;
        self.dates.get(index).copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TurnControlsHitbox {
    back_tasks: Rect,
    search: Rect,
    clear_search: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewTabsHitbox {
    tabs: [Rect; 5],
    rendered_right: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SummaryControlsHitbox {
    ranges: [Rect; 3],
    metrics: [Rect; 3],
    bucket_grain: Rect,
    toggle_all_projects: Rect,
    toggle_long_context: Rect,
    inspect: Rect,
    toggle_selected: Rect,
    collapse_all: Rect,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SettingsControlsHitbox {
    rows: [Rect; 8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowControlsHitbox {
    toggle_turns: Rect,
    toggle_models: Rect,
    scopes: [Rect; 2],
    toggle_api_long_context: Rect,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TrendControlsHitbox {
    sections: [Rect; 3],
    inspect: Rect,
    previous_day: Rect,
    next_day: Rect,
    now: Rect,
}

#[derive(Clone, Debug)]
struct PreparedSummary {
    usage: UsageSummary,
    range_note: Option<&'static str>,
    represented_tokens: u64,
    available_tokens: u64,
    covered_buckets: usize,
    expected_buckets: usize,
    estimated_covered_tokens: u64,
    long_context_breakdown_complete: bool,
    daily_coverage: BTreeMap<NaiveDate, SummaryDailyCoverage>,
    hourly_coverage: BTreeMap<NaiveDateTime, SummaryDailyCoverage>,
    partial_reasons: Vec<String>,
}

#[derive(Clone, Debug)]
struct SummaryDailyCoverage {
    expected_buckets: usize,
    covered_buckets: usize,
    available_tokens: u64,
    represented_tokens: u64,
    estimated_covered_tokens: u64,
    long_context_breakdown_complete: bool,
    source_partial: bool,
}

impl Default for SummaryDailyCoverage {
    fn default() -> Self {
        Self {
            expected_buckets: 0,
            covered_buckets: 0,
            available_tokens: 0,
            represented_tokens: 0,
            estimated_covered_tokens: 0,
            long_context_breakdown_complete: true,
            source_partial: false,
        }
    }
}

impl SummaryDailyCoverage {
    fn add_assign(&mut self, other: &Self) {
        self.expected_buckets = self.expected_buckets.saturating_add(other.expected_buckets);
        self.covered_buckets = self.covered_buckets.saturating_add(other.covered_buckets);
        self.available_tokens = self.available_tokens.saturating_add(other.available_tokens);
        self.represented_tokens = self
            .represented_tokens
            .saturating_add(other.represented_tokens);
        self.estimated_covered_tokens = self
            .estimated_covered_tokens
            .saturating_add(other.estimated_covered_tokens);
        self.long_context_breakdown_complete &= other.long_context_breakdown_complete;
        self.source_partial |= other.source_partial;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryDailyState {
    Complete,
    Partial,
    Missing,
}

#[derive(Clone, Debug)]
struct SummaryChartBucket {
    starts_at: NaiveDateTime,
    totals: SummaryMetrics,
    coverage: SummaryDailyCoverage,
}

#[derive(Clone, Debug)]
struct SummaryChartData {
    grain: SummaryGrain,
    buckets: Vec<SummaryChartBucket>,
    project_values: HashMap<String, BTreeMap<NaiveDateTime, SummaryMetrics>>,
}

fn summary_daily_status_symbols(states: &[SummaryDailyState]) -> String {
    states
        .iter()
        .map(|state| match state {
            SummaryDailyState::Complete => 'C',
            SummaryDailyState::Partial => 'P',
            SummaryDailyState::Missing => 'M',
        })
        .collect()
}

impl PreparedSummary {
    fn coverage_percent(&self, metric: SummaryMetric) -> f64 {
        let token_coverage = if self.available_tokens == 0 {
            100.0
        } else {
            self.represented_tokens as f64 / self.available_tokens as f64 * 100.0
        };
        let time_coverage = if self.expected_buckets == 0 {
            100.0
        } else {
            self.covered_buckets as f64 / self.expected_buckets as f64 * 100.0
        };
        let pricing_coverage = if metric == SummaryMetric::ApiEquivalent {
            self.usage.totals.api_equivalent_cost.priced_token_percent()
        } else {
            100.0
        };
        let estimator_coverage = if metric != SummaryMetric::Estimated || self.available_tokens == 0
        {
            100.0
        } else {
            self.estimated_covered_tokens as f64 / self.available_tokens as f64 * 100.0
        };
        token_coverage
            .min(time_coverage)
            .min(pricing_coverage)
            .min(estimator_coverage)
            .clamp(0.0, 100.0)
    }

    fn partial(&self, metric: SummaryMetric, api_long_context: bool) -> bool {
        !self.partial_reasons.is_empty()
            || self.represented_tokens < self.available_tokens
            || self.covered_buckets < self.expected_buckets
            || (metric == SummaryMetric::ApiEquivalent && {
                let amount = self.usage.totals.api_equivalent_cost;
                amount.priced_samples < amount.observed_samples
                    || amount.priced_tokens < amount.observed_tokens
            })
            || (metric == SummaryMetric::Estimated
                && api_long_context
                && !self.long_context_breakdown_complete)
    }

    fn api_chart_is_lower_bound(&self) -> bool {
        let amount = self.usage.totals.api_equivalent_cost;
        !amount.range_is_exact()
            || amount.priced_samples < amount.observed_samples
            || amount.priced_tokens < amount.observed_tokens
            || self.represented_tokens < self.available_tokens
            || self.covered_buckets < self.expected_buckets
            || self.partial_reasons.iter().any(|reason| {
                reason.starts_with("rollout_")
                    || matches!(
                        reason.as_str(),
                        "local_scan_disabled" | "range_starts_within_15m_bucket"
                    )
            })
    }

    fn daily_state(
        &self,
        date: NaiveDate,
        totals: SummaryMetrics,
        metric: SummaryMetric,
        api_long_context: bool,
    ) -> SummaryDailyState {
        let Some(coverage) = self.daily_coverage.get(&date) else {
            return SummaryDailyState::Missing;
        };
        if coverage.covered_buckets == 0 {
            return SummaryDailyState::Missing;
        }
        let metric_partial = match metric {
            SummaryMetric::Tokens => false,
            SummaryMetric::Estimated => {
                coverage.estimated_covered_tokens < coverage.available_tokens
                    || (api_long_context && !coverage.long_context_breakdown_complete)
            }
            SummaryMetric::ApiEquivalent => {
                let amount = totals.api_equivalent_cost;
                amount.priced_samples < amount.observed_samples
                    || amount.priced_tokens < amount.observed_tokens
                    || !amount.range_is_exact()
            }
        };
        if coverage.covered_buckets < coverage.expected_buckets
            || coverage.represented_tokens < coverage.available_tokens
            || coverage.source_partial
            || metric_partial
            || summary_day_is_partial_window_edge(self.usage.window, date)
        {
            SummaryDailyState::Partial
        } else {
            SummaryDailyState::Complete
        }
    }

    fn chart_bucket_state(
        &self,
        bucket: &SummaryChartBucket,
        grain: SummaryGrain,
        metric: SummaryMetric,
        api_long_context: bool,
    ) -> SummaryDailyState {
        if grain == SummaryGrain::Day {
            return self.daily_state(
                bucket.starts_at.date(),
                bucket.totals,
                metric,
                api_long_context,
            );
        }
        let coverage = &bucket.coverage;
        if coverage.covered_buckets == 0 {
            return SummaryDailyState::Missing;
        }
        let metric_partial = match metric {
            SummaryMetric::Tokens => false,
            SummaryMetric::Estimated => {
                coverage.estimated_covered_tokens < coverage.available_tokens
                    || (api_long_context && !coverage.long_context_breakdown_complete)
            }
            SummaryMetric::ApiEquivalent => {
                let amount = bucket.totals.api_equivalent_cost;
                amount.priced_samples < amount.observed_samples
                    || amount.priced_tokens < amount.observed_tokens
                    || !amount.range_is_exact()
            }
        };
        if coverage.covered_buckets < coverage.expected_buckets
            || coverage.represented_tokens < coverage.available_tokens
            || coverage.source_partial
            || metric_partial
            || summary_bucket_is_partial_window_edge(self.usage.window, bucket.starts_at, grain)
        {
            SummaryDailyState::Partial
        } else {
            SummaryDailyState::Complete
        }
    }
}

#[derive(Clone, Debug)]
struct SummaryCache {
    range: SummaryRange,
    snapshot_as_of: DateTime<Utc>,
    query_bucket: i64,
    query_local_date: NaiveDate,
    prepared: PreparedSummary,
    chart: SummaryChartData,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryRowKind {
    Project,
    Session,
    Turn,
}

#[derive(Clone, Debug)]
struct SummaryTreeRow {
    id: String,
    kind: SummaryRowKind,
    prefix: String,
    label: String,
    source: Option<String>,
    metrics: SummaryMetrics,
    has_children: bool,
    collapsed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QuitConfirmationHitbox {
    confirm: Rect,
    cancel: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResumeConfirmationHitbox {
    confirm: Rect,
    copy: Rect,
    cancel: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResumeConfirmation {
    thread_id: String,
    copy_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClipboardRequest {
    thread_id: String,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenNoticeTone {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenNotice {
    message: String,
    tone: OpenNoticeTone,
    created_at: Instant,
}

#[derive(Clone, Debug)]
enum ResumeLaunchRequest {
    Create {
        target: ResumeTarget,
        codex_home: PathBuf,
        codex_bin: Option<PathBuf>,
        options: ZellijOptions,
    },
    Focus {
        thread_id: String,
        pane_id: PaneId,
        codex_home: PathBuf,
    },
}

struct PreparedTrendData {
    five_hour_remaining: Vec<TrendPoint>,
    weekly_remaining: Vec<TrendPoint>,
    weekly_tokens: Vec<TrendPoint>,
    weekly_estimated: Vec<TrendPoint>,
    half_hour_tokens: Vec<TrendPoint>,
    half_hour_estimated: Vec<TrendPoint>,
    five_hour_remaining_readout: Option<TrendReadout>,
    weekly_remaining_readout: Option<TrendReadout>,
    weekly_tokens_readout: Option<TrendReadout>,
    weekly_estimated_readout: Option<TrendReadout>,
    half_hour_bounds: [DateTime<Utc>; 2],
    weekly_history_present: bool,
    half_hour_history_present: bool,
    history_warning_count: usize,
    history_read_only: bool,
    api_long_context_multiplier: bool,
}

struct TrendPanelSpec<'a> {
    panel: TrendPanelId,
    title: &'a str,
    graph_kind: TrendGraphKind,
    value_kind: TrendValueKind,
    fixed_y_bounds: Option<[f64; 2]>,
    fixed_x_bounds: Option<[DateTime<Utc>; 2]>,
    history_warning_count: usize,
    history_read_only: bool,
    readout_label: Option<&'static str>,
    theme: Theme,
}

struct TrendControlSpec<'a> {
    shortcut: &'a str,
    suffix: &'static str,
    selected: bool,
    shortcuts_active: bool,
    theme: Theme,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ResumeLaunchOutcome {
    Created(PaneId),
    Focused(PaneId),
    Missing(PaneId),
}

#[derive(Debug)]
struct ResumeLaunchCompletion {
    thread_id: String,
    result: Result<ResumeLaunchOutcome, String>,
}

struct RefreshCompletion {
    result: Option<CollectionResult>,
    history: Option<HistoryData>,
    recorder_health: Option<RecorderHealth>,
    refreshed_account: bool,
    summary_backfill: bool,
}

#[derive(Default)]
struct RefreshWorker {
    handle: Option<thread::JoinHandle<()>>,
}

impl RefreshWorker {
    fn start(&mut self, handle: thread::JoinHandle<()>) {
        self.join();
        self.handle = Some(handle);
    }

    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn detach(&mut self) {
        self.handle.take();
    }
}

impl Drop for RefreshWorker {
    fn drop(&mut self) {
        self.join();
    }
}

fn account_limits_are_fresh(result: &CollectionResult) -> bool {
    result
        .account
        .limits
        .iter()
        .any(|limit| limit.provenance == Provenance::ServerSnapshot)
}

fn account_refresh_is_complete(result: &CollectionResult) -> bool {
    let reset_credits_complete = result
        .account
        .rate_limit_reset_credits
        .as_ref()
        .is_none_or(|reset_credits| reset_credits.provenance == Provenance::ServerSnapshot);

    account_limits_are_fresh(result)
        && !result.account.rate_limit_reset_credits_partial
        && reset_credits_complete
}

fn collection_history_observation(
    result: &CollectionResult,
    offline: bool,
) -> Cow<'_, HistoryObservation> {
    if offline || account_limits_are_fresh(result) {
        return Cow::Borrowed(&result.history_observation);
    }
    let mut observation = result.history_observation.clone();
    observation.quota_points.clear();
    observation.weekly_local_points.clear();
    Cow::Owned(observation)
}

#[derive(Clone, Debug, Default)]
struct RecorderHealth {
    status: Option<RecorderStatusFile>,
    error: Option<String>,
}

struct RunLoopChannels<'a> {
    refresh_sender: &'a mpsc::Sender<RefreshCompletion>,
    refresh_receiver: &'a Receiver<RefreshCompletion>,
    resume_sender: &'a mpsc::Sender<ResumeLaunchCompletion>,
    resume_receiver: &'a Receiver<ResumeLaunchCompletion>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RedrawReasons(u8);

impl RedrawReasons {
    const INPUT: u8 = 1 << 0;
    const SNAPSHOT: u8 = 1 << 1;
    const RESUME: u8 = 1 << 2;
    const NOTICE: u8 = 1 << 3;
    const RESIZE: u8 = 1 << 4;

    fn insert(&mut self, reason: u8) {
        self.0 |= reason;
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn clear(&mut self) {
        self.0 = 0;
    }

    fn label(self) -> String {
        let mut labels = Vec::with_capacity(5);
        for (reason, label) in [
            (Self::INPUT, "input"),
            (Self::SNAPSHOT, "snapshot"),
            (Self::RESUME, "resume"),
            (Self::NOTICE, "notice"),
            (Self::RESIZE, "resize"),
        ] {
            if self.0 & reason != 0 {
                labels.push(label);
            }
        }
        labels.join("+")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollTarget {
    Tasks,
    Turns,
    Summary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScrollbarHitbox {
    track: Rect,
    thumb: Rect,
    max_offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScrollDrag {
    target: ScrollTarget,
    grab_row: u16,
    pointer_row: Option<u16>,
}

impl TableHitbox {
    fn index_at(self, column: u16, row: u16) -> Option<usize> {
        let inside = column >= self.rows.x
            && column < self.rows.right()
            && row >= self.rows.y
            && row < self.rows.bottom();
        inside.then(|| self.offset + usize::from(row - self.rows.y))
    }

    fn contains_viewport(self, column: u16, row: u16) -> bool {
        column >= self.viewport.x
            && column < self.viewport.right()
            && row >= self.viewport.y
            && row < self.viewport.bottom()
    }
}

impl View {
    const ALL: [Self; 5] = [
        Self::Overview,
        Self::Trends,
        Self::Summary,
        Self::Health,
        Self::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Trends => "Trends",
            Self::Summary => "Summary",
            Self::Health => "Other",
            Self::Settings => "Settings",
        }
    }

    fn compact_label(self) -> &'static str {
        match self {
            Self::Overview => "Ovw",
            Self::Trends => "Tr",
            Self::Summary => "Sum",
            Self::Health => "Other",
            Self::Settings => "Set",
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Overview => '1',
            Self::Trends => '2',
            Self::Summary => 'U',
            Self::Health => '3',
            Self::Settings => '4',
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Trends => 1,
            Self::Summary => 2,
            Self::Health => 3,
            Self::Settings => 4,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Overview => Self::Trends,
            Self::Trends => Self::Summary,
            Self::Summary => Self::Health,
            Self::Health => Self::Settings,
            Self::Settings => Self::Overview,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Overview => Self::Settings,
            Self::Trends => Self::Overview,
            Self::Summary => Self::Trends,
            Self::Health => Self::Summary,
            Self::Settings => Self::Health,
        }
    }
}

struct App {
    snapshot: Snapshot,
    account: AccountSnapshot,
    history: HistoryData,
    recorder_health: RecorderHealth,
    theme: Theme,
    view: View,
    window_scope: WindowScope,
    trend_section: TrendSection,
    trend_day_offset: u16,
    focus: Focus,
    task_source_filter: TaskSourceFilter,
    task_list_mode: TaskListMode,
    // Tree parents are collapsed unless the user explicitly expands them.
    expanded_task_threads: HashSet<String>,
    task_search: String,
    task_search_before_edit: String,
    task_search_cursor: usize,
    task_search_restore_thread_id: Option<String>,
    task_search_restore_turn_id: Option<String>,
    task_search_restore_task_offset: usize,
    task_search_restore_turn_offset: usize,
    turn_search: String,
    turn_search_before_edit: String,
    turn_search_cursor: usize,
    turn_search_restore_turn_id: Option<String>,
    turn_search_restore_offset: usize,
    turns_default_visible: bool,
    turns_temporarily_visible: bool,
    models_visible: bool,
    api_long_context_multiplier: bool,
    table_columns: UiTableColumns,
    selected_setting: usize,
    open_config: OpenConfig,
    open_config_error: Option<String>,
    zellij_environment: bool,
    resume_confirmation: Option<ResumeConfirmation>,
    resume_confirmation_hitbox: Option<ResumeConfirmationHitbox>,
    pending_clipboard: Option<ClipboardRequest>,
    pending_resume: Option<ResumeLaunchRequest>,
    launching_threads: HashSet<String>,
    open_panes: HashMap<String, PaneId>,
    open_notice: Option<OpenNotice>,
    selected_task: usize,
    selected_turn: usize,
    turn_offset: usize,
    task_table_offset: usize,
    task_reveal_pending: bool,
    task_table_hitbox: Option<TableHitbox>,
    turn_table_hitbox: Option<TableHitbox>,
    task_controls_hitbox: Option<TaskControlsHitbox>,
    task_tree_marker_hitboxes: Vec<TaskTreeMarkerHitbox>,
    turn_controls_hitbox: Option<TurnControlsHitbox>,
    window_controls_hitbox: Option<WindowControlsHitbox>,
    settings_controls_hitbox: Option<SettingsControlsHitbox>,
    trend_controls_hitbox: Option<TrendControlsHitbox>,
    trend_chart_hitboxes: Vec<TrendChartHitbox>,
    trend_inspect_mode: bool,
    trend_inspection: Option<TrendInspection>,
    trend_drag: Option<TrendDrag>,
    summary_range: SummaryRange,
    summary_grain: SummaryGrain,
    summary_metric: SummaryMetric,
    summary_show_all_projects: bool,
    summary_expanded_nodes: HashSet<String>,
    summary_selected_id: Option<String>,
    summary_offset: usize,
    summary_cache: Option<SummaryCache>,
    // Run-local registry: existing project colors never move when refreshed
    // history discovers another key. It is cleared only when the theme flips.
    summary_project_colors: HashMap<String, Color>,
    summary_controls_hitbox: Option<SummaryControlsHitbox>,
    summary_table_hitbox: Option<TableHitbox>,
    summary_tree_marker_hitboxes: Vec<SummaryTreeMarkerHitbox>,
    summary_bar_hitboxes: Vec<SummaryBarHitbox>,
    summary_daily_hitbox: Option<SummaryDailyHitbox>,
    summary_inspected_date: Option<NaiveDateTime>,
    summary_daily_dragging: bool,
    summary_scrollbar_hitbox: Option<ScrollbarHitbox>,
    summary_backfill_pending: bool,
    summary_backfill_running: bool,
    view_tabs_hitbox: Option<ViewTabsHitbox>,
    task_scrollbar_hitbox: Option<ScrollbarHitbox>,
    turn_scrollbar_hitbox: Option<ScrollbarHitbox>,
    scroll_drag: Option<ScrollDrag>,
    quit_confirmation_visible: bool,
    quit_confirmation_hitbox: Option<QuitConfirmationHitbox>,
    quit_requested: bool,
    turn_reveal_pending: bool,
    worker_running: bool,
    last_local_refresh: Instant,
    next_account_refresh: Instant,
    account_refresh_retry_count: usize,
}

impl App {
    fn new(result: CollectionResult, theme: Theme) -> Self {
        Self {
            snapshot: result.snapshot,
            account: result.account,
            history: HistoryData::default(),
            recorder_health: RecorderHealth::default(),
            theme,
            view: View::Overview,
            window_scope: WindowScope::FiveHours,
            trend_section: TrendSection::Remaining,
            trend_day_offset: 0,
            focus: Focus::Tasks,
            task_source_filter: TaskSourceFilter::All,
            task_list_mode: TaskListMode::Flat,
            expanded_task_threads: HashSet::new(),
            task_search: String::new(),
            task_search_before_edit: String::new(),
            task_search_cursor: 0,
            task_search_restore_thread_id: None,
            task_search_restore_turn_id: None,
            task_search_restore_task_offset: 0,
            task_search_restore_turn_offset: 0,
            turn_search: String::new(),
            turn_search_before_edit: String::new(),
            turn_search_cursor: 0,
            turn_search_restore_turn_id: None,
            turn_search_restore_offset: 0,
            turns_default_visible: true,
            turns_temporarily_visible: false,
            models_visible: true,
            api_long_context_multiplier: false,
            table_columns: UiTableColumns::default(),
            selected_setting: 0,
            open_config: OpenConfig::default(),
            open_config_error: None,
            zellij_environment: std::env::var_os("ZELLIJ").is_some(),
            resume_confirmation: None,
            resume_confirmation_hitbox: None,
            pending_clipboard: None,
            pending_resume: None,
            launching_threads: HashSet::new(),
            open_panes: HashMap::new(),
            open_notice: None,
            selected_task: 0,
            selected_turn: 0,
            turn_offset: 0,
            task_table_offset: 0,
            task_reveal_pending: false,
            task_table_hitbox: None,
            turn_table_hitbox: None,
            task_controls_hitbox: None,
            task_tree_marker_hitboxes: Vec::new(),
            turn_controls_hitbox: None,
            window_controls_hitbox: None,
            settings_controls_hitbox: None,
            trend_controls_hitbox: None,
            trend_chart_hitboxes: Vec::new(),
            trend_inspect_mode: false,
            trend_inspection: None,
            trend_drag: None,
            summary_range: SummaryRange::Cycle,
            summary_grain: SummaryGrain::Day,
            summary_metric: SummaryMetric::Tokens,
            summary_show_all_projects: false,
            summary_expanded_nodes: HashSet::new(),
            summary_selected_id: None,
            summary_offset: 0,
            summary_cache: None,
            summary_project_colors: HashMap::new(),
            summary_controls_hitbox: None,
            summary_table_hitbox: None,
            summary_tree_marker_hitboxes: Vec::new(),
            summary_bar_hitboxes: Vec::new(),
            summary_daily_hitbox: None,
            summary_inspected_date: None,
            summary_daily_dragging: false,
            summary_scrollbar_hitbox: None,
            summary_backfill_pending: false,
            summary_backfill_running: false,
            view_tabs_hitbox: None,
            task_scrollbar_hitbox: None,
            turn_scrollbar_hitbox: None,
            scroll_drag: None,
            quit_confirmation_visible: false,
            quit_confirmation_hitbox: None,
            quit_requested: false,
            turn_reveal_pending: false,
            worker_running: false,
            last_local_refresh: Instant::now(),
            next_account_refresh: Instant::now(),
            account_refresh_retry_count: 0,
        }
    }

    fn account_refresh_due(&self, now: Instant) -> bool {
        now >= self.next_account_refresh
    }

    fn reset_credit_fetch_status(&self, now: Instant) -> Option<&'static str> {
        let reset_credits = self.snapshot.rate_limit_reset_credits.as_ref();
        let details_incomplete = reset_credits.is_none_or(|reset_credits| {
            self.snapshot.rate_limit_reset_credits_partial
                || reset_credits.provenance != Provenance::ServerSnapshot
        });
        if !details_incomplete {
            return None;
        }

        let awaiting_initial_refresh = self.snapshot.sources.iter().any(|source| {
            source.source == "app_server"
                && source.status == "stale"
                && source.message.as_deref() == Some("no cached account snapshot")
        });
        if awaiting_initial_refresh {
            if self.account_refresh_retry_count > 0 {
                return Some("retrying");
            }
            return (self.worker_running || self.account_refresh_due(now)).then_some("loading");
        }

        let retryable = self.snapshot.rate_limit_reset_credits_partial
            || reset_credits.is_some_and(|reset_credits| {
                reset_credits.provenance != Provenance::ServerSnapshot
            });
        (retryable && self.account_refresh_retry_count > 0).then_some("retrying")
    }

    fn schedule_next_account_refresh(&mut self, result: &CollectionResult, now: Instant) {
        let delay = if account_refresh_is_complete(result) {
            self.account_refresh_retry_count = 0;
            ACCOUNT_REFRESH
        } else {
            let delay = ACCOUNT_REFRESH_RETRY_DELAYS
                .get(self.account_refresh_retry_count)
                .copied()
                .unwrap_or(ACCOUNT_REFRESH);
            self.account_refresh_retry_count = self
                .account_refresh_retry_count
                .saturating_add(1)
                .min(ACCOUNT_REFRESH_RETRY_DELAYS.len());
            delay
        };
        self.next_account_refresh = now.checked_add(delay).unwrap_or(now);
    }

    fn apply_ui_state(&mut self, state: &UiState, theme_override: Option<Theme>) {
        self.theme = theme_override.unwrap_or_else(|| state.theme.into());
        self.view = state.view.into();
        self.window_scope = state.window_scope.into();
        self.turns_default_visible = state.turns_visible;
        self.turns_temporarily_visible = false;
        self.models_visible = state.models_visible;
        self.api_long_context_multiplier = state.api_long_context_multiplier;
        self.table_columns = state.table_columns;
        self.task_list_mode = state.task_list_mode.into();
        self.expanded_task_threads.clear();
        self.task_source_filter = state.task_source_filter.into();
        self.reconcile_task_filter(true);
        if self.view != View::Overview {
            self.transition_to_tasks();
        }
    }

    fn apply_open_config(&mut self, config: OpenConfig, error: Option<String>) {
        self.open_config = config;
        self.open_config_error = error;
    }

    fn replace_history(&mut self, history: HistoryData) {
        let summary_inputs_unchanged =
            self.summary_cache.is_some() && summary_history_inputs_eq(&self.history, &history);
        if !self.summary_backfill_running {
            let query_now = Utc::now().max(self.snapshot.as_of);
            self.summary_backfill_pending = summary_history_backfill_needed(&history, query_now);
        }
        self.history = history;
        if !summary_inputs_unchanged {
            self.summary_cache = None;
        }
    }

    fn replace_recorder_health(&mut self, recorder_health: RecorderHealth) {
        self.recorder_health = recorder_health;
    }

    fn set_open_notice(&mut self, message: impl Into<String>, tone: OpenNoticeTone) {
        self.open_notice = Some(OpenNotice {
            message: message.into(),
            tone,
            created_at: Instant::now(),
        });
    }

    fn expire_open_notice_at(&mut self, now: Instant) -> bool {
        let expired = self.open_notice.as_ref().is_some_and(|notice| {
            now.saturating_duration_since(notice.created_at) >= OPEN_NOTICE_DURATION
        });
        if expired {
            self.open_notice = None;
        }
        expired
    }

    fn open_config_unavailable_reason(&self) -> Option<String> {
        if let Some(error) = self.open_config_error.as_deref() {
            return Some(format!("Open config is unavailable: {error}"));
        }
        if !self.open_config.enabled {
            return Some("Open is disabled in the user configuration".to_string());
        }
        None
    }

    fn target_open_unavailable_reason(
        &self,
        target: &ResumeTarget,
        probe_cwd: bool,
    ) -> Option<String> {
        if let Some(reason) = self.open_config_unavailable_reason() {
            return Some(reason);
        }
        if self.launching_threads.contains(&target.thread_id) {
            return Some("This task is already opening in Zellij".to_string());
        }
        let result = if probe_cwd {
            check_eligibility(target)
        } else {
            check_eligibility_without_cwd_probe(target)
        };
        result.err().map(|error| error.to_string())
    }

    fn open_control_available(&self) -> bool {
        if self.view != View::Overview || self.focus != Focus::Tasks || !self.shortcuts_active() {
            return false;
        }
        let Some(task) = self.selected_task_record() else {
            return false;
        };
        if self.open_config_unavailable_reason().is_some()
            || self.launching_threads.contains(&task.thread_id)
        {
            return false;
        }
        if self.open_panes.contains_key(&task.thread_id) {
            return true;
        }
        let target = ResumeTarget::from_task(task);
        self.target_open_unavailable_reason(&target, false)
            .is_none()
    }

    fn activate_open(&mut self) {
        if self.view != View::Overview || self.focus != Focus::Tasks {
            return;
        }
        let Some(task) = self.selected_task_record().cloned() else {
            self.set_open_notice("No task selected", OpenNoticeTone::Warning);
            return;
        };
        if let Some(reason) = self.open_config_unavailable_reason() {
            self.set_open_notice(reason, OpenNoticeTone::Error);
            return;
        }
        if self.launching_threads.contains(&task.thread_id) {
            self.set_open_notice(
                "This task is already opening in Zellij",
                OpenNoticeTone::Info,
            );
            return;
        }
        if self.zellij_environment
            && let Some(pane_id) = self.open_panes.get(&task.thread_id).cloned()
        {
            self.start_focus_request(task.thread_id, pane_id);
            return;
        }
        let target = ResumeTarget::from_task(&task);
        if let Some(reason) = self.target_open_unavailable_reason(&target, true) {
            let tone = if self.launching_threads.contains(&target.thread_id) {
                OpenNoticeTone::Info
            } else {
                OpenNoticeTone::Error
            };
            self.set_open_notice(reason, tone);
            return;
        }
        self.open_notice = None;
        self.resume_confirmation = Some(ResumeConfirmation {
            thread_id: target.thread_id,
            copy_error: None,
        });
    }

    fn start_focus_request(&mut self, thread_id: String, pane_id: PaneId) {
        if self.pending_resume.is_some() {
            self.set_open_notice(
                "Another Open request is waiting to launch",
                OpenNoticeTone::Warning,
            );
            return;
        }
        let pane_label = pane_id.as_str().to_string();
        self.pending_resume = Some(ResumeLaunchRequest::Focus {
            thread_id: thread_id.clone(),
            pane_id,
            codex_home: self.snapshot.codex_home.clone(),
        });
        self.launching_threads.insert(thread_id.clone());
        self.set_open_notice(
            format!(
                "Focusing {pane_label} for {}...",
                short_thread_id(&thread_id)
            ),
            OpenNoticeTone::Info,
        );
    }

    fn close_resume_confirmation(&mut self) {
        self.resume_confirmation = None;
        self.resume_confirmation_hitbox = None;
    }

    fn confirm_resume(&mut self) {
        if !self.zellij_environment {
            return;
        }
        if self
            .resume_confirmation_hitbox
            .is_none_or(|hitbox| hitbox.confirm.is_empty())
        {
            return;
        }
        let Some(thread_id) = self
            .resume_confirmation
            .as_ref()
            .map(|confirmation| confirmation.thread_id.clone())
        else {
            return;
        };
        let Some(task) = self
            .snapshot
            .tasks
            .iter()
            .find(|task| task.thread_id == thread_id)
            .cloned()
        else {
            self.close_resume_confirmation();
            self.set_open_notice(
                "The selected task is no longer available",
                OpenNoticeTone::Error,
            );
            return;
        };
        let target = ResumeTarget::from_task(&task);
        if let Some(pane_id) = self.open_panes.get(&thread_id).cloned() {
            self.close_resume_confirmation();
            self.start_focus_request(thread_id, pane_id);
            return;
        }
        if let Some(reason) = self.target_open_unavailable_reason(&target, true) {
            self.close_resume_confirmation();
            self.set_open_notice(reason, OpenNoticeTone::Error);
            return;
        }
        if self.pending_resume.is_some() {
            self.close_resume_confirmation();
            self.set_open_notice(
                "Another Open request is waiting to launch",
                OpenNoticeTone::Warning,
            );
            return;
        }

        let options = ZellijOptions {
            floating: self.open_config.zellij.floating,
            width_percent: self.open_config.zellij.width_percent,
            height_percent: self.open_config.zellij.height_percent,
            close_on_exit: self.open_config.zellij.close_on_exit,
        };
        self.pending_resume = Some(ResumeLaunchRequest::Create {
            target,
            codex_home: self.snapshot.codex_home.clone(),
            codex_bin: self.open_config.codex_bin.clone(),
            options,
        });
        self.launching_threads.insert(thread_id.clone());
        self.close_resume_confirmation();
        self.set_open_notice(
            format!("Opening {} in Zellij...", short_thread_id(&thread_id)),
            OpenNoticeTone::Info,
        );
    }

    fn request_resume_command_copy(&mut self) {
        if self
            .resume_confirmation_hitbox
            .is_none_or(|hitbox| hitbox.copy.is_empty())
        {
            return;
        }
        let Some(thread_id) = self
            .resume_confirmation
            .as_ref()
            .map(|confirmation| confirmation.thread_id.clone())
        else {
            return;
        };
        if self.pending_clipboard.is_some() {
            self.set_resume_copy_error(&thread_id, "Another copy request is still pending");
            return;
        }
        let Some(task) = self
            .snapshot
            .tasks
            .iter()
            .find(|task| task.thread_id == thread_id)
            .cloned()
        else {
            self.set_resume_copy_error(&thread_id, "The selected task is no longer available");
            return;
        };
        let target = ResumeTarget::from_task(&task);
        if let Some(reason) = self.target_open_unavailable_reason(&target, true) {
            self.set_resume_copy_error(&thread_id, reason);
            return;
        }
        let result = LaunchContext::capture(
            self.snapshot.codex_home.clone(),
            self.open_config.codex_bin.clone(),
        )
        .and_then(|context| prepare_resume_copy_command(&target, &context))
        .and_then(|plan| render_resume_command(&plan));
        match result {
            Ok(text) if text.len() <= MAX_CLIPBOARD_TEXT_BYTES => {
                if let Some(confirmation) = self.resume_confirmation.as_mut() {
                    confirmation.copy_error = None;
                }
                self.pending_clipboard = Some(ClipboardRequest { thread_id, text });
            }
            Ok(_) => self.set_resume_copy_error(
                &thread_id,
                format!(
                    "Resume command exceeds the {} KiB clipboard limit",
                    MAX_CLIPBOARD_TEXT_BYTES / 1024
                ),
            ),
            Err(error) => self.set_resume_copy_error(&thread_id, error.to_string()),
        }
    }

    fn set_resume_copy_error(&mut self, thread_id: &str, message: impl Into<String>) {
        if let Some(confirmation) = self
            .resume_confirmation
            .as_mut()
            .filter(|confirmation| confirmation.thread_id == thread_id)
        {
            confirmation.copy_error = Some(message.into());
        }
    }

    fn apply_clipboard_result(&mut self, request: ClipboardRequest, result: io::Result<()>) {
        match result {
            Ok(()) => {
                if self
                    .resume_confirmation
                    .as_ref()
                    .is_some_and(|confirmation| confirmation.thread_id == request.thread_id)
                {
                    self.close_resume_confirmation();
                }
                self.set_open_notice(
                    format!(
                        "Resume command sent to terminal clipboard for {}",
                        short_thread_id(&request.thread_id)
                    ),
                    OpenNoticeTone::Success,
                );
            }
            Err(error) => self.set_resume_copy_error(
                &request.thread_id,
                format!("Could not send resume command to terminal clipboard: {error}"),
            ),
        }
    }

    fn apply_resume_completion(&mut self, completion: ResumeLaunchCompletion) {
        self.launching_threads.remove(&completion.thread_id);
        match completion.result {
            Ok(ResumeLaunchOutcome::Created(pane_id)) => {
                let pane_label = pane_id.as_str().to_string();
                self.open_panes
                    .insert(completion.thread_id.clone(), pane_id);
                self.set_open_notice(
                    format!(
                        "Opened {} in {pane_label}",
                        short_thread_id(&completion.thread_id)
                    ),
                    OpenNoticeTone::Success,
                );
            }
            Ok(ResumeLaunchOutcome::Focused(pane_id)) => {
                let pane_label = pane_id.as_str().to_string();
                self.open_panes
                    .insert(completion.thread_id.clone(), pane_id);
                self.set_open_notice(
                    format!(
                        "Focused {pane_label} for {}",
                        short_thread_id(&completion.thread_id)
                    ),
                    OpenNoticeTone::Success,
                );
            }
            Ok(ResumeLaunchOutcome::Missing(pane_id)) => {
                if self.open_panes.get(&completion.thread_id) == Some(&pane_id) {
                    self.open_panes.remove(&completion.thread_id);
                }
                self.set_open_notice(
                    "The previous pane was closed; press O again to resume in a new terminal",
                    OpenNoticeTone::Warning,
                );
            }
            Err(error) => {
                self.set_open_notice(format!("Open failed: {error}"), OpenNoticeTone::Error);
            }
        }
    }

    fn ui_state(&self) -> UiState {
        UiState {
            theme: self.theme.into(),
            view: self.view.into(),
            window_scope: self.window_scope.into(),
            turns_visible: self.turns_default_visible,
            models_visible: self.models_visible,
            api_long_context_multiplier: self.api_long_context_multiplier,
            table_columns: self.table_columns,
            task_list_mode: self.task_list_mode.into(),
            task_source_filter: self.task_source_filter.into(),
            ..UiState::default()
        }
    }

    fn task_matches_filter(&self, task: &TaskRecord) -> bool {
        let query = self.task_search.to_lowercase();
        self.task_matches_filter_query(task, &query)
    }

    fn task_matches_filter_query(&self, task: &TaskRecord, query: &str) -> bool {
        if !self.task_source_filter.matches(task.source.as_deref()) {
            return false;
        }
        query.is_empty()
            || task.title.to_lowercase().contains(query)
            || task_project_name(task).is_some_and(|project| project.to_lowercase().contains(query))
    }

    fn filtered_task_rows(&self) -> Vec<TaskListRow> {
        self.filtered_task_rows_with_expanded(Some(&self.expanded_task_threads))
    }

    fn filtered_task_rows_with_expanded(
        &self,
        expanded_task_threads: Option<&HashSet<String>>,
    ) -> Vec<TaskListRow> {
        let query = self.task_search.to_lowercase();
        let filtered = self
            .snapshot
            .tasks
            .iter()
            .enumerate()
            .filter_map(|(index, task)| {
                self.task_matches_filter_query(task, &query)
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        if self.task_list_mode == TaskListMode::Flat {
            return filtered
                .into_iter()
                .map(|index| TaskListRow {
                    index,
                    prefix: String::new(),
                    depth: 0,
                    has_children: false,
                    collapsed: false,
                    hidden_descendants: Vec::new(),
                })
                .collect();
        }

        let visible_by_thread = filtered
            .iter()
            .filter_map(|index| {
                self.snapshot
                    .tasks
                    .get(*index)
                    .map(|task| (task.thread_id.as_str(), *index))
            })
            .collect::<HashMap<_, _>>();
        let mut parent_by_index = vec![None; self.snapshot.tasks.len()];
        for &child_index in &filtered {
            let Some(child) = self.snapshot.tasks.get(child_index) else {
                continue;
            };
            if !child
                .source
                .as_deref()
                .is_some_and(|source| source.eq_ignore_ascii_case("subagent"))
            {
                continue;
            }
            let Some(parent_index) = child
                .parent_thread_id
                .as_deref()
                .and_then(|thread_id| visible_by_thread.get(thread_id))
                .copied()
            else {
                continue;
            };
            if parent_index == child_index
                || task_parent_edge_would_cycle(child_index, parent_index, &parent_by_index)
            {
                continue;
            }
            parent_by_index[child_index] = Some(parent_index);
        }

        let mut children = vec![Vec::new(); self.snapshot.tasks.len()];
        let mut roots = Vec::new();
        for &index in &filtered {
            if let Some(parent) = parent_by_index[index] {
                children[parent].push(index);
            } else {
                roots.push(index);
            }
        }

        let mut subtree_ranks = vec![None; self.snapshot.tasks.len()];
        for &index in &filtered {
            task_subtree_rank(index, &children, &mut subtree_ranks);
        }
        for siblings in &mut children {
            siblings.sort_by_key(|index| (subtree_ranks[*index].unwrap_or(*index), *index));
        }
        roots.sort_by_key(|index| (subtree_ranks[*index].unwrap_or(*index), *index));

        let mut rows = Vec::with_capacity(filtered.len());
        for root in roots {
            append_task_tree_rows(
                root,
                &children,
                &self.snapshot.tasks,
                expanded_task_threads,
                &mut Vec::new(),
                &mut rows,
            );
        }
        rows
    }

    fn filtered_task_indices(&self) -> Vec<usize> {
        self.filtered_task_rows()
            .into_iter()
            .map(|row| row.index)
            .collect()
    }

    fn raw_selected_thread_id(&self) -> Option<&str> {
        self.snapshot
            .tasks
            .get(self.selected_task)
            .map(|task| task.thread_id.as_str())
    }

    fn selected_thread_id(&self) -> Option<&str> {
        let task = self.snapshot.tasks.get(self.selected_task)?;
        self.filtered_task_indices()
            .contains(&self.selected_task)
            .then_some(task.thread_id.as_str())
    }

    fn selected_task_record(&self) -> Option<&TaskRecord> {
        let task = self.snapshot.tasks.get(self.selected_task)?;
        self.filtered_task_indices()
            .contains(&self.selected_task)
            .then_some(task)
    }

    fn nearest_visible_task_ancestor(
        &self,
        index: usize,
        visible: &HashSet<usize>,
    ) -> Option<usize> {
        let by_thread = self
            .snapshot
            .tasks
            .iter()
            .enumerate()
            .map(|(index, task)| (task.thread_id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut cursor = index;
        let mut seen = HashSet::from([index]);
        loop {
            let task = self.snapshot.tasks.get(cursor)?;
            if !task
                .source
                .as_deref()
                .is_some_and(|source| source.eq_ignore_ascii_case("subagent"))
            {
                return None;
            }
            let parent = task
                .parent_thread_id
                .as_deref()
                .and_then(|thread_id| by_thread.get(thread_id))
                .copied()?;
            if visible.contains(&parent) {
                return Some(parent);
            }
            if !seen.insert(parent) {
                return None;
            }
            cursor = parent;
        }
    }

    fn selected_turn_record(&self) -> Option<&TurnRecord> {
        let index = *self.filtered_turn_indices().get(self.selected_turn)?;
        self.snapshot.turns.get(index)
    }

    fn raw_turn_indices(&self) -> Vec<usize> {
        let Some(thread_id) = self.selected_thread_id() else {
            return Vec::new();
        };
        self.snapshot
            .turns
            .iter()
            .enumerate()
            .filter_map(|(index, turn)| (turn.thread_id == thread_id).then_some(index))
            .collect()
    }

    fn turn_matches_filter_query(&self, turn: &TurnRecord, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        turn.turn_id.to_lowercase().contains(query)
            || turn
                .model
                .as_deref()
                .is_some_and(|model| model.to_lowercase().contains(query))
            || turn
                .reasoning_effort
                .as_deref()
                .is_some_and(|effort| effort.to_lowercase().contains(query))
            || turn
                .message_preview
                .as_deref()
                .is_some_and(|message| message.to_lowercase().contains(query))
            || turn.status.label().to_lowercase().contains(query)
            || (turn.is_fast() && "fast".contains(query))
    }

    fn filtered_turn_indices(&self) -> Vec<usize> {
        let query = self.turn_search.to_lowercase();
        self.raw_turn_indices()
            .into_iter()
            .filter(|index| {
                self.snapshot
                    .turns
                    .get(*index)
                    .is_some_and(|turn| self.turn_matches_filter_query(turn, &query))
            })
            .collect()
    }

    fn selected_task_turn_count(&self) -> usize {
        self.filtered_turn_indices().len()
    }

    fn selected_task_raw_turn_count(&self) -> usize {
        self.raw_turn_indices().len()
    }

    fn turns_visible(&self) -> bool {
        self.turns_default_visible || self.turns_temporarily_visible
    }

    fn shortcuts_active(&self) -> bool {
        !self.focus.is_search()
            && !self.quit_confirmation_visible
            && self.resume_confirmation.is_none()
    }

    fn close_temporary_turns(&mut self) {
        self.turns_temporarily_visible = false;
        if matches!(self.focus, Focus::Turns | Focus::TurnSearch) {
            self.transition_to_tasks();
        }
    }

    fn toggle_turns_default_visibility(&mut self) {
        let was_visible = self.turns_visible();
        self.turns_default_visible = !self.turns_default_visible;
        self.turns_temporarily_visible = false;
        if !was_visible && self.turns_visible() {
            self.task_reveal_pending = true;
        }
        if !self.turns_default_visible && matches!(self.focus, Focus::Turns | Focus::TurnSearch) {
            self.transition_to_tasks();
        }
    }

    fn toggle_models_visibility(&mut self) {
        self.models_visible = !self.models_visible;
        self.task_reveal_pending = true;
        self.turn_reveal_pending = true;
    }

    fn toggle_api_long_context_multiplier(&mut self) {
        self.api_long_context_multiplier = !self.api_long_context_multiplier;
        self.summary_selected_id = None;
        self.summary_offset = 0;
        self.clear_trend_inspection();
    }

    fn selected_setting_item(&self) -> SettingItem {
        SettingItem::ALL[self.selected_setting.min(SettingItem::ALL.len() - 1)]
    }

    fn select_setting(&mut self, item: SettingItem) {
        self.selected_setting = item.index();
    }

    fn move_setting_selection(&mut self, forward: bool) {
        self.selected_setting = if forward {
            (self.selected_setting + 1).min(SettingItem::ALL.len() - 1)
        } else {
            self.selected_setting.saturating_sub(1)
        };
    }

    fn toggle_setting(&mut self, item: SettingItem) {
        self.select_setting(item);
        match item {
            SettingItem::Theme => self.toggle_theme(),
            SettingItem::Turns => self.toggle_turns_default_visibility(),
            SettingItem::Models => self.toggle_models_visibility(),
            SettingItem::ApiLongContext => self.toggle_api_long_context_multiplier(),
            SettingItem::Tokens => self.table_columns.tokens = !self.table_columns.tokens,
            SettingItem::TokenShare => {
                self.table_columns.token_share = !self.table_columns.token_share;
            }
            SettingItem::EstimatedQuota => {
                self.table_columns.estimated_quota = !self.table_columns.estimated_quota;
            }
            SettingItem::ApiEquivalent => {
                self.table_columns.api_equivalent = !self.table_columns.api_equivalent;
            }
        }
        self.task_reveal_pending = true;
        self.turn_reveal_pending = true;
    }

    fn setting_value(&self, item: SettingItem) -> &'static str {
        match item {
            SettingItem::Theme => match self.theme {
                Theme::Dark => "Dark",
                Theme::Light => "Light",
            },
            SettingItem::Turns => on_off(self.turns_default_visible),
            SettingItem::Models => on_off(self.models_visible),
            SettingItem::ApiLongContext => on_off(self.api_long_context_multiplier),
            SettingItem::Tokens => on_off(self.table_columns.tokens),
            SettingItem::TokenShare => on_off(self.table_columns.token_share),
            SettingItem::EstimatedQuota => on_off(self.table_columns.estimated_quota),
            SettingItem::ApiEquivalent => on_off(self.table_columns.api_equivalent),
        }
    }

    fn reset_turn_selection(&mut self) {
        self.selected_turn = 0;
        self.turn_offset = 0;
        self.turn_reveal_pending = false;
    }

    fn reconcile_task_filter(&mut self, reset_viewport: bool) {
        let filtered = self.filtered_task_indices();
        if reset_viewport {
            self.task_table_offset = 0;
        }
        if filtered.is_empty() {
            self.task_table_offset = 0;
            self.reset_turn_selection();
            self.task_reveal_pending = false;
            self.close_temporary_turns();
            return;
        }

        let visible = filtered.iter().copied().collect::<HashSet<_>>();
        let target = if visible.contains(&self.selected_task) {
            self.selected_task
        } else {
            self.nearest_visible_task_ancestor(self.selected_task, &visible)
                .unwrap_or(filtered[0])
        };
        let selection_changed = target != self.selected_task;
        if selection_changed {
            self.selected_task = target;
            self.task_table_offset = 0;
            self.reset_turn_selection();
            self.close_temporary_turns();
        }
        if selection_changed || reset_viewport {
            self.task_reveal_pending = true;
        }
    }

    fn set_task_source_filter(&mut self, filter: TaskSourceFilter) {
        self.transition_to_tasks();
        self.task_search_before_edit.clone_from(&self.task_search);
        self.clear_task_search_restore();
        if self.task_source_filter == filter {
            return;
        }
        self.task_source_filter = filter;
        self.reconcile_task_filter(true);
    }

    fn begin_task_search(&mut self) {
        if self.focus != Focus::TaskSearch {
            self.transition_to_tasks();
            self.task_search_before_edit.clone_from(&self.task_search);
            self.task_search_cursor = self.task_search.chars().count();
            self.task_search_restore_thread_id = self.selected_thread_id().map(str::to_string);
            self.task_search_restore_turn_id =
                self.selected_turn_record().map(|turn| turn.turn_id.clone());
            self.task_search_restore_task_offset = self.task_table_offset;
            self.task_search_restore_turn_offset = self.turn_offset;
            self.focus = Focus::TaskSearch;
        }
    }

    fn accept_task_search(&mut self) {
        self.task_search_before_edit.clone_from(&self.task_search);
        self.clear_task_search_restore();
        self.focus = Focus::Tasks;
        if !self.turns_default_visible {
            self.turns_temporarily_visible = false;
        }
    }

    fn cancel_task_search(&mut self) {
        if self.focus == Focus::TaskSearch {
            let restore_thread_id = self.task_search_restore_thread_id.take();
            let restore_turn_id = self.task_search_restore_turn_id.take();
            let restore_task_offset = self.task_search_restore_task_offset;
            let restore_turn_offset = self.task_search_restore_turn_offset;
            self.task_search.clone_from(&self.task_search_before_edit);
            self.task_search_cursor = self.task_search.chars().count();
            self.focus = Focus::Tasks;
            let visible_tasks = self.filtered_task_indices();
            let restored_task = restore_thread_id.as_deref().and_then(|thread_id| {
                visible_tasks
                    .iter()
                    .copied()
                    .find(|index| self.snapshot.tasks[*index].thread_id == thread_id)
            });
            if let Some(task_index) = restored_task {
                self.selected_task = task_index;
                let filtered_turns = self.filtered_turn_indices();
                let turn_count = filtered_turns.len();
                self.selected_turn = restore_turn_id
                    .as_deref()
                    .and_then(|turn_id| {
                        filtered_turns
                            .iter()
                            .position(|index| self.snapshot.turns[*index].turn_id == turn_id)
                    })
                    .unwrap_or(0)
                    .min(turn_count.saturating_sub(1));
                self.task_table_offset = restore_task_offset;
                self.turn_offset = if turn_count == 0 {
                    0
                } else {
                    restore_turn_offset.min(turn_count - 1)
                };
                self.task_reveal_pending = false;
                self.turn_reveal_pending = false;
            } else {
                self.reconcile_task_filter(true);
            }
        }
    }

    fn clear_task_search_restore(&mut self) {
        self.task_search_restore_thread_id = None;
        self.task_search_restore_turn_id = None;
    }

    fn insert_task_search(&mut self, character: char) {
        if !character.is_control() {
            let byte_index = byte_index_at_char(&self.task_search, self.task_search_cursor);
            self.task_search.insert(byte_index, character);
            self.task_search_cursor += 1;
            self.reconcile_task_filter(true);
        }
    }

    fn backspace_task_search(&mut self) {
        if self.task_search_cursor == 0 {
            return;
        }
        let start = byte_index_at_char(&self.task_search, self.task_search_cursor - 1);
        let end = byte_index_at_char(&self.task_search, self.task_search_cursor);
        self.task_search.replace_range(start..end, "");
        self.task_search_cursor -= 1;
        self.reconcile_task_filter(true);
    }

    fn delete_task_search(&mut self) {
        if self.task_search_cursor >= self.task_search.chars().count() {
            return;
        }
        let start = byte_index_at_char(&self.task_search, self.task_search_cursor);
        let end = byte_index_at_char(&self.task_search, self.task_search_cursor + 1);
        self.task_search.replace_range(start..end, "");
        self.reconcile_task_filter(true);
    }

    fn clear_task_search(&mut self) {
        self.task_search.clear();
        self.task_search_cursor = 0;
        self.task_search_before_edit.clear();
        self.clear_task_search_restore();
        self.reconcile_task_filter(true);
    }

    fn reconcile_turn_filter(&mut self, reset_viewport: bool, preferred_turn_id: Option<&str>) {
        let filtered = self.filtered_turn_indices();
        if reset_viewport {
            self.turn_offset = 0;
        }
        if filtered.is_empty() {
            self.selected_turn = 0;
            self.turn_offset = 0;
            self.turn_reveal_pending = false;
            return;
        }
        self.selected_turn = preferred_turn_id
            .and_then(|turn_id| {
                filtered
                    .iter()
                    .position(|index| self.snapshot.turns[*index].turn_id == turn_id)
            })
            .unwrap_or_else(|| self.selected_turn.min(filtered.len() - 1));
        self.turn_offset = self.turn_offset.min(filtered.len() - 1);
        if reset_viewport {
            self.turn_reveal_pending = true;
        }
    }

    fn begin_turn_search(&mut self) {
        if self.focus != Focus::TurnSearch && self.turns_visible() {
            self.turn_search_before_edit.clone_from(&self.turn_search);
            self.turn_search_cursor = self.turn_search.chars().count();
            self.turn_search_restore_turn_id =
                self.selected_turn_record().map(|turn| turn.turn_id.clone());
            self.turn_search_restore_offset = self.turn_offset;
            self.turns_temporarily_visible = !self.turns_default_visible;
            self.focus = Focus::TurnSearch;
        }
    }

    fn accept_turn_search(&mut self) {
        self.turn_search_before_edit.clone_from(&self.turn_search);
        self.turn_search_restore_turn_id = None;
        self.focus = Focus::Turns;
    }

    fn cancel_turn_search(&mut self) {
        if self.focus != Focus::TurnSearch {
            return;
        }
        let restore_turn_id = self.turn_search_restore_turn_id.take();
        let restore_offset = self.turn_search_restore_offset;
        self.turn_search.clone_from(&self.turn_search_before_edit);
        self.turn_search_cursor = self.turn_search.chars().count();
        self.focus = Focus::Turns;
        self.reconcile_turn_filter(false, restore_turn_id.as_deref());
        let turn_count = self.selected_task_turn_count();
        self.turn_offset = if turn_count == 0 {
            0
        } else {
            restore_offset.min(turn_count - 1)
        };
        self.turn_reveal_pending = false;
    }

    fn insert_turn_search(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        let selected = self.selected_turn_record().map(|turn| turn.turn_id.clone());
        let byte_index = byte_index_at_char(&self.turn_search, self.turn_search_cursor);
        self.turn_search.insert(byte_index, character);
        self.turn_search_cursor += 1;
        self.reconcile_turn_filter(true, selected.as_deref());
    }

    fn backspace_turn_search(&mut self) {
        if self.turn_search_cursor == 0 {
            return;
        }
        let selected = self.selected_turn_record().map(|turn| turn.turn_id.clone());
        let start = byte_index_at_char(&self.turn_search, self.turn_search_cursor - 1);
        let end = byte_index_at_char(&self.turn_search, self.turn_search_cursor);
        self.turn_search.replace_range(start..end, "");
        self.turn_search_cursor -= 1;
        self.reconcile_turn_filter(true, selected.as_deref());
    }

    fn delete_turn_search(&mut self) {
        if self.turn_search_cursor >= self.turn_search.chars().count() {
            return;
        }
        let selected = self.selected_turn_record().map(|turn| turn.turn_id.clone());
        let start = byte_index_at_char(&self.turn_search, self.turn_search_cursor);
        let end = byte_index_at_char(&self.turn_search, self.turn_search_cursor + 1);
        self.turn_search.replace_range(start..end, "");
        self.reconcile_turn_filter(true, selected.as_deref());
    }

    fn clear_turn_search(&mut self) {
        let selected = self.selected_turn_record().map(|turn| turn.turn_id.clone());
        self.turn_search.clear();
        self.clear_turn_search_edit_state();
        self.reconcile_turn_filter(true, selected.as_deref());
    }

    fn clear_turn_search_edit_state(&mut self) {
        self.turn_search_cursor = 0;
        self.turn_search_before_edit.clear();
        self.turn_search_restore_turn_id = None;
        self.turn_search_restore_offset = 0;
    }

    fn accept_active_search(&mut self) {
        match self.focus {
            Focus::TaskSearch => self.accept_task_search(),
            Focus::TurnSearch => self.accept_turn_search(),
            Focus::Tasks | Focus::Turns => {}
        }
    }

    fn cycle_task_source_filter(&mut self, forward: bool) {
        let index = self.task_source_filter.index();
        let next = if forward {
            (index + 1) % TaskSourceFilter::ALL.len()
        } else {
            index
                .checked_sub(1)
                .unwrap_or(TaskSourceFilter::ALL.len() - 1)
        };
        self.set_task_source_filter(TaskSourceFilter::ALL[next]);
    }

    fn toggle_task_list_mode(&mut self) {
        self.task_list_mode = self.task_list_mode.toggle();
        self.reconcile_task_filter(true);
    }

    fn set_task_collapsed(&mut self, index: usize, collapsed: bool) -> bool {
        if self.task_list_mode != TaskListMode::Tree
            || !self
                .filtered_task_rows()
                .iter()
                .any(|row| row.index == index && row.has_children)
        {
            return false;
        }
        let Some(thread_id) = self
            .snapshot
            .tasks
            .get(index)
            .map(|task| task.thread_id.clone())
        else {
            return false;
        };
        let changed = if collapsed {
            self.expanded_task_threads.remove(&thread_id)
        } else {
            self.expanded_task_threads.insert(thread_id)
        };
        if changed {
            self.reconcile_task_filter(false);
            self.task_reveal_pending = true;
        }
        changed
    }

    fn set_selected_task_collapsed(&mut self, collapsed: bool) -> bool {
        self.set_task_collapsed(self.selected_task, collapsed)
    }

    fn filtered_collapsible_task_threads(&self) -> Vec<String> {
        if self.task_list_mode != TaskListMode::Tree {
            return Vec::new();
        }
        self.filtered_task_rows_with_expanded(None)
            .into_iter()
            .filter(|row| row.has_children)
            .filter_map(|row| {
                self.snapshot
                    .tasks
                    .get(row.index)
                    .map(|task| task.thread_id.clone())
            })
            .collect()
    }

    fn all_filtered_task_threads_collapsed(&self) -> bool {
        let collapsible = self.filtered_collapsible_task_threads();
        !collapsible.is_empty()
            && collapsible
                .iter()
                .all(|thread_id| !self.expanded_task_threads.contains(thread_id))
    }

    fn toggle_all_task_threads(&mut self) -> bool {
        let collapsible = self.filtered_collapsible_task_threads();
        if collapsible.is_empty() {
            return false;
        }
        let expand = collapsible
            .iter()
            .all(|thread_id| !self.expanded_task_threads.contains(thread_id));
        let mut changed = false;
        for thread_id in collapsible {
            changed |= if expand {
                self.expanded_task_threads.insert(thread_id)
            } else {
                self.expanded_task_threads.remove(&thread_id)
            };
        }
        if changed {
            self.reconcile_task_filter(false);
            self.task_reveal_pending = true;
        }
        changed
    }

    fn replace(&mut self, result: CollectionResult, refreshed_account: bool) {
        if refreshed_account {
            self.schedule_next_account_refresh(&result, Instant::now());
        }
        let next_snapshot_as_of = result.snapshot.as_of;
        let summary_inputs_unchanged = self.summary_cache.as_ref().is_some_and(|cache| {
            summary_snapshot_inputs_eq(
                &self.snapshot,
                &result.snapshot,
                cache,
                self.summary_range,
                next_snapshot_as_of.max(self.snapshot.as_of),
            )
        });
        let filtered = self.filtered_task_indices();
        let task_viewport_was_at_top = self.task_table_offset == 0;
        let selected_position = filtered
            .iter()
            .position(|index| *index == self.selected_task);
        let selected_task_was_visible = self.task_table_hitbox.is_some_and(|hitbox| {
            selected_position.is_some_and(|position| {
                position >= hitbox.offset
                    && position < hitbox.offset.saturating_add(hitbox.capacity)
            })
        });
        let task_viewport_thread_id = filtered
            .get(self.task_table_offset)
            .and_then(|index| self.snapshot.tasks.get(*index))
            .map(|task| task.thread_id.clone());
        let selected = self.raw_selected_thread_id().map(str::to_string);
        let selected_turn_id = self.selected_turn_record().map(|turn| turn.turn_id.clone());
        let selected_turn_was_visible = self.turn_table_hitbox.is_some_and(|hitbox| {
            self.selected_turn >= hitbox.offset
                && self.selected_turn < hitbox.offset.saturating_add(hitbox.capacity)
        });
        let turn_viewport_id = self
            .filtered_turn_indices()
            .get(self.turn_offset)
            .and_then(|index| self.snapshot.turns.get(*index))
            .map(|turn| turn.turn_id.clone());
        self.snapshot = result.snapshot;
        self.account = result.account;
        if summary_inputs_unchanged {
            if let Some(cache) = self.summary_cache.as_mut()
                && cache.range == self.summary_range
            {
                // `snapshot_as_of` guards the cache against source refreshes.
                // When every Summary-relevant snapshot input is unchanged, move
                // that guard forward without rebuilding the 30-day aggregate.
                cache.snapshot_as_of = next_snapshot_as_of;
            }
        } else {
            self.summary_cache = None;
        }
        let existing_threads = self
            .snapshot
            .tasks
            .iter()
            .map(|task| task.thread_id.as_str())
            .collect::<HashSet<_>>();
        self.expanded_task_threads
            .retain(|thread_id| existing_threads.contains(thread_id.as_str()));
        self.task_table_hitbox = None;
        self.turn_table_hitbox = None;
        self.task_controls_hitbox = None;
        self.task_tree_marker_hitboxes.clear();
        self.turn_controls_hitbox = None;
        self.window_controls_hitbox = None;
        self.settings_controls_hitbox = None;
        self.summary_controls_hitbox = None;
        self.summary_table_hitbox = None;
        self.summary_tree_marker_hitboxes.clear();
        self.summary_bar_hitboxes.clear();
        self.summary_daily_hitbox = None;
        self.summary_daily_dragging = false;
        self.summary_scrollbar_hitbox = None;
        self.view_tabs_hitbox = None;
        self.task_scrollbar_hitbox = None;
        self.turn_scrollbar_hitbox = None;
        self.scroll_drag = None;
        self.resume_confirmation_hitbox = None;
        let restored_task = selected.as_deref().and_then(|thread_id| {
            self.snapshot
                .tasks
                .iter()
                .position(|task| task.thread_id == thread_id)
        });
        let task_was_restored = restored_task.is_some();
        self.selected_task = restored_task
            .unwrap_or(0)
            .min(self.snapshot.tasks.len().saturating_sub(1));
        if !task_was_restored {
            self.task_table_offset = 0;
            self.task_reveal_pending = false;
        }
        let filtered_turns = self.filtered_turn_indices();
        let turn_count = filtered_turns.len();
        let restored_turn = task_was_restored
            .then_some(selected_turn_id.as_deref())
            .flatten()
            .and_then(|turn_id| {
                filtered_turns
                    .iter()
                    .position(|index| self.snapshot.turns[*index].turn_id == turn_id)
            });
        let turn_was_restored = restored_turn.is_some();
        self.selected_turn = restored_turn.unwrap_or(0).min(turn_count.saturating_sub(1));
        self.turn_offset = if turn_was_restored {
            self.turn_offset.min(turn_count.saturating_sub(1))
        } else {
            0
        };
        self.reconcile_task_filter(false);

        if task_was_restored && !self.task_reveal_pending {
            // Offset zero is a live anchor; restoring the old first row would hide new tasks.
            if task_viewport_was_at_top {
                self.task_table_offset = 0;
            } else {
                let restored_viewport = task_viewport_thread_id.as_deref().and_then(|thread_id| {
                    let task_index = self
                        .snapshot
                        .tasks
                        .iter()
                        .position(|task| task.thread_id == thread_id)?;
                    self.filtered_task_indices()
                        .iter()
                        .position(|index| *index == task_index)
                });
                if let Some(position) = restored_viewport {
                    self.task_table_offset = position;
                }
                if selected_task_was_visible {
                    self.task_reveal_pending = true;
                }
            }
        }

        let selected_thread_was_restored = self.selected_thread_id() == selected.as_deref();
        if turn_was_restored && selected_thread_was_restored && !self.turn_reveal_pending {
            let restored_viewport = turn_viewport_id.as_deref().and_then(|turn_id| {
                self.filtered_turn_indices()
                    .iter()
                    .position(|index| self.snapshot.turns[*index].turn_id == turn_id)
            });
            if let Some(position) = restored_viewport {
                self.turn_offset = position;
            }
            if selected_turn_was_visible {
                self.turn_reveal_pending = true;
            }
        }
        if matches!(self.focus, Focus::Turns | Focus::TurnSearch)
            && self.selected_task_raw_turn_count() == 0
        {
            self.close_temporary_turns();
        }
        self.worker_running = false;
        self.last_local_refresh = Instant::now();
    }

    fn finish_unchanged_refresh(&mut self) {
        self.worker_running = false;
        self.last_local_refresh = Instant::now();
    }

    fn select_next(&mut self) {
        let filtered = self.filtered_task_indices();
        let Some(position) = filtered
            .iter()
            .position(|index| *index == self.selected_task)
        else {
            return;
        };
        let target = filtered[(position + 1).min(filtered.len() - 1)];
        self.select_task(target, true);
    }

    fn select_previous(&mut self) {
        let filtered = self.filtered_task_indices();
        let Some(position) = filtered
            .iter()
            .position(|index| *index == self.selected_task)
        else {
            return;
        };
        self.select_task(filtered[position.saturating_sub(1)], true);
    }

    fn select_first_task(&mut self) {
        if let Some(index) = self.filtered_task_indices().first().copied() {
            self.select_task(index, true);
        }
    }

    fn select_last_task(&mut self) {
        if let Some(index) = self.filtered_task_indices().last().copied() {
            self.select_task(index, true);
        }
    }

    fn select_task(&mut self, index: usize, reveal: bool) -> bool {
        let filtered = self.filtered_task_indices();
        let Some(position) = filtered.iter().position(|candidate| *candidate == index) else {
            return false;
        };
        if self.selected_task != index {
            self.selected_task = index;
            self.reset_turn_selection();
            if !self.turns_default_visible {
                self.close_temporary_turns();
            }
        }
        if reveal {
            if let Some(hitbox) = self.task_table_hitbox {
                self.task_table_offset = reveal_offset(
                    self.task_table_offset,
                    position,
                    filtered.len(),
                    hitbox.capacity,
                );
                self.task_reveal_pending = false;
            } else {
                self.task_reveal_pending = true;
            }
        } else {
            self.task_reveal_pending = false;
        }
        true
    }

    fn scroll_tasks(&mut self, down: bool, lines: usize) {
        let Some(hitbox) = self.task_table_hitbox else {
            return;
        };
        self.task_reveal_pending = false;
        self.task_table_offset = scroll_offset(
            self.task_table_offset,
            self.filtered_task_indices().len(),
            hitbox.capacity,
            down,
            lines,
        );
    }

    fn select_next_turn(&mut self) {
        let turn_count = self.selected_task_turn_count();
        if turn_count > 0 {
            self.select_turn((self.selected_turn + 1).min(turn_count - 1), true);
        }
    }

    fn select_previous_turn(&mut self) {
        self.select_turn(self.selected_turn.saturating_sub(1), true);
    }

    fn select_first_turn(&mut self) {
        self.select_turn(0, true);
    }

    fn select_last_turn(&mut self) {
        self.select_turn(self.selected_task_turn_count().saturating_sub(1), true);
    }

    fn select_turn(&mut self, index: usize, reveal: bool) -> bool {
        let turn_count = self.selected_task_turn_count();
        if index >= turn_count {
            return false;
        }
        self.selected_turn = index;
        if reveal {
            if let Some(hitbox) = self.turn_table_hitbox {
                self.turn_offset =
                    reveal_offset(self.turn_offset, index, turn_count, hitbox.capacity);
                self.turn_reveal_pending = false;
            } else {
                self.turn_reveal_pending = true;
            }
        } else {
            self.turn_reveal_pending = false;
        }
        true
    }

    fn focus_turns(&mut self) {
        if self.view == View::Overview && self.selected_task_raw_turn_count() > 0 {
            let was_visible = self.turns_visible();
            self.turns_temporarily_visible = !self.turns_default_visible;
            if !was_visible && self.turns_visible() {
                self.task_reveal_pending = true;
            }
            self.focus = Focus::Turns;
            self.select_turn(self.selected_turn, true);
        }
    }

    fn transition_to_tasks(&mut self) {
        if matches!(self.focus, Focus::Turns | Focus::TurnSearch) {
            if self.turn_search.is_empty() {
                self.clear_turn_search_edit_state();
            } else {
                self.clear_turn_search();
            }
        }
        self.focus = Focus::Tasks;
        if !self.turns_default_visible {
            self.turns_temporarily_visible = false;
        }
    }

    fn focus_tasks(&mut self) {
        self.transition_to_tasks();
        self.select_task(self.selected_task, true);
    }

    fn select_next_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_next(),
            Focus::Turns => self.select_next_turn(),
            Focus::TaskSearch | Focus::TurnSearch => {}
        }
    }

    fn select_previous_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_previous(),
            Focus::Turns => self.select_previous_turn(),
            Focus::TaskSearch | Focus::TurnSearch => {}
        }
    }

    fn select_first_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_first_task(),
            Focus::Turns => self.select_first_turn(),
            Focus::TaskSearch | Focus::TurnSearch => {}
        }
    }

    fn select_last_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_last_task(),
            Focus::Turns => self.select_last_turn(),
            Focus::TaskSearch | Focus::TurnSearch => {}
        }
    }

    fn scroll_turns(&mut self, down: bool, lines: usize) {
        let Some(hitbox) = self.turn_table_hitbox else {
            return;
        };
        self.turn_reveal_pending = false;
        self.turn_offset = scroll_offset(
            self.turn_offset,
            self.selected_task_turn_count(),
            hitbox.capacity,
            down,
            lines,
        );
    }

    fn toggle_theme(&mut self) {
        self.theme = self.theme.toggle();
        self.summary_project_colors.clear();
        extend_summary_project_colors_from_history(
            &mut self.summary_project_colors,
            &self.history,
            self.theme,
        );
    }

    fn select_task_at(&mut self, column: u16, row: u16) -> bool {
        let Some(position) = self
            .task_table_hitbox
            .and_then(|hitbox| hitbox.index_at(column, row))
        else {
            return false;
        };
        let Some(index) = self.filtered_task_indices().get(position).copied() else {
            return false;
        };
        self.select_task(index, false)
    }

    fn activate_task_tree_marker_at(&mut self, column: u16, row: u16) -> bool {
        let Some(marker) = self
            .task_tree_marker_hitboxes
            .iter()
            .find(|marker| rect_contains(marker.area, column, row))
            .copied()
        else {
            return false;
        };
        self.accept_active_search();
        self.transition_to_tasks();
        if !self.select_task(marker.task_index, false) {
            return false;
        }
        let collapsed = self
            .filtered_task_rows()
            .iter()
            .find(|row| row.index == marker.task_index)
            .is_some_and(|row| row.collapsed);
        self.set_task_collapsed(marker.task_index, !collapsed);
        true
    }

    fn activate_task_control_at(&mut self, column: u16, row: u16) -> bool {
        let Some(hitbox) = self.task_controls_hitbox else {
            return false;
        };
        if rect_contains(hitbox.enter_turns, column, row)
            && self.focus == Focus::Tasks
            && self.selected_task_raw_turn_count() > 0
        {
            self.focus_turns();
            return true;
        }
        if rect_contains(hitbox.open_terminal, column, row) && self.focus == Focus::Tasks {
            self.activate_open();
            return true;
        }
        if rect_contains(hitbox.toggle_tree, column, row) {
            self.accept_active_search();
            self.toggle_task_list_mode();
            return true;
        }
        if rect_contains(hitbox.collapse_all, column, row)
            && self.task_list_mode == TaskListMode::Tree
        {
            self.accept_active_search();
            self.toggle_all_task_threads();
            return true;
        }
        if rect_contains(hitbox.clear_search, column, row) {
            self.accept_active_search();
            self.clear_task_search();
            return true;
        }
        if rect_contains(hitbox.search, column, row) {
            self.accept_active_search();
            self.begin_task_search();
            return true;
        }
        for filter in TaskSourceFilter::ALL {
            if rect_contains(hitbox.sources[filter.index()], column, row) {
                self.accept_active_search();
                self.set_task_source_filter(filter);
                return true;
            }
        }
        false
    }

    fn activate_turn_control_at(&mut self, column: u16, row: u16) -> bool {
        let Some(hitbox) = self.turn_controls_hitbox else {
            return false;
        };
        if rect_contains(hitbox.back_tasks, column, row) && self.focus == Focus::Turns {
            self.focus_tasks();
            return true;
        }
        if rect_contains(hitbox.clear_search, column, row) {
            self.accept_active_search();
            self.clear_turn_search();
            return true;
        }
        if rect_contains(hitbox.search, column, row) {
            self.begin_turn_search();
            return true;
        }
        false
    }

    fn activate_view_at(&mut self, column: u16, row: u16) -> bool {
        let Some(hitbox) = self.view_tabs_hitbox else {
            return false;
        };
        let Some(view) = View::ALL
            .into_iter()
            .find(|view| rect_contains(hitbox.tabs[view.index()], column, row))
        else {
            return false;
        };
        self.accept_active_search();
        self.set_view(view);
        true
    }

    fn activate_setting_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Settings {
            return false;
        }
        let Some(hitbox) = self.settings_controls_hitbox else {
            return false;
        };
        let Some(item) = SettingItem::ALL
            .into_iter()
            .find(|item| rect_contains(hitbox.rows[item.index()], column, row))
        else {
            return false;
        };
        self.toggle_setting(item);
        true
    }

    fn set_view(&mut self, view: View) {
        if self.view != view && self.turns_temporarily_visible {
            self.close_temporary_turns();
        }
        if self.view != view {
            self.clear_trend_inspection();
            self.summary_daily_dragging = false;
        }
        if view != View::Overview {
            self.close_temporary_turns();
            self.transition_to_tasks();
        }
        self.view = view;
    }

    fn set_summary_range(&mut self, range: SummaryRange) {
        if self.summary_range == range {
            return;
        }
        self.summary_range = range;
        self.summary_cache = None;
        self.summary_selected_id = None;
        self.summary_offset = 0;
        self.summary_inspected_date = None;
        self.summary_daily_dragging = false;
    }

    fn cycle_summary_grain(&mut self) {
        self.summary_grain = self.summary_grain.next();
        self.summary_inspected_date = None;
        self.summary_daily_dragging = false;
    }

    fn set_summary_metric(&mut self, metric: SummaryMetric) {
        if self.summary_metric == metric {
            return;
        }
        self.summary_metric = metric;
        if self.summary_cache.as_ref().is_some_and(|cache| {
            summary_chart_project_count(&cache.prepared, metric, self.api_long_context_multiplier)
                <= SUMMARY_STACKED_PROJECT_LIMIT
        }) {
            self.summary_show_all_projects = false;
        }
        self.summary_selected_id = None;
        self.summary_offset = 0;
    }

    fn can_toggle_summary_all_projects(&self) -> bool {
        self.summary_cache.as_ref().is_some_and(|cache| {
            summary_chart_project_count(
                &cache.prepared,
                self.summary_metric,
                self.api_long_context_multiplier,
            ) > SUMMARY_STACKED_PROJECT_LIMIT
        })
    }

    fn toggle_summary_all_projects(&mut self) -> bool {
        if !self.can_toggle_summary_all_projects() {
            return false;
        }
        self.summary_show_all_projects = !self.summary_show_all_projects;
        true
    }

    fn default_summary_inspection_date(&self) -> Option<NaiveDateTime> {
        let hitbox = self.summary_daily_hitbox.as_ref()?;
        let cache = self.summary_cache.as_ref()?;
        hitbox
            .dates
            .iter()
            .rev()
            .find(|date| {
                cache.chart.buckets.iter().any(|bucket| {
                    bucket.starts_at == **date
                        && cache.prepared.chart_bucket_state(
                            bucket,
                            cache.chart.grain,
                            self.summary_metric,
                            self.api_long_context_multiplier,
                        ) != SummaryDailyState::Missing
                })
            })
            .or_else(|| hitbox.dates.last())
            .copied()
    }

    fn toggle_summary_inspection(&mut self) -> bool {
        if self.summary_inspected_date.take().is_some() {
            return true;
        }
        if self.summary_daily_hitbox.is_none() {
            return false;
        }
        let Some(date) = self.default_summary_inspection_date() else {
            return false;
        };
        self.summary_inspected_date = Some(date);
        true
    }

    fn step_summary_inspection(&mut self, forward: bool) -> bool {
        let Some(selected) = self.summary_inspected_date else {
            return false;
        };
        let Some(dates) = self
            .summary_daily_hitbox
            .as_ref()
            .map(|hitbox| hitbox.dates.as_slice())
        else {
            return false;
        };
        let Some(index) = dates.iter().position(|date| *date == selected) else {
            self.summary_inspected_date = self.default_summary_inspection_date();
            return self.summary_inspected_date.is_some();
        };
        let next = if forward {
            (index + 1).min(dates.len().saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
        self.summary_inspected_date = dates.get(next).copied();
        true
    }

    fn edge_summary_inspection(&mut self, end: bool) -> bool {
        let Some(dates) = self
            .summary_daily_hitbox
            .as_ref()
            .map(|hitbox| hitbox.dates.as_slice())
        else {
            return false;
        };
        self.summary_inspected_date = if end {
            dates.last().copied()
        } else {
            dates.first().copied()
        };
        self.summary_inspected_date.is_some()
    }

    fn summary_rows(&self) -> Vec<SummaryTreeRow> {
        self.summary_cache.as_ref().map_or_else(Vec::new, |cache| {
            summary_tree_rows(
                &cache.prepared.usage,
                self.summary_metric,
                self.api_long_context_multiplier,
                &self.summary_expanded_nodes,
            )
        })
    }

    fn summary_selected_index(&self, rows: &[SummaryTreeRow]) -> usize {
        self.summary_selected_id
            .as_deref()
            .and_then(|selected| rows.iter().position(|row| row.id == selected))
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1))
    }

    fn select_summary_index(&mut self, index: usize, reveal: bool) -> bool {
        let rows = self.summary_rows();
        let Some(row) = rows.get(index) else {
            return false;
        };
        self.summary_selected_id = Some(row.id.clone());
        if reveal && let Some(hitbox) = self.summary_table_hitbox {
            self.summary_offset =
                reveal_offset(self.summary_offset, index, rows.len(), hitbox.capacity);
        }
        true
    }

    fn move_summary_selection(&mut self, down: bool) {
        let rows = self.summary_rows();
        if rows.is_empty() {
            self.summary_selected_id = None;
            return;
        }
        let current = self.summary_selected_index(&rows);
        let next = if down {
            (current + 1).min(rows.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        self.select_summary_index(next, true);
    }

    fn select_summary_edge(&mut self, end: bool) {
        let rows = self.summary_rows();
        if rows.is_empty() {
            self.summary_selected_id = None;
            return;
        }
        self.select_summary_index(if end { rows.len() - 1 } else { 0 }, true);
    }

    fn toggle_summary_node(&mut self, node_id: &str) -> bool {
        let rows = self.summary_rows();
        let Some(row) = rows.iter().find(|row| row.id == node_id) else {
            return false;
        };
        if !row.has_children {
            return false;
        }
        if self.summary_expanded_nodes.contains(node_id) {
            self.summary_expanded_nodes.remove(node_id);
        } else {
            self.summary_expanded_nodes.insert(node_id.to_string());
        }
        true
    }

    fn toggle_selected_summary_node(&mut self) -> bool {
        self.summary_selected_id
            .clone()
            .is_some_and(|node_id| self.toggle_summary_node(&node_id))
    }

    fn set_selected_summary_node_collapsed(&mut self, collapsed: bool) -> bool {
        let Some(node_id) = self.summary_selected_id.clone() else {
            return false;
        };
        if !self
            .summary_rows()
            .iter()
            .any(|row| row.id == node_id && row.has_children)
        {
            return false;
        }
        if collapsed {
            self.summary_expanded_nodes.remove(&node_id)
        } else {
            self.summary_expanded_nodes.insert(node_id)
        }
    }

    fn collapse_all_summary_nodes(&mut self) -> bool {
        if self.summary_expanded_nodes.is_empty() {
            return false;
        }
        let rows = self.summary_rows();
        let selected_project_id = if rows.is_empty() {
            None
        } else {
            let selected_index = self.summary_selected_index(&rows);
            rows[..=selected_index]
                .iter()
                .rev()
                .find(|row| row.kind == SummaryRowKind::Project)
                .map(|row| row.id.clone())
        };
        self.summary_expanded_nodes.clear();
        let collapsed_rows = self.summary_rows();
        let selected_index = selected_project_id
            .as_deref()
            .and_then(|project_id| collapsed_rows.iter().position(|row| row.id == project_id))
            .unwrap_or(0);
        self.summary_offset = 0;
        self.select_summary_index(selected_index, true);
        true
    }

    fn activate_summary_control_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Summary {
            return false;
        }
        let Some(hitbox) = self.summary_controls_hitbox else {
            return false;
        };
        if let Some(range) = SummaryRange::ALL
            .into_iter()
            .find(|range| rect_contains(hitbox.ranges[range.index()], column, row))
        {
            self.set_summary_range(range);
            return true;
        }
        if let Some(metric) = SummaryMetric::ALL
            .into_iter()
            .find(|metric| rect_contains(hitbox.metrics[metric.index()], column, row))
        {
            self.set_summary_metric(metric);
            return true;
        }
        if rect_contains(hitbox.bucket_grain, column, row) {
            self.cycle_summary_grain();
            return true;
        }
        if rect_contains(hitbox.toggle_all_projects, column, row) {
            return self.toggle_summary_all_projects();
        }
        if rect_contains(hitbox.toggle_long_context, column, row) {
            self.toggle_api_long_context_multiplier();
            return true;
        }
        if rect_contains(hitbox.inspect, column, row) {
            return self.toggle_summary_inspection();
        }
        if rect_contains(hitbox.toggle_selected, column, row) {
            return self.toggle_selected_summary_node();
        }
        if rect_contains(hitbox.collapse_all, column, row) {
            return self.collapse_all_summary_nodes();
        }
        false
    }

    fn activate_summary_tree_marker_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Summary {
            return false;
        }
        let Some(hitbox) = self
            .summary_tree_marker_hitboxes
            .iter()
            .find(|hitbox| rect_contains(hitbox.area, column, row))
            .cloned()
        else {
            return false;
        };
        self.summary_selected_id = Some(hitbox.node_id.clone());
        self.toggle_summary_node(&hitbox.node_id)
    }

    fn select_summary_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Summary {
            return false;
        }
        let Some(index) = self
            .summary_table_hitbox
            .and_then(|hitbox| hitbox.index_at(column, row))
        else {
            return false;
        };
        self.select_summary_index(index, false)
    }

    fn activate_summary_bar_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Summary {
            return false;
        }
        let Some(project_key) = self
            .summary_bar_hitboxes
            .iter()
            .find(|hitbox| rect_contains(hitbox.area, column, row))
            .map(|hitbox| hitbox.project_key.clone())
        else {
            return false;
        };
        let node_id = summary_project_node_id(&project_key);
        let rows = self.summary_rows();
        let Some(index) = rows.iter().position(|row| row.id == node_id) else {
            return false;
        };
        self.select_summary_index(index, true)
    }

    fn begin_summary_daily_drag_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Summary {
            return false;
        }
        let Some(date) = self
            .summary_daily_hitbox
            .as_ref()
            .filter(|hitbox| hitbox.contains(column, row))
            .and_then(|hitbox| hitbox.date_at_column(column))
        else {
            return false;
        };
        self.scroll_drag = None;
        self.trend_drag = None;
        self.summary_daily_dragging = true;
        self.summary_inspected_date = Some(date);
        true
    }

    fn drag_summary_daily_to(&mut self, column: u16) -> bool {
        if !self.summary_daily_dragging {
            return false;
        }
        let Some(date) = self
            .summary_daily_hitbox
            .as_ref()
            .and_then(|hitbox| hitbox.date_at_column(column))
        else {
            return false;
        };
        if self.summary_inspected_date == Some(date) {
            return false;
        }
        self.summary_inspected_date = Some(date);
        true
    }

    fn scroll_summary(&mut self, down: bool, lines: usize) {
        let Some(hitbox) = self.summary_table_hitbox else {
            return;
        };
        self.summary_offset = scroll_offset(
            self.summary_offset,
            self.summary_rows().len(),
            hitbox.capacity,
            down,
            lines,
        );
    }

    fn set_window_scope(&mut self, scope: WindowScope) {
        self.window_scope = scope;
    }

    fn set_trend_section(&mut self, section: TrendSection) {
        if self.trend_section != section {
            self.clear_trend_inspection();
        }
        self.trend_section = section;
    }

    fn trend_section_control_visible(&self, section: TrendSection) -> bool {
        self.trend_controls_hitbox
            .is_some_and(|hitbox| !hitbox.sections[section.index()].is_empty())
    }

    fn trend_previous_day_control_visible(&self) -> bool {
        self.trend_controls_hitbox
            .is_some_and(|hitbox| !hitbox.previous_day.is_empty())
    }

    fn trend_next_day_control_visible(&self) -> bool {
        self.trend_controls_hitbox
            .is_some_and(|hitbox| !hitbox.next_day.is_empty())
    }

    fn trend_now_control_visible(&self) -> bool {
        self.trend_controls_hitbox
            .is_some_and(|hitbox| !hitbox.now.is_empty())
    }

    fn trend_inspect_control_visible(&self) -> bool {
        self.trend_controls_hitbox
            .is_some_and(|hitbox| !hitbox.inspect.is_empty())
    }

    fn show_previous_trend_day(&mut self) {
        let maximum = u16::try_from(HISTORY_VIEW_DAYS.saturating_sub(1)).unwrap_or(u16::MAX);
        let next = self.trend_day_offset.saturating_add(1).min(maximum);
        if self.trend_day_offset != next {
            self.clear_trend_inspection();
        }
        self.trend_day_offset = next;
    }

    fn show_next_trend_day(&mut self) {
        let next = self.trend_day_offset.saturating_sub(1);
        if self.trend_day_offset != next {
            self.clear_trend_inspection();
        }
        self.trend_day_offset = next;
    }

    fn show_current_trend_day(&mut self) {
        if self.trend_day_offset != 0 {
            self.clear_trend_inspection();
        }
        self.trend_day_offset = 0;
    }

    fn clear_trend_inspection(&mut self) {
        self.trend_inspect_mode = false;
        self.trend_inspection = None;
        self.trend_drag = None;
    }

    fn toggle_trend_inspection(&mut self) {
        if self.trend_inspect_mode {
            self.clear_trend_inspection();
            return;
        }
        self.trend_inspect_mode = true;
        self.trend_inspection = self
            .trend_chart_hitboxes
            .iter()
            .find_map(TrendChartHitbox::latest_inspection);
    }

    fn begin_trend_drag_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Trends {
            return false;
        }
        let Some((panel, inspection)) = self
            .trend_chart_hitboxes
            .iter()
            .find(|hitbox| hitbox.contains(column, row))
            .and_then(|hitbox| {
                hitbox
                    .inspection_at_column(column)
                    .map(|inspection| (hitbox.panel, inspection))
            })
        else {
            return false;
        };
        self.scroll_drag = None;
        self.trend_drag = Some(TrendDrag { panel });
        self.trend_inspect_mode = true;
        self.trend_inspection = Some(inspection);
        true
    }

    fn drag_trend_to(&mut self, column: u16) -> bool {
        let Some(drag) = self.trend_drag else {
            return false;
        };
        let Some(inspection) = self
            .trend_chart_hitboxes
            .iter()
            .find(|hitbox| hitbox.panel == drag.panel)
            .and_then(|hitbox| hitbox.inspection_at_column(column))
        else {
            return false;
        };
        if self.trend_inspection == Some(inspection) {
            return false;
        }
        self.trend_inspection = Some(inspection);
        true
    }

    fn step_trend_inspection(&mut self, forward: bool) {
        if !self.trend_inspect_mode {
            return;
        }
        let Some(current) = self.trend_inspection.or_else(|| {
            self.trend_chart_hitboxes
                .iter()
                .find_map(TrendChartHitbox::latest_inspection)
        }) else {
            return;
        };
        let Some(hitbox) = self
            .trend_chart_hitboxes
            .iter()
            .find(|hitbox| hitbox.panel == current.panel)
        else {
            self.trend_inspection = self
                .trend_chart_hitboxes
                .iter()
                .find_map(TrendChartHitbox::latest_inspection);
            return;
        };
        self.trend_inspection = hitbox.step_inspection(current.at, forward);
    }

    fn edge_trend_inspection(&mut self, end: bool) {
        if !self.trend_inspect_mode {
            return;
        }
        let Some(panel) = self
            .trend_inspection
            .map(|inspection| inspection.panel)
            .or_else(|| self.trend_chart_hitboxes.first().map(|hitbox| hitbox.panel))
        else {
            return;
        };
        self.trend_inspection = self
            .trend_chart_hitboxes
            .iter()
            .find(|hitbox| hitbox.panel == panel)
            .and_then(|hitbox| {
                if end {
                    hitbox.latest_inspection()
                } else {
                    hitbox.earliest_inspection()
                }
            });
    }

    fn move_trend_inspection_panel(&mut self, forward: bool) {
        if !self.trend_inspect_mode {
            return;
        }
        let available = self
            .trend_chart_hitboxes
            .iter()
            .filter(|hitbox| hitbox.has_inspectable_points())
            .collect::<Vec<_>>();
        if available.is_empty() {
            self.trend_inspection = None;
            return;
        }
        let current = self
            .trend_inspection
            .and_then(|inspection| {
                available
                    .iter()
                    .position(|hitbox| hitbox.panel == inspection.panel)
            })
            .unwrap_or(0);
        let next = if forward {
            (current + 1).min(available.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        let target_at = self.trend_inspection.map(|inspection| inspection.at);
        self.trend_inspection = target_at
            .and_then(|at| available[next].nearest_inspection(at))
            .or_else(|| available[next].latest_inspection());
    }

    fn activate_window_control_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Overview {
            return false;
        }
        let Some(hitbox) = self.window_controls_hitbox else {
            return false;
        };
        if rect_contains(hitbox.toggle_turns, column, row) {
            self.accept_active_search();
            self.toggle_turns_default_visibility();
            return true;
        }
        if rect_contains(hitbox.toggle_models, column, row) {
            self.accept_active_search();
            self.toggle_models_visibility();
            return true;
        }
        if rect_contains(hitbox.toggle_api_long_context, column, row) {
            self.accept_active_search();
            self.toggle_api_long_context_multiplier();
            return true;
        }
        let Some(scope) = WindowScope::ALL
            .into_iter()
            .find(|scope| rect_contains(hitbox.scopes[scope.index()], column, row))
        else {
            return false;
        };
        self.accept_active_search();
        self.set_window_scope(scope);
        true
    }

    fn activate_trend_control_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Trends {
            return false;
        }
        let Some(hitbox) = self.trend_controls_hitbox else {
            return false;
        };
        if let Some(section) = TrendSection::ALL
            .into_iter()
            .find(|section| rect_contains(hitbox.sections[section.index()], column, row))
        {
            self.set_trend_section(section);
            return true;
        }
        if rect_contains(hitbox.inspect, column, row) {
            self.toggle_trend_inspection();
            return true;
        }
        if rect_contains(hitbox.previous_day, column, row) {
            self.show_previous_trend_day();
            return true;
        }
        if rect_contains(hitbox.next_day, column, row) {
            self.show_next_trend_day();
            return true;
        }
        if rect_contains(hitbox.now, column, row) {
            self.show_current_trend_day();
            return true;
        }
        false
    }

    fn open_quit_confirmation(&mut self) {
        self.quit_confirmation_visible = true;
        self.quit_requested = false;
        self.scroll_drag = None;
        self.trend_drag = None;
        self.summary_daily_dragging = false;
    }

    fn close_quit_confirmation(&mut self) {
        self.quit_confirmation_visible = false;
        self.quit_confirmation_hitbox = None;
        self.quit_requested = false;
    }

    fn select_turn_at(&mut self, column: u16, row: u16) -> bool {
        let Some(index) = self
            .turn_table_hitbox
            .and_then(|hitbox| hitbox.index_at(column, row))
            .filter(|index| *index < self.selected_task_turn_count())
        else {
            return false;
        };
        self.select_turn(index, false)
    }

    fn scrollbar_hitbox(&self, target: ScrollTarget) -> Option<ScrollbarHitbox> {
        match target {
            ScrollTarget::Tasks => self.task_scrollbar_hitbox,
            ScrollTarget::Turns => self.turn_scrollbar_hitbox,
            ScrollTarget::Summary => self.summary_scrollbar_hitbox,
        }
    }

    fn begin_scrollbar_drag_at(&mut self, column: u16, row: u16) -> bool {
        let Some((target, hitbox)) = [
            ScrollTarget::Summary,
            ScrollTarget::Turns,
            ScrollTarget::Tasks,
        ]
        .into_iter()
        .find_map(|target| {
            self.scrollbar_hitbox(target)
                .filter(|hitbox| rect_contains(hitbox.track, column, row))
                .map(|hitbox| (target, hitbox))
        }) else {
            return false;
        };
        self.accept_active_search();
        match target {
            ScrollTarget::Tasks => self.transition_to_tasks(),
            ScrollTarget::Turns => self.focus = Focus::Turns,
            ScrollTarget::Summary => {}
        }
        let on_thumb = rect_contains(hitbox.thumb, column, row);
        self.scroll_drag = Some(ScrollDrag {
            target,
            grab_row: if on_thumb {
                row.saturating_sub(hitbox.thumb.y)
            } else {
                hitbox.thumb.height / 2
            },
            pointer_row: on_thumb.then_some(row),
        });
        if !on_thumb {
            self.drag_scrollbar_to(row);
        }
        true
    }

    fn drag_scrollbar_to(&mut self, row: u16) -> bool {
        let Some(mut drag) = self.scroll_drag else {
            return false;
        };
        if drag.pointer_row == Some(row) {
            return true;
        }
        drag.pointer_row = Some(row);
        self.scroll_drag = Some(drag);
        let Some(hitbox) = self.scrollbar_hitbox(drag.target) else {
            self.scroll_drag = None;
            return false;
        };
        let travel = hitbox.track.height.saturating_sub(hitbox.thumb.height);
        let pointer_row = row.saturating_sub(hitbox.track.y);
        let thumb_row = pointer_row.saturating_sub(drag.grab_row).min(travel);
        let offset = scale_rounded(
            usize::from(thumb_row),
            hitbox.max_offset,
            usize::from(travel),
        );
        match drag.target {
            ScrollTarget::Tasks => {
                self.task_reveal_pending = false;
                self.task_table_offset = offset;
            }
            ScrollTarget::Turns => {
                self.turn_reveal_pending = false;
                self.turn_offset = offset;
            }
            ScrollTarget::Summary => self.summary_offset = offset,
        }
        true
    }
}

fn task_parent_edge_would_cycle(
    child: usize,
    parent: usize,
    parent_by_index: &[Option<usize>],
) -> bool {
    let mut cursor = Some(parent);
    let mut remaining = parent_by_index.len().saturating_add(1);
    while let Some(index) = cursor {
        if index == child || remaining == 0 {
            return true;
        }
        cursor = parent_by_index.get(index).copied().flatten();
        remaining = remaining.saturating_sub(1);
    }
    false
}

fn task_subtree_rank(
    index: usize,
    children: &[Vec<usize>],
    subtree_ranks: &mut [Option<usize>],
) -> usize {
    if let Some(rank) = subtree_ranks[index] {
        return rank;
    }
    let mut rank = index;
    for &child in &children[index] {
        rank = rank.min(task_subtree_rank(child, children, subtree_ranks));
    }
    subtree_ranks[index] = Some(rank);
    rank
}

fn append_task_tree_rows(
    index: usize,
    children: &[Vec<usize>],
    tasks: &[TaskRecord],
    expanded_task_threads: Option<&HashSet<String>>,
    guides: &mut Vec<bool>,
    rows: &mut Vec<TaskListRow>,
) {
    let mut prefix = String::new();
    if let Some((&is_last, ancestors)) = guides.split_last() {
        for ancestor_is_last in ancestors {
            prefix.push_str(if *ancestor_is_last { "  " } else { "│ " });
        }
        prefix.push_str(if is_last { "└─ " } else { "├─ " });
    }
    let has_children = !children[index].is_empty();
    let collapsed = has_children
        && tasks.get(index).is_some_and(|task| {
            expanded_task_threads.is_some_and(|expanded| !expanded.contains(&task.thread_id))
        });
    let mut hidden_descendants = Vec::new();
    if collapsed {
        collect_task_descendants(index, children, &mut hidden_descendants);
    }
    rows.push(TaskListRow {
        index,
        prefix,
        depth: guides.len(),
        has_children,
        collapsed,
        hidden_descendants,
    });
    if collapsed {
        return;
    }

    let child_count = children[index].len();
    for (position, &child) in children[index].iter().enumerate() {
        guides.push(position + 1 == child_count);
        append_task_tree_rows(child, children, tasks, expanded_task_threads, guides, rows);
        guides.pop();
    }
}

fn collect_task_descendants(index: usize, children: &[Vec<usize>], descendants: &mut Vec<usize>) {
    let Some(direct_children) = children.get(index) else {
        return;
    };
    for &child in direct_children {
        descendants.push(child);
        collect_task_descendants(child, children, descendants);
    }
}

fn handle_mouse_event(app: &mut App, event: MouseEvent) -> bool {
    if app.resume_confirmation.is_some() {
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            if app
                .resume_confirmation_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.confirm, event.column, event.row))
            {
                app.confirm_resume();
            } else if app
                .resume_confirmation_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.copy, event.column, event.row))
            {
                app.request_resume_command_copy();
            } else if app
                .resume_confirmation_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.cancel, event.column, event.row))
            {
                app.close_resume_confirmation();
            }
        }
        return true;
    }

    if app.quit_confirmation_visible {
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            if app
                .quit_confirmation_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.confirm, event.column, event.row))
            {
                app.quit_requested = true;
            } else if app
                .quit_confirmation_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.cancel, event.column, event.row))
            {
                app.close_quit_confirmation();
            }
        }
        return true;
    }

    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.scroll_drag = None;
            app.trend_drag = None;
            app.summary_daily_dragging = false;
            if app.activate_view_at(event.column, event.row)
                || app.activate_setting_at(event.column, event.row)
                || app.activate_window_control_at(event.column, event.row)
                || app.activate_trend_control_at(event.column, event.row)
                || app.activate_summary_control_at(event.column, event.row)
                || app.begin_trend_drag_at(event.column, event.row)
                || app.activate_task_control_at(event.column, event.row)
                || app.activate_turn_control_at(event.column, event.row)
                || app.activate_task_tree_marker_at(event.column, event.row)
                || app.activate_summary_tree_marker_at(event.column, event.row)
                || app.activate_summary_bar_at(event.column, event.row)
                || app.begin_summary_daily_drag_at(event.column, event.row)
                || app.begin_scrollbar_drag_at(event.column, event.row)
            {
                true
            } else {
                let activate_selected_task =
                    app.focus == Focus::Tasks && app.selected_task_record().is_some();
                let previously_selected_task = app.selected_task;
                app.accept_active_search();
                if app.select_summary_at(event.column, event.row) {
                    true
                } else if app.select_turn_at(event.column, event.row) {
                    app.focus = Focus::Turns;
                    true
                } else if app.select_task_at(event.column, event.row) {
                    if activate_selected_task && app.selected_task == previously_selected_task {
                        app.focus_turns();
                    } else {
                        app.focus_tasks();
                    }
                    true
                } else {
                    false
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.scroll_drag.is_some() {
                app.drag_scrollbar_to(event.row)
            } else if app.summary_daily_dragging {
                app.drag_summary_daily_to(event.column)
            } else {
                app.drag_trend_to(event.column)
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let ended_scroll_drag = app.scroll_drag.take().is_some();
            let ended_trend_drag = app.trend_drag.take().is_some();
            let ended_summary_drag = std::mem::take(&mut app.summary_daily_dragging);
            ended_scroll_drag || ended_trend_drag || ended_summary_drag
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let down = matches!(event.kind, MouseEventKind::ScrollDown);
            if app
                .summary_scrollbar_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.track, event.column, event.row))
                || app
                    .summary_table_hitbox
                    .is_some_and(|hitbox| hitbox.contains_viewport(event.column, event.row))
            {
                app.scroll_summary(down, MOUSE_SCROLL_LINES);
                true
            } else if app
                .turn_scrollbar_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.track, event.column, event.row))
                || app
                    .turn_table_hitbox
                    .is_some_and(|hitbox| hitbox.contains_viewport(event.column, event.row))
            {
                app.scroll_turns(down, MOUSE_SCROLL_LINES);
                true
            } else if app
                .task_scrollbar_hitbox
                .is_some_and(|hitbox| rect_contains(hitbox.track, event.column, event.row))
                || app
                    .task_table_hitbox
                    .is_some_and(|hitbox| hitbox.contains_viewport(event.column, event.row))
            {
                app.scroll_tasks(down, MOUSE_SCROLL_LINES);
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn view_tabs_hitbox(area: Rect) -> ViewTabsHitbox {
    let compact = view_tabs_compact(area.width);
    let mut tabs = [Rect::default(); 5];
    let mut x = area.x;
    for (position, view) in View::ALL.into_iter().enumerate() {
        let label = if compact {
            view.compact_label()
        } else {
            view.label()
        };
        let width = UnicodeWidthStr::width(TAB_PADDING)
            + 2
            + UnicodeWidthStr::width(label)
            + UnicodeWidthStr::width(TAB_PADDING);
        let width = u16::try_from(width).unwrap_or(u16::MAX);
        tabs[view.index()] = fully_visible_horizontal_hitbox(area, x, width);
        x = x.saturating_add(width);
        if position + 1 < View::ALL.len() {
            x = x.saturating_add(
                u16::try_from(UnicodeWidthStr::width(TAB_DIVIDER)).unwrap_or(u16::MAX),
            );
        }
    }
    ViewTabsHitbox {
        tabs,
        rendered_right: x.min(area.right()),
    }
}

fn view_tabs_compact(width: u16) -> bool {
    let full_width = View::ALL
        .into_iter()
        .map(|view| {
            UnicodeWidthStr::width(TAB_PADDING)
                + 2
                + UnicodeWidthStr::width(view.label())
                + UnicodeWidthStr::width(TAB_PADDING)
        })
        .sum::<usize>()
        + UnicodeWidthStr::width(TAB_DIVIDER) * View::ALL.len().saturating_sub(1);
    let compact_overview_controls = UnicodeWidthStr::width(" [V][M][5][W][L]");
    full_width.saturating_add(compact_overview_controls) > usize::from(width)
}

fn clipped_horizontal_hitbox(area: Rect, x: u16, width: u16) -> Rect {
    let start = x.max(area.x).min(area.right());
    let end = x.saturating_add(width).min(area.right());
    Rect::new(
        start,
        area.y,
        end.saturating_sub(start),
        u16::from(area.height > 0),
    )
}

fn fully_visible_horizontal_hitbox(area: Rect, x: u16, width: u16) -> Rect {
    let Some(end) = x.checked_add(width) else {
        return Rect::default();
    };
    if area.height == 0 || x < area.x || end > area.right() {
        Rect::default()
    } else {
        Rect::new(x, area.y, width, 1)
    }
}

fn fast_model_line(value: &str, column_width: usize, theme: Theme) -> Line<'static> {
    const SUFFIX: &str = " FAST";
    let value_width = column_width.saturating_sub(UnicodeWidthStr::width(SUFFIX));
    Line::from(vec![
        Span::raw(truncate_display_text(value, value_width)),
        Span::styled(
            SUFFIX,
            Style::default()
                .fg(theme.palette().warning)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn handle_key_event(app: &mut App, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }

    if app.resume_confirmation.is_some() {
        match key.code {
            KeyCode::Enter => app.confirm_resume(),
            KeyCode::Char('c') | KeyCode::Char('C') => app.request_resume_command_copy(),
            KeyCode::Esc => app.close_resume_confirmation(),
            _ => {}
        }
        return false;
    }

    if app.quit_confirmation_visible {
        match key.code {
            KeyCode::Enter | KeyCode::Char('q') => return true,
            KeyCode::Esc => app.close_quit_confirmation(),
            _ => {}
        }
        return false;
    }

    if app.focus == Focus::TaskSearch {
        match key.code {
            KeyCode::Esc => app.cancel_task_search(),
            KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab => app.accept_task_search(),
            KeyCode::Backspace => app.backspace_task_search(),
            KeyCode::Delete => app.delete_task_search(),
            KeyCode::Left => {
                app.task_search_cursor = app.task_search_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                app.task_search_cursor =
                    (app.task_search_cursor + 1).min(app.task_search.chars().count());
            }
            KeyCode::Home => app.task_search_cursor = 0,
            KeyCode::End => app.task_search_cursor = app.task_search.chars().count(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.insert_task_search(character);
            }
            _ => {}
        }
        return false;
    }

    if app.focus == Focus::TurnSearch {
        match key.code {
            KeyCode::Esc => app.cancel_turn_search(),
            KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab => app.accept_turn_search(),
            KeyCode::Backspace => app.backspace_turn_search(),
            KeyCode::Delete => app.delete_turn_search(),
            KeyCode::Left => {
                app.turn_search_cursor = app.turn_search_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                app.turn_search_cursor =
                    (app.turn_search_cursor + 1).min(app.turn_search.chars().count());
            }
            KeyCode::Home => app.turn_search_cursor = 0,
            KeyCode::End => app.turn_search_cursor = app.turn_search.chars().count(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.insert_turn_search(character);
            }
            _ => {}
        }
        return false;
    }

    if app.view == View::Trends && app.trend_inspect_mode {
        match key.code {
            KeyCode::Char('i' | 'I') | KeyCode::Esc => app.clear_trend_inspection(),
            KeyCode::Left => app.step_trend_inspection(false),
            KeyCode::Right => app.step_trend_inspection(true),
            KeyCode::Up => app.move_trend_inspection_panel(false),
            KeyCode::Down => app.move_trend_inspection_panel(true),
            KeyCode::Home => app.edge_trend_inspection(false),
            KeyCode::End => app.edge_trend_inspection(true),
            _ => {}
        }
        if matches!(
            key.code,
            KeyCode::Char('i' | 'I')
                | KeyCode::Esc
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
        ) {
            return false;
        }
    }

    if app.view == View::Summary && app.summary_inspected_date.is_some() {
        match key.code {
            KeyCode::Char('i' | 'I') | KeyCode::Esc => {
                app.summary_inspected_date = None;
            }
            KeyCode::Left | KeyCode::Char('[') => {
                app.step_summary_inspection(false);
            }
            KeyCode::Right | KeyCode::Char(']') => {
                app.step_summary_inspection(true);
            }
            KeyCode::Home => {
                app.edge_summary_inspection(false);
            }
            KeyCode::End => {
                app.edge_summary_inspection(true);
            }
            _ => {}
        }
        if matches!(
            key.code,
            KeyCode::Char('i' | 'I')
                | KeyCode::Esc
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('[' | ']')
                | KeyCode::Home
                | KeyCode::End
        ) {
            return false;
        }
    }

    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Esc => app.open_quit_confirmation(),
        KeyCode::Tab | KeyCode::Right => app.set_view(app.view.next()),
        KeyCode::BackTab | KeyCode::Left => app.set_view(app.view.previous()),
        KeyCode::Char('1') => app.set_view(View::Overview),
        KeyCode::Char('2') => app.set_view(View::Trends),
        KeyCode::Char('u' | 'U') => app.set_view(View::Summary),
        KeyCode::Char('3') => app.set_view(View::Health),
        KeyCode::Char('4') => app.set_view(View::Settings),
        KeyCode::Char('c' | 'C') if app.view == View::Summary => {
            app.set_summary_range(SummaryRange::Cycle);
        }
        KeyCode::Char('7') if app.view == View::Summary => {
            app.set_summary_range(SummaryRange::SevenDays);
        }
        KeyCode::Char('m' | 'M') if app.view == View::Summary => {
            app.set_summary_range(SummaryRange::ThirtyDays);
        }
        KeyCode::Char('K') if app.view == View::Summary => {
            app.set_summary_metric(SummaryMetric::Tokens);
        }
        KeyCode::Char('e' | 'E') if app.view == View::Summary => {
            app.set_summary_metric(SummaryMetric::Estimated);
        }
        KeyCode::Char('a' | 'A') if app.view == View::Summary => {
            app.set_summary_metric(SummaryMetric::ApiEquivalent);
        }
        KeyCode::Char('b' | 'B') if app.view == View::Summary => {
            app.cycle_summary_grain();
        }
        KeyCode::Char('g' | 'G') if app.view == View::Summary => {
            app.toggle_summary_all_projects();
        }
        KeyCode::Char('i' | 'I') if app.view == View::Summary => {
            app.toggle_summary_inspection();
        }
        KeyCode::Char('l' | 'L') if app.view == View::Summary => {
            app.toggle_api_long_context_multiplier();
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.view == View::Summary => {
            app.toggle_selected_summary_node();
        }
        KeyCode::Char('+') if app.view == View::Summary => {
            app.set_selected_summary_node_collapsed(false);
        }
        KeyCode::Char('-') if app.view == View::Summary => {
            app.set_selected_summary_node_collapsed(true);
        }
        KeyCode::Char('x' | 'X') if app.view == View::Summary => {
            app.collapse_all_summary_nodes();
        }
        KeyCode::Down | KeyCode::Char('j') if app.view == View::Summary => {
            app.move_summary_selection(true);
        }
        KeyCode::Up | KeyCode::Char('k') if app.view == View::Summary => {
            app.move_summary_selection(false);
        }
        KeyCode::Home if app.view == View::Summary => app.select_summary_edge(false),
        KeyCode::End if app.view == View::Summary => app.select_summary_edge(true),
        KeyCode::PageDown if app.view == View::Summary => {
            app.scroll_summary(true, PAGE_SCROLL_LINES);
        }
        KeyCode::PageUp if app.view == View::Summary => {
            app.scroll_summary(false, PAGE_SCROLL_LINES);
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.view == View::Settings => {
            app.toggle_setting(app.selected_setting_item());
        }
        KeyCode::Char(character) if app.view == View::Settings => {
            if let Some(item) = SettingItem::from_shortcut(character) {
                app.toggle_setting(item);
            }
        }
        KeyCode::Down if app.view == View::Settings => app.move_setting_selection(true),
        KeyCode::Up if app.view == View::Settings => app.move_setting_selection(false),
        KeyCode::Home if app.view == View::Settings => app.selected_setting = 0,
        KeyCode::End if app.view == View::Settings => {
            app.selected_setting = SettingItem::ALL.len() - 1;
        }
        KeyCode::Char('5') if app.view == View::Overview => {
            app.set_window_scope(WindowScope::FiveHours);
        }
        KeyCode::Char('w' | 'W') if app.view == View::Overview => {
            app.set_window_scope(WindowScope::Week);
        }
        KeyCode::Char('r' | 'R')
            if app.view == View::Trends
                && app.trend_section_control_visible(TrendSection::Remaining) =>
        {
            app.set_trend_section(TrendSection::Remaining);
        }
        KeyCode::Char('w' | 'W')
            if app.view == View::Trends
                && app.trend_section_control_visible(TrendSection::Weekly) =>
        {
            app.set_trend_section(TrendSection::Weekly);
        }
        KeyCode::Char('h' | 'H')
            if app.view == View::Trends
                && app.trend_section_control_visible(TrendSection::HalfHour) =>
        {
            app.set_trend_section(TrendSection::HalfHour);
        }
        KeyCode::Char('i' | 'I')
            if app.view == View::Trends && app.trend_inspect_control_visible() =>
        {
            app.toggle_trend_inspection();
        }
        KeyCode::Char('[')
            if app.view == View::Trends && app.trend_previous_day_control_visible() =>
        {
            app.show_previous_trend_day();
        }
        KeyCode::Char(']') if app.view == View::Trends && app.trend_next_day_control_visible() => {
            app.show_next_trend_day();
        }
        KeyCode::Char('n' | 'N') if app.view == View::Trends && app.trend_now_control_visible() => {
            app.show_current_trend_day();
        }
        KeyCode::Char('t' | 'T') => app.toggle_theme(),
        KeyCode::Char('/') | KeyCode::Char('f' | 'F') if app.view == View::Overview => {
            match app.focus {
                Focus::Tasks => app.begin_task_search(),
                Focus::Turns => app.begin_turn_search(),
                Focus::TaskSearch | Focus::TurnSearch => {}
            }
        }
        KeyCode::Char('v' | 'V') if app.view == View::Overview => {
            app.toggle_turns_default_visibility();
        }
        KeyCode::Char('m' | 'M') if app.view == View::Overview => {
            app.toggle_models_visibility();
        }
        KeyCode::Char('l' | 'L') if app.view == View::Overview => {
            app.toggle_api_long_context_multiplier();
        }
        KeyCode::Char('o' | 'O') if app.view == View::Overview && app.focus == Focus::Tasks => {
            app.activate_open();
        }
        KeyCode::Char('r' | 'R') if app.view == View::Overview => {
            app.toggle_task_list_mode();
        }
        KeyCode::Char('E')
            if app.view == View::Overview && app.task_list_mode == TaskListMode::Tree =>
        {
            app.toggle_all_task_threads();
        }
        KeyCode::Char('-')
            if app.view == View::Overview
                && app.focus == Focus::Tasks
                && app.task_list_mode == TaskListMode::Tree =>
        {
            app.set_selected_task_collapsed(true);
        }
        KeyCode::Char('+')
            if app.view == View::Overview
                && app.focus == Focus::Tasks
                && app.task_list_mode == TaskListMode::Tree =>
        {
            app.set_selected_task_collapsed(false);
        }
        KeyCode::Char('a' | 'A') if app.view == View::Overview => {
            app.set_task_source_filter(TaskSourceFilter::All);
        }
        KeyCode::Char('d' | 'D') if app.view == View::Overview => {
            app.set_task_source_filter(TaskSourceFilter::Desktop);
        }
        KeyCode::Char('s' | 'S') if app.view == View::Overview => {
            app.set_task_source_filter(TaskSourceFilter::Subagent);
        }
        KeyCode::Char('c' | 'C') if app.view == View::Overview => {
            app.set_task_source_filter(TaskSourceFilter::Cli);
        }
        KeyCode::Char(']') if app.view == View::Overview => {
            app.cycle_task_source_filter(true);
        }
        KeyCode::Char('[') if app.view == View::Overview => {
            app.cycle_task_source_filter(false);
        }
        KeyCode::Delete if app.view == View::Overview => match app.focus {
            Focus::Tasks if !app.task_search.is_empty() => app.clear_task_search(),
            Focus::Turns if !app.turn_search.is_empty() => app.clear_turn_search(),
            Focus::TaskSearch | Focus::TurnSearch => {}
            Focus::Tasks | Focus::Turns => {}
        },
        KeyCode::Enter if app.view == View::Overview && app.focus == Focus::Tasks => {
            app.focus_turns();
        }
        KeyCode::Backspace if app.view == View::Overview && app.focus == Focus::Turns => {
            app.focus_tasks();
        }
        KeyCode::Down | KeyCode::Char('j') if app.view == View::Overview => {
            app.select_next_focused();
        }
        KeyCode::Up | KeyCode::Char('k') if app.view == View::Overview => {
            app.select_previous_focused();
        }
        KeyCode::Home if app.view == View::Overview => app.select_first_focused(),
        KeyCode::End if app.view == View::Overview => app.select_last_focused(),
        KeyCode::PageDown if app.view == View::Overview => match app.focus {
            Focus::Tasks => app.scroll_tasks(true, PAGE_SCROLL_LINES),
            Focus::Turns => app.scroll_turns(true, PAGE_SCROLL_LINES),
            Focus::TaskSearch | Focus::TurnSearch => {}
        },
        KeyCode::PageUp if app.view == View::Overview => match app.focus {
            Focus::Tasks => app.scroll_tasks(false, PAGE_SCROLL_LINES),
            Focus::Turns => app.scroll_turns(false, PAGE_SCROLL_LINES),
            Focus::TaskSearch | Focus::TurnSearch => {}
        },
        _ => {}
    }
    false
}

fn render_scrollbar(frame: &mut Frame<'_>, hitbox: ScrollbarHitbox, theme: Theme, active: bool) {
    let palette = theme.palette();
    for row in hitbox.track.y..hitbox.track.bottom() {
        let in_thumb = row >= hitbox.thumb.y && row < hitbox.thumb.bottom();
        if let Some(cell) = frame.buffer_mut().cell_mut((hitbox.track.x, row)) {
            cell.set_symbol(if in_thumb { "█" } else { "│" });
            cell.set_style(
                Style::default()
                    .fg(if in_thumb {
                        if active {
                            palette.accent
                        } else {
                            palette.muted
                        }
                    } else {
                        palette.border
                    })
                    .add_modifier(if in_thumb {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            );
        }
    }
}

pub fn run(config: CollectConfig) -> Result<()> {
    run_with_theme_override(config, None)
}

pub fn run_with_theme(config: CollectConfig, theme: Theme) -> Result<()> {
    run_with_theme_override(config, Some(theme))
}

fn run_with_theme_override(config: CollectConfig, theme_override: Option<Theme>) -> Result<()> {
    let (ui_state_store, rollout_cache, history_store, mut app) =
        prepare_initial_tui(&config, theme_override);
    let terminal_enter_span = config.startup_trace.span("tui.terminal_enter");
    let _guard = TerminalGuard::enter()?;
    terminal_enter_span.finish("backend=crossterm");
    let terminal_setup_span = config.startup_trace.span("tui.terminal_setup");
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal_setup_span.finish("clear=true");

    let (sender, receiver) = mpsc::channel::<RefreshCompletion>();
    let (resume_sender, resume_receiver) = mpsc::channel::<ResumeLaunchCompletion>();
    let channels = RunLoopChannels {
        refresh_sender: &sender,
        refresh_receiver: &receiver,
        resume_sender: &resume_sender,
        resume_receiver: &resume_receiver,
    };
    let result = run_loop(
        &mut terminal,
        &mut app,
        &config,
        &channels,
        rollout_cache,
        Arc::clone(&history_store),
        &ui_state_store,
    );
    flush_staged_history_on_exit(&history_store, &config.perf_log);
    let _ = ui_state_store.save(&app.ui_state());
    config.perf_log.finish();
    terminal.show_cursor()?;
    result
}

fn prepare_initial_tui(
    config: &CollectConfig,
    theme_override: Option<Theme>,
) -> (
    UiStateStore,
    Arc<Mutex<RolloutCache>>,
    Arc<Mutex<HistoryStore>>,
    App,
) {
    let bootstrap_span = config.startup_trace.span("tui.bootstrap");
    let state_span = config.startup_trace.span("tui.ui_state_load");
    let mut ui_state_store = UiStateStore::discover();
    let ui_state = ui_state_store.load();
    state_span.finish("source=user_state");
    let open_config_span = config.startup_trace.span("tui.open_config_load");
    let open_config_store = OpenConfigStore::discover();
    let open_config_path_available = open_config_store.path().is_some();
    let (open_config, open_config_error) = match open_config_store.load_or_create() {
        Ok(open_config) => (open_config, None),
        Err(error) => {
            let message = open_config_store.path().map_or_else(
                || error.to_string(),
                |path| format!("{}: {error}", path.display()),
            );
            (OpenConfig::disabled(), Some(message))
        }
    };
    open_config_span.finish(format!(
        "path_available={} enabled={} status={}",
        open_config_path_available,
        open_config.enabled,
        if open_config_error.is_some() {
            "error"
        } else {
            "loaded"
        }
    ));
    let cache_span = config.startup_trace.span("tui.cache_create");
    let rollout_cache = Arc::new(Mutex::new(RolloutCache::new()));
    cache_span.finish(if config.rollout_cache_dir.is_some() {
        "kind=persistent"
    } else {
        "kind=in_memory"
    });
    let snapshot_span = config.startup_trace.span("tui.initial_snapshot");
    let initial = {
        let mut cache = rollout_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        collect_snapshot_cached(config, None, false, &mut cache)
    };
    snapshot_span.finish_with(|| {
        format!(
            "tasks={} turns={} files={} lines={}",
            initial.snapshot.tasks.len(),
            initial.snapshot.turns.len(),
            initial.snapshot.stats.scanned_files,
            initial.snapshot.stats.parsed_lines
        )
    });
    let history_span = config.startup_trace.span("tui.history_load");
    let mut history_store =
        HistoryStore::discover_with_redaction(&config.codex_home, config.redact_content);
    let initial_history_observation = collection_history_observation(&initial, config.offline);
    let (history, recorder_health) = stage_and_load_history(
        &mut history_store,
        initial_history_observation.as_ref(),
        initial.snapshot.as_of,
        &config.perf_log,
        true,
    );
    history_span.finish_with(|| {
        format!(
            "quota_points={} local_buckets={} warnings={} read_only={}",
            history.quota_points.len(),
            history.half_hour_buckets.len(),
            history.warnings.len(),
            history.read_only
        )
    });
    let app_span = config.startup_trace.span("tui.app_create");
    let initial_theme = theme_override.unwrap_or_else(|| ui_state.theme.into());
    let mut app = App::new(initial, initial_theme);
    app.replace_history(history);
    app.replace_recorder_health(recorder_health);
    app.apply_ui_state(&ui_state, theme_override);
    app.apply_open_config(open_config, open_config_error);
    app_span.finish_with(|| {
        format!(
            "theme={} turns_visible={} models_visible={} tree={}",
            match initial_theme {
                Theme::Dark => "dark",
                Theme::Light => "light",
            },
            app.turns_visible(),
            app.models_visible,
            matches!(app.task_list_mode, TaskListMode::Tree)
        )
    });
    bootstrap_span.finish("status=ready_to_render");
    (
        ui_state_store,
        rollout_cache,
        Arc::new(Mutex::new(history_store)),
        app,
    )
}

fn stage_and_load_history(
    store: &mut HistoryStore,
    observation: &HistoryObservation,
    now: DateTime<Utc>,
    perf_log: &PerfLog,
    force_flush: bool,
) -> (HistoryData, RecorderHealth) {
    stage_and_load_history_with_mode(store, observation, now, perf_log, force_flush, false)
}

fn stage_full_and_load_history(
    store: &mut HistoryStore,
    observation: &HistoryObservation,
    now: DateTime<Utc>,
    perf_log: &PerfLog,
) -> (HistoryData, RecorderHealth) {
    stage_and_load_history_with_mode(store, observation, now, perf_log, true, true)
}

fn stage_and_load_history_with_mode(
    store: &mut HistoryStore,
    observation: &HistoryObservation,
    now: DateTime<Utc>,
    perf_log: &PerfLog,
    force_flush: bool,
    full_observation: bool,
) -> (HistoryData, RecorderHealth) {
    let total_started = Instant::now();
    let stage_started = Instant::now();
    if full_observation {
        store.stage_full_observation(observation);
    } else {
        store.stage(observation);
    }
    let stage_elapsed = stage_started.elapsed();
    let record_started = Instant::now();
    let write_result = if force_flush {
        store.flush_staged()
    } else {
        store.flush_staged_if_due(HISTORY_FLUSH_INTERVAL)
    };
    let record_elapsed = record_started.elapsed();
    let load_started = Instant::now();
    let mut history = store.load_since_with_staged(history_view_since(now));
    let load_elapsed = load_started.elapsed();
    let mut metrics =
        HistoryMetrics::with_durations(total_started.elapsed(), record_elapsed, Some(load_elapsed));
    metrics.stage_us = u64::try_from(stage_elapsed.as_micros()).unwrap_or(u64::MAX);
    metrics.record_performed = match &write_result {
        Ok(report) => report.is_some(),
        Err(_) => true,
    };
    if let Ok(Some(report)) = &write_result {
        metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
        metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
        metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
        metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
        metrics.read_only = report.read_only;
    } else {
        metrics.warnings = 1;
    }
    metrics.quota_points = u64::try_from(history.quota_points.len()).unwrap_or(u64::MAX);
    metrics.local_buckets = u64::try_from(history.half_hour_buckets.len()).unwrap_or(u64::MAX);
    metrics.weekly_local_points =
        u64::try_from(history.weekly_local_points.len()).unwrap_or(u64::MAX);
    match write_result {
        Ok(Some(report)) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings);
        }
        Ok(None) => {}
        Err(error) => history
            .warnings
            .push(format!("history persistence failed: {error}")),
    }
    history.warnings.sort();
    history.warnings.dedup();
    if metrics.record_performed {
        perf_log.record_history(metrics);
    } else {
        perf_log.record_history_runtime(total_started.elapsed());
    }
    let recorder_health = load_recorder_health(store);
    (history, recorder_health)
}

fn flush_or_reload_history_if_due(
    store: &mut HistoryStore,
    now: DateTime<Utc>,
    perf_log: &PerfLog,
) -> Option<(HistoryData, RecorderHealth)> {
    let total_started = Instant::now();
    let record_started = Instant::now();
    let write_result = store.flush_staged_if_due(HISTORY_FLUSH_INTERVAL);
    let record_elapsed = record_started.elapsed();
    let record_performed = match &write_result {
        Ok(report) => report.is_some(),
        Err(_) => true,
    };
    let load_started = Instant::now();
    let reloaded = store.reload_since_if_stale_with_staged(history_view_since(now));
    let (mut history, load_elapsed) = match reloaded {
        Some(history) => (history, Some(load_started.elapsed())),
        None if record_performed => (
            store.load_since_with_staged(history_view_since(now)),
            Some(load_started.elapsed()),
        ),
        None => {
            perf_log.record_history_runtime(total_started.elapsed());
            return None;
        }
    };
    let mut metrics =
        HistoryMetrics::with_durations(total_started.elapsed(), record_elapsed, load_elapsed);
    metrics.record_performed = record_performed;
    if let Ok(Some(report)) = &write_result {
        metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
        metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
        metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
        metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
        metrics.read_only = report.read_only;
    } else if write_result.is_err() {
        metrics.warnings = 1;
    }
    metrics.quota_points = u64::try_from(history.quota_points.len()).unwrap_or(u64::MAX);
    metrics.local_buckets = u64::try_from(history.half_hour_buckets.len()).unwrap_or(u64::MAX);
    metrics.weekly_local_points =
        u64::try_from(history.weekly_local_points.len()).unwrap_or(u64::MAX);
    match write_result {
        Ok(Some(report)) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings);
        }
        Ok(None) => {}
        Err(error) => history
            .warnings
            .push(format!("history persistence failed: {error}")),
    }
    history.warnings.sort();
    history.warnings.dedup();
    metrics.warnings = metrics
        .warnings
        .max(u64::try_from(history.warnings.len()).unwrap_or(u64::MAX));
    metrics.read_only |= history.read_only;
    perf_log.record_history(metrics);
    let recorder_health = load_recorder_health(store);
    Some((history, recorder_health))
}

fn flush_staged_history_on_exit(history_store: &Arc<Mutex<HistoryStore>>, perf_log: &PerfLog) {
    let total_started = Instant::now();
    let mut store = history_store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let record_started = Instant::now();
    let write_result = store.flush_staged();
    if matches!(&write_result, Ok(None)) {
        return;
    }
    let mut metrics =
        HistoryMetrics::with_durations(total_started.elapsed(), record_started.elapsed(), None);
    metrics.record_performed = true;
    match write_result {
        Ok(Some(report)) => {
            metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
            metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
            metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
            metrics.read_only = report.read_only;
        }
        Ok(None) => {}
        Err(_) => metrics.warnings = 1,
    }
    perf_log.record_history(metrics);
}

fn history_view_since(now: DateTime<Utc>) -> DateTime<Utc> {
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let aligned_seconds = now.timestamp().div_euclid(bucket_seconds) * bucket_seconds;
    DateTime::from_timestamp(aligned_seconds, 0).unwrap_or(now)
        - ChronoDuration::days(SUMMARY_HISTORY_DAYS)
}

fn summary_backfill_config(config: &CollectConfig) -> CollectConfig {
    let mut config = config.clone();
    config.lookback_days = SUMMARY_HISTORY_DAYS;
    config.max_files = config.max_files.max(SUMMARY_BACKFILL_MAX_FILES);
    // The backfill only reconstructs local project history. Avoid a redundant
    // App Server request while the regular refresh worker owns account data.
    config.offline = true;
    config
}

fn summary_history_backfill_needed(history: &HistoryData, now: DateTime<Utc>) -> bool {
    if history.read_only {
        return false;
    }
    if history
        .summary_backfill_attempted_at
        .is_some_and(|attempted_at| {
            attempted_at > now
                || now - attempted_at < ChronoDuration::days(SUMMARY_BACKFILL_RETRY_DAYS)
        })
    {
        return false;
    }
    history.summary_backfill_attempt_complete == Some(false)
        || !summary_history_coverage_complete(history, now)
}

fn summary_history_coverage_complete(history: &HistoryData, now: DateTime<Utc>) -> bool {
    let ends_at = now
        .checked_add_signed(ChronoDuration::nanoseconds(1))
        .unwrap_or(now);
    let Some(window) = SummaryWindow::new(now - ChronoDuration::days(30), ends_at) else {
        return true;
    };
    let expected = expected_summary_daily_coverage(window)
        .values()
        .map(|coverage| coverage.expected_buckets)
        .fold(0_usize, usize::saturating_add);
    let covered = history
        .half_hour_buckets
        .iter()
        .filter(|bucket| {
            window.contains(bucket.starts_at)
                && bucket.project_breakdown_revision == HISTORY_PROJECT_BREAKDOWN_REVISION
                && bucket.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION
                && bucket.estimator_revision == HISTORY_ESTIMATOR_REVISION
                && bucket.api_long_context_extra_cost_units.is_some()
        })
        .count();
    covered >= expected
}

fn summary_backfill_scan_complete(snapshot: &Snapshot) -> bool {
    let stats = &snapshot.stats;
    stats.truncated_files == 0
        && stats.unreadable_files == 0
        && stats.skipped_lines == 0
        && stats.ambiguous_token_resets == 0
        && stats.scanned_files == stats.discovered_files
        && snapshot
            .sources
            .iter()
            .any(|source| source.source == "rollout_jsonl" && source.status == "ok")
}

fn retain_summary_backfill_evidence_buckets(observation: &mut HistoryObservation) {
    // A complete filesystem walk proves that every retained rollout was read; it
    // cannot prove that an absent rollout never existed or was not deleted. A
    // backfill therefore persists only buckets backed by a model call. Zero
    // buckets recorded prospectively by the recorder remain in HistoryStore and
    // are not deleted by this merge. Spark calls do not increment the bucket-level
    // call count, but still create a project group, so either signal is evidence.
    observation
        .half_hour_buckets
        .retain(|bucket| bucket.call_count > 0 || !bucket.project_groups.is_empty());
}

fn load_recorder_health(store: &HistoryStore) -> RecorderHealth {
    let Some(history_root) = store.history_root() else {
        return RecorderHealth {
            status: None,
            error: Some("recorder state directory is unavailable".to_string()),
        };
    };
    let path = default_status_file(history_root);
    match read_recorder_status(&path) {
        Ok(Some(status))
            if status
                .history_namespace
                .as_deref()
                .is_some_and(|namespace| namespace != store.namespace()) =>
        {
            RecorderHealth {
                status: None,
                error: Some(format!(
                    "recorder targets history namespace {}, expected {}",
                    status.history_namespace.as_deref().unwrap_or("unknown"),
                    store.namespace()
                )),
            }
        }
        Ok(status) => RecorderHealth {
            status,
            error: None,
        },
        Err(error) => RecorderHealth {
            status: None,
            error: Some(format!("{}: {error}", path.display())),
        },
    }
}

pub fn debug_startup(
    config: CollectConfig,
    theme_override: Option<Theme>,
    width: u16,
    height: u16,
) -> Result<()> {
    ensure!(
        u32::from(width) * u32::from(height) <= MAX_DEBUG_STARTUP_CELLS,
        "debug-startup canvas exceeds {MAX_DEBUG_STARTUP_CELLS} cells"
    );
    let trace = config.startup_trace.clone();
    let (_ui_state_store, _rollout_cache, _history_store, mut app) =
        prepare_initial_tui(&config, theme_override);
    let terminal_span = trace.span("tui.headless_terminal_setup");
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal_span.finish_with(|| format!("width={width} height={height}"));
    let draw_span = trace.span("tui.first_frame");
    terminal.draw(|frame| render(frame, &mut app))?;
    draw_span.finish_with(|| format!("backend=test width={width} height={height}"));
    trace.finish("startup.ready", "mode=debug_startup backend=test");
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    config: &CollectConfig,
    channels: &RunLoopChannels<'_>,
    rollout_cache: Arc<Mutex<RolloutCache>>,
    history_store: Arc<Mutex<HistoryStore>>,
    ui_state_store: &UiStateStore,
) -> Result<()> {
    let mut first_frame = true;
    let mut redraw_reasons = RedrawReasons::default();
    let mut refresh_worker = RefreshWorker::default();
    loop {
        while let Ok(completion) = channels.refresh_receiver.try_recv() {
            let mut refresh_changed = false;
            if completion.summary_backfill {
                app.summary_backfill_running = false;
            }
            if let Some(result) = completion.result {
                app.replace(result, completion.refreshed_account);
                refresh_changed = true;
            } else {
                app.finish_unchanged_refresh();
            }
            if let Some(history) = completion.history {
                app.replace_history(history);
                refresh_changed = true;
            }
            if let Some(recorder_health) = completion.recorder_health {
                app.replace_recorder_health(recorder_health);
                refresh_changed = true;
            }
            if refresh_changed {
                redraw_reasons.insert(RedrawReasons::SNAPSHOT);
            }
            refresh_worker.join();
        }
        while let Ok(completion) = channels.resume_receiver.try_recv() {
            app.apply_resume_completion(completion);
            redraw_reasons.insert(RedrawReasons::RESUME);
        }
        if app.expire_open_notice_at(Instant::now()) {
            redraw_reasons.insert(RedrawReasons::NOTICE);
        }

        if first_frame {
            let draw_span = config.startup_trace.span("tui.first_frame");
            let draw_started = Instant::now();
            terminal.draw(|frame| render(frame, app))?;
            config.perf_log.record_draw(draw_started.elapsed());
            draw_span.finish("backend=crossterm");
            config
                .startup_trace
                .finish("startup.ready", "mode=tui backend=crossterm");
            first_frame = false;
            redraw_reasons.clear();
        } else if !redraw_reasons.is_empty() {
            let reasons = redraw_reasons;
            let draw_span = config.startup_trace.span("tui.draw");
            let draw_started = Instant::now();
            terminal.draw(|frame| render(frame, app))?;
            config.perf_log.record_draw(draw_started.elapsed());
            draw_span.finish_with(|| format!("backend=crossterm reason={}", reasons.label()));
            redraw_reasons.clear();
        }

        if start_refresh_if_due(
            app,
            config,
            channels,
            &rollout_cache,
            &history_store,
            &mut refresh_worker,
        ) {
            redraw_reasons.insert(RedrawReasons::SNAPSHOT);
        }

        if event::poll(next_run_loop_poll_timeout(
            app,
            Instant::now(),
            !config.offline,
        ))? {
            config.perf_log.record_event_wakeup();
            let previous_ui_state = app.ui_state();
            let mut should_quit = false;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    redraw_reasons.insert(RedrawReasons::INPUT);
                    if handle_key_event(app, key) {
                        should_quit = true;
                    }
                }
                Event::Mouse(mouse) => {
                    let kind = mouse.kind;
                    let handled = handle_mouse_event(app, mouse);
                    if mouse_event_requests_redraw(kind, handled) {
                        redraw_reasons.insert(RedrawReasons::INPUT);
                    }
                    if app.quit_requested {
                        should_quit = true;
                    }
                }
                Event::Resize(_, _) => redraw_reasons.insert(RedrawReasons::RESIZE),
                _ => {}
            }
            let current_ui_state = app.ui_state();
            if current_ui_state != previous_ui_state {
                let _ = ui_state_store.save(&current_ui_state);
            }
            if should_quit {
                refresh_worker.detach();
                return Ok(());
            }
        }

        if let Some(request) = app.pending_resume.take() {
            let worker_sender = channels.resume_sender.clone();
            thread::spawn(move || {
                let _ = worker_sender.send(execute_resume_request(request));
            });
        }

        if let Some(request) = app.pending_clipboard.take() {
            let result = write_osc52_clipboard(terminal.backend_mut(), &request.text);
            app.apply_clipboard_result(request, result);
        }
        config.perf_log.maybe_sample();
    }
}

fn start_refresh_if_due(
    app: &mut App,
    config: &CollectConfig,
    channels: &RunLoopChannels<'_>,
    rollout_cache: &Arc<Mutex<RolloutCache>>,
    history_store: &Arc<Mutex<HistoryStore>>,
    refresh_worker: &mut RefreshWorker,
) -> bool {
    let now = Instant::now();
    let local_refresh_due = now.saturating_duration_since(app.last_local_refresh) >= LOCAL_REFRESH;
    let account_refresh_due = !config.offline && app.account_refresh_due(now);
    if local_refresh_due
        && app.view == View::Summary
        && app.summary_range == SummaryRange::ThirtyDays
        && !app.summary_backfill_running
    {
        let query_now = Utc::now().max(app.snapshot.as_of);
        app.summary_backfill_pending = summary_history_backfill_needed(&app.history, query_now);
    }
    let summary_backfill_due = app.view == View::Summary
        && app.summary_range == SummaryRange::ThirtyDays
        && app.summary_backfill_pending
        && !account_refresh_due;
    if app.worker_running || (!summary_backfill_due && !local_refresh_due && !account_refresh_due) {
        return false;
    }

    if summary_backfill_due {
        let worker_config = summary_backfill_config(config);
        let worker_sender = channels.refresh_sender.clone();
        let worker_history = Arc::clone(history_store);
        app.worker_running = true;
        app.summary_backfill_pending = false;
        app.summary_backfill_running = true;
        refresh_worker.start(thread::spawn(move || {
            let mut cache = RolloutCache::new();
            let result = collect_snapshot_cached(&worker_config, None, false, &mut cache);
            let scan_complete = summary_backfill_scan_complete(&result.snapshot);
            let CollectionResult {
                snapshot,
                account,
                mut history_observation,
            } = result;
            let observed_at = snapshot.as_of;
            // Summary reconstruction is deliberately local-only. Never let
            // offline fallback quota/weekly points replace server history.
            history_observation.quota_points.clear();
            history_observation.weekly_local_points.clear();
            retain_summary_backfill_evidence_buckets(&mut history_observation);
            drop(snapshot);
            drop(account);
            drop(cache);
            let (mut history, recorder_health) = {
                let mut history_store = worker_history
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let (mut history, recorder_health) = stage_full_and_load_history(
                    &mut history_store,
                    &history_observation,
                    observed_at,
                    &worker_config.perf_log,
                );
                let coverage_complete = summary_history_coverage_complete(&history, observed_at);
                match history_store
                    .mark_summary_backfill_attempt(observed_at, scan_complete && coverage_complete)
                {
                    Ok(()) => {
                        history.summary_backfill_attempted_at = Some(observed_at);
                        history.summary_backfill_attempt_complete =
                            Some(scan_complete && coverage_complete);
                    }
                    Err(error) => {
                        // Keep an in-memory cooldown even when the durable
                        // marker cannot be written, otherwise a read-only or
                        // full state directory would trigger an immediate
                        // expensive rescan loop.
                        history.summary_backfill_attempted_at = Some(observed_at);
                        history.summary_backfill_attempt_complete =
                            Some(scan_complete && coverage_complete);
                        history
                            .warnings
                            .push(format!("summary backfill marker failed: {error}"));
                    }
                }
                (history, recorder_health)
            };
            history.warnings.sort();
            history.warnings.dedup();
            let _ = worker_sender.send(RefreshCompletion {
                result: None,
                history: Some(history),
                recorder_health: Some(recorder_health),
                refreshed_account: false,
                summary_backfill: true,
            });
        }));
        return true;
    }

    let worker_config = config.clone();
    let cached_account = app.account.clone();
    let worker_sender = channels.refresh_sender.clone();
    let worker_cache = Arc::clone(rollout_cache);
    let worker_history = Arc::clone(history_store);
    app.worker_running = true;
    refresh_worker.start(thread::spawn(move || {
        let result = {
            let mut cache = worker_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if account_refresh_due {
                Some(collect_snapshot_cached(
                    &worker_config,
                    Some(cached_account),
                    true,
                    &mut cache,
                ))
            } else {
                collect_snapshot_cached_if_changed(&worker_config, Some(cached_account), &mut cache)
            }
        };
        let history_and_recorder = {
            let mut history_store = worker_history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match result.as_ref() {
                Some(result) => {
                    let history_observation =
                        collection_history_observation(result, worker_config.offline);
                    Some(stage_and_load_history(
                        &mut history_store,
                        history_observation.as_ref(),
                        result.snapshot.as_of,
                        &worker_config.perf_log,
                        false,
                    ))
                }
                None => flush_or_reload_history_if_due(
                    &mut history_store,
                    Utc::now(),
                    &worker_config.perf_log,
                ),
            }
        };
        let (history, recorder_health) = history_and_recorder
            .map_or((None, None), |(history, recorder_health)| {
                (Some(history), Some(recorder_health))
            });
        let _ = worker_sender.send(RefreshCompletion {
            result,
            history,
            recorder_health,
            refreshed_account: account_refresh_due,
            summary_backfill: false,
        });
    }));
    false
}

fn mouse_event_requests_redraw(kind: MouseEventKind, handled: bool) -> bool {
    !matches!(kind, MouseEventKind::Moved)
        && (handled || matches!(kind, MouseEventKind::Down(MouseButton::Left)))
}

fn next_run_loop_poll_timeout(app: &App, now: Instant, account_refresh_enabled: bool) -> Duration {
    let local_refresh_wait =
        LOCAL_REFRESH.saturating_sub(now.saturating_duration_since(app.last_local_refresh));
    let mut timeout = if app.worker_running {
        BACKGROUND_CHANNEL_POLL
    } else {
        local_refresh_wait
    };
    if account_refresh_enabled && !app.worker_running {
        timeout = timeout.min(app.next_account_refresh.saturating_duration_since(now));
    }
    if !app.launching_threads.is_empty() {
        timeout = timeout.min(BACKGROUND_CHANNEL_POLL);
    }
    if let Some(notice) = app.open_notice.as_ref() {
        let notice_wait =
            OPEN_NOTICE_DURATION.saturating_sub(now.saturating_duration_since(notice.created_at));
        timeout = timeout.min(notice_wait);
    }
    timeout
}

fn write_osc52_clipboard<W: Write>(writer: &mut W, text: &str) -> io::Result<()> {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "clipboard text exceeds the {} KiB limit",
                MAX_CLIPBOARD_TEXT_BYTES / 1024
            ),
        ));
    }
    let payload = BASE64_STANDARD.encode(text.as_bytes());
    writer.write_all(b"\x1b]52;c;")?;
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\x07")?;
    writer.flush()
}

fn execute_resume_request(request: ResumeLaunchRequest) -> ResumeLaunchCompletion {
    let (thread_id, result) = match request {
        ResumeLaunchRequest::Create {
            target,
            codex_home,
            codex_bin,
            options,
        } => {
            let thread_id = target.thread_id.clone();
            let result = (|| -> Result<ResumeLaunchOutcome, String> {
                let context = LaunchContext::capture(codex_home, codex_bin)
                    .map_err(|error| error.to_string())?;
                let plan = prepare_zellij_launch(&target, &context, &options)
                    .map_err(|error| error.to_string())?;
                match execute_zellij_launch(&plan).map_err(|error| error.to_string())? {
                    LaunchResult::Created { pane_id } => Ok(ResumeLaunchOutcome::Created(pane_id)),
                }
            })();
            (thread_id, result)
        }
        ResumeLaunchRequest::Focus {
            thread_id,
            pane_id,
            codex_home,
        } => {
            let result = (|| -> Result<ResumeLaunchOutcome, String> {
                let context =
                    LaunchContext::capture(codex_home, None).map_err(|error| error.to_string())?;
                let zellij_bin =
                    prepare_zellij_focus(&context).map_err(|error| error.to_string())?;
                match focus_existing_pane(&zellij_bin, &pane_id)
                    .map_err(|error| error.to_string())?
                {
                    FocusResult::Focused => Ok(ResumeLaunchOutcome::Focused(pane_id)),
                    FocusResult::Missing => Ok(ResumeLaunchOutcome::Missing(pane_id)),
                }
            })();
            (thread_id, result)
        }
    };
    ResumeLaunchCompletion { thread_id, result }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    render_at(frame, app, Utc::now());
}

fn render_at(frame: &mut Frame<'_>, app: &mut App, now: DateTime<Utc>) {
    let area = frame.area();
    app.task_table_hitbox = None;
    app.turn_table_hitbox = None;
    app.task_controls_hitbox = None;
    app.task_tree_marker_hitboxes.clear();
    app.turn_controls_hitbox = None;
    app.window_controls_hitbox = None;
    app.settings_controls_hitbox = None;
    app.trend_controls_hitbox = None;
    app.trend_chart_hitboxes.clear();
    app.summary_controls_hitbox = None;
    app.summary_table_hitbox = None;
    app.summary_tree_marker_hitboxes.clear();
    app.summary_bar_hitboxes.clear();
    app.summary_daily_hitbox = None;
    app.summary_scrollbar_hitbox = None;
    app.view_tabs_hitbox = None;
    app.task_scrollbar_hitbox = None;
    app.turn_scrollbar_hitbox = None;
    app.quit_confirmation_hitbox = None;
    app.resume_confirmation_hitbox = None;
    let palette = app.theme.palette();
    frame.render_widget(Block::default().style(app.theme.base_style()), area);
    let initial_tab_area = Rect::new(area.x, area.y, area.width, u16::from(area.height > 0));
    let initial_tabs = view_tabs_hitbox(initial_tab_area);
    let controls_on_second_row = app.view == View::Overview
        && usize::from(area.right().saturating_sub(initial_tabs.rendered_right))
            < overview_controls_min_width()
        && area.height > 2;
    let header_height = 1 + u16::from(controls_on_second_row);
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(1)])
        .split(area);
    let tab_area = Rect::new(root[0].x, root[0].y, root[0].width, 1);
    let compact_tabs = view_tabs_compact(tab_area.width);

    let titles = View::ALL
        .into_iter()
        .map(|view| {
            let selected = view == app.view;
            let shortcut_active = app.shortcuts_active();
            let label = if compact_tabs {
                view.compact_label()
            } else {
                view.label()
            };
            Line::from(vec![
                Span::styled(
                    view.shortcut().to_string(),
                    Style::default()
                        .fg(if shortcut_active {
                            palette.accent
                        } else {
                            palette.muted
                        })
                        .add_modifier(if shortcut_active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw(" "),
                Span::styled(
                    label,
                    Style::default()
                        .fg(if selected {
                            palette.title
                        } else {
                            palette.muted
                        })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let tabs = Tabs::new(titles)
        .select(app.view.index())
        .style(Style::default().fg(palette.muted))
        .highlight_style(Style::default())
        .padding(TAB_PADDING, TAB_PADDING)
        .divider(Span::styled(
            TAB_DIVIDER,
            Style::default().fg(palette.muted),
        ));
    app.view_tabs_hitbox = Some(view_tabs_hitbox(tab_area));
    frame.render_widget(tabs, tab_area);
    if app.view == View::Overview {
        app.window_controls_hitbox = Some(if controls_on_second_row {
            let controls_area = Rect::new(root[0].x, root[0].y + 1, root[0].width, 1);
            render_overview_controls_from(frame, controls_area, app, controls_area.x)
        } else {
            render_overview_controls(frame, tab_area, app)
        });
    }

    match app.view {
        View::Overview => render_overview(frame, root[1], app),
        View::Trends => render_trends_at(frame, root[1], app, now),
        View::Summary => render_summary_at(frame, root[1], app, now),
        View::Health => render_health(frame, root[1], app),
        View::Settings => render_settings(frame, root[1], app),
    };
    if app.trend_drag.is_some_and(|drag| {
        !app.trend_chart_hitboxes
            .iter()
            .any(|hitbox| hitbox.panel == drag.panel)
    }) {
        app.trend_drag = None;
    }
    if app
        .scroll_drag
        .is_some_and(|drag| app.scrollbar_hitbox(drag.target).is_none())
    {
        app.scroll_drag = None;
    }
    if app.resume_confirmation.is_some() {
        app.resume_confirmation_hitbox = Some(render_resume_confirmation(frame, area, app));
    } else if app.quit_confirmation_visible {
        app.quit_confirmation_hitbox = Some(render_quit_confirmation(frame, area, app.theme));
    }
}

fn render_overview_controls(frame: &mut Frame<'_>, area: Rect, app: &App) -> WindowControlsHitbox {
    let tabs = view_tabs_hitbox(area);
    let start_x = tabs.rendered_right;
    render_overview_controls_from(frame, area, app, start_x)
}

fn overview_controls_min_width() -> usize {
    UnicodeWidthStr::width(" [V][M][5][W][L]")
}

fn render_overview_controls_from(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    start_x: u16,
) -> WindowControlsHitbox {
    let palette = app.theme.palette();
    let remaining = usize::from(area.right().saturating_sub(start_x));
    let full_long_context_label = "[L]EST Longx";
    let full_width = UnicodeWidthStr::width(" | [V]Turns [M]Models [5h] [Week] ")
        + UnicodeWidthStr::width(full_long_context_label);
    let compact = remaining < full_width;
    let standalone = start_x <= area.x;
    let separator = if standalone {
        ""
    } else if compact {
        " "
    } else {
        TAB_DIVIDER
    };
    let gap = if compact { "" } else { " " };
    let separator_width = u16::try_from(UnicodeWidthStr::width(separator)).unwrap_or(u16::MAX);
    let gap_width = u16::try_from(UnicodeWidthStr::width(gap)).unwrap_or(u16::MAX);
    let mut spans = Vec::new();
    let mut x = start_x;
    let shortcuts_active = app.shortcuts_active();

    let turns_label = if compact { "[V]" } else { "[V]Turns" };
    let turns_width = u16::try_from(UnicodeWidthStr::width(turns_label)).unwrap_or(u16::MAX);
    let mut toggle_turns = Rect::default();
    if x.saturating_add(separator_width)
        .saturating_add(turns_width)
        <= area.right()
    {
        spans.push(Span::styled(separator, Style::default().fg(palette.muted)));
        x = x.saturating_add(separator_width);
        toggle_turns = clipped_horizontal_hitbox(area, x, turns_width);
        let turns_style = if app.turns_default_visible {
            Style::default()
                .fg(palette.background)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        let turns_shortcut_style = if !shortcuts_active {
            turns_style
        } else if app.turns_default_visible {
            turns_style.add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled("[", turns_style));
        spans.push(Span::styled("V", turns_shortcut_style));
        spans.push(Span::styled(
            if compact { "]" } else { "]Turns" },
            turns_style,
        ));
        x = x.saturating_add(turns_width);
    }

    let models_label = if compact { "[M]" } else { "[M]Models" };
    let models_width = u16::try_from(UnicodeWidthStr::width(models_label)).unwrap_or(u16::MAX);
    let mut toggle_models = Rect::default();
    if !toggle_turns.is_empty()
        && x.saturating_add(gap_width).saturating_add(models_width) <= area.right()
    {
        spans.push(Span::raw(gap));
        x = x.saturating_add(gap_width);
        toggle_models = clipped_horizontal_hitbox(area, x, models_width);
        let models_style = if app.models_visible {
            Style::default()
                .fg(palette.background)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        let models_shortcut_style = if !shortcuts_active {
            models_style
        } else if app.models_visible {
            models_style.add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled("[", models_style));
        spans.push(Span::styled("M", models_shortcut_style));
        spans.push(Span::styled(
            if compact { "]" } else { "]Models" },
            models_style,
        ));
        x = x.saturating_add(models_width);
    }

    let mut scopes = [Rect::default(); 2];
    for scope in WindowScope::ALL {
        let label = if compact {
            scope.shortcut().to_string()
        } else {
            scope.label().to_string()
        };
        let width = u16::try_from(UnicodeWidthStr::width(label.as_str()) + 2).unwrap_or(u16::MAX);
        if toggle_turns.is_empty()
            || toggle_models.is_empty()
            || x.saturating_add(gap_width).saturating_add(width) > area.right()
        {
            break;
        }
        spans.push(Span::raw(gap));
        x = x.saturating_add(gap_width);
        scopes[scope.index()] = clipped_horizontal_hitbox(area, x, width);
        let selected = app.window_scope == scope;
        let style = if selected {
            Style::default()
                .fg(palette.background)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        let shortcut_style = if !shortcuts_active {
            style
        } else if selected {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        };
        let mut label_chars = label.chars();
        let _ = label_chars.next();
        spans.push(Span::styled("[", style));
        spans.push(Span::styled(scope.shortcut().to_string(), shortcut_style));
        spans.push(Span::styled(
            format!("{}]", label_chars.collect::<String>()),
            style,
        ));
        x = x.saturating_add(width);
    }

    let api_long_context_label = if compact {
        "[L]"
    } else {
        full_long_context_label
    };
    let api_long_context_width =
        u16::try_from(UnicodeWidthStr::width(api_long_context_label)).unwrap_or(u16::MAX);
    let mut toggle_api_long_context = Rect::default();
    if scopes.iter().all(|scope| !scope.is_empty())
        && x.saturating_add(gap_width)
            .saturating_add(api_long_context_width)
            <= area.right()
    {
        spans.push(Span::raw(gap));
        x = x.saturating_add(gap_width);
        toggle_api_long_context = clipped_horizontal_hitbox(area, x, api_long_context_width);
        let style = if app.api_long_context_multiplier {
            Style::default()
                .fg(palette.background)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        let shortcut_style = if !shortcuts_active {
            style
        } else if app.api_long_context_multiplier {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled("[", style));
        spans.push(Span::styled("L", shortcut_style));
        spans.push(Span::styled(
            if compact { "]" } else { "]EST Longx" },
            style,
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(
            start_x.min(area.right()),
            area.y,
            area.right().saturating_sub(start_x),
            u16::from(area.height > 0),
        ),
    );
    WindowControlsHitbox {
        toggle_turns,
        toggle_models,
        scopes,
        toggle_api_long_context,
    }
}

fn render_resume_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
) -> ResumeConfirmationHitbox {
    let palette = app.theme.palette();
    let popup_width = area.width.min(88);
    let popup_height = area.height.min(12);
    let popup = Rect::new(
        area.x
            .saturating_add(area.width.saturating_sub(popup_width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(popup_height) / 2),
        popup_width,
        popup_height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .style(app.theme.base_style())
        .title(Span::styled(
            " Resume in new Codex terminal? ",
            Style::default()
                .fg(palette.title)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let thread_id = app
        .resume_confirmation
        .as_ref()
        .map(|confirmation| confirmation.thread_id.as_str())
        .unwrap_or_default();
    let task = app
        .snapshot
        .tasks
        .iter()
        .find(|task| task.thread_id == thread_id);
    let content_height = inner.height.saturating_sub(2);
    let mut confirmation_content_fits = false;
    if content_height > 0 {
        let width = usize::from(inner.width);
        let mut lines = if let Some(task) = task {
            let source = task.source.as_deref().unwrap_or("unknown");
            let cwd = task
                .cwd
                .as_deref()
                .map(|path| terminal_safe_text(path.to_string_lossy().as_ref()))
                .unwrap_or_else(|| "-".to_string());
            let target = if !app.zellij_environment {
                "clipboard · run in another terminal".to_string()
            } else if app.open_config.zellij.floating {
                format!(
                    "zellij floating pane · {}% x {}% · current session",
                    app.open_config.zellij.width_percent, app.open_config.zellij.height_percent
                )
            } else {
                "zellij pane · current session".to_string()
            };
            let cwd_label = "Cwd:     ";
            let cwd = truncate_middle_display_text(
                &cwd,
                width.saturating_sub(UnicodeWidthStr::width(cwd_label)),
            );
            vec![
                Line::from(truncate_display_text(
                    &format!("Task:    {}", terminal_safe_text(&task.title)),
                    width,
                )),
                Line::from(truncate_display_text(
                    &format!("Thread:  {}", terminal_safe_text(&task.thread_id)),
                    width,
                )),
                Line::from(truncate_display_text(
                    &format!("Source:  {}", terminal_safe_text(source)),
                    width,
                )),
                Line::from(truncate_display_text(
                    &format!(
                        "Status:  {} · {}",
                        task.status.label(),
                        status_evidence(task.status_provenance, task.status_confidence)
                    ),
                    width,
                )),
                Line::from(format!("{cwd_label}{cwd}")),
                Line::from(truncate_display_text(&format!("Target:  {target}"), width)),
            ]
        } else {
            vec![Line::styled(
                truncate_display_text(
                    &format!(
                        "Task is no longer available: {}",
                        terminal_safe_text(thread_id)
                    ),
                    width,
                ),
                Style::default().fg(palette.error),
            )]
        };
        if task.is_some_and(|task| {
            matches!(task.status, TaskStatus::Stale | TaskStatus::Unknown)
                || matches!(
                    task.status_confidence,
                    Confidence::Low | Confidence::Unknown
                )
        }) {
            lines.push(Line::styled(
                truncate_display_text(
                    "Status is uncertain; another frontend may still be active.",
                    width,
                ),
                Style::default().fg(palette.warning),
            ));
        }
        let copy_error = app
            .resume_confirmation
            .as_ref()
            .and_then(|confirmation| confirmation.copy_error.as_deref());
        if let Some(error) = copy_error {
            lines.push(Line::styled(
                truncate_display_text(
                    &format!("Copy failed: {}", terminal_safe_text(error)),
                    width,
                ),
                Style::default().fg(palette.error),
            ));
        } else {
            let instruction = if app.zellij_environment {
                "Open creates a new CLI frontend; Copy prepares the command."
            } else {
                "Copy the command, then run it in a new terminal."
            };
            lines.push(Line::styled(
                truncate_display_text(instruction, width),
                Style::default().fg(palette.warning),
            ));
        }
        confirmation_content_fits = task.is_some()
            && inner.width >= RESUME_CONFIRM_MIN_INNER_WIDTH
            && usize::from(content_height) >= lines.len();
        if task.is_some() && !confirmation_content_fits {
            lines = vec![Line::styled(
                truncate_display_text("Resize terminal to review cwd and confirm.", width),
                Style::default().fg(palette.warning),
            )];
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y, inner.width, content_height),
        );
    }

    let full = inner.width >= RESUME_CONFIRM_MIN_INNER_WIDTH;
    let confirm_label = if full { "[↵] Open" } else { "[↵]" };
    let copy_label = if full { "[C] Copy" } else { "[C]" };
    let cancel_label = if full { "[Esc] Cancel" } else { "[Esc]" };
    let gap = if full { "   " } else { " " };
    let confirm_width = u16::try_from(UnicodeWidthStr::width(confirm_label)).unwrap_or(u16::MAX);
    let copy_width = u16::try_from(UnicodeWidthStr::width(copy_label)).unwrap_or(u16::MAX);
    let cancel_width = u16::try_from(UnicodeWidthStr::width(cancel_label)).unwrap_or(u16::MAX);
    let gap_width = u16::try_from(UnicodeWidthStr::width(gap)).unwrap_or(u16::MAX);
    let button_count = if app.zellij_environment { 3u16 } else { 2u16 };
    let controls_width = copy_width
        .saturating_add(cancel_width)
        .saturating_add(if app.zellij_environment {
            confirm_width
        } else {
            0
        })
        .saturating_add(gap_width.saturating_mul(button_count.saturating_sub(1)));
    let button_y = inner.bottom().saturating_sub(1);
    let button_style = Style::default()
        .fg(palette.foreground)
        .bg(palette.gauge_track);
    let shortcut_style = button_style.fg(palette.accent).add_modifier(Modifier::BOLD);
    let mut confirm = Rect::default();
    let mut copy = Rect::default();
    let mut cancel = Rect::default();
    if confirmation_content_fits && inner.height > 0 && controls_width <= inner.width {
        let group_x = inner
            .x
            .saturating_add(inner.width.saturating_sub(controls_width) / 2);
        let copy_x = if app.zellij_environment {
            confirm = Rect::new(group_x, button_y, confirm_width, 1);
            group_x
                .saturating_add(confirm_width)
                .saturating_add(gap_width)
        } else {
            group_x
        };
        copy = Rect::new(copy_x, button_y, copy_width, 1);
        cancel = Rect::new(
            copy_x.saturating_add(copy_width).saturating_add(gap_width),
            button_y,
            cancel_width,
            1,
        );
        let mut spans = Vec::new();
        if app.zellij_environment {
            spans.extend([
                Span::styled("[", button_style),
                Span::styled("↵", shortcut_style),
                Span::styled(if full { "] Open" } else { "]" }, button_style),
                Span::raw(gap),
            ]);
        }
        spans.extend([
            Span::styled("[", button_style),
            Span::styled("C", shortcut_style),
            Span::styled(if full { "] Copy" } else { "]" }, button_style),
            Span::raw(gap),
            Span::styled("[", button_style),
            Span::styled("Esc", shortcut_style),
            Span::styled(if full { "] Cancel" } else { "]" }, button_style),
        ]);
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(group_x, button_y, controls_width, 1),
        );
    } else if inner.height > 0 && cancel_width <= inner.width {
        let cancel_x = inner
            .x
            .saturating_add(inner.width.saturating_sub(cancel_width) / 2);
        cancel = Rect::new(cancel_x, button_y, cancel_width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("[", button_style),
                Span::styled("Esc", shortcut_style),
                Span::styled(if full { "] Cancel" } else { "]" }, button_style),
            ])),
            cancel,
        );
    }

    ResumeConfirmationHitbox {
        confirm,
        copy,
        cancel,
    }
}

fn render_quit_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: Theme,
) -> QuitConfirmationHitbox {
    let palette = theme.palette();
    let popup_width = area.width.min(44);
    let popup_height = area.height.min(7);
    let popup = Rect::new(
        area.x
            .saturating_add(area.width.saturating_sub(popup_width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(popup_height) / 2),
        popup_width,
        popup_height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .style(theme.base_style())
        .title(Span::styled(
            " Quit? ",
            Style::default()
                .fg(palette.title)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    if !inner.is_empty() {
        let message_y = inner
            .y
            .saturating_add(u16::from(inner.height > 2))
            .min(inner.bottom().saturating_sub(1));
        frame.render_widget(
            Paragraph::new("Exit codex-usage-monit?")
                .style(Style::default().fg(palette.foreground))
                .alignment(Alignment::Center),
            Rect::new(inner.x, message_y, inner.width, 1),
        );
    }

    let full = inner.width >= 23;
    let confirm_label = if full { "[↵] Exit" } else { "[↵]" };
    let cancel_label = if full { "[Esc] Cancel" } else { "[Esc]" };
    let gap = if full { "   " } else { " " };
    let confirm_width = u16::try_from(UnicodeWidthStr::width(confirm_label)).unwrap_or(u16::MAX);
    let cancel_width = u16::try_from(UnicodeWidthStr::width(cancel_label)).unwrap_or(u16::MAX);
    let gap_width = u16::try_from(UnicodeWidthStr::width(gap)).unwrap_or(u16::MAX);
    let both_width = confirm_width
        .saturating_add(gap_width)
        .saturating_add(cancel_width);
    let button_y = inner.bottom().saturating_sub(1);
    let button_row = Rect::new(inner.x, button_y, inner.width, u16::from(inner.height > 0));
    let button_style = Style::default()
        .fg(palette.foreground)
        .bg(palette.gauge_track);
    let shortcut_style = button_style.fg(palette.accent).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut confirm = Rect::default();
    let mut cancel = Rect::default();
    let (group_x, group_width) = if button_row.height > 0 && both_width <= inner.width {
        let group_x = inner
            .x
            .saturating_add(inner.width.saturating_sub(both_width) / 2);
        confirm = Rect::new(group_x, button_y, confirm_width, 1);
        cancel = Rect::new(
            group_x
                .saturating_add(confirm_width)
                .saturating_add(gap_width),
            button_y,
            cancel_width,
            1,
        );
        spans.extend([
            Span::styled("[", button_style),
            Span::styled("↵", shortcut_style),
            Span::styled(if full { "] Exit" } else { "]" }, button_style),
            Span::raw(gap),
            Span::styled("[", button_style),
            Span::styled("Esc", shortcut_style),
            Span::styled(if full { "] Cancel" } else { "]" }, button_style),
        ]);
        (group_x, both_width)
    } else if button_row.height > 0 && confirm_width <= inner.width {
        let group_x = inner
            .x
            .saturating_add(inner.width.saturating_sub(confirm_width) / 2);
        confirm = Rect::new(group_x, button_y, confirm_width, 1);
        spans.extend([
            Span::styled("[", button_style),
            Span::styled("↵", shortcut_style),
            Span::styled(if full { "] Exit" } else { "]" }, button_style),
        ]);
        (group_x, confirm_width)
    } else {
        (inner.x, 0)
    };
    if group_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(group_x, button_y, group_width, 1),
        );
    }

    QuitConfirmationHitbox { confirm, cancel }
}

fn render_overview(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let compact = area.height < 30;
    let base_quota_height = if compact { 3 } else { 5 };
    let quota_height = overview_quota_height(&app.snapshot, area.width, base_quota_height);
    let app_server_failed = app_server_call_failed(&app.snapshot);
    let notice_height = u16::from(app_server_failed);
    let mut constraints = vec![Constraint::Length(quota_height)];
    constraints.push(Constraint::Min(9));
    if app_server_failed {
        constraints.push(Constraint::Length(notice_height));
    }
    if app.models_visible {
        let desired_height = if compact { 8 } else { 10 };
        let model_height = if compact && (quota_height > base_quota_height || app_server_failed) {
            desired_height.min(
                area.height
                    .saturating_sub(quota_height)
                    .saturating_sub(notice_height)
                    .saturating_sub(9),
            )
        } else {
            desired_height
        };
        constraints.push(Constraint::Length(model_height));
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut row_index = 0;
    render_limits(frame, rows[row_index], &app.snapshot, app.theme);
    row_index += 1;
    let task_area = rows[row_index];
    row_index += 1;

    if app.turns_visible() {
        let narrow = area.width < 100;
        let body = Layout::default()
            .direction(if narrow {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if narrow {
                [Constraint::Percentage(40), Constraint::Percentage(60)]
            } else {
                [Constraint::Percentage(52), Constraint::Percentage(48)]
            })
            .split(task_area);
        render_tasks(frame, body[0], app, true);
        render_turns(frame, body[1], app, true);
    } else {
        render_tasks(frame, task_area, app, true);
    }
    if app_server_failed {
        render_app_server_failure_notice(frame, rows[row_index], app.theme);
        row_index += 1;
    }
    if app.models_visible {
        render_models(frame, rows[row_index], app);
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "On" } else { "Off" }
}

fn settings_hint(app: &App) -> Line<'static> {
    let normal = Style::default().fg(app.theme.palette().muted);
    let shortcut = if app.shortcuts_active() {
        Style::default()
            .fg(app.theme.palette().accent)
            .add_modifier(Modifier::BOLD)
    } else {
        normal
    };
    Line::from(vec![
        Span::styled(" [", normal),
        Span::styled("↑↓", shortcut),
        Span::styled("]Select [", normal),
        Span::styled("↵", shortcut),
        Span::styled("]Toggle ", normal),
    ])
}

fn settings_row(app: &App, item: SettingItem) -> Row<'static> {
    let selected = app.selected_setting == item.index();
    let shortcuts_active = app.shortcuts_active();
    let palette = app.theme.palette();
    let row_style = if selected {
        Style::default()
            .fg(palette.background)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.foreground)
    };
    let shortcut_style = if !shortcuts_active && selected {
        row_style
    } else if !shortcuts_active {
        Style::default().fg(palette.muted)
    } else if selected {
        row_style.add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    };
    let label = Line::from(vec![
        Span::styled("[", row_style),
        Span::styled(item.shortcut().to_string(), shortcut_style),
        Span::styled(format!("]{}", item.label()), row_style),
    ]);
    Row::new([
        Cell::from(if selected { "▌" } else { " " }),
        Cell::from(label),
        Cell::from(app.setting_value(item)),
    ])
    .style(row_style)
}

fn render_settings_group(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    title: &'static str,
    items: &[SettingItem],
    show_hint: bool,
    hitbox: &mut SettingsControlsHitbox,
) {
    if area.is_empty() {
        return;
    }
    let mut block = panel(title, app.theme);
    if show_hint {
        block = block.title_bottom(settings_hint(app));
    }
    let inner = block.inner(area);
    for (offset, item) in items.iter().enumerate() {
        let y = inner
            .y
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        if y < inner.bottom() {
            hitbox.rows[item.index()] = Rect::new(inner.x, y, inner.width, 1);
        }
    }
    let rows = items
        .iter()
        .copied()
        .map(|item| settings_row(app, item))
        .collect::<Vec<_>>();
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(18),
                Constraint::Length(8),
            ],
        )
        .block(block),
        area,
    );
}

fn render_settings(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let mut hitbox = SettingsControlsHitbox::default();
    if area.height < 14 {
        let capacity = usize::from(area.height.saturating_sub(2)).min(SettingItem::ALL.len());
        let end = app
            .selected_setting
            .min(SettingItem::ALL.len() - 1)
            .saturating_add(1)
            .max(capacity)
            .min(SettingItem::ALL.len());
        let start = end.saturating_sub(capacity);
        render_settings_group(
            frame,
            area,
            app,
            " Settings",
            &SettingItem::ALL[start..end],
            true,
            &mut hitbox,
        );
    } else {
        let rows = Layout::vertical([
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Min(3),
        ])
        .split(area);
        render_settings_group(
            frame,
            rows[0],
            app,
            " Display",
            &SettingItem::ALL[..4],
            false,
            &mut hitbox,
        );
        render_settings_group(
            frame,
            rows[1],
            app,
            " Table columns",
            &SettingItem::ALL[4..],
            true,
            &mut hitbox,
        );
        let description = [
            "Column choices apply to Tasks, Turns, and Models.",
            "API EQ. uses the selected 5h/week scope and is independent of EST Longx.",
            "Narrow panes keep API EQ. visible first and may omit lower-priority columns.",
        ];
        frame.render_widget(
            Paragraph::new(description.map(Line::from).to_vec())
                .block(panel(" Behavior", app.theme))
                .style(Style::default().fg(app.theme.palette().muted))
                .wrap(Wrap { trim: true }),
            rows[2],
        );
    }
    app.settings_controls_hitbox = Some(hitbox);
}

fn summary_snapshot_inputs_eq(
    current: &Snapshot,
    incoming: &Snapshot,
    cache: &SummaryCache,
    range: SummaryRange,
    query_now: DateTime<Utc>,
) -> bool {
    cache.range == range
        && summary_cache_window_matches(cache, incoming, range, query_now)
        && summary_task_metadata_eq(&current.tasks, &incoming.tasks)
}

fn summary_cache_window_matches(
    cache: &SummaryCache,
    snapshot: &Snapshot,
    range: SummaryRange,
    query_now: DateTime<Utc>,
) -> bool {
    if range != SummaryRange::Cycle {
        return true;
    }
    let (incoming_window, incoming_note) = range.window(snapshot, query_now);
    match (cache.prepared.range_note, incoming_note) {
        (None, None) => cache.prepared.usage.window.starts_at == incoming_window.starts_at,
        (Some(current), Some(incoming)) => current == incoming,
        _ => false,
    }
}

fn summary_task_metadata_eq(current: &[TaskRecord], incoming: &[TaskRecord]) -> bool {
    if current.len() != incoming.len() {
        return false;
    }
    let incoming_by_id = incoming
        .iter()
        .map(|task| (task.thread_id.as_str(), task))
        .collect::<HashMap<_, _>>();
    current.iter().all(|task| {
        incoming_by_id
            .get(task.thread_id.as_str())
            .is_some_and(|candidate| {
                task.parent_thread_id == candidate.parent_thread_id
                    && task.cwd == candidate.cwd
                    && task.title == candidate.title
                    && task.source == candidate.source
            })
    })
}

fn summary_history_inputs_eq(current: &HistoryData, incoming: &HistoryData) -> bool {
    current.read_only == incoming.read_only
        && current.warnings == incoming.warnings
        && current.half_hour_buckets.len() == incoming.half_hour_buckets.len()
        && current
            .half_hour_buckets
            .iter()
            .rev()
            .zip(incoming.half_hour_buckets.iter().rev())
            .all(|(current, incoming)| {
                current.starts_at == incoming.starts_at
                    && current.ends_at == incoming.ends_at
                    && current.sampled_at == incoming.sampled_at
                    && current.token_usage == incoming.token_usage
                    && current.long_context_usage_unknown == incoming.long_context_usage_unknown
                    && current.estimator_revision == incoming.estimator_revision
                    && current.project_breakdown_revision == incoming.project_breakdown_revision
                    && current.api_pricing_catalog_revision == incoming.api_pricing_catalog_revision
                    && current.project_groups == incoming.project_groups
                    && current.partial_reasons == incoming.partial_reasons
            })
}

fn prepare_summary(app: &App, query_now: DateTime<Utc>) -> PreparedSummary {
    let (window, range_note) = app.summary_range.window(&app.snapshot, query_now);
    let mut samples = Vec::new();
    let mut represented_tokens = 0_u64;
    let mut available_tokens = 0_u64;
    let mut covered_buckets = 0_usize;
    let mut estimated_covered_tokens = 0_u64;
    let mut long_context_breakdown_complete = true;
    let mut daily_coverage = expected_summary_daily_coverage(window);
    let mut hourly_coverage = expected_summary_hourly_coverage(window);
    let mut partial_reasons = app.history.warnings.clone();
    if range_note.is_some() {
        partial_reasons.push("cycle_window_unavailable".to_string());
    }

    for bucket in &app.history.half_hour_buckets {
        let selected = window.contains(bucket.starts_at);
        if !selected {
            continue;
        }
        let local_date = display_local_date(bucket.starts_at);
        let local_hour = display_local_hour(bucket.starts_at);
        for coverage in [
            daily_coverage.entry(local_date).or_default(),
            hourly_coverage.entry(local_hour).or_default(),
        ] {
            coverage.available_tokens = coverage
                .available_tokens
                .saturating_add(bucket.token_usage.total_tokens);
            coverage.long_context_breakdown_complete &= !bucket.long_context_usage_unknown;
            coverage.source_partial |=
                !bucket.partial_reasons.is_empty() || bucket.sampled_at < bucket.ends_at;
        }
        if bucket.sampled_at < bucket.ends_at {
            partial_reasons.push("history_bucket_open".to_string());
        }
        let project_breakdown_current =
            bucket.project_breakdown_revision == HISTORY_PROJECT_BREAKDOWN_REVISION;
        if project_breakdown_current {
            covered_buckets = covered_buckets.saturating_add(1);
            for coverage in [
                daily_coverage.entry(local_date).or_default(),
                hourly_coverage.entry(local_hour).or_default(),
            ] {
                coverage.covered_buckets = coverage.covered_buckets.saturating_add(1);
            }
        } else {
            partial_reasons.push("project_breakdown_unavailable".to_string());
        }
        available_tokens = available_tokens.saturating_add(bucket.token_usage.total_tokens);
        partial_reasons.extend(bucket.partial_reasons.iter().cloned());
        long_context_breakdown_complete &= !bucket.long_context_usage_unknown;
        if bucket.api_pricing_catalog_revision != API_PRICING_CATALOG_REVISION {
            partial_reasons.push("api_pricing_catalog_outdated".to_string());
        }
        let estimator_current = bucket.estimator_revision == HISTORY_ESTIMATOR_REVISION;
        if estimator_current {
            estimated_covered_tokens =
                estimated_covered_tokens.saturating_add(bucket.token_usage.total_tokens);
            for coverage in [
                daily_coverage.entry(local_date).or_default(),
                hourly_coverage.entry(local_hour).or_default(),
            ] {
                coverage.estimated_covered_tokens = coverage
                    .estimated_covered_tokens
                    .saturating_add(bucket.token_usage.total_tokens);
            }
        } else {
            partial_reasons.push("estimator_revision_changed".to_string());
        }
        if !project_breakdown_current {
            continue;
        }
        for group in &bucket.project_groups {
            represented_tokens = represented_tokens.saturating_add(group.token_usage.total_tokens);
            for coverage in [
                daily_coverage.entry(local_date).or_default(),
                hourly_coverage.entry(local_hour).or_default(),
            ] {
                coverage.represented_tokens = coverage
                    .represented_tokens
                    .saturating_add(group.token_usage.total_tokens);
            }
            if estimator_current
                && (!group.token_usage.is_zero() || group.estimated_cost_units > 0)
                && group.api_long_context_extra_cost_units.is_none()
            {
                long_context_breakdown_complete = false;
                daily_coverage
                    .entry(local_date)
                    .or_default()
                    .long_context_breakdown_complete = false;
                hourly_coverage
                    .entry(local_hour)
                    .or_default()
                    .long_context_breakdown_complete = false;
            }
            let (estimated_cost_units, api_long_context_extra_cost_units) =
                summary_estimated_units_for_revision(
                    group.estimated_cost_units,
                    group.api_long_context_extra_cost_units.unwrap_or_default(),
                    bucket.estimator_revision,
                );
            samples.push(SummarySample {
                timestamp: bucket.starts_at,
                thread_id: group.thread_id.clone(),
                parent_thread_id: group.parent_thread_id.clone(),
                turn_id: group.turn_id.clone(),
                session_thread_id: group.session_thread_id.clone(),
                session_turn_id: group.session_turn_id.clone(),
                message_preview: group.message_preview.clone(),
                turn_started_at: group.turn_started_at,
                project_key: group.project_id.clone(),
                project_label: group.project_label.clone(),
                cwd: None,
                title: group.title.clone(),
                source: group.source.clone(),
                token_usage: group.token_usage,
                estimated_cost_units,
                api_long_context_extra_cost_units,
                api_equivalent_cost: summary_api_cost_for_catalog(
                    group.api_equivalent_cost,
                    bucket.api_pricing_catalog_revision,
                ),
                call_count: group.call_count,
            });
        }
    }

    // The live task index is the freshest metadata source. Overlay it without
    // adding usage so post-completion renames and redaction take effect even
    // when a closed history bucket has no new token evidence.
    samples.extend(app.snapshot.tasks.iter().map(|task| {
        SummarySample {
            timestamp: app.snapshot.as_of,
            thread_id: task.thread_id.clone(),
            parent_thread_id: task.parent_thread_id.clone(),
            turn_id: None,
            session_thread_id: None,
            session_turn_id: None,
            message_preview: None,
            turn_started_at: None,
            project_key: None,
            project_label: task
                .cwd
                .as_deref()
                .and_then(|cwd| cwd.file_name())
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            cwd: task.cwd.clone(),
            title: Some(task.title.clone()),
            source: task.source.clone(),
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: 0,
            api_equivalent_cost: ApiCostAmount::default(),
            call_count: 0,
        }
    }));

    if window
        .starts_at
        .timestamp()
        .rem_euclid(LOCAL_BUCKET_MINUTES * 60)
        != 0
    {
        partial_reasons.push("range_starts_within_15m_bucket".to_string());
    }
    if app.history.read_only {
        partial_reasons.push("history_read_only".to_string());
    }
    partial_reasons.sort();
    partial_reasons.dedup();

    let expected_buckets = daily_coverage
        .values()
        .map(|coverage| coverage.expected_buckets)
        .fold(0_usize, usize::saturating_add);
    let usage = summarize_samples_with_local_time(&samples, window, display_local_datetime);
    PreparedSummary {
        usage,
        range_note,
        represented_tokens,
        available_tokens,
        covered_buckets,
        expected_buckets,
        estimated_covered_tokens,
        long_context_breakdown_complete,
        daily_coverage,
        hourly_coverage,
        partial_reasons,
    }
}

fn expected_summary_daily_coverage(
    window: SummaryWindow,
) -> BTreeMap<NaiveDate, SummaryDailyCoverage> {
    expected_summary_coverage(window, display_local_date)
}

fn expected_summary_hourly_coverage(
    window: SummaryWindow,
) -> BTreeMap<NaiveDateTime, SummaryDailyCoverage> {
    expected_summary_coverage(window, display_local_hour)
}

fn expected_summary_coverage<K: Ord>(
    window: SummaryWindow,
    local_key: impl Fn(DateTime<Utc>) -> K,
) -> BTreeMap<K, SummaryDailyCoverage> {
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let mut starts_at_seconds = window
        .starts_at
        .timestamp()
        .div_euclid(bucket_seconds)
        .saturating_mul(bucket_seconds);
    let aligned = DateTime::from_timestamp(starts_at_seconds, 0).unwrap_or(window.starts_at);
    if aligned < window.starts_at {
        starts_at_seconds = starts_at_seconds.saturating_add(bucket_seconds);
    }
    let mut starts_at = DateTime::from_timestamp(starts_at_seconds, 0).unwrap_or(window.starts_at);
    let mut coverage = BTreeMap::<K, SummaryDailyCoverage>::new();
    while starts_at < window.ends_at {
        let bucket = coverage.entry(local_key(starts_at)).or_default();
        bucket.expected_buckets = bucket.expected_buckets.saturating_add(1);
        let Some(next) = starts_at.checked_add_signed(ChronoDuration::seconds(bucket_seconds))
        else {
            break;
        };
        starts_at = next;
    }
    coverage
}

fn prepare_summary_chart(prepared: &PreparedSummary, grain: SummaryGrain) -> SummaryChartData {
    let mut coverage_by_bucket = BTreeMap::<NaiveDateTime, SummaryDailyCoverage>::new();
    for (hour, coverage) in &prepared.hourly_coverage {
        coverage_by_bucket
            .entry(grain.bucket_start(*hour))
            .or_default()
            .add_assign(coverage);
    }

    let mut totals_by_bucket = BTreeMap::<NaiveDateTime, SummaryMetrics>::new();
    for hour in &prepared.usage.hours {
        totals_by_bucket
            .entry(grain.bucket_start(hour.starts_at))
            .or_default()
            .add_assign(hour.totals);
    }
    for starts_at in totals_by_bucket.keys().copied().collect::<Vec<_>>() {
        coverage_by_bucket.entry(starts_at).or_default();
    }

    let starts = coverage_by_bucket.keys().copied().collect::<Vec<_>>();
    let buckets = starts
        .iter()
        .map(|starts_at| SummaryChartBucket {
            starts_at: *starts_at,
            totals: totals_by_bucket.get(starts_at).copied().unwrap_or_default(),
            coverage: coverage_by_bucket
                .get(starts_at)
                .cloned()
                .unwrap_or_default(),
        })
        .collect();
    let project_values = prepared
        .usage
        .projects
        .iter()
        .map(|project| {
            let mut by_bucket = BTreeMap::<NaiveDateTime, SummaryMetrics>::new();
            for hour in &project.hours {
                by_bucket
                    .entry(grain.bucket_start(hour.starts_at))
                    .or_default()
                    .add_assign(hour.totals);
            }
            (project.key.clone(), by_bucket)
        })
        .collect();

    SummaryChartData {
        grain,
        buckets,
        project_values,
    }
}

fn summary_api_cost_for_catalog(amount: ApiCostAmount, catalog_revision: u32) -> ApiCostAmount {
    if catalog_revision == API_PRICING_CATALOG_REVISION {
        amount
    } else {
        ApiCostAmount {
            observed_samples: amount.observed_samples,
            observed_tokens: amount.observed_tokens,
            ..ApiCostAmount::default()
        }
    }
}

fn summary_estimated_units_for_revision(
    base_units: u128,
    long_context_extra_units: u128,
    estimator_revision: u32,
) -> (u128, u128) {
    if estimator_revision == HISTORY_ESTIMATOR_REVISION {
        (base_units, long_context_extra_units)
    } else {
        (0, 0)
    }
}

fn ensure_summary_cache(app: &mut App, query_now: DateTime<Utc>) {
    let query_now = query_now.max(app.snapshot.as_of);
    let query_bucket = query_now.timestamp().div_euclid(LOCAL_BUCKET_MINUTES * 60);
    let query_local_date = display_local_date(query_now);
    let current = app.summary_cache.as_ref().is_some_and(|cache| {
        cache.range == app.summary_range
            && cache.snapshot_as_of == app.snapshot.as_of
            && cache.query_bucket == query_bucket
            && cache.query_local_date == query_local_date
            && summary_cache_window_matches(cache, &app.snapshot, app.summary_range, query_now)
    });
    if current {
        if let Some(cache) = app.summary_cache.as_mut()
            && cache.chart.grain != app.summary_grain
        {
            cache.chart = prepare_summary_chart(&cache.prepared, app.summary_grain);
        }
        return;
    }
    let prepared = prepare_summary(app, query_now);
    if summary_chart_project_count(
        &prepared,
        app.summary_metric,
        app.api_long_context_multiplier,
    ) <= SUMMARY_STACKED_PROJECT_LIMIT
    {
        app.summary_show_all_projects = false;
    }
    let chart = prepare_summary_chart(&prepared, app.summary_grain);
    app.summary_cache = Some(SummaryCache {
        range: app.summary_range,
        snapshot_as_of: app.snapshot.as_of,
        query_bucket,
        query_local_date,
        prepared,
        chart,
    });
}

fn summary_project_node_id(project_key: &str) -> String {
    format!("project:{project_key}")
}

fn summary_thread_node_id(thread_id: &str) -> String {
    format!("thread:{thread_id}")
}

fn summary_turn_node_id(session_thread_id: &str, key: &SummaryTurnKey) -> String {
    match key {
        SummaryTurnKey::Exact(turn_id) => format!("turn:{session_thread_id}:{turn_id}"),
        SummaryTurnKey::UnassignedSession => {
            format!("turn-unassigned-session:{session_thread_id}")
        }
        SummaryTurnKey::UnassignedDelegated => {
            format!("turn-unassigned-delegated:{session_thread_id}")
        }
    }
}

fn summary_tree_prefix(guides: &[bool]) -> String {
    let mut prefix = String::new();
    if let Some((&is_last, ancestors)) = guides.split_last() {
        for ancestor_is_last in ancestors {
            prefix.push_str(if *ancestor_is_last { "  " } else { "│ " });
        }
        prefix.push_str(if is_last { "└─ " } else { "├─ " });
    }
    prefix
}

fn summary_project_order(
    summary: &UsageSummary,
    metric: SummaryMetric,
    api_long_context: bool,
) -> Vec<&ProjectSummary> {
    let mut projects = summary.projects.iter().collect::<Vec<_>>();
    projects.sort_by(|left, right| {
        metric
            .value(right.totals, api_long_context)
            .cmp(&metric.value(left.totals, api_long_context))
            .then_with(|| left.key.cmp(&right.key))
    });
    projects
}

fn summary_project_display_labels(summary: &UsageSummary) -> HashMap<String, String> {
    let mut counts = HashMap::<&str, usize>::new();
    for project in &summary.projects {
        *counts.entry(project.label.as_str()).or_default() += 1;
    }
    summary
        .projects
        .iter()
        .map(|project| {
            let label = if counts.get(project.label.as_str()).copied().unwrap_or(0) > 1 {
                format!(
                    "{} · {:06x}",
                    project.label,
                    stable_summary_key_hash(&project.key) & 0x00ff_ffff
                )
            } else {
                project.label.clone()
            };
            (project.key.clone(), terminal_safe_text(&label))
        })
        .collect()
}

fn stable_summary_key_hash(value: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    value.bytes().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

fn sorted_summary_sessions(
    sessions: &[SessionSummary],
    metric: SummaryMetric,
    api_long_context: bool,
) -> Vec<&SessionSummary> {
    let mut sessions = sessions.iter().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        metric
            .value(right.totals, api_long_context)
            .cmp(&metric.value(left.totals, api_long_context))
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    sessions
}

fn sorted_summary_turns(
    turns: &[TurnSummary],
    metric: SummaryMetric,
    api_long_context: bool,
) -> Vec<&TurnSummary> {
    let mut turns = turns.iter().collect::<Vec<_>>();
    turns.sort_by(|left, right| {
        metric
            .value(right.totals, api_long_context)
            .cmp(&metric.value(left.totals, api_long_context))
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| left.key.cmp(&right.key))
    });
    turns
}

fn append_summary_session_rows(
    session: &SessionSummary,
    metric: SummaryMetric,
    api_long_context: bool,
    expanded: &HashSet<String>,
    guides: &mut Vec<bool>,
    rows: &mut Vec<SummaryTreeRow>,
) {
    let id = summary_thread_node_id(&session.thread_id);
    let has_children = !session.turns.is_empty();
    let collapsed = has_children && !expanded.contains(&id);
    rows.push(SummaryTreeRow {
        id,
        kind: SummaryRowKind::Session,
        prefix: summary_tree_prefix(guides),
        label: session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .map(terminal_safe_text)
            .unwrap_or_else(|| format!("Session {}", short_thread_id(&session.thread_id))),
        source: session.source.clone(),
        metrics: session.totals,
        has_children,
        collapsed,
    });
    if collapsed {
        return;
    }
    let turns = sorted_summary_turns(&session.turns, metric, api_long_context);
    let turn_count = turns.len();
    for (position, turn) in turns.into_iter().enumerate() {
        guides.push(position + 1 == turn_count);
        rows.push(SummaryTreeRow {
            id: summary_turn_node_id(&session.thread_id, &turn.key),
            kind: SummaryRowKind::Turn,
            prefix: summary_tree_prefix(guides),
            label: match &turn.key {
                SummaryTurnKey::Exact(turn_id) => turn
                    .message_preview
                    .as_deref()
                    .filter(|message| !message.trim().is_empty())
                    .map(terminal_safe_text)
                    .unwrap_or_else(|| format!("Turn {}", short_thread_id(turn_id))),
                SummaryTurnKey::UnassignedSession => "Unassigned session usage".to_string(),
                SummaryTurnKey::UnassignedDelegated => "Unassigned delegated usage".to_string(),
            },
            source: None,
            metrics: turn.totals,
            has_children: false,
            collapsed: false,
        });
        guides.pop();
    }
}

fn summary_tree_rows(
    summary: &UsageSummary,
    metric: SummaryMetric,
    api_long_context: bool,
    expanded: &HashSet<String>,
) -> Vec<SummaryTreeRow> {
    let mut rows = Vec::new();
    let display_labels = summary_project_display_labels(summary);
    for project in summary_project_order(summary, metric, api_long_context) {
        let id = summary_project_node_id(&project.key);
        let has_children = !project.sessions.is_empty();
        let collapsed = has_children && !expanded.contains(&id);
        rows.push(SummaryTreeRow {
            id,
            kind: SummaryRowKind::Project,
            prefix: String::new(),
            label: display_labels
                .get(&project.key)
                .cloned()
                .unwrap_or_else(|| terminal_safe_text(&project.label)),
            source: None,
            metrics: project.totals,
            has_children,
            collapsed,
        });
        if collapsed {
            continue;
        }
        let sessions = sorted_summary_sessions(&project.sessions, metric, api_long_context);
        let session_count = sessions.len();
        let mut guides = Vec::new();
        for (position, session) in sessions.into_iter().enumerate() {
            guides.push(position + 1 == session_count);
            append_summary_session_rows(
                session,
                metric,
                api_long_context,
                expanded,
                &mut guides,
                &mut rows,
            );
            guides.pop();
        }
    }
    rows
}

fn percent_u128(value: u128, total: u128) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64 * 100.0
    }
}

fn format_compact_u128(value: u128) -> String {
    let value = value as f64;
    if value >= 1_000_000_000_000.0 {
        format!("{:.2}T", value / 1_000_000_000_000.0)
    } else if value >= 1_000_000_000.0 {
        format!("{:.2}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

fn format_estimated_credits(units: u128, spaced_unit: bool) -> String {
    let credits = units as f64 / ESTIMATED_COST_UNITS_PER_CREDIT as f64;
    let compact = if credits >= 1_000_000.0 {
        format!("{:.2}M", credits / 1_000_000.0)
    } else if credits >= 1_000.0 {
        format!("{:.2}K", credits / 1_000.0)
    } else if credits >= 100.0 {
        format!("{credits:.1}")
    } else {
        format!("{credits:.2}")
    };
    format!("~{compact}{}cr", if spaced_unit { " " } else { "" })
}

fn format_summary_metric(
    metrics: SummaryMetrics,
    metric: SummaryMetric,
    api_long_context: bool,
) -> String {
    match metric {
        SummaryMetric::Tokens => format_tokens(metrics.token_usage),
        SummaryMetric::Estimated => {
            format_estimated_credits(metrics.estimated_units(api_long_context), true)
        }
        SummaryMetric::ApiEquivalent => format_api_cost_amount(metrics.api_equivalent_cost),
    }
}

fn summary_chart_metric_label(app: &App, prepared: &PreparedSummary) -> String {
    if app.summary_metric == SummaryMetric::Estimated {
        "~EST credit-rate eq.".to_string()
    } else if app.summary_metric == SummaryMetric::ApiEquivalent
        && prepared.api_chart_is_lower_bound()
    {
        format!("{} lower bound", app.summary_metric.label())
    } else {
        app.summary_metric.label().to_string()
    }
}

fn summary_project_color(project_key: &str, theme: Theme) -> Color {
    let hash = summary_project_hash(project_key);
    summary_project_color_candidate(hash, 0, theme)
}

#[cfg(test)]
fn summary_project_colors(
    summary: &UsageSummary,
    history: &HistoryData,
    theme: Theme,
) -> HashMap<String, Color> {
    let mut colors = HashMap::new();
    extend_summary_project_colors(&mut colors, summary, history, theme);
    colors
}

#[cfg(test)]
fn extend_summary_project_colors(
    colors: &mut HashMap<String, Color>,
    summary: &UsageSummary,
    history: &HistoryData,
    theme: Theme,
) {
    extend_summary_project_colors_from_history(colors, history, theme);
    extend_assigned_summary_project_colors(
        colors,
        summary.projects.iter().map(|project| project.key.as_str()),
        theme,
    );
}

fn extend_summary_project_colors_from_history(
    colors: &mut HashMap<String, Color>,
    history: &HistoryData,
    theme: Theme,
) {
    let project_keys = history
        .half_hour_buckets
        .iter()
        .flat_map(|bucket| &bucket.project_groups)
        .filter_map(|group| group.project_id.as_deref())
        .filter(|project_id| !project_id.is_empty())
        .collect::<HashSet<_>>();

    // Summary history is loaded over one fixed 31-day horizon regardless of
    // the selected Cycle/7d/30d range. Assigning against that full project
    // universe keeps a project's color stable when a range hides or reveals
    // another project that sorts before it in the collision resolver.
    extend_assigned_summary_project_colors(colors, project_keys, theme);
}

#[cfg(test)]
fn assign_summary_project_colors<'a>(
    project_keys: impl IntoIterator<Item = &'a str>,
    theme: Theme,
) -> HashMap<String, Color> {
    let mut colors = HashMap::new();
    extend_assigned_summary_project_colors(&mut colors, project_keys, theme);
    colors
}

fn extend_assigned_summary_project_colors<'a>(
    colors: &mut HashMap<String, Color>,
    project_keys: impl IntoIterator<Item = &'a str>,
    theme: Theme,
) {
    let mut projects = project_keys
        .into_iter()
        .filter(|key| !colors.contains_key(*key))
        .map(|key| (summary_project_hash(key), key.to_string()))
        .collect::<Vec<_>>();
    projects.sort_unstable();

    // Reserve the neutral Other color and then choose the first deterministic
    // candidate with enough distance from every color already assigned. The
    // stable hash/key order makes this independent of the current metric rank.
    let mut assigned = Vec::with_capacity(colors.len().saturating_add(1));
    assigned.push(theme.palette().muted);
    assigned.extend(colors.values().copied());
    for (hash, key) in projects {
        let mut best = summary_project_color_candidate(hash, 0, theme);
        let mut best_distance = 0_u32;
        for attempt in 0..SUMMARY_PROJECT_COLOR_CANDIDATES {
            let candidate = summary_project_color_candidate(hash, attempt, theme);
            let minimum_distance = assigned
                .iter()
                .map(|existing| summary_color_distance_squared(candidate, *existing))
                .min()
                .unwrap_or(u32::MAX);
            if minimum_distance >= SUMMARY_PROJECT_COLOR_MIN_DISTANCE_SQUARED {
                best = candidate;
                break;
            }
            if minimum_distance > best_distance {
                best = candidate;
                best_distance = minimum_distance;
            }
        }
        assigned.push(best);
        colors.insert(key, best);
    }
}

fn summary_project_color_candidate(hash: u64, attempt: usize, theme: Theme) -> Color {
    const GOLDEN_ANGLE: f64 = 137.507_764_050_037_85;
    let attempt = u64::try_from(attempt).unwrap_or(u64::MAX);
    let candidate_hash = hash ^ attempt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let hue = ((hash % 36_000) as f64 / 100.0 + attempt as f64 * GOLDEN_ANGLE).rem_euclid(360.0);
    summary_project_color_with_hue(candidate_hash, hue, theme)
}

fn summary_color_distance_squared(left: Color, right: Color) -> u32 {
    let (
        Color::Rgb(left_red, left_green, left_blue),
        Color::Rgb(right_red, right_green, right_blue),
    ) = (left, right)
    else {
        return 0;
    };
    let red_mean = (u32::from(left_red) + u32::from(right_red)) / 2;
    let red = i32::from(left_red) - i32::from(right_red);
    let green = i32::from(left_green) - i32::from(right_green);
    let blue = i32::from(left_blue) - i32::from(right_blue);
    let red = red.unsigned_abs().saturating_pow(2);
    let green = green.unsigned_abs().saturating_pow(2);
    let blue = blue.unsigned_abs().saturating_pow(2);
    ((512 + red_mean).saturating_mul(red) >> 8)
        .saturating_add(4_u32.saturating_mul(green))
        .saturating_add((767_u32.saturating_sub(red_mean)).saturating_mul(blue) >> 8)
}

fn summary_project_hash(project_key: &str) -> u64 {
    let hash = project_key
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        });
    let hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

fn summary_project_color_with_hue(hash: u64, hue: f64, theme: Theme) -> Color {
    let saturation = 0.62 + ((hash >> 16) % 11) as f64 / 100.0;
    let lightness = match theme {
        Theme::Dark => 0.58 + ((hash >> 24) % 7) as f64 / 100.0,
        Theme::Light => 0.36 + ((hash >> 24) % 7) as f64 / 100.0,
    };
    hsl_color(hue, saturation, lightness)
}

fn hsl_color(hue: f64, saturation: f64, lightness: f64) -> Color {
    let hue = hue.rem_euclid(360.0) / 30.0;
    let chroma = saturation * lightness.min(1.0 - lightness);
    let channel = |offset: f64| {
        let k = (offset + hue).rem_euclid(12.0);
        let shape = (-1.0_f64).max((k - 3.0).min((9.0 - k).min(1.0)));
        ((lightness - chroma * shape) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::Rgb(channel(0.0), channel(8.0), channel(4.0))
}

struct SummaryControlSpec {
    leading: &'static str,
    shortcut: String,
    suffix: &'static str,
    selected: bool,
    shortcuts_active: bool,
    theme: Theme,
}

fn append_summary_control(
    spans: &mut Vec<Span<'static>>,
    area: Rect,
    x: &mut u16,
    spec: SummaryControlSpec,
) -> Rect {
    let SummaryControlSpec {
        leading,
        shortcut,
        suffix,
        selected,
        shortcuts_active,
        theme,
    } = spec;
    let palette = theme.palette();
    let leading_width = u16::try_from(UnicodeWidthStr::width(leading)).unwrap_or(u16::MAX);
    let label = format!("[{shortcut}]{suffix}");
    let width = u16::try_from(UnicodeWidthStr::width(label.as_str())).unwrap_or(u16::MAX);
    if x.saturating_add(leading_width).saturating_add(width) > area.right() {
        return Rect::default();
    }
    if !leading.is_empty() {
        spans.push(Span::styled(
            leading.to_string(),
            Style::default().fg(palette.muted),
        ));
        *x = x.saturating_add(leading_width);
    }
    let normal = if selected {
        Style::default()
            .fg(palette.background)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let shortcut_style = if !shortcuts_active {
        normal
    } else if selected {
        normal.add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    };
    let hitbox = Rect::new(*x, area.y, width, 1);
    spans.push(Span::styled("[", normal));
    spans.push(Span::styled(shortcut, shortcut_style));
    spans.push(Span::styled(format!("]{suffix}"), normal));
    *x = x.saturating_add(width);
    hitbox
}

fn render_summary_controls(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    selected_can_toggle: bool,
    can_collapse_all: bool,
    can_toggle_all_projects: bool,
    can_inspect: bool,
) -> SummaryControlsHitbox {
    const FULL_CONTROLS_WIDTH: u16 = 118;
    let mut hitbox = SummaryControlsHitbox::default();
    if area.is_empty() {
        return hitbox;
    }
    let compact = area.width < FULL_CONTROLS_WIDTH;
    let roomy_compact = compact && area.width >= 49;
    let mut spans = Vec::new();
    let mut x = area.x;
    for (position, range) in SummaryRange::ALL.into_iter().enumerate() {
        hitbox.ranges[range.index()] = append_summary_control(
            &mut spans,
            area,
            &mut x,
            SummaryControlSpec {
                leading: if position == 0 || compact { "" } else { " " },
                shortcut: range.shortcut().to_string(),
                suffix: if compact { "" } else { range.label() },
                selected: app.summary_range == range,
                shortcuts_active: app.shortcuts_active(),
                theme: app.theme,
            },
        );
    }
    for (position, metric) in SummaryMetric::ALL.into_iter().enumerate() {
        hitbox.metrics[metric.index()] = append_summary_control(
            &mut spans,
            area,
            &mut x,
            SummaryControlSpec {
                leading: if position == 0 {
                    if compact { " " } else { " | " }
                } else if compact {
                    ""
                } else {
                    " "
                },
                shortcut: metric.shortcut().to_string(),
                suffix: if compact { "" } else { metric.label() },
                selected: app.summary_metric == metric,
                shortcuts_active: app.shortcuts_active(),
                theme: app.theme,
            },
        );
    }
    hitbox.bucket_grain = append_summary_control(
        &mut spans,
        area,
        &mut x,
        SummaryControlSpec {
            leading: if compact { " " } else { " | " },
            shortcut: "B".to_string(),
            suffix: app.summary_grain.control_suffix(),
            selected: false,
            shortcuts_active: app.shortcuts_active(),
            theme: app.theme,
        },
    );
    hitbox.inspect = append_summary_control(
        &mut spans,
        area,
        &mut x,
        SummaryControlSpec {
            leading: " ",
            shortcut: "I".to_string(),
            suffix: if compact && !roomy_compact {
                ""
            } else {
                "Inspect"
            },
            selected: app.summary_inspected_date.is_some(),
            shortcuts_active: can_inspect && app.shortcuts_active(),
            theme: app.theme,
        },
    );
    hitbox.toggle_all_projects = append_summary_control(
        &mut spans,
        area,
        &mut x,
        SummaryControlSpec {
            leading: if compact { " " } else { " | " },
            shortcut: "G".to_string(),
            suffix: if compact && !roomy_compact { "" } else { "All" },
            selected: app.summary_show_all_projects,
            shortcuts_active: can_toggle_all_projects && app.shortcuts_active(),
            theme: app.theme,
        },
    );
    hitbox.toggle_long_context = append_summary_control(
        &mut spans,
        area,
        &mut x,
        SummaryControlSpec {
            leading: " ",
            shortcut: "L".to_string(),
            suffix: if compact { "" } else { "Longx" },
            selected: app.api_long_context_multiplier,
            shortcuts_active: app.shortcuts_active(),
            theme: app.theme,
        },
    );
    hitbox.toggle_selected = append_summary_control(
        &mut spans,
        area,
        &mut x,
        SummaryControlSpec {
            leading: if compact { " " } else { " | " },
            shortcut: ENTER_FOCUS_HINT.to_string(),
            suffix: if compact { "" } else { "Toggle" },
            selected: false,
            shortcuts_active: selected_can_toggle && app.shortcuts_active(),
            theme: app.theme,
        },
    );
    hitbox.collapse_all = append_summary_control(
        &mut spans,
        area,
        &mut x,
        SummaryControlSpec {
            leading: " ",
            shortcut: "X".to_string(),
            suffix: if compact { "" } else { "Collapse" },
            selected: false,
            shortcuts_active: can_collapse_all && app.shortcuts_active(),
            theme: app.theme,
        },
    );
    if app.summary_backfill_running {
        let status = [" · BACKFILLING 30d HISTORY…", " · BACKFILL 30d…"]
            .into_iter()
            .find(|status| {
                x.saturating_add(u16::try_from(UnicodeWidthStr::width(*status)).unwrap_or(u16::MAX))
                    <= area.right()
            });
        if let Some(status) = status {
            spans.push(Span::styled(
                status,
                Style::default()
                    .fg(app.theme.palette().warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(app.theme.base_style()),
        area,
    );
    hitbox
}

fn render_summary_tree(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    prepared: &PreparedSummary,
    rows: &[SummaryTreeRow],
    project_colors: &HashMap<String, Color>,
) {
    const DEFAULT_VALUE_WIDTH: u16 = 14;
    const MAX_VALUE_WIDTH: u16 = 28;
    const SHARE_WIDTH: u16 = 8;
    const PROJECT_SWATCH_OFFSET_AFTER_PREFIX: u16 = 9;
    let palette = app.theme.palette();
    let partial = prepared.partial(app.summary_metric, app.api_long_context_multiplier);
    let mut total = format_summary_metric(
        prepared.usage.totals,
        app.summary_metric,
        app.api_long_context_multiplier,
    );
    if partial {
        total = format!("known {total}");
    }
    let range_label = prepared.range_note.map_or_else(
        || app.summary_range.label().to_string(),
        |note| format!("{} ({note})", app.summary_range.label()),
    );
    let mut title = format!(
        "Usage tree · {range_label} · {total} · {:.0}% coverage",
        prepared.coverage_percent(app.summary_metric)
    );
    if partial {
        title.push_str(" · PARTIAL");
    }
    if app.summary_backfill_running {
        title = format!(
            "Usage tree · BACKFILLING 30d · {range_label} · {total} · {:.0}% coverage{}",
            prepared.coverage_percent(app.summary_metric),
            if partial { " · PARTIAL" } else { "" }
        );
    } else if app.summary_range == SummaryRange::ThirtyDays
        && app.history.summary_backfill_attempt_complete == Some(false)
        && !summary_history_coverage_complete(
            &app.history,
            prepared.usage.window.ends_at - ChronoDuration::nanoseconds(1),
        )
    {
        title.push_str(" · BACKFILL PARTIAL");
    }
    let block = panel(&title, app.theme);
    let inner = block.inner(area);
    let metric_values = rows
        .iter()
        .map(|row| {
            format_summary_metric(
                row.metrics,
                app.summary_metric,
                app.api_long_context_multiplier,
            )
        })
        .collect::<Vec<_>>();
    let widest_value = metric_values
        .iter()
        .map(|value| UnicodeWidthStr::width(value.as_str()))
        .chain([UnicodeWidthStr::width(app.summary_metric.label())])
        .max()
        .unwrap_or(usize::from(DEFAULT_VALUE_WIDTH));
    // Preserve the complete ordinary API range and its trailing lower-bound
    // marker. At tiny widths the value column takes priority over share/label;
    // exceptionally long values are middle-truncated so their suffix survives.
    let maximum_value_width = if inner.width < 40 {
        inner.width
    } else {
        inner
            .width
            .saturating_sub(SHARE_WIDTH)
            .saturating_sub(14)
            .min(MAX_VALUE_WIDTH)
    };
    let value_width = u16::try_from(widest_value)
        .unwrap_or(u16::MAX)
        .max(DEFAULT_VALUE_WIDTH.min(maximum_value_width))
        .min(maximum_value_width);
    let visible_capacity = usize::from(inner.height.saturating_sub(1));
    app.summary_offset = app
        .summary_offset
        .min(rows.len().saturating_sub(visible_capacity));
    let selected_position = app.summary_selected_index(rows);
    app.summary_selected_id = rows.get(selected_position).map(|row| row.id.clone());
    let offset = app.summary_offset;
    let selected_in_view = selected_position
        .checked_sub(offset)
        .filter(|index| *index < visible_capacity);
    let label_x = inner
        .x
        .saturating_add(1)
        .saturating_add(value_width)
        .saturating_add(1)
        .saturating_add(SHARE_WIDTH)
        .saturating_add(1);
    app.summary_tree_marker_hitboxes = rows
        .iter()
        .skip(offset)
        .take(visible_capacity)
        .enumerate()
        .filter_map(|(position, row)| {
            if !row.has_children {
                return None;
            }
            let marker_x = label_x.saturating_add(
                u16::try_from(UnicodeWidthStr::width(row.prefix.as_str())).unwrap_or(u16::MAX),
            );
            (marker_x.saturating_add(3) <= inner.right()).then_some(SummaryTreeMarkerHitbox {
                area: Rect::new(
                    marker_x,
                    inner
                        .y
                        .saturating_add(1)
                        .saturating_add(u16::try_from(position).unwrap_or(u16::MAX)),
                    3,
                    1,
                ),
                node_id: row.id.clone(),
            })
        })
        .collect();
    let table_rows = rows
        .iter()
        .zip(&metric_values)
        .skip(offset)
        .take(visible_capacity)
        .map(|(row, metric_value)| {
            let selected = app.summary_selected_id.as_deref() == Some(row.id.as_str());
            let project_color = (row.kind == SummaryRowKind::Project)
                .then(|| row.id.strip_prefix("project:"))
                .flatten()
                .and_then(|project_key| project_colors.get(project_key))
                .copied();
            let base = if row.kind == SummaryRowKind::Project {
                Style::default()
                    .fg(palette.foreground)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.foreground)
            };
            let label_style = project_color
                .map(|color| {
                    summary_project_body_style(app.theme, color, selected)
                        .add_modifier(Modifier::BOLD)
                })
                .unwrap_or(base);
            let kind = match row.kind {
                SummaryRowKind::Project => "PROJ ",
                SummaryRowKind::Session => "SESS ",
                SummaryRowKind::Turn => "TURN ",
            };
            let kind_style = match row.kind {
                SummaryRowKind::Project => Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
                SummaryRowKind::Session => Style::default().fg(palette.foreground),
                SummaryRowKind::Turn => Style::default().fg(palette.muted),
            };
            let source = row
                .source
                .as_deref()
                .filter(|source| !source.is_empty())
                .map(|source| format!(" · {}", terminal_safe_text(source)))
                .unwrap_or_default();
            let mut label_spans = vec![Span::styled(
                row.prefix.clone(),
                Style::default().fg(palette.muted),
            )];
            if row.has_children {
                let marker = if row.collapsed { "+" } else { "-" };
                let marker_style = if selected && app.shortcuts_active() {
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(palette.muted)
                };
                label_spans.extend([
                    Span::styled("[", Style::default().fg(palette.muted)),
                    Span::styled(marker, marker_style),
                    Span::styled("] ", Style::default().fg(palette.muted)),
                ]);
            }
            label_spans.push(Span::styled(kind, kind_style));
            if let Some(color) = project_color {
                label_spans.push(Span::styled("■ ", Style::default().fg(color)));
            }
            label_spans.extend([
                Span::styled(row.label.clone(), label_style),
                Span::styled(source, Style::default().fg(palette.muted)),
            ]);
            let label = Line::from(label_spans);
            let share = app.summary_metric.share_percent(
                row.metrics,
                prepared.usage.totals,
                app.api_long_context_multiplier,
            );
            Row::new([
                Cell::from(truncate_middle_display_text(
                    metric_value,
                    usize::from(value_width),
                )),
                Cell::from(format!("{share:.1}%")),
                Cell::from(label),
            ])
            .style(base)
        })
        .collect::<Vec<_>>();
    let share_header = if partial { "KNOWN%" } else { "SHARE" };
    let header = Row::new([
        app.summary_metric.label(),
        share_header,
        "TYPE · PROJECT / SESSION / TURN",
    ])
    .style(
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(value_width),
            Constraint::Length(SHARE_WIDTH),
            Constraint::Min(16),
        ],
    )
    .flex(Flex::Legacy)
    .column_spacing(1)
    .header(header)
    .block(block)
    .row_highlight_style(
        Style::default()
            .fg(palette.background)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_spacing(HighlightSpacing::Always)
    .highlight_symbol("▌");
    let mut state = TableState::default().with_selected(selected_in_view);
    frame.render_stateful_widget(table, area, &mut state);

    // Ratatui applies the row highlight after rendering cell spans, so its
    // inverse foreground would otherwise erase the selected project's stable
    // swatch color. Restore only cells that actually rendered the swatch;
    // narrow layouts that clipped it remain untouched.
    for (position, row) in rows.iter().skip(offset).take(visible_capacity).enumerate() {
        let Some(project_key) = (row.kind == SummaryRowKind::Project)
            .then(|| row.id.strip_prefix("project:"))
            .flatten()
        else {
            continue;
        };
        let Some(color) = project_colors.get(project_key).copied() else {
            continue;
        };
        let swatch_x = label_x
            .saturating_add(
                u16::try_from(UnicodeWidthStr::width(row.prefix.as_str())).unwrap_or(u16::MAX),
            )
            .saturating_add(PROJECT_SWATCH_OFFSET_AFTER_PREFIX);
        let swatch_y = inner
            .y
            .saturating_add(1)
            .saturating_add(u16::try_from(position).unwrap_or(u16::MAX));
        if swatch_x < inner.right()
            && swatch_y < inner.bottom()
            && let Some(cell) = frame.buffer_mut().cell_mut((swatch_x, swatch_y))
            && cell.symbol() == "■"
        {
            cell.set_fg(color);
        }
    }

    let remaining_rows = rows.len().saturating_sub(offset);
    let visible_height = inner
        .height
        .saturating_sub(1)
        .min(u16::try_from(remaining_rows).unwrap_or(u16::MAX));
    let row_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        visible_height,
    );
    app.summary_table_hitbox = (!row_area.is_empty()).then_some(TableHitbox {
        viewport: inner,
        rows: row_area,
        offset,
        capacity: visible_capacity,
    });
    app.summary_scrollbar_hitbox = scrollbar_geometry(
        Rect::new(
            area.right().saturating_sub(1),
            row_area.y,
            1,
            row_area.height,
        ),
        rows.len(),
        visible_capacity,
        offset,
    );
    if let Some(scrollbar) = app.summary_scrollbar_hitbox {
        render_scrollbar(
            frame,
            scrollbar,
            app.theme,
            app.scroll_drag
                .is_some_and(|drag| drag.target == ScrollTarget::Summary),
        );
    }
}

fn summary_project_body_style(theme: Theme, color: Color, selected: bool) -> Style {
    let foreground = match theme {
        Theme::Dark => color,
        Theme::Light => theme.palette().foreground,
    };
    Style::default().fg(foreground).add_modifier(if selected {
        Modifier::BOLD
    } else {
        Modifier::empty()
    })
}

fn render_summary_bars(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    prepared: &PreparedSummary,
    project_colors: &HashMap<String, Color>,
) {
    let title = format!(
        "Top projects · {}{}",
        summary_chart_metric_label(app, prepared),
        if prepared.partial(app.summary_metric, app.api_long_context_multiplier) {
            " · share of known"
        } else {
            ""
        }
    );
    let block = panel(&title, app.theme);
    let inner = block.inner(area);
    if inner.is_empty() {
        frame.render_widget(block, area);
        return;
    }
    let projects = summary_project_order(
        &prepared.usage,
        app.summary_metric,
        app.api_long_context_multiplier,
    );
    let projects = projects
        .into_iter()
        .take(usize::from(inner.height).min(8))
        .collect::<Vec<_>>();
    let total_value = app
        .summary_metric
        .value(prepared.usage.totals, app.api_long_context_multiplier);
    if projects.is_empty() || total_value == 0 {
        frame.render_widget(
            Paragraph::new("No project usage recorded for this range")
                .alignment(Alignment::Center)
                .style(Style::default().fg(app.theme.palette().muted))
                .block(block),
            area,
        );
        return;
    }
    let selected_project = app
        .summary_selected_id
        .as_deref()
        .and_then(|selected| selected.strip_prefix("project:"));
    let display_labels = summary_project_display_labels(&prepared.usage);
    let metric_values = projects
        .iter()
        .map(|project| {
            format_summary_metric(
                project.totals,
                app.summary_metric,
                app.api_long_context_multiplier,
            )
        })
        .collect::<Vec<_>>();
    let value_width = metric_values
        .iter()
        .map(|value| UnicodeWidthStr::width(value.as_str()))
        .max()
        .unwrap_or(0)
        .min(20);
    let share_width = 6_usize;
    let available_width = usize::from(inner.width);
    let bar_width = if available_width >= 36 {
        (available_width / 5).clamp(6, 14)
    } else {
        0
    };
    let swatch_width = 2_usize;
    let reserved_width = swatch_width
        .saturating_add(value_width)
        .saturating_add(share_width)
        .saturating_add(bar_width)
        .saturating_add(if bar_width == 0 { 2 } else { 3 });
    let label_width = available_width.saturating_sub(reserved_width).max(1);
    let lines = projects
        .iter()
        .enumerate()
        .map(|(index, project)| {
            let share = app.summary_metric.share_percent(
                project.totals,
                prepared.usage.totals,
                app.api_long_context_multiplier,
            );
            let label = display_labels
                .get(&project.key)
                .map(String::as_str)
                .unwrap_or(project.label.as_str());
            let label = truncate_display_text(label, label_width);
            let label_padding = label_width.saturating_sub(UnicodeWidthStr::width(label.as_str()));
            let metric_value = &metric_values[index];
            let metric_padding =
                value_width.saturating_sub(UnicodeWidthStr::width(metric_value.as_str()));
            let color = project_colors
                .get(&project.key)
                .copied()
                .unwrap_or_else(|| summary_project_color(&project.key, app.theme));
            let selected = selected_project == Some(project.key.as_str());
            let label_style = summary_project_body_style(app.theme, color, selected);
            let mut spans = vec![
                Span::styled("■ ", Style::default().fg(color)),
                Span::styled(label, label_style),
                Span::raw(" ".repeat(label_padding.saturating_add(1))),
                Span::raw(" ".repeat(metric_padding)),
                Span::styled(
                    metric_value.clone(),
                    Style::default().fg(app.theme.palette().foreground),
                ),
                Span::styled(
                    format!(" {share:>5.1}%"),
                    Style::default().fg(app.theme.palette().muted),
                ),
            ];
            if bar_width > 0 {
                let filled = if share <= 0.0 {
                    0
                } else {
                    ((share / 100.0 * bar_width as f64).round() as usize).clamp(1, bar_width)
                };
                spans.extend([
                    Span::raw(" "),
                    Span::styled(
                        "█".repeat(filled),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "·".repeat(bar_width.saturating_sub(filled)),
                        Style::default().fg(app.theme.palette().muted),
                    ),
                ]);
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    app.summary_bar_hitboxes = projects
        .iter()
        .enumerate()
        .map(|(index, project)| SummaryBarHitbox {
            area: Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                inner.width,
                1,
            ),
            project_key: project.key.clone(),
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .style(app.theme.base_style())
            .block(block),
        area,
    );
}

fn format_summary_axis(value: f64, metric: SummaryMetric) -> String {
    let value = value.max(0.0).round() as u128;
    match metric {
        SummaryMetric::Tokens => format_compact_u128(value),
        SummaryMetric::Estimated => format_estimated_credits(value, false),
        SummaryMetric::ApiEquivalent => format_pico_usd(crate::domain::PicoUsd::new(value)),
    }
}

#[derive(Clone, Debug)]
struct SummaryProjectSeries {
    project_key: Option<String>,
    label: String,
    color: Color,
    /// Only the synthetic `Other` series needs a materialized aggregate.
    /// Direct project series stay sparse in `SummaryChartData` and are looked
    /// up by bucket, which keeps 30d/1h memory proportional to observed data.
    aggregate_values: Option<Vec<SummaryMetrics>>,
}

fn summary_project_metrics_at(
    candidate: &SummaryProjectSeries,
    chart: &SummaryChartData,
    index: usize,
) -> SummaryMetrics {
    let Some(bucket) = chart.buckets.get(index) else {
        return SummaryMetrics::default();
    };
    if let Some(project_key) = candidate.project_key.as_deref() {
        return chart
            .project_values
            .get(project_key)
            .and_then(|values| values.get(&bucket.starts_at))
            .copied()
            .unwrap_or_default();
    }
    candidate
        .aggregate_values
        .as_ref()
        .and_then(|values| values.get(index))
        .copied()
        .unwrap_or_default()
}

fn summary_chart_project_count(
    prepared: &PreparedSummary,
    metric: SummaryMetric,
    api_long_context: bool,
) -> usize {
    prepared
        .usage
        .projects
        .iter()
        .filter(|project| metric.value(project.totals, api_long_context) > 0)
        .count()
}

fn summary_project_series(
    app: &App,
    prepared: &PreparedSummary,
    chart: &SummaryChartData,
    project_colors: &HashMap<String, Color>,
) -> Vec<SummaryProjectSeries> {
    let projects = summary_project_order(
        &prepared.usage,
        app.summary_metric,
        app.api_long_context_multiplier,
    )
    .into_iter()
    // A project with a zero total for the selected additive metric cannot
    // contribute a visible value on any day. Keep it in the tree and ranking
    // panels, but do not allocate and rasterize an all-zero chart series.
    .filter(|project| {
        app.summary_metric
            .value(project.totals, app.api_long_context_multiplier)
            > 0
    })
    .collect::<Vec<_>>();
    let direct_count = if app.summary_show_all_projects {
        projects.len()
    } else {
        projects.len().min(SUMMARY_STACKED_PROJECT_LIMIT)
    };
    let display_labels = summary_project_display_labels(&prepared.usage);
    let mut series = projects[..direct_count]
        .iter()
        .map(|project| SummaryProjectSeries {
            project_key: Some(project.key.clone()),
            label: display_labels
                .get(&project.key)
                .cloned()
                .unwrap_or_else(|| project.label.clone()),
            color: project_colors
                .get(&project.key)
                .copied()
                .unwrap_or_else(|| summary_project_color(&project.key, app.theme)),
            aggregate_values: None,
        })
        .collect::<Vec<_>>();
    if direct_count < projects.len() {
        let mut other_days = vec![SummaryMetrics::default(); chart.buckets.len()];
        for project in &projects[direct_count..] {
            if let Some(values) = chart.project_values.get(&project.key) {
                for (starts_at, value) in values {
                    if let Ok(index) = chart
                        .buckets
                        .binary_search_by_key(starts_at, |bucket| bucket.starts_at)
                    {
                        other_days[index].add_assign(*value);
                    }
                }
            }
        }
        series.push(SummaryProjectSeries {
            project_key: None,
            label: "Other".to_string(),
            color: app.theme.palette().muted,
            aggregate_values: Some(other_days),
        });
    }
    series
}

fn summary_bucket_label(starts_at: NaiveDateTime, grain: SummaryGrain, axis: bool) -> String {
    if grain == SummaryGrain::Day {
        starts_at.format("%m-%d").to_string()
    } else if axis {
        starts_at.format("%m-%d %Hh").to_string()
    } else {
        starts_at.format("%m-%d %H:%M").to_string()
    }
}

fn summary_bucket_dst_note(
    bucket: &SummaryChartBucket,
    grain: SummaryGrain,
) -> Option<&'static str> {
    let nominal_buckets = usize::try_from(grain.hours())
        .unwrap_or(usize::MAX)
        .saturating_mul(usize::try_from(60 / LOCAL_BUCKET_MINUTES).unwrap_or_default());
    (bucket.coverage.expected_buckets > nominal_buckets).then_some("DST overlap")
}

fn summary_project_series_has_values(
    series: &[SummaryProjectSeries],
    chart: &SummaryChartData,
    states: &[SummaryDailyState],
    metric: SummaryMetric,
    long_context_multiplier: bool,
) -> bool {
    series.iter().any(|candidate| {
        states.iter().enumerate().any(|(index, state)| {
            *state != SummaryDailyState::Missing
                && metric.value(
                    summary_project_metrics_at(candidate, chart, index),
                    long_context_multiplier,
                ) > 0
        })
    })
}

fn prioritized_summary_project_series<'a>(
    series: &'a [SummaryProjectSeries],
    selected_project: Option<&str>,
) -> Vec<&'a SummaryProjectSeries> {
    let selected = selected_project.and_then(|selected| {
        series
            .iter()
            .find(|candidate| candidate.project_key.as_deref() == Some(selected))
    });
    let other = series
        .iter()
        .find(|candidate| candidate.project_key.is_none());
    selected
        .into_iter()
        .chain(other)
        .chain(series.iter().filter(|candidate| {
            selected.is_none_or(|selected| !std::ptr::eq(*candidate, selected))
                && other.is_none_or(|other| !std::ptr::eq(*candidate, other))
        }))
        .collect()
}

fn summary_project_legend_line(
    series: &[SummaryProjectSeries],
    selected_project: Option<&str>,
    width: u16,
    theme: Theme,
) -> Line<'static> {
    let width = usize::from(width);
    let mut used = 0_usize;
    let mut spans = Vec::new();
    let mut shown = 0_usize;
    for candidate in prioritized_summary_project_series(series, selected_project) {
        let label = truncate_display_text(&candidate.label, 18);
        let separator = if shown == 0 { "" } else { "  " };
        let item_width = UnicodeWidthStr::width(separator)
            .saturating_add(2)
            .saturating_add(UnicodeWidthStr::width(label.as_str()));
        if used.saturating_add(item_width) > width {
            break;
        }
        spans.push(Span::raw(separator.to_string()));
        spans.push(Span::styled("■ ", Style::default().fg(candidate.color)));
        let selected = candidate.project_key.as_deref() == selected_project;
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(if selected {
                    theme.palette().title
                } else {
                    theme.palette().foreground
                })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
        used = used.saturating_add(item_width);
        shown = shown.saturating_add(1);
    }
    let omitted = series.len().saturating_sub(shown);
    if omitted > 0 {
        let suffix = format!("  +{omitted}");
        if used.saturating_add(UnicodeWidthStr::width(suffix.as_str())) <= width {
            spans.push(Span::styled(
                suffix,
                Style::default().fg(theme.palette().muted),
            ));
        }
    }
    Line::from(spans)
}

fn summary_daily_readout_line(
    chart: &SummaryChartData,
    series: &[SummaryProjectSeries],
    selected_project: Option<&str>,
    index: usize,
    state: SummaryDailyState,
    width: u16,
    app: &App,
) -> Line<'static> {
    let Some(bucket) = chart.buckets.get(index) else {
        return Line::default();
    };
    let mut bucket_label = summary_bucket_label(bucket.starts_at, chart.grain, false);
    if let Some(note) = summary_bucket_dst_note(bucket, chart.grain) {
        bucket_label.push_str(" (");
        bucket_label.push_str(note);
        bucket_label.push(')');
    }
    let palette = app.theme.palette();
    let status = match state {
        SummaryDailyState::Complete => ("C", palette.accent),
        SummaryDailyState::Partial => ("P lower bound", palette.warning),
        SummaryDailyState::Missing => ("MISSING", palette.warning),
    };
    let mut spans = vec![
        Span::styled(
            bucket_label.clone(),
            Style::default()
                .fg(palette.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · "),
        Span::styled(
            status.0,
            Style::default().fg(status.1).add_modifier(Modifier::BOLD),
        ),
    ];
    let mut used = UnicodeWidthStr::width(bucket_label.as_str())
        .saturating_add(3)
        .saturating_add(UnicodeWidthStr::width(status.0));
    if state == SummaryDailyState::Missing {
        let message = " · no local project evidence";
        if used.saturating_add(UnicodeWidthStr::width(message)) <= usize::from(width) {
            spans.push(Span::styled(message, Style::default().fg(palette.muted)));
        }
        return Line::from(spans);
    }

    let visible = prioritized_summary_project_series(series, selected_project)
        .into_iter()
        .filter(|candidate| {
            let selected = candidate.project_key.as_deref() == selected_project;
            let metrics = summary_project_metrics_at(candidate, chart, index);
            selected
                || app
                    .summary_metric
                    .value(metrics, app.api_long_context_multiplier)
                    > 0
                || metrics.api_equivalent_cost.observed_samples > 0
        })
        .collect::<Vec<_>>();
    let total = format_summary_metric(
        bucket.totals,
        app.summary_metric,
        app.api_long_context_multiplier,
    );
    let total_text = format!(" · Total {total}");
    let total_width = UnicodeWidthStr::width(total_text.as_str());
    let minimum_item_width = |candidate: &SummaryProjectSeries| {
        let value = format_summary_metric(
            summary_project_metrics_at(candidate, chart, index),
            app.summary_metric,
            app.api_long_context_multiplier,
        );
        // Preserve the complete "Other" label because it is the sole cue that
        // the remaining projects were aggregated. A selected project may use a
        // one-cell truncated label in very narrow layouts, but its swatch and
        // complete value remain visible.
        let label_width = if candidate.project_key.is_none() {
            UnicodeWidthStr::width(candidate.label.as_str()).min(14)
        } else {
            1
        };
        6_usize
            .saturating_add(label_width.max(1))
            .saturating_add(UnicodeWidthStr::width(value.as_str()))
    };
    let omission_width = |omitted: usize| {
        if omitted == 0 {
            0
        } else {
            UnicodeWidthStr::width(format!(" · +{omitted}").as_str())
        }
    };
    let preferred_count = if visible.first().is_some_and(|candidate| {
        candidate.project_key.as_deref() == selected_project && selected_project.is_some()
    }) && visible
        .get(1)
        .is_some_and(|candidate| candidate.project_key.is_none())
    {
        2
    } else {
        visible.len().min(1)
    };
    let preferred_reserve = visible
        .iter()
        .take(preferred_count)
        .map(|candidate| minimum_item_width(candidate))
        .fold(0_usize, usize::saturating_add)
        .saturating_add(omission_width(
            visible.len().saturating_sub(preferred_count),
        ));
    let mandatory_count =
        if preferred_count > 1 && used.saturating_add(preferred_reserve) > usize::from(width) {
            1
        } else {
            preferred_count
        };
    let mandatory_reserve = visible
        .iter()
        .take(mandatory_count)
        .map(|candidate| minimum_item_width(candidate))
        .fold(0_usize, usize::saturating_add)
        .saturating_add(omission_width(
            visible.len().saturating_sub(mandatory_count),
        ));
    if used
        .saturating_add(total_width)
        .saturating_add(mandatory_reserve)
        <= usize::from(width)
    {
        used = used.saturating_add(total_width);
        spans.push(Span::styled(
            total_text,
            Style::default().fg(palette.foreground),
        ));
    }
    let mut shown = 0_usize;
    for (position, candidate) in visible.iter().enumerate() {
        let metrics = summary_project_metrics_at(candidate, chart, index);
        let value =
            format_summary_metric(metrics, app.summary_metric, app.api_long_context_multiplier);
        let reserve_after = if position < mandatory_count {
            visible
                .iter()
                .skip(position.saturating_add(1))
                .take(mandatory_count.saturating_sub(position.saturating_add(1)))
                .map(|candidate| minimum_item_width(candidate))
                .fold(0_usize, usize::saturating_add)
                .saturating_add(omission_width(
                    visible.len().saturating_sub(mandatory_count),
                ))
        } else {
            omission_width(visible.len().saturating_sub(position.saturating_add(1)))
        };
        let available = usize::from(width)
            .saturating_sub(used)
            .saturating_sub(reserve_after);
        // " · " + "■ " + " " before the value.
        let fixed_width = 6_usize.saturating_add(UnicodeWidthStr::width(value.as_str()));
        if available <= fixed_width {
            break;
        }
        let label_width = available.saturating_sub(fixed_width).min(14);
        let label = truncate_display_text(&candidate.label, label_width);
        let selected = candidate.project_key.as_deref() == selected_project;
        let body_style = summary_project_body_style(app.theme, candidate.color, selected);
        spans.push(Span::raw(" · "));
        spans.push(Span::styled("■ ", Style::default().fg(candidate.color)));
        spans.push(Span::styled(label.clone(), body_style));
        spans.push(Span::styled(format!(" {value}"), body_style));
        let item_width = fixed_width.saturating_add(UnicodeWidthStr::width(label.as_str()));
        used = used.saturating_add(item_width);
        shown = shown.saturating_add(1);
    }
    let omitted = visible.len().saturating_sub(shown);
    if omitted > 0 {
        let suffix = format!(" · +{omitted}");
        if used.saturating_add(UnicodeWidthStr::width(suffix.as_str())) <= usize::from(width) {
            spans.push(Span::styled(suffix, Style::default().fg(palette.muted)));
        }
    }
    Line::from(spans)
}

fn summary_date_column(plot: Rect, date_index: usize, date_count: usize) -> Option<u16> {
    if plot.is_empty() || date_index >= date_count {
        return None;
    }
    if plot.width <= 1 || date_count <= 1 {
        return Some(plot.x);
    }
    let numerator = date_index
        .saturating_mul(usize::from(plot.width - 1))
        .saturating_mul(2)
        .saturating_add(date_count - 1);
    let denominator = (date_count - 1).saturating_mul(2);
    let offset = numerator.checked_div(denominator).unwrap_or_default();
    Some(
        plot.x
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
    )
}

fn render_summary_annotations(
    frame: &mut Frame<'_>,
    inner: Rect,
    theme: Theme,
    annotations: &[Line<'static>],
) {
    for (index, line) in annotations.iter().cloned().enumerate() {
        frame.render_widget(
            Paragraph::new(line).style(theme.base_style()),
            Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                inner.width,
                1,
            ),
        );
    }
}

fn summary_daily_is_lower_bound(
    prepared: &PreparedSummary,
    metric: SummaryMetric,
    api_long_context: bool,
    complete_buckets: usize,
    total_buckets: usize,
) -> bool {
    complete_buckets < total_buckets
        || prepared.partial(metric, api_long_context)
        || (metric == SummaryMetric::ApiEquivalent && prepared.api_chart_is_lower_bound())
}

fn render_summary_daily(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    prepared: &PreparedSummary,
    chart: &SummaryChartData,
    project_colors: &HashMap<String, Color>,
) {
    let states = chart
        .buckets
        .iter()
        .map(|bucket| {
            prepared.chart_bucket_state(
                bucket,
                chart.grain,
                app.summary_metric,
                app.api_long_context_multiplier,
            )
        })
        .collect::<Vec<_>>();
    let complete_buckets = states
        .iter()
        .filter(|state| **state == SummaryDailyState::Complete)
        .count();
    let partial_buckets = states
        .iter()
        .filter(|state| **state == SummaryDailyState::Partial)
        .count();
    let known_buckets = complete_buckets.saturating_add(partial_buckets);
    let total_buckets = states.len();
    let missing_buckets = total_buckets.saturating_sub(known_buckets);
    let metric = match app.summary_metric {
        SummaryMetric::Tokens => "Tokens",
        SummaryMetric::Estimated => "~EST CR.",
        SummaryMetric::ApiEquivalent => "API EQ.",
    };
    let chart_project_count = summary_chart_project_count(
        prepared,
        app.summary_metric,
        app.api_long_context_multiplier,
    );
    let project_mode =
        if app.summary_show_all_projects || chart_project_count <= SUMMARY_STACKED_PROJECT_LIMIT {
            "all projects".to_string()
        } else {
            format!("Top {SUMMARY_STACKED_PROJECT_LIMIT} + Other")
        };
    let title = format!(
        "Project mix · {metric} · {} local · {complete_buckets}C/{partial_buckets}P/{missing_buckets}M · {project_mode}",
        chart.grain.label()
    );
    let lower_bound = summary_daily_is_lower_bound(
        prepared,
        app.summary_metric,
        app.api_long_context_multiplier,
        complete_buckets,
        total_buckets,
    );
    let block = panel(&title, app.theme);
    let inner = block.inner(area);
    if inner.is_empty() {
        frame.render_widget(block, area);
        return;
    }
    frame.render_widget(block, area);
    let palette = app.theme.palette();
    let series = summary_project_series(app, prepared, chart, project_colors);
    let has_plotted_values = summary_project_series_has_values(
        &series,
        chart,
        &states,
        app.summary_metric,
        app.api_long_context_multiplier,
    );
    let can_plot = known_buckets > 0 && !series.is_empty() && has_plotted_values;
    if !can_plot {
        app.summary_inspected_date = None;
        app.summary_daily_dragging = false;
    }
    let mut explicit_inspection = app.summary_inspected_date.and_then(|date| {
        chart
            .buckets
            .iter()
            .position(|candidate| candidate.starts_at == date)
    });
    if app.summary_inspected_date.is_some() && explicit_inspection.is_none() {
        app.summary_inspected_date = None;
    }
    let automatic_readout_index = states
        .iter()
        .rposition(|state| *state != SummaryDailyState::Missing);
    let readout_index = explicit_inspection.or(automatic_readout_index);
    let selected_project = app
        .summary_selected_id
        .as_deref()
        .and_then(|selected| selected.strip_prefix("project:"));
    let readout = readout_index.map(|index| {
        summary_daily_readout_line(
            chart,
            &series,
            selected_project,
            index,
            states[index],
            inner.width,
            app,
        )
    });
    let project_legend = (!series.is_empty())
        .then(|| summary_project_legend_line(&series, selected_project, inner.width, app.theme));
    let mut coverage_legend = Vec::new();
    if lower_bound {
        coverage_legend.push(Span::styled(
            "LOWER BOUND · ",
            Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ));
    }
    coverage_legend.extend([
        Span::styled("C", Style::default().fg(palette.accent)),
        Span::styled(" complete · ", Style::default().fg(palette.muted)),
        Span::styled("P", Style::default().fg(palette.warning)),
        Span::styled(" partial · M missing", Style::default().fg(palette.muted)),
    ]);
    let coverage_legend = Line::from(coverage_legend);
    let status_prefix = if lower_bound {
        "LOWER · C/P/M "
    } else {
        "C/P/M "
    };
    let status = if usize::from(inner.width) >= UnicodeWidthStr::width(status_prefix) + states.len()
    {
        let mut status = vec![Span::styled(
            status_prefix,
            Style::default().fg(if lower_bound {
                palette.warning
            } else {
                palette.muted
            }),
        )];
        let symbols = summary_daily_status_symbols(&states);
        status.extend(states.iter().zip(symbols.chars()).map(|(state, symbol)| {
            let style = match state {
                SummaryDailyState::Complete => Style::default().fg(palette.accent),
                SummaryDailyState::Partial => Style::default().fg(palette.warning),
                SummaryDailyState::Missing => Style::default()
                    .fg(palette.muted)
                    .add_modifier(Modifier::DIM),
            };
            Span::styled(symbol.to_string(), style)
        }));
        Some(Line::from(status))
    } else {
        None
    };
    let annotation_limit = usize::from(inner.height.saturating_sub(5));
    let mut annotations = Vec::<Line<'static>>::new();
    if let Some(readout) = readout {
        annotations.push(readout);
    }
    let remaining = annotation_limit.saturating_sub(annotations.len());
    if remaining >= 3 {
        if let Some(project_legend) = project_legend {
            annotations.push(project_legend);
        }
        annotations.push(coverage_legend);
        if let Some(status) = status {
            annotations.push(status);
        }
    } else if remaining == 2 {
        if let Some(project_legend) = project_legend {
            annotations.push(project_legend);
        }
        if let Some(status) = status {
            annotations.push(status);
        } else {
            annotations.push(coverage_legend);
        }
    } else if remaining == 1 {
        if let Some(status) = status {
            annotations.push(status);
        } else {
            annotations.push(coverage_legend);
        }
    }
    annotations.truncate(annotation_limit);
    let annotation_height = u16::try_from(annotations.len()).unwrap_or(inner.height);
    let chart_area = Rect::new(
        inner.x,
        inner.y.saturating_add(annotation_height),
        inner.width,
        inner.height.saturating_sub(annotation_height),
    );
    if !can_plot {
        render_summary_annotations(frame, inner, app.theme, &annotations);
        let message = if known_buckets == 0 {
            "No project-level history for these local time buckets; missing buckets are unknown"
        } else if !has_plotted_values {
            "Known time buckets contain no non-zero project usage for this metric"
        } else {
            "No project-level time series for this range"
        };
        frame.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .style(Style::default().fg(app.theme.palette().muted)),
            chart_area,
        );
        return;
    }
    let maximum_value = chart
        .buckets
        .iter()
        .zip(states.iter().copied())
        .filter(|(_, state)| *state != SummaryDailyState::Missing)
        .map(|(bucket, _)| {
            app.summary_metric
                .value(bucket.totals, app.api_long_context_multiplier) as f64
        })
        .fold(0.0_f64, f64::max);
    let y_max = if maximum_value <= 0.0 {
        1.0
    } else {
        nice_trend_maximum(maximum_value)
    };
    let x_max = chart.buckets.len().saturating_sub(1).max(1) as f64;
    let first = chart
        .buckets
        .first()
        .map(|bucket| summary_bucket_label(bucket.starts_at, chart.grain, true))
        .unwrap_or_default();
    let last = chart
        .buckets
        .last()
        .map(|bucket| summary_bucket_label(bucket.starts_at, chart.grain, true))
        .unwrap_or_default();
    let middle = chart
        .buckets
        .get(chart.buckets.len() / 2)
        .map(|bucket| summary_bucket_label(bucket.starts_at, chart.grain, true))
        .unwrap_or_default();
    let x_labels = vec![first, middle, last];
    let y_labels = vec![
        format_summary_axis(0.0, app.summary_metric),
        format_summary_axis(y_max / 2.0, app.summary_metric),
        format_summary_axis(y_max, app.summary_metric),
    ];
    let geometry = trend_chart_geometry(chart_area, &x_labels, &y_labels, &[]);
    let exact_hitbox = geometry.and_then(|geometry| {
        SummaryDailyHitbox::exact(
            geometry.plot,
            chart
                .buckets
                .iter()
                .map(|bucket| bucket.starts_at)
                .collect(),
        )
    });
    if exact_hitbox.is_none() {
        app.summary_inspected_date = None;
        app.summary_daily_dragging = false;
        explicit_inspection = None;
        if let (Some(index), Some(readout)) = (automatic_readout_index, annotations.first_mut()) {
            *readout = summary_daily_readout_line(
                chart,
                &series,
                selected_project,
                index,
                states[index],
                inner.width,
                app,
            );
        }
    }
    app.summary_daily_hitbox = exact_hitbox;
    render_summary_annotations(frame, inner, app.theme, &annotations);
    frame.render_widget(
        Chart::new(Vec::<Dataset<'_>>::new())
            .style(app.theme.base_style())
            .x_axis(
                Axis::default()
                    .bounds([0.0, x_max])
                    .labels(x_labels)
                    .style(Style::default().fg(app.theme.palette().muted)),
            )
            .y_axis(
                Axis::default()
                    .bounds([0.0, y_max])
                    .labels(y_labels)
                    .style(Style::default().fg(app.theme.palette().muted)),
            ),
        chart_area,
    );
    let Some(geometry) = geometry else {
        return;
    };
    let area_series = series
        .iter()
        .map(|candidate| {
            StackedAreaSeries::new(
                candidate.color,
                (0..chart.buckets.len())
                    .map(|index| {
                        app.summary_metric.value(
                            summary_project_metrics_at(candidate, chart, index),
                            app.api_long_context_multiplier,
                        )
                    })
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    let area_states = states
        .iter()
        .map(|state| match state {
            SummaryDailyState::Complete => StackedAreaState::Complete,
            SummaryDailyState::Partial => StackedAreaState::Partial,
            SummaryDailyState::Missing => StackedAreaState::Missing,
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        StackedArea::new(&area_series, &area_states, y_max, palette.background),
        geometry.plot,
    );
    if app.summary_daily_hitbox.is_some()
        && let Some(index) = explicit_inspection
        && let Some(column) = summary_date_column(geometry.plot, index, chart.buckets.len())
    {
        for row in geometry.plot.y..geometry.plot.bottom() {
            if let Some(cell) = frame.buffer_mut().cell_mut((column, row)) {
                cell.set_symbol("│").set_fg(palette.title);
            }
        }
    }
}

fn render_summary_at(frame: &mut Frame<'_>, area: Rect, app: &mut App, now: DateTime<Utc>) {
    ensure_summary_cache(app, now);
    let rows = app.summary_rows();
    if rows.is_empty() {
        app.summary_selected_id = None;
        app.summary_offset = 0;
    } else if app
        .summary_selected_id
        .as_deref()
        .is_none_or(|selected| !rows.iter().any(|row| row.id == selected))
    {
        app.summary_selected_id = Some(rows[0].id.clone());
    }
    let Some(cache) = app.summary_cache.take() else {
        return;
    };
    let chart_project_count = summary_chart_project_count(
        &cache.prepared,
        app.summary_metric,
        app.api_long_context_multiplier,
    );
    if chart_project_count <= SUMMARY_STACKED_PROJECT_LIMIT {
        app.summary_show_all_projects = false;
    }
    let mut project_colors = std::mem::take(&mut app.summary_project_colors);
    if project_colors.is_empty() {
        extend_summary_project_colors_from_history(&mut project_colors, &app.history, app.theme);
    }
    extend_assigned_summary_project_colors(
        &mut project_colors,
        cache
            .prepared
            .usage
            .projects
            .iter()
            .map(|project| project.key.as_str()),
        app.theme,
    );
    let controls_and_body =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    let body = controls_and_body[1];
    let selected_can_toggle = app.summary_selected_id.as_deref().is_some_and(|selected| {
        rows.iter()
            .any(|row| row.id == selected && row.has_children)
    });
    let can_collapse_all = !app.summary_expanded_nodes.is_empty();
    let can_toggle_all_projects = chart_project_count > SUMMARY_STACKED_PROJECT_LIMIT;
    if body.height < 18 {
        let sections =
            Layout::vertical([Constraint::Percentage(65), Constraint::Percentage(35)]).split(body);
        render_summary_tree(
            frame,
            sections[0],
            app,
            &cache.prepared,
            &rows,
            &project_colors,
        );
        render_summary_bars(frame, sections[1], app, &cache.prepared, &project_colors);
    } else if body.width < 110 || body.height < 28 {
        let sections = Layout::vertical([
            Constraint::Percentage(34),
            Constraint::Percentage(23),
            Constraint::Percentage(43),
        ])
        .split(body);
        render_summary_tree(
            frame,
            sections[0],
            app,
            &cache.prepared,
            &rows,
            &project_colors,
        );
        render_summary_bars(frame, sections[1], app, &cache.prepared, &project_colors);
        render_summary_daily(
            frame,
            sections[2],
            app,
            &cache.prepared,
            &cache.chart,
            &project_colors,
        );
    } else {
        let sections =
            Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).split(body);
        let top = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(sections[0]);
        render_summary_tree(frame, top[0], app, &cache.prepared, &rows, &project_colors);
        render_summary_bars(frame, top[1], app, &cache.prepared, &project_colors);
        render_summary_daily(
            frame,
            sections[1],
            app,
            &cache.prepared,
            &cache.chart,
            &project_colors,
        );
    }
    let can_inspect = app.summary_daily_hitbox.is_some();
    if !can_inspect {
        app.summary_inspected_date = None;
        app.summary_daily_dragging = false;
    }
    app.summary_controls_hitbox = Some(render_summary_controls(
        frame,
        controls_and_body[0],
        app,
        selected_can_toggle,
        can_collapse_all,
        can_toggle_all_projects,
        can_inspect,
    ));
    app.summary_project_colors = project_colors;
    app.summary_cache = Some(cache);
}

fn render_trend_controls(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    compact: bool,
) -> TrendControlsHitbox {
    let palette = app.theme.palette();
    let mut hitbox = TrendControlsHitbox::default();
    if area.is_empty() {
        return hitbox;
    }

    let full_width = TrendSection::ALL
        .into_iter()
        .map(|section| 3 + UnicodeWidthStr::width(section.label()))
        .sum::<usize>()
        + UnicodeWidthStr::width("[I]Inspect")
        + UnicodeWidthStr::width("[[]Prev")
        + UnicodeWidthStr::width("[]]Next")
        + UnicodeWidthStr::width("[N]Now")
        + 6;
    let terse = compact && full_width > usize::from(area.width);
    let mut spans = Vec::new();
    let mut x = area.x;
    if compact {
        for section in TrendSection::ALL {
            let suffix = if terse {
                "]"
            } else {
                section_label_suffix(section)
            };
            let shortcut = section.shortcut().to_string();
            hitbox.sections[section.index()] = append_trend_control(
                &mut spans,
                area,
                &mut x,
                TrendControlSpec {
                    shortcut: &shortcut,
                    suffix,
                    selected: app.trend_section == section,
                    shortcuts_active: app.shortcuts_active(),
                    theme: app.theme,
                },
            );
        }
    }
    hitbox.inspect = append_trend_control(
        &mut spans,
        area,
        &mut x,
        TrendControlSpec {
            shortcut: "I",
            suffix: if terse { "]" } else { "]Inspect" },
            selected: app.trend_inspect_mode,
            shortcuts_active: app.shortcuts_active(),
            theme: app.theme,
        },
    );
    if !compact || app.trend_section == TrendSection::HalfHour {
        hitbox.previous_day = append_trend_control(
            &mut spans,
            area,
            &mut x,
            TrendControlSpec {
                shortcut: "[",
                suffix: if terse { "]" } else { "]Prev" },
                selected: false,
                shortcuts_active: app.shortcuts_active(),
                theme: app.theme,
            },
        );
        hitbox.next_day = append_trend_control(
            &mut spans,
            area,
            &mut x,
            TrendControlSpec {
                shortcut: "]",
                suffix: if terse { "]" } else { "]Next" },
                selected: false,
                shortcuts_active: app.shortcuts_active(),
                theme: app.theme,
            },
        );
        hitbox.now = append_trend_control(
            &mut spans,
            area,
            &mut x,
            TrendControlSpec {
                shortcut: "N",
                suffix: if terse { "]" } else { "]Now" },
                selected: app.trend_day_offset == 0,
                shortcuts_active: app.shortcuts_active(),
                theme: app.theme,
            },
        );

        let offset_label = if app.trend_day_offset == 0 {
            " · latest 24h".to_string()
        } else {
            format!(" · {}d back", app.trend_day_offset)
        };
        if x.saturating_add(
            u16::try_from(UnicodeWidthStr::width(offset_label.as_str())).unwrap_or(u16::MAX),
        ) <= area.right()
        {
            spans.push(Span::styled(
                offset_label,
                Style::default().fg(palette.muted),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    hitbox
}

fn section_label_suffix(section: TrendSection) -> &'static str {
    match section {
        TrendSection::Remaining => "]Remaining",
        TrendSection::Weekly => "]Weekly",
        TrendSection::HalfHour => "]15-minute",
    }
}

fn append_trend_control(
    spans: &mut Vec<Span<'static>>,
    area: Rect,
    x: &mut u16,
    spec: TrendControlSpec<'_>,
) -> Rect {
    let gap_width = u16::from(*x > area.x);
    let width = u16::try_from(
        1 + UnicodeWidthStr::width(spec.shortcut) + UnicodeWidthStr::width(spec.suffix),
    )
    .unwrap_or(u16::MAX);
    if x.saturating_add(gap_width).saturating_add(width) > area.right() {
        return Rect::default();
    }
    if gap_width > 0 {
        spans.push(Span::raw(" "));
        *x = x.saturating_add(1);
    }

    let palette = spec.theme.palette();
    let style = if spec.selected {
        Style::default()
            .fg(palette.background)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let shortcut_style = if !spec.shortcuts_active {
        style
    } else if spec.selected {
        style.add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    };
    let hitbox = clipped_horizontal_hitbox(area, *x, width);
    spans.push(Span::styled("[", style));
    spans.push(Span::styled(spec.shortcut.to_string(), shortcut_style));
    spans.push(Span::styled(spec.suffix, style));
    *x = x.saturating_add(width);
    hitbox
}

fn prepare_trend_data_at(app: &App, now: DateTime<Utc>) -> PreparedTrendData {
    let five_hour_remaining = remaining_trend(&app.history, 300);
    let weekly_remaining = remaining_trend(&app.history, 10_080);
    let five_hour_remaining_readout = remaining_trend_readout(&app.history, 300, now);
    let weekly_remaining_readout = remaining_trend_readout(&app.history, 10_080, now);
    let half_hour_bounds = trend_day_bounds(now, app.trend_day_offset);
    let weekly_reset = app.history.latest_weekly_reset();

    let weekly_cumulative = weekly_reset
        .map(|reset| app.history.weekly_cumulative_series(reset))
        .unwrap_or_default();
    let weekly_estimated_cumulative = weekly_reset
        .map(|reset| {
            app.history.weekly_cumulative_series_with_api_long_context(
                reset,
                app.api_long_context_multiplier,
            )
        })
        .unwrap_or_default();
    let weekly_history_present = weekly_cumulative
        .iter()
        .any(|point| point.sampled_at.is_some());
    let weekly_tokens = weekly_cumulative
        .iter()
        .map(|point| TrendPoint {
            at: point.at,
            value: point.token_usage.total_tokens as f64,
            readout_value: TrendReadoutValue::Tokens(point.token_usage.total_tokens),
            sampled_at: point.sampled_at,
            interval: None,
            partial: !point.partial_reasons.is_empty(),
        })
        .collect();
    let weekly_estimated = weekly_estimated_cumulative
        .iter()
        .filter_map(|point| {
            point.estimated_quota_percent.map(|value| TrendPoint {
                at: point.at,
                value,
                readout_value: TrendReadoutValue::Percent(value),
                sampled_at: point.sampled_at,
                interval: None,
                partial: !point.partial_reasons.is_empty(),
            })
        })
        .collect();
    let weekly_readout_point = weekly_reset
        .filter(|reset| {
            let starts_at = *reset - ChronoDuration::minutes(10_080);
            starts_at <= now && now < *reset
        })
        .and_then(|_| {
            weekly_cumulative
                .iter()
                .rfind(|point| point.sampled_at.is_some_and(|sampled_at| sampled_at <= now))
                .and_then(|point| point.sampled_at.map(|sampled_at| (point, sampled_at)))
        });
    let weekly_tokens_readout = weekly_readout_point.map(|(point, sampled_at)| TrendReadout {
        sampled_at,
        value: TrendReadoutValue::Tokens(point.token_usage.total_tokens),
        interval: None,
        partial: !point.partial_reasons.is_empty(),
    });
    let weekly_estimated_readout_point = weekly_reset
        .filter(|reset| {
            let starts_at = *reset - ChronoDuration::minutes(10_080);
            starts_at <= now && now < *reset
        })
        .and_then(|_| {
            weekly_estimated_cumulative
                .iter()
                .rfind(|point| point.sampled_at.is_some_and(|sampled_at| sampled_at <= now))
                .and_then(|point| point.sampled_at.map(|sampled_at| (point, sampled_at)))
        });
    let weekly_estimated_readout =
        weekly_estimated_readout_point.and_then(|(point, sampled_at)| {
            point.estimated_quota_percent.map(|value| TrendReadout {
                sampled_at,
                value: TrendReadoutValue::Percent(value),
                interval: None,
                partial: !point.partial_reasons.is_empty(),
            })
        });

    let half_hour_buckets = app
        .history
        .half_hour_series()
        .iter()
        .filter(|bucket| {
            bucket.starts_at >= half_hour_bounds[0] && bucket.starts_at < half_hour_bounds[1]
        })
        .collect::<Vec<_>>();
    let half_hour_history_present = !half_hour_buckets.is_empty();
    let half_hour_tokens = half_hour_buckets
        .iter()
        .map(|bucket| TrendPoint {
            at: bucket.starts_at + (bucket.ends_at - bucket.starts_at) / 2,
            value: bucket.token_usage.total_tokens as f64,
            readout_value: TrendReadoutValue::Tokens(bucket.token_usage.total_tokens),
            sampled_at: Some(bucket.sampled_at),
            interval: Some(TrendInterval {
                starts_at: bucket.starts_at,
                ends_at: bucket.ends_at,
            }),
            partial: !bucket.partial_reasons.is_empty(),
        })
        .collect();
    let half_hour_estimated = half_hour_estimated_trend(
        &app.history,
        half_hour_bounds,
        app.api_long_context_multiplier,
    );

    PreparedTrendData {
        five_hour_remaining,
        weekly_remaining,
        weekly_tokens,
        weekly_estimated,
        half_hour_tokens,
        half_hour_estimated,
        five_hour_remaining_readout,
        weekly_remaining_readout,
        weekly_tokens_readout,
        weekly_estimated_readout,
        half_hour_bounds,
        weekly_history_present,
        half_hour_history_present,
        history_warning_count: app.history.warnings.len(),
        history_read_only: app.history.read_only,
        api_long_context_multiplier: app.api_long_context_multiplier,
    }
}

fn remaining_trend_readout(
    history: &HistoryData,
    duration_mins: i64,
    now: DateTime<Utc>,
) -> Option<TrendReadout> {
    history
        .quota_points
        .iter()
        .filter(|point| {
            point.duration_mins == duration_mins
                && point.observed_at <= now
                && now < point.resets_at
        })
        .max_by_key(|point| (point.observed_at, point.resets_at))
        .map(|point| TrendReadout {
            sampled_at: point.observed_at,
            value: TrendReadoutValue::Percent(point.remaining_percent),
            interval: None,
            partial: false,
        })
}

fn half_hour_estimated_trend(
    history: &HistoryData,
    bounds: [DateTime<Utc>; 2],
    api_long_context: bool,
) -> Vec<TrendPoint> {
    const WEEKLY_WINDOW_MINUTES: i64 = 10_080;
    let resets = weekly_resets_overlapping(history, bounds);
    let estimates_by_reset = resets
        .iter()
        .copied()
        .map(|reset| {
            let points = history
                .estimated_half_hour_series_with_api_long_context(reset, api_long_context)
                .into_iter()
                .filter(|point| point.starts_at >= bounds[0] && point.starts_at < bounds[1])
                .map(|point| (point.starts_at, point))
                .collect::<BTreeMap<_, _>>();
            (reset, points)
        })
        .collect::<BTreeMap<_, _>>();
    let mut points = BTreeMap::new();
    for bucket in history
        .half_hour_series()
        .iter()
        .filter(|bucket| bucket.starts_at >= bounds[0] && bucket.starts_at < bounds[1])
    {
        let crosses_reset = resets.iter().any(|reset| {
            let cycle_starts_at = *reset - ChronoDuration::minutes(WEEKLY_WINDOW_MINUTES);
            bucket.starts_at < cycle_starts_at && cycle_starts_at < bucket.ends_at
        });
        if crosses_reset {
            continue;
        }
        let Some(reset) = resets
            .iter()
            .copied()
            .filter(|reset| {
                let cycle_starts_at = *reset - ChronoDuration::minutes(WEEKLY_WINDOW_MINUTES);
                bucket.starts_at >= cycle_starts_at && bucket.ends_at <= *reset
            })
            .max()
        else {
            continue;
        };
        let Some(point) = estimates_by_reset
            .get(&reset)
            .and_then(|points| points.get(&bucket.starts_at))
        else {
            continue;
        };
        let Some(value) = point.estimated_quota_percent else {
            continue;
        };
        let at = point.starts_at + (point.ends_at - point.starts_at) / 2;
        points.insert(
            at,
            TrendPoint {
                at,
                value,
                readout_value: TrendReadoutValue::Percent(value),
                sampled_at: Some(bucket.sampled_at),
                interval: Some(TrendInterval {
                    starts_at: point.starts_at,
                    ends_at: point.ends_at,
                }),
                partial: !point.partial_reasons.is_empty(),
            },
        );
    }
    points.into_values().collect()
}

fn weekly_resets_overlapping(
    history: &HistoryData,
    bounds: [DateTime<Utc>; 2],
) -> Vec<DateTime<Utc>> {
    const WEEKLY_WINDOW_MINUTES: i64 = 10_080;
    const RESET_DRIFT_SECONDS: i64 = 120;

    let mut candidates = history
        .quota_points
        .iter()
        .filter(|point| point.duration_mins == WEEKLY_WINDOW_MINUTES)
        .map(|point| (point.resets_at, point.observed_at))
        .chain(
            history
                .weekly_local_points
                .iter()
                .map(|point| (point.resets_at, point.observed_at)),
        )
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|(reset, observed_at)| (*reset, *observed_at));

    let mut resets = Vec::<(DateTime<Utc>, DateTime<Utc>)>::new();
    let mut cluster_end = None;
    let mut representative = None::<(DateTime<Utc>, DateTime<Utc>)>;
    for (reset, observed_at) in candidates {
        let joins_cluster = cluster_end
            .is_some_and(|end| reset - end <= ChronoDuration::seconds(RESET_DRIFT_SECONDS));
        if !joins_cluster && let Some(previous) = representative.take() {
            resets.push(previous);
        }
        cluster_end = Some(reset);
        if representative.is_none_or(|current| (observed_at, reset) > (current.1, current.0)) {
            representative = Some((reset, observed_at));
        }
    }
    if let Some(last) = representative {
        resets.push(last);
    }
    resets.sort_by_key(|(reset, _)| *reset);

    resets
        .into_iter()
        .map(|(reset, _)| reset)
        .filter(|reset| {
            let starts_at = *reset - ChronoDuration::minutes(WEEKLY_WINDOW_MINUTES);
            starts_at < bounds[1] && *reset > bounds[0]
        })
        .collect()
}

fn remaining_trend(history: &HistoryData, duration_mins: i64) -> Vec<TrendPoint> {
    let mut points = history.remaining_series(duration_mins);
    // Keep real observations from every loaded cycle. A reset credit can start
    // a new cycle before the previous resets_at, so the observed timestamps are
    // the honest transition boundary; the renderer still splits recorder gaps.
    points.sort_by(|left, right| {
        left.observed_at
            .cmp(&right.observed_at)
            .then_with(|| left.resets_at.cmp(&right.resets_at))
    });
    points
        .into_iter()
        .map(|point| TrendPoint {
            at: point.observed_at,
            value: point.remaining_percent,
            readout_value: TrendReadoutValue::Percent(point.remaining_percent),
            sampled_at: Some(point.observed_at),
            interval: None,
            partial: false,
        })
        .collect()
}

fn trend_day_bounds(as_of: DateTime<Utc>, day_offset: u16) -> [DateTime<Utc>; 2] {
    let shifted = as_of - ChronoDuration::days(i64::from(day_offset));
    let bucket_seconds = LOCAL_BUCKET_MINUTES * 60;
    let end_seconds = shifted
        .timestamp()
        .saturating_add(bucket_seconds - 1)
        .div_euclid(bucket_seconds)
        .saturating_mul(bucket_seconds);
    let end = DateTime::from_timestamp(end_seconds, 0).unwrap_or(shifted);
    [end - ChronoDuration::hours(24), end]
}

fn render_trends_at(frame: &mut Frame<'_>, area: Rect, app: &mut App, now: DateTime<Utc>) {
    let compact = area.width < 120 || area.height < 29;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    app.trend_controls_hitbox = Some(render_trend_controls(frame, rows[0], app, compact));
    let body = rows[1];
    if body.is_empty() {
        return;
    }
    let data = prepare_trend_data_at(app, now);
    let visible_panels: &[TrendPanelId] = if compact {
        match app.trend_section {
            TrendSection::Remaining => &[TrendPanelId::Remaining],
            TrendSection::Weekly => &[TrendPanelId::WeeklyTokens, TrendPanelId::WeeklyEstimated],
            TrendSection::HalfHour => &[TrendPanelId::LocalTokens, TrendPanelId::LocalEstimated],
        }
    } else {
        &[
            TrendPanelId::Remaining,
            TrendPanelId::WeeklyTokens,
            TrendPanelId::WeeklyEstimated,
            TrendPanelId::LocalTokens,
            TrendPanelId::LocalEstimated,
        ]
    };
    let inspect_mode = app.trend_inspect_mode;
    let mut inspection = app
        .trend_inspection
        .filter(|inspection| visible_panels.contains(&inspection.panel));
    let mut chart_hitboxes = Vec::new();

    if compact {
        match app.trend_section {
            TrendSection::Remaining => {
                if let Some(hitbox) = render_remaining_trend_panel(
                    frame,
                    body,
                    &data,
                    app.theme,
                    inspect_mode,
                    &mut inspection,
                ) {
                    chart_hitboxes.push(hitbox);
                }
            }
            TrendSection::Weekly => {
                let panels = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(body);
                if let Some(hitbox) = render_weekly_token_trend_panel(
                    frame,
                    panels[0],
                    &data,
                    app.theme,
                    inspect_mode,
                    &mut inspection,
                ) {
                    chart_hitboxes.push(hitbox);
                }
                if let Some(hitbox) = render_weekly_estimated_trend_panel(
                    frame,
                    panels[1],
                    &data,
                    app.theme,
                    inspect_mode,
                    &mut inspection,
                ) {
                    chart_hitboxes.push(hitbox);
                }
            }
            TrendSection::HalfHour => {
                let panels = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(body);
                if let Some(hitbox) = render_half_hour_token_trend_panel(
                    frame,
                    panels[0],
                    &data,
                    app.theme,
                    inspect_mode,
                    &mut inspection,
                ) {
                    chart_hitboxes.push(hitbox);
                }
                if let Some(hitbox) = render_half_hour_estimated_trend_panel(
                    frame,
                    panels[1],
                    &data,
                    app.theme,
                    inspect_mode,
                    &mut inspection,
                ) {
                    chart_hitboxes.push(hitbox);
                }
            }
        }
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(body);
        if let Some(hitbox) = render_remaining_trend_panel(
            frame,
            rows[0],
            &data,
            app.theme,
            inspect_mode,
            &mut inspection,
        ) {
            chart_hitboxes.push(hitbox);
        }
        let weekly = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        if let Some(hitbox) = render_weekly_token_trend_panel(
            frame,
            weekly[0],
            &data,
            app.theme,
            inspect_mode,
            &mut inspection,
        ) {
            chart_hitboxes.push(hitbox);
        }
        if let Some(hitbox) = render_weekly_estimated_trend_panel(
            frame,
            weekly[1],
            &data,
            app.theme,
            inspect_mode,
            &mut inspection,
        ) {
            chart_hitboxes.push(hitbox);
        }
        let half_hour = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[2]);
        if let Some(hitbox) = render_half_hour_token_trend_panel(
            frame,
            half_hour[0],
            &data,
            app.theme,
            inspect_mode,
            &mut inspection,
        ) {
            chart_hitboxes.push(hitbox);
        }
        if let Some(hitbox) = render_half_hour_estimated_trend_panel(
            frame,
            half_hour[1],
            &data,
            app.theme,
            inspect_mode,
            &mut inspection,
        ) {
            chart_hitboxes.push(hitbox);
        }
    }
    if inspect_mode
        && inspection.is_none()
        && let Some(default) = chart_hitboxes
            .iter()
            .find_map(TrendChartHitbox::latest_inspection)
    {
        inspection = Some(default);
    }
    app.trend_inspection = inspect_mode.then_some(inspection).flatten();
    app.trend_chart_hitboxes = chart_hitboxes;
}

fn render_empty_trend_panel(frame: &mut Frame<'_>, area: Rect, title: &str, theme: Theme) {
    render_trend_message_panel(frame, area, title, "No history recorded yet", theme, false);
}

fn render_trend_message_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    message: &str,
    theme: Theme,
    warning: bool,
) {
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(if warning {
                theme.palette().warning
            } else {
                theme.palette().muted
            }))
            .alignment(Alignment::Center)
            .block(panel(title, theme)),
        area,
    );
}

fn render_remaining_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
    inspect_mode: bool,
    inspection: &mut Option<TrendInspection>,
) -> Option<TrendChartHitbox> {
    let palette = theme.palette();
    render_time_series_panel(
        frame,
        area,
        &[
            TrendSeries {
                name: "5h",
                points: &data.five_hour_remaining,
                readout: data.five_hour_remaining_readout,
                color: palette.accent,
            },
            TrendSeries {
                name: "Week",
                points: &data.weekly_remaining,
                readout: data.weekly_remaining_readout,
                color: palette.warning,
            },
        ],
        TrendPanelSpec {
            panel: TrendPanelId::Remaining,
            title: "Quota Remaining",
            graph_kind: TrendGraphKind::Line {
                maximum_gap: ChronoDuration::minutes(15),
            },
            value_kind: TrendValueKind::Percent,
            fixed_y_bounds: Some([0.0, 100.0]),
            fixed_x_bounds: None,
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            readout_label: Some("As of"),
            theme,
        },
        inspect_mode,
        inspection,
    )
}

fn render_weekly_token_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
    inspect_mode: bool,
    inspection: &mut Option<TrendInspection>,
) -> Option<TrendChartHitbox> {
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "Tokens",
            points: &data.weekly_tokens,
            readout: data.weekly_tokens_readout,
            color: theme.palette().accent,
        }],
        TrendPanelSpec {
            panel: TrendPanelId::WeeklyTokens,
            title: "Weekly Local Tokens",
            graph_kind: TrendGraphKind::Line {
                maximum_gap: ChronoDuration::minutes(45),
            },
            value_kind: TrendValueKind::Tokens,
            fixed_y_bounds: None,
            fixed_x_bounds: None,
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            readout_label: Some("As of"),
            theme,
        },
        inspect_mode,
        inspection,
    )
}

fn render_weekly_estimated_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
    inspect_mode: bool,
    inspection: &mut Option<TrendInspection>,
) -> Option<TrendChartHitbox> {
    let base_title = if data.api_long_context_multiplier {
        "Weekly ~EST Usage · EST Longx ON"
    } else {
        "Weekly ~EST Usage"
    };
    if data.weekly_estimated.is_empty() && data.weekly_history_present {
        if inspection.is_some_and(|inspection| inspection.panel == TrendPanelId::WeeklyEstimated) {
            *inspection = None;
        }
        let title = trend_panel_status_title(
            base_title,
            data.history_warning_count,
            data.history_read_only,
        );
        render_trend_message_panel(
            frame,
            area,
            &title,
            "Estimate unavailable: weekly calibration is incomplete",
            theme,
            true,
        );
        return None;
    }
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "~EST",
            points: &data.weekly_estimated,
            readout: data.weekly_estimated_readout,
            color: theme.palette().warning,
        }],
        TrendPanelSpec {
            panel: TrendPanelId::WeeklyEstimated,
            title: base_title,
            graph_kind: TrendGraphKind::Line {
                maximum_gap: ChronoDuration::minutes(45),
            },
            value_kind: TrendValueKind::Percent,
            fixed_y_bounds: Some([0.0, 100.0]),
            fixed_x_bounds: None,
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            readout_label: Some("As of"),
            theme,
        },
        inspect_mode,
        inspection,
    )
}

fn render_half_hour_token_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
    inspect_mode: bool,
    inspection: &mut Option<TrendInspection>,
) -> Option<TrendChartHitbox> {
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "Tokens",
            points: &data.half_hour_tokens,
            readout: None,
            color: theme.palette().accent,
        }],
        TrendPanelSpec {
            panel: TrendPanelId::LocalTokens,
            title: "15m Local Tokens",
            graph_kind: TrendGraphKind::Bar {
                expected_step: ChronoDuration::minutes(LOCAL_BUCKET_MINUTES),
            },
            value_kind: TrendValueKind::Tokens,
            fixed_y_bounds: None,
            fixed_x_bounds: Some(data.half_hour_bounds),
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            readout_label: None,
            theme,
        },
        inspect_mode,
        inspection,
    )
}

fn render_half_hour_estimated_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
    inspect_mode: bool,
    inspection: &mut Option<TrendInspection>,
) -> Option<TrendChartHitbox> {
    let base_title = if data.api_long_context_multiplier {
        "15m ~EST Usage · EST Longx ON"
    } else {
        "15m ~EST Usage"
    };
    if data.half_hour_estimated.is_empty() && data.half_hour_history_present {
        if inspection.is_some_and(|inspection| inspection.panel == TrendPanelId::LocalEstimated) {
            *inspection = None;
        }
        let title = trend_panel_status_title(
            base_title,
            data.history_warning_count,
            data.history_read_only,
        );
        render_trend_message_panel(
            frame,
            area,
            &title,
            "Estimate unavailable: weekly calibration is incomplete",
            theme,
            true,
        );
        return None;
    }
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "~EST",
            points: &data.half_hour_estimated,
            readout: None,
            color: theme.palette().warning,
        }],
        TrendPanelSpec {
            panel: TrendPanelId::LocalEstimated,
            title: base_title,
            graph_kind: TrendGraphKind::Bar {
                expected_step: ChronoDuration::minutes(LOCAL_BUCKET_MINUTES),
            },
            value_kind: TrendValueKind::Percent,
            fixed_y_bounds: None,
            fixed_x_bounds: Some(data.half_hour_bounds),
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            readout_label: None,
            theme,
        },
        inspect_mode,
        inspection,
    )
}

fn trend_panel_status_title(base: &str, warning_count: usize, read_only: bool) -> String {
    let mut title = base.to_string();
    if warning_count > 0 {
        title.push_str(&format!(" · PARTIAL {warning_count}"));
    }
    if read_only {
        title.push_str(" · READ-ONLY");
    }
    title
}

fn render_time_series_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[TrendSeries<'_>],
    spec: TrendPanelSpec<'_>,
    inspect_mode: bool,
    inspection: &mut Option<TrendInspection>,
) -> Option<TrendChartHitbox> {
    let nonempty_series = series
        .iter()
        .filter(|series| !series.points.is_empty())
        .count();
    let point_count = series
        .iter()
        .map(|series| series.points.len())
        .sum::<usize>();
    if point_count == 0 {
        let title = trend_panel_status_title(
            spec.title,
            spec.history_warning_count,
            spec.history_read_only,
        );
        render_empty_trend_panel(frame, area, &title, spec.theme);
        if inspection.is_some_and(|inspection| inspection.panel == spec.panel) {
            *inspection = None;
        }
        return None;
    }

    let mut minimum_time = spec.fixed_x_bounds.map(|bounds| bounds[0]);
    let mut maximum_time = spec.fixed_x_bounds.map(|bounds| bounds[1]);
    let mut maximum_value = 0.0_f64;
    let mut partial = false;
    for point in series.iter().flat_map(|series| series.points) {
        minimum_time = Some(minimum_time.map_or(point.at, |value| value.min(point.at)));
        maximum_time = Some(maximum_time.map_or(point.at, |value| value.max(point.at)));
        if point.value.is_finite() {
            maximum_value = maximum_value.max(point.value.max(0.0));
        }
        partial |= point.partial;
    }
    let minimum_time = minimum_time.unwrap_or_else(Utc::now);
    let maximum_time = maximum_time.unwrap_or(minimum_time);
    let mut x_bounds = [
        minimum_time.timestamp() as f64,
        maximum_time.timestamp() as f64,
    ];
    if x_bounds[0] >= x_bounds[1] {
        x_bounds[0] -= 1_800.0;
        x_bounds[1] += 1_800.0;
    }
    let y_bounds = spec.fixed_y_bounds.unwrap_or_else(|| {
        let maximum = if maximum_value <= 0.0 {
            1.0
        } else {
            nice_trend_maximum(maximum_value)
        };
        [0.0, maximum]
    });

    let active_at = if inspect_mode {
        match *inspection {
            Some(current) if current.panel == spec.panel => {
                nearest_inspectable_time(series, current.at, spec.graph_kind)
                    .or_else(|| latest_inspectable_time(series))
            }
            None => latest_inspectable_time(series),
            Some(_) => None,
        }
    } else {
        None
    };
    if let Some(at) = active_at {
        *inspection = Some(TrendInspection {
            panel: spec.panel,
            at,
        });
    }
    let selected_points = series
        .iter()
        .map(|trend_series| {
            active_at
                .and_then(|at| nearest_inspectable_point(trend_series.points, at, spec.graph_kind))
        })
        .collect::<Vec<_>>();
    let displayed_series = series
        .iter()
        .zip(&selected_points)
        .map(|(trend_series, selected)| TrendSeries {
            name: trend_series.name,
            points: trend_series.points,
            readout: if active_at.is_some() {
                selected.and_then(|point| point.readout())
            } else {
                trend_series.readout
            },
            color: trend_series.color,
        })
        .collect::<Vec<_>>();

    let mut prepared = Vec::with_capacity(series.len());
    let mut gap_count = 0_usize;
    for series in series {
        let (segments, gaps) = prepare_trend_segments(series.points, spec.graph_kind);
        prepared.push(segments);
        gap_count = gap_count.saturating_add(gaps);
    }
    let vertical_guide = active_at.map(|at| {
        vec![
            (at.timestamp() as f64, y_bounds[0]),
            (at.timestamp() as f64, y_bounds[1]),
        ]
    });
    let horizontal_guide = (nonempty_series == 1)
        .then(|| selected_points.iter().flatten().next().copied())
        .flatten()
        .map(|point| {
            let value = point.value.clamp(y_bounds[0], y_bounds[1]);
            vec![(x_bounds[0], value), (x_bounds[1], value)]
        });
    let selected_markers = selected_points
        .iter()
        .map(|point| {
            point
                .map(|point| vec![(point.at.timestamp() as f64, point.value.max(0.0))])
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let mut datasets = Vec::new();
    let palette = spec.theme.palette();
    if let Some(guide) = vertical_guide.as_deref() {
        datasets.push(
            Dataset::default()
                .data(guide)
                .graph_type(GraphType::Line)
                .marker(Marker::Braille)
                .style(
                    Style::default()
                        .fg(palette.border)
                        .add_modifier(Modifier::BOLD),
                ),
        );
    }
    if let Some(guide) = horizontal_guide.as_deref() {
        datasets.push(
            Dataset::default()
                .data(guide)
                .graph_type(GraphType::Line)
                .marker(Marker::Braille)
                .style(Style::default().fg(palette.border)),
        );
    }
    for (series, segments) in series.iter().zip(&prepared) {
        for (segment_index, segment) in segments.iter().enumerate() {
            let graph_type = match spec.graph_kind {
                TrendGraphKind::Line { .. } if segment.len() > 1 => GraphType::Line,
                TrendGraphKind::Line { .. } => GraphType::Scatter,
                TrendGraphKind::Bar { .. } => GraphType::Bar,
            };
            let marker = match spec.graph_kind {
                TrendGraphKind::Line { .. } => Marker::Braille,
                // Braille provides two horizontal plot positions per terminal
                // cell, keeping a full day of 96 quarter-hour bars distinct in
                // the common half-width Trends layout.
                TrendGraphKind::Bar { .. } => Marker::Braille,
            };
            let mut dataset = Dataset::default()
                .data(segment)
                .graph_type(graph_type)
                .marker(marker)
                .style(Style::default().fg(series.color));
            if nonempty_series > 1 && segment_index == 0 {
                dataset = dataset.name(series.name);
            }
            datasets.push(dataset);
        }
    }
    for ((series, selected), marker) in series.iter().zip(&selected_points).zip(&selected_markers) {
        if selected.is_none() {
            continue;
        }
        datasets.push(
            Dataset::default()
                .data(marker)
                .graph_type(GraphType::Scatter)
                .marker(Marker::Block)
                .style(
                    Style::default()
                        .fg(series.color)
                        .add_modifier(Modifier::BOLD),
                ),
        );
    }

    let mut panel_title = format!(
        "{} · {}",
        trend_panel_status_title(
            spec.title,
            spec.history_warning_count,
            spec.history_read_only,
        ),
        if point_count == 1 {
            "1 sample".to_string()
        } else {
            format!("{point_count} samples")
        }
    );
    if gap_count > 0 {
        panel_title.push_str(&format!(" · {gap_count} gaps"));
    }
    if partial && spec.history_warning_count == 0 {
        panel_title.push_str(" · PARTIAL");
    }

    let panel_block = panel(&panel_title, spec.theme);
    let inner = panel_block.inner(area);
    frame.render_widget(panel_block, area);
    if inner.is_empty() {
        return None;
    }
    let chart_area = spec.readout_label.map_or(inner, |readout_label| {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        frame.render_widget(
            Paragraph::new(trend_readout_line(
                &displayed_series,
                if active_at.is_some() {
                    "Inspect"
                } else {
                    readout_label
                },
                rows[0].width,
                spec.theme,
            ))
            .style(spec.theme.base_style())
            .alignment(Alignment::Right),
            rows[0],
        );
        rows[1]
    });
    if chart_area.is_empty() {
        return None;
    }

    let x_labels = trend_time_axis_labels(minimum_time, maximum_time, chart_area.width);
    let y_labels = vec![
        format_trend_axis_value(y_bounds[0], spec.value_kind),
        format_trend_axis_value(y_bounds[1], spec.value_kind),
    ];
    let legend_names = if nonempty_series > 1 {
        series
            .iter()
            .filter(|series| !series.points.is_empty())
            .map(|series| series.name)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let geometry = trend_chart_geometry(chart_area, &x_labels, &y_labels, legend_names.as_slice());
    let chart = Chart::new(datasets)
        .style(spec.theme.base_style())
        .x_axis(
            Axis::default()
                .bounds(x_bounds)
                .labels(x_labels)
                .style(Style::default().fg(palette.muted)),
        )
        .y_axis(
            Axis::default()
                .bounds(y_bounds)
                .labels(y_labels)
                .style(Style::default().fg(palette.muted)),
        )
        .legend_position(
            (nonempty_series > 1).then_some(ratatui::widgets::LegendPosition::TopRight),
        );
    frame.render_widget(chart, chart_area);
    let geometry = geometry?;
    if active_at.is_some() && spec.readout_label.is_none() {
        let overlay_right = geometry
            .legend
            .filter(|legend| legend.y == geometry.plot.y)
            .map_or(geometry.plot.right(), |legend| legend.x);
        let overlay_area = Rect::new(
            geometry.plot.x,
            geometry.plot.y,
            overlay_right.saturating_sub(geometry.plot.x),
            u16::from(geometry.plot.height > 0),
        );
        if !overlay_area.is_empty() {
            frame.render_widget(Clear, overlay_area);
            frame.render_widget(
                Paragraph::new(trend_readout_line(
                    &displayed_series,
                    "Inspect",
                    overlay_area.width,
                    spec.theme,
                ))
                .style(spec.theme.base_style()),
                overlay_area,
            );
        }
    }

    let mut inspectable_times = series
        .iter()
        .flat_map(|series| series.points.iter())
        .filter(|point| point.sampled_at.is_some() && point.value.is_finite())
        .map(|point| point.at)
        .collect::<Vec<_>>();
    inspectable_times.sort_unstable();
    inspectable_times.dedup();
    Some(TrendChartHitbox {
        panel: spec.panel,
        plot: geometry.plot,
        legend: geometry.legend,
        x_bounds,
        graph_kind: spec.graph_kind,
        inspectable_times,
    })
}

fn latest_inspectable_time(series: &[TrendSeries<'_>]) -> Option<DateTime<Utc>> {
    series
        .iter()
        .flat_map(|series| series.points.iter())
        .filter(|point| point.sampled_at.is_some() && point.value.is_finite())
        .map(|point| point.at)
        .max()
}

fn nearest_inspectable_time(
    series: &[TrendSeries<'_>],
    at: DateTime<Utc>,
    graph_kind: TrendGraphKind,
) -> Option<DateTime<Utc>> {
    let tolerance_ms = graph_kind
        .selection_tolerance()
        .num_milliseconds()
        .unsigned_abs();
    series
        .iter()
        .flat_map(|series| series.points.iter())
        .filter(|point| point.sampled_at.is_some() && point.value.is_finite())
        .map(|point| ((point.at - at).num_milliseconds().unsigned_abs(), point.at))
        .filter(|(distance, _)| *distance <= tolerance_ms)
        .min_by_key(|(distance, point_at)| (*distance, *point_at))
        .map(|(_, point_at)| point_at)
}

fn nearest_inspectable_point(
    points: &[TrendPoint],
    at: DateTime<Utc>,
    graph_kind: TrendGraphKind,
) -> Option<&TrendPoint> {
    let tolerance_ms = graph_kind
        .selection_tolerance()
        .num_milliseconds()
        .unsigned_abs();
    points
        .iter()
        .filter(|point| point.sampled_at.is_some() && point.value.is_finite())
        .map(|point| ((point.at - at).num_milliseconds().unsigned_abs(), point))
        .filter(|(distance, _)| *distance <= tolerance_ms)
        .min_by_key(|(distance, point)| (*distance, point.at))
        .map(|(_, point)| point)
}

fn trend_chart_geometry(
    area: Rect,
    x_labels: &[String],
    y_labels: &[String],
    legend_names: &[&str],
) -> Option<TrendChartGeometry> {
    if area.is_empty() {
        return None;
    }

    let mut x = area.left();
    let mut y = area.bottom() - 1;
    if !x_labels.is_empty() && y > area.top() {
        y -= 1;
    }

    let has_y_axis = !y_labels.is_empty();
    let mut left_label_width = y_labels
        .iter()
        .map(|label| UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or_default();
    if let Some(first_x_label) = x_labels.first() {
        left_label_width = left_label_width.max(
            UnicodeWidthStr::width(first_x_label.as_str()).saturating_sub(usize::from(has_y_axis)),
        );
    }
    let left_label_width = u16::try_from(left_label_width)
        .unwrap_or(u16::MAX)
        .min(area.width / 3);
    x = x.saturating_add(left_label_width);
    if !x_labels.is_empty() && y > area.top() {
        y -= 1;
    }
    if has_y_axis && x + 1 < area.right() {
        x += 1;
    }

    let plot = Rect::new(
        x,
        area.top(),
        area.right().saturating_sub(x),
        y.saturating_sub(area.top()).saturating_add(1),
    );
    if plot.is_empty() {
        return None;
    }

    let legend = if legend_names.is_empty() {
        None
    } else {
        let inner_width = legend_names
            .iter()
            .map(|name| UnicodeWidthStr::width(*name))
            .max()
            .unwrap_or_default();
        let legend_width = u16::try_from(inner_width.saturating_add(2)).unwrap_or(u16::MAX);
        let legend_height = u16::try_from(legend_names.len().saturating_add(2)).unwrap_or(u16::MAX);
        let maximum_width = Layout::horizontal([Constraint::Ratio(1, 4)])
            .flex(Flex::Start)
            .split(plot)[0]
            .width;
        let maximum_height = Layout::vertical([Constraint::Ratio(1, 4)])
            .flex(Flex::Start)
            .split(plot)[0]
            .height;
        (inner_width > 0
            && legend_width <= maximum_width
            && legend_height <= maximum_height
            && legend_width <= plot.width
            && legend_height <= plot.height)
            .then(|| {
                Rect::new(
                    plot.right() - legend_width,
                    plot.top(),
                    legend_width,
                    legend_height,
                )
            })
    };
    Some(TrendChartGeometry { plot, legend })
}

fn trend_readout_line(
    series: &[TrendSeries<'_>],
    label: &str,
    width: u16,
    theme: Theme,
) -> Line<'static> {
    let maximum_width = usize::from(width);
    if maximum_width == 0 {
        return Line::default();
    }
    if series.iter().all(|series| series.readout.is_none()) {
        for message in [
            "Current sample unavailable".to_string(),
            format!("{label}: unavailable"),
            "Unavailable".to_string(),
        ] {
            if UnicodeWidthStr::width(message.as_str()) <= maximum_width {
                return Line::styled(message, Style::default().fg(theme.palette().muted));
            }
        }
        return Line::styled("…", Style::default().fg(theme.palette().muted));
    }

    let candidates = [
        (Some(label), "%m-%d %H:%M:%S", false, true),
        (Some(label), "%H:%M:%S", false, true),
        (None, "%H:%M:%S", false, false),
        (None, "%H:%M:%S", true, false),
        (None, "%H:%M", true, false),
    ];
    for (prefix, time_format, compact_names, spaced) in candidates {
        let (line, line_width) =
            build_trend_readout_line(series, prefix, time_format, compact_names, spaced, theme);
        if line_width <= maximum_width {
            return line;
        }
    }

    Line::styled("…", Style::default().fg(theme.palette().muted))
}

fn build_trend_readout_line(
    series: &[TrendSeries<'_>],
    label: Option<&str>,
    time_format: &str,
    compact_names: bool,
    spaced: bool,
    theme: Theme,
) -> (Line<'static>, usize) {
    let palette = theme.palette();
    let mut spans = Vec::new();
    let mut width = 0_usize;
    if let Some(label) = label {
        push_trend_readout_span(
            &mut spans,
            &mut width,
            format!("{label} · "),
            Style::default().fg(palette.muted),
        );
    }

    for (index, trend_series) in series.iter().enumerate() {
        if index > 0 {
            push_trend_readout_span(
                &mut spans,
                &mut width,
                if spaced { " · " } else { " " }.to_string(),
                Style::default().fg(palette.muted),
            );
        }
        let name = if compact_names {
            match trend_series.name {
                "Week" => "W",
                "Tokens" => "Tok",
                name => name,
            }
        } else {
            trend_series.name
        };
        let Some(readout) = trend_series.readout else {
            push_trend_readout_span(
                &mut spans,
                &mut width,
                format!("{name} —"),
                Style::default().fg(palette.muted),
            );
            continue;
        };
        let separator = if spaced { " @ " } else { "@" };
        push_trend_readout_span(
            &mut spans,
            &mut width,
            format!("{name} {}", format_trend_readout_value(readout.value)),
            Style::default()
                .fg(trend_series.color)
                .add_modifier(Modifier::BOLD),
        );
        push_trend_readout_span(
            &mut spans,
            &mut width,
            format!(
                "{separator}{}",
                format_trend_readout_time(readout, time_format)
            ),
            Style::default().fg(if readout.partial {
                palette.warning
            } else {
                palette.muted
            }),
        );
    }

    (Line::from(spans), width)
}

fn format_trend_readout_time(readout: TrendReadout, time_format: &str) -> String {
    let Some(interval) = readout.interval else {
        return format_local_time(readout.sampled_at, time_format);
    };
    let include_date = time_format.contains("%m-%d");
    let starts_at = format_local_time(
        interval.starts_at,
        if include_date { "%m-%d %H:%M" } else { "%H:%M" },
    );
    let crosses_local_date = format_local_time(interval.starts_at, "%Y-%m-%d")
        != format_local_time(interval.ends_at, "%Y-%m-%d");
    let ends_at = format_local_time(
        interval.ends_at,
        if include_date && crosses_local_date {
            "%m-%d %H:%M"
        } else {
            "%H:%M"
        },
    );
    format!("{starts_at}–{ends_at}")
}

fn push_trend_readout_span(
    spans: &mut Vec<Span<'static>>,
    width: &mut usize,
    text: String,
    style: Style,
) {
    *width = width.saturating_add(UnicodeWidthStr::width(text.as_str()));
    spans.push(Span::styled(text, style));
}

fn format_trend_readout_value(value: TrendReadoutValue) -> String {
    match value {
        TrendReadoutValue::Percent(value) => {
            if !value.is_finite() {
                return "—".to_string();
            }
            let mut value = format!("{value:.2}");
            while value.ends_with('0') {
                value.pop();
            }
            if value.ends_with('.') {
                value.pop();
            }
            format!("{value}%")
        }
        TrendReadoutValue::Tokens(value) => format_exact_token_count(value),
    }
}

fn format_exact_token_count(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len().saturating_sub(1) / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

fn prepare_trend_segments(
    points: &[TrendPoint],
    graph_kind: TrendGraphKind,
) -> (Vec<Vec<(f64, f64)>>, usize) {
    if points.is_empty() {
        return (Vec::new(), 0);
    }
    if matches!(graph_kind, TrendGraphKind::Bar { .. }) {
        let data = points
            .iter()
            .filter(|point| point.value.is_finite())
            .map(|point| (point.at.timestamp() as f64, point.value.max(0.0)))
            .collect::<Vec<_>>();
        let expected_step = match graph_kind {
            TrendGraphKind::Bar { expected_step } => expected_step.num_seconds().max(1),
            TrendGraphKind::Line { .. } => 1,
        };
        let gaps = points
            .windows(2)
            .map(|pair| {
                let elapsed = (pair[1].at - pair[0].at).num_seconds().max(0);
                usize::try_from(elapsed / expected_step)
                    .unwrap_or(usize::MAX)
                    .saturating_sub(1)
            })
            .sum();
        return (
            (data.is_empty())
                .then(Vec::new)
                .unwrap_or_else(|| vec![data]),
            gaps,
        );
    }

    let maximum_gap = match graph_kind {
        TrendGraphKind::Line { maximum_gap } => maximum_gap,
        TrendGraphKind::Bar { .. } => chrono::Duration::zero(),
    };
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut gaps = 0_usize;
    let mut previous = None;
    for point in points.iter().filter(|point| point.value.is_finite()) {
        if previous.is_some_and(|previous| point.at - previous > maximum_gap) {
            if !segment.is_empty() {
                segments.push(std::mem::take(&mut segment));
            }
            gaps = gaps.saturating_add(1);
        }
        segment.push((point.at.timestamp() as f64, point.value.max(0.0)));
        previous = Some(point.at);
    }
    if !segment.is_empty() {
        segments.push(segment);
    }
    (segments, gaps)
}

fn nice_trend_maximum(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let exponent = value.log10().floor();
    let magnitude = 10_f64.powf(exponent);
    let normalized = value / magnitude;
    let rounded = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    rounded * magnitude
}

fn trend_time_axis_labels(start: DateTime<Utc>, end: DateTime<Utc>, width: u16) -> Vec<String> {
    let format = if width < 50 { "%H:%M" } else { "%m-%d %H:%M" };
    if width < 70 {
        vec![
            format_local_time(start, format),
            format_local_time(end, format),
        ]
    } else {
        let midpoint = start + (end - start) / 2;
        vec![
            format_local_time(start, format),
            format_local_time(midpoint, format),
            format_local_time(end, format),
        ]
    }
}

fn format_trend_axis_value(value: f64, kind: TrendValueKind) -> String {
    match kind {
        TrendValueKind::Percent => format!("{value:.0}%"),
        TrendValueKind::Tokens => {
            if value >= 1_000_000_000.0 {
                format!("{:.1}B", value / 1_000_000_000.0)
            } else if value >= 1_000_000.0 {
                format!("{:.1}M", value / 1_000_000.0)
            } else if value >= 1_000.0 {
                format!("{:.0}K", value / 1_000.0)
            } else {
                format!("{value:.0}")
            }
        }
    }
}

fn reset_expiry_gauge_alert_lines(reminder: ResetExpiryReminder, width: u16) -> Vec<String> {
    let expires_at = local_full_time_label(Some(reminder.expires_at), "unavailable");
    let full = format!("! RESET CREDIT EXPIRES {expires_at}");
    let compact = format!("! EXP {expires_at}");
    let minimal = format!("! {expires_at}");
    let max_width = usize::from(width.max(1));

    for candidate in [full, compact, minimal] {
        if UnicodeWidthStr::width(candidate.as_str()) <= max_width {
            return vec![candidate];
        }
    }

    let date = format_local_time(reminder.expires_at, "%Y-%m-%d");
    let time = format_local_time(reminder.expires_at, "%H:%M:%S %:z");
    let date_line = format!("! EXP {date}");
    if UnicodeWidthStr::width(date_line.as_str()) <= max_width
        && UnicodeWidthStr::width(time.as_str()) <= max_width
    {
        return vec![date_line, time];
    }

    split_exact_display_lines(&format!("! {expires_at}"), width)
}

fn split_exact_display_lines(value: &str, width: u16) -> Vec<String> {
    let max_width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0_usize;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if !line.is_empty() && line_width.saturating_add(character_width) > max_width {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        line.push(character);
        line_width = line_width.saturating_add(character_width);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn is_reset_expiry_gauge(
    bucket: &crate::domain::LimitBucket,
    window: &crate::domain::LimitWindow,
    reminder: ResetExpiryReminder,
) -> bool {
    bucket.limit_id.trim().eq_ignore_ascii_case("codex")
        && window.window_duration_mins == Some(WindowScope::Week.duration_mins())
        && window.resets_at == Some(reminder.weekly_reset_at)
}

fn ordered_quota_windows(
    snapshot: &Snapshot,
) -> Vec<(&crate::domain::LimitBucket, &crate::domain::LimitWindow)> {
    let mut windows = snapshot
        .limits
        .iter()
        .flat_map(|bucket| {
            [bucket.primary.as_ref(), bucket.secondary.as_ref()]
                .into_iter()
                .flatten()
                .map(move |window| (bucket, window))
        })
        .collect::<Vec<_>>();
    windows.sort_by_key(|(bucket, _)| !bucket.limit_id.trim().eq_ignore_ascii_case("codex"));
    windows
}

fn reset_expiry_gauge_inner_width(
    snapshot: &Snapshot,
    area_width: u16,
    reminder: ResetExpiryReminder,
) -> Option<u16> {
    let windows = ordered_quota_windows(snapshot);
    let target = windows
        .iter()
        .position(|(bucket, window)| is_reset_expiry_gauge(bucket, window, reminder))?;
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            windows
                .iter()
                .map(|_| Constraint::Ratio(1, windows.len() as u32))
                .collect::<Vec<_>>(),
        )
        .split(Rect::new(0, 0, area_width, 1));
    Some(columns[target].width.saturating_sub(2))
}

fn overview_quota_height(snapshot: &Snapshot, area_width: u16, base_height: u16) -> u16 {
    reset_expiry_reminder(snapshot)
        .and_then(|reminder| {
            reset_expiry_gauge_inner_width(snapshot, area_width, reminder).map(|inner_width| {
                let alert_height =
                    u16::try_from(reset_expiry_gauge_alert_lines(reminder, inner_width).len())
                        .unwrap_or(u16::MAX);
                base_height.max(alert_height.saturating_mul(2).saturating_add(3))
            })
        })
        .unwrap_or(base_height)
}

fn app_server_call_failed(snapshot: &Snapshot) -> bool {
    snapshot
        .warnings
        .iter()
        .any(|value| value.starts_with("app-server refresh failed:"))
        || snapshot.sources.iter().any(|source| {
            source.source == "app_server"
                && (source.status == "error"
                    || (source.status == "stale"
                        && source.message.as_deref() != Some("no cached account snapshot")))
        })
}

fn app_server_failure_message(width: u16) -> &'static str {
    if width >= 59 {
        "Unable to call codex app-server · try installing Codex CLI"
    } else if width >= 37 {
        "codex app-server failed · install CLI"
    } else {
        "app-server failed · CLI"
    }
}

fn render_app_server_failure_notice(frame: &mut Frame<'_>, area: Rect, theme: Theme) {
    frame.render_widget(
        Paragraph::new(app_server_failure_message(area.width))
            .style(
                Style::default()
                    .fg(theme.palette().warning)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center),
        area,
    );
}

fn render_health(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let palette = app.theme.palette();
    let source_height = u16::try_from(app.snapshot.sources.len())
        .unwrap_or(u16::MAX)
        .saturating_add(3)
        .clamp(3, 7);
    let desired_reset_height = u16::try_from(reset_window_count(&app.snapshot))
        .unwrap_or(u16::MAX)
        .saturating_add(
            app.snapshot
                .rate_limit_reset_credits
                .as_ref()
                .and_then(|credits| credits.credits.as_ref())
                .map_or(0, Vec::len)
                .try_into()
                .unwrap_or(u16::MAX),
        )
        .saturating_add(3)
        .max(3);
    let available_reset_height = area
        .height
        .saturating_sub(source_height.saturating_add(8))
        .max(3);
    let reset_height = desired_reset_height.min(available_reset_height);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(source_height),
            Constraint::Length(5),
            Constraint::Length(reset_height),
            Constraint::Min(3),
        ])
        .split(area);

    let source_rows = app
        .snapshot
        .sources
        .iter()
        .map(|source| {
            Row::new([
                Cell::from(terminal_safe_text(&source.source)),
                Cell::from(terminal_safe_text(&source.status)),
                Cell::from(format_local_time(source.as_of, "%H:%M:%S")),
                Cell::from(terminal_safe_text(
                    source.message.as_deref().unwrap_or_default(),
                )),
            ])
        })
        .collect::<Vec<_>>();
    let source_table = Table::new(
        source_rows,
        [
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(10),
        ],
    )
    .header(table_header(
        ["SOURCE", "STATE", "AS OF", "DETAIL"],
        app.theme,
    ))
    .block(panel("Sources", app.theme));
    frame.render_widget(source_table, rows[0]);

    let stats = &app.snapshot.stats;
    let status_counts = count_statuses(&app.snapshot.tasks);
    let stats_text = vec![
        Line::from(format!(
            "Files  {}/{} scanned ({} truncated, {} unreadable)    Lines  {} parsed / {} skipped    Token counters  {} ambiguous resets",
            stats.scanned_files,
            stats.discovered_files,
            stats.truncated_files,
            stats.unreadable_files,
            stats.parsed_lines,
            stats.skipped_lines,
            stats.ambiguous_token_resets
        )),
        Line::from(format!(
            "Tasks  {} running    {} completed    {} stale/unknown",
            status_counts.0, status_counts.1, status_counts.2
        )),
        Line::from(format!(
            "Snapshot  {}    Schema v{}    Partial {}",
            app.snapshot.as_of.format("%Y-%m-%d %H:%M:%S UTC"),
            app.snapshot.schema_version,
            app.snapshot.partial
        )),
    ];
    let collection_title = format!("Collection · recorder {}", recorder_panel_status(app));
    frame.render_widget(
        Paragraph::new(stats_text).block(panel(&collection_title, app.theme)),
        rows[1],
    );

    render_resets(
        frame,
        rows[2],
        &app.snapshot,
        app.reset_credit_fetch_status(Instant::now()),
        app.theme,
    );

    let issues = app
        .snapshot
        .errors
        .iter()
        .map(|value| {
            Line::from(Span::styled(
                terminal_safe_text(value),
                Style::default().fg(palette.error),
            ))
        })
        .chain(app.snapshot.warnings.iter().map(|value| {
            Line::from(Span::styled(
                terminal_safe_text(value),
                Style::default().fg(palette.warning),
            ))
        }))
        .chain(app.history.warnings.iter().map(|value| {
            Line::from(Span::styled(
                format!("history: {}", terminal_safe_text(value)),
                Style::default().fg(palette.warning),
            ))
        }))
        .chain(app.recorder_health.error.iter().map(|value| {
            Line::from(Span::styled(
                format!("recorder: {}", terminal_safe_text(value)),
                Style::default().fg(palette.warning),
            ))
        }))
        .collect::<Vec<_>>();
    let issues = if issues.is_empty() {
        vec![Line::from(Span::styled(
            "No collection issues",
            Style::default().fg(palette.success),
        ))]
    } else {
        issues
    };
    frame.render_widget(
        Paragraph::new(issues)
            .block(panel("Diagnostics", app.theme))
            .wrap(Wrap { trim: true }),
        rows[3],
    );
}

fn recorder_panel_status(app: &App) -> String {
    recorder_panel_status_at(app, Utc::now())
}

fn recorder_panel_status_at(app: &App, now: DateTime<Utc>) -> String {
    if app.recorder_health.error.is_some() {
        return "error".to_string();
    }
    let Some(status) = app.recorder_health.status.as_ref() else {
        return "idle".to_string();
    };
    let state = if status.last_error.is_some() {
        "error"
    } else if status.heartbeat_is_recent(now) {
        "running"
    } else {
        "stale"
    };
    format!(
        "{state} {}",
        format_local_time(status.last_attempt_at, "%H:%M:%S")
    )
}

fn reset_window_count(snapshot: &Snapshot) -> usize {
    snapshot
        .limits
        .iter()
        .map(|bucket| {
            usize::from(bucket.primary.is_some()) + usize::from(bucket.secondary.is_some())
        })
        .sum()
}

fn local_full_time_label(value: Option<chrono::DateTime<chrono::Utc>>, missing: &str) -> String {
    value
        .map(|value| format_local_time(value, "%Y-%m-%d %H:%M:%S %:z"))
        .unwrap_or_else(|| missing.to_string())
}

fn local_granted_time_label(value: chrono::DateTime<chrono::Utc>, compact: bool) -> String {
    format_local_time(
        value,
        if compact {
            "%m-%d %H:%M"
        } else {
            "%Y-%m-%d %H:%M:%S %:z"
        },
    )
}

fn render_resets(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &Snapshot,
    credit_fetch_status: Option<&str>,
    theme: Theme,
) {
    let palette = theme.palette();
    let windows = snapshot
        .limits
        .iter()
        .flat_map(|bucket| {
            [
                ("primary", "P", bucket.primary.as_ref()),
                ("secondary", "S", bucket.secondary.as_ref()),
            ]
            .into_iter()
            .filter_map(move |(slot, compact_slot, window)| {
                window.map(|window| (bucket, slot, compact_slot, window))
            })
        })
        .collect::<Vec<_>>();
    let reset_credit_details = snapshot
        .rate_limit_reset_credits
        .as_ref()
        .and_then(|credits| credits.credits.as_deref())
        .unwrap_or_default();
    let row_capacity = usize::from(area.height.saturating_sub(3));
    let mut visible_credit_count = if reset_credit_details.is_empty() || windows.is_empty() {
        reset_credit_details.len().min(row_capacity)
    } else {
        reset_credit_details
            .len()
            .min(row_capacity.saturating_add(1) / 2)
    };
    let mut visible_window_count = windows
        .len()
        .min(row_capacity.saturating_sub(visible_credit_count));
    let remaining = row_capacity
        .saturating_sub(visible_credit_count)
        .saturating_sub(visible_window_count);
    visible_credit_count = visible_credit_count.saturating_add(
        reset_credit_details
            .len()
            .saturating_sub(visible_credit_count)
            .min(remaining),
    );
    visible_window_count = visible_window_count.saturating_add(
        windows.len().saturating_sub(visible_window_count).min(
            row_capacity
                .saturating_sub(visible_credit_count)
                .saturating_sub(visible_window_count),
        ),
    );

    let mut title = match &snapshot.rate_limit_reset_credits {
        Some(reset_credits) => {
            let mut title = format!(
                "Resets · {} available · {}",
                reset_credits.available_count,
                provenance_label(reset_credits.provenance)
            );
            if let Some(status) = credit_fetch_status {
                title.push_str(&format!(" · {}", status.to_ascii_uppercase()));
            }
            match &reset_credits.credits {
                None => title.push_str(" · DETAILS UNAVAILABLE"),
                Some(details) => {
                    let returned_count = u64::try_from(details.len()).unwrap_or(u64::MAX);
                    if returned_count < reset_credits.available_count {
                        title.push_str(&format!(
                            " · DETAILS {returned_count}/{}",
                            reset_credits.available_count
                        ));
                    }
                    if visible_credit_count < details.len() {
                        title.push_str(&format!(
                            " · SHOWING {visible_credit_count}/{}",
                            details.len()
                        ));
                    }
                }
            }
            title
        }
        None => credit_fetch_status.map_or_else(
            || "Resets · credits unavailable".to_string(),
            |status| format!("Resets · credits {status}"),
        ),
    };
    if visible_window_count < windows.len() {
        title.push_str(&format!(
            " · WINDOWS {visible_window_count}/{}",
            windows.len()
        ));
    }
    if snapshot.rate_limit_reset_credits_partial {
        title.push_str(" · PARTIAL");
    }
    let block = panel(&title, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    if windows.is_empty() && reset_credit_details.is_empty() {
        frame.render_widget(
            Paragraph::new("No reset-window data").style(Style::default().fg(palette.muted)),
            inner,
        );
        return;
    }

    let compact = area.width < 80;
    let mut rows = reset_credit_details
        .iter()
        .take(visible_credit_count)
        .map(|credit| {
            let item = terminal_safe_text(
                credit
                    .title
                    .as_deref()
                    .unwrap_or(credit.reset_type.as_str()),
            );
            let status = terminal_safe_text(&credit.status);
            Row::new(vec![
                Cell::from(item),
                Cell::from(status),
                Cell::from(local_granted_time_label(credit.granted_at, compact)),
                Cell::from(local_full_time_label(credit.expires_at, "never")),
            ])
        })
        .collect::<Vec<_>>();
    rows.extend(windows.into_iter().take(visible_window_count).map(
        |(bucket, slot, compact_slot, window)| {
            let limit_id = terminal_safe_text(&bucket.limit_id);
            let window_label = window.label();
            let reset_time = local_full_time_label(window.resets_at, "unavailable");
            if compact {
                Row::new(vec![
                    Cell::from(limit_id),
                    Cell::from(format!("{compact_slot}/{window_label}")),
                    Cell::from("-"),
                    Cell::from(reset_time),
                ])
            } else {
                Row::new(vec![
                    Cell::from(limit_id),
                    Cell::from(format!("{slot} {window_label} {:.0}%", window.used_percent)),
                    Cell::from("-"),
                    Cell::from(reset_time),
                ])
            }
        },
    ));
    let (constraints, header) = if compact {
        (
            vec![
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(11),
                Constraint::Length(26),
            ],
            table_header(["ITEM", "STATE", "GRANTED", "RESET TIME (LOCAL)"], theme),
        )
    } else {
        (
            vec![
                Constraint::Min(11),
                Constraint::Length(9),
                Constraint::Length(26),
                Constraint::Length(26),
            ],
            table_header(
                ["ITEM", "STATE", "GRANTED (LOCAL)", "RESET TIME (LOCAL)"],
                theme,
            ),
        )
    };
    frame.render_widget(
        Table::new(rows, constraints)
            .column_spacing(1)
            .header(header),
        inner,
    );
}

fn render_limits(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, theme: Theme) {
    let palette = theme.palette();
    let reset_reminder = reset_expiry_reminder(snapshot);
    let mut reset_reminder_rendered = false;
    let windows = ordered_quota_windows(snapshot);

    if windows.is_empty() {
        frame.render_widget(
            Paragraph::new("Quota unavailable")
                .alignment(Alignment::Center)
                .block(panel("Quota", theme)),
            area,
        );
        return;
    }

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            windows
                .iter()
                .map(|_| Constraint::Ratio(1, windows.len() as u32))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (index, (bucket, window)) in windows.iter().enumerate() {
        let reset = window
            .resets_at
            .map(|value| format_local_time(value, "%m-%d %H:%M"))
            .unwrap_or_else(|| "unknown".to_string());
        let reset_time = window
            .resets_at
            .map(|value| format_local_time(value, "%H:%M"))
            .unwrap_or_else(|| "?".to_string());
        let color = quota_color(window.used_percent, theme);
        let title = format!(
            "{} · {} · {}",
            window.label(),
            terminal_safe_text(&bucket.limit_id),
            provenance_label(bucket.provenance)
        );
        let width = columns[index].width.saturating_sub(2);
        let label = if width >= 52 {
            format!(
                "{:.0}% used | {:.0}% left | reset {reset}",
                window.used_percent, window.remaining_percent
            )
        } else if width >= 28 {
            format!(
                "{:.0}%/{:.0}% | {reset}",
                window.used_percent, window.remaining_percent
            )
        } else {
            format!(
                "{:.0}/{:.0}% {reset_time}",
                window.used_percent, window.remaining_percent
            )
        };
        let reminder = reset_reminder.filter(|reminder| {
            !reset_reminder_rendered && is_reset_expiry_gauge(bucket, window, *reminder)
        });
        if reminder.is_some() {
            reset_reminder_rendered = true;
        }
        let ratio = (window.used_percent / 100.0).clamp(0.0, 1.0);
        let block = panel(&title, theme);
        let inner = block.inner(columns[index]);
        let gauge = Gauge::default()
            .block(block)
            .gauge_style(Style::default().fg(color).bg(palette.gauge_track))
            .ratio(ratio)
            .label(if reminder.is_some() { "" } else { &label });
        frame.render_widget(gauge, columns[index]);
        if let Some(reminder) = reminder {
            render_reset_expiry_gauge_label(frame, inner, &label, reminder, theme, color, ratio);
        }
    }
}

fn render_reset_expiry_gauge_label(
    frame: &mut Frame<'_>,
    area: Rect,
    usage_label: &str,
    reminder: ResetExpiryReminder,
    theme: Theme,
    gauge_color: Color,
    ratio: f64,
) {
    if area.is_empty() {
        return;
    }
    let palette = theme.palette();
    let alert_lines = reset_expiry_gauge_alert_lines(reminder, area.width);
    let alert_height = u16::try_from(alert_lines.len()).unwrap_or(u16::MAX);
    let centered_usage_y = area.y.saturating_add(area.height / 2);
    let latest_usage_y = area
        .bottom()
        .saturating_sub(alert_height)
        .saturating_sub(1)
        .max(area.y);
    let usage_y = centered_usage_y.min(latest_usage_y);
    let usage_style = Style::default();
    let warning_style = Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD);
    let covered_text_style = Style::default().fg(palette.gauge_track).bg(gauge_color);
    let filled_end = area.x.saturating_add(
        (f64::from(area.width) * ratio)
            .round()
            .clamp(0.0, f64::from(area.width)) as u16,
    );

    render_centered_gauge_span(
        frame,
        area,
        usage_y,
        usage_label,
        usage_style,
        filled_end,
        covered_text_style,
    );
    for (index, line) in alert_lines.iter().enumerate() {
        let y = usage_y
            .saturating_add(1)
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        render_centered_gauge_span(
            frame,
            area,
            y,
            line,
            warning_style,
            filled_end,
            covered_text_style,
        );
    }
}

fn render_centered_gauge_span(
    frame: &mut Frame<'_>,
    area: Rect,
    y: u16,
    text: &str,
    style: Style,
    filled_end: u16,
    covered_text_style: Style,
) {
    if y >= area.bottom() {
        return;
    }
    let text_width = u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX);
    let visible_width = text_width.min(area.width);
    let x = area
        .x
        .saturating_add(area.width.saturating_sub(visible_width) / 2);
    let buffer = frame.buffer_mut();
    buffer.set_span(x, y, &Span::styled(text, style), visible_width);
    let covered_right = x.saturating_add(visible_width).min(filled_end);
    if covered_right > x {
        buffer.set_style(
            Rect::new(x, y, covered_right.saturating_sub(x), 1),
            covered_text_style,
        );
    }
}

fn configured_table_min_width(
    columns: UiTableColumns,
    structural_widths: &[u16],
    metric_widths: [u16; 4],
) -> u16 {
    let mut widths = structural_widths.to_vec();
    for (visible, width) in [
        (columns.tokens, metric_widths[0]),
        (columns.token_share, metric_widths[1]),
        (columns.estimated_quota, metric_widths[2]),
        (columns.api_equivalent, metric_widths[3]),
    ] {
        if visible {
            widths.push(width);
        }
    }
    widths
        .iter()
        .copied()
        .fold(0_u16, u16::saturating_add)
        .saturating_add(u16::try_from(widths.len().saturating_sub(1)).unwrap_or(u16::MAX))
}

fn fit_table_columns(
    mut columns: UiTableColumns,
    available_width: u16,
    structural_widths: &[u16],
    metric_widths: [u16; 4],
) -> UiTableColumns {
    for hide in [
        SettingItem::TokenShare,
        SettingItem::EstimatedQuota,
        SettingItem::Tokens,
        SettingItem::ApiEquivalent,
    ] {
        if configured_table_min_width(columns, structural_widths, metric_widths) <= available_width
        {
            break;
        }
        match hide {
            SettingItem::Tokens => columns.tokens = false,
            SettingItem::TokenShare => columns.token_share = false,
            SettingItem::EstimatedQuota => columns.estimated_quota = false,
            SettingItem::ApiEquivalent => columns.api_equivalent = false,
            SettingItem::Theme
            | SettingItem::Turns
            | SettingItem::Models
            | SettingItem::ApiLongContext => unreachable!("display settings are not columns"),
        }
    }
    columns
}

fn task_visible_columns(
    columns: UiTableColumns,
    table_inner_width: u16,
    api_cost_width: u16,
) -> UiTableColumns {
    fit_table_columns(
        columns,
        table_inner_width.saturating_sub(TASK_HIGHLIGHT_WIDTH),
        &[24],
        [
            TASK_TOKENS_WIDTH,
            TASK_TOKEN_SHARE_WIDTH,
            TASK_QUOTA_WIDTH,
            api_cost_width,
        ],
    )
}

fn task_table_constraints(columns: UiTableColumns, api_cost_width: u16) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    if columns.tokens {
        constraints.push(Constraint::Length(TASK_TOKENS_WIDTH));
    }
    if columns.token_share {
        constraints.push(Constraint::Length(TASK_TOKEN_SHARE_WIDTH));
    }
    if columns.estimated_quota {
        constraints.push(Constraint::Length(TASK_QUOTA_WIDTH));
    }
    if columns.api_equivalent {
        constraints.push(Constraint::Length(api_cost_width));
    }
    constraints.push(Constraint::Min(24));
    constraints
}

fn render_tasks(frame: &mut Frame<'_>, area: Rect, app: &mut App, window_only: bool) {
    let filtered = app.filtered_task_rows();
    let selected_position = filtered
        .iter()
        .position(|row| row.index == app.selected_task);
    let (block, controls) = task_panel_block(area, app, window_only, filtered.len());
    app.task_controls_hitbox = Some(controls);
    let table_inner = block.inner(area);
    let visible_capacity = usize::from(table_inner.height.saturating_sub(1));
    app.task_table_offset = app
        .task_table_offset
        .min(filtered.len().saturating_sub(visible_capacity));
    if app.task_reveal_pending {
        if let Some(position) = selected_position {
            app.task_table_offset = reveal_offset(
                app.task_table_offset,
                position,
                filtered.len(),
                visible_capacity,
            );
        }
        app.task_reveal_pending = false;
    }
    let offset = app.task_table_offset;
    let selected_in_view = selected_position
        .and_then(|position| position.checked_sub(offset))
        .filter(|index| *index < visible_capacity);
    let theme = app.theme;
    let palette = theme.palette();
    let tasks_focused = app.focus == Focus::Tasks;
    let tree_mode = app.task_list_mode == TaskListMode::Tree;
    let cost_scope = if window_only {
        app.window_scope
    } else {
        WindowScope::FiveHours
    };
    let api_cost_analysis = window_analysis(&app.snapshot, cost_scope);
    let api_cost_state = api_cost_window_state(api_cost_analysis);
    let api_cost_values = if app.table_columns.api_equivalent {
        filtered
            .iter()
            .skip(offset)
            .take(visible_capacity)
            .map(|row| {
                let cost = aggregate_task_row_usage_with_api_long_context(
                    &app.snapshot,
                    cost_scope,
                    row,
                    window_only,
                    false,
                )
                .api_equivalent_cost;
                format_scoped_api_cost_amount(api_cost_state, cost)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let api_cost_width = api_cost_column_width(api_cost_analysis, &api_cost_values);
    let visible_columns =
        task_visible_columns(app.table_columns, table_inner.width, api_cost_width);
    let task_column = task_table_columns(table_inner, visible_columns, api_cost_width)
        .last()
        .copied()
        .unwrap_or(table_inner);
    if tree_mode {
        app.task_tree_marker_hitboxes = filtered
            .iter()
            .skip(offset)
            .take(visible_capacity)
            .enumerate()
            .filter_map(|(position, row)| {
                if !row.has_children {
                    return None;
                }
                let marker_x = task_column.x.saturating_add(
                    u16::try_from(UnicodeWidthStr::width(row.prefix.as_str())).unwrap_or(u16::MAX),
                );
                (marker_x.saturating_add(TASK_TREE_MARKER_WIDTH) <= task_column.right()).then_some(
                    TaskTreeMarkerHitbox {
                        area: Rect::new(
                            marker_x,
                            table_inner
                                .y
                                .saturating_add(1)
                                .saturating_add(u16::try_from(position).unwrap_or(u16::MAX)),
                            TASK_TREE_MARKER_WIDTH,
                            1,
                        ),
                        task_index: row.index,
                    },
                )
            })
            .collect();
    }
    let task_rows = filtered
        .iter()
        .skip(offset)
        .take(visible_capacity)
        .enumerate()
        .filter_map(|(visible_position, row)| {
            app.snapshot
                .tasks
                .get(row.index)
                .map(|task| (task, row, visible_position))
        })
        .map(|(task, row, visible_position)| {
            let usage = aggregate_task_row_usage_with_api_long_context(
                &app.snapshot,
                app.window_scope,
                row,
                window_only,
                app.api_long_context_multiplier,
            );
            let tokens = usage.token_usage;
            let local_share = usage.local_token_share_percent;
            let estimated_quota = usage.estimated_quota_percent;
            let quota_confidence = usage.quota_confidence;
            let tone = task_status_tone(task.status);
            let status_prefix = if visible_columns.tokens {
                String::new()
            } else {
                format!("{} ", status_marker(tone))
            };
            let task_cell = if tree_mode {
                let marker_style = Style::default().fg(palette.muted);
                let shortcut_style =
                    if tasks_focused && app.shortcuts_active() && row.index == app.selected_task {
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        marker_style
                    };
                let mut spans = vec![Span::raw(row.prefix.clone())];
                if row.has_children {
                    spans.push(Span::styled("[", marker_style));
                    spans.push(Span::styled(
                        if row.collapsed { "+" } else { "-" },
                        shortcut_style,
                    ));
                    spans.push(Span::styled("]", marker_style));
                } else {
                    spans.push(Span::raw("   "));
                }
                spans.push(Span::raw(" "));
                spans.push(Span::raw(status_prefix));
                spans.push(Span::raw(task_display_label(task, row.depth > 0)));
                Cell::from(Line::from(spans))
            } else {
                Cell::from(format!(
                    "{status_prefix}{}",
                    task_display_label(task, false)
                ))
            };
            let mut cells = Vec::new();
            if visible_columns.tokens {
                cells.push(Cell::from(format!(
                    "{} {}",
                    status_marker(tone),
                    format_tokens(tokens)
                )));
            }
            if visible_columns.token_share {
                cells.push(Cell::from(format!("{local_share:.1}%")));
            }
            if visible_columns.estimated_quota {
                cells.push(Cell::from(format_estimated_quota(
                    estimated_quota,
                    quota_confidence,
                )));
            }
            if visible_columns.api_equivalent {
                cells.push(Cell::from(api_cost_values[visible_position].clone()));
            }
            cells.push(task_cell);
            Row::new(cells).style(status_tone_style(tone, theme))
        })
        .collect::<Vec<_>>();
    let mut headers = Vec::new();
    if visible_columns.tokens {
        headers.push("TOKENS");
    }
    if visible_columns.token_share {
        headers.push(if window_only {
            app.window_scope.token_share_header()
        } else {
            WindowScope::FiveHours.token_share_header()
        });
    }
    if visible_columns.estimated_quota {
        headers.push(if window_only {
            app.window_scope.quota_header()
        } else {
            WindowScope::FiveHours.quota_header()
        });
    }
    if visible_columns.api_equivalent {
        headers.push("API EQ.");
    }
    headers.push("TASK");
    let header = Row::new(headers).style(
        Style::default()
            .fg(app.theme.palette().accent)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(
        task_rows,
        task_table_constraints(visible_columns, api_cost_width),
    )
    .flex(Flex::Legacy)
    .column_spacing(TASK_COLUMN_SPACING)
    .header(header)
    .block(block)
    .row_highlight_style(
        Style::default()
            .fg(if tasks_focused {
                palette.accent
            } else {
                palette.muted
            })
            .add_modifier(if tasks_focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    )
    .highlight_spacing(HighlightSpacing::Always)
    .highlight_symbol(if tasks_focused { "▌" } else { "▏" });

    let mut state = TableState::default().with_selected(selected_in_view);
    frame.render_stateful_widget(table, area, &mut state);

    let remaining_rows = filtered.len().saturating_sub(offset);
    let visible_height = table_inner
        .height
        .saturating_sub(1)
        .min(u16::try_from(remaining_rows).unwrap_or(u16::MAX));
    let rows = Rect::new(
        table_inner.x,
        table_inner.y.saturating_add(1),
        table_inner.width,
        visible_height,
    );
    app.task_table_hitbox = (!rows.is_empty()).then_some(TableHitbox {
        viewport: table_inner,
        rows,
        offset,
        capacity: visible_capacity,
    });
    app.task_scrollbar_hitbox = scrollbar_geometry(
        Rect::new(area.right().saturating_sub(1), rows.y, 1, rows.height),
        filtered.len(),
        visible_capacity,
        offset,
    );
    if let Some(scrollbar) = app.task_scrollbar_hitbox {
        render_scrollbar(
            frame,
            scrollbar,
            theme,
            app.focus == Focus::Tasks
                || app
                    .scroll_drag
                    .is_some_and(|drag| drag.target == ScrollTarget::Tasks),
        );
    }
}

fn task_panel_block(
    area: Rect,
    app: &App,
    window_only: bool,
    filtered_count: usize,
) -> (Block<'static>, TaskControlsHitbox) {
    let palette = app.theme.palette();
    let inner_right = area.right().saturating_sub(1);
    let title = if window_only {
        app.window_scope.task_title()
    } else {
        "Recent tasks"
    };
    let full_controls_width = UnicodeWidthStr::width(format!(" {title}").as_str())
        + 2
        + 1
        + UnicodeWidthStr::width("[O]Open")
        + 1
        + UnicodeWidthStr::width("[R]Tree")
        + 1
        + UnicodeWidthStr::width("[E]Collapse")
        + TaskSourceFilter::ALL
            .into_iter()
            .map(|filter| 1 + UnicodeWidthStr::width(filter.label(false)) + 2)
            .sum::<usize>()
        + 1
        + UnicodeWidthStr::width("Filter:")
        + UnicodeWidthStr::width(CLEAR_FILTER_LABEL)
        + usize::from(FILTER_CLEAR_GAP_WIDTH + FILTER_MIN_QUERY_WIDTH);
    let compact = usize::from(area.width.saturating_sub(2)) < full_controls_width;
    let mut spans = vec![Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD),
    )];
    let enter_available = app.focus == Focus::Tasks
        && app.shortcuts_active()
        && app
            .snapshot
            .tasks
            .get(app.selected_task)
            .is_some_and(|task| app.task_matches_filter(task))
        && app.selected_task_raw_turn_count() > 0;
    let mut title_x = area.x.saturating_add(1).saturating_add(
        u16::try_from(UnicodeWidthStr::width(format!(" {title}").as_str())).unwrap_or(u16::MAX),
    );
    spans.push(Span::raw(" "));
    title_x = title_x.saturating_add(1);
    let enter_turns = title_hitbox(area, title_x, 1);
    spans.push(Span::styled(
        if enter_available {
            ENTER_FOCUS_HINT
        } else {
            " "
        },
        if enter_available {
            Style::default()
                .fg(palette.accent)
                .bg(palette.gauge_track)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        },
    ));
    title_x = title_x.saturating_add(1);

    if !compact {
        spans.push(Span::raw(" "));
        title_x = title_x.saturating_add(1);
    }
    let open_label = if compact { "[O]" } else { "[O]Open" };
    let open_width = u16::try_from(UnicodeWidthStr::width(open_label)).unwrap_or(u16::MAX);
    let open_terminal = title_hitbox(area, title_x, open_width);
    let open_style = Style::default().fg(palette.muted);
    let open_shortcut_style = if app.open_control_available() {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        open_style
    };
    spans.push(Span::styled("[", open_style));
    spans.push(Span::styled("O", open_shortcut_style));
    spans.push(Span::styled(
        if compact { "]" } else { "]Open" },
        open_style,
    ));
    title_x = title_x.saturating_add(open_width);

    if !compact {
        spans.push(Span::raw(" "));
        title_x = title_x.saturating_add(1);
    }
    let tree_label = if compact { "[R]" } else { "[R]Tree" };
    let tree_width = u16::try_from(UnicodeWidthStr::width(tree_label)).unwrap_or(u16::MAX);
    let toggle_tree = title_hitbox(area, title_x, tree_width);
    let tree_selected = app.task_list_mode == TaskListMode::Tree;
    let tree_style = if tree_selected {
        Style::default()
            .fg(palette.background)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let tree_shortcut_style = if !app.shortcuts_active() {
        tree_style
    } else if tree_selected {
        tree_style.add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    };
    spans.push(Span::styled("[", tree_style));
    spans.push(Span::styled("R", tree_shortcut_style));
    spans.push(Span::styled(
        if compact { "]" } else { "]Tree" },
        tree_style,
    ));
    title_x = title_x.saturating_add(tree_width);

    if !compact {
        spans.push(Span::raw(" "));
        title_x = title_x.saturating_add(1);
    }
    let expand_all = app.all_filtered_task_threads_collapsed();
    let collapse_width = if compact {
        3
    } else {
        u16::try_from(UnicodeWidthStr::width("[E]Collapse")).unwrap_or(u16::MAX)
    };
    let collapse_all = title_hitbox(area, title_x, collapse_width);
    let collapse_style = Style::default().fg(palette.muted);
    let collapse_available = !app.filtered_collapsible_task_threads().is_empty();
    let collapse_shortcut_style = if tree_selected && collapse_available && app.shortcuts_active() {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        collapse_style
    };
    spans.push(Span::styled("[", collapse_style));
    spans.push(Span::styled("E", collapse_shortcut_style));
    spans.push(Span::styled(
        if compact {
            "]"
        } else if expand_all {
            "]Expand  "
        } else {
            "]Collapse"
        },
        collapse_style,
    ));
    title_x = title_x.saturating_add(collapse_width);

    let mut source_hitboxes = [Rect::default(); 4];
    let shortcuts_active = app.shortcuts_active();
    for filter in TaskSourceFilter::ALL {
        if !compact {
            spans.push(Span::raw(" "));
            title_x = title_x.saturating_add(1);
        }
        let label = filter.label(compact);
        let label_width = u16::try_from(UnicodeWidthStr::width(label) + 2).unwrap_or(u16::MAX);
        source_hitboxes[filter.index()] = title_hitbox(area, title_x, label_width);
        let selected = app.task_source_filter == filter;
        let style = if selected {
            Style::default()
                .fg(palette.background)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        let shortcut_style = if !shortcuts_active {
            style
        } else if selected {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        };
        let mut label_chars = label.chars();
        let _ = label_chars.next();
        spans.push(Span::styled("[", style));
        spans.push(Span::styled(filter.shortcut().to_string(), shortcut_style));
        spans.push(Span::styled(
            format!("{}]", label_chars.collect::<String>()),
            style,
        ));
        title_x = title_x.saturating_add(label_width);
    }

    spans.push(Span::raw(" "));
    title_x = title_x.saturating_add(1);
    let search_start = title_x;
    let query_start = search_start.saturating_add("Filter:".len() as u16);
    let clear_width = u16::try_from(CLEAR_FILTER_LABEL.len()).unwrap_or(u16::MAX);
    let clear_reserve = clear_width
        .saturating_add(FILTER_CLEAR_GAP_WIDTH)
        .saturating_add(FILTER_MIN_QUERY_WIDTH);
    let clear_search = if !app.task_search.is_empty()
        && inner_right.saturating_sub(query_start) >= clear_reserve
    {
        Rect::new(
            inner_right - clear_width,
            area.y,
            clear_width,
            u16::from(area.height > 0),
        )
    } else {
        Rect::default()
    };
    let search_right = if clear_search.is_empty() {
        inner_right
    } else {
        clear_search.x
    };
    let search_style = if app.focus == Focus::TaskSearch {
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    spans.push(Span::styled(
        "F",
        if app.focus == Focus::Tasks && app.shortcuts_active() {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            search_style
        },
    ));
    spans.push(Span::styled("ilter:", search_style));
    let query_right = if clear_search.is_empty() {
        search_right
    } else {
        search_right.saturating_sub(FILTER_CLEAR_GAP_WIDTH)
    };
    let query_width = usize::from(query_right.saturating_sub(query_start));
    let rendered_query_width;
    if app.focus == Focus::TaskSearch {
        let (before, after, cursor_visible) =
            search_cursor_window(&app.task_search, app.task_search_cursor, query_width);
        rendered_query_width = UnicodeWidthStr::width(before.as_str())
            + UnicodeWidthStr::width(after.as_str())
            + usize::from(cursor_visible);
        spans.push(Span::styled(before, Style::default().fg(palette.title)));
        if cursor_visible {
            spans.push(Span::styled("▌", Style::default().fg(palette.accent)));
        }
        spans.push(Span::styled(after, Style::default().fg(palette.title)));
    } else {
        let query = compact_search_text(&app.task_search, query_width);
        rendered_query_width = UnicodeWidthStr::width(query.as_str());
        spans.push(Span::styled(query, Style::default().fg(palette.title)));
    }
    if !clear_search.is_empty() {
        let rendered_right =
            query_start.saturating_add(u16::try_from(rendered_query_width).unwrap_or(u16::MAX));
        let padding = clear_search.x.saturating_sub(rendered_right);
        spans.push(Span::raw(" ".repeat(usize::from(padding))));
        let clear_style = Style::default().fg(palette.muted);
        let shortcut_style = if app.focus == Focus::Tasks && app.shortcuts_active() {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            clear_style
        };
        spans.push(Span::styled("[", clear_style));
        spans.push(Span::styled("Del", shortcut_style));
        spans.push(Span::styled("]", clear_style));
    } else {
        spans.push(Span::raw(" "));
    }

    let (status, status_color) = if let Some(notice) = app
        .open_notice
        .as_ref()
        .filter(|notice| notice.created_at.elapsed() <= OPEN_NOTICE_DURATION)
    {
        let color = match notice.tone {
            OpenNoticeTone::Info => palette.accent,
            OpenNoticeTone::Success => palette.success,
            OpenNoticeTone::Warning => palette.warning,
            OpenNoticeTone::Error => palette.error,
        };
        (
            format!(" Open · {} ", terminal_safe_text(&notice.message)),
            color,
        )
    } else {
        let status = app
            .snapshot
            .tasks
            .get(app.selected_task)
            .filter(|task| app.task_matches_filter(task))
            .map(|task| {
                format!(
                    " {filtered_count}/{} · {} {} ",
                    app.snapshot.tasks.len(),
                    task.status.label(),
                    status_evidence(task.status_provenance, task.status_confidence)
                )
            })
            .unwrap_or_else(|| {
                let label = if app.snapshot.tasks.is_empty() {
                    "no tasks"
                } else {
                    "no matches"
                };
                format!(" 0/{} · {label} ", app.snapshot.tasks.len())
            });
        (status, palette.muted)
    };
    let border_color = if matches!(app.focus, Focus::Tasks | Focus::TaskSearch) {
        palette.accent
    } else {
        palette.border
    };
    let status_width = u16::try_from(UnicodeWidthStr::width(status.as_str())).unwrap_or(u16::MAX);
    let (legend, show_status) = task_footer_legend(app.theme, area.width, status_width);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(spans))
        .title_bottom(legend);
    if show_status {
        block = block.title_bottom(
            Line::from(Span::styled(status, Style::default().fg(status_color))).right_aligned(),
        );
    }
    let search_x = search_start.min(search_right);
    let controls = TaskControlsHitbox {
        sources: source_hitboxes,
        search: Rect::new(
            search_x,
            area.y,
            search_right.saturating_sub(search_x),
            u16::from(area.height > 0),
        ),
        clear_search,
        enter_turns,
        open_terminal,
        toggle_tree,
        collapse_all,
    };
    (block, controls)
}

fn title_hitbox(area: Rect, x: u16, width: u16) -> Rect {
    let inner_left = area.x.saturating_add(1);
    let inner_right = area.right().saturating_sub(1);
    let Some(end) = x.checked_add(width) else {
        return Rect::default();
    };
    if area.height == 0 || x < inner_left || end > inner_right {
        Rect::default()
    } else {
        Rect::new(x, area.y, width, 1)
    }
}

fn turn_panel_block(area: Rect, app: &App, title: &str) -> (Block<'static>, TurnControlsHitbox) {
    let palette = app.theme.palette();
    let inner_right = area.right().saturating_sub(1);
    let mut spans = vec![Span::styled(
        title.to_string(),
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD),
    )];
    let mut x = area
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(UnicodeWidthStr::width(title)).unwrap_or(u16::MAX));
    spans.push(Span::raw(" "));
    x = x.saturating_add(1);
    let back_available = app.focus == Focus::Turns && app.shortcuts_active();
    let back_tasks = title_hitbox(area, x, 1);
    spans.push(Span::styled(
        if back_available { BACK_FOCUS_HINT } else { " " },
        if back_available {
            Style::default()
                .fg(palette.accent)
                .bg(palette.gauge_track)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        },
    ));
    x = x.saturating_add(1);
    spans.push(Span::raw(" "));
    x = x.saturating_add(1);

    let search_start = x;
    let query_start = search_start.saturating_add("Filter:".len() as u16);
    let clear_width = u16::try_from(CLEAR_FILTER_LABEL.len()).unwrap_or(u16::MAX);
    let clear_reserve = clear_width
        .saturating_add(FILTER_CLEAR_GAP_WIDTH)
        .saturating_add(FILTER_MIN_QUERY_WIDTH);
    let clear_search = if !app.turn_search.is_empty()
        && inner_right.saturating_sub(query_start) >= clear_reserve
    {
        Rect::new(
            inner_right - clear_width,
            area.y,
            clear_width,
            u16::from(area.height > 0),
        )
    } else {
        Rect::default()
    };
    let search_right = if clear_search.is_empty() {
        inner_right
    } else {
        clear_search.x
    };
    let search_style = if app.focus == Focus::TurnSearch {
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    spans.push(Span::styled(
        "F",
        if app.focus == Focus::Turns && app.shortcuts_active() {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            search_style
        },
    ));
    spans.push(Span::styled("ilter:", search_style));
    let query_right = if clear_search.is_empty() {
        search_right
    } else {
        search_right.saturating_sub(FILTER_CLEAR_GAP_WIDTH)
    };
    let query_width = usize::from(query_right.saturating_sub(query_start));
    let rendered_query_width;
    if app.focus == Focus::TurnSearch {
        let (before, after, cursor_visible) =
            search_cursor_window(&app.turn_search, app.turn_search_cursor, query_width);
        rendered_query_width = UnicodeWidthStr::width(before.as_str())
            + UnicodeWidthStr::width(after.as_str())
            + usize::from(cursor_visible);
        spans.push(Span::styled(before, Style::default().fg(palette.title)));
        if cursor_visible {
            spans.push(Span::styled("▌", Style::default().fg(palette.accent)));
        }
        spans.push(Span::styled(after, Style::default().fg(palette.title)));
    } else {
        let query = compact_search_text(&app.turn_search, query_width);
        rendered_query_width = UnicodeWidthStr::width(query.as_str());
        spans.push(Span::styled(query, Style::default().fg(palette.title)));
    }
    if !clear_search.is_empty() {
        let rendered_right =
            query_start.saturating_add(u16::try_from(rendered_query_width).unwrap_or(u16::MAX));
        spans.push(Span::raw(" ".repeat(usize::from(
            clear_search.x.saturating_sub(rendered_right),
        ))));
        let clear_style = Style::default().fg(palette.muted);
        let shortcut_style = if app.focus == Focus::Turns && app.shortcuts_active() {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            clear_style
        };
        spans.push(Span::styled("[", clear_style));
        spans.push(Span::styled("Del", shortcut_style));
        spans.push(Span::styled("]", clear_style));
    } else {
        spans.push(Span::raw(" "));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if back_available {
            palette.accent
        } else {
            palette.border
        }))
        .title(Line::from(spans));
    (
        block,
        TurnControlsHitbox {
            back_tasks,
            search: Rect::new(
                search_start.min(search_right),
                area.y,
                search_right.saturating_sub(search_start.min(search_right)),
                u16::from(area.height > 0),
            ),
            clear_search,
        },
    )
}

fn turn_visible_columns(
    columns: UiTableColumns,
    table_inner_width: u16,
    api_cost_width: u16,
) -> (UiTableColumns, bool) {
    let available_width = table_inner_width.saturating_sub(TASK_HIGHLIGHT_WIDTH);
    let metric_widths = [
        TURN_TOKENS_WIDTH,
        TURN_TOKEN_SHARE_WIDTH,
        TURN_QUOTA_WIDTH,
        api_cost_width,
    ];
    let full = [TURN_MODEL_WIDTH, TURN_EFFORT_WIDTH, TURN_MESSAGE_WIDTH];
    if configured_table_min_width(columns, &full, metric_widths) <= available_width {
        return (columns, true);
    }
    (
        fit_table_columns(
            columns,
            available_width,
            &[TURN_COMPACT_MODEL_WIDTH, TURN_COMPACT_MESSAGE_WIDTH],
            metric_widths,
        ),
        false,
    )
}

fn turn_table_constraints(
    columns: UiTableColumns,
    show_effort: bool,
    api_cost_width: u16,
) -> Vec<Constraint> {
    let mut constraints = if show_effort {
        vec![
            Constraint::Length(TURN_MODEL_WIDTH),
            Constraint::Length(TURN_EFFORT_WIDTH),
            Constraint::Min(TURN_MESSAGE_WIDTH),
        ]
    } else {
        vec![
            Constraint::Length(TURN_COMPACT_MODEL_WIDTH),
            Constraint::Min(TURN_COMPACT_MESSAGE_WIDTH),
        ]
    };
    if columns.tokens {
        constraints.push(Constraint::Length(TURN_TOKENS_WIDTH));
    }
    if columns.token_share {
        constraints.push(Constraint::Length(TURN_TOKEN_SHARE_WIDTH));
    }
    if columns.estimated_quota {
        constraints.push(Constraint::Length(TURN_QUOTA_WIDTH));
    }
    if columns.api_equivalent {
        constraints.push(Constraint::Length(api_cost_width));
    }
    constraints
}

fn render_turns(frame: &mut Frame<'_>, area: Rect, app: &mut App, window_only: bool) {
    let detail_height = turn_detail_height(area.height);
    let (table_area, detail_area) = if detail_height == 0 {
        (area, None)
    } else {
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(detail_height)])
            .split(area);
        (regions[0], Some(regions[1]))
    };

    let turns = app
        .filtered_turn_indices()
        .into_iter()
        .filter_map(|index| app.snapshot.turns.get(index))
        .collect::<Vec<_>>();
    app.selected_turn = app.selected_turn.min(turns.len().saturating_sub(1));

    let title_base = if window_only {
        match app.window_scope {
            WindowScope::FiveHours => "Turns · 5h cycle",
            WindowScope::Week => "Turns · Week cycle",
        }
    } else {
        "Turns"
    };
    let turns_focused = app.focus == Focus::Turns;
    let (table_block, turn_controls) = turn_panel_block(table_area, app, title_base);
    app.turn_controls_hitbox = Some(turn_controls);
    let table_inner = table_block.inner(table_area);
    let visible_capacity = usize::from(table_inner.height.saturating_sub(1));
    app.turn_offset = app
        .turn_offset
        .min(turns.len().saturating_sub(visible_capacity));
    if app.turn_reveal_pending {
        app.turn_offset = reveal_offset(
            app.turn_offset,
            app.selected_turn,
            turns.len(),
            visible_capacity,
        );
        app.turn_reveal_pending = false;
    }
    let offset = app.turn_offset;
    let selected_in_view = app
        .selected_turn
        .checked_sub(offset)
        .filter(|index| *index < visible_capacity);
    let cost_scope = if window_only {
        app.window_scope
    } else {
        WindowScope::FiveHours
    };
    let api_cost_analysis = window_analysis(&app.snapshot, cost_scope);
    let api_cost_state = api_cost_window_state(api_cost_analysis);
    let api_cost_values = if app.table_columns.api_equivalent {
        turns
            .iter()
            .skip(offset)
            .take(visible_capacity)
            .map(|turn| {
                let cost = turn_usage_for_scope_with_api_long_context(
                    &app.snapshot,
                    cost_scope,
                    turn,
                    false,
                )
                .api_equivalent_cost;
                format_scoped_api_cost_amount(api_cost_state, cost)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let api_cost_width = api_cost_column_width(api_cost_analysis, &api_cost_values);
    let (visible_columns, show_effort_column) =
        turn_visible_columns(app.table_columns, table_inner.width, api_cost_width);
    let model_column_width = if show_effort_column {
        usize::from(TURN_MODEL_WIDTH)
    } else {
        usize::from(TURN_COMPACT_MODEL_WIDTH)
    };
    let theme = app.theme;
    let rows = turns
        .iter()
        .skip(offset)
        .take(visible_capacity)
        .enumerate()
        .map(|(visible_position, turn)| {
            let usage = turn_usage_for_scope_with_api_long_context(
                &app.snapshot,
                app.window_scope,
                turn,
                app.api_long_context_multiplier,
            );
            let tokens = if window_only {
                usage.token_usage
            } else {
                turn.token_usage
            };
            let local_share = if window_only {
                usage.local_token_share_percent
            } else {
                turn.local_token_share_percent
            };
            let estimated_quota = if window_only {
                usage.estimated_quota_percent
            } else {
                turn.estimated_quota_percent
            };
            let quota_confidence = if window_only {
                usage.quota_confidence
            } else {
                turn.quota_confidence
            };
            let model = terminal_safe_text(turn.model.as_deref().unwrap_or("unknown"));
            let effort = terminal_safe_text(turn.reasoning_effort.as_deref().unwrap_or("unknown"));
            let tone = turn_status_tone(turn.status);
            let message = terminal_safe_text(turn.message_preview.as_deref().unwrap_or("-"));
            let message = if visible_columns.tokens {
                message
            } else {
                format!("{} {message}", status_marker(tone))
            };
            let mut cells = Vec::new();
            let model = if turn.is_fast() {
                fast_model_line(&model, model_column_width, theme)
            } else {
                Line::from(model)
            };
            if show_effort_column {
                cells.push(Cell::from(model));
                cells.push(Cell::from(effort));
                cells.push(Cell::from(message));
            } else {
                let compact_model = if turn.is_fast() {
                    fast_model_line(
                        &format!(
                            "{effort}/{}",
                            terminal_safe_text(turn.model.as_deref().unwrap_or("unknown"))
                        ),
                        model_column_width,
                        theme,
                    )
                } else {
                    Line::from(format!(
                        "{effort}/{}",
                        terminal_safe_text(turn.model.as_deref().unwrap_or("unknown"))
                    ))
                };
                cells.push(Cell::from(compact_model));
                cells.push(Cell::from(message));
            }
            if visible_columns.tokens {
                cells.push(Cell::from(format!(
                    "{} {}",
                    status_marker(tone),
                    format_tokens(tokens)
                )));
            }
            if visible_columns.token_share {
                cells.push(Cell::from(format!("{local_share:.1}%")));
            }
            if visible_columns.estimated_quota {
                cells.push(Cell::from(format_estimated_quota(
                    estimated_quota,
                    quota_confidence,
                )));
            }
            if visible_columns.api_equivalent {
                cells.push(Cell::from(api_cost_values[visible_position].clone()));
            }
            Row::new(cells).style(status_tone_style(tone, theme))
        });
    let mut headers = if show_effort_column {
        vec!["MODEL", "EFFORT", "MESSAGE"]
    } else {
        vec!["EFFORT/MODEL", "MESSAGE"]
    };
    if visible_columns.tokens {
        headers.push("TOKENS");
    }
    if visible_columns.token_share {
        headers.push("TOKEN%");
    }
    if visible_columns.estimated_quota {
        headers.push("EST.Q");
    }
    if visible_columns.api_equivalent {
        headers.push("API EQ.");
    }
    let header = Row::new(headers).style(
        Style::default()
            .fg(theme.palette().accent)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(
        rows,
        turn_table_constraints(visible_columns, show_effort_column, api_cost_width),
    )
    .header(header)
    .block(table_block)
    .row_highlight_style(
        Style::default()
            .fg(if turns_focused {
                theme.palette().accent
            } else {
                theme.palette().muted
            })
            .add_modifier(if turns_focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    )
    .highlight_spacing(HighlightSpacing::Always)
    .highlight_symbol(if turns_focused { "▌" } else { "▏" });
    let mut state = TableState::default().with_selected(selected_in_view);
    frame.render_stateful_widget(table, table_area, &mut state);

    let remaining_rows = turns.len().saturating_sub(offset);
    let visible_height = table_inner
        .height
        .saturating_sub(1)
        .min(u16::try_from(remaining_rows).unwrap_or(u16::MAX));
    let rows = Rect::new(
        table_inner.x,
        table_inner.y.saturating_add(1),
        table_inner.width,
        visible_height,
    );
    app.turn_table_hitbox = (!rows.is_empty()).then_some(TableHitbox {
        viewport: table_inner,
        rows,
        offset,
        capacity: visible_capacity,
    });
    app.turn_scrollbar_hitbox = scrollbar_geometry(
        Rect::new(table_area.right().saturating_sub(1), rows.y, 1, rows.height),
        turns.len(),
        visible_capacity,
        offset,
    );
    if let Some(scrollbar) = app.turn_scrollbar_hitbox {
        render_scrollbar(
            frame,
            scrollbar,
            theme,
            app.focus == Focus::Turns
                || app
                    .scroll_drag
                    .is_some_and(|drag| drag.target == ScrollTarget::Turns),
        );
    }

    if let Some(detail_area) = detail_area {
        let detail_scope = if window_only {
            app.window_scope
        } else {
            WindowScope::FiveHours
        };
        let selected_turn = turns.get(app.selected_turn).copied();
        let selected_usage = selected_turn
            .map(|turn| {
                let mut usage = turn_usage_for_scope_with_api_long_context(
                    &app.snapshot,
                    detail_scope,
                    turn,
                    app.api_long_context_multiplier,
                );
                usage.api_equivalent_cost = turn_usage_for_scope_with_api_long_context(
                    &app.snapshot,
                    detail_scope,
                    turn,
                    false,
                )
                .api_equivalent_cost;
                usage
            })
            .unwrap_or_default();
        render_turn_detail(
            frame,
            detail_area,
            selected_turn,
            if app.selected_task_raw_turn_count() == 0 {
                "No turns for selected task"
            } else {
                "No matching turns"
            },
            app.selected_turn,
            turns.len(),
            window_only,
            detail_scope,
            selected_usage,
            api_cost_state,
            theme,
        );
    }
}

fn turn_detail_height(area_height: u16) -> u16 {
    match area_height {
        24.. => 8,
        20..=23 => 7,
        16..=19 => 6,
        12..=15 => 5,
        8..=11 => 4,
        7 => 3,
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_turn_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    turn: Option<&TurnRecord>,
    empty_message: &'static str,
    selected_index: usize,
    turn_count: usize,
    window_only: bool,
    window_scope: WindowScope,
    selected_window_usage: WindowUsage,
    api_cost_state: ApiCostWindowState,
    theme: Theme,
) {
    let Some(turn) = turn else {
        frame.render_widget(
            Paragraph::new(empty_message).block(panel("Turn detail", theme)),
            area,
        );
        return;
    };

    let duration = format_duration(turn.duration_ms);
    let title = format!(
        "Turn detail · {}/{} · {} · {duration}",
        selected_index + 1,
        turn_count,
        turn.status.label().to_ascii_uppercase()
    );
    let model = terminal_safe_text(turn.model.as_deref().unwrap_or("unknown"));
    let effort = terminal_safe_text(turn.reasoning_effort.as_deref().unwrap_or("unknown"));
    let mut bottom_title = vec![Span::styled(
        format!(" {model} · {effort}"),
        Style::default().fg(theme.palette().muted),
    )];
    if turn.is_fast() {
        bottom_title.push(Span::styled(
            " · FAST",
            Style::default()
                .fg(theme.palette().warning)
                .add_modifier(Modifier::BOLD),
        ));
    }
    bottom_title.push(Span::raw(" "));
    let content_width = usize::from(area.width.saturating_sub(2));
    let all_tokens = format_token_breakdown("all", turn.token_usage, content_width);
    let selected_window_tokens = format_token_breakdown(
        window_scope.label(),
        selected_window_usage.token_usage,
        content_width,
    );
    let (first_tokens, second_tokens) = if window_only {
        (selected_window_tokens, all_tokens)
    } else {
        (all_tokens, selected_window_tokens)
    };
    let started = format_turn_timestamp(turn.started_at.as_ref());
    let completed = format_turn_timestamp(turn.completed_at.as_ref());
    let message = terminal_safe_text(turn.message_preview.as_deref().unwrap_or("-"));
    let quota_confidence = if window_only {
        selected_window_usage.quota_confidence
    } else {
        turn.quota_confidence
    };
    let estimated_quota = format_estimated_quota(
        if window_only {
            selected_window_usage.estimated_quota_percent
        } else {
            turn.estimated_quota_percent
        },
        quota_confidence,
    );
    let local_share_percent = if window_only {
        selected_window_usage.local_token_share_percent
    } else {
        turn.local_token_share_percent
    };
    let quota_allocation = format!("share={local_share_percent:.1}% · est={estimated_quota}");
    let allocation_lines = if selected_window_usage.api_equivalent_cost.observed_samples > 0 {
        let api_cost = selected_window_usage.api_equivalent_cost;
        let coverage = if api_cost.priced_samples < api_cost.observed_samples
            || api_cost.priced_tokens < api_cost.observed_tokens
        {
            format!(" · cov={:.1}%", api_cost.priced_token_percent())
        } else {
            String::new()
        };
        let api_allocation = format!(
            "api[{}]={}{}",
            window_scope.label(),
            format_scoped_api_cost_amount(api_cost_state, api_cost),
            coverage,
        );
        let combined = format!("{quota_allocation} · {api_allocation}");
        if UnicodeWidthStr::width(combined.as_str()) <= content_width {
            vec![Line::from(combined)]
        } else {
            vec![Line::from(quota_allocation), Line::from(api_allocation)]
        }
    } else {
        vec![Line::from(quota_allocation)]
    };
    let split_allocation = allocation_lines.len() > 1;
    let mut lines = vec![Line::from(first_tokens), Line::from(second_tokens)];
    lines.extend(allocation_lines);
    lines.push(Line::from(format!(
        "start={started} · end={completed} · duration={duration}"
    )));
    let turn_id = terminal_safe_text(&turn.turn_id);
    if split_allocation {
        let compact_turn_id = truncate_display_text(&turn_id, 9);
        lines.push(Line::from(format!(
            "turn={compact_turn_id} · message={message}"
        )));
    } else {
        lines.push(Line::from(format!("turn={turn_id}")));
        lines.push(Line::from(format!("message={message}")));
    }
    let block = panel(&title, theme).title_bottom(Line::from(bottom_title));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn format_token_breakdown(label: &str, usage: TokenUsage, width: usize) -> String {
    let exact = format!(
        "{label} total={} in={} cache={} out={} reason={}",
        usage.total_tokens,
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens
    );
    if exact.len() <= width {
        exact
    } else {
        format!(
            "{label} total={} in={} cache={} out={} reason={}",
            format_tokens(usage),
            format_tokens(TokenUsage {
                total_tokens: usage.input_tokens,
                ..TokenUsage::default()
            }),
            format_tokens(TokenUsage {
                total_tokens: usage.cached_input_tokens,
                ..TokenUsage::default()
            }),
            format_tokens(TokenUsage {
                total_tokens: usage.output_tokens,
                ..TokenUsage::default()
            }),
            format_tokens(TokenUsage {
                total_tokens: usage.reasoning_output_tokens,
                ..TokenUsage::default()
            })
        )
    }
}

fn format_duration(duration_ms: Option<u64>) -> String {
    let Some(duration_ms) = duration_ms else {
        return "-".to_string();
    };
    if duration_ms < 1_000 {
        format!("{duration_ms}ms")
    } else if duration_ms < 60_000 {
        format!("{:.1}s", duration_ms as f64 / 1_000.0)
    } else {
        let total_seconds = duration_ms / 1_000;
        let hours = total_seconds / 3_600;
        let minutes = (total_seconds % 3_600) / 60;
        let seconds = total_seconds % 60;
        if hours > 0 {
            format!("{hours}h{minutes:02}m{seconds:02}s")
        } else {
            format!("{minutes}m{seconds:02}s")
        }
    }
}

fn format_turn_timestamp(value: Option<&chrono::DateTime<chrono::Utc>>) -> String {
    value
        .map(|value| format_local_time(*value, "%m-%d %H:%M:%S"))
        .unwrap_or_else(|| "-".to_string())
}

fn attribution_summary_lines(
    attribution: Option<&AttributionSummary>,
    window_scope: WindowScope,
    selected_partial: bool,
    partial_reasons: &[String],
    compact: bool,
) -> Vec<String> {
    let Some(attribution) = attribution else {
        return vec![
            format!(
                "Attribution  {} reset cycle unavailable",
                window_scope.label()
            ),
            "No active quota window with duration and reset time".to_string(),
        ];
    };
    let window = attribution
        .window
        .as_ref()
        .map(|window| {
            format!(
                "{} reset cycle · {:.1}% used · {} to {}",
                terminal_safe_text(&window.label),
                window.used_percent,
                format_local_time(window.starts_at, "%m-%d %H:%M"),
                format_local_time(window.ends_at, "%m-%d %H:%M")
            )
        })
        .unwrap_or_else(|| format!("{} reset cycle unavailable", window_scope.label()));
    let has_window = attribution.window.is_some();
    let has_local_denominator = attribution.local_token_usage.total_tokens > 0;
    let estimate_available =
        has_window && has_local_denominator && attribution.confidence != Confidence::Unknown;
    let allocation = if estimate_available {
        if compact {
            format!(
                "Tokens {} · EST ~{:.2}pp · codex gauge × credit-rate share",
                format_tokens(attribution.local_token_usage),
                attribution.proxy_projected_percent,
            )
        } else {
            format!(
                "{} token total · ~{:.2}pp estimated · codex gauge × credit-rate share",
                format_tokens(attribution.local_token_usage),
                attribution.proxy_projected_percent,
            )
        }
    } else if !has_window && compact {
        format!(
            "Tokens {} · EST - · no quota window",
            format_tokens(attribution.local_token_usage)
        )
    } else if !has_window {
        format!(
            "{} token total · estimate unavailable without a quota window",
            format_tokens(attribution.local_token_usage)
        )
    } else if !has_local_denominator && compact {
        "Tokens 0 · EST - · no token denominator".to_string()
    } else if !has_local_denominator {
        "0 token total · estimate unavailable without a token denominator".to_string()
    } else if compact {
        format!(
            "Tokens {} · EST - · estimate unavailable",
            format_tokens(attribution.local_token_usage)
        )
    } else {
        format!(
            "{} token total · estimate unavailable",
            format_tokens(attribution.local_token_usage)
        )
    };
    let mut quality = if compact {
        "Credit-rate-weighted quota proxy · not server accounting".to_string()
    } else {
        "Credit-rate-weighted quota proxy, not server per-task accounting".to_string()
    };
    if attribution.external_activity_possible {
        quality.push_str(if compact {
            " · external"
        } else {
            " · external possible"
        });
    }
    if attribution.settled {
        quality.push_str(" · settled");
    }
    if selected_partial {
        quality.push_str(" · partial");
        if !partial_reasons.is_empty() {
            quality.push_str(": ");
            quality.push_str(&terminal_safe_text(&partial_reasons.join(", ")));
        }
    }
    vec![format!("Attribution  {window}"), allocation, quality]
}

fn api_equivalent_summary_line(analysis: &WindowAnalysis) -> Option<String> {
    let rates_as_of = analysis.api_pricing.rates_as_of.trim();
    if analysis.api_pricing.catalog_revision == 0 || rates_as_of.is_empty() {
        return None;
    }

    let state = api_cost_window_state(Some(analysis));
    let amount = format_scoped_api_cost_amount(state, analysis.api_equivalent_cost.amount);
    let coverage = if matches!(
        state,
        ApiCostWindowState::Unavailable | ApiCostWindowState::NoLocalData
    ) {
        "-".to_string()
    } else {
        format!(
            "{:.1}%",
            analysis.api_equivalent_cost.amount.priced_token_percent()
        )
    };
    Some(format!(
        "API equivalent {amount} · model calls only · coverage {coverage} · rates {}",
        terminal_safe_text(rates_as_of),
    ))
}

#[cfg(test)]
fn models_for_scope(snapshot: &Snapshot, scope: WindowScope) -> Vec<ModelUsage> {
    models_for_scope_with_api_long_context(snapshot, scope, false)
}

fn models_for_scope_with_api_long_context(
    snapshot: &Snapshot,
    scope: WindowScope,
    api_long_context: bool,
) -> Vec<ModelUsage> {
    window_analysis_with_api_long_context(snapshot, scope, api_long_context)
        .map(|analysis| analysis.models.clone())
        .unwrap_or_else(|| {
            if scope == WindowScope::FiveHours && has_legacy_codex_window(snapshot) {
                snapshot.models.clone()
            } else {
                Vec::new()
            }
        })
}

fn wrapped_text_height(lines: &[String], width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    lines
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()).max(1).div_ceil(width))
        .sum()
}

fn model_visible_columns(
    columns: UiTableColumns,
    width: u16,
    api_cost_width: u16,
) -> UiTableColumns {
    fit_table_columns(
        columns,
        width,
        &[18],
        [
            MODEL_TOKENS_WIDTH,
            MODEL_TOKEN_SHARE_WIDTH,
            MODEL_QUOTA_WIDTH,
            api_cost_width,
        ],
    )
}

fn model_table_constraints(columns: UiTableColumns, api_cost_width: u16) -> Vec<Constraint> {
    let mut constraints = vec![Constraint::Min(18)];
    if columns.tokens {
        constraints.push(Constraint::Length(MODEL_TOKENS_WIDTH));
    }
    if columns.token_share {
        constraints.push(Constraint::Length(MODEL_TOKEN_SHARE_WIDTH));
    }
    if columns.estimated_quota {
        constraints.push(Constraint::Length(MODEL_QUOTA_WIDTH));
    }
    if columns.api_equivalent {
        constraints.push(Constraint::Length(api_cost_width));
    }
    constraints
}

fn model_api_cost_for_analysis(
    analysis: Option<&WindowAnalysis>,
    model: &ModelUsage,
) -> ApiCostAmount {
    analysis
        .and_then(|analysis| {
            analysis
                .models
                .iter()
                .find(|base| base.model == model.model)
                .map(|base| base.api_equivalent_cost)
        })
        .unwrap_or(model.api_equivalent_cost)
}

fn render_models(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let theme = app.theme;
    let window_scope = app.window_scope;
    // API-equivalent cost always follows the published API pricing rules. The
    // optional long-context switch changes only the Codex quota estimate.
    let api_cost_analysis = window_analysis(&app.snapshot, window_scope);
    let api_cost_state = api_cost_window_state(api_cost_analysis);
    let analysis = window_analysis_with_api_long_context(
        &app.snapshot,
        window_scope,
        app.api_long_context_multiplier,
    );
    let attribution = attribution_for_scope_with_api_long_context(
        &app.snapshot,
        window_scope,
        app.api_long_context_multiplier,
    );
    let mut models = models_for_scope_with_api_long_context(
        &app.snapshot,
        window_scope,
        app.api_long_context_multiplier,
    );
    models.sort_by(|left, right| {
        right
            .token_usage
            .total_tokens
            .cmp(&left.token_usage.total_tokens)
            .then_with(|| left.model.cmp(&right.model))
    });

    let panel_inner = Block::default().borders(Borders::ALL).inner(area);
    let compact = panel_inner.width < 100;
    let api_summary_line = api_cost_analysis.and_then(api_equivalent_summary_line);
    let selected_partial = analysis
        .map(|analysis| analysis.partial)
        .unwrap_or(window_scope == WindowScope::FiveHours && app.snapshot.partial);
    let partial_reasons = analysis
        .map(|analysis| analysis.partial_reasons.as_slice())
        .unwrap_or_default();
    let mut attribution_lines = attribution_summary_lines(
        attribution,
        window_scope,
        selected_partial,
        partial_reasons,
        compact,
    );
    if let Some(api_summary_line) = api_summary_line {
        attribution_lines.insert(attribution_lines.len().min(2), api_summary_line);
    }
    let attribution_height = u16::try_from(wrapped_text_height(
        &attribution_lines,
        usize::from(panel_inner.width),
    ))
    .unwrap_or(u16::MAX)
    .min(panel_inner.height);
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(attribution_height), Constraint::Min(0)])
        .split(panel_inner);
    let model_area = regions[1];
    let visible_capacity = usize::from(model_area.height.saturating_sub(1));
    let api_cost_values = if app.table_columns.api_equivalent {
        models
            .iter()
            .take(visible_capacity)
            .map(|model| {
                format_scoped_api_cost_amount(
                    api_cost_state,
                    model_api_cost_for_analysis(api_cost_analysis, model),
                )
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let api_cost_width = api_cost_column_width(api_cost_analysis, &api_cost_values);
    let visible_columns =
        model_visible_columns(app.table_columns, model_area.width, api_cost_width);
    let visible_count = models.len().min(visible_capacity);
    let scope = attribution
        .and_then(|attribution| attribution.window.as_ref())
        .map(|window| window.label.clone());
    let mut title_suffix = scope.as_deref().unwrap_or(window_scope.label()).to_string();
    if app.api_long_context_multiplier {
        title_suffix.push_str(" · EST Longx ON");
    }
    if attribution.is_none() {
        title_suffix.push_str(" unavailable");
    }
    if visible_count < models.len() {
        title_suffix.push_str(&format!(" · top {visible_count}/{}", models.len()));
    }
    frame.render_widget(models_panel_block(app, &title_suffix), area);

    frame.render_widget(
        Paragraph::new(
            attribution_lines
                .into_iter()
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .style(Style::default().fg(theme.palette().muted))
        .wrap(Wrap { trim: true }),
        regions[0],
    );

    if model_area.is_empty() {
        return;
    }
    if models.is_empty() {
        let message = if attribution.is_some() {
            format!(
                "No token usage in the current {} window",
                scope.as_deref().unwrap_or(window_scope.label())
            )
        } else if window_scope == WindowScope::FiveHours
            && has_active_window(&app.snapshot, WindowScope::Week.duration_mins())
        {
            "5h window unavailable; weekly reset-cycle data remains available".to_string()
        } else {
            format!("No active {} quota window", window_scope.label())
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(theme.palette().muted))
                .wrap(Wrap { trim: true }),
            model_area,
        );
        return;
    }

    let rows = models
        .iter()
        .take(visible_capacity)
        .enumerate()
        .map(|(visible_position, model)| {
            let mut cells = vec![Cell::from(terminal_safe_text(&model.model))];
            if visible_columns.tokens {
                cells.push(Cell::from(format_tokens(model.token_usage)));
            }
            if visible_columns.token_share {
                cells.push(Cell::from(format!(
                    "{:.1}%",
                    model.local_token_share_percent
                )));
            }
            if visible_columns.estimated_quota {
                cells.push(Cell::from(format_estimated_quota(
                    model.estimated_quota_percent,
                    model.quota_confidence,
                )));
            }
            if visible_columns.api_equivalent {
                cells.push(Cell::from(api_cost_values[visible_position].clone()));
            }
            Row::new(cells)
        })
        .collect::<Vec<_>>();
    let mut headers = vec!["MODEL"];
    if visible_columns.tokens {
        headers.push("TOKENS");
    }
    if visible_columns.token_share {
        headers.push("TOKEN SHARE");
    }
    if visible_columns.estimated_quota {
        headers.push("EST. QUOTA");
    }
    if visible_columns.api_equivalent {
        headers.push("API EQ.");
    }
    let header = Row::new(headers).style(
        Style::default()
            .fg(theme.palette().accent)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(
        rows,
        model_table_constraints(visible_columns, api_cost_width),
    )
    .header(header);
    frame.render_widget(table, model_area);
}

fn has_active_window(snapshot: &Snapshot, duration_mins: i64) -> bool {
    snapshot
        .limits
        .iter()
        .flat_map(|bucket| [bucket.primary.as_ref(), bucket.secondary.as_ref()])
        .flatten()
        .any(|window| {
            window.window_duration_mins == Some(duration_mins)
                && window.resets_at.is_some_and(|reset| reset > snapshot.as_of)
        })
}

fn models_panel_block(app: &App, suffix: &str) -> Block<'static> {
    let palette = app.theme.palette();
    let spans = vec![
        Span::styled(
            " Models",
            Style::default()
                .fg(palette.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", terminal_safe_text(suffix)),
            Style::default().fg(palette.muted),
        ),
    ];
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.border))
        .title(Line::from(spans))
}

fn panel(title: &str, theme: Theme) -> Block<'_> {
    panel_with_focus_hint(title, None, theme)
}

fn panel_with_focus_hint<'a>(title: &'a str, hint: Option<&'a str>, theme: Theme) -> Block<'a> {
    let palette = theme.palette();
    let mut title_spans = vec![Span::styled(
        title,
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(hint) = hint {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            hint,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.border))
        .title(Line::from(title_spans))
}

fn table_header<const N: usize>(labels: [&str; N], theme: Theme) -> Row<'static> {
    Row::new(labels.map(|label| Cell::from(label.to_string()))).style(
        Style::default()
            .fg(theme.palette().accent)
            .add_modifier(Modifier::BOLD),
    )
}

fn quota_color(used_percent: f64, theme: Theme) -> Color {
    let palette = theme.palette();
    if used_percent >= 90.0 {
        palette.error
    } else if used_percent >= 70.0 {
        palette.warning
    } else {
        palette.success
    }
}

fn task_status_tone(status: TaskStatus) -> StatusTone {
    match status {
        TaskStatus::Running => StatusTone::Active,
        TaskStatus::WaitingApproval | TaskStatus::WaitingInput => StatusTone::Waiting,
        TaskStatus::Completed | TaskStatus::Idle => StatusTone::Done,
        TaskStatus::Interrupted => StatusTone::Stopped,
        TaskStatus::Failed => StatusTone::Failed,
        TaskStatus::Stale | TaskStatus::Unknown => StatusTone::Stale,
    }
}

fn turn_status_tone(status: TurnStatus) -> StatusTone {
    match status {
        TurnStatus::InProgress => StatusTone::Active,
        TurnStatus::Completed => StatusTone::Done,
        TurnStatus::Interrupted => StatusTone::Stopped,
        TurnStatus::Failed => StatusTone::Failed,
        TurnStatus::Stale | TurnStatus::Unknown => StatusTone::Stale,
    }
}

fn status_tone_style(tone: StatusTone, theme: Theme) -> Style {
    let (foreground, background) = match theme {
        Theme::Dark => (
            Color::Rgb(210, 214, 220),
            match tone {
                StatusTone::Active => Color::Rgb(20, 49, 34),
                StatusTone::Waiting => Color::Rgb(54, 48, 24),
                StatusTone::Done => Color::Rgb(24, 42, 47),
                StatusTone::Stopped => Color::Rgb(36, 38, 54),
                StatusTone::Failed => Color::Rgb(57, 28, 31),
                StatusTone::Stale => Color::Rgb(34, 34, 38),
            },
        ),
        Theme::Light => match tone {
            StatusTone::Active => (Color::Rgb(24, 92, 55), Color::Rgb(231, 246, 236)),
            StatusTone::Waiting => (Color::Rgb(116, 74, 0), Color::Rgb(255, 244, 214)),
            StatusTone::Done => (Color::Rgb(7, 89, 133), Color::Rgb(230, 243, 248)),
            StatusTone::Stopped => (Color::Rgb(91, 63, 120), Color::Rgb(240, 236, 248)),
            StatusTone::Failed => (Color::Rgb(159, 18, 57), Color::Rgb(253, 235, 237)),
            StatusTone::Stale => (Color::Rgb(71, 84, 103), Color::Rgb(238, 240, 243)),
        },
    };
    Style::default().fg(foreground).bg(background)
}

fn status_marker(tone: StatusTone) -> &'static str {
    match tone {
        StatusTone::Active => "R",
        StatusTone::Waiting => "W",
        StatusTone::Done => "D",
        StatusTone::Stopped => "X",
        StatusTone::Failed => "F",
        StatusTone::Stale => "?",
    }
}

fn status_legend(theme: Theme, width: u16) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let statuses = [
        ("RUN", StatusTone::Active),
        ("WAIT", StatusTone::Waiting),
        ("DONE", StatusTone::Done),
        ("STOP", StatusTone::Stopped),
        ("FAIL", StatusTone::Failed),
        ("STALE", StatusTone::Stale),
    ];
    let compact = width < 58;
    for (index, (label, tone)) in statuses.into_iter().enumerate() {
        let style = status_tone_style(tone, theme).add_modifier(Modifier::BOLD);
        let style = if theme == Theme::Dark {
            style.fg(theme.palette().title)
        } else {
            style
        };
        spans.push(Span::styled(
            if compact {
                format!("{}:{label}", status_marker(tone))
            } else {
                format!(" {} {label} ", status_marker(tone))
            },
            style,
        ));
        if index + 1 < statuses.len() || !compact {
            spans.push(Span::raw(" "));
        }
    }
    Line::from(spans)
}

fn task_footer_legend(theme: Theme, width: u16, status_width: u16) -> (Line<'static>, bool) {
    let inner_width = width.saturating_sub(2);
    let full = status_legend(theme, u16::MAX);
    let compact = status_legend(theme, 0);
    let full_width = u16::try_from(full.width()).unwrap_or(u16::MAX);
    let full_with_status = full_width.saturating_add(1).saturating_add(status_width);
    if full_with_status <= inner_width {
        return (full, true);
    }

    let compact_with_status = u16::try_from(compact.width())
        .unwrap_or(u16::MAX)
        .saturating_add(1)
        .saturating_add(status_width);
    if compact_with_status <= inner_width {
        (compact, true)
    } else if full_width <= inner_width {
        (full, false)
    } else {
        (compact, false)
    }
}

fn task_table_columns(area: Rect, columns: UiTableColumns, api_cost_width: u16) -> Vec<Rect> {
    let [_highlight, column_area] = Layout::horizontal([
        Constraint::Length(TASK_HIGHLIGHT_WIDTH),
        Constraint::Fill(0),
    ])
    .areas(area);
    Layout::horizontal(task_table_constraints(columns, api_cost_width))
        .flex(Flex::Legacy)
        .spacing(TASK_COLUMN_SPACING)
        .split(column_area)
        .to_vec()
}

fn task_display_label(task: &TaskRecord, omit_project: bool) -> String {
    let source = task.source.as_deref().unwrap_or("unknown");
    let label = if omit_project {
        format!("{source} | {}t | {}", task.turn_count, task.title)
    } else {
        let project = task_project_name(task).unwrap_or("-");
        format!(
            "{project} | {source} | {}t | {}",
            task.turn_count, task.title
        )
    };
    terminal_safe_text(&label)
}

fn task_project_name(task: &TaskRecord) -> Option<&str> {
    task.cwd.as_deref()?.file_name()?.to_str()
}

fn format_tokens(tokens: TokenUsage) -> String {
    let value = tokens.total_tokens as f64;
    if value >= 1_000_000_000.0 {
        format!("{:.2}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        tokens.total_tokens.to_string()
    }
}

fn count_statuses(tasks: &[TaskRecord]) -> (usize, usize, usize) {
    let active = tasks.iter().filter(|task| task.status.is_active()).count();
    let completed = tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Completed | TaskStatus::Idle))
        .count();
    let uncertain = tasks.len().saturating_sub(active + completed);
    (active, completed, uncertain)
}

fn format_estimated_quota(value: f64, confidence: Confidence) -> String {
    match confidence {
        Confidence::Unknown => "-".to_string(),
        Confidence::Low | Confidence::Medium | Confidence::High => format!("~{value:.1}%"),
    }
}

fn status_evidence(provenance: Provenance, confidence: Confidence) -> String {
    let provenance = provenance_label(provenance);
    let confidence = match confidence {
        Confidence::High => "H",
        Confidence::Medium => "M",
        Confidence::Low => "L",
        Confidence::Unknown => "?",
    };
    format!("{provenance}/{confidence}")
}

fn provenance_label(provenance: Provenance) -> &'static str {
    match provenance {
        Provenance::Live => "LIVE",
        Provenance::ServerSnapshot => "SERVER",
        Provenance::LocalExact => "EXACT",
        Provenance::Inferred => "INFER",
        Provenance::Estimated => "EST",
        Provenance::Stale => "STALE",
        Provenance::Unknown => "UNK",
    }
}

struct TerminalGuard;

#[cfg(not(windows))]
const BUTTON_MOUSE_CAPTURE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h";
#[cfg(not(windows))]
const BUTTON_MOUSE_CAPTURE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1002l\x1b[?1000l";

fn enable_button_mouse_capture<W: Write>(writer: &mut W) -> io::Result<()> {
    #[cfg(windows)]
    execute!(writer, EnableMouseCapture)?;
    #[cfg(not(windows))]
    {
        writer.write_all(BUTTON_MOUSE_CAPTURE_ENABLE)?;
        writer.flush()?;
    }
    Ok(())
}

fn disable_button_mouse_capture<W: Write>(writer: &mut W) -> io::Result<()> {
    #[cfg(windows)]
    execute!(writer, DisableMouseCapture)?;
    #[cfg(not(windows))]
    {
        writer.write_all(BUTTON_MOUSE_CAPTURE_DISABLE)?;
        writer.flush()?;
    }
    Ok(())
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        let entered = execute!(stdout, EnterAlternateScreen)
            .and_then(|()| enable_button_mouse_capture(&mut stdout));
        if let Err(error) = entered {
            let _ = disable_button_mouse_capture(&mut stdout);
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = disable_button_mouse_capture(&mut stdout);
        let _ = execute!(stdout, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests;
