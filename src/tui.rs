use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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
    Block, Borders, Cell, Gauge, Paragraph, Row, Table, TableState, Tabs, Wrap,
};

use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, Confidence, Provenance, Snapshot, TaskRecord, TaskStatus, TokenUsage,
    TurnRecord, terminal_safe_text,
};
use crate::rollout::RolloutCache;
use crate::snapshot::{CollectionResult, collect_snapshot_cached};

const LOCAL_REFRESH: Duration = Duration::from_secs(2);
const ACCOUNT_REFRESH: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Overview,
    Window,
    Health,
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
    view: View,
    selected_task: usize,
    turn_offset: usize,
    worker_running: bool,
    last_local_refresh: Instant,
    last_account_refresh: Instant,
}

impl App {
    fn new(result: CollectionResult) -> Self {
        Self {
            snapshot: result.snapshot,
            account: result.account,
            view: View::Overview,
            selected_task: 0,
            turn_offset: 0,
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

    fn replace(&mut self, result: CollectionResult, refreshed_account: bool) {
        let selected = self.selected_thread_id().map(str::to_string);
        self.snapshot = result.snapshot;
        self.account = result.account;
        self.selected_task = selected
            .as_deref()
            .and_then(|thread_id| {
                self.snapshot
                    .tasks
                    .iter()
                    .position(|task| task.thread_id == thread_id)
            })
            .unwrap_or(0)
            .min(self.snapshot.tasks.len().saturating_sub(1));
        let turn_count = self
            .selected_thread_id()
            .map(|thread_id| {
                self.snapshot
                    .turns
                    .iter()
                    .filter(|turn| turn.thread_id == thread_id)
                    .count()
            })
            .unwrap_or(0);
        self.turn_offset = self.turn_offset.min(turn_count.saturating_sub(1));
        self.worker_running = false;
        self.last_local_refresh = Instant::now();
        if refreshed_account {
            self.last_account_refresh = Instant::now();
        }
    }

    fn select_next(&mut self) {
        if !self.snapshot.tasks.is_empty() {
            self.selected_task = (self.selected_task + 1).min(self.snapshot.tasks.len() - 1);
            self.turn_offset = 0;
        }
    }

    fn select_previous(&mut self) {
        self.selected_task = self.selected_task.saturating_sub(1);
        self.turn_offset = 0;
    }

    fn scroll_turns_down(&mut self) {
        let turn_count = self
            .selected_thread_id()
            .map(|thread_id| {
                self.snapshot
                    .turns
                    .iter()
                    .filter(|turn| turn.thread_id == thread_id)
                    .count()
            })
            .unwrap_or(0);
        self.turn_offset = self
            .turn_offset
            .saturating_add(5)
            .min(turn_count.saturating_sub(1));
    }
}

pub fn run(config: CollectConfig) -> Result<()> {
    let rollout_cache = Arc::new(Mutex::new(RolloutCache::new()));
    let initial = {
        let mut cache = rollout_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        collect_snapshot_cached(&config, None, true, &mut cache)
    };
    let mut app = App::new(initial);
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

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Tab | KeyCode::Right => app.view = app.view.next(),
                KeyCode::BackTab | KeyCode::Left => app.view = app.view.previous(),
                KeyCode::Char('1') => app.view = View::Overview,
                KeyCode::Char('2') => app.view = View::Window,
                KeyCode::Char('3') => app.view = View::Health,
                KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
                KeyCode::Home => {
                    app.selected_task = 0;
                    app.turn_offset = 0;
                }
                KeyCode::End => {
                    app.selected_task = app.snapshot.tasks.len().saturating_sub(1);
                    app.turn_offset = 0;
                }
                KeyCode::PageDown => app.scroll_turns_down(),
                KeyCode::PageUp => app.turn_offset = app.turn_offset.saturating_sub(5),
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
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
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
            Constraint::Length(if compact { 4 } else { 5 }),
            Constraint::Min(10),
            Constraint::Length(if compact { 5 } else { 7 }),
        ])
        .split(area);
    render_limits(frame, rows[0], &app.snapshot);

    let body = Layout::default()
        .direction(if area.width < 100 {
            Direction::Vertical
        } else {
            Direction::Horizontal
        })
        .constraints([Constraint::Percentage(54), Constraint::Percentage(46)])
        .split(rows[1]);
    render_tasks(frame, body[0], app, false);
    render_turns(frame, body[1], app, false);
    render_models(frame, rows[2], &app.snapshot);
}

fn render_window(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let compact = area.height < 30;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if compact { 4 } else { 5 }),
            Constraint::Length(5),
            Constraint::Min(9),
            Constraint::Length(if compact { 5 } else { 7 }),
        ])
        .split(area);
    render_limits(frame, rows[0], &app.snapshot);
    render_attribution(frame, rows[1], &app.snapshot);

    let body = Layout::default()
        .direction(if area.width < 100 {
            Direction::Vertical
        } else {
            Direction::Horizontal
        })
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(rows[2]);
    render_tasks(frame, body[0], app, true);
    render_turns(frame, body[1], app, true);
    render_models(frame, rows[3], &app.snapshot);
}

fn render_health(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
    .header(table_header(["SOURCE", "STATE", "AS OF", "DETAIL"]))
    .block(panel("Sources"));
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
        Paragraph::new(stats_text).block(panel("Collection")),
        rows[1],
    );

    let issues = app
        .snapshot
        .errors
        .iter()
        .map(|value| {
            Line::from(Span::styled(
                terminal_safe_text(value),
                Style::default().fg(Color::Red),
            ))
        })
        .chain(app.snapshot.warnings.iter().map(|value| {
            Line::from(Span::styled(
                terminal_safe_text(value),
                Style::default().fg(Color::Yellow),
            ))
        }))
        .collect::<Vec<_>>();
    let issues = if issues.is_empty() {
        vec![Line::from(Span::styled(
            "No collection issues",
            Style::default().fg(Color::Green),
        ))]
    } else {
        issues
    };
    frame.render_widget(
        Paragraph::new(issues)
            .block(panel("Diagnostics"))
            .wrap(Wrap { trim: true }),
        rows[2],
    );
}

