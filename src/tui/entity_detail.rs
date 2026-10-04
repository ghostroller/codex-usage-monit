use super::*;
use crate::summary_report::SummaryCoverageState;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug)]
pub(super) struct EntityDetailPopup {
    pub(super) title: String,
    pub(super) lines: Vec<Line<'static>>,
    pub(super) offset: usize,
    pub(super) line_count: usize,
    receiver: Option<Receiver<crate::session_details::SessionDetails>>,
    cancel: Option<Arc<AtomicBool>>,
    wrapped: Vec<Line<'static>>,
    wrapped_width: Option<u16>,
}

impl Drop for EntityDetailPopup {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct EntityDetailHitbox {
    pub(super) content: Rect,
    pub(super) scrollbar: Option<ScrollbarHitbox>,
    pub(super) up: Rect,
    pub(super) down: Rect,
    pub(super) back: Rect,
}

impl App {
    pub(super) fn entity_detail_available(&self) -> bool {
        match self.view {
            View::Overview if !self.focus.is_search() => match self.focus {
                Focus::Tasks => self.selected_task_record().is_some(),
                Focus::Turns => self.turns_visible() && self.selected_turn_record().is_some(),
                _ => false,
            },
            View::Summary => {
                let rows = self.summary_rows();
                rows.get(self.summary_selected_index(&rows))
                    .is_some_and(|row| row.kind != SummaryRowKind::Project)
            }
            _ => false,
        }
    }

    pub(super) fn open_entity_detail(&mut self) -> bool {
        if !self.entity_detail_available() {
            return false;
        }
        let detail = match self.view {
            View::Overview if self.focus == Focus::Tasks => self
                .selected_task_record()
                .map(|task| task_detail(self, task)),
            View::Overview => self
                .selected_turn_record()
                .map(|turn| turn_detail(self, turn)),
            View::Summary => summary_detail(self),
            _ => None,
        };
        let Some(detail) = detail else {
            return false;
        };
        self.entity_detail = Some(detail);
        self.entity_detail_hitbox = None;
        self.scroll_drag = None;
        self.trend_drag = None;
        self.summary_daily_dragging = false;
        self.start_entity_detail_enrichment();
        true
    }

    fn start_entity_detail_enrichment(&mut self) {
        let target = detail_enrichment_target(self);
        let Some(detail) = self.entity_detail.as_mut() else {
            return;
        };
        let Some(DetailReadTarget {
            thread_id,
            turn_id,
            starts_at,
            as_of,
        }) = target
        else {
            detail.lines.push(Line::default());
            detail.lines.push(Line::from(
                "Recorded details: unavailable (no unambiguous local rollout association)",
            ));
            return;
        };
        let codex_home = self.snapshot.codex_home.clone();
        let redact = self.local_redact_content;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        detail.lines.push(Line::default());
        detail.lines.push(Line::from(
            "Recorded details: loading local evidence at capture...",
        ));
        detail.receiver = Some(receiver);
        detail.cancel = Some(cancel);
        thread::spawn(move || {
            let data = crate::session_details::load_session_details_in_range(
                &codex_home,
                &thread_id,
                turn_id.as_deref(),
                redact,
                starts_at,
                Some(as_of),
                &worker_cancel,
            );
            if !worker_cancel.load(Ordering::Relaxed) {
                let _ = sender.send(data);
            }
        });
    }

    pub(super) fn poll_entity_detail(&mut self) -> bool {
        let Some(detail) = self.entity_detail.as_mut() else {
            return false;
        };
        let Some(receiver) = detail.receiver.as_ref() else {
            return false;
        };
        match receiver.try_recv() {
            Ok(data) => {
                detail.receiver = None;
                detail.lines.pop();
                detail.lines.push(Line::styled(
                    "Recorded details (at capture)",
                    Style::default()
                        .fg(self.theme.palette().title)
                        .add_modifier(Modifier::BOLD),
                ));
                append_enrichment(&mut detail.lines, data.display_lines());
                detail.wrapped_width = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                detail.receiver = None;
                detail.lines.pop();
                detail.lines.push(Line::from(
                    "Recorded details: unavailable (reader did not return evidence)",
                ));
                detail.wrapped_width = None;
                true
            }
        }
    }

    pub(super) fn entity_detail_loading(&self) -> bool {
        self.entity_detail
            .as_ref()
            .is_some_and(|detail| detail.receiver.is_some())
    }

    pub(super) fn close_entity_detail(&mut self) {
        self.entity_detail = None;
        self.entity_detail_hitbox = None;
        self.scroll_drag = None;
    }

