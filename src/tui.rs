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
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
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
mod text;

use geometry::{reveal_offset, scale_rounded, scroll_offset, scrollbar_geometry};
use text::{
    byte_index_at_char, compact_search_text, search_cursor_window, short_thread_id,
    truncate_display_text, truncate_middle_display_text,
};

use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, AttributionSummary, Confidence, ModelUsage, Provenance, Snapshot, TaskRecord,
    TaskStatus, TokenUsage, TurnRecord, TurnStatus, WindowAnalysis, WindowUsage,
    terminal_safe_text,
};
use crate::history::{HistoryData, HistoryObservation, HistoryStore};
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
use crate::ui_state::{
    UiState, UiStateStore, UiTaskListMode, UiTaskSourceFilter, UiTheme, UiView, UiWindowScope,
};

const LOCAL_REFRESH: Duration = Duration::from_secs(2);
const ACCOUNT_REFRESH: Duration = Duration::from_secs(45);
const HISTORY_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const HISTORY_VIEW_DAYS: i64 = 8;
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
    Health,
}

impl From<UiView> for View {
    fn from(value: UiView) -> Self {
        match value {
            UiView::Overview => Self::Overview,
            UiView::Trends => Self::Trends,
            UiView::Health => Self::Health,
        }
    }
}

impl From<View> for UiView {
    fn from(value: View) -> Self {
        match value {
            View::Overview => Self::Overview,
            View::Trends => Self::Trends,
            View::Health => Self::Health,
        }
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
            Self::HalfHour => "Half-hour",
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
    partial: bool,
}

#[derive(Clone, Copy)]
struct TrendSeries<'a> {
    name: &'static str,
    points: &'a [TrendPoint],
    color: Color,
}

#[derive(Clone, Copy)]
enum TrendValueKind {
    Percent,
    Tokens,
}

#[derive(Clone, Copy)]
enum TrendGraphKind {
    Line { maximum_gap: chrono::Duration },
    Bar { expected_step: chrono::Duration },
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

fn attribution_for_scope(snapshot: &Snapshot, scope: WindowScope) -> Option<&AttributionSummary> {
    window_analysis(snapshot, scope)
        .map(|analysis| &analysis.attribution)
        .or_else(|| {
            (scope == WindowScope::FiveHours && has_legacy_codex_window(snapshot))
                .then_some(&snapshot.attribution)
        })
}

fn task_usage_for_scope(snapshot: &Snapshot, scope: WindowScope, task: &TaskRecord) -> WindowUsage {
    if let Some(analysis) = window_analysis(snapshot, scope) {
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
        }
    } else {
        WindowUsage::default()
    }
}

