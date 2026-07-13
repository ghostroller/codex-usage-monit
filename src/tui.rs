use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, HighlightSpacing, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, Confidence, Provenance, Snapshot, TaskRecord, TaskStatus, TokenUsage,
    TurnRecord, TurnStatus, WindowAnalysis, WindowUsage, terminal_safe_text,
};
use crate::rollout::RolloutCache;
use crate::snapshot::{CollectionResult, collect_snapshot_cached};

const LOCAL_REFRESH: Duration = Duration::from_secs(2);
const ACCOUNT_REFRESH: Duration = Duration::from_secs(45);
const MOUSE_SCROLL_LINES: usize = 3;
const PAGE_SCROLL_LINES: usize = 5;
const TAB_PADDING: &str = " ";
const TAB_DIVIDER: &str = " | ";
const ENTER_FOCUS_HINT: &str = "↵";
const BACK_FOCUS_HINT: &str = "←";
const TASK_TOKENS_WIDTH: u16 = 10;
const TASK_LOCAL_WIDTH: u16 = 8;
const TASK_QUOTA_WIDTH: u16 = 8;
const TASK_COLUMN_SPACING: u16 = 1;
const TASK_HIGHLIGHT_WIDTH: u16 = 1;
const TASK_TREE_MARKER_WIDTH: u16 = 3;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Overview,
    Window,
    Health,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WindowScope {
    #[default]
    FiveHours,
    Week,
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

    fn local_header(self) -> &'static str {
        match self {
            Self::FiveHours => "LOCAL5H",
            Self::Week => "LOCALWK",
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
    snapshot
        .window_analyses
        .iter()
        .find(|analysis| analysis.duration_mins == scope.duration_mins())
}

fn attribution_for_scope(
    snapshot: &Snapshot,
    scope: WindowScope,
) -> Option<&crate::domain::AttributionSummary> {
    window_analysis(snapshot, scope)
        .map(|analysis| &analysis.attribution)
        .or_else(|| {
            (scope == WindowScope::FiveHours && snapshot.attribution.window.is_some())
                .then_some(&snapshot.attribution)
        })
}

fn task_usage_for_scope(snapshot: &Snapshot, scope: WindowScope, task: &TaskRecord) -> WindowUsage {
    window_analysis(snapshot, scope)
        .and_then(|analysis| {
            analysis
                .threads
                .iter()
                .find(|usage| usage.thread_id == task.thread_id)
                .map(|usage| usage.usage)
        })
        .unwrap_or_else(|| {
            if scope == WindowScope::FiveHours {
                WindowUsage {
                    token_usage: task.window_token_usage,
                    local_token_share_percent: task.local_token_share_percent,
                    estimated_quota_percent: task.estimated_quota_percent,
                    quota_confidence: task.quota_confidence,
                }
            } else {
                WindowUsage::default()
            }
        })
}

fn turn_usage_for_scope(snapshot: &Snapshot, scope: WindowScope, turn: &TurnRecord) -> WindowUsage {
    window_analysis(snapshot, scope)
        .and_then(|analysis| {
            analysis
                .turns
                .iter()
                .find(|usage| usage.thread_id == turn.thread_id && usage.turn_id == turn.turn_id)
                .map(|usage| usage.usage)
        })
        .unwrap_or_else(|| {
            if scope == WindowScope::FiveHours {
                WindowUsage {
                    token_usage: turn.window_token_usage,
                    local_token_share_percent: turn.local_token_share_percent,
                    estimated_quota_percent: turn.estimated_quota_percent,
                    quota_confidence: turn.quota_confidence,
                }
            } else {
                WindowUsage::default()
            }
        })
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
    toggle_turns: Rect,
    toggle_tree: Rect,
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
    scopes: [Rect; 2],
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
    const ALL: [Self; 3] = [Self::Overview, Self::Window, Self::Health];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Window => "Window",
            Self::Health => "Data health",
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Overview => '1',
            Self::Window => '2',
            Self::Health => '3',
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Window => 1,
            Self::Health => 2,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Overview => Self::Window,
            Self::Window => Self::Health,
            Self::Health => Self::Overview,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Overview => Self::Health,
            Self::Window => Self::Overview,
            Self::Health => Self::Window,
        }
    }
}

struct App {
    snapshot: Snapshot,
    account: AccountSnapshot,
    theme: Theme,
    view: View,
    window_scope: WindowScope,
    focus: Focus,
    task_source_filter: TaskSourceFilter,
    task_list_mode: TaskListMode,
    collapsed_task_threads: HashSet<String>,
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
    view_tabs_hitbox: Option<ViewTabsHitbox>,
    task_scrollbar_hitbox: Option<ScrollbarHitbox>,
    turn_scrollbar_hitbox: Option<ScrollbarHitbox>,
    scroll_drag: Option<ScrollDrag>,
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
            theme,
            view: View::Overview,
            window_scope: WindowScope::FiveHours,
            focus: Focus::Tasks,
            task_source_filter: TaskSourceFilter::All,
            task_list_mode: TaskListMode::Flat,
            collapsed_task_threads: HashSet::new(),
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
            view_tabs_hitbox: None,
            task_scrollbar_hitbox: None,
            turn_scrollbar_hitbox: None,
            scroll_drag: None,
            turn_reveal_pending: false,
            worker_running: false,
            last_local_refresh: Instant::now(),
            last_account_refresh: Instant::now(),
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
                &self.collapsed_task_threads,
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

    fn close_temporary_turns(&mut self) {
        self.turns_temporarily_visible = false;
        if matches!(self.focus, Focus::Turns | Focus::TurnSearch) {
            self.focus = Focus::Tasks;
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
            self.focus = Focus::Tasks;
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
        self.focus = Focus::Tasks;
        if !self.turns_default_visible {
            self.turns_temporarily_visible = false;
        }
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
            if !self.turns_default_visible {
                self.turns_temporarily_visible = false;
            }
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
        self.turn_search_cursor = 0;
        self.turn_search_before_edit.clear();
        self.turn_search_restore_turn_id = None;
        self.reconcile_turn_filter(true, selected.as_deref());
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
            self.collapsed_task_threads.insert(thread_id)
        } else {
            self.collapsed_task_threads.remove(&thread_id)
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
        self.collapsed_task_threads
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
        if self.view != View::Health && self.selected_task_raw_turn_count() > 0 {
            let was_visible = self.turns_visible();
            self.turns_temporarily_visible = !self.turns_default_visible;
            if !was_visible && self.turns_visible() {
                self.task_reveal_pending = true;
            }
            self.focus = Focus::Turns;
            self.select_turn(self.selected_turn, true);
        }
    }

    fn focus_tasks(&mut self) {
        self.focus = Focus::Tasks;
        if !self.turns_default_visible {
            self.turns_temporarily_visible = false;
        }
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
        self.focus = Focus::Tasks;
        if !self.turns_default_visible {
            self.turns_temporarily_visible = false;
        }
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
        if rect_contains(hitbox.toggle_turns, column, row) {
            self.accept_active_search();
            self.toggle_turns_default_visibility();
            return true;
        }
        if rect_contains(hitbox.toggle_tree, column, row) {
            self.accept_active_search();
            self.toggle_task_list_mode();
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
        if view == View::Health {
            self.close_temporary_turns();
            self.focus = Focus::Tasks;
        }
        self.view = view;
    }

    fn set_window_scope(&mut self, scope: WindowScope) {
        self.window_scope = scope;
    }

    fn activate_window_control_at(&mut self, column: u16, row: u16) -> bool {
        if self.view != View::Window {
            return false;
        }
        let Some(hitbox) = self.window_controls_hitbox else {
            return false;
        };
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
            ScrollTarget::Tasks => {
                self.focus = Focus::Tasks;
                if !self.turns_default_visible {
                    self.turns_temporarily_visible = false;
                }
            }
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
    collapsed_task_threads: &HashSet<String>,
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
        && tasks
            .get(index)
            .is_some_and(|task| collapsed_task_threads.contains(&task.thread_id));
    rows.push(TaskListRow {
        index,
        prefix,
        depth: guides.len(),
        has_children,
        collapsed,
    });
    if collapsed {
        return;
    }

    let child_count = children[index].len();
    for (position, &child) in children[index].iter().enumerate() {
        guides.push(position + 1 == child_count);
        append_task_tree_rows(child, children, tasks, collapsed_task_threads, guides, rows);
        guides.pop();
    }
}

fn handle_mouse_event(app: &mut App, event: MouseEvent) -> bool {
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.scroll_drag = None;
            if app.activate_view_at(event.column, event.row)
                || app.activate_window_control_at(event.column, event.row)
                || app.activate_task_control_at(event.column, event.row)
                || app.activate_turn_control_at(event.column, event.row)
                || app.activate_task_tree_marker_at(event.column, event.row)
                || app.begin_scrollbar_drag_at(event.column, event.row)
            {
                true
            } else {
                app.accept_active_search();
                if app.select_turn_at(event.column, event.row) {
                    app.focus = Focus::Turns;
                    true
                } else if app.select_task_at(event.column, event.row) {
                    app.focus_tasks();
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

fn byte_index_at_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(value.len())
}

fn search_cursor_window(value: &str, cursor: usize, max_width: usize) -> (String, String, bool) {
    if max_width == 0 {
        return (String::new(), String::new(), false);
    }
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let content_width = max_width - 1;
    let left_target = content_width / 2;
    let mut left = cursor;
    let mut right = cursor;

    while left > 0 {
        let candidate = chars[left - 1..cursor].iter().collect::<String>();
        if UnicodeWidthStr::width(candidate.as_str()) > left_target {
            break;
        }
        left -= 1;
    }
    while right < chars.len() {
        let candidate = chars[left..right + 1].iter().collect::<String>();
        if UnicodeWidthStr::width(candidate.as_str()) > content_width {
            break;
        }
        right += 1;
    }
    while left > 0 {
        let candidate = chars[left - 1..right].iter().collect::<String>();
        if UnicodeWidthStr::width(candidate.as_str()) > content_width {
            break;
        }
        left -= 1;
    }

    (
        chars[left..cursor].iter().collect(),
        chars[cursor..right].iter().collect(),
        true,
    )
}

fn compact_search_text(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut start = chars.len();
    while start > 0 {
        let suffix = chars[start - 1..].iter().collect::<String>();
        let candidate = format!("<{suffix}");
        if UnicodeWidthStr::width(candidate.as_str()) > max_width {
            break;
        }
        start -= 1;
    }
    format!("<{}", chars[start..].iter().collect::<String>())
}

fn handle_key_event(app: &mut App, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
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
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab | KeyCode::Right => app.set_view(app.view.next()),
        KeyCode::BackTab | KeyCode::Left => app.set_view(app.view.previous()),
        KeyCode::Char('1') => app.set_view(View::Overview),
        KeyCode::Char('2') => app.set_view(View::Window),
        KeyCode::Char('3') => app.set_view(View::Health),
        KeyCode::Char('5') if app.view == View::Window => {
            app.set_window_scope(WindowScope::FiveHours);
        }
        KeyCode::Char('w' | 'W') if app.view == View::Window => {
            app.set_window_scope(WindowScope::Week);
        }
        KeyCode::Char('t' | 'T') => app.toggle_theme(),
        KeyCode::Char('/') | KeyCode::Char('f' | 'F') if app.view != View::Health => {
            match app.focus {
                Focus::Tasks => app.begin_task_search(),
                Focus::Turns => app.begin_turn_search(),
                Focus::TaskSearch | Focus::TurnSearch => {}
            }
        }
        KeyCode::Char('v' | 'V') if app.view != View::Health => {
            app.toggle_turns_default_visibility();
        }
        KeyCode::Char('r' | 'R') if app.view != View::Health => {
            app.toggle_task_list_mode();
        }
        KeyCode::Char('-')
            if app.view != View::Health
                && app.focus == Focus::Tasks
                && app.task_list_mode == TaskListMode::Tree =>
        {
            app.set_selected_task_collapsed(true);
        }
        KeyCode::Char('+')
            if app.view != View::Health
                && app.focus == Focus::Tasks
                && app.task_list_mode == TaskListMode::Tree =>
        {
            app.set_selected_task_collapsed(false);
        }
        KeyCode::Char('a' | 'A') if app.view != View::Health => {
            app.set_task_source_filter(TaskSourceFilter::All);
        }
        KeyCode::Char('d' | 'D') if app.view != View::Health => {
            app.set_task_source_filter(TaskSourceFilter::Desktop);
        }
        KeyCode::Char('s' | 'S') if app.view != View::Health => {
            app.set_task_source_filter(TaskSourceFilter::Subagent);
        }
        KeyCode::Char('c' | 'C') if app.view != View::Health => {
            app.set_task_source_filter(TaskSourceFilter::Cli);
        }
        KeyCode::Char(']') if app.view != View::Health => {
            app.cycle_task_source_filter(true);
        }
        KeyCode::Char('[') if app.view != View::Health => {
            app.cycle_task_source_filter(false);
        }
        KeyCode::Delete if app.view != View::Health => match app.focus {
            Focus::Tasks => app.clear_task_search(),
            Focus::Turns => app.clear_turn_search(),
            Focus::TaskSearch | Focus::TurnSearch => {}
        },
        KeyCode::Enter if app.view != View::Health && app.focus == Focus::Tasks => {
            app.focus_turns();
        }
        KeyCode::Backspace if app.view != View::Health && app.focus == Focus::Turns => {
            app.focus_tasks();
        }
        KeyCode::Down | KeyCode::Char('j') if app.view != View::Health => {
            app.select_next_focused();
        }
        KeyCode::Up | KeyCode::Char('k') if app.view != View::Health => {
            app.select_previous_focused();
        }
        KeyCode::Home if app.view != View::Health => app.select_first_focused(),
        KeyCode::End if app.view != View::Health => app.select_last_focused(),
        KeyCode::PageDown if app.view != View::Health => match app.focus {
            Focus::Tasks => app.scroll_tasks(true, PAGE_SCROLL_LINES),
            Focus::Turns => app.scroll_turns(true, PAGE_SCROLL_LINES),
            Focus::TaskSearch | Focus::TurnSearch => {}
        },
        KeyCode::PageUp if app.view != View::Health => match app.focus {
            Focus::Tasks => app.scroll_tasks(false, PAGE_SCROLL_LINES),
            Focus::Turns => app.scroll_turns(false, PAGE_SCROLL_LINES),
            Focus::TaskSearch | Focus::TurnSearch => {}
        },
        _ => {}
    }
    false
}

fn reveal_offset(offset: usize, selected: usize, item_count: usize, capacity: usize) -> usize {
    let max_offset = item_count.saturating_sub(capacity);
    let offset = offset.min(max_offset);
    if capacity == 0 || selected < offset {
        selected.min(max_offset)
    } else if selected >= offset.saturating_add(capacity) {
        selected
            .saturating_add(1)
            .saturating_sub(capacity)
            .min(max_offset)
    } else {
        offset
    }
}

fn scroll_offset(
    offset: usize,
    item_count: usize,
    capacity: usize,
    down: bool,
    lines: usize,
) -> usize {
    let max_offset = item_count.saturating_sub(capacity);
    if down {
        offset.saturating_add(lines).min(max_offset)
    } else {
        offset.saturating_sub(lines)
    }
}

fn scrollbar_geometry(
    track: Rect,
    item_count: usize,
    capacity: usize,
    offset: usize,
) -> Option<ScrollbarHitbox> {
    if track.width == 0 || track.height < 2 || capacity == 0 || item_count <= capacity {
        return None;
    }
    let track_height = usize::from(track.height);
    let thumb_height = track_height
        .saturating_mul(capacity)
        .div_ceil(item_count)
        .clamp(1, track_height - 1);
    let max_offset = item_count - capacity;
    let travel = track_height - thumb_height;
    let thumb_offset = scale_rounded(offset.min(max_offset), travel, max_offset);
    Some(ScrollbarHitbox {
        track,
        thumb: Rect::new(
            track.x,
            track
                .y
                .saturating_add(u16::try_from(thumb_offset).unwrap_or(u16::MAX)),
            1,
            u16::try_from(thumb_height).unwrap_or(track.height),
        ),
        max_offset,
    })
}

fn scale_rounded(value: usize, scale: usize, denominator: usize) -> usize {
    if denominator == 0 {
        return 0;
    }
    let denominator = denominator as u128;
    let scaled = ((value as u128) * (scale as u128) + denominator / 2) / denominator;
    usize::try_from(scaled).unwrap_or(usize::MAX)
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
    run_with_theme(config, Theme::Dark)
}

pub fn run_with_theme(config: CollectConfig, theme: Theme) -> Result<()> {
    let rollout_cache = Arc::new(Mutex::new(RolloutCache::new()));
    let initial = {
        let mut cache = rollout_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        collect_snapshot_cached(&config, None, true, &mut cache)
    };
    let mut app = App::new(initial, theme);
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (sender, receiver) = mpsc::channel::<(CollectionResult, bool)>();
    let result = run_loop(
        &mut terminal,
        &mut app,
        &config,
        &sender,
        &receiver,
        rollout_cache,
    );
    terminal.show_cursor()?;
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    config: &CollectConfig,
    sender: &mpsc::Sender<(CollectionResult, bool)>,
    receiver: &Receiver<(CollectionResult, bool)>,
    rollout_cache: Arc<Mutex<RolloutCache>>,
) -> Result<()> {
    loop {
        while let Ok((result, refreshed_account)) = receiver.try_recv() {
            app.replace(result, refreshed_account);
        }

        terminal.draw(|frame| render(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key_event(app, key) {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => {
                    handle_mouse_event(app, mouse);
                }
                _ => {}
            }
        }

        if !app.worker_running && app.last_local_refresh.elapsed() >= LOCAL_REFRESH {
            let refresh_account =
                !config.offline && app.last_account_refresh.elapsed() >= ACCOUNT_REFRESH;
            let worker_config = config.clone();
            let cached_account = app.account.clone();
            let worker_sender = sender.clone();
            let worker_cache = Arc::clone(&rollout_cache);
            app.worker_running = true;
            thread::spawn(move || {
                let result = {
                    let mut cache = worker_cache
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    collect_snapshot_cached(
                        &worker_config,
                        Some(cached_account),
                        refresh_account,
                        &mut cache,
                    )
                };
                let _ = worker_sender.send((result, refresh_account));
            });
        }
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.task_table_hitbox = None;
    app.turn_table_hitbox = None;
    app.task_controls_hitbox = None;
    app.task_tree_marker_hitboxes.clear();
    app.turn_controls_hitbox = None;
    app.window_controls_hitbox = None;
    app.view_tabs_hitbox = None;
    app.task_scrollbar_hitbox = None;
    app.turn_scrollbar_hitbox = None;
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
            let shortcut_active = !app.focus.is_search();
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

    match app.view {
        View::Overview => render_overview(frame, root[1], app),
        View::Window => render_window(frame, root[1], app),
        View::Health => render_health(frame, root[1], app),
    };
    if app
        .scroll_drag
        .is_some_and(|drag| app.scrollbar_hitbox(drag.target).is_none())
    {
        app.scroll_drag = None;
    }
}

fn render_overview(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let compact = area.height < 28;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if compact { 3 } else { 5 }),
            Constraint::Min(10),
            Constraint::Length(if compact { 4 } else { 7 }),
        ])
        .split(area);
    render_limits(frame, rows[0], &app.snapshot, app.theme);

    if app.turns_visible() {
        let narrow = area.width < 100;
        let body = Layout::default()
            .direction(if narrow {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if narrow {
                [Constraint::Percentage(42), Constraint::Percentage(58)]
            } else {
                [Constraint::Percentage(54), Constraint::Percentage(46)]
            })
            .split(rows[1]);
        render_tasks(frame, body[0], app, false);
        render_turns(frame, body[1], app, false);
    } else {
        render_tasks(frame, rows[1], app, false);
    }
    render_models(
        frame,
        rows[2],
        &app.snapshot,
        app.theme,
        WindowScope::FiveHours,
    );
}

fn render_window(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let compact = area.height < 30;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if compact { 3 } else { 5 }),
            Constraint::Length(if compact { 4 } else { 5 }),
            Constraint::Min(9),
            Constraint::Length(if compact { 4 } else { 7 }),
        ])
        .split(area);
    render_limits(frame, rows[0], &app.snapshot, app.theme);
    render_attribution(frame, rows[1], app);

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
            .split(rows[2]);
        render_tasks(frame, body[0], app, true);
        render_turns(frame, body[1], app, true);
    } else {
        render_tasks(frame, rows[2], app, true);
    }
    render_models(frame, rows[3], &app.snapshot, app.theme, app.window_scope);
}