    pub(super) fn scroll_entity_detail(&mut self, down: bool, lines: usize) {
        let capacity = self
            .entity_detail_hitbox
            .map_or(0, |hitbox| usize::from(hitbox.content.height));
        if let Some(detail) = self.entity_detail.as_mut() {
            detail.offset = scroll_offset(detail.offset, detail.line_count, capacity, down, lines);
        }
    }
}

fn is_local_entity(app: &App, thread_id: &str, source: Option<&str>, cwd: Option<&Path>) -> bool {
    if thread_id.starts_with("remote:")
        || source.is_some_and(|source| source.starts_with("remote:"))
    {
        return false;
    }
    app.local_snapshot.tasks.iter().any(|task| {
        task.thread_id == thread_id
            && task.source.as_deref() == source
            && task.cwd.as_deref() == cwd
    })
}

struct DetailReadTarget {
    thread_id: String,
    turn_id: Option<String>,
    starts_at: Option<DateTime<Utc>>,
    as_of: DateTime<Utc>,
}

fn raw_local_history_identity<'a>(app: &App, identity: &'a str) -> Option<&'a str> {
    // v2 history scopes each raw ID to its source. Only the known local source
    // can be reversed; logical copy identities have no raw-file authority here.
    if identity.is_empty() || identity.starts_with("logical-thread:") {
        return None;
    }
    if let Some(local_source) = &app.history_local_source_id
        && let Some(raw) = identity.strip_suffix(&format!("@{}", local_source.as_str()))
    {
        return (!raw.is_empty()).then_some(raw);
    }
    // Legacy history has unscoped IDs. An unknown scope must not be mistaken
    // for a raw local ID, even when its prefix resembles a current session.
    (!identity.contains('@')).then_some(identity)
}