fn turn_usage_for_scope(snapshot: &Snapshot, scope: WindowScope, turn: &TurnRecord) -> WindowUsage {
    if let Some(analysis) = window_analysis(snapshot, scope) {
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
) -> WindowUsage {
    if window_only {
        task_usage_for_scope(snapshot, scope, task)
    } else {
        WindowUsage {
            token_usage: task.token_usage,
            local_token_share_percent: task.local_token_share_percent,
            estimated_quota_percent: task.estimated_quota_percent,
            quota_confidence: task.quota_confidence,
        }
    }
}

fn aggregate_task_row_usage(
    snapshot: &Snapshot,
    scope: WindowScope,
    row: &TaskListRow,
    window_only: bool,
) -> WindowUsage {
    let Some(task) = snapshot.tasks.get(row.index) else {
        return WindowUsage::default();
    };
    if window_only
        && !row.hidden_descendants.is_empty()
        && let Some(analysis) = window_analysis(snapshot, scope)
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

    let mut aggregate = task_record_usage(snapshot, scope, task, window_only);
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
        let usage = task_record_usage(snapshot, scope, descendant, window_only);
        aggregate.token_usage.add_assign(usage.token_usage);
        aggregate.local_token_share_percent += usage.local_token_share_percent;
        aggregate.estimated_quota_percent += usage.estimated_quota_percent;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TurnControlsHitbox {
    back_tasks: Rect,
    search: Rect,
    clear_search: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewTabsHitbox {
    tabs: [Rect; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowControlsHitbox {
    toggle_turns: Rect,
    toggle_models: Rect,
    scopes: [Rect; 2],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TrendControlsHitbox {
    sections: [Rect; 3],
    previous_day: Rect,
    next_day: Rect,
    now: Rect,
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
    half_hour_bounds: [DateTime<Utc>; 2],
    weekly_history_present: bool,
    half_hour_history_present: bool,
    history_warning_count: usize,
    history_read_only: bool,
}

struct TrendPanelSpec<'a> {
    title: &'a str,
    graph_kind: TrendGraphKind,
    value_kind: TrendValueKind,
    fixed_y_bounds: Option<[f64; 2]>,
    fixed_x_bounds: Option<[DateTime<Utc>; 2]>,
    history_warning_count: usize,
    history_read_only: bool,
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
}

impl Drop for RefreshWorker {
    fn drop(&mut self) {
        self.join();
    }
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
    const ALL: [Self; 3] = [Self::Overview, Self::Trends, Self::Health];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Trends => "Trends",
            Self::Health => "Other",
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Overview => '1',
            Self::Trends => '2',
            Self::Health => '3',
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Trends => 1,
            Self::Health => 2,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Overview => Self::Trends,
            Self::Trends => Self::Health,
            Self::Health => Self::Overview,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Overview => Self::Health,
            Self::Trends => Self::Overview,
            Self::Health => Self::Trends,
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
    trend_controls_hitbox: Option<TrendControlsHitbox>,
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
    last_account_refresh: Instant,
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
            trend_controls_hitbox: None,
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
            last_account_refresh: Instant::now(),
        }
    }

    fn apply_ui_state(&mut self, state: &UiState, theme_override: Option<Theme>) {
        self.theme = theme_override.unwrap_or_else(|| state.theme.into());
        self.view = state.view.into();
        self.window_scope = state.window_scope.into();
        self.turns_default_visible = state.turns_visible;
        self.turns_temporarily_visible = false;
        self.models_visible = state.models_visible;
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
        self.history = history;
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
        if refreshed_account {
            self.last_account_refresh = Instant::now();
        }
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

    fn set_view(&mut self, view: View) {
        if self.view != view && self.turns_temporarily_visible {
            self.close_temporary_turns();
        }
        if view != View::Overview {
            self.close_temporary_turns();
            self.transition_to_tasks();
        }
        self.view = view;
    }

    fn set_window_scope(&mut self, scope: WindowScope) {
        self.window_scope = scope;
    }

    fn set_trend_section(&mut self, section: TrendSection) {
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

    fn show_previous_trend_day(&mut self) {
        let maximum = u16::try_from(HISTORY_VIEW_DAYS.saturating_sub(1)).unwrap_or(u16::MAX);
        self.trend_day_offset = self.trend_day_offset.saturating_add(1).min(maximum);
    }

    fn show_next_trend_day(&mut self) {
        self.trend_day_offset = self.trend_day_offset.saturating_sub(1);
    }

    fn show_current_trend_day(&mut self) {
        self.trend_day_offset = 0;
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
        }
    }

    fn begin_scrollbar_drag_at(&mut self, column: u16, row: u16) -> bool {
        let Some((target, hitbox)) = [ScrollTarget::Turns, ScrollTarget::Tasks]
            .into_iter()
            .find_map(|target| {
                self.scrollbar_hitbox(target)
                    .filter(|hitbox| rect_contains(hitbox.track, column, row))
                    .map(|hitbox| (target, hitbox))
            })
        else {
            return false;
        };
        self.accept_active_search();
        match target {
            ScrollTarget::Tasks => self.transition_to_tasks(),
            ScrollTarget::Turns => self.focus = Focus::Turns,
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
            if app.activate_view_at(event.column, event.row)
                || app.activate_window_control_at(event.column, event.row)
                || app.activate_trend_control_at(event.column, event.row)
                || app.activate_task_control_at(event.column, event.row)
                || app.activate_turn_control_at(event.column, event.row)
                || app.activate_task_tree_marker_at(event.column, event.row)
                || app.begin_scrollbar_drag_at(event.column, event.row)
            {
                true
            } else {
                let activate_selected_task =
                    app.focus == Focus::Tasks && app.selected_task_record().is_some();
                let previously_selected_task = app.selected_task;
                app.accept_active_search();
                if app.select_turn_at(event.column, event.row) {
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
        MouseEventKind::Drag(MouseButton::Left) => app.drag_scrollbar_to(event.row),
        MouseEventKind::Up(MouseButton::Left) => app.scroll_drag.take().is_some(),
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let down = matches!(event.kind, MouseEventKind::ScrollDown);
            if app
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
    let mut tabs = [Rect::default(); 3];
    let mut x = area.x;
    for (position, view) in View::ALL.into_iter().enumerate() {
        let width = UnicodeWidthStr::width(TAB_PADDING)
            + 2
            + UnicodeWidthStr::width(view.label())
            + UnicodeWidthStr::width(TAB_PADDING);
        let width = u16::try_from(width).unwrap_or(u16::MAX);
        tabs[view.index()] = clipped_horizontal_hitbox(area, x, width);
        x = x.saturating_add(width);
        if position + 1 < View::ALL.len() {
            x = x.saturating_add(
                u16::try_from(UnicodeWidthStr::width(TAB_DIVIDER)).unwrap_or(u16::MAX),
            );
        }
    }
    ViewTabsHitbox { tabs }
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

    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Esc => app.open_quit_confirmation(),
        KeyCode::Tab | KeyCode::Right => app.set_view(app.view.next()),
        KeyCode::BackTab | KeyCode::Left => app.set_view(app.view.previous()),
        KeyCode::Char('1') => app.set_view(View::Overview),
        KeyCode::Char('2') => app.set_view(View::Trends),
        KeyCode::Char('3') => app.set_view(View::Health),
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
        collect_snapshot_cached(config, None, true, &mut cache)
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
    let mut history_store = HistoryStore::discover(&config.codex_home);
    let (history, recorder_health) = stage_and_load_history(
        &mut history_store,
        &initial.history_observation,
        initial.snapshot.as_of,
        &config.perf_log,
        true,
    );
    history_span.finish_with(|| {
        format!(
            "quota_points={} half_hour_buckets={} warnings={} read_only={}",
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
    let total_started = Instant::now();
    let stage_started = Instant::now();
    store.stage(observation);
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
    metrics.half_hour_buckets = u64::try_from(history.half_hour_buckets.len()).unwrap_or(u64::MAX);
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
    metrics.half_hour_buckets = u64::try_from(history.half_hour_buckets.len()).unwrap_or(u64::MAX);
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
    let aligned_seconds = now.timestamp().div_euclid(30 * 60) * 30 * 60;
    DateTime::from_timestamp(aligned_seconds, 0).unwrap_or(now)
        - ChronoDuration::days(HISTORY_VIEW_DAYS)
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

        if event::poll(next_run_loop_poll_timeout(app, Instant::now()))? {
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
                refresh_worker.join();
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

        if !app.worker_running && app.last_local_refresh.elapsed() >= LOCAL_REFRESH {
            let refresh_account =
                !config.offline && app.last_account_refresh.elapsed() >= ACCOUNT_REFRESH;
            let worker_config = config.clone();
            let cached_account = app.account.clone();
            let worker_sender = channels.refresh_sender.clone();
            let worker_cache = Arc::clone(&rollout_cache);
            let worker_history = Arc::clone(&history_store);
            app.worker_running = true;
            refresh_worker.start(thread::spawn(move || {
                let result = {
                    let mut cache = worker_cache
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if refresh_account {
                        Some(collect_snapshot_cached(
                            &worker_config,
                            Some(cached_account),
                            true,
                            &mut cache,
                        ))
                    } else {
                        collect_snapshot_cached_if_changed(
                            &worker_config,
                            Some(cached_account),
                            &mut cache,
                        )
                    }
                };
                let history_and_recorder = {
                    let mut history_store = worker_history
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match result.as_ref() {
                        Some(result) => Some(stage_and_load_history(
                            &mut history_store,
                            &result.history_observation,
                            result.snapshot.as_of,
                            &worker_config.perf_log,
                            false,
                        )),
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
                    refreshed_account: refresh_account,
                });
            }));
        }
        config.perf_log.maybe_sample();
    }
}

fn mouse_event_requests_redraw(kind: MouseEventKind, handled: bool) -> bool {
    !matches!(kind, MouseEventKind::Moved)
        && (handled || matches!(kind, MouseEventKind::Down(MouseButton::Left)))
}

fn next_run_loop_poll_timeout(app: &App, now: Instant) -> Duration {
    let local_refresh_wait =
        LOCAL_REFRESH.saturating_sub(now.saturating_duration_since(app.last_local_refresh));
    let mut timeout = if app.worker_running {
        BACKGROUND_CHANNEL_POLL
    } else {
        local_refresh_wait
    };
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
    app.trend_controls_hitbox = None;
    app.view_tabs_hitbox = None;
    app.task_scrollbar_hitbox = None;
    app.turn_scrollbar_hitbox = None;
    app.quit_confirmation_hitbox = None;
    app.resume_confirmation_hitbox = None;
    let palette = app.theme.palette();
    frame.render_widget(Block::default().style(app.theme.base_style()), area);
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let titles = View::ALL
        .into_iter()
        .map(|view| {
            let selected = view == app.view;
            let shortcut_active = app.shortcuts_active();
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
                    view.label(),
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
    app.view_tabs_hitbox = Some(view_tabs_hitbox(root[0]));
    frame.render_widget(tabs, root[0]);
    if app.view == View::Overview {
        app.window_controls_hitbox = Some(render_overview_controls(frame, root[0], app));
    }

    match app.view {
        View::Overview => render_overview(frame, root[1], app),
        View::Trends => render_trends_at(frame, root[1], app, now),
        View::Health => render_health(frame, root[1], app),
    };
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
    let palette = app.theme.palette();
    let tabs = view_tabs_hitbox(area);
    let start_x = View::ALL
        .last()
        .map(|view| tabs.tabs[view.index()].right())
        .unwrap_or(area.x);
    let remaining = usize::from(area.right().saturating_sub(start_x));
    let full_width = UnicodeWidthStr::width(" | [V]Turns [M]Models [5h] [Week]");
    let compact = remaining < full_width;
    let separator = if compact { " " } else { TAB_DIVIDER };
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
    let mut constraints = vec![Constraint::Length(quota_height)];
    constraints.push(Constraint::Min(9));
    if app.models_visible {
        let desired_height = if compact { 8 } else { 10 };
        let model_height = if compact && quota_height > base_quota_height {
            desired_height.min(area.height.saturating_sub(quota_height).saturating_sub(9))
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
    if app.models_visible {
        render_models(frame, rows[row_index], app);
    }
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
        + UnicodeWidthStr::width("[[]Prev")
        + UnicodeWidthStr::width("[]]Next")
        + UnicodeWidthStr::width("[N]Now")
        + 5;
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
        TrendSection::HalfHour => "]Half-hour",
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
    let half_hour_bounds = trend_day_bounds(now, app.trend_day_offset);
    let weekly_reset = app.history.latest_weekly_reset();

    let weekly_cumulative = weekly_reset
        .map(|reset| app.history.weekly_cumulative_series(reset))
        .unwrap_or_default();
    let weekly_history_present = weekly_cumulative.len() > 1;
    let weekly_tokens = weekly_cumulative
        .iter()
        .map(|point| TrendPoint {
            at: point.at,
            value: point.token_usage.total_tokens as f64,
            partial: !point.partial_reasons.is_empty(),
        })
        .collect();
    let weekly_estimated = weekly_cumulative
        .iter()
        .filter_map(|point| {
            point.estimated_quota_percent.map(|value| TrendPoint {
                at: point.at,
                value,
                partial: !point.partial_reasons.is_empty(),
            })
        })
        .collect();

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
            at: bucket.starts_at + ChronoDuration::minutes(15),
            value: bucket.token_usage.total_tokens as f64,
            partial: !bucket.partial_reasons.is_empty(),
        })
        .collect();
    let half_hour_estimated = half_hour_estimated_trend(&app.history, half_hour_bounds);

    PreparedTrendData {
        five_hour_remaining,
        weekly_remaining,
        weekly_tokens,
        weekly_estimated,
        half_hour_tokens,
        half_hour_estimated,
        half_hour_bounds,
        weekly_history_present,
        half_hour_history_present,
        history_warning_count: app.history.warnings.len(),
        history_read_only: app.history.read_only,
    }
}

fn half_hour_estimated_trend(history: &HistoryData, bounds: [DateTime<Utc>; 2]) -> Vec<TrendPoint> {
    let mut points = BTreeMap::new();
    for reset in weekly_resets_overlapping(history, bounds) {
        for point in history
            .estimated_half_hour_series(reset)
            .into_iter()
            .filter(|point| point.starts_at >= bounds[0] && point.starts_at < bounds[1])
        {
            let Some(value) = point.estimated_quota_percent else {
                continue;
            };
            let at = point.starts_at + (point.ends_at - point.starts_at) / 2;
            let candidate = TrendPoint {
                at,
                value,
                partial: !point.partial_reasons.is_empty(),
            };
            points
                .entry(at)
                .and_modify(|existing: &mut TrendPoint| {
                    if existing.partial && !candidate.partial {
                        *existing = candidate;
                    }
                })
                .or_insert(candidate);
        }
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
            partial: false,
        })
        .collect()
}

fn trend_day_bounds(as_of: DateTime<Utc>, day_offset: u16) -> [DateTime<Utc>; 2] {
    let shifted = as_of - ChronoDuration::days(i64::from(day_offset));
    let end_seconds = shifted
        .timestamp()
        .saturating_add(30 * 60 - 1)
        .div_euclid(30 * 60)
        .saturating_mul(30 * 60);
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

    if compact {
        match app.trend_section {
            TrendSection::Remaining => render_remaining_trend_panel(frame, body, &data, app.theme),
            TrendSection::Weekly => {
                let panels = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(body);
                render_weekly_token_trend_panel(frame, panels[0], &data, app.theme);
                render_weekly_estimated_trend_panel(frame, panels[1], &data, app.theme);
            }
            TrendSection::HalfHour => {
                let panels = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(body);
                render_half_hour_token_trend_panel(frame, panels[0], &data, app.theme);
                render_half_hour_estimated_trend_panel(frame, panels[1], &data, app.theme);
            }
        }
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(body);
    render_remaining_trend_panel(frame, rows[0], &data, app.theme);
    let weekly = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    render_weekly_token_trend_panel(frame, weekly[0], &data, app.theme);
    render_weekly_estimated_trend_panel(frame, weekly[1], &data, app.theme);
    let half_hour = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[2]);
    render_half_hour_token_trend_panel(frame, half_hour[0], &data, app.theme);
    render_half_hour_estimated_trend_panel(frame, half_hour[1], &data, app.theme);
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
) {
    let palette = theme.palette();
    render_time_series_panel(
        frame,
        area,
        &[
            TrendSeries {
                name: "5h",
                points: &data.five_hour_remaining,
                color: palette.accent,
            },
            TrendSeries {
                name: "Week",
                points: &data.weekly_remaining,
                color: palette.warning,
            },
        ],
        TrendPanelSpec {
            title: "Quota Remaining",
            graph_kind: TrendGraphKind::Line {
                maximum_gap: ChronoDuration::minutes(15),
            },
            value_kind: TrendValueKind::Percent,
            fixed_y_bounds: Some([0.0, 100.0]),
            fixed_x_bounds: None,
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            theme,
        },
    );
}

fn render_weekly_token_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
) {
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "Tokens",
            points: &data.weekly_tokens,
            color: theme.palette().accent,
        }],
        TrendPanelSpec {
            title: "Weekly Local Tokens",
            graph_kind: TrendGraphKind::Line {
                maximum_gap: ChronoDuration::minutes(45),
            },
            value_kind: TrendValueKind::Tokens,
            fixed_y_bounds: None,
            fixed_x_bounds: None,
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            theme,
        },
    );
}

fn render_weekly_estimated_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
) {
    if data.weekly_estimated.is_empty() && data.weekly_history_present {
        let title = trend_panel_status_title(
            "Weekly ~EST Usage",
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
        return;
    }
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "~EST",
            points: &data.weekly_estimated,
            color: theme.palette().warning,
        }],
        TrendPanelSpec {
            title: "Weekly ~EST Usage",
            graph_kind: TrendGraphKind::Line {
                maximum_gap: ChronoDuration::minutes(45),
            },
            value_kind: TrendValueKind::Percent,
            fixed_y_bounds: Some([0.0, 100.0]),
            fixed_x_bounds: None,
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            theme,
        },
    );
}

fn render_half_hour_token_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
) {
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "Tokens",
            points: &data.half_hour_tokens,
            color: theme.palette().accent,
        }],
        TrendPanelSpec {
            title: "30m Local Tokens",
            graph_kind: TrendGraphKind::Bar {
                expected_step: ChronoDuration::minutes(30),
            },
            value_kind: TrendValueKind::Tokens,
            fixed_y_bounds: None,
            fixed_x_bounds: Some(data.half_hour_bounds),
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            theme,
        },
    );
}

fn render_half_hour_estimated_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    data: &PreparedTrendData,
    theme: Theme,
) {
    if data.half_hour_estimated.is_empty() && data.half_hour_history_present {
        let title = trend_panel_status_title(
            "30m ~EST Usage",
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
        return;
    }
    render_time_series_panel(
        frame,
        area,
        &[TrendSeries {
            name: "~EST",
            points: &data.half_hour_estimated,
            color: theme.palette().warning,
        }],
        TrendPanelSpec {
            title: "30m ~EST Usage",
            graph_kind: TrendGraphKind::Bar {
                expected_step: ChronoDuration::minutes(30),
            },
            value_kind: TrendValueKind::Percent,
            fixed_y_bounds: None,
            fixed_x_bounds: Some(data.half_hour_bounds),
            history_warning_count: data.history_warning_count,
            history_read_only: data.history_read_only,
            theme,
        },
    );
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
) {
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
        return;
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

    let mut prepared = Vec::with_capacity(series.len());
    let mut gap_count = 0_usize;
    for series in series {
        let (segments, gaps) = prepare_trend_segments(series.points, spec.graph_kind);
        prepared.push(segments);
        gap_count = gap_count.saturating_add(gaps);
    }
    let mut datasets = Vec::new();
    for (series, segments) in series.iter().zip(&prepared) {
        for (segment_index, segment) in segments.iter().enumerate() {
            let graph_type = match spec.graph_kind {
                TrendGraphKind::Line { .. } if segment.len() > 1 => GraphType::Line,
                TrendGraphKind::Line { .. } => GraphType::Scatter,
                TrendGraphKind::Bar { .. } => GraphType::Bar,
            };
            let marker = match spec.graph_kind {
                TrendGraphKind::Line { .. } => Marker::Braille,
                TrendGraphKind::Bar { .. } => Marker::Bar,
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

    let palette = spec.theme.palette();
    let x_labels = trend_time_axis_labels(minimum_time, maximum_time, area.width);
    let y_labels = vec![
        format_trend_axis_value(y_bounds[0], spec.value_kind),
        format_trend_axis_value(y_bounds[1], spec.value_kind),
    ];
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

    let chart = Chart::new(datasets)
        .style(spec.theme.base_style())
        .block(panel(&panel_title, spec.theme))
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
    frame.render_widget(chart, area);
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

fn reset_expiry_gauge_inner_width(
    snapshot: &Snapshot,
    area_width: u16,
    reminder: ResetExpiryReminder,
) -> Option<u16> {
    let windows = snapshot
        .limits
        .iter()
        .flat_map(|bucket| {
            [bucket.primary.as_ref(), bucket.secondary.as_ref()]
                .into_iter()
                .flatten()
                .map(move |window| (bucket, window))
        })
        .collect::<Vec<_>>();
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
    let Some(reminder) = reset_expiry_reminder(snapshot) else {
        return base_height;
    };
    let Some(inner_width) = reset_expiry_gauge_inner_width(snapshot, area_width, reminder) else {
        return base_height;
    };
    let alert_height = u16::try_from(reset_expiry_gauge_alert_lines(reminder, inner_width).len())
        .unwrap_or(u16::MAX);
    base_height.max(alert_height.saturating_mul(2).saturating_add(3))
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

    render_resets(frame, rows[2], &app.snapshot, app.theme);

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

fn render_resets(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, theme: Theme) {
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
        None => "Resets · credits unavailable".to_string(),
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
    let windows = snapshot
        .limits
        .iter()
        .flat_map(|bucket| {
            [bucket.primary.as_ref(), bucket.secondary.as_ref()]
                .into_iter()
                .flatten()
                .map(move |window| (bucket, window))
        })
        .collect::<Vec<_>>();

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
    let task_column = task_table_columns(table_inner)[3];
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
        .filter_map(|row| app.snapshot.tasks.get(row.index).map(|task| (task, row)))
        .map(|(task, row)| {
            let usage = aggregate_task_row_usage(&app.snapshot, app.window_scope, row, window_only);
            let tokens = usage.token_usage;
            let local_share = usage.local_token_share_percent;
            let estimated_quota = usage.estimated_quota_percent;
            let quota_confidence = usage.quota_confidence;
            let tone = task_status_tone(task.status);
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
                spans.push(Span::raw(task_display_label(task, row.depth > 0)));
                Cell::from(Line::from(spans))
            } else {
                Cell::from(task_display_label(task, false))
            };
            Row::new([
                Cell::from(format!("{} {}", status_marker(tone), format_tokens(tokens))),
                Cell::from(format!("{local_share:.1}%")),
                Cell::from(format_estimated_quota(estimated_quota, quota_confidence)),
                task_cell,
            ])
            .style(status_tone_style(tone, theme))
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        task_rows,
        [
            Constraint::Length(TASK_TOKENS_WIDTH),
            Constraint::Length(TASK_TOKEN_SHARE_WIDTH),
            Constraint::Length(TASK_QUOTA_WIDTH),
            Constraint::Min(12),
        ],
    )
    .flex(Flex::Legacy)
    .column_spacing(TASK_COLUMN_SPACING)
    .header(table_header(
        [
            "TOKENS",
            if window_only {
                app.window_scope.token_share_header()
            } else {
                WindowScope::FiveHours.token_share_header()
            },
            if window_only {
                app.window_scope.quota_header()
            } else {
                WindowScope::FiveHours.quota_header()
            },
            "TASK",
        ],
        app.theme,
    ))
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
    let start = x.max(inner_left).min(inner_right);
    let end = x.saturating_add(width).min(inner_right);
    Rect::new(
        start,
        area.y,
        end.saturating_sub(start),
        u16::from(area.height > 0),
    )
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
    let show_effort_column = table_area.width >= 72;
    let model_column_width = if show_effort_column {
        16
    } else {
        usize::from(table_area.width.saturating_sub(38).clamp(12, 17))
    };
    let theme = app.theme;
    let rows = turns.iter().skip(offset).map(|turn| {
        let usage = turn_usage_for_scope(&app.snapshot, app.window_scope, turn);
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
        let message = terminal_safe_text(turn.message_preview.as_deref().unwrap_or("-"));
        let tone = turn_status_tone(turn.status);
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
        cells.extend([
            Cell::from(format!("{} {}", status_marker(tone), format_tokens(tokens))),
            Cell::from(format!("{local_share:.1}%")),
            Cell::from(format_estimated_quota(estimated_quota, quota_confidence)),
        ]);
        Row::new(cells).style(status_tone_style(tone, theme))
    });
    let (constraints, header) = if show_effort_column {
        (
            vec![
                Constraint::Length(16),
                Constraint::Length(7),
                Constraint::Min(14),
                Constraint::Length(9),
                Constraint::Length(7),
                Constraint::Length(7),
            ],
            table_header(
                ["MODEL", "EFFORT", "MESSAGE", "TOKENS", "TOKEN%", "EST.Q"],
                theme,
            ),
        )
    } else {
        // Reserve nine message cells after borders, spacing, and numeric columns.
        (
            vec![
                Constraint::Length(u16::try_from(model_column_width).unwrap_or(u16::MAX)),
                Constraint::Min(9),
                Constraint::Length(9),
                Constraint::Length(7),
                Constraint::Length(7),
            ],
            table_header(
                ["EFFORT/MODEL", "MESSAGE", "TOKENS", "TOKEN%", "EST.Q"],
                theme,
            ),
        )
    };
    let table = Table::new(rows, constraints)
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
            .map(|turn| turn_usage_for_scope(&app.snapshot, detail_scope, turn))
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
    let lines = vec![
        Line::from(first_tokens),
        Line::from(second_tokens),
        Line::from(format!(
            "token.share={:.1}% · est.quota={}",
            if window_only {
                selected_window_usage.local_token_share_percent
            } else {
                turn.local_token_share_percent
            },
            estimated_quota
        )),
        Line::from(format!(
            "start={started} · end={completed} · duration={duration}"
        )),
        Line::from(format!("turn={}", terminal_safe_text(&turn.turn_id))),
        Line::from(format!("message={message}")),
    ];
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
                "Tokens {} · EST ~{:.2}pp · codex gauge × price-weighted share",
                format_tokens(attribution.local_token_usage),
                attribution.proxy_projected_percent,
            )
        } else {
            format!(
                "{} token total · ~{:.2}pp estimated · codex gauge × price-weighted share",
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
        "Price-weighted quota proxy · not server accounting".to_string()
    } else {
        "Price-weighted quota proxy, not server per-task accounting".to_string()
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

fn models_for_scope(snapshot: &Snapshot, scope: WindowScope) -> Vec<ModelUsage> {
    window_analysis(snapshot, scope)
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

fn render_models(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let theme = app.theme;
    let window_scope = app.window_scope;
    let analysis = window_analysis(&app.snapshot, window_scope);
    let attribution = attribution_for_scope(&app.snapshot, window_scope);
    let mut models = models_for_scope(&app.snapshot, window_scope);
    models.sort_by(|left, right| {
        right
            .token_usage
            .total_tokens
            .cmp(&left.token_usage.total_tokens)
            .then_with(|| left.model.cmp(&right.model))
    });

    let panel_inner = Block::default().borders(Borders::ALL).inner(area);
    let selected_partial = analysis
        .map(|analysis| analysis.partial)
        .unwrap_or(window_scope == WindowScope::FiveHours && app.snapshot.partial);
    let partial_reasons = analysis
        .map(|analysis| analysis.partial_reasons.as_slice())
        .unwrap_or_default();
    let attribution_lines = attribution_summary_lines(
        attribution,
        window_scope,
        selected_partial,
        partial_reasons,
        panel_inner.width < 100,
    );
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
    let visible_count = models.len().min(visible_capacity);
    let scope = attribution
        .and_then(|attribution| attribution.window.as_ref())
        .map(|window| window.label.clone());
    let mut title_suffix = scope.as_deref().unwrap_or(window_scope.label()).to_string();
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

    let rows = models.iter().take(visible_capacity).map(|model| {
        Row::new([
            Cell::from(terminal_safe_text(&model.model)),
            Cell::from(format_tokens(model.token_usage)),
            Cell::from(format!("{:.1}%", model.local_token_share_percent)),
            Cell::from(format_estimated_quota(
                model.estimated_quota_percent,
                model.quota_confidence,
            )),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(18),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .header(table_header(
        ["MODEL", "TOKENS", "TOKEN SHARE", "EST. QUOTA"],
        theme,
    ));
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

fn task_table_columns(area: Rect) -> [Rect; 4] {
    let [_highlight, columns] = Layout::horizontal([
        Constraint::Length(TASK_HIGHLIGHT_WIDTH),
        Constraint::Fill(0),
    ])
    .areas(area);
    Layout::horizontal([
        Constraint::Length(TASK_TOKENS_WIDTH),
        Constraint::Length(TASK_TOKEN_SHARE_WIDTH),
        Constraint::Length(TASK_QUOTA_WIDTH),
        Constraint::Min(12),
    ])
    .flex(Flex::Legacy)
    .spacing(TASK_COLUMN_SPACING)
    .areas(columns)
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
