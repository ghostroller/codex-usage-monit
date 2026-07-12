use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
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
    selected: Color,
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
                selected: Color::Rgb(255, 255, 255),
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
                selected: Color::Rgb(0, 108, 117),
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
    selected_task: usize,
    selected_turn: usize,
    turn_offset: usize,
    task_table_offset: usize,
    task_reveal_pending: bool,
    task_table_hitbox: Option<TableHitbox>,
    turn_table_hitbox: Option<TableHitbox>,
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
            selected_task: 0,
            selected_turn: 0,
            turn_offset: 0,
            task_table_offset: 0,
            task_reveal_pending: false,
            task_table_hitbox: None,
            turn_table_hitbox: None,
            worker_running: false,
            last_local_refresh: Instant::now(),
            last_account_refresh: Instant::now(),
        }
    }

    fn selected_thread_id(&self) -> Option<&str> {
        self.snapshot
            .tasks
            .get(self.selected_task)
            .map(|task| task.thread_id.as_str())
    }

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

    fn replace(&mut self, result: CollectionResult, refreshed_account: bool) {
        let selected = self.selected_thread_id().map(str::to_string);
        let selected_turn_id = self.selected_turn_record().map(|turn| turn.turn_id.clone());
        self.snapshot = result.snapshot;
        self.account = result.account;
        self.task_table_hitbox = None;
        self.turn_table_hitbox = None;
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
        self.worker_running = false;
        self.last_local_refresh = Instant::now();
        if refreshed_account {
            self.last_account_refresh = Instant::now();
        }
    }

    fn select_next(&mut self) {
        if !self.snapshot.tasks.is_empty() {
            let target = (self.selected_task + 1).min(self.snapshot.tasks.len() - 1);
            self.select_task(target, true);
        }
    }

    fn select_previous(&mut self) {
        self.select_task(self.selected_task.saturating_sub(1), true);
    }

    fn select_first_task(&mut self) {
        self.select_task(0, true);
    }

    fn select_last_task(&mut self) {
        self.select_task(self.snapshot.tasks.len().saturating_sub(1), true);
    }

    fn select_task(&mut self, index: usize, reveal: bool) -> bool {
        if index >= self.snapshot.tasks.len() {
            return false;
        }
        if self.selected_task != index {
            self.selected_task = index;
            self.selected_turn = 0;
            self.turn_offset = 0;
        }
        if reveal {
            if let Some(hitbox) = self.task_table_hitbox {
                self.task_table_offset = reveal_offset(
                    self.task_table_offset,
                    index,
                    self.snapshot.tasks.len(),
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
            self.snapshot.tasks.len(),
            hitbox.capacity,
            down,
            lines,
        );
    }

    fn scroll_turns(&mut self, down: bool, lines: usize) {
        let Some(hitbox) = self.turn_table_hitbox else {
            return;
        };
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
        let Some(index) = self
            .task_table_hitbox
            .and_then(|hitbox| hitbox.index_at(column, row))
            .filter(|index| *index < self.snapshot.tasks.len())
        else {
            return false;
        };
        self.select_task(index, false)
    }

    fn select_turn_at(&mut self, column: u16, row: u16) -> bool {
        let Some(index) = self
            .turn_table_hitbox
            .and_then(|hitbox| hitbox.index_at(column, row))
            .filter(|index| *index < self.selected_task_turn_count())
        else {
            return false;
        };
        self.selected_turn = index;
        true
    }
}

fn handle_mouse_event(app: &mut App, event: MouseEvent) -> bool {
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.select_turn_at(event.column, event.row)
                || app.select_task_at(event.column, event.row)
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
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Tab | KeyCode::Right => app.view = app.view.next(),
                    KeyCode::BackTab | KeyCode::Left => app.view = app.view.previous(),
                    KeyCode::Char('1') => app.view = View::Overview,
                    KeyCode::Char('2') => app.view = View::Window,
                    KeyCode::Char('3') => app.view = View::Health,
                    KeyCode::Char('t') => app.toggle_theme(),
                    KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                    KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
                    KeyCode::Home => app.select_first_task(),
                    KeyCode::End => app.select_last_task(),
                    KeyCode::PageDown => app.scroll_turns(true, PAGE_SCROLL_LINES),
                    KeyCode::PageUp => app.scroll_turns(false, PAGE_SCROLL_LINES),
                    _ => {}
                },
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
    let palette = app.theme.palette();
    frame.render_widget(Block::default().style(app.theme.base_style()), area);
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let titles = ["Overview", "Window", "Data health"]
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    let tabs = Tabs::new(titles)
        .select(app.view.index())
        .style(Style::default().fg(palette.muted))
        .highlight_style(
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::styled(" | ", Style::default().fg(palette.muted)));
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
    let base_title = if window_only {
        "Tasks · current window"
    } else {
        "Recent tasks"
    };
    let title = app
        .snapshot
        .tasks
        .get(app.selected_task)
        .map(|task| {
            format!(
                "{base_title} · {} {}",
                task.status.label(),
                status_evidence(task.status_provenance, task.status_confidence)
            )
        })
        .unwrap_or_else(|| base_title.to_string());

    let block = panel(&title, app.theme);
    let table_inner = block.inner(area);
    let visible_capacity = usize::from(table_inner.height.saturating_sub(1));
    app.task_table_offset = app
        .task_table_offset
        .min(app.snapshot.tasks.len().saturating_sub(visible_capacity));
    if app.task_reveal_pending {
        app.task_table_offset = reveal_offset(
            app.task_table_offset,
            app.selected_task,
            app.snapshot.tasks.len(),
            visible_capacity,
        );
        app.task_reveal_pending = false;
    }
    let offset = app.task_table_offset;
    let selected_in_view = app
        .selected_task
        .checked_sub(offset)
        .filter(|index| *index < visible_capacity);
    let theme = app.theme;
    let task_rows = app.snapshot.tasks.iter().skip(offset).map(|task| {
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
            .fg(app.theme.palette().selected)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_spacing(HighlightSpacing::Always)
    .highlight_symbol("▌");

    let mut state = TableState::default().with_selected(selected_in_view);
    frame.render_stateful_widget(table, area, &mut state);

    let remaining_rows = app.snapshot.tasks.len().saturating_sub(offset);
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
    let table_block =
        panel(title_base, app.theme).title_bottom(status_legend(app.theme, table_area.width));
    let table_inner = table_block.inner(table_area);
    let visible_capacity = usize::from(table_inner.height.saturating_sub(1));
    app.turn_offset = app
        .turn_offset
        .min(turns.len().saturating_sub(visible_capacity));
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
                .fg(theme.palette().selected)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_symbol("▌");
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
    let project = task
        .cwd
        .as_deref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("-");
    let source = task.source.as_deref().unwrap_or("unknown");
    terminal_safe_text(&format!(
        "{project} | {source} | {}t | {}",
        task.turn_count, task.title
    ))
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
                                cell.fg == palette.selected
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