fn detail_enrichment_target(app: &App) -> Option<DetailReadTarget> {
    match app.view {
        View::Overview => {
            let task = app.selected_task_record()?;
            if !is_local_entity(
                app,
                &task.thread_id,
                task.source.as_deref(),
                task.cwd.as_deref(),
            ) {
                return None;
            }
            let turn_id = if app.focus == Focus::Turns {
                Some(app.selected_turn_record()?.turn_id.clone())
            } else {
                None
            };
            Some(DetailReadTarget {
                thread_id: task.thread_id.clone(),
                turn_id,
                starts_at: None,
                as_of: app.snapshot.as_of,
            })
        }
        View::Summary => {
            if matches!(
                app.history_source_applied_selection,
                HistorySourceSelection::Remote(_)
            ) || (matches!(
                app.history_source_applied_selection,
                HistorySourceSelection::AllIncluded
            ) && !app.history_remote_sources.is_empty())
            {
                return None;
            }
            if let HistorySourceSelection::Local(source) = &app.history_source_applied_selection
                && app.history_local_source_id.as_ref() != Some(source)
            {
                return None;
            }
            let cache = app.summary_cache.as_ref()?;
            let rows = app.summary_rows();
            let index = app.summary_selected_index(&rows);
            let selected = rows.get(index)?;
            let project_id = selected_summary_project_id(&rows, index)?;
            for project in &cache.prepared.usage.projects {
                if summary_project_node_id(&project.key) != project_id {
                    continue;
                }
                for session in &project.sessions {
                    // The applied history scope above proves local origin even
                    // when a historical session has aged out of Overview.
                    // The reader independently verifies session_meta ownership.
                    if session.thread_id.starts_with("remote:")
                        || session
                            .source
                            .as_deref()
                            .is_some_and(|source| source.starts_with("remote:"))
                    {
                        continue;
                    }
                    let Some(raw_thread_id) = raw_local_history_identity(app, &session.thread_id)
                    else {
                        continue;
                    };
                    // Summary intervals exclude their end; the raw reader's
                    // capture cutoff is inclusive.
                    let range_last = cache
                        .prepared
                        .usage
                        .window
                        .ends_at
                        .checked_sub_signed(ChronoDuration::nanoseconds(1))?;
                    let as_of = cache.snapshot_as_of.min(range_last);
                    if selected.kind == SummaryRowKind::Session
                        && selected.id == summary_thread_node_id(&session.thread_id)
                    {
                        return Some(DetailReadTarget {
                            thread_id: raw_thread_id.to_owned(),
                            turn_id: None,
                            starts_at: Some(cache.prepared.usage.window.starts_at),
                            as_of,
                        });
                    }
                    for turn in &session.turns {
                        if selected.kind == SummaryRowKind::Turn
                            && selected.id == summary_turn_node_id(&session.thread_id, &turn.key)
                            && let SummaryTurnKey::Exact(turn_id) = &turn.key
                        {
                            let raw_turn_id = raw_local_history_identity(app, turn_id)?;
                            return Some(DetailReadTarget {
                                thread_id: raw_thread_id.to_owned(),
                                turn_id: Some(raw_turn_id.to_owned()),
                                starts_at: Some(cache.prepared.usage.window.starts_at),
                                as_of,
                            });
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

struct DetailLines {
    lines: Vec<Line<'static>>,
    heading_style: Style,
}

impl DetailLines {
    fn new(app: &App) -> Self {
        Self {
            lines: Vec::new(),
            heading_style: Style::default()
                .fg(app.theme.palette().title)
                .add_modifier(Modifier::BOLD),
        }
    }

    fn section(&mut self, title: &str) {
        if !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.lines
            .push(Line::styled(title.to_owned(), self.heading_style));
    }

    fn field(&mut self, name: &str, value: impl std::fmt::Display) {
        self.lines.push(Line::from(format!(
            "{name}: {}",
            terminal_safe_text(&value.to_string())
        )));
    }

    fn note(&mut self, value: &str) {
        self.lines.push(Line::from(terminal_safe_text(value)));
    }

    fn finish(self, title: &str) -> EntityDetailPopup {
        EntityDetailPopup {
            title: title.to_owned(),
            lines: self.lines,
            offset: 0,
            line_count: 0,
            receiver: None,
            cancel: None,
            wrapped: Vec::new(),
            wrapped_width: None,
        }
    }
}

fn optional_text(value: Option<&str>) -> &str {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unavailable")
}

fn timestamp(value: Option<DateTime<Utc>>) -> String {
    value.map_or_else(
        || "unavailable".to_string(),
        |value| {
            value
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        },
    )
}

fn duration(value: Option<u64>) -> String {
    value.map_or_else(
        || "unavailable".to_string(),
        |value| format!("{value} ms ({})", format_duration(Some(value))),
    )
}

fn elapsed(start: Option<DateTime<Utc>>, end: DateTime<Utc>) -> Option<u64> {
    start.and_then(|start| {
        end.signed_duration_since(start)
            .num_milliseconds()
            .try_into()
            .ok()
    })
}

fn tokens(lines: &mut DetailLines, label: &str, usage: TokenUsage) {
    lines.field(label, usage.total_tokens);
    lines.field("Input", usage.input_tokens);
    lines.field("Cached input", usage.cached_input_tokens);
    lines.field("Cache write input", usage.cache_write_input_tokens);
    lines.field("Output", usage.output_tokens);
    lines.field("Reasoning output", usage.reasoning_output_tokens);
    lines.field("Unclassified", usage.unclassified());
    lines.field(
        "Input / total",
        ratio(usage.input_tokens, usage.total_tokens),
    );
    lines.field(
        "Output / total",
        ratio(usage.output_tokens, usage.total_tokens),
    );
    lines.field(
        "Cache read / input",
        ratio(usage.cached_input_tokens, usage.input_tokens),
    );
    lines.field(
        "Cache write / input",
        ratio(usage.cache_write_input_tokens, usage.input_tokens),
    );
    lines.field(
        "Reasoning / output",
        ratio(usage.reasoning_output_tokens, usage.output_tokens),
    );
}

fn ratio(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        "unavailable (zero denominator)".to_string()
    } else {
        format!("{:.2}%", numerator as f64 / denominator as f64 * 100.0)
    }
}

fn api_amount(
    lines: &mut DetailLines,
    prefix: &str,
    amount: ApiCostAmount,
    state: ApiCostWindowState,
) {
    lines.field(
        &format!("{prefix} API equivalent"),
        format_scoped_api_cost_amount(state, amount),
    );
    lines.field(
        "Priced token coverage",
        format!(
            "{} / {} ({})",
            amount.priced_tokens,
            amount.observed_tokens,
            ratio(amount.priced_tokens, amount.observed_tokens)
        ),
    );
    lines.field(
        "Usage samples",
        format!(
            "{} observed; {} priced",
            amount.observed_samples, amount.priced_samples
        ),
    );
}

fn scope_usage(
    lines: &mut DetailLines,
    label: &str,
    usage: WindowUsage,
    state: ApiCostWindowState,
) {
    tokens(lines, &format!("{label} window tokens"), usage.token_usage);
    lines.field(
        &format!("{label} TOKEN%"),
        format!("{:.4}%", usage.local_token_share_percent),
    );
    lines.field(
        &format!("{label} estimated quota"),
        format_estimated_quota(usage.estimated_quota_percent, usage.quota_confidence),
    );
    lines.field("Quota confidence", format!("{:?}", usage.quota_confidence));
    api_amount(lines, label, usage.api_equivalent_cost, state);
}

fn workspace_label(source: Option<&str>) -> &'static str {
    if source.is_some_and(|source| source.starts_with("remote:")) {
        "Project/workspace label"
    } else {
        "Workspace"
    }
}

fn detail_task_window_usage(app: &App, task: &TaskRecord) -> WindowUsage {
    let mut usage = task_usage_for_scope_with_api_long_context(
        &app.snapshot,
        app.window_scope,
        task,
        app.api_long_context_multiplier,
    );
    usage.api_equivalent_cost =
        task_usage_for_scope_with_api_long_context(&app.snapshot, app.window_scope, task, false)
            .api_equivalent_cost;
    usage
}

fn detail_turn_window_usage(app: &App, turn: &TurnRecord) -> WindowUsage {
    let mut usage = turn_usage_for_scope_with_api_long_context(
        &app.snapshot,
        app.window_scope,
        turn,
        app.api_long_context_multiplier,
    );
    usage.api_equivalent_cost =
        turn_usage_for_scope_with_api_long_context(&app.snapshot, app.window_scope, turn, false)
            .api_equivalent_cost;
    usage
}

fn window_description(lines: &mut DetailLines, app: &App) -> bool {
    lines.field("Selected scope", app.window_scope.label());
    lines.field(
        "EST Longx",
        if app.api_long_context_multiplier {
            "enabled"
        } else {
            "disabled"
        },
    );
    if let Some(attribution) = attribution_for_scope_with_api_long_context(
        &app.snapshot,
        app.window_scope,
        app.api_long_context_multiplier,
    ) && let Some(window) = attribution.window.as_ref()
    {
        lines.field("Cycle start", timestamp(Some(window.starts_at)));
        lines.field("Cycle end", timestamp(Some(window.ends_at)));
        lines.field("Account gauge", format!("{:.2}%", window.used_percent));
        lines.field("Attribution method", &attribution.method);
        lines.field(
            "External activity possible",
            attribution.external_activity_possible,
        );
        true
    } else {
        lines.field("Selected window", "unavailable");
        false
    }
}

fn add_usage(left: &mut WindowUsage, right: WindowUsage) {
    let left_participates = quota_estimate_participates(left);
    left.token_usage.add_assign(right.token_usage);
    left.local_token_share_percent += right.local_token_share_percent;
    left.estimated_quota_percent += right.estimated_quota_percent;
    left.api_equivalent_cost
        .add_assign(right.api_equivalent_cost);
    if quota_estimate_participates(&right) {
        left.quota_confidence = if left_participates {
            weakest_quota_confidence(left.quota_confidence, right.quota_confidence)
        } else {
            right.quota_confidence
        };
    }
}

/// Resolve the complete trustworthy subtree independently of filters/collapse.
fn descendants<'a>(app: &'a App, root: &TaskRecord) -> Vec<&'a TaskRecord> {
    let mut seen = HashSet::from([root.thread_id.as_str()]);
    let mut pending = vec![root.thread_id.as_str()];
    let mut result = Vec::new();
    while let Some(parent_id) = pending.pop() {
        let Some(parent) = app
            .snapshot
            .tasks
            .iter()
            .find(|task| task.thread_id == parent_id)
        else {
            continue;
        };
        for child in &app.snapshot.tasks {
            if child.parent_thread_id.as_deref() == Some(parent_id)
                && app.trusts_task_parent_edge(child, parent)
                && seen.insert(child.thread_id.as_str())
            {
                result.push(child);
                pending.push(child.thread_id.as_str());
            }
        }
    }
    result.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    result
}

fn task_detail(app: &App, task: &TaskRecord) -> EntityDetailPopup {
    let mut lines = DetailLines::new(app);
    lines.section("Overview");
    lines.field("Captured at", timestamp(Some(app.snapshot.as_of)));
    lines.field("Title", &task.title);
    lines.field("Thread ID", &task.thread_id);
    lines.field(
        workspace_label(task.source.as_deref()),
        task.cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unavailable".to_string()),
    );
    lines.field("Source", optional_text(task.source.as_deref()));
    lines.field("Archived", task.archived);
    lines.field("Status", format!("{:?}", task.status));
    lines.field("Status provenance", format!("{:?}", task.status_provenance));
    lines.field("Status confidence", format!("{:?}", task.status_confidence));
    lines.field("Created", timestamp(task.created_at));
    lines.field("Last activity", timestamp(task.updated_at));
    lines.field(
        "Since last activity",
        duration(elapsed(task.updated_at, app.snapshot.as_of)),
    );
    lines.field(
        "Session span",
        duration(
            task.created_at
                .zip(task.updated_at)
                .and_then(|(start, end)| elapsed(Some(start), end)),
        ),
    );
    lines.field("Observed turns", task.turn_count);
    let own_turns = app
        .snapshot
        .turns
        .iter()
        .filter(|turn| turn.thread_id == task.thread_id)
        .collect::<Vec<_>>();
    turn_statistics(&mut lines, app, &own_turns);
    let children = descendants(app, task);
    let mut delegated = TokenUsage::default();
    for child in &children {
        delegated.add_assign(child.token_usage);
    }
    let mut total = task.token_usage;
    total.add_assign(delegated);
    lines.section("Usage");
    tokens(&mut lines, "Own cumulative tokens", task.token_usage);
    tokens(&mut lines, "Delegated cumulative tokens", delegated);
    tokens(&mut lines, "Total cumulative tokens", total);
    if window_description(&mut lines, app) {
        let own = detail_task_window_usage(app, task);
        let mut delegated = WindowUsage::default();
        for child in &children {
            add_usage(&mut delegated, detail_task_window_usage(app, child));
        }
        let mut total = own;
        add_usage(&mut total, delegated);
        let state = api_cost_window_state(window_analysis(&app.snapshot, app.window_scope));
        scope_usage(&mut lines, "Own", own, state);
        scope_usage(&mut lines, "Delegated", delegated, state);
        scope_usage(&mut lines, "Total", total, state);
    }
    lines.section("Related");
    lines.field(
        "Parent thread",
        optional_text(task.parent_thread_id.as_deref()),
    );
    lines.field(
        "Direct child agents",
        children
            .iter()
            .filter(|child| child.parent_thread_id.as_deref() == Some(task.thread_id.as_str()))
            .count(),
    );
    lines.field("Delegated descendants", children.len());
    lines.field(
        "Subtree sessions (including self)",
        children.len().saturating_add(1),
    );
    for child in children {
        lines.field("Child thread", &child.thread_id);
        lines.field("Child title", &child.title);
        lines.field("Child status", format!("{:?}", child.status));
        let models = app
            .snapshot
            .turns
            .iter()
            .filter(|turn| turn.thread_id == child.thread_id)
            .filter_map(|turn| turn.model.as_deref())
            .collect::<BTreeSet<_>>();
        lines.field(
            "Child models (turn metadata)",
            if models.is_empty() {
                "unavailable".to_string()
            } else {
                models.into_iter().collect::<Vec<_>>().join(", ")
            },
        );
        lines.field(
            "Child own cumulative tokens",
            child.token_usage.total_tokens,
        );
        if attribution_for_scope_with_api_long_context(
            &app.snapshot,
            app.window_scope,
            app.api_long_context_multiplier,
        )
        .is_some()
        {
            let usage = detail_task_window_usage(app, child);
            lines.field("Child own window tokens", usage.token_usage.total_tokens);
            lines.field(
                "Child priced subtotal",
                format_scoped_api_cost_amount(
                    api_cost_window_state(window_analysis(&app.snapshot, app.window_scope)),
                    usage.api_equivalent_cost,
                ),
            );
        }
    }
    lines.section("Data notes");
    overview_notes(&mut lines, app);
    lines.note("Own/delegated/total values use the complete trusted subtree, independent of tree expansion and search filters.");
    lines.note("Session span includes idle gaps. Completed turn durations may overlap; their sum is not wall-clock work time.");
    lines.finish("Session details")
}

fn turn_statistics(lines: &mut DetailLines, app: &App, turns: &[&TurnRecord]) {
    for status in [
        TurnStatus::InProgress,
        TurnStatus::Completed,
        TurnStatus::Interrupted,
        TurnStatus::Failed,
        TurnStatus::Stale,
        TurnStatus::Unknown,
    ] {
        lines.field(
            &format!("Turns {}", status.label()),
            turns.iter().filter(|turn| turn.status == status).count(),
        );
    }
    let completed_turns = turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Completed)
        .collect::<Vec<_>>();
    let known_durations = completed_turns
        .iter()
        .filter_map(|turn| turn.duration_ms)
        .collect::<Vec<_>>();
    let completed = known_durations
        .iter()
        .copied()
        .fold(0u64, u64::saturating_add);
    lines.field(
        "Completed turn duration sum",
        if !completed_turns.is_empty() && known_durations.is_empty() {
            "unavailable".to_string()
        } else {
            format!(
                "{} ({} / {} completed turns have durations)",
                duration(Some(completed)),
                known_durations.len(),
                completed_turns.len()
            )
        },
    );
    if let Some(turn) = turns
        .iter()
        .max_by_key(|turn| turn.token_usage.total_tokens)
    {
        lines.field(
            "Largest known turn",
            format!(
                "{} ({} tokens)",
                turn.turn_id, turn.token_usage.total_tokens
            ),
        );
    } else {
        lines.field("Largest known turn", "unavailable");
    }
    if let Some(turn) = turns
        .iter()
        .max_by_key(|turn| turn.completed_at.or(turn.started_at))
    {
        lines.field(
            "Most recent known turn",
            format!(
                "{} ({})",
                turn.turn_id,
                timestamp(turn.completed_at.or(turn.started_at))
            ),
        );
    } else {
        lines.field("Most recent known turn", "unavailable");
    }
    if let Some(turn) = turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Completed && turn.duration_ms.is_some())
        .max_by_key(|turn| turn.duration_ms)
    {
        lines.field(
            "Longest completed turn",
            format!("{} ({})", turn.turn_id, duration(turn.duration_ms)),
        );
    } else {
        lines.field("Longest completed turn", "unavailable");
    }
    let most_expensive = turns
        .iter()
        .map(|turn| {
            (
                *turn,
                detail_turn_window_usage(app, turn).api_equivalent_cost,
            )
        })
        .filter(|(_, amount)| amount.priced_samples > 0)
        .max_by_key(|(_, amount)| amount.minimum_pico_usd.value());
    lines.field(
        "Largest priced subtotal (selected cycle)",
        most_expensive.map_or_else(
            || "unavailable".to_string(),
            |(turn, cost)| {
                format!(
                    "{} ({})",
                    turn.turn_id,
                    format_scoped_api_cost_amount(
                        api_cost_window_state(window_analysis(&app.snapshot, app.window_scope)),
                        cost
                    )
                )
            },
        ),
    );
    for turn in turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::InProgress)
    {
        lines.field(
            "Running turn elapsed",
            format!(
                "{} ({})",
                turn.turn_id,
                duration(elapsed(turn.started_at, app.snapshot.as_of))
            ),
        );
    }
}

