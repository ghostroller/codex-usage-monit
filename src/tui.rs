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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, HighlightSpacing, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, Confidence, Provenance, Snapshot, TaskRecord, TaskStatus, TokenUsage,
    TurnRecord, TurnStatus, terminal_safe_text,
};
use crate::rollout::RolloutCache;
use crate::snapshot::{CollectionResult, collect_snapshot_cached};

const LOCAL_REFRESH: Duration = Duration::from_secs(2);
const ACCOUNT_REFRESH: Duration = Duration::from_secs(45);
const MOUSE_SCROLL_LINES: usize = 3;
const PAGE_SCROLL_LINES: usize = 5;
const TAB_PADDING: &str = " ";
const TAB_DIVIDER: &str = " | ";

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
enum Focus {
    #[default]
    Tasks,
    Turns,
    Search,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TaskSourceFilter {
    #[default]
    All,
    Desktop,
    Subagent,
    Cli,
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
            (Self::All, _) => "All",
            (Self::Desktop, true) => "Desk",
            (Self::Desktop, false) => "Desktop",
            (Self::Subagent, true) => "Sub",
            (Self::Subagent, false) => "Subagent",
            (Self::Cli, _) => "CLI",
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewTabsHitbox {
    tabs: [Rect; 3],
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
    focus: Focus,
    task_source_filter: TaskSourceFilter,
    task_search: String,
    task_search_before_edit: String,
    task_search_cursor: usize,
    task_search_restore_thread_id: Option<String>,
    task_search_restore_turn_id: Option<String>,
    task_search_restore_task_offset: usize,
    task_search_restore_turn_offset: usize,
    selected_task: usize,
    selected_turn: usize,
    turn_offset: usize,
    task_table_offset: usize,
    task_reveal_pending: bool,
    task_table_hitbox: Option<TableHitbox>,
    turn_table_hitbox: Option<TableHitbox>,
    task_controls_hitbox: Option<TaskControlsHitbox>,
    view_tabs_hitbox: Option<ViewTabsHitbox>,
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
            focus: Focus::Tasks,
            task_source_filter: TaskSourceFilter::All,
            task_search: String::new(),
            task_search_before_edit: String::new(),
            task_search_cursor: 0,
            task_search_restore_thread_id: None,
            task_search_restore_turn_id: None,
            task_search_restore_task_offset: 0,
            task_search_restore_turn_offset: 0,
            selected_task: 0,
            selected_turn: 0,
            turn_offset: 0,
            task_table_offset: 0,
            task_reveal_pending: false,
            task_table_hitbox: None,
            turn_table_hitbox: None,
            task_controls_hitbox: None,
            view_tabs_hitbox: None,
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

    fn filtered_task_indices(&self) -> Vec<usize> {
        let query = self.task_search.to_lowercase();
        self.snapshot
            .tasks
            .iter()
            .enumerate()
            .filter_map(|(index, task)| {
                self.task_matches_filter_query(task, &query)
                    .then_some(index)
            })
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
        self.task_matches_filter(task)
            .then_some(task.thread_id.as_str())
    }

    #[cfg(test)]
    fn selected_turn_record(&self) -> Option<&TurnRecord> {
        let thread_id = self.selected_thread_id()?;
        self.snapshot
            .turns
            .iter()
            .filter(|turn| turn.thread_id == thread_id)
            .nth(self.selected_turn)
    }

    fn selected_task_turn_count(&self) -> usize {
        self.selected_thread_id()
            .map(|thread_id| {
                self.snapshot
                    .turns
                    .iter()
                    .filter(|turn| turn.thread_id == thread_id)
                    .count()
            })
            .unwrap_or(0)
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
            if self.focus == Focus::Turns {
                self.focus = Focus::Tasks;
            }
            return;
        }

        let target = if filtered.contains(&self.selected_task) {
            self.selected_task
        } else {
            filtered[0]
        };
        let selection_changed = target != self.selected_task;
        if selection_changed {
            self.selected_task = target;
            self.task_table_offset = 0;
            self.reset_turn_selection();
            if self.focus == Focus::Turns {
                self.focus = Focus::Tasks;
            }
        }
        if selection_changed || reset_viewport {
            self.task_reveal_pending = true;
        }
    }

    fn set_task_source_filter(&mut self, filter: TaskSourceFilter) {
        self.focus = Focus::Tasks;
        self.task_search_before_edit.clone_from(&self.task_search);
        self.clear_task_search_restore();
        if self.task_source_filter == filter {
            return;
        }
        self.task_source_filter = filter;
        self.reconcile_task_filter(true);
    }

    fn begin_task_search(&mut self) {
        if self.focus != Focus::Search {
            self.task_search_before_edit.clone_from(&self.task_search);
            self.task_search_cursor = self.task_search.chars().count();
            self.task_search_restore_thread_id = self.selected_thread_id().map(str::to_string);
            self.task_search_restore_turn_id = self
                .selected_thread_id()
                .and_then(|thread_id| {
                    self.snapshot
                        .turns
                        .iter()
                        .filter(|turn| turn.thread_id == thread_id)
                        .nth(self.selected_turn)
                })
                .map(|turn| turn.turn_id.clone());
            self.task_search_restore_task_offset = self.task_table_offset;
            self.task_search_restore_turn_offset = self.turn_offset;
            self.focus = Focus::Search;
        }
    }

    fn accept_task_search(&mut self) {
        self.task_search_before_edit.clone_from(&self.task_search);
        self.clear_task_search_restore();
        self.focus = Focus::Tasks;
    }

    fn cancel_task_search(&mut self) {
        if self.focus == Focus::Search {
            let restore_thread_id = self.task_search_restore_thread_id.take();
            let restore_turn_id = self.task_search_restore_turn_id.take();
            let restore_task_offset = self.task_search_restore_task_offset;
            let restore_turn_offset = self.task_search_restore_turn_offset;
            self.task_search.clone_from(&self.task_search_before_edit);
            self.task_search_cursor = self.task_search.chars().count();
            self.focus = Focus::Tasks;
            let restored_task = restore_thread_id.as_deref().and_then(|thread_id| {
                self.snapshot
                    .tasks
                    .iter()
                    .position(|task| task.thread_id == thread_id && self.task_matches_filter(task))
            });
            if let Some(task_index) = restored_task {
                self.selected_task = task_index;
                let thread_id = &self.snapshot.tasks[task_index].thread_id;
                let turn_count = self
                    .snapshot
                    .turns
                    .iter()
                    .filter(|turn| turn.thread_id == thread_id.as_str())
                    .count();
                self.selected_turn = restore_turn_id
                    .as_deref()
                    .and_then(|turn_id| {
                        self.snapshot
                            .turns
                            .iter()
                            .filter(|turn| turn.thread_id == thread_id.as_str())
                            .position(|turn| turn.turn_id == turn_id)
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

    fn replace(&mut self, result: CollectionResult, refreshed_account: bool) {
        let filtered = self.filtered_task_indices();
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
        let selected_turn_id = selected.as_deref().and_then(|thread_id| {
            self.snapshot
                .turns
                .iter()
                .filter(|turn| turn.thread_id == thread_id)
                .nth(self.selected_turn)
                .map(|turn| turn.turn_id.clone())
        });
        let selected_turn_was_visible = self.turn_table_hitbox.is_some_and(|hitbox| {
            self.selected_turn >= hitbox.offset
                && self.selected_turn < hitbox.offset.saturating_add(hitbox.capacity)
        });
        let turn_viewport_id = selected.as_deref().and_then(|thread_id| {
            self.snapshot
                .turns
                .iter()
                .filter(|turn| turn.thread_id == thread_id)
                .nth(self.turn_offset)
                .map(|turn| turn.turn_id.clone())
        });
        self.snapshot = result.snapshot;
        self.account = result.account;
        self.task_table_hitbox = None;
        self.turn_table_hitbox = None;
        self.task_controls_hitbox = None;
        self.view_tabs_hitbox = None;
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
        let selected_thread_id = self.selected_thread_id().map(str::to_string);
        let turn_count = self.selected_task_turn_count();
        let restored_turn = task_was_restored
            .then_some(selected_turn_id.as_deref())
            .flatten()
            .and_then(|turn_id| {
                self.snapshot
                    .turns
                    .iter()
                    .filter(|turn| {
                        selected_thread_id
                            .as_deref()
                            .is_some_and(|thread_id| turn.thread_id == thread_id)
                    })
                    .position(|turn| turn.turn_id == turn_id)
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

        let selected_thread_was_restored = self.selected_thread_id() == selected.as_deref();
        if turn_was_restored && selected_thread_was_restored && !self.turn_reveal_pending {
            let restored_viewport = turn_viewport_id.as_deref().and_then(|turn_id| {
                let thread_id = self.selected_thread_id()?;
                self.snapshot
                    .turns
                    .iter()
                    .filter(|turn| turn.thread_id == thread_id)
                    .position(|turn| turn.turn_id == turn_id)
            });
            if let Some(position) = restored_viewport {
                self.turn_offset = position;
            }
            if selected_turn_was_visible {
                self.turn_reveal_pending = true;
            }
        }
        if self.focus == Focus::Turns && self.selected_task_turn_count() == 0 {
            self.focus = Focus::Tasks;
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
        if self.view != View::Health && self.selected_task_turn_count() > 0 {
            self.focus = Focus::Turns;
            self.select_turn(self.selected_turn, true);
        }
    }

    fn focus_tasks(&mut self) {
        self.focus = Focus::Tasks;
        self.select_task(self.selected_task, true);
    }

    fn select_next_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_next(),
            Focus::Turns => self.select_next_turn(),
            Focus::Search => {}
        }
    }

    fn select_previous_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_previous(),
            Focus::Turns => self.select_previous_turn(),
            Focus::Search => {}
        }
    }

    fn select_first_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_first_task(),
            Focus::Turns => self.select_first_turn(),
            Focus::Search => {}
        }
    }

    fn select_last_focused(&mut self) {
        match self.focus {
            Focus::Tasks => self.select_last_task(),
            Focus::Turns => self.select_last_turn(),
            Focus::Search => {}
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

    fn activate_task_control_at(&mut self, column: u16, row: u16) -> bool {
        let Some(hitbox) = self.task_controls_hitbox else {
            return false;
        };
        if rect_contains(hitbox.clear_search, column, row) {
            self.clear_task_search();
            return true;
        }
        if rect_contains(hitbox.search, column, row) {
            self.begin_task_search();
            return true;
        }
        for filter in TaskSourceFilter::ALL {
            if rect_contains(hitbox.sources[filter.index()], column, row) {
                self.set_task_source_filter(filter);
                return true;
            }
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
        if self.focus == Focus::Search {
            self.accept_task_search();
        }
        self.view = view;
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
}

fn handle_mouse_event(app: &mut App, event: MouseEvent) -> bool {
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if app.activate_view_at(event.column, event.row)
                || app.activate_task_control_at(event.column, event.row)
            {
                true
            } else {
                if app.focus == Focus::Search {
                    app.accept_task_search();
                }
                if app.select_turn_at(event.column, event.row) {
                    app.focus = Focus::Turns;
                    true
                } else if app.select_task_at(event.column, event.row) {
                    app.focus = Focus::Tasks;
                    true
                } else {
                    false
                }
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let down = matches!(event.kind, MouseEventKind::ScrollDown);
            if app
                .turn_table_hitbox
                .is_some_and(|hitbox| hitbox.contains_viewport(event.column, event.row))
            {
                app.scroll_turns(down, MOUSE_SCROLL_LINES);
                true
            } else if app
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

    if app.focus == Focus::Search {
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

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab | KeyCode::Right => app.view = app.view.next(),
        KeyCode::BackTab | KeyCode::Left => app.view = app.view.previous(),
        KeyCode::Char('1') => app.view = View::Overview,
        KeyCode::Char('2') => app.view = View::Window,
        KeyCode::Char('3') => app.view = View::Health,
        KeyCode::Char('t') => app.toggle_theme(),
        KeyCode::Char('/') | KeyCode::Char('f') if app.view != View::Health => {
            app.begin_task_search();
        }
        KeyCode::Char(']') if app.view != View::Health => {
            app.cycle_task_source_filter(true);
        }
        KeyCode::Char('[') if app.view != View::Health => {
            app.cycle_task_source_filter(false);
        }
        KeyCode::Delete if app.view != View::Health => app.clear_task_search(),
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
            Focus::Search => {}
        },
        KeyCode::PageUp if app.view != View::Health => match app.focus {
            Focus::Tasks => app.scroll_tasks(false, PAGE_SCROLL_LINES),
            Focus::Turns => app.scroll_turns(false, PAGE_SCROLL_LINES),
            Focus::Search => {}
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
    app.view_tabs_hitbox = None;
    let palette = app.theme.palette();
    frame.render_widget(Block::default().style(app.theme.base_style()), area);
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let titles = View::ALL
        .into_iter()
        .map(|view| Line::from(view.label()))
        .collect::<Vec<_>>();
    let tabs = Tabs::new(titles)
        .select(app.view.index())
        .style(Style::default().fg(palette.muted))
        .highlight_style(
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )
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
    render_models(frame, rows[2], &app.snapshot, app.theme);
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
    render_attribution(frame, rows[1], &app.snapshot, app.theme);

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
    render_models(frame, rows[3], &app.snapshot, app.theme);
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
    let filtered = app.filtered_task_indices();
    let selected_position = filtered
        .iter()
        .position(|index| *index == app.selected_task);
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
    let task_rows = filtered
        .iter()
        .skip(offset)
        .filter_map(|index| app.snapshot.tasks.get(*index))
        .map(|task| {
            let tokens = if window_only {
                task.window_token_usage
            } else {
                task.token_usage
            };
            let tone = task_status_tone(task.status);
            Row::new([
                Cell::from(format!("{} {}", status_marker(tone), format_tokens(tokens))),
                Cell::from(format!("{:.1}%", task.local_token_share_percent)),
                Cell::from(format!("{:.1}%", task.estimated_quota_percent)),
                Cell::from(task_display_label(task)),
            ])
            .style(status_tone_style(tone, theme))
        });
    let tasks_focused = app.focus == Focus::Tasks;
    let palette = theme.palette();
    let table = Table::new(
        task_rows,
        [
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(12),
        ],
    )
    .header(table_header(
        ["TOKENS", "LOCAL5H", "EST.Q5H", "TASK"],
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
}

fn task_panel_block(
    area: Rect,
    app: &App,
    window_only: bool,
    filtered_count: usize,
) -> (Block<'static>, TaskControlsHitbox) {
    let palette = app.theme.palette();
    let inner_right = area.right().saturating_sub(1);
    let compact = area.width < 60;
    let title = if window_only {
        "5h tasks"
    } else {
        "Recent tasks"
    };
    let mut spans = vec![Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(palette.title)
            .add_modifier(Modifier::BOLD),
    )];
    let mut title_x = area.x.saturating_add(1 + title.len() as u16 + 1);
    let mut source_hitboxes = [Rect::default(); 4];
    for filter in TaskSourceFilter::ALL {
        spans.push(Span::raw(" "));
        title_x = title_x.saturating_add(1);
        let label = format!("[{}]", filter.label(compact));
        let label_width = label.len() as u16;
        source_hitboxes[filter.index()] = title_hitbox(area, title_x, label_width);
        let style = if app.task_source_filter == filter {
            Style::default()
                .fg(palette.background)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(label, style));
        title_x = title_x.saturating_add(label_width);
    }

    spans.push(Span::raw(" "));
    title_x = title_x.saturating_add(1);
    let search_start = title_x;
    let clear_search = if !app.task_search.is_empty() && inner_right > search_start {
        Rect::new(inner_right - 1, area.y, 1, u16::from(area.height > 0))
    } else {
        Rect::default()
    };
    let search_right = if clear_search.is_empty() {
        inner_right
    } else {
        clear_search.x
    };
    let search_style = Style::default()
        .fg(if app.focus == Focus::Search {
            palette.accent
        } else {
            palette.muted
        })
        .add_modifier(Modifier::BOLD);
    spans.push(Span::styled("Filter:", search_style));
    let query_start = search_start.saturating_add("Filter:".len() as u16);
    let query_right = if clear_search.is_empty() {
        search_right
    } else {
        search_right.saturating_sub(1)
    };
    let query_width = usize::from(query_right.saturating_sub(query_start));
    let rendered_query_width;
    if app.focus == Focus::Search {
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
    let border_color = if matches!(app.focus, Focus::Tasks | Focus::Search) {
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

    let selected_thread = app.selected_thread_id().map(str::to_string);
    let turns = app
        .snapshot
        .turns
        .iter()
        .filter(|turn| {
            selected_thread
                .as_deref()
                .is_some_and(|thread| turn.thread_id == thread)
        })
        .collect::<Vec<_>>();
    app.selected_turn = app.selected_turn.min(turns.len().saturating_sub(1));

    let title_base = if window_only {
        "Turns · current window"
    } else {
        "Turns"
    };
    let turns_focused = app.focus == Focus::Turns;
    let table_block = panel(title_base, app.theme)
        .border_style(Style::default().fg(if turns_focused {
            app.theme.palette().accent
        } else {
            app.theme.palette().border
        }))
        .title_bottom(status_legend(app.theme, table_area.width));
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
        let tokens = if window_only {
            turn.window_token_usage
        } else {
            turn.token_usage
        };
        let model = terminal_safe_text(turn.model.as_deref().unwrap_or("unknown"));
        let effort = terminal_safe_text(turn.reasoning_effort.as_deref().unwrap_or("unknown"));
        let message = terminal_safe_text(turn.message_preview.as_deref().unwrap_or("-"));
        let tone = turn_status_tone(turn.status);
        let mut cells = Vec::new();
        if show_effort_column {
            cells.push(Cell::from(model));
            cells.push(Cell::from(effort));
            cells.push(Cell::from(message));
        } else {
            cells.push(Cell::from(format!("{effort}/{model}")));
            cells.push(Cell::from(message));
        }
        cells.extend([
            Cell::from(format!("{} {}", status_marker(tone), format_tokens(tokens))),
            Cell::from(format!("{:.1}%", turn.local_token_share_percent)),
            Cell::from(format!("{:.1}%", turn.estimated_quota_percent)),
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

    if let Some(detail_area) = detail_area {
        render_turn_detail(
            frame,
            detail_area,
            turns.get(app.selected_turn).copied(),
            app.selected_turn,
            turns.len(),
            window_only,
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
    selected_index: usize,
    turn_count: usize,
    window_only: bool,
    theme: Theme,
) {
    let Some(turn) = turn else {
        frame.render_widget(
            Paragraph::new("No turns for selected task").block(panel("Turn detail", theme)),
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
    let bottom_title = Span::styled(
        format!(" {model} · {effort} "),
        Style::default().fg(theme.palette().muted),
    );
    let content_width = usize::from(area.width.saturating_sub(2));
    let all_tokens = format_token_breakdown("all", turn.token_usage, content_width);
    let window_tokens = format_token_breakdown("5h", turn.window_token_usage, content_width);
    let (first_tokens, second_tokens) = if window_only {
        (window_tokens, all_tokens)
    } else {
        (all_tokens, window_tokens)
    };
    let started = format_turn_timestamp(turn.started_at.as_ref());
    let completed = format_turn_timestamp(turn.completed_at.as_ref());
    let message = terminal_safe_text(turn.message_preview.as_deref().unwrap_or("-"));
    let lines = vec![
        Line::from(first_tokens),
        Line::from(second_tokens),
        Line::from(format!(
            "local={:.1}% · est.quota={:.1}% · confidence={}",
            turn.local_token_share_percent,
            turn.estimated_quota_percent,
            confidence_label(turn.quota_confidence)
        )),
        Line::from(format!(
            "start={started} · end={completed} · duration={duration}"
        )),
        Line::from(format!("turn={}", terminal_safe_text(&turn.turn_id))),
        Line::from(format!("message={message}")),
    ];
    let block = panel(&title, theme).title_bottom(bottom_title);
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

fn render_models(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, theme: Theme) {
    let scope = snapshot
        .attribution
        .window
        .as_ref()
        .map(|window| window.label.as_str());
    if snapshot.models.is_empty() {
        let (title, message) = if let Some(scope) = scope {
            (
                format!("Models · {scope}"),
                format!("No local model usage in the current {scope} window"),
            )
        } else if has_active_window(snapshot, 10_080) && !has_active_window(snapshot, 300) {
            (
                "Models · 5h unavailable".to_string(),
                "5h window unavailable; weekly quota data remains available".to_string(),
            )
        } else {
            (
                "Models · 5h unavailable".to_string(),
                "No active 5h quota window".to_string(),
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
    let mut models = snapshot.models.iter().collect::<Vec<_>>();
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
            Cell::from(format!("{:.1}%", model.estimated_quota_percent)),
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

fn render_attribution(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, theme: Theme) {
    let attribution = &snapshot.attribution;
    let window = attribution
        .window
        .as_ref()
        .map(|window| {
            format!(
                "{} · {:.1}% used · {} to {}",
                window.label,
                window.used_percent,
                window.starts_at.with_timezone(&Local).format("%m-%d %H:%M"),
                window.ends_at.with_timezone(&Local).format("%m-%d %H:%M")
            )
        })
        .unwrap_or_else(|| "No active quota window".to_string());
    let detail = format!(
        "{} local · +{:.2}pp observed · {:.2}pp estimated · {:.2}pp unattributed · {:.0}% coverage · {}{}{}",
        format_tokens(attribution.local_token_usage),
        attribution.observed_delta_percent,
        attribution.estimated_assigned_percent,
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
        }
    );
    frame.render_widget(
        Paragraph::new(vec![Line::from(window), Line::from(detail)])
            .block(panel("Attribution", theme))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn panel(title: &str, theme: Theme) -> Block<'_> {
    let palette = theme.palette();
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.border))
        .title(Span::styled(
            title,
            Style::default()
                .fg(palette.title)
                .add_modifier(Modifier::BOLD),
        ))
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

fn task_display_label(task: &TaskRecord) -> String {
    let project = task_project_name(task).unwrap_or("-");
    let source = task.source.as_deref().unwrap_or("unknown");
    terminal_safe_text(&format!(
        "{project} | {source} | {}t | {}",
        task.turn_count, task.title
    ))
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
        AttributionSummary, CollectionStats, LimitBucket, LimitWindow, ModelUsage, WindowDescriptor,
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

    fn render_models_content(snapshot: &Snapshot, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_models(frame, frame.area(), snapshot, Theme::Dark))
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
        assert!(content.contains("weekly quota data remains available"));
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
            assert_eq!(app.focus, Focus::Search);
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
    fn search_input_has_priority_over_global_shortcuts_and_supports_cancel() {
        let mut app = interaction_test_app(3, 1);
        let initial_theme = app.theme;
        let initial_view = app.view;
        assert!(!handle_key_event(&mut app, key_event(KeyCode::Char('/'))));
        assert_eq!(app.focus, Focus::Search);

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
    fn view_tabs_use_rendered_padding_and_support_mouse_switching() {
        let mut app = interaction_test_app(3, 2);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let tabs = app.view_tabs_hitbox.expect("view tabs should render");
        assert_eq!(tabs.tabs[View::Overview.index()], Rect::new(0, 0, 10, 1));
        assert_eq!(tabs.tabs[View::Window.index()], Rect::new(13, 0, 8, 1));
        assert_eq!(tabs.tabs[View::Health.index()], Rect::new(24, 0, 13, 1));

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
        assert_eq!(app.focus, Focus::Search);
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