fn render_health(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let palette = app.theme.palette();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(5),
            Constraint::Min(6),
        ])
        .split(area);

    let source_rows = app.snapshot.sources.iter().map(|source| {
        Row::new([
            Cell::from(terminal_safe_text(&source.source)),
            Cell::from(terminal_safe_text(&source.status)),
            Cell::from(
                source
                    .as_of
                    .with_timezone(&Local)
                    .format("%H:%M:%S")
                    .to_string(),
            ),
            Cell::from(terminal_safe_text(
                source.message.as_deref().unwrap_or_default(),
            )),
        ])
    });
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
            "Files  {}/{} scanned ({} truncated, {} unreadable)    Lines  {} parsed / {} skipped    Resets  {} ambiguous",
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
    frame.render_widget(
        Paragraph::new(stats_text).block(panel("Collection", app.theme)),
        rows[1],
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
        rows[2],
    );
}

fn render_limits(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, theme: Theme) {
    let palette = theme.palette();
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
            .map(|value| {
                value
                    .with_timezone(&Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".to_string());
        let reset_time = window
            .resets_at
            .map(|value| value.with_timezone(&Local).format("%H:%M").to_string())
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
        let gauge = Gauge::default()
            .block(panel(&title, theme))
            .gauge_style(Style::default().fg(color).bg(palette.gauge_track))
            .ratio((window.used_percent / 100.0).clamp(0.0, 1.0))
            .label(label);
        frame.render_widget(gauge, columns[index]);
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
            let usage = task_usage_for_scope(&app.snapshot, app.window_scope, task);
            let tokens = if window_only {
                usage.token_usage
            } else {
                task.token_usage
            };
            let local_share = if window_only {
                usage.local_token_share_percent
            } else {
                task.local_token_share_percent
            };
            let estimated_quota = if window_only {
                usage.estimated_quota_percent
            } else {
                task.estimated_quota_percent
            };
            let quota_confidence = if window_only {
                usage.quota_confidence
            } else {
                task.quota_confidence
            };
            let tone = task_status_tone(task.status);
            let task_cell = if tree_mode {
                let marker_style = Style::default().fg(palette.muted);
                let shortcut_style = if tasks_focused && row.index == app.selected_task {
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
            Constraint::Length(TASK_LOCAL_WIDTH),
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
                app.window_scope.local_header()
            } else {
                WindowScope::FiveHours.local_header()
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
        + UnicodeWidthStr::width("[V]Turns")
        + 1
        + UnicodeWidthStr::width("[R]Tree")
        + TaskSourceFilter::ALL
            .into_iter()
            .map(|filter| 1 + UnicodeWidthStr::width(filter.label(false)) + 2)
            .sum::<usize>()
        + 1
        + UnicodeWidthStr::width("Filter:")
        + 3;
    let compact = usize::from(area.width.saturating_sub(2)) < full_controls_width;
    let mut spans = vec![Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD),
    )];
    let enter_available = app.focus == Focus::Tasks
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

    spans.push(Span::raw(" "));
    title_x = title_x.saturating_add(1);
    let toggle_label = if compact { "[V]" } else { "[V]Turns" };
    let toggle_width = u16::try_from(UnicodeWidthStr::width(toggle_label)).unwrap_or(u16::MAX);
    let toggle_turns = title_hitbox(area, title_x, toggle_width);
    let toggle_style = if app.turns_default_visible {
        Style::default()
            .fg(palette.background)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let toggle_shortcut_style = if app.focus.is_search() {
        toggle_style
    } else if app.turns_default_visible {
        toggle_style.add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    };
    spans.push(Span::styled("[", toggle_style));
    spans.push(Span::styled("V", toggle_shortcut_style));
    spans.push(Span::styled(
        if compact { "]" } else { "]Turns" },
        toggle_style,
    ));
    title_x = title_x.saturating_add(toggle_width);

    spans.push(Span::raw(" "));
    title_x = title_x.saturating_add(1);
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
    let tree_shortcut_style = if app.focus.is_search() {
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

    let mut source_hitboxes = [Rect::default(); 4];
    let shortcuts_active = !app.focus.is_search();
    for filter in TaskSourceFilter::ALL {
        spans.push(Span::raw(" "));
        title_x = title_x.saturating_add(1);
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
    let clear_search = if !app.task_search.is_empty() && query_start < inner_right.saturating_sub(1)
    {
        Rect::new(inner_right - 1, area.y, 1, u16::from(area.height > 0))
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
        if app.focus == Focus::Tasks {
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
        search_right.saturating_sub(1)
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
        spans.push(Span::styled(
            "×",
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw(" "));
    }

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
    let border_color = if matches!(app.focus, Focus::Tasks | Focus::TaskSearch) {
        palette.accent
    } else {
        palette.border
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(spans))
        .title_bottom(Span::styled(status, Style::default().fg(palette.muted)));
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
        toggle_turns,
        toggle_tree,
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
    let back_available = app.focus == Focus::Turns;
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
    let clear_search = if !app.turn_search.is_empty() && inner_right > search_start {
        Rect::new(inner_right - 1, area.y, 1, u16::from(area.height > 0))
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
        if app.focus == Focus::Turns {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            search_style
        },
    ));
    spans.push(Span::styled("ilter:", search_style));
    let query_start = search_start.saturating_add("Filter:".len() as u16);
    let query_right = if clear_search.is_empty() {
        search_right
    } else {
        search_right.saturating_sub(1)
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
        spans.push(Span::styled(
            "×",
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::BOLD),
        ));
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
        .title(Line::from(spans))
        .title_bottom(status_legend(app.theme, area.width));
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
            Line::from(vec![
                Span::styled(
                    "FAST ",
                    Style::default()
                        .fg(theme.palette().warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(model),
            ])
        } else {
            Line::from(model)
        };
        if show_effort_column {
            cells.push(Cell::from(model));
            cells.push(Cell::from(effort));
            cells.push(Cell::from(message));
        } else {
            let compact_model = if turn.is_fast() {
                Line::from(vec![
                    Span::styled(
                        "FAST ",
                        Style::default()
                            .fg(theme.palette().warning)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "{effort}/{}",
                        terminal_safe_text(turn.model.as_deref().unwrap_or("unknown"))
                    )),
                ])
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
                ["MODEL", "EFFORT", "MESSAGE", "TOKENS", "LOCAL", "EST.Q"],
                theme,
            ),
        )
    } else {
        // Reserve nine message cells after borders, spacing, and numeric columns.
        let model_width = table_area.width.saturating_sub(38).clamp(12, 17);
        (
            vec![
                Constraint::Length(model_width),
                Constraint::Min(9),
                Constraint::Length(9),
                Constraint::Length(7),
                Constraint::Length(7),
            ],
            table_header(
                ["EFFORT/MODEL", "MESSAGE", "TOKENS", "LOCAL", "EST.Q"],
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
            "local={:.1}% · est.quota={} · confidence={}",
            if window_only {
                selected_window_usage.local_token_share_percent
            } else {
                turn.local_token_share_percent
            },
            estimated_quota,
            confidence_label(quota_confidence)
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
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

fn render_models(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &Snapshot,
    theme: Theme,
    window_scope: WindowScope,
) {
    let analysis = window_analysis(snapshot, window_scope);
    let attribution = attribution_for_scope(snapshot, window_scope);
    let models = analysis
        .map(|analysis| analysis.models.as_slice())
        .unwrap_or_else(|| {
            if window_scope == WindowScope::FiveHours {
                snapshot.models.as_slice()
            } else {
                &[]
            }
        });
    let scope = attribution
        .and_then(|attribution| attribution.window.as_ref())
        .map(|window| window.label.as_str());
    if models.is_empty() {
        let (title, message) = if let Some(scope) = scope {
            (
                format!("Models · {scope}"),
                format!("No local model usage in the current {scope} window"),
            )
        } else if window_scope == WindowScope::FiveHours
            && has_active_window(snapshot, WindowScope::Week.duration_mins())
        {
            (
                "Models · 5h unavailable".to_string(),
                "5h window unavailable; weekly reset-cycle data remains available".to_string(),
            )
        } else {
            (
                format!("Models · {} unavailable", window_scope.label()),
                format!("No active {} quota window", window_scope.label()),
            )
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(theme.palette().muted))
                .block(panel(&title, theme))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let visible_capacity = usize::from(area.height.saturating_sub(3));
    let mut models = models.iter().collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .token_usage
            .total_tokens
            .cmp(&left.token_usage.total_tokens)
            .then_with(|| left.model.cmp(&right.model))
    });
    let visible_count = models.len().min(visible_capacity);
    let mut title = format!("Models · {}", scope.unwrap_or("current window"));
    if visible_count < models.len() {
        title.push_str(&format!(" · top {visible_count}/{}", models.len()));
    }

    let rows = models.into_iter().take(visible_capacity).map(|model| {
        Row::new([
            Cell::from(terminal_safe_text(&model.model)),
            Cell::from(format_tokens(model.token_usage)),
            Cell::from(format!("{:.1}%", model.local_token_share_percent)),
            Cell::from(format_estimated_quota(
                model.estimated_quota_percent,
                model.quota_confidence,
            )),
            Cell::from(confidence_label(model.quota_confidence)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(18),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(8),
        ],
    )
    .header(table_header(
        ["MODEL", "TOKENS", "LOCAL SHARE", "EST. QUOTA", "CONF"],
        theme,
    ))
    .block(panel(&title, theme));
    frame.render_widget(table, area);
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

fn render_attribution(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let snapshot = &app.snapshot;
    let selected_analysis = window_analysis(snapshot, app.window_scope);
    let selected_partial = selected_analysis
        .map(|analysis| analysis.partial)
        .unwrap_or_else(|| app.window_scope == WindowScope::FiveHours && snapshot.partial);
    let attribution = attribution_for_scope(snapshot, app.window_scope);
    let (window, detail) = if let Some(attribution) = attribution {
        let window = attribution
            .window
            .as_ref()
            .map(|window| {
                format!(
                    "{} reset cycle · {:.1}% used · {} to {}",
                    window.label,
                    window.used_percent,
                    window.starts_at.with_timezone(&Local).format("%m-%d %H:%M"),
                    window.ends_at.with_timezone(&Local).format("%m-%d %H:%M")
                )
            })
            .unwrap_or_else(|| format!("{} reset cycle unavailable", app.window_scope.label()));
        let estimated_quota = if attribution.confidence == Confidence::Unknown {
            "-".to_string()
        } else {
            format!("{:.2}pp", attribution.estimated_assigned_percent)
        };
        let detail = format!(
            "{} local · +{:.2}pp observed · {} estimated · {:.2}pp unattributed · {:.0}% coverage · {}{}{}{}",
            format_tokens(attribution.local_token_usage),
            attribution.observed_delta_percent,
            estimated_quota,
            attribution.unattributed_percent,
            attribution.attribution_coverage_percent,
            confidence_label(attribution.confidence),
            if attribution.external_activity_possible {
                " · external possible"
            } else {
                ""
            },
            if attribution.settled {
                " · settled"
            } else {
                ""
            },
            if selected_partial { " · partial" } else { "" }
        );
        (window, detail)
    } else {
        (
            format!("{} reset cycle unavailable", app.window_scope.label()),
            "No active quota window with duration and reset time".to_string(),
        )
    };
    let (block, controls) = window_attribution_block(area, app);
    app.window_controls_hitbox = Some(controls);
    frame.render_widget(
        Paragraph::new(vec![Line::from(window), Line::from(detail)])
            .block(block)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn window_attribution_block(area: Rect, app: &App) -> (Block<'static>, WindowControlsHitbox) {
    let palette = app.theme.palette();
    let shortcuts_active = !app.focus.is_search();
    let mut spans = vec![Span::styled(
        " Attribution",
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD),
    )];
    let mut x = area.x.saturating_add(1 + " Attribution".len() as u16);
    let mut scopes = [Rect::default(); 2];
    for scope in WindowScope::ALL {
        spans.push(Span::raw(" "));
        x = x.saturating_add(1);
        let label = scope.label();
        let width = u16::try_from(UnicodeWidthStr::width(label) + 2).unwrap_or(u16::MAX);
        scopes[scope.index()] = title_hitbox(area, x, width);
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
    (
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette.border))
            .title(Line::from(spans)),
        WindowControlsHitbox { scopes },
    )
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

fn task_table_columns(area: Rect) -> [Rect; 4] {
    let [_highlight, columns] = Layout::horizontal([
        Constraint::Length(TASK_HIGHLIGHT_WIDTH),
        Constraint::Fill(0),
    ])
    .areas(area);
    Layout::horizontal([
        Constraint::Length(TASK_TOKENS_WIDTH),
        Constraint::Length(TASK_LOCAL_WIDTH),
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

fn confidence_label(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
        Confidence::Unknown => "unknown",
    }
}

fn format_estimated_quota(value: f64, confidence: Confidence) -> String {
    if confidence == Confidence::Unknown {
        "-".to_string()
    } else {
        format!("{value:.1}%")
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

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AttributionSummary, CollectionStats, LimitBucket, LimitWindow, ModelUsage,
        ThreadWindowUsage, TurnWindowUsage, WindowAnalysis, WindowDescriptor, WindowUsage,
    };
    use ratatui::backend::TestBackend;

    fn mouse_test_app(task_count: usize) -> App {
        interaction_test_app(task_count, 0)
    }

    fn interaction_test_app(task_count: usize, turns_per_task: usize) -> App {
        let now = chrono::Utc::now();
        let tasks = (0..task_count)
            .map(|index| TaskRecord {
                thread_id: format!("task-thread-{index}"),
                title: format!("task {index}"),
                cwd: Some("/tmp/project".into()),
                source: Some("desktop".to_string()),
                parent_thread_id: None,
                created_at: Some(now),
                updated_at: Some(now),
                status: TaskStatus::Completed,
                status_provenance: Provenance::LocalExact,
                status_confidence: Confidence::High,
                token_usage: TokenUsage {
                    total_tokens: index as u64 + 1,
                    ..TokenUsage::default()
                },
                turn_count: turns_per_task,
                window_token_usage: TokenUsage::default(),
                local_token_share_percent: 0.0,
                estimated_quota_percent: 0.0,
                quota_confidence: Confidence::Unknown,
            })
            .collect();
        let mut turns = Vec::new();
        for task_index in 0..task_count {
            for turn_index in 0..turns_per_task {
                let token_base = (turn_index as u64 + 1) * 100;
                turns.push(TurnRecord {
                    thread_id: format!("task-thread-{task_index}"),
                    turn_id: format!("turn-{task_index}-{turn_index}"),
                    model: Some(format!("model-{turn_index}")),
                    reasoning_effort: Some("high".to_string()),
                    service_tier: None,
                    message_preview: Some(format!("message {task_index}/{turn_index}")),
                    started_at: Some(now - chrono::Duration::seconds(turn_index as i64 + 2)),
                    completed_at: Some(now - chrono::Duration::seconds(turn_index as i64)),
                    duration_ms: Some(2_000),
                    status: TurnStatus::Completed,
                    token_usage: TokenUsage {
                        input_tokens: token_base / 2,
                        cached_input_tokens: token_base / 5,
                        output_tokens: token_base / 3,
                        reasoning_output_tokens: token_base / 10,
                        total_tokens: token_base,
                    },
                    window_token_usage: TokenUsage {
                        input_tokens: token_base / 4,
                        cached_input_tokens: token_base / 10,
                        output_tokens: token_base / 6,
                        reasoning_output_tokens: token_base / 20,
                        total_tokens: token_base / 2,
                    },
                    local_token_share_percent: turn_index as f64,
                    estimated_quota_percent: turn_index as f64 / 10.0,
                    quota_confidence: Confidence::Medium,
                });
            }
        }
        App::new(
            CollectionResult {
                snapshot: Snapshot {
                    schema_version: 1,
                    as_of: now,
                    partial: false,
                    codex_home: "/tmp/.codex".into(),
                    sources: Vec::new(),
                    limits: Vec::new(),
                    account_usage: None,
                    tasks,
                    turns,
                    models: Vec::new(),
                    attribution: AttributionSummary::default(),
                    window_analyses: Vec::new(),
                    stats: CollectionStats::default(),
                    warnings: Vec::new(),
                    errors: Vec::new(),
                },
                account: AccountSnapshot::default(),
            },
            Theme::Light,
        )
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn set_task_metadata(app: &mut App, index: usize, title: &str, source: Option<&str>) {
        let task = &mut app.snapshot.tasks[index];
        task.title = title.to_string();
        task.source = source.map(str::to_string);
    }

    fn set_task_parent(app: &mut App, child: usize, parent: usize) {
        let parent_thread_id = app.snapshot.tasks[parent].thread_id.clone();
        let task = &mut app.snapshot.tasks[child];
        task.source = Some("subagent".to_string());
        task.parent_thread_id = Some(parent_thread_id);
    }

    fn render_models_content(snapshot: &Snapshot, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_models(
                    frame,
                    frame.area(),
                    snapshot,
                    Theme::Dark,
                    WindowScope::FiveHours,
                )
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn model_usage(model: &str, total_tokens: u64) -> ModelUsage {
        ModelUsage {
            model: model.to_string(),
            token_usage: TokenUsage {
                total_tokens,
                ..TokenUsage::default()
            },
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
        }
    }

    fn add_window_analysis(
        app: &mut App,
        scope: WindowScope,
        total_tokens: u64,
        local_share_percent: f64,
    ) {
        let now = app.snapshot.as_of;
        let usage = WindowUsage {
            token_usage: TokenUsage {
                input_tokens: total_tokens,
                total_tokens,
                ..TokenUsage::default()
            },
            local_token_share_percent: local_share_percent,
            estimated_quota_percent: 2.5,
            quota_confidence: Confidence::Medium,
        };
        let thread_id = app.snapshot.tasks[0].thread_id.clone();
        let turn_id = app
            .snapshot
            .turns
            .iter()
            .find(|turn| turn.thread_id == thread_id)
            .map(|turn| turn.turn_id.clone())
            .unwrap_or_else(|| "turn-0-0".to_string());
        app.snapshot
            .window_analyses
            .retain(|analysis| analysis.duration_mins != scope.duration_mins());
        app.snapshot.window_analyses.push(WindowAnalysis {
            duration_mins: scope.duration_mins(),
            attribution: AttributionSummary {
                window: Some(WindowDescriptor {
                    limit_id: "codex".to_string(),
                    label: scope.label().to_ascii_lowercase(),
                    starts_at: now - chrono::Duration::minutes(scope.duration_mins() - 60),
                    ends_at: now + chrono::Duration::hours(1),
                    used_percent: 23.0,
                }),
                local_token_usage: usage.token_usage,
                observed_delta_percent: 1.0,
                estimated_assigned_percent: 1.0,
                unattributed_percent: 22.0,
                attribution_coverage_percent: 4.3,
                external_activity_possible: true,
                confidence: Confidence::Medium,
                method: "observed_delta_token_proportional".to_string(),
                settled: true,
            },
            partial: false,
            partial_reasons: Vec::new(),
            threads: vec![ThreadWindowUsage {
                thread_id: thread_id.clone(),
                usage,
            }],
            turns: vec![TurnWindowUsage {
                thread_id,
                turn_id,
                usage,
            }],
            models: vec![ModelUsage {
                model: "gpt-window".to_string(),
                token_usage: usage.token_usage,
                local_token_share_percent: local_share_percent,
                estimated_quota_percent: usage.estimated_quota_percent,
                quota_confidence: usage.quota_confidence,
            }],
        });
    }

    #[test]
    fn quota_thresholds_are_distinct() {
        for theme in [Theme::Dark, Theme::Light] {
            let palette = theme.palette();
            assert_eq!(quota_color(50.0, theme), palette.success);
            assert_eq!(quota_color(75.0, theme), palette.warning);
            assert_eq!(quota_color(95.0, theme), palette.error);
        }
    }

    #[test]
    fn status_tones_have_distinct_subtle_backgrounds() {
        assert_eq!(task_status_tone(TaskStatus::Running), StatusTone::Active);
        assert_eq!(turn_status_tone(TurnStatus::Completed), StatusTone::Done);
        for theme in [Theme::Dark, Theme::Light] {
            assert_ne!(
                status_tone_style(StatusTone::Active, theme).bg,
                status_tone_style(StatusTone::Done, theme).bg
            );
            assert_ne!(
                status_tone_style(StatusTone::Waiting, theme).bg,
                status_tone_style(StatusTone::Failed, theme).bg
            );
        }
        assert_eq!(status_marker(StatusTone::Active), "R");
        assert_eq!(status_marker(StatusTone::Stopped), "X");
        assert_eq!(status_marker(StatusTone::Stale), "?");
    }

    #[test]
    fn light_theme_uses_an_explicit_bright_canvas_and_toggles() {
        let palette = Theme::Light.palette();
        assert_eq!(palette.background, Color::Rgb(247, 249, 252));
        assert_eq!(palette.foreground, Color::Rgb(23, 32, 42));
        assert_eq!(palette.border, Color::Rgb(125, 137, 152));
        assert_ne!(palette.background, Theme::Dark.palette().background);
        assert_eq!(Theme::Dark.toggle(), Theme::Light);
        assert_eq!(Theme::Light.toggle(), Theme::Dark);
    }

    #[test]
    fn status_legend_compacts_at_the_horizontal_layout_breakpoint() {
        let compact = status_legend(Theme::Light, 46);
        let full = status_legend(Theme::Light, 80);
        let compact_text = compact
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(compact.width() <= 44);
        assert!(full.width() > compact.width());
        assert!(compact_text.contains("R:RUN"));
        assert!(compact_text.contains("?:STALE"));
    }

    #[test]
    fn confidence_labels_are_stable() {
        assert_eq!(confidence_label(Confidence::Medium), "medium");
        assert_eq!(format_estimated_quota(0.0, Confidence::Unknown), "-");
        assert_eq!(format_estimated_quota(2.26, Confidence::Low), "2.3%");
    }

    #[test]
    fn limit_window_labels_known_windows() {
        assert_eq!(LimitWindow::new(10.0, Some(300), None).label(), "5h");
        assert_eq!(LimitWindow::new(10.0, Some(10_080), None).label(), "week");
    }

    #[test]
    fn models_panel_explains_missing_five_hour_with_weekly_data() {
        let mut app = interaction_test_app(0, 0);
        let now = app.snapshot.as_of;
        app.snapshot.limits = vec![LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: None,
            plan_type: Some("test".to_string()),
            primary: Some(LimitWindow::new(
                23.0,
                Some(10_080),
                Some(now + chrono::Duration::days(6)),
            )),
            secondary: Some(LimitWindow::new(
                5.0,
                Some(1_440),
                Some(now + chrono::Duration::hours(20)),
            )),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        }];

        let content = render_models_content(&app.snapshot, 120, 7);

        assert!(content.contains("Models · 5h unavailable"));
        assert!(content.contains("weekly reset-cycle data remains available"));
    }

    #[test]
    fn models_panel_distinguishes_an_empty_active_window() {
        let mut app = interaction_test_app(0, 0);
        let now = app.snapshot.as_of;
        app.snapshot.attribution.window = Some(WindowDescriptor {
            limit_id: "codex".to_string(),
            label: "5h".to_string(),
            starts_at: now - chrono::Duration::hours(1),
            ends_at: now + chrono::Duration::hours(4),
            used_percent: 0.0,
        });

        let content = render_models_content(&app.snapshot, 100, 4);

        assert!(content.contains("Models · 5h"));
        assert!(content.contains("No local model usage in the current 5h window"));
        assert!(!content.contains("5h unavailable"));
    }

    #[test]
    fn models_panel_prioritizes_token_usage_and_reports_clipping() {
        let mut app = interaction_test_app(0, 0);
        let now = app.snapshot.as_of;
        app.snapshot.attribution.window = Some(WindowDescriptor {
            limit_id: "codex".to_string(),
            label: "5h".to_string(),
            starts_at: now - chrono::Duration::hours(1),
            ends_at: now + chrono::Duration::hours(4),
            used_percent: 10.0,
        });
        app.snapshot.models = vec![
            model_usage("small-model", 10),
            model_usage("largest-model", 1_000),
            model_usage("medium-model", 100),
        ];

        let compact = render_models_content(&app.snapshot, 100, 4);
        assert!(compact.contains("Models · 5h · top 1/3"));
        assert!(compact.contains("largest-model"));
        assert!(!compact.contains("small-model"));
        assert!(!compact.contains("medium-model"));

        let expanded = render_models_content(&app.snapshot, 100, 7);
        let largest = expanded.find("largest-model").unwrap();
        let medium = expanded.find("medium-model").unwrap();
        let small = expanded.find("small-model").unwrap();
        assert!(largest < medium && medium < small);
        assert!(!expanded.contains("top 3/3"));
    }

    #[test]
    fn task_hitbox_maps_only_its_visible_rows() {
        let hitbox = TableHitbox {
            viewport: Rect::new(9, 6, 32, 8),
            rows: Rect::new(10, 7, 30, 5),
            offset: 8,
            capacity: 5,
        };

        assert_eq!(hitbox.index_at(10, 7), Some(8));
        assert_eq!(hitbox.index_at(39, 11), Some(12));
        assert_eq!(hitbox.index_at(9, 7), None);
        assert_eq!(hitbox.index_at(40, 7), None);
        assert_eq!(hitbox.index_at(10, 6), None);
        assert_eq!(hitbox.index_at(10, 12), None);
        assert!(hitbox.contains_viewport(9, 6));
        assert!(!hitbox.contains_viewport(41, 6));
    }

    #[test]
    fn viewport_offsets_scroll_and_reveal_with_clamped_bounds() {
        assert_eq!(scroll_offset(0, 20, 5, true, MOUSE_SCROLL_LINES), 3);
        assert_eq!(scroll_offset(14, 20, 5, true, MOUSE_SCROLL_LINES), 15);
        assert_eq!(scroll_offset(2, 20, 5, false, MOUSE_SCROLL_LINES), 0);
        assert_eq!(scroll_offset(9, 3, 5, true, MOUSE_SCROLL_LINES), 0);

        assert_eq!(reveal_offset(3, 2, 20, 5), 2);
        assert_eq!(reveal_offset(3, 7, 20, 5), 3);
        assert_eq!(reveal_offset(3, 8, 20, 5), 4);
        assert_eq!(reveal_offset(20, 19, 20, 5), 15);
    }

    #[test]
    fn scrollbar_geometry_maps_offsets_to_a_proportional_thumb() {
        let track = Rect::new(79, 7, 1, 10);
        assert!(scrollbar_geometry(track, 10, 10, 0).is_none());
        assert!(scrollbar_geometry(Rect::default(), 100, 10, 0).is_none());
        assert!(scrollbar_geometry(Rect::new(0, 0, 1, 1), 2, 1, 0).is_none());
        assert_eq!(
            scrollbar_geometry(Rect::new(0, 0, 1, 4), 5, 4, 0)
                .unwrap()
                .thumb
                .height,
            3
        );

        let top = scrollbar_geometry(track, 100, 20, 0).unwrap();
        assert_eq!(top.thumb, Rect::new(79, 7, 1, 2));
        assert_eq!(top.max_offset, 80);

        let middle = scrollbar_geometry(track, 100, 20, 40).unwrap();
        assert_eq!(middle.thumb, Rect::new(79, 11, 1, 2));

        let bottom = scrollbar_geometry(track, 100, 20, 80).unwrap();
        assert_eq!(bottom.thumb, Rect::new(79, 15, 1, 2));
        assert_eq!(scale_rounded(0, 80, 8), 0);
        assert_eq!(scale_rounded(4, 80, 8), 40);
        assert_eq!(scale_rounded(8, 80, 8), 80);
    }

    #[test]
    fn turn_detail_allocation_never_reduces_table_capacity_as_height_grows() {
        let mut previous_capacity = 0;
        for height in 7_u16..=40 {
            let capacity = height
                .saturating_sub(turn_detail_height(height))
                .saturating_sub(3);
            assert!(
                capacity >= previous_capacity,
                "turn capacity shrank at height {height}"
            );
            previous_capacity = capacity;
        }
    }

    #[test]
    fn task_filters_match_title_and_source_as_an_intersection() {
        let mut app = interaction_test_app(4, 2);
        set_task_metadata(&mut app, 0, "Alpha build", Some("desktop"));
        set_task_metadata(&mut app, 1, "beta review", Some("subagent"));
        set_task_metadata(&mut app, 2, "ALPHA tests", Some("cli"));
        set_task_metadata(&mut app, 3, "alpha archive", None);

        assert_eq!(app.filtered_task_indices(), vec![0, 1, 2, 3]);
        app.task_search = "alpha".to_string();
        app.reconcile_task_filter(true);
        assert_eq!(app.filtered_task_indices(), vec![0, 2, 3]);
        assert_eq!(app.selected_task, 0);

        app.set_task_source_filter(TaskSourceFilter::Cli);
        assert_eq!(app.filtered_task_indices(), vec![2]);
        assert_eq!(app.selected_task, 2);
        assert_eq!(app.selected_thread_id(), Some("task-thread-2"));
        assert_eq!(app.selected_task_turn_count(), 2);

        app.set_task_source_filter(TaskSourceFilter::Subagent);
        assert!(app.filtered_task_indices().is_empty());
        assert_eq!(app.selected_thread_id(), None);
        assert_eq!(app.selected_task_turn_count(), 0);

        app.set_task_source_filter(TaskSourceFilter::All);
        assert_eq!(app.filtered_task_indices(), vec![0, 2, 3]);
        assert_eq!(app.selected_task, 2);
        assert!(TaskSourceFilter::Desktop.matches(Some("vscode")));
    }

    #[test]
    fn task_filter_matches_title_or_project_basename_but_not_parent_path() {
        let mut app = interaction_test_app(4, 1);
        set_task_metadata(&mut app, 0, "unrelated request", Some("desktop"));
        set_task_metadata(&mut app, 1, "Codex Usage Monitor review", Some("subagent"));
        set_task_metadata(&mut app, 2, "another request", Some("cli"));
        set_task_metadata(&mut app, 3, "root task", Some("desktop"));
        app.snapshot.tasks[0].cwd = Some("/work/codex-usage-monit".into());
        app.snapshot.tasks[1].cwd = Some("/tmp/other-project".into());
        app.snapshot.tasks[2].cwd = Some("/else/codex-usage-monit".into());
        app.snapshot.tasks[3].cwd = Some("/".into());

        app.task_search = "CODEX-USAGE-MONIT".to_string();
        app.reconcile_task_filter(true);
        assert_eq!(app.filtered_task_indices(), vec![0, 2]);

        app.task_search = "codex usage monitor".to_string();
        app.reconcile_task_filter(true);
        assert_eq!(app.filtered_task_indices(), vec![1]);

        app.task_search = "work".to_string();
        app.reconcile_task_filter(true);
        assert!(app.filtered_task_indices().is_empty());

        app.task_search = "codex-usage-monit".to_string();
        app.set_task_source_filter(TaskSourceFilter::Cli);
        assert_eq!(app.filtered_task_indices(), vec![2]);
    }

    #[test]
    fn tree_rows_group_visible_subagents_by_subtree_recency_and_break_cycles() {
        let mut app = interaction_test_app(8, 1);
        set_task_parent(&mut app, 0, 3);
        set_task_parent(&mut app, 2, 5);
        set_task_parent(&mut app, 3, 5);
        app.snapshot.tasks[4].source = Some("subagent".to_string());
        app.snapshot.tasks[4].parent_thread_id = Some("missing-parent".to_string());
        set_task_parent(&mut app, 6, 7);
        set_task_parent(&mut app, 7, 6);
        app.task_list_mode = TaskListMode::Tree;

        let rows = app.filtered_task_rows();
        assert_eq!(
            rows.iter().map(|row| row.index).collect::<Vec<_>>(),
            vec![5, 3, 0, 2, 1, 4, 7, 6]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.prefix.as_str())
                .collect::<Vec<_>>(),
            vec!["", "├─ ", "│ └─ ", "└─ ", "", "", "", "└─ "]
        );

        app.task_source_filter = TaskSourceFilter::Subagent;
        let rows = app.filtered_task_rows();
        assert_eq!(
            rows.iter().map(|row| row.index).collect::<Vec<_>>(),
            vec![3, 0, 2, 4, 7, 6]
        );
        assert!(
            rows.iter()
                .all(|row| { app.snapshot.tasks[row.index].source.as_deref() == Some("subagent") })
        );
        assert_eq!(rows[0].prefix, "");
        assert_eq!(rows[1].prefix, "└─ ");

        app.task_source_filter = TaskSourceFilter::All;
        app.task_search = "task 0".to_string();
        let rows = app.filtered_task_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index, 0);
        assert!(rows[0].prefix.is_empty());
    }

    #[test]
    fn tree_collapse_hides_nested_rows_keeps_rank_and_promotes_filtered_orphans() {
        let mut app = interaction_test_app(6, 2);
        set_task_parent(&mut app, 0, 3);
        set_task_parent(&mut app, 2, 5);
        set_task_parent(&mut app, 3, 5);
        app.snapshot.tasks[4].source = Some("subagent".to_string());
        app.snapshot.tasks[4].parent_thread_id = Some("missing-parent".to_string());
        app.task_list_mode = TaskListMode::Tree;

        let rows = app.filtered_task_rows();
        assert_eq!(
            rows.iter().map(|row| row.index).collect::<Vec<_>>(),
            vec![5, 3, 0, 2, 1, 4]
        );
        assert_eq!(rows.iter().find(|row| row.index == 0).unwrap().depth, 2);
        assert_eq!(rows.iter().find(|row| row.index == 4).unwrap().depth, 0);
        assert!(!task_display_label(&app.snapshot.tasks[0], true).contains("project |"));
        assert!(task_display_label(&app.snapshot.tasks[4], false).contains("project |"));

        app.selected_task = 0;
        app.selected_turn = 1;
        app.turn_offset = 1;
        assert!(app.set_task_collapsed(3, true));
        assert_eq!(app.selected_task, 3);
        assert_eq!(app.selected_turn, 0);
        assert_eq!(app.turn_offset, 0);
        assert_eq!(
            app.filtered_task_indices(),
            vec![5, 3, 2, 1, 4],
            "the hidden newest grandchild must still rank its branch first"
        );

        assert!(app.set_task_collapsed(5, true));
        assert_eq!(app.selected_task, 5);
        assert_eq!(app.filtered_task_indices(), vec![5, 1, 4]);

        app.task_search = "task 0".to_string();
        let rows = app.filtered_task_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index, 0);
        assert_eq!(rows[0].depth, 0);
        assert!(!rows[0].has_children);
        assert!(task_display_label(&app.snapshot.tasks[0], false).contains("project |"));

        app.task_list_mode = TaskListMode::Flat;
        let flat = app.filtered_task_rows();
        assert_eq!(flat[0].depth, 0);
        assert!(task_display_label(&app.snapshot.tasks[0], false).contains("project |"));
    }

    #[test]
    fn tree_plus_minus_toggle_selected_parent_but_search_consumes_the_symbols() {
        let mut app = interaction_test_app(3, 2);
        set_task_parent(&mut app, 0, 2);
        app.task_list_mode = TaskListMode::Tree;
        app.selected_task = 2;

        handle_key_event(&mut app, key_event(KeyCode::Char('-')));
        assert!(app.collapsed_task_threads.contains("task-thread-2"));
        assert_eq!(app.filtered_task_indices(), vec![2, 1]);
        handle_key_event(&mut app, key_event(KeyCode::Char('+')));
        assert!(!app.collapsed_task_threads.contains("task-thread-2"));
        assert_eq!(app.filtered_task_indices(), vec![2, 0, 1]);

        handle_key_event(&mut app, key_event(KeyCode::Char('-')));
        app.begin_task_search();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let marker = app
            .task_tree_marker_hitboxes
            .iter()
            .find(|marker| marker.task_index == 2)
            .unwrap();
        let shortcut = &terminal.backend().buffer()[(marker.area.x + 1, marker.area.y)];
        assert_eq!(shortcut.symbol(), "+");
        assert!(!shortcut.modifier.contains(Modifier::UNDERLINED));

        handle_key_event(&mut app, key_event(KeyCode::Char('-')));
        handle_key_event(&mut app, key_event(KeyCode::Char('+')));
        assert_eq!(app.task_search, "-+");
        assert!(app.collapsed_task_threads.contains("task-thread-2"));
    }

    #[test]
    fn tree_marker_mouse_click_selects_once_and_has_stable_geometry_and_placeholder() {
        for theme in [Theme::Dark, Theme::Light] {
            let mut app = interaction_test_app(4, 2);
            set_task_parent(&mut app, 0, 3);
            app.task_list_mode = TaskListMode::Tree;
            app.theme = theme;
            app.selected_task = 1;
            app.selected_turn = 1;
            app.turn_offset = 1;
            app.focus = Focus::Turns;
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let marker = app
                .task_tree_marker_hitboxes
                .iter()
                .find(|marker| marker.task_index == 3)
                .copied()
                .unwrap();
            assert_eq!(marker.area.width, TASK_TREE_MARKER_WIDTH);
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(marker.area.x, marker.area.y)].symbol(), "[");
            assert_eq!(buffer[(marker.area.x + 1, marker.area.y)].symbol(), "-");
            assert_eq!(buffer[(marker.area.x + 2, marker.area.y)].symbol(), "]");

            assert!(handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    marker.area.right() - 1,
                    marker.area.y,
                ),
            ));
            assert_eq!(app.focus, Focus::Tasks);
            assert_eq!(app.selected_task, 3);
            assert_eq!(app.selected_turn, 0);
            assert_eq!(app.turn_offset, 0);
            assert!(!app.filtered_task_indices().contains(&0));

            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let collapsed = app
                .task_tree_marker_hitboxes
                .iter()
                .find(|marker| marker.task_index == 3)
                .copied()
                .unwrap();
            assert_eq!(collapsed.area, marker.area);
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer[(collapsed.area.x + 1, collapsed.area.y)].symbol(),
                "+"
            );
            assert!(
                buffer[(collapsed.area.x + 1, collapsed.area.y)]
                    .modifier
                    .contains(Modifier::UNDERLINED)
            );

            let leaf_position = app
                .filtered_task_indices()
                .iter()
                .position(|index| *index == 1)
                .unwrap();
            let task_column = task_table_columns(app.task_table_hitbox.unwrap().viewport)[3];
            let leaf_y = app
                .task_table_hitbox
                .unwrap()
                .rows
                .y
                .saturating_add(u16::try_from(leaf_position).unwrap());
            assert!(
                (0..TASK_TREE_MARKER_WIDTH)
                    .all(|offset| buffer[(task_column.x + offset, leaf_y)].symbol() == " ")
            );
        }
    }

    #[test]
    fn compact_tree_marker_hitbox_matches_all_three_clickable_cells() {
        let mut app = interaction_test_app(4, 1);
        set_task_parent(&mut app, 0, 3);
        app.task_list_mode = TaskListMode::Tree;
        app.turns_default_visible = false;
        app.selected_task = 3;
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();

        for (offset, expected_before) in [(0, "-"), (1, "+"), (2, "-")] {
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let marker = app
                .task_tree_marker_hitboxes
                .iter()
                .find(|marker| marker.task_index == 3)
                .copied()
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(marker.area.width, 3);
            assert_eq!(buffer[(marker.area.x, marker.area.y)].symbol(), "[");
            assert_eq!(
                buffer[(marker.area.x + 1, marker.area.y)].symbol(),
                expected_before
            );
            assert_eq!(buffer[(marker.area.x + 2, marker.area.y)].symbol(), "]");
            assert!(handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    marker.area.x + offset,
                    marker.area.y,
                ),
            ));
            assert_eq!(
                app.collapsed_task_threads.contains("task-thread-3"),
                expected_before == "-"
            );
        }
    }

    #[test]
    fn tree_mode_and_refresh_move_a_newly_hidden_child_to_its_collapsed_parent() {
        let mut flat = interaction_test_app(4, 2);
        set_task_parent(&mut flat, 0, 3);
        flat.selected_task = 0;
        flat.selected_turn = 1;
        flat.turn_offset = 1;
        flat.collapsed_task_threads
            .insert("task-thread-3".to_string());
        handle_key_event(&mut flat, key_event(KeyCode::Char('R')));
        assert_eq!(flat.task_list_mode, TaskListMode::Tree);
        assert_eq!(flat.selected_task, 3);
        assert_eq!(flat.selected_turn, 0);
        assert_eq!(flat.turn_offset, 0);
        assert!(!flat.filtered_task_indices().contains(&0));

        let mut refreshed = interaction_test_app(4, 2);
        refreshed.task_list_mode = TaskListMode::Tree;
        refreshed.selected_task = 0;
        refreshed.selected_turn = 1;
        refreshed.turn_offset = 1;
        refreshed
            .collapsed_task_threads
            .insert("task-thread-3".to_string());
        let mut snapshot = refreshed.snapshot.clone();
        snapshot.tasks[0].source = Some("subagent".to_string());
        snapshot.tasks[0].parent_thread_id = Some("task-thread-3".to_string());
        refreshed.replace(
            CollectionResult {
                snapshot,
                account: refreshed.account.clone(),
            },
            false,
        );
        assert_eq!(refreshed.selected_task, 3);
        assert_eq!(refreshed.selected_turn, 0);
        assert_eq!(refreshed.turn_offset, 0);
        assert_eq!(refreshed.selected_thread_id(), Some("task-thread-3"));
        assert!(!refreshed.filtered_task_indices().contains(&0));
    }

    #[test]
    fn refresh_retains_live_collapses_and_drops_removed_parent_state() {
        let mut app = interaction_test_app(4, 1);
        set_task_parent(&mut app, 0, 3);
        app.task_list_mode = TaskListMode::Tree;
        app.selected_task = 3;
        assert!(app.set_task_collapsed(3, true));
        let parent_id = app.snapshot.tasks[3].thread_id.clone();
        let child_id = app.snapshot.tasks[0].thread_id.clone();

        app.replace(
            CollectionResult {
                snapshot: app.snapshot.clone(),
                account: app.account.clone(),
            },
            false,
        );
        assert!(app.collapsed_task_threads.contains(&parent_id));
        assert!(
            !app.filtered_task_indices()
                .iter()
                .any(|index| app.snapshot.tasks[*index].thread_id == child_id)
        );

        let mut snapshot = app.snapshot.clone();
        snapshot.tasks.retain(|task| task.thread_id != parent_id);
        app.replace(
            CollectionResult {
                snapshot,
                account: app.account.clone(),
            },
            false,
        );
        assert!(!app.collapsed_task_threads.contains(&parent_id));
        let child = app
            .filtered_task_rows()
            .into_iter()
            .find(|row| app.snapshot.tasks[row.index].thread_id == child_id)
            .unwrap();
        assert_eq!(child.depth, 0);
        assert!(!child.has_children);
        assert!(task_display_label(&app.snapshot.tasks[child.index], false).contains("project |"));
    }

    #[test]
    fn tree_keyboard_toggle_preserves_selection_and_search_consumes_the_shortcut() {
        let mut app = interaction_test_app(6, 2);
        set_task_parent(&mut app, 0, 4);
        app.selected_task = 0;
        app.selected_turn = 1;
        app.task_table_offset = 3;
        app.turn_offset = 1;

        handle_key_event(&mut app, key_event(KeyCode::Char('R')));
        assert_eq!(app.task_list_mode, TaskListMode::Tree);
        assert_eq!(app.selected_task, 0);
        assert_eq!(app.selected_turn, 1);
        assert_eq!(app.turn_offset, 1);
        assert_eq!(app.task_table_offset, 0);
        assert!(app.task_reveal_pending);

        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        handle_key_event(&mut app, key_event(KeyCode::Char('r')));
        assert_eq!(app.focus, Focus::TaskSearch);
        assert_eq!(app.task_search, "r");
        assert_eq!(app.task_list_mode, TaskListMode::Tree);
        handle_key_event(&mut app, key_event(KeyCode::Esc));

        app.focus = Focus::Turns;
        app.begin_turn_search();
        handle_key_event(&mut app, key_event(KeyCode::Char('R')));
        assert_eq!(app.focus, Focus::TurnSearch);
        assert_eq!(app.turn_search, "R");
        assert_eq!(app.task_list_mode, TaskListMode::Tree);
    }

    #[test]
    fn tree_control_is_fully_clickable_stable_and_muted_while_searching() {
        for (width, expected_width) in [(60, 3), (120, 7)] {
            let mut app = interaction_test_app(8, 1);
            app.turns_default_visible = false;
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let controls = app.task_controls_hitbox.unwrap();
            let initial = controls.toggle_tree;
            assert_eq!(initial.width, expected_width);
            assert!(controls.toggle_turns.right() <= initial.x);
            assert!(initial.right() <= controls.sources[0].x);
            assert_eq!(
                terminal.backend().buffer()[(initial.x + 1, initial.y)].symbol(),
                "R"
            );

            assert!(handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    initial.right() - 1,
                    initial.y,
                ),
            ));
            assert_eq!(app.task_list_mode, TaskListMode::Tree);
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let selected = app.task_controls_hitbox.unwrap().toggle_tree;
            assert_eq!(selected, initial);
            assert!(
                terminal.backend().buffer()[(selected.x + 1, selected.y)]
                    .modifier
                    .contains(Modifier::UNDERLINED)
            );

            handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    selected.right() - 1,
                    selected.y,
                ),
            );
            app.begin_task_search();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let searching = app.task_controls_hitbox.unwrap().toggle_tree;
            let shortcut = &terminal.backend().buffer()[(searching.x + 1, searching.y)];
            assert_eq!(searching, initial);
            assert_eq!(shortcut.fg, app.theme.palette().muted);
            assert!(!shortcut.modifier.contains(Modifier::BOLD));
            assert!(!shortcut.modifier.contains(Modifier::UNDERLINED));
        }
    }

    #[test]
    fn filtered_task_navigation_clicks_and_scroll_use_visible_positions() {
        let mut app = interaction_test_app(30, 1);
        for index in 0..app.snapshot.tasks.len() {
            app.snapshot.tasks[index].source = Some(if index % 3 == 1 {
                "cli".to_string()
            } else {
                "desktop".to_string()
            });
        }
        app.set_task_source_filter(TaskSourceFilter::Cli);
        let filtered = app.filtered_task_indices();
        assert_eq!(filtered, vec![1, 4, 7, 10, 13, 16, 19, 22, 25, 28]);
        assert_eq!(app.selected_task, 1);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hitbox = app.task_table_hitbox.expect("filtered rows should render");
        assert!(hitbox.capacity < filtered.len());
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                hitbox.viewport.x,
                hitbox.viewport.y,
            ),
        ));
        assert_eq!(app.task_table_offset, MOUSE_SCROLL_LINES);
        assert_eq!(app.selected_task, 1);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let scrolled = app.task_table_hitbox.expect("filtered rows should remain");
        let expected = filtered[scrolled.offset];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                scrolled.rows.x,
                scrolled.rows.y,
            ),
        ));
        assert_eq!(app.selected_task, expected);

        handle_key_event(&mut app, key_event(KeyCode::Down));
        assert_eq!(app.selected_task, filtered[scrolled.offset + 1]);
        handle_key_event(&mut app, key_event(KeyCode::Up));
        assert_eq!(app.selected_task, expected);
    }

    #[test]
    fn tree_click_scroll_and_refresh_keep_flattened_positions_mapped_by_thread() {
        let mut app = interaction_test_app(30, 1);
        set_task_parent(&mut app, 0, 10);
        set_task_parent(&mut app, 1, 10);
        app.task_list_mode = TaskListMode::Tree;
        assert_eq!(&app.filtered_task_indices()[..4], &[10, 0, 1, 2]);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rows = app.task_table_hitbox.unwrap().rows;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), rows.x, rows.y + 1,),
        ));
        assert_eq!(app.selected_task, 0);

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::ScrollDown, rows.x, rows.y),
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hitbox = app.task_table_hitbox.unwrap();
        let flattened = app.filtered_task_indices();
        let expected_clicked = flattened[hitbox.offset];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y,
            ),
        ));
        assert_eq!(app.selected_task, expected_clicked);

        let viewport_thread_id = app.snapshot.tasks[flattened[hitbox.offset]]
            .thread_id
            .clone();
        let mut snapshot = app.snapshot.clone();
        let mut newest_child = snapshot.tasks[0].clone();
        newest_child.thread_id = "newest-tree-child".to_string();
        newest_child.parent_thread_id = Some("task-thread-10".to_string());
        snapshot.tasks.insert(0, newest_child);
        app.replace(
            CollectionResult {
                snapshot,
                account: app.account.clone(),
            },
            false,
        );

        let flattened = app.filtered_task_indices();
        assert_eq!(
            app.snapshot.tasks[flattened[app.task_table_offset]].thread_id,
            viewport_thread_id
        );
        assert_eq!(
            app.raw_selected_thread_id(),
            Some(viewport_thread_id.as_str())
        );
    }

    #[test]
    fn source_shortcuts_cycle_and_reselecting_active_button_keeps_viewport() {
        let mut app = interaction_test_app(30, 1);
        for index in 0..app.snapshot.tasks.len() {
            app.snapshot.tasks[index].source = Some(
                match index % 3 {
                    0 => "desktop",
                    1 => "subagent",
                    _ => "cli",
                }
                .to_string(),
            );
        }
        handle_key_event(&mut app, key_event(KeyCode::Char(']')));
        assert_eq!(app.task_source_filter, TaskSourceFilter::Desktop);
        handle_key_event(&mut app, key_event(KeyCode::Char(']')));
        assert_eq!(app.task_source_filter, TaskSourceFilter::Subagent);
        handle_key_event(&mut app, key_event(KeyCode::Char('[')));
        assert_eq!(app.task_source_filter, TaskSourceFilter::Desktop);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let task_hitbox = app.task_table_hitbox.unwrap();
        handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                task_hitbox.viewport.x,
                task_hitbox.viewport.y,
            ),
        );
        let scrolled_offset = app.task_table_offset;
        assert!(scrolled_offset > 0);
        let desktop = app.task_controls_hitbox.unwrap().sources[TaskSourceFilter::Desktop.index()];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                desktop.x,
                desktop.y,
            ),
        ));
        assert_eq!(app.task_table_offset, scrolled_offset);
        assert_eq!(app.focus, Focus::Tasks);
    }

    #[test]
    fn shortcut_labels_highlight_real_bindings_and_direct_source_keys() {
        let mut app = interaction_test_app(8, 1);
        for (index, source) in [
            "desktop", "subagent", "cli", "desktop", "subagent", "cli", "desktop", "subagent",
        ]
        .into_iter()
        .enumerate()
        {
            app.snapshot.tasks[index].source = Some(source.to_string());
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let palette = app.theme.palette();
        let buffer = terminal.backend().buffer();
        let controls = app.task_controls_hitbox.unwrap();
        assert_eq!(buffer[(controls.search.x, controls.search.y)].symbol(), "F");
        assert_eq!(
            buffer[(controls.search.x, controls.search.y)].fg,
            palette.accent
        );

        for filter in TaskSourceFilter::ALL {
            let button = controls.sources[filter.index()];
            let shortcut = &buffer[(button.x + 1, button.y)];
            assert_eq!(shortcut.symbol(), filter.shortcut().to_string());
            if filter == TaskSourceFilter::All {
                assert!(shortcut.modifier.contains(Modifier::UNDERLINED));
            } else {
                assert_eq!(shortcut.fg, palette.accent);
                assert!(shortcut.modifier.contains(Modifier::BOLD));
            }
        }

        let tabs = app.view_tabs_hitbox.unwrap();
        for view in View::ALL {
            let tab = tabs.tabs[view.index()];
            let shortcut = &buffer[(tab.x + 1, tab.y)];
            assert_eq!(shortcut.symbol(), view.shortcut().to_string());
            assert_eq!(shortcut.fg, palette.accent);
        }

        for (key, expected) in [
            ('d', TaskSourceFilter::Desktop),
            ('S', TaskSourceFilter::Subagent),
            ('c', TaskSourceFilter::Cli),
            ('A', TaskSourceFilter::All),
        ] {
            handle_key_event(&mut app, key_event(KeyCode::Char(key)));
            assert_eq!(app.task_source_filter, expected);
        }

        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        handle_key_event(&mut app, key_event(KeyCode::Char('d')));
        assert_eq!(app.focus, Focus::TaskSearch);
        assert_eq!(app.task_search, "d");
        assert_eq!(app.task_source_filter, TaskSourceFilter::All);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let controls = app.task_controls_hitbox.unwrap();
        assert_eq!(
            buffer[(controls.search.x, controls.search.y)].fg,
            palette.title
        );
        let all = controls.sources[TaskSourceFilter::All.index()];
        assert!(
            !buffer[(all.x + 1, all.y)]
                .modifier
                .contains(Modifier::UNDERLINED)
        );
        let desktop = controls.sources[TaskSourceFilter::Desktop.index()];
        assert_eq!(buffer[(desktop.x + 1, desktop.y)].fg, palette.muted);
        let overview = app.view_tabs_hitbox.unwrap().tabs[View::Overview.index()];
        assert_eq!(buffer[(overview.x + 1, overview.y)].fg, palette.muted);
    }

    #[test]
    fn window_scope_shortcuts_and_mouse_switch_reset_cycle_data() {
        let mut app = interaction_test_app(3, 2);
        add_window_analysis(&mut app, WindowScope::FiveHours, 111, 11.0);
        add_window_analysis(&mut app, WindowScope::Week, 777, 63.0);
        app.view = View::Window;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let controls = app
            .window_controls_hitbox
            .expect("window controls should render");
        let palette = app.theme.palette();
        let buffer = terminal.backend().buffer();
        for scope in WindowScope::ALL {
            let button = controls.scopes[scope.index()];
            let shortcut = &buffer[(button.x + 1, button.y)];
            assert_eq!(shortcut.symbol(), scope.shortcut().to_string());
            if scope == WindowScope::FiveHours {
                assert!(shortcut.modifier.contains(Modifier::UNDERLINED));
            } else {
                assert_eq!(shortcut.fg, palette.accent);
                assert!(shortcut.modifier.contains(Modifier::BOLD));
            }
        }

        handle_key_event(&mut app, key_event(KeyCode::Char('W')));
        assert_eq!(app.window_scope, WindowScope::Week);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("week reset cycle"));
        assert!(content.contains("Week-cycle tasks"));
        assert!(content.contains("LOCALWK"));
        assert!(content.contains("777"));
        assert!(content.contains("63.0%"));
        assert!(content.contains("gpt-window"));

        let five_hours = app.window_controls_hitbox.unwrap().scopes[WindowScope::FiveHours.index()];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                five_hours.x,
                five_hours.y,
            ),
        ));
        assert_eq!(app.window_scope, WindowScope::FiveHours);

        handle_key_event(&mut app, key_event(KeyCode::Char('5')));
        assert_eq!(app.window_scope, WindowScope::FiveHours);
        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        handle_key_event(&mut app, key_event(KeyCode::Char('w')));
        assert_eq!(app.focus, Focus::TaskSearch);
        assert_eq!(app.task_search, "w");
        assert_eq!(app.window_scope, WindowScope::FiveHours);
    }

    #[test]
    fn missing_selected_reset_cycle_is_explicitly_unavailable() {
        let mut app = interaction_test_app(1, 1);
        add_window_analysis(&mut app, WindowScope::FiveHours, 100, 100.0);
        app.view = View::Window;
        app.window_scope = WindowScope::Week;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Week reset cycle unavailable"));
        assert!(content.contains("Models · Week unavailable"));
        assert!(!content.contains("No local model usage in the current Week window"));
    }

    #[test]
    fn window_partial_marker_follows_the_selected_scope() {
        let mut app = interaction_test_app(1, 1);
        add_window_analysis(&mut app, WindowScope::FiveHours, 100, 40.0);
        add_window_analysis(&mut app, WindowScope::Week, 250, 100.0);
        let weekly = app
            .snapshot
            .window_analyses
            .iter_mut()
            .find(|analysis| analysis.duration_mins == WindowScope::Week.duration_mins())
            .unwrap();
        weekly.partial = true;
        weekly
            .partial_reasons
            .push("rollout_lookback_incomplete".to_string());
        weekly.attribution.estimated_assigned_percent = 0.0;
        weekly.attribution.confidence = Confidence::Unknown;
        for thread in &mut weekly.threads {
            thread.usage.estimated_quota_percent = 0.0;
            thread.usage.quota_confidence = Confidence::Unknown;
        }
        for turn in &mut weekly.turns {
            turn.usage.estimated_quota_percent = 0.0;
            turn.usage.quota_confidence = Confidence::Unknown;
        }
        for model in &mut weekly.models {
            model.estimated_quota_percent = 0.0;
            model.quota_confidence = Confidence::Unknown;
        }
        app.snapshot.partial = true;
        app.view = View::Window;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let five_hour = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!five_hour.contains(" · partial"));

        app.window_scope = WindowScope::Week;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let weekly = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(weekly.contains(" · partial"));
        assert!(weekly.contains("est.quota=-"));
        assert!(!weekly.contains("0.0% estimated"));
    }

    #[test]
    fn source_buttons_search_hitbox_and_empty_results_are_safe() {
        for (width, height) in [(80, 24), (100, 30), (120, 40)] {
            let mut app = interaction_test_app(8, 2);
            for (index, source) in [
                "desktop", "subagent", "cli", "desktop", "subagent", "cli", "desktop", "subagent",
            ]
            .into_iter()
            .enumerate()
            {
                app.snapshot.tasks[index].source = Some(source.to_string());
            }
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let controls = app
                .task_controls_hitbox
                .expect("task controls should render");
            assert!(controls.sources.iter().all(|area| !area.is_empty()));
            assert!(!controls.search.is_empty());
            for pair in controls.sources.windows(2) {
                assert!(pair[0].right() <= pair[1].x);
            }
            assert!(controls.sources[3].right() <= controls.search.x);

            let subagent = controls.sources[TaskSourceFilter::Subagent.index()];
            assert!(handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    subagent.x,
                    subagent.y,
                ),
            ));
            assert_eq!(app.task_source_filter, TaskSourceFilter::Subagent);
            assert!(
                app.filtered_task_indices()
                    .iter()
                    .all(|index| app.snapshot.tasks[*index].source.as_deref() == Some("subagent"))
            );

            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let controls = app.task_controls_hitbox.unwrap();
            assert!(handle_mouse_event(
                &mut app,
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    controls.search.x,
                    controls.search.y,
                ),
            ));
            assert_eq!(app.focus, Focus::TaskSearch);
            for character in "no such task".chars() {
                handle_key_event(&mut app, key_event(KeyCode::Char(character)));
            }
            assert!(app.filtered_task_indices().is_empty());
            assert_eq!(app.selected_thread_id(), None);
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            assert!(app.task_table_hitbox.is_none());
            assert!(app.turn_table_hitbox.is_none());
            let clear = app.task_controls_hitbox.unwrap().clear_search;
            assert!(!clear.is_empty());
            assert!(handle_mouse_event(
                &mut app,
                mouse_event(MouseEventKind::Down(MouseButton::Left), clear.x, clear.y,),
            ));
            assert!(app.task_search.is_empty());
            assert!(!app.filtered_task_indices().is_empty());

            app.view = View::Health;
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            assert!(app.task_controls_hitbox.is_none());
        }
    }

    #[test]
    fn window_task_controls_compact_before_filter_hitboxes_are_clipped() {
        for scope in WindowScope::ALL {
            for query in ["", "task"] {
                let mut app = interaction_test_app(8, 2);
                app.view = View::Window;
                app.window_scope = scope;
                app.task_search = query.to_string();
                app.task_search_before_edit = app.task_search.clone();
                app.task_search_cursor = app.task_search.chars().count();
                let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();

                let controls = app.task_controls_hitbox.unwrap();
                assert!(controls.enter_turns.right() <= controls.toggle_turns.x);
                assert!(controls.toggle_turns.right() <= controls.sources[0].x);
                for pair in controls.sources.windows(2) {
                    assert!(pair[0].right() <= pair[1].x);
                }
                assert!(controls.sources[3].right() <= controls.search.x);
                assert!(controls.sources.iter().all(|button| !button.is_empty()));
                assert!(!controls.search.is_empty());

                let buffer = terminal.backend().buffer();
                for filter in TaskSourceFilter::ALL {
                    let button = controls.sources[filter.index()];
                    assert_eq!(
                        buffer[(button.x + 1, button.y)].symbol(),
                        filter.shortcut().to_string()
                    );
                }
                assert_eq!(buffer[(controls.search.x, controls.search.y)].symbol(), "F");
                if query.is_empty() {
                    assert!(controls.clear_search.is_empty());
                } else {
                    assert!(!controls.clear_search.is_empty());
                    assert_eq!(
                        buffer[(controls.clear_search.x, controls.clear_search.y)].symbol(),
                        "×"
                    );
                    assert!(controls.search.right() <= controls.clear_search.x);
                }
            }
        }
    }

    #[test]
    fn search_input_has_priority_over_global_shortcuts_and_supports_cancel() {
        let mut app = interaction_test_app(3, 1);
        let initial_theme = app.theme;
        let initial_view = app.view;
        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('/'))));
        assert_eq!(app.focus, Focus::TaskSearch);

        for character in ['q', 't', '1', 'j', '测'] {
            assert!(!handle_key_event(
                &mut app,
                key_event(KeyCode::Char(character)),
            ));
        }
        assert_eq!(app.task_search, "qt1j测");
        assert_eq!(app.theme, initial_theme);
        assert_eq!(app.view, initial_view);
        assert_eq!(app.selected_task, 0);

        handle_key_event(&mut app, key_event(KeyCode::Left));
        handle_key_event(&mut app, key_event(KeyCode::Backspace));
        assert_eq!(app.task_search, "qt1测");
        handle_key_event(&mut app, key_event(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Tasks);
        assert!(app.task_search.is_empty());

        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        for character in "task 2".chars() {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }
        handle_key_event(&mut app, key_event(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.task_search, "task 2");
        assert_eq!(app.selected_task, 2);
    }

    #[test]
    fn turn_filter_is_independent_and_consumes_shortcuts_while_editing() {
        let mut app = interaction_test_app(2, 4);
        app.task_search = "task".to_string();
        app.task_search_before_edit = app.task_search.clone();
        handle_key_event(&mut app, key_event(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Turns);

        handle_key_event(&mut app, key_event(KeyCode::Char('f')));
        assert_eq!(app.focus, Focus::TurnSearch);
        for character in "model-2".chars() {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }
        assert_eq!(app.task_search, "task");
        assert_eq!(app.selected_task_turn_count(), 1);
        assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-0-2");

        let initial_view = app.view;
        let initial_visibility = app.turns_default_visible;
        for character in ['v', '1'] {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }
        assert_eq!(app.view, initial_view);
        assert_eq!(app.turns_default_visible, initial_visibility);
        assert_eq!(app.turn_search, "model-2v1");

        handle_key_event(&mut app, key_event(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Turns);
        assert!(app.turn_search.is_empty());
        assert_eq!(app.selected_task_turn_count(), 4);

        app.snapshot.turns[2].service_tier = Some("priority".to_string());
        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        for character in "fa".chars() {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }
        handle_key_event(&mut app, key_event(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Turns);
        assert_eq!(app.selected_task_turn_count(), 1);
        assert!(app.selected_turn_record().unwrap().is_fast());
        assert_eq!(app.task_search, "task");
    }

    #[test]
    fn turn_filter_mouse_controls_work_while_temporarily_expanded() {
        let mut app = interaction_test_app(1, 4);
        app.toggle_turns_default_visibility();
        app.focus_turns();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let controls = app.turn_controls_hitbox.unwrap();
        assert!(!controls.back_tasks.is_empty());
        assert!(!controls.search.is_empty());
        assert!(controls.back_tasks.right() <= controls.search.x);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                controls.search.x,
                controls.search.y,
            ),
        ));
        assert_eq!(app.focus, Focus::TurnSearch);
        assert!(app.turns_temporarily_visible);

        for character in "model-2".chars() {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }
        assert_eq!(app.selected_task_turn_count(), 1);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let clear = app.turn_controls_hitbox.unwrap().clear_search;
        assert!(!clear.is_empty());
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), clear.x, clear.y),
        ));
        assert!(app.turn_search.is_empty());
        assert_eq!(app.selected_task_turn_count(), 4);
        assert_eq!(app.focus, Focus::Turns);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let back = app.turn_controls_hitbox.unwrap().back_tasks;
        assert!(!back.is_empty());
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), back.x, back.y),
        ));
        assert_eq!(app.focus, Focus::Tasks);
        assert!(!app.turns_temporarily_visible);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_none());
    }

    #[test]
    fn clicking_turn_filter_clear_commits_the_editor_before_esc() {
        let mut app = interaction_test_app(1, 4);
        app.turn_search = "model-".to_string();
        app.reconcile_turn_filter(true, None);
        app.focus_turns();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let search = app.turn_controls_hitbox.unwrap().search;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), search.x, search.y),
        ));
        assert_eq!(app.focus, Focus::TurnSearch);
        assert_eq!(app.turn_search_before_edit, "model-");
        handle_key_event(&mut app, key_event(KeyCode::Char('2')));
        assert_eq!(app.turn_search, "model-2");
        assert_eq!(app.selected_task_turn_count(), 1);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let clear = app.turn_controls_hitbox.unwrap().clear_search;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), clear.x, clear.y),
        ));
        assert_eq!(app.focus, Focus::Turns);
        assert!(app.turn_search.is_empty());
        assert!(app.turn_search_before_edit.is_empty());
        assert!(app.turn_search_restore_turn_id.is_none());
        assert_eq!(app.selected_task_turn_count(), 4);

        assert!(handle_key_event(&mut app, key_event(KeyCode::Esc)));
        assert_eq!(app.focus, Focus::Turns);
        assert!(app.turn_search.is_empty());
    }

    #[test]
    fn filtered_turn_rows_keep_selection_detail_mouse_and_scroll_in_sync() {
        let mut app = interaction_test_app(1, 8);
        app.focus_turns();
        app.turn_search = "model-6".to_string();
        app.reconcile_turn_filter(true, None);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let hitbox = app.turn_table_hitbox.unwrap();
        assert_eq!(app.selected_task_turn_count(), 1);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y,
            ),
        ));
        assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-0-6");
        let selected_before = app.selected_turn;
        handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                hitbox.viewport.x,
                hitbox.viewport.y,
            ),
        );
        assert_eq!(app.selected_turn, selected_before);
        assert_eq!(app.turn_offset, 0);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("turn=turn-0-6"));
        assert!(content.contains("message=message 0/6"));

        app.turn_search = "no matching turn".to_string();
        app.reconcile_turn_filter(true, None);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("No matching turns"));
        assert!(!content.contains("No turns for selected task"));
    }

    #[test]
    fn turns_visibility_and_focus_buttons_share_the_keyboard_state_machine() {
        let mut app = interaction_test_app(1, 3);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_some());

        let toggle = app.task_controls_hitbox.unwrap().toggle_turns;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), toggle.x, toggle.y),
        ));
        assert!(!app.turns_default_visible);
        assert!(!app.turns_temporarily_visible);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_none());
        assert_eq!(app.task_table_hitbox.unwrap().viewport.width, 118);

        let enter = app.task_controls_hitbox.unwrap().enter_turns;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), enter.x, enter.y),
        ));
        assert_eq!(app.focus, Focus::Turns);
        assert!(app.turns_temporarily_visible);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_some());

        let back = app.turn_controls_hitbox.unwrap().back_tasks;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), back.x, back.y),
        ));
        assert_eq!(app.focus, Focus::Tasks);
        assert!(!app.turns_temporarily_visible);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_none());

        handle_key_event(&mut app, key_event(KeyCode::Char('v')));
        assert!(app.turns_default_visible);
        handle_key_event(&mut app, key_event(KeyCode::Enter));
        handle_key_event(&mut app, key_event(KeyCode::Backspace));
        assert_eq!(app.focus, Focus::Tasks);
        assert!(app.turns_visible());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_some());
    }

    #[test]
    fn opening_turns_reveals_a_task_after_the_compact_viewport_shrinks() {
        for open_temporarily in [false, true] {
            let mut app = interaction_test_app(30, 1);
            app.toggle_turns_default_visibility();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();

            let hidden_hitbox = app.task_table_hitbox.unwrap();
            let target = hidden_hitbox.capacity - 1;
            assert!(app.select_task(target, true));
            assert_eq!(app.task_table_offset, 0);
            assert!(!app.task_reveal_pending);

            let key = if open_temporarily {
                KeyCode::Enter
            } else {
                KeyCode::Char('v')
            };
            handle_key_event(&mut app, key_event(key));
            assert!(app.task_reveal_pending);
            terminal.draw(|frame| render(frame, &mut app)).unwrap();

            let visible_hitbox = app.task_table_hitbox.unwrap();
            assert!(visible_hitbox.capacity < hidden_hitbox.capacity);
            assert!(target >= visible_hitbox.offset);
            assert!(target < visible_hitbox.offset + visible_hitbox.capacity);
            assert!(!app.task_reveal_pending);
            assert_eq!(
                app.focus,
                if open_temporarily {
                    Focus::Turns
                } else {
                    Focus::Tasks
                }
            );
            assert_eq!(app.turns_temporarily_visible, open_temporarily);
            assert_eq!(app.turns_default_visible, !open_temporarily);
        }
    }

    #[test]
    fn compact_task_controls_keep_focus_visibility_and_filter_hitboxes_separate() {
        let mut app = interaction_test_app(4, 2);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let controls = app.task_controls_hitbox.unwrap();
        assert!(!controls.enter_turns.is_empty());
        assert!(!controls.toggle_turns.is_empty());
        assert!(!controls.search.is_empty());
        assert!(controls.enter_turns.right() <= controls.toggle_turns.x);
        assert!(controls.toggle_turns.right() <= controls.sources[0].x);
        for pair in controls.sources.windows(2) {
            assert!(pair[0].right() <= pair[1].x);
        }
        assert!(controls.sources[3].right() <= controls.search.x);

        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(controls.enter_turns.x, controls.enter_turns.y)].symbol(),
            ENTER_FOCUS_HINT
        );
        assert_eq!(
            buffer[(controls.toggle_turns.x + 1, controls.toggle_turns.y)].symbol(),
            "V"
        );
        assert_eq!(buffer[(controls.search.x, controls.search.y)].symbol(), "F");
    }

    #[test]
    fn temporary_turns_close_on_task_focus_and_view_changes() {
        let mut app = interaction_test_app(2, 2);
        app.toggle_turns_default_visibility();
        app.focus_turns();
        assert!(app.turns_temporarily_visible);
        app.focus_tasks();
        assert!(!app.turns_temporarily_visible);

        app.focus_turns();
        app.set_view(View::Window);
        assert_eq!(app.focus, Focus::Tasks);
        assert!(!app.turns_temporarily_visible);

        app.focus_turns();
        app.set_view(View::Health);
        assert_eq!(app.focus, Focus::Tasks);
        assert!(!app.turns_temporarily_visible);
    }

    #[test]
    fn clicking_the_selected_task_closes_temporarily_visible_turns() {
        let mut app = interaction_test_app(2, 2);
        app.toggle_turns_default_visibility();
        app.focus_turns();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let task_rows = app.task_table_hitbox.unwrap().rows;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                task_rows.x,
                task_rows.y,
            ),
        ));
        assert_eq!(app.selected_task, 0);
        assert_eq!(app.focus, Focus::Tasks);
        assert!(!app.turns_temporarily_visible);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_none());
    }

    #[test]
    fn clicking_the_task_scrollbar_closes_temporarily_visible_turns() {
        let mut app = interaction_test_app(30, 2);
        app.toggle_turns_default_visibility();
        app.focus_turns();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let scrollbar = app.task_scrollbar_hitbox.unwrap();
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                scrollbar.thumb.x,
                scrollbar.thumb.y,
            ),
        ));
        assert_eq!(app.focus, Focus::Tasks);
        assert!(!app.turns_temporarily_visible);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.turn_table_hitbox.is_none());
    }

    #[test]
    fn fast_turns_are_badged_without_changing_ordinary_turns() {
        let mut app = interaction_test_app(1, 2);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let ordinary = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!ordinary.contains("FAST"));

        app.snapshot.turns[1].service_tier = Some("priority".to_string());
        app.focus_turns();
        app.select_turn(1, true);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let fast = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(fast.contains("FAST"));
        assert!(fast.contains("model-0"));
        assert!(fast.contains("model-1"));
    }

    #[test]
    fn search_cancel_restores_task_turn_and_viewports() {
        let mut app = interaction_test_app(8, 4);
        app.selected_task = 6;
        app.selected_turn = 3;
        app.task_table_offset = 4;
        app.turn_offset = 2;

        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        for character in "task 1".chars() {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }
        assert_eq!(app.selected_task, 1);
        assert_eq!(app.selected_turn, 0);
        handle_key_event(&mut app, key_event(KeyCode::Esc));

        assert_eq!(app.focus, Focus::Tasks);
        assert!(app.task_search.is_empty());
        assert_eq!(app.selected_task, 6);
        assert_eq!(app.selected_turn, 3);
        assert_eq!(app.task_table_offset, 4);
        assert_eq!(app.turn_offset, 2);
    }

    #[test]
    fn long_unicode_search_keeps_the_cursor_visible_in_narrow_panels() {
        let (before, after, cursor_visible) =
            search_cursor_window("abcdefghijklmnopqrstuvwxyz测试", 28, 6);
        assert!(cursor_visible);
        assert!(
            UnicodeWidthStr::width(before.as_str()) + UnicodeWidthStr::width(after.as_str()) < 6
        );
        assert!(after.is_empty());

        let mut app = interaction_test_app(4, 1);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        for character in "abcdefghijklmnopqrstuvwxyz测试".chars() {
            handle_key_event(&mut app, key_event(KeyCode::Char(character)));
        }

        for key in [KeyCode::End, KeyCode::Home] {
            handle_key_event(&mut app, key_event(key));
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let search = app.task_controls_hitbox.unwrap().search;
            let buffer = terminal.backend().buffer();
            assert!((search.x..search.right()).any(|x| buffer[(x, search.y)].symbol() == "▌"));
        }
    }

    #[test]
    fn keyboard_focus_moves_between_tasks_and_turns_and_reveals_selection() {
        let mut app = interaction_test_app(2, 30);
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.selected_task, 0);
        assert_eq!(app.selected_turn, 0);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        handle_key_event(&mut app, key_event(KeyCode::Down));
        assert_eq!(app.selected_task, 1);
        assert_eq!(app.selected_turn, 0);

        handle_key_event(&mut app, key_event(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Turns);
        handle_key_event(&mut app, key_event(KeyCode::End));
        assert_eq!(app.selected_task, 1);
        assert_eq!(app.selected_turn, 29);
        assert!(app.turn_offset > 0);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let turn_hitbox = app.turn_table_hitbox.expect("turn rows should render");
        assert!(app.selected_turn >= turn_hitbox.offset);
        assert!(app.selected_turn < turn_hitbox.offset + turn_hitbox.capacity);
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("30/30"));

        handle_key_event(&mut app, key_event(KeyCode::Backspace));
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.selected_turn, 29);
        handle_key_event(&mut app, key_event(KeyCode::Up));
        assert_eq!(app.selected_task, 0);
        assert_eq!(app.selected_turn, 0);

        let mut no_turns = interaction_test_app(1, 0);
        handle_key_event(&mut no_turns, key_event(KeyCode::Enter));
        assert_eq!(no_turns.focus, Focus::Tasks);
    }

    #[test]
    fn health_view_ignores_hidden_panel_navigation() {
        let mut app = interaction_test_app(3, 3);
        handle_key_event(&mut app, key_event(KeyCode::Enter));
        handle_key_event(&mut app, key_event(KeyCode::Down));
        assert_eq!(app.focus, Focus::Turns);
        assert_eq!(app.selected_turn, 1);
        app.view = View::Health;
        let before = (
            app.selected_task,
            app.selected_turn,
            app.task_table_offset,
            app.turn_offset,
        );

        for key in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Backspace,
        ] {
            handle_key_event(&mut app, key_event(key));
        }
        assert_eq!(
            (
                app.selected_task,
                app.selected_turn,
                app.task_table_offset,
                app.turn_offset
            ),
            before
        );
        assert_eq!(app.focus, Focus::Turns);
    }

    #[test]
    fn refresh_preserves_viewport_rows_and_leaves_empty_turn_focus() {
        let mut app = interaction_test_app(50, 30);
        app.selected_task = 20;
        app.selected_turn = 10;
        app.task_table_offset = 10;
        app.turn_offset = 5;
        app.focus = Focus::Turns;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let selected_thread = app.raw_selected_thread_id().unwrap().to_string();
        let selected_turn = app.selected_turn_record().unwrap().turn_id.clone();
        let task_top = app.filtered_task_indices()[app.task_table_offset];
        let task_top_id = app.snapshot.tasks[task_top].thread_id.clone();
        let turn_top_id = app
            .snapshot
            .turns
            .iter()
            .filter(|turn| turn.thread_id == selected_thread)
            .nth(app.turn_offset)
            .unwrap()
            .turn_id
            .clone();

        let mut snapshot = app.snapshot.clone();
        let mut inserted_task = snapshot.tasks[0].clone();
        inserted_task.thread_id = "inserted-task".to_string();
        inserted_task.title = "inserted task".to_string();
        snapshot.tasks.insert(0, inserted_task);
        let mut inserted_turn = snapshot
            .turns
            .iter()
            .find(|turn| turn.thread_id == selected_thread)
            .unwrap()
            .clone();
        inserted_turn.turn_id = "inserted-turn".to_string();
        snapshot.turns.insert(0, inserted_turn);
        app.replace(
            CollectionResult {
                snapshot,
                account: app.account.clone(),
            },
            false,
        );

        assert_eq!(app.raw_selected_thread_id(), Some(selected_thread.as_str()));
        assert_eq!(app.selected_turn_record().unwrap().turn_id, selected_turn);
        assert_eq!(
            app.snapshot.tasks[app.filtered_task_indices()[app.task_table_offset]].thread_id,
            task_top_id
        );
        assert_eq!(
            app.snapshot
                .turns
                .iter()
                .filter(|turn| turn.thread_id == selected_thread)
                .nth(app.turn_offset)
                .unwrap()
                .turn_id,
            turn_top_id
        );

        let mut no_turns = app.snapshot.clone();
        no_turns
            .turns
            .retain(|turn| turn.thread_id != selected_thread);
        app.replace(
            CollectionResult {
                snapshot: no_turns,
                account: app.account.clone(),
            },
            false,
        );
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.selected_task_turn_count(), 0);
    }

    #[test]
    fn refresh_keeps_a_top_task_viewport_following_new_rows() {
        let mut app = interaction_test_app(50, 1);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let selected_thread = app.raw_selected_thread_id().unwrap().to_string();
        let capacity = app.task_table_hitbox.unwrap().capacity;
        let mut snapshot = app.snapshot.clone();
        for index in 0..capacity + 2 {
            let mut inserted_task = snapshot.tasks[0].clone();
            inserted_task.thread_id = format!("newest-task-{index}");
            inserted_task.title = format!("newest task {index}");
            snapshot.tasks.insert(0, inserted_task);
        }
        let newest_thread = format!("newest-task-{}", capacity + 1);

        app.replace(
            CollectionResult {
                snapshot,
                account: app.account.clone(),
            },
            false,
        );
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.task_table_offset, 0);
        assert_eq!(app.raw_selected_thread_id(), Some(selected_thread.as_str()));
        let hitbox = app.task_table_hitbox.expect("tasks should remain visible");
        let first_visible = app.filtered_task_indices()[hitbox.offset];
        assert_eq!(app.snapshot.tasks[first_visible].thread_id, newest_thread);
        let selected_position = app
            .filtered_task_indices()
            .iter()
            .position(|index| *index == app.selected_task)
            .unwrap();
        assert!(selected_position >= hitbox.capacity);
    }

    #[test]
    fn refresh_restores_filtered_turn_selection_and_viewport_by_id() {
        let mut app = interaction_test_app(1, 30);
        for (index, turn) in app.snapshot.turns.iter_mut().enumerate() {
            turn.model = Some(if index % 2 == 0 { "keep" } else { "drop" }.to_string());
        }
        app.turn_search = "keep".to_string();
        app.selected_turn = 10;
        app.turn_offset = 5;
        app.focus = Focus::Turns;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let selected_turn_id = app.selected_turn_record().unwrap().turn_id.clone();
        let viewport_turn_id = app.snapshot.turns[app.filtered_turn_indices()[app.turn_offset]]
            .turn_id
            .clone();
        let mut snapshot = app.snapshot.clone();
        let mut inserted = snapshot.turns[0].clone();
        inserted.turn_id = "inserted-matching-turn".to_string();
        snapshot.turns.insert(0, inserted);

        app.replace(
            CollectionResult {
                snapshot,
                account: app.account.clone(),
            },
            false,
        );

        assert_eq!(
            app.selected_turn_record().unwrap().turn_id,
            selected_turn_id
        );
        assert_eq!(
            app.snapshot.turns[app.filtered_turn_indices()[app.turn_offset]].turn_id,
            viewport_turn_id
        );
    }

    #[test]
    fn refresh_reveals_a_previously_visible_selection_after_reordering() {
        let mut app = interaction_test_app(50, 30);
        app.selected_task = 20;
        app.selected_turn = 10;
        app.task_table_offset = 10;
        app.turn_offset = 5;
        app.focus = Focus::Turns;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let selected_thread = app.raw_selected_thread_id().unwrap().to_string();
        let selected_turn = app.selected_turn_record().unwrap().turn_id.clone();
        let mut snapshot = app.snapshot.clone();
        let task_index = snapshot
            .tasks
            .iter()
            .position(|task| task.thread_id == selected_thread)
            .unwrap();
        let task = snapshot.tasks.remove(task_index);
        snapshot.tasks.insert(0, task);
        let turn_index = snapshot
            .turns
            .iter()
            .position(|turn| turn.turn_id == selected_turn)
            .unwrap();
        let turn = snapshot.turns.remove(turn_index);
        let first_thread_turn = snapshot
            .turns
            .iter()
            .position(|turn| turn.thread_id == selected_thread)
            .unwrap();
        snapshot.turns.insert(first_thread_turn, turn);

        app.replace(
            CollectionResult {
                snapshot,
                account: app.account.clone(),
            },
            false,
        );
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.raw_selected_thread_id(), Some(selected_thread.as_str()));
        assert_eq!(app.selected_turn_record().unwrap().turn_id, selected_turn);
        let task_position = app
            .filtered_task_indices()
            .iter()
            .position(|index| *index == app.selected_task)
            .unwrap();
        let task_hitbox = app.task_table_hitbox.unwrap();
        assert!(task_position >= task_hitbox.offset);
        assert!(task_position < task_hitbox.offset + task_hitbox.capacity);
        let turn_hitbox = app.turn_table_hitbox.unwrap();
        assert!(app.selected_turn >= turn_hitbox.offset);
        assert!(app.selected_turn < turn_hitbox.offset + turn_hitbox.capacity);
    }

    #[test]
    fn focused_panel_uses_the_vivid_selection_marker() {
        let mut app = interaction_test_app(1, 2);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let palette = app.theme.palette();
        let task_markers = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == "▌" && cell.fg == palette.accent)
            .count();
        assert_eq!(task_markers, 1);

        handle_key_event(&mut app, key_event(KeyCode::Enter));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == "▏" && cell.fg == palette.muted)
        );
        assert_eq!(
            buffer
                .content()
                .iter()
                .filter(|cell| cell.symbol() == "▌" && cell.fg == palette.accent)
                .count(),
            1
        );
    }

    #[test]
    fn focus_transition_hints_follow_available_keyboard_actions_without_layout_shift() {
        let mut app = interaction_test_app(1, 2);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let palette = app.theme.palette();
        let task_controls = app.task_controls_hitbox.unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == ENTER_FOCUS_HINT && cell.fg == palette.accent)
        );
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.symbol() != BACK_FOCUS_HINT)
        );

        handle_key_event(&mut app, key_event(KeyCode::Enter));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.symbol() != ENTER_FOCUS_HINT)
        );
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == BACK_FOCUS_HINT && cell.fg == palette.accent)
        );
        assert_eq!(app.task_controls_hitbox.unwrap(), task_controls);

        handle_key_event(&mut app, key_event(KeyCode::Backspace));
        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer.content().iter().all(|cell| {
                cell.symbol() != ENTER_FOCUS_HINT && cell.symbol() != BACK_FOCUS_HINT
            })
        );
        assert_eq!(app.task_controls_hitbox.unwrap(), task_controls);

        let mut no_turns = interaction_test_app(1, 0);
        terminal.draw(|frame| render(frame, &mut no_turns)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.symbol() != ENTER_FOCUS_HINT)
        );
    }

    #[test]
    fn view_tabs_use_rendered_padding_and_support_mouse_switching() {
        let mut app = interaction_test_app(3, 2);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let tabs = app.view_tabs_hitbox.expect("view tabs should render");
        assert_eq!(tabs.tabs[View::Overview.index()], Rect::new(0, 0, 12, 1));
        assert_eq!(tabs.tabs[View::Window.index()], Rect::new(15, 0, 10, 1));
        assert_eq!(tabs.tabs[View::Health.index()], Rect::new(28, 0, 15, 1));

        let divider = tabs.tabs[View::Overview.index()].right();
        assert!(!handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), divider, 0),
        ));
        assert_eq!(app.view, View::Overview);
        assert!(!handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Right),
                tabs.tabs[View::Window.index()].x,
                0,
            ),
        ));

        let window = tabs.tabs[View::Window.index()];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), window.x, window.y),
        ));
        assert_eq!(app.view, View::Window);

        handle_key_event(&mut app, key_event(KeyCode::Char('/')));
        handle_key_event(&mut app, key_event(KeyCode::Char('q')));
        assert_eq!(app.focus, Focus::TaskSearch);
        let health = tabs.tabs[View::Health.index()];
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), health.x, health.y),
        ));
        assert_eq!(app.view, View::Health);
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.task_search, "q");
        assert!(handle_key_event(&mut app, key_event(KeyCode::Char('q'))));

        for width in [8, 20, 80] {
            let area = Rect::new(0, 0, width, 1);
            let hitbox = view_tabs_hitbox(area);
            assert!(
                hitbox
                    .tabs
                    .iter()
                    .all(|tab| tab.x >= area.x && tab.right() <= area.right())
            );
        }
    }

    #[test]
    fn mouse_focus_and_wheel_stay_independent_and_backspace_reveals_task() {
        let mut app = interaction_test_app(30, 5);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let turn_hitbox = app.turn_table_hitbox.unwrap();
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                turn_hitbox.rows.x,
                turn_hitbox.rows.y,
            ),
        ));
        assert_eq!(app.focus, Focus::Turns);

        let task_hitbox = app.task_table_hitbox.unwrap();
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                task_hitbox.viewport.x,
                task_hitbox.viewport.y,
            ),
        ));
        assert_eq!(app.focus, Focus::Turns);
        assert_eq!(app.selected_task, 0);
        assert!(app.task_table_offset > 0);

        handle_key_event(&mut app, key_event(KeyCode::Backspace));
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.task_table_offset, 0);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let tasks = app.task_table_hitbox.unwrap();
        assert!(app.selected_task >= tasks.offset);
        assert!(app.selected_task < tasks.offset + tasks.capacity);
    }

    #[test]
    fn keyboard_task_selection_in_health_is_revealed_on_return() {
        let mut app = mouse_test_app(50);
        app.selected_task = 40;
        app.task_table_offset = 26;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        app.view = View::Health;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.select_first_task();
        assert!(app.task_reveal_pending);
        app.view = View::Overview;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let overview = app.task_table_hitbox.expect("task rows should be visible");
        assert_eq!(app.selected_task, 0);
        assert_eq!(overview.offset, 0);
        assert!(!app.task_reveal_pending);

        app.view = View::Health;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.select_last_task();
        assert!(app.task_reveal_pending);
        app.view = View::Window;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let window = app.task_table_hitbox.expect("task rows should be visible");
        assert_eq!(app.selected_task, 49);
        assert!(app.selected_task >= window.offset);
        assert!(app.selected_task < window.offset + usize::from(window.rows.height));
        assert!(!app.task_reveal_pending);
    }

    #[test]
    fn task_and_turn_scrollbars_drag_without_changing_selection_or_wheel_behavior() {
        let mut app = interaction_test_app(30, 30);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let task_bar = app.task_scrollbar_hitbox.expect("task scrollbar");
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                task_bar.thumb.x,
                task_bar.thumb.y,
            ),
        ));
        assert_eq!(app.task_table_offset, 0);
        assert_eq!(app.selected_task, 0);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                0,
                task_bar.track.bottom().saturating_add(10),
            ),
        ));
        assert_eq!(app.task_table_offset, task_bar.max_offset);
        assert_eq!(app.selected_task, 0);
        assert_eq!(app.focus, Focus::Tasks);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Up(MouseButton::Left), 0, 0),
        ));
        let dragged_offset = app.task_table_offset;
        assert!(!handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 0, 0),
        ));
        assert_eq!(app.task_table_offset, dragged_offset);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let task_bar = app.task_scrollbar_hitbox.expect("task scrollbar");
        assert_eq!(task_bar.thumb.bottom(), task_bar.track.bottom());
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::ScrollUp, task_bar.track.x, task_bar.track.y,),
        ));
        assert_eq!(
            app.task_table_offset,
            dragged_offset.saturating_sub(MOUSE_SCROLL_LINES)
        );
        assert_eq!(app.selected_task, 0);

        app.begin_task_search();
        let turn_bar = app.turn_scrollbar_hitbox.expect("turn scrollbar");
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                turn_bar.track.x,
                turn_bar.track.bottom().saturating_sub(1),
            ),
        ));
        assert_eq!(app.focus, Focus::Turns);
        assert_eq!(app.turn_offset, turn_bar.max_offset);
        assert_eq!(app.selected_turn, 0);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                turn_bar.track.x,
                turn_bar.track.bottom().saturating_sub(1),
            ),
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let turn_bar = app.turn_scrollbar_hitbox.expect("turn scrollbar");
        assert_eq!(turn_bar.thumb.bottom(), turn_bar.track.bottom());
    }

    #[test]
    fn horizontal_drag_on_a_scrollbar_thumb_does_not_quantize_the_offset() {
        let mut app = mouse_test_app(100);
        app.task_table_offset = 44;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let scrollbar = app.task_scrollbar_hitbox.expect("task scrollbar");

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                scrollbar.thumb.x,
                scrollbar.thumb.y,
            ),
        ));
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                scrollbar.thumb.x.saturating_sub(10),
                scrollbar.thumb.y,
            ),
        ));
        assert_eq!(app.task_table_offset, 44);

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                0,
                scrollbar.track.bottom().saturating_sub(1),
            ),
        ));
        assert!(app.task_table_offset > 44);
    }

    #[test]
    fn filtered_task_scrollbar_keeps_row_clicks_mapped_to_absolute_tasks() {
        let mut app = interaction_test_app(30, 1);
        for index in 0..30 {
            app.snapshot.tasks[index].source =
                Some(if index % 2 == 0 { "cli" } else { "desktop" }.to_string());
        }
        app.set_task_source_filter(TaskSourceFilter::Cli);
        let filtered = app.filtered_task_indices();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let scrollbar = app.task_scrollbar_hitbox.expect("filtered task scrollbar");

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                scrollbar.track.x,
                scrollbar.track.bottom().saturating_sub(1),
            ),
        ));
        assert_eq!(app.task_table_offset, scrollbar.max_offset);
        handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                scrollbar.track.x,
                scrollbar.track.bottom().saturating_sub(1),
            ),
        );
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rows = app.task_table_hitbox.unwrap().rows;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(MouseEventKind::Down(MouseButton::Left), rows.x, rows.y),
        ));
        assert_eq!(app.selected_task, filtered[scrollbar.max_offset]);
    }

    #[test]
    fn mouse_wheel_routes_by_table_and_keeps_selection_stable() {
        let mut app = interaction_test_app(30, 20);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let task_hitbox = app.task_table_hitbox.expect("task rows should be visible");
        let turn_hitbox = app.turn_table_hitbox.expect("turn rows should be visible");

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                task_hitbox.viewport.x,
                task_hitbox.viewport.y,
            ),
        ));
        assert_eq!(app.task_table_offset, MOUSE_SCROLL_LINES);
        assert_eq!(app.selected_task, 0);
        assert_eq!(app.selected_turn, 0);
        assert_eq!(app.turn_offset, 0);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollUp,
                task_hitbox.viewport.x,
                task_hitbox.viewport.y,
            ),
        ));
        assert_eq!(app.task_table_offset, 0);
        handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                task_hitbox.viewport.x,
                task_hitbox.viewport.y,
            ),
        );

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                turn_hitbox.viewport.x,
                turn_hitbox.viewport.y,
            ),
        ));
        assert_eq!(app.turn_offset, MOUSE_SCROLL_LINES);
        assert_eq!(app.selected_turn, 0);
        assert_eq!(app.selected_task, 0);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollUp,
                turn_hitbox.viewport.x,
                turn_hitbox.viewport.y,
            ),
        ));
        assert_eq!(app.turn_offset, 0);
        handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                turn_hitbox.viewport.x,
                turn_hitbox.viewport.y,
            ),
        );

        assert!(!handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                turn_hitbox.viewport.x,
                turn_hitbox.viewport.bottom().saturating_add(1),
            ),
        ));
        assert!(!handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollLeft,
                turn_hitbox.viewport.x,
                turn_hitbox.viewport.y,
            ),
        ));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let task_hitbox = app.task_table_hitbox.expect("task rows should be visible");
        assert_eq!(task_hitbox.offset, MOUSE_SCROLL_LINES);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                task_hitbox.rows.x,
                task_hitbox.rows.y,
            ),
        ));
        assert_eq!(app.selected_task, MOUSE_SCROLL_LINES);
        assert_eq!(app.selected_turn, 0);
        assert_eq!(app.turn_offset, 0);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let turn_hitbox = app.turn_table_hitbox.expect("turn rows should be visible");
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                turn_hitbox.viewport.x,
                turn_hitbox.viewport.y,
            ),
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let turn_hitbox = app.turn_table_hitbox.expect("turn rows should be visible");
        let target_row = turn_hitbox.rows.y.saturating_add(1);
        let target_index = turn_hitbox.offset + 1;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                turn_hitbox.rows.x,
                target_row,
            ),
        ));
        assert_eq!(app.selected_turn, target_index);

        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::ScrollDown,
                turn_hitbox.viewport.x,
                turn_hitbox.viewport.y,
            ),
        ));
        assert_eq!(app.selected_turn, target_index);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains(&format!("turn=turn-3-{target_index}")));
    }

    #[test]
    fn turn_click_maps_scrolled_rows_across_views_and_sizes() {
        for (width, height) in [(80, 24), (100, 30), (120, 40)] {
            for view in [View::Overview, View::Window] {
                let mut app = interaction_test_app(1, 30);
                app.view = view;
                app.turn_offset = 10;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let hitbox = app.turn_table_hitbox.expect("turn rows should be visible");
                assert!(hitbox.offset > 0);
                let relative = usize::from(hitbox.rows.height > 1);
                let target = hitbox.offset + relative;

                assert!(handle_mouse_event(
                    &mut app,
                    mouse_event(
                        MouseEventKind::Down(MouseButton::Left),
                        hitbox.rows.x,
                        hitbox.rows.y + relative as u16,
                    ),
                ));
                assert_eq!(app.selected_turn, target);
                assert_eq!(app.turn_offset, hitbox.offset);

                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let content = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(content.contains("Turn detail"));
                assert!(content.contains(&format!("{}/30", target + 1)));
            }
        }
    }

    #[test]
    fn refresh_preserves_selected_turn_by_id_and_falls_back_when_removed() {
        let mut app = interaction_test_app(1, 3);
        app.selected_turn = 1;
        let selected_id = app.selected_turn_record().unwrap().turn_id.clone();

        let mut inserted_snapshot = app.snapshot.clone();
        let mut inserted = inserted_snapshot.turns[0].clone();
        inserted.turn_id = "newest-turn".to_string();
        inserted_snapshot.turns.insert(0, inserted);
        app.replace(
            CollectionResult {
                snapshot: inserted_snapshot,
                account: app.account.clone(),
            },
            false,
        );
        assert_eq!(app.selected_turn, 2);
        assert_eq!(app.selected_turn_record().unwrap().turn_id, selected_id);

        let mut removed_snapshot = app.snapshot.clone();
        removed_snapshot
            .turns
            .retain(|turn| turn.turn_id != selected_id);
        app.turn_offset = 2;
        app.replace(
            CollectionResult {
                snapshot: removed_snapshot,
                account: app.account.clone(),
            },
            false,
        );
        assert_eq!(app.selected_turn, 0);
        assert_eq!(app.turn_offset, 0);
        assert_ne!(app.selected_turn_record().unwrap().turn_id, selected_id);

        let replacement = interaction_test_app(2, 10);
        let mut replacement_snapshot = replacement.snapshot;
        replacement_snapshot
            .tasks
            .retain(|task| task.thread_id == "task-thread-1");
        replacement_snapshot
            .turns
            .retain(|turn| turn.thread_id == "task-thread-1");
        app.task_table_offset = 7;
        app.turn_offset = 4;
        app.selected_turn = 1;
        app.replace(
            CollectionResult {
                snapshot: replacement_snapshot,
                account: replacement.account,
            },
            false,
        );
        assert_eq!(app.selected_task, 0);
        assert_eq!(app.task_table_offset, 0);
        assert_eq!(app.selected_turn, 0);
        assert_eq!(app.turn_offset, 0);
        assert_eq!(app.selected_turn_record().unwrap().turn_id, "turn-1-0");
    }

    #[test]
    fn task_click_uses_rendered_scroll_offset_without_jumping() {
        for (width, height) in [(80, 24), (100, 30), (120, 40)] {
            for view in [View::Overview, View::Window] {
                let mut app = mouse_test_app(50);
                app.view = view;
                app.selected_task = 40;
                app.task_table_offset = 35;
                app.turn_offset = 5;
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();

                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let hitbox = app.task_table_hitbox.expect("task rows should be visible");
                assert!(hitbox.offset > 0, "expected a scrolled table in {view:?}");

                let target = hitbox.offset;
                assert_ne!(target, app.selected_task);
                assert!(handle_mouse_event(
                    &mut app,
                    mouse_event(
                        MouseEventKind::Down(MouseButton::Left),
                        hitbox.rows.x,
                        hitbox.rows.y,
                    ),
                ));
                assert_eq!(app.selected_task, target);
                assert_eq!(app.turn_offset, 0);

                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let rerendered = app
                    .task_table_hitbox
                    .expect("task rows should remain visible");
                assert_eq!(rerendered.offset, hitbox.offset);
                assert_eq!(app.task_table_offset, hitbox.offset);
            }
        }
    }

    #[test]
    fn task_scroll_offset_refills_rows_after_resize_and_view_change() {
        let mut app = mouse_test_app(50);
        app.view = View::Window;
        app.selected_task = 40;
        app.task_table_offset = 39;

        let mut narrow = Terminal::new(TestBackend::new(80, 24)).unwrap();
        narrow.draw(|frame| render(frame, &mut app)).unwrap();
        let narrow_hitbox = app.task_table_hitbox.expect("task rows should be visible");
        assert_eq!(narrow_hitbox.rows.height, 2);
        assert!(narrow_hitbox.offset > 30);

        let mut wide = Terminal::new(TestBackend::new(120, 40)).unwrap();
        wide.draw(|frame| render(frame, &mut app)).unwrap();
        let wide_hitbox = app.task_table_hitbox.expect("task rows should be visible");
        assert_eq!(wide_hitbox.rows.height, 19);
        assert!(wide_hitbox.offset <= 31);
        assert!(app.selected_task >= wide_hitbox.offset);
        assert!(app.selected_task < wide_hitbox.offset + usize::from(wide_hitbox.rows.height));

        app.view = View::Overview;
        wide.draw(|frame| render(frame, &mut app)).unwrap();
        let overview_hitbox = app.task_table_hitbox.expect("task rows should be visible");
        assert_eq!(overview_hitbox.rows.height, 24);
        assert!(overview_hitbox.offset <= 26);
        assert!(app.selected_task >= overview_hitbox.offset);
        assert!(
            app.selected_task < overview_hitbox.offset + usize::from(overview_hitbox.rows.height)
        );
    }

    #[test]
    fn task_click_ignores_non_rows_non_left_clicks_and_stale_views() {
        let mut app = mouse_test_app(3);
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hitbox = app.task_table_hitbox.expect("task rows should be visible");

        for event in [
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y - 1,
            ),
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x - 1,
                hitbox.rows.y,
            ),
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.bottom(),
            ),
            mouse_event(
                MouseEventKind::Down(MouseButton::Right),
                hitbox.rows.x,
                hitbox.rows.y + 1,
            ),
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y + 1,
            ),
        ] {
            assert!(!handle_mouse_event(&mut app, event));
            assert_eq!(app.selected_task, 0);
        }

        app.turn_offset = 7;
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y,
            ),
        ));
        assert_eq!(app.turn_offset, 7);
        assert!(handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y + 1,
            ),
        ));
        assert_eq!(app.selected_task, 1);
        assert_eq!(app.turn_offset, 0);

        app.view = View::Health;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.task_table_hitbox.is_none());
        assert!(app.turn_table_hitbox.is_none());
        assert!(!handle_mouse_event(
            &mut app,
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                hitbox.rows.x,
                hitbox.rows.y,
            ),
        ));
        assert_eq!(app.selected_task, 1);
    }

    #[test]
    fn renders_all_views_at_common_terminal_sizes() {
        for (width, height) in [(80, 24), (120, 40)] {
            let now = chrono::Utc::now();
            let snapshot = Snapshot {
                schema_version: 1,
                as_of: now,
                partial: false,
                codex_home: "/tmp/.codex".into(),
                sources: Vec::new(),
                limits: vec![LimitBucket {
                    limit_id: "codex".to_string(),
                    limit_name: None,
                    plan_type: Some("test".to_string()),
                    primary: Some(LimitWindow::new(
                        25.0,
                        Some(300),
                        Some(now + chrono::Duration::hours(2)),
                    )),
                    secondary: Some(LimitWindow::new(
                        40.0,
                        Some(10_080),
                        Some(now + chrono::Duration::days(4)),
                    )),
                    credits: None,
                    rate_limit_reached_type: None,
                    provenance: Provenance::ServerSnapshot,
                    as_of: now,
                }],
                account_usage: None,
                tasks: vec![TaskRecord {
                    thread_id: "task-thread".to_string(),
                    title: "task".to_string(),
                    cwd: Some("/tmp/project".into()),
                    source: Some("desktop".to_string()),
                    parent_thread_id: None,
                    created_at: Some(now),
                    updated_at: Some(now),
                    status: TaskStatus::Completed,
                    status_provenance: Provenance::LocalExact,
                    status_confidence: Confidence::High,
                    token_usage: TokenUsage {
                        total_tokens: 42,
                        ..TokenUsage::default()
                    },
                    turn_count: 1,
                    window_token_usage: TokenUsage {
                        total_tokens: 42,
                        ..TokenUsage::default()
                    },
                    local_token_share_percent: 100.0,
                    estimated_quota_percent: 1.0,
                    quota_confidence: Confidence::Low,
                }],
                turns: vec![TurnRecord {
                    thread_id: "task-thread".to_string(),
                    turn_id: "turn-1".to_string(),
                    model: Some("gpt-5.6-sol".to_string()),
                    reasoning_effort: Some("ultra".to_string()),
                    service_tier: None,
                    message_preview: Some("inspect message preview".to_string()),
                    started_at: Some(now),
                    completed_at: Some(now),
                    duration_ms: Some(1),
                    status: crate::domain::TurnStatus::Completed,
                    token_usage: TokenUsage {
                        total_tokens: 42,
                        ..TokenUsage::default()
                    },
                    window_token_usage: TokenUsage {
                        total_tokens: 42,
                        ..TokenUsage::default()
                    },
                    local_token_share_percent: 100.0,
                    estimated_quota_percent: 1.0,
                    quota_confidence: Confidence::Low,
                }],
                models: Vec::new(),
                attribution: AttributionSummary::default(),
                window_analyses: Vec::new(),
                stats: CollectionStats::default(),
                warnings: Vec::new(),
                errors: Vec::new(),
            };
            let result = CollectionResult {
                snapshot,
                account: AccountSnapshot::default(),
            };

            for theme in [Theme::Dark, Theme::Light] {
                let mut app = App::new(result.clone(), theme);
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();

                for view in [View::Overview, View::Window, View::Health] {
                    app.view = view;
                    terminal.draw(|frame| render(frame, &mut app)).unwrap();
                    let buffer = terminal.backend().buffer();
                    if theme == Theme::Light {
                        let palette = theme.palette();
                        let cell_count = usize::from(width) * usize::from(height);
                        assert!(
                            buffer
                                .content()
                                .iter()
                                .filter(|cell| cell.bg == palette.background)
                                .count()
                                > cell_count / 2
                        );
                        assert!(
                            buffer
                                .content()
                                .iter()
                                .filter(|cell| cell.fg == palette.border)
                                .count()
                                > 10
                        );
                    }
                    if view != View::Health {
                        let content = buffer
                            .content()
                            .iter()
                            .map(|cell| cell.symbol())
                            .collect::<String>();
                        assert!(content.contains("ultra"));
                        assert!(
                            content.contains("gpt-5.6-sol"),
                            "missing full model at {width}x{height} in {view:?}/{theme:?}: {content}"
                        );
                        assert!(content.contains("inspect"));
                        assert!(content.contains("Turn detail"));
                        assert!(content.contains("total=42"));
                        assert!(content.contains("1ms"));
                        assert!(content.contains("D 42"));
                        assert!(content.contains("LOCAL"));
                        assert!(content.contains("EST.Q"));
                        assert!(content.contains("DONE"));
                        assert!(content.contains("STALE"));
                        assert!(!content.contains("STATE EVIDENCE"));

                        if theme == Theme::Light {
                            let palette = theme.palette();
                            let done = status_tone_style(StatusTone::Done, theme);
                            assert!(
                                buffer
                                    .content()
                                    .iter()
                                    .any(|cell| cell.bg == palette.gauge_track)
                            );
                            assert!(buffer.content().iter().any(|cell| {
                                cell.fg == palette.accent
                                    && Some(cell.bg) == done.bg
                                    && !cell.symbol().trim().is_empty()
                            }));
                            for tone in [
                                StatusTone::Active,
                                StatusTone::Waiting,
                                StatusTone::Done,
                                StatusTone::Stopped,
                                StatusTone::Failed,
                                StatusTone::Stale,
                            ] {
                                let style = status_tone_style(tone, theme);
                                assert!(buffer.content().iter().any(|cell| {
                                    Some(cell.fg) == style.fg && Some(cell.bg) == style.bg
                                }));
                            }
                        }
                    }
                }

                let content = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(content.contains("Data health"));

                if theme == Theme::Light {
                    let light_background = theme.palette().background;
                    app.toggle_theme();
                    terminal.draw(|frame| render(frame, &mut app)).unwrap();
                    let dark_background = Theme::Dark.palette().background;
                    assert!(
                        terminal
                            .backend()
                            .buffer()
                            .content()
                            .iter()
                            .any(|cell| cell.bg == dark_background)
                    );
                    assert!(
                        terminal
                            .backend()
                            .buffer()
                            .content()
                            .iter()
                            .all(|cell| cell.bg != light_background)
                    );
                }
            }
        }
    }
}