fn turn_detail(app: &App, turn: &TurnRecord) -> EntityDetailPopup {
    let mut lines = DetailLines::new(app);
    lines.section("Overview");
    lines.field("Captured at", timestamp(Some(app.snapshot.as_of)));
    lines.field("Thread ID", &turn.thread_id);
    lines.field("Turn ID", &turn.turn_id);
    lines.field(
        "Message preview",
        optional_text(turn.message_preview.as_deref()),
    );
    lines.field("Model", optional_text(turn.model.as_deref()));
    lines.field(
        "Reasoning effort",
        optional_text(turn.reasoning_effort.as_deref()),
    );
    lines.field("Service tier", optional_text(turn.service_tier.as_deref()));
    lines.field("Status", turn.status.label());
    lines.field("Started", timestamp(turn.started_at));
    lines.field("Completed", timestamp(turn.completed_at));
    lines.field("Duration", duration(turn.duration_ms));
    if turn.status == TurnStatus::InProgress {
        lines.field(
            "Elapsed at capture",
            duration(elapsed(turn.started_at, app.snapshot.as_of)),
        );
    }
    lines.section("Usage");
    tokens(&mut lines, "Own cumulative tokens", turn.token_usage);
    if window_description(&mut lines, app) {
        scope_usage(
            &mut lines,
            "Own",
            detail_turn_window_usage(app, turn),
            api_cost_window_state(window_analysis(&app.snapshot, app.window_scope)),
        );
    }
    lines.field(
        "Delegated turn usage",
        "unavailable (no exact turn linkage in snapshot)",
    );
    lines.section("Related");
    if let Some(task) = app
        .snapshot
        .tasks
        .iter()
        .find(|task| task.thread_id == turn.thread_id)
    {
        lines.field("Session title", &task.title);
        lines.field(
            workspace_label(task.source.as_deref()),
            task.cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unavailable".to_string()),
        );
        lines.field("Source", optional_text(task.source.as_deref()));
        lines.field(
            "Parent thread",
            optional_text(task.parent_thread_id.as_deref()),
        );
    }
    lines.section("Data notes");
    overview_notes(&mut lines, app);
    lines.note("Message preview retains at most the first 72 characters. Missing full content cannot be recovered from this snapshot.");
    lines.note("Elapsed is measured at capture; completion duration prefers the logged value and otherwise uses end minus start.");
    lines.finish("Turn details")
}