fn render_limits(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
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
                .block(panel("Quota")),
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
        let color = quota_color(window.used_percent);
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
            .block(panel(&title))
            .gauge_style(Style::default().fg(color).bg(Color::Black))
            .ratio((window.used_percent / 100.0).clamp(0.0, 1.0))
            .label(label);
        frame.render_widget(gauge, columns[index]);
    }
}

fn render_tasks(frame: &mut Frame<'_>, area: Rect, app: &mut App, window_only: bool) {
    let task_rows = app.snapshot.tasks.iter().map(|task| {
        let tokens = if window_only {
            task.window_token_usage
        } else {
            task.token_usage
        };
        Row::new([
            Cell::from(format!(
                "{} {}",
                task.status.label(),
                status_evidence(task.status_provenance, task.status_confidence)
            ))
            .style(status_style(task.status)),
            Cell::from(format_tokens(tokens)),
            Cell::from(format!("{:.1}%", task.local_token_share_percent)),
            Cell::from(format!("{:.1}%", task.estimated_quota_percent)),
            Cell::from(task_display_label(task)),
        ])
    });

    let table = Table::new(
        task_rows,
        [
            Constraint::Length(15),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(12),
        ],
    )
    .header(table_header([
        "STATE EVIDENCE",
        "TOKENS",
        "LOCAL5H",
        "EST.Q5H",
        "TASK",
    ]))
    .block(panel(if window_only {
        "Tasks · current window"
    } else {
        "Recent tasks"
    }))
    .row_highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
    .highlight_symbol("▌");

    let mut state = TableState::default().with_selected(if app.snapshot.tasks.is_empty() {
        None
    } else {
        Some(app.selected_task)
    });
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_turns(frame: &mut Frame<'_>, area: Rect, app: &App, window_only: bool) {
    let selected = app.selected_thread_id();
    let turns = app
        .snapshot
        .turns
        .iter()
        .filter(|turn| selected.is_some_and(|thread| turn.thread_id == thread))
        .skip(app.turn_offset);

    let rows = turns.map(|turn| {
        let tokens = if window_only {
            turn.window_token_usage
        } else {
            turn.token_usage
        };
        Row::new([
            Cell::from(turn_status_label(turn)),
            Cell::from(terminal_safe_text(
                turn.model.as_deref().unwrap_or("unknown"),
            )),
            Cell::from(format_tokens(tokens)),
            Cell::from(format!("{:.1}%", turn.local_token_share_percent)),
            Cell::from(format!("{:.1}%", turn.estimated_quota_percent)),
        ])
    });
    let title = if window_only {
        if app.turn_offset == 0 {
            "Turns · current window".to_string()
        } else {
            format!("Turns · current window · +{}", app.turn_offset)
        }
    } else if app.turn_offset == 0 {
        "Turns".to_string()
    } else {
        format!("Turns · +{}", app.turn_offset)
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Min(12),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
        ],
    )
    .header(table_header(["STATE", "MODEL", "TOKENS", "LOCAL", "EST.Q"]))
    .block(panel(&title));
    frame.render_widget(table, area);
}

fn render_models(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let rows = snapshot.models.iter().map(|model| {
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
    .header(table_header([
        "MODEL",
        "TOKENS",
        "LOCAL SHARE",
        "EST. QUOTA",
        "CONF",
    ]))
    .block(panel("Models · current window"));
    frame.render_widget(table, area);
}

fn render_attribution(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
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
            .block(panel("Attribution"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
}

fn table_header<const N: usize>(labels: [&str; N]) -> Row<'static> {
    Row::new(labels.map(|label| Cell::from(label.to_string()))).style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn quota_color(used_percent: f64) -> Color {
    if used_percent >= 90.0 {
        Color::Red
    } else if used_percent >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn status_style(status: TaskStatus) -> Style {
    let color = match status {
        TaskStatus::Running => Color::Green,
        TaskStatus::WaitingApproval | TaskStatus::WaitingInput => Color::Yellow,
        TaskStatus::Completed | TaskStatus::Idle => Color::Cyan,
        TaskStatus::Interrupted | TaskStatus::Stale | TaskStatus::Unknown => Color::DarkGray,
        TaskStatus::Failed => Color::Red,
    };
    Style::default().fg(color)
}

fn turn_status_label(turn: &TurnRecord) -> String {
    format!("{:?}", turn.status).to_uppercase()
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
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AttributionSummary, CollectionStats, LimitBucket, LimitWindow};
    use ratatui::backend::TestBackend;

    #[test]
    fn quota_thresholds_are_distinct() {
        assert_eq!(quota_color(50.0), Color::Green);
        assert_eq!(quota_color(75.0), Color::Yellow);
        assert_eq!(quota_color(95.0), Color::Red);
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
                tasks: Vec::new(),
                turns: Vec::new(),
                models: Vec::new(),
                attribution: AttributionSummary::default(),
                stats: CollectionStats::default(),
                warnings: Vec::new(),
                errors: Vec::new(),
            };
            let mut app = App::new(CollectionResult {
                snapshot,
                account: AccountSnapshot::default(),
            });
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();

            for view in [View::Overview, View::Window, View::Health] {
                app.view = view;
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
            }

            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(content.contains("Data health"));
        }
    }
}