fn overview_notes(lines: &mut DetailLines, app: &App) {
    lines.note(
        "This popup freezes the captured entity. Background refresh does not replace its content.",
    );
    lines.field("Snapshot partial", app.snapshot.partial);
    if let Some(analysis) = window_analysis_with_api_long_context(
        &app.snapshot,
        app.window_scope,
        app.api_long_context_multiplier,
    ) {
        lines.field("Window partial", analysis.partial);
        for reason in &analysis.partial_reasons {
            lines.field("Partial reason", reason);
        }
        for reason in &analysis.api_equivalent_cost.partial_reasons {
            lines.field("API partial reason", reason);
        }
        lines.field(
            "API price catalog revision",
            analysis.api_pricing.catalog_revision,
        );
        lines.field("API rates date", &analysis.api_pricing.rates_as_of);
        lines.field("API price source", &analysis.api_pricing.source_url);
    }
    lines.note("Cached/cache-write input are input subsets; reasoning output is an output subset. Do not add them again to total tokens.");
    lines.note("TOKEN% is the observed local token share. Estimated quota is a low-confidence account-gauge projection, not official per-session billing.");
    lines.note("API equivalent covers priced model tokens only, not the Codex subscription bill or tool charges. Unpriced usage is excluded; incomplete totals are lower bounds.");
    lines.note("Usage samples are token-delta records; a cumulative record may contain several model requests.");
    lines.note("Running status is inferred from rollout activity. Waiting for approval or user input cannot be recovered reliably.");
}

fn summary_detail(app: &App) -> Option<EntityDetailPopup> {
    let rows = app.summary_rows();
    let index = app.summary_selected_index(&rows);
    let selected = rows.get(index)?;
    let project_id = selected_summary_project_id(&rows, index)?;
    let cache = app.summary_cache.as_ref()?;
    for project in &cache.prepared.usage.projects {
        if summary_project_node_id(&project.key) != project_id {
            continue;
        }
        for session in &project.sessions {
            if selected.kind == SummaryRowKind::Session
                && selected.id == summary_thread_node_id(&session.thread_id)
            {
                return Some(summary_entity_detail(app, cache, project, session, None));
            }
            for turn in &session.turns {
                if selected.kind == SummaryRowKind::Turn
                    && selected.id == summary_turn_node_id(&session.thread_id, &turn.key)
                {
                    return Some(summary_entity_detail(
                        app,
                        cache,
                        project,
                        session,
                        Some(turn),
                    ));
                }
            }
        }
    }
    None
}

fn selected_summary_project_id(rows: &[SummaryTreeRow], index: usize) -> Option<&str> {
    rows.get(..=index)?
        .iter()
        .rev()
        .find(|row| row.kind == SummaryRowKind::Project)
        .map(|row| row.id.as_str())
}

fn summary_entity_detail(
    app: &App,
    cache: &SummaryCache,
    project: &crate::summary::ProjectSummary,
    session: &SessionSummary,
    turn: Option<&TurnSummary>,
) -> EntityDetailPopup {
    let mut lines = DetailLines::new(app);
    let prepared = &cache.prepared;
    let metrics = turn.map_or(session.totals, |turn| turn.totals);
    lines.section("Overview");
    lines.field("Captured at", timestamp(Some(cache.snapshot_as_of)));
    lines.field("View", "Summary history");
    lines.field("Title", optional_text(session.title.as_deref()));
    lines.field("Thread ID", &session.thread_id);
    lines.field(
        workspace_label(session.source.as_deref()),
        session
            .cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unavailable".to_string()),
    );
    lines.field("Source", optional_text(session.source.as_deref()));
    lines.field("Project", &project.label);
    lines.field("Project ID", &project.key);
    if let Some(turn) = turn {
        lines.field("Turn ID", turn.key.exact_turn_id().unwrap_or("unavailable"));
        lines.field(
            "Attribution",
            match turn.key {
                SummaryTurnKey::Exact(_) => "Exact root session turn",
                SummaryTurnKey::UnassignedSession => "Unassigned session usage",
                SummaryTurnKey::UnassignedDelegated => "Unassigned delegated usage",
            },
        );
        lines.field(
            "Message preview",
            optional_text(turn.message_preview.as_deref()),
        );
        lines.field("Started", timestamp(turn.started_at));
    } else {
        lines.field(
            "Known user turns",
            session
                .turns
                .iter()
                .filter(|turn| turn.key.exact_turn_id().is_some())
                .count(),
        );
        lines.field(
            "Unassigned usage rows",
            session
                .turns
                .iter()
                .filter(|turn| turn.key.exact_turn_id().is_none())
                .count(),
        );
    }
    for name in [
        "Status",
        "Completed",
        "Duration",
        "Model",
        "Reasoning effort",
        "Service tier",
    ] {
        lines.field(name, "unavailable (not retained in Summary)");
    }
    lines.section("Usage");
    lines.field("History range", cache.range.label());
    lines.field(
        "Range start",
        timestamp(Some(prepared.usage.window.starts_at)),
    );
    lines.field("Range end", timestamp(Some(prepared.usage.window.ends_at)));
    tokens(&mut lines, "Total range tokens", metrics.token_usage);
    lines.field(
        "Credit-rate equivalent",
        format_estimated_credits(
            metrics.estimated_units(app.api_long_context_multiplier),
            true,
        ),
    );
    let api_state = if prepared.coverage_state(
        SummaryMetric::ApiEquivalent,
        app.api_long_context_multiplier,
    ) == SummaryCoverageState::Missing
    {
        ApiCostWindowState::NoLocalData
    } else if prepared.api_chart_is_lower_bound() {
        ApiCostWindowState::Incomplete
    } else {
        ApiCostWindowState::Complete
    };
    api_amount(&mut lines, "Range", metrics.api_equivalent_cost, api_state);
    lines.field("Usage samples (all observed records)", metrics.call_count);
    lines.field(
        "Selected metric",
        match app.summary_metric {
            SummaryMetric::Tokens => "Tokens",
            SummaryMetric::Estimated => "Estimated",
            SummaryMetric::ApiEquivalent => "API equivalent",
        },
    );
    lines.field(
        "Share of report selected metric",
        format!(
            "{:.4}%",
            app.summary_metric.share_percent(
                metrics,
                prepared.usage.totals,
                app.api_long_context_multiplier
            )
        ),
    );
    if turn.is_none() {
        lines.section("Known user-turn statistics");
        let user_turns = session
            .turns
            .iter()
            .filter(|turn| turn.key.exact_turn_id().is_some())
            .collect::<Vec<_>>();
        lines.field(
            "Largest known user turn (range)",
            user_turns
                .iter()
                .max_by_key(|turn| turn.totals.token_usage.total_tokens)
                .map_or_else(
                    || "unavailable".to_string(),
                    |turn| {
                        format!(
                            "{} ({} tokens)",
                            turn.key.exact_turn_id().unwrap_or("unavailable"),
                            turn.totals.token_usage.total_tokens
                        )
                    },
                ),
        );
        lines.field(
            "Largest priced user-turn subtotal (range)",
            user_turns
                .iter()
                .filter(|turn| turn.totals.api_equivalent_cost.priced_samples > 0)
                .max_by_key(|turn| turn.totals.api_equivalent_cost.minimum_pico_usd.value())
                .map_or_else(
                    || "unavailable".to_string(),
                    |turn| {
                        format!(
                            "{} ({})",
                            turn.key.exact_turn_id().unwrap_or("unavailable"),
                            format_scoped_api_cost_amount(
                                api_state,
                                turn.totals.api_equivalent_cost,
                            )
                        )
                    },
                ),
        );
        lines.field(
            "Latest known user-turn start",
            timestamp(user_turns.iter().filter_map(|turn| turn.started_at).max()),
        );
        lines.note("Rankings exclude unassigned rows. Priced subtotals rank known amounts; unpriced usage may change the true cost order.");
    }
    lines.section("Related");
    lines.field("Root session", &session.thread_id);
    if turn.is_none() {
        for turn in &session.turns {
            lines.field(
                "Turn",
                match &turn.key {
                    SummaryTurnKey::Exact(id) => {
                        format!("{id} ({} tokens)", turn.totals.token_usage.total_tokens)
                    }
                    SummaryTurnKey::UnassignedSession => format!(
                        "Unassigned session usage ({} tokens)",
                        turn.totals.token_usage.total_tokens
                    ),
                    SummaryTurnKey::UnassignedDelegated => format!(
                        "Unassigned delegated usage ({} tokens)",
                        turn.totals.token_usage.total_tokens
                    ),
                },
            );
        }
    }
    lines.section("Data notes");
    lines.note("This popup freezes the selected Summary entity and history range. Background refresh does not replace its content.");
    lines.field(
        "Report coverage",
        format!("{:.2}%", prepared.coverage_percent(app.summary_metric)),
    );
    lines.field(
        "Report buckets",
        format!(
            "{} / {}",
            prepared.covered_buckets, prepared.expected_buckets
        ),
    );
    lines.field(
        "Report partial",
        prepared.partial(app.summary_metric, app.api_long_context_multiplier),
    );
    lines.field(
        "Selected value is lower bound",
        prepared.value_is_lower_bound(app.summary_metric, app.api_long_context_multiplier),
    );
    lines.field(
        "Long-context breakdown complete",
        prepared.long_context_breakdown_complete,
    );
    for reason in &prepared.partial_reasons {
        lines.field("Partial reason", reason);
    }
    lines.note("Summary values cover the selected history range, not lifetime totals or the Overview quota cycle.");
    lines.note("Root turn totals include directly observed usage plus delegated descendants with exact turn attribution. Missing attribution stays in separate unassigned rows.");
    lines.note("Own/delegated breakdown, status, completion time and model metadata are not retained in Summary; no current or remote session is guessed from an ID.");
    lines.note("Credit-rate equivalent is an additive estimate, not an account quota percentage. API equivalent is priced model-token cost, not an actual bill.");
    lines.note("Cached/cache-write input and reasoning output are subsets. Usage samples are not necessarily individual model requests.");
    lines.finish(if turn.is_some() {
        "Summary turn details"
    } else {
        "Summary session details"
    })
}

/// Hard-wrap by grapheme so long paths and IDs remain readable in full.
fn wrapped_lines(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut output = Vec::new();
    for line in lines {
        let mut current = Line::default().style(line.style);
        let mut used = 0usize;
        for span in &line.spans {
            let mut chunk = String::new();
            for grapheme in span.content.graphemes(true) {
                let next = UnicodeWidthStr::width(grapheme);
                if used > 0 && used.saturating_add(next) > usize::from(width) {
                    if !chunk.is_empty() {
                        current
                            .spans
                            .push(Span::styled(std::mem::take(&mut chunk), span.style));
                    }
                    output.push(current);
                    current = Line::default().style(line.style);
                    used = 0;
                }
                chunk.push_str(grapheme);
                used = used.saturating_add(next);
            }
            if !chunk.is_empty() {
                current.spans.push(Span::styled(chunk, span.style));
            }
        }
        output.push(current);
    }
    output
}

fn append_enrichment(lines: &mut Vec<Line<'static>>, values: Vec<String>) {
    const MAX_BYTES: usize = 256 * 1024;
    const MAX_LINES: usize = 8_192;
    let mut bytes = 0usize;
    let mut truncated = false;
    for (index, value) in values.into_iter().enumerate() {
        if index >= MAX_LINES || bytes >= MAX_BYTES {
            truncated = true;
            break;
        }
        let safe = terminal_safe_text(&value);
        let available = MAX_BYTES.saturating_sub(bytes);
        if safe.len() > available {
            let end = safe
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= available)
                .last()
                .unwrap_or(0);
            lines.push(Line::from(safe[..end].to_owned()));
            truncated = true;
            break;
        }
        bytes = bytes.saturating_add(safe.len());
        lines.push(Line::from(safe));
    }
    if truncated {
        lines.push(Line::from(
            "Recorded content truncated at the popup display limit (256 KiB / 8192 lines).",
        ));
    }
}

pub(super) fn render_entity_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
) -> EntityDetailHitbox {
    let palette = app.theme.palette();
    let width = if area.width >= 8 {
        area.width.saturating_sub(4).min(100)
    } else {
        area.width
    };
    let height = if area.height >= 8 {
        area.height.saturating_sub(4).min(38)
    } else {
        area.height
    };
    let popup = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let Some(detail) = app.entity_detail.as_mut() else {
        return EntityDetailHitbox::default();
    };
    let block = panel(&detail.title, app.theme)
        .border_style(Style::default().fg(palette.accent))
        .style(app.theme.base_style());
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    let content = Rect::new(
        inner.x,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height.saturating_sub(1),
    );
    if detail.wrapped_width != Some(content.width) {
        detail.wrapped = wrapped_lines(&detail.lines, content.width);
        detail.wrapped_width = Some(content.width);
    }
    detail.line_count = detail.wrapped.len();
    let capacity = usize::from(content.height);
    detail.offset = detail
        .offset
        .min(detail.line_count.saturating_sub(capacity));
    let visible = detail
        .wrapped
        .iter()
        .skip(detail.offset)
        .take(capacity)
        .cloned()
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().fg(palette.foreground)),
        content,
    );
    let scrollbar = scrollbar_geometry(
        Rect::new(content.right(), content.y, 1, content.height),
        detail.line_count,
        capacity,
        detail.offset,
    );
    if let Some(bar) = scrollbar {
        render_scrollbar(frame, bar, app.theme, true);
    }
    let controls = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    let mut hitbox = EntityDetailHitbox {
        content,
        scrollbar,
        ..EntityDetailHitbox::default()
    };
    let mut spans = Vec::new();
    let mut x = controls.x;
    let full = controls.width >= 26;
    let specs: &[(&str, &str, bool)] = if controls.width < 13 {
        &[("←", "", true)]
    } else {
        &[
            ("↑", if full { " Up" } else { "" }, detail.offset > 0),
            (
                "↓",
                if full { " Down" } else { "" },
                detail.offset < detail.line_count.saturating_sub(capacity),
            ),
            ("←", if full { " Back" } else { "" }, true),
        ]
    };
    for &(key, suffix, active) in specs {
        let leading = if spans.is_empty() { "" } else { "  " };
        let control = append_summary_control(
            &mut spans,
            controls,
            &mut x,
            SummaryControlSpec {
                leading,
                shortcut: key.to_string(),
                suffix,
                selected: false,
                shortcuts_active: active,
                theme: app.theme,
            },
        );
        match key {
            "↑" => hitbox.up = control,
            "↓" => hitbox.down = control,
            _ => hitbox.back = control,
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), controls);
    hitbox
}
