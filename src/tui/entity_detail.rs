use super::*;
use crate::summary_report::SummaryCoverageState;
use unicode_segmentation::UnicodeSegmentation;

mod usage;
use usage::UsageBlock;

#[derive(Debug)]
pub(super) enum DetailNode {
    Lines(Vec<Line<'static>>),
    Usage(UsageBlock),
    MessagePreview {
        turn_id: Option<String>,
        preview: Option<String>,
        redacted: bool,
    },
    Section {
        id: String,
        title: String,
        foldable: bool,
        children: Vec<DetailNode>,
    },
}

#[derive(Clone, Copy)]
struct MessagePreviewContext<'a> {
    recorded: Option<&'a crate::session_details::SessionDetails>,
    loading: bool,
}

#[derive(Debug)]
pub(super) struct DetailHeader {
    pub(super) id: String,
    pub(super) line: usize,
    end_line: usize,
}

#[derive(Debug)]
struct WrappedDetailHeader {
    id: String,
    range: std::ops::Range<usize>,
    section_end: usize,
    line: usize,
    width: usize,
}

enum DetailScrollAnchor {
    Header { id: String, row: usize },
    Body { id: String, offset: usize },
}

#[derive(Debug)]
pub(super) struct EntityDetailPopup {
    pub(super) title: String,
    pub(super) lines: Vec<Line<'static>>,
    pub(super) offset: usize,
    pub(super) line_count: usize,
    pub(super) document: Vec<DetailNode>,
    pub(super) expanded: HashSet<String>,
    pub(super) selected_section: Option<String>,
    pub(super) headers: Vec<DetailHeader>,
    pub(super) section_hitboxes: Vec<(String, Rect)>,
    recorded: Option<crate::session_details::SessionDetails>,
    recorded_expanded: Option<HashSet<String>>,
    receiver: Option<Receiver<crate::session_details::SessionDetails>>,
    cancel: Option<Arc<AtomicBool>>,
    wrapped: Vec<Line<'static>>,
    wrapped_width: Option<u16>,
    wrapped_height: usize,
    wrapped_headers: Vec<WrappedDetailHeader>,
    header_anchor: Option<(String, usize)>,
}

impl EntityDetailPopup {
    fn recording_status(&mut self, status: &str, theme: Theme) {
        self.document.retain(|node| {
            !matches!(node,
                DetailNode::Lines(lines) if lines.iter().any(|line|
                    line.spans.iter().any(|span| span.content.starts_with("Recorded details:")))
            )
        });
        self.document.push(DetailNode::Lines(vec![
            Line::default(),
            Line::from(status.to_owned()),
        ]));
        self.rebuild(self.wrapped_width.unwrap_or(96), theme);
    }

    pub(super) fn set_recorded_details(
        &mut self,
        data: crate::session_details::SessionDetails,
        theme: Theme,
    ) {
        self.document.retain(|node| {
            !matches!(node,
                DetailNode::Lines(lines) if lines.iter().any(|line|
                    line.spans.iter().any(|span| span.content.starts_with("Recorded details:")))
            )
        });
        // Replacing injected evidence must not duplicate groups or inherit their state.
        self.document.retain(|node| {
            !matches!(node,
                DetailNode::Section { id, .. } if id.starts_with("recorded.")
            )
        });
        self.expanded.retain(|id| !id.starts_with("recorded."));
        if self
            .selected_section
            .as_ref()
            .is_some_and(|id| id.starts_with("recorded."))
        {
            self.selected_section = None;
        }
        self.recorded = Some(data);
        self.recorded_expanded = None;
        self.rebuild(self.wrapped_width.unwrap_or(96), theme);
    }

    pub(super) fn rebuild(&mut self, width: u16, theme: Theme) {
        if let Some(data) = &self.recorded {
            let expanded = self
                .expanded
                .iter()
                .filter_map(|id| id.strip_prefix("recorded.").map(str::to_owned))
                .collect::<HashSet<_>>();
            if self.recorded_expanded.as_ref() != Some(&expanded) {
                self.document.retain(|node| {
                    !matches!(node,
                        DetailNode::Section { id, .. } if id.starts_with("recorded.")
                    )
                });
                self.document.extend(
                    data.detail_sections(&expanded)
                        .into_iter()
                        .map(recorded_section),
                );
                self.recorded_expanded = Some(expanded);
            }
        }
        self.lines.clear();
        self.headers.clear();
        project_detail_nodes(
            &self.document,
            &self.expanded,
            self.selected_section.as_deref(),
            width,
            theme,
            0,
            MessagePreviewContext {
                recorded: self.recorded.as_ref(),
                loading: self.receiver.is_some(),
            },
            &mut self.lines,
            &mut self.headers,
        );
        self.wrapped_width = None;
    }

    fn anchor_header(&mut self, id: &str) {
        let body_capacity = self
            .wrapped_height
            .saturating_sub(self.sticky_headers(self.offset, self.wrapped_height).len());
        let row = self
            .wrapped_headers
            .iter()
            .find(|header| header.id == id)
            .filter(|header| {
                header.range.end > self.offset
                    && header.range.start < self.offset.saturating_add(body_capacity)
            })
            .map_or(0, |header| header.range.start.saturating_sub(self.offset));
        self.header_anchor = Some((id.to_owned(), row));
    }

    fn sticky_limit(capacity: usize) -> usize {
        // Keep at least two rows for the actual content in compact terminals.
        capacity.saturating_sub(2).min(3)
    }

    fn sticky_headers(&self, offset: usize, capacity: usize) -> Vec<&WrappedDetailHeader> {
        let mut ancestors: Vec<_> = self
            .wrapped_headers
            .iter()
            .filter(|header| {
                self.expanded.contains(&header.id)
                    && header.range.end <= offset
                    && offset < header.section_end
            })
            .collect();
        let skip = ancestors.len().saturating_sub(Self::sticky_limit(capacity));
        ancestors.drain(..skip);
        ancestors
    }

    pub(super) fn scroll_limit(&self, capacity: usize) -> usize {
        if capacity == 0 {
            return 0;
        }
        let count = self.wrapped.len();
        let start = count.saturating_sub(capacity);
        // A pinned ancestor can end near the bottom. Iterating a dynamic clamp
        // can then bounce between two offsets. Search the at-most-four tail
        // positions once and choose the first that exposes the final line.
        let end = start
            .saturating_add(Self::sticky_limit(capacity))
            .min(count.saturating_sub(1));
        (start..=end)
            .find(|&offset| {
                let body = capacity.saturating_sub(self.sticky_headers(offset, capacity).len());
                offset.saturating_add(body) >= count
            })
            .unwrap_or(end)
    }

    fn resize_anchor(&self) -> Option<DetailScrollAnchor> {
        let sticky = self.sticky_headers(self.offset, self.wrapped_height);
        let body_capacity = self.wrapped_height.saturating_sub(sticky.len());
        let visible = |header: &&WrappedDetailHeader| {
            header.range.end > self.offset
                && header.range.start < self.offset.saturating_add(body_capacity)
        };
        if let Some(id) = self.selected_section.as_deref() {
            if let Some(header) = self
                .wrapped_headers
                .iter()
                .filter(visible)
                .find(|header| header.id == id)
            {
                return Some(DetailScrollAnchor::Header {
                    id: header.id.clone(),
                    row: header.range.start.saturating_sub(self.offset),
                });
            }
            if sticky.iter().any(|header| header.id == id)
                && let Some(header) = sticky.last()
            {
                return Some(DetailScrollAnchor::Body {
                    id: header.id.clone(),
                    offset: self.offset.saturating_sub(header.range.end),
                });
            }
        }
        if let Some(header) = sticky.last() {
            return Some(DetailScrollAnchor::Body {
                id: header.id.clone(),
                offset: self.offset.saturating_sub(header.range.end),
            });
        }
        self.wrapped_headers
            .iter()
            .find(visible)
            .map(|header| DetailScrollAnchor::Header {
                id: header.id.clone(),
                row: header.range.start.saturating_sub(self.offset),
            })
    }
}

fn recorded_section(section: crate::session_details::SessionDetailSection) -> DetailNode {
    let mut lines = Vec::new();
    append_enrichment(&mut lines, section.lines);
    let mut children = vec![DetailNode::Lines(lines)];
    children.extend(section.children.into_iter().map(recorded_section));
    DetailNode::Section {
        id: format!("recorded.{}", section.id),
        title: terminal_safe_text(&section.title),
        foldable: true,
        children,
    }
}

fn captured_message_preview(nodes: &[DetailNode]) -> Option<&str> {
    nodes.iter().find_map(|node| match node {
        DetailNode::MessagePreview { preview, .. } => preview.as_deref(),
        DetailNode::Section { children, .. } => captured_message_preview(children),
        _ => None,
    })
}

fn message_preview_lines(
    turn_id: Option<&str>,
    preview: Option<&str>,
    redacted: bool,
    context: MessagePreviewContext<'_>,
) -> Vec<Line<'static>> {
    use crate::session_details::{MessagePreviewText, MessagePreviewUnavailable};

    if redacted || context.recorded.is_some_and(|data| data.redacted) {
        return vec![Line::from("Full message unavailable: content redacted.")];
    }
    let status = if turn_id.is_none_or(str::is_empty) {
        "Full message unavailable: no exact turn association."
    } else if let Some(data) = context.recorded {
        match data.message_preview_text(turn_id, preview) {
            MessagePreviewText::Available { text, truncated } => {
                let mut lines = vec![Line::from(if truncated {
                    "Message from local log (truncated at the retention limit):"
                } else {
                    "Full message from local log:"
                })];
                append_enrichment(
                    &mut lines,
                    text.split('\n').take(8_193).map(str::to_owned).collect(),
                );
                return lines;
            }
            MessagePreviewText::Unavailable(reason) => match reason {
                MessagePreviewUnavailable::ExactTurnRequired => {
                    "Full message unavailable: no exact turn association."
                }
                MessagePreviewUnavailable::Redacted => {
                    return vec![Line::from("Full message unavailable: content redacted.")];
                }
                MessagePreviewUnavailable::NoMatchingUserMessage => {
                    "Full message unavailable: no matching user message in the local log."
                }
                MessagePreviewUnavailable::Ambiguous => {
                    "Full message unavailable: multiple user messages match this preview."
                }
                MessagePreviewUnavailable::RetentionLimit => {
                    "Full message unavailable: message content was not retained within the detail limits."
                }
            },
        }
    } else if context.loading {
        "Full message: loading local log..."
    } else {
        "Full message unavailable: no accessible local message evidence."
    };
    let mut lines = vec![Line::from(status)];
    if let Some(preview) = preview.filter(|text| !text.trim().is_empty()) {
        lines.push(Line::from("Saved preview:"));
        append_enrichment(
            &mut lines,
            preview.split('\n').take(8_193).map(str::to_owned).collect(),
        );
    } else {
        lines.push(Line::from("Saved preview: unavailable."));
    }
    lines
}

fn collapsed_message_preview_line(
    preview: Option<&str>,
    redacted: bool,
    context: MessagePreviewContext<'_>,
    width: u16,
    depth: usize,
) -> Line<'static> {
    let text = if redacted || context.recorded.is_some_and(|data| data.redacted) {
        "Saved preview: content redacted.".to_owned()
    } else {
        let preview = preview
            .map(terminal_safe_text)
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty());
        match preview {
            Some(preview) => format!("Saved preview: {preview}"),
            None => "Saved preview: unavailable.".to_owned(),
        }
    };
    let indent = depth
        .saturating_mul(2)
        .min(usize::from(width.saturating_sub(1)));
    // This teaser only uses the captured short preview. Resolve the retained
    // user body exclusively through the expanded MessagePreview projection.
    let mut line = sticky_detail_line(&Line::from(text), width.saturating_sub(indent as u16));
    if indent > 0 {
        line.spans.insert(0, Span::raw(" ".repeat(indent)));
    }
    line
}

#[allow(clippy::too_many_arguments)]
fn project_detail_nodes(
    nodes: &[DetailNode],
    expanded: &HashSet<String>,
    selected: Option<&str>,
    width: u16,
    theme: Theme,
    depth: usize,
    preview_context: MessagePreviewContext<'_>,
    lines: &mut Vec<Line<'static>>,
    headers: &mut Vec<DetailHeader>,
) {
    let palette = theme.palette();
    for node in nodes {
        match node {
            DetailNode::MessagePreview {
                turn_id,
                preview,
                redacted,
            } => {
                let indent = "  ".repeat(depth);
                lines.extend(
                    message_preview_lines(
                        turn_id.as_deref(),
                        preview.as_deref(),
                        *redacted,
                        preview_context,
                    )
                    .into_iter()
                    .map(|mut line| {
                        if !indent.is_empty() && !line.spans.is_empty() {
                            line.spans.insert(0, Span::raw(indent.clone()));
                        }
                        line
                    }),
                );
            }
            DetailNode::Lines(values) => {
                lines.extend(values.iter().cloned().map(|mut line| {
                    if depth > 0 && !line.spans.is_empty() {
                        line.spans.insert(0, Span::raw("  ".repeat(depth)));
                    }
                    line
                }));
            }
            DetailNode::Usage(block) => {
                let indent_columns = depth
                    .saturating_mul(2)
                    .min(usize::from(width.saturating_sub(1)));
                let indent = " ".repeat(indent_columns);
                let content_width = width.saturating_sub(indent_columns as u16);
                lines.extend(
                    block
                        .render(content_width, theme)
                        .into_iter()
                        .map(|mut line| {
                            if !indent.is_empty() && !line.spans.is_empty() {
                                line.spans.insert(0, Span::raw(indent.clone()));
                            }
                            line
                        }),
                );
            }
            DetailNode::Section {
                id,
                title,
                foldable,
                children,
            } => {
                if depth == 0 && !lines.is_empty() {
                    lines.push(Line::default());
                }
                let header_index = headers.len();
                if *foldable {
                    let focused = selected == Some(id.as_str());
                    let key_style = if focused {
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(palette.foreground)
                    };
                    let style = if focused {
                        Style::default()
                            .fg(palette.title)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(palette.foreground)
                    };
                    headers.push(DetailHeader {
                        id: id.clone(),
                        line: lines.len(),
                        end_line: 0,
                    });
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}[", "  ".repeat(depth)), style),
                        Span::styled("↵", key_style),
                        Span::styled(
                            format!(
                                "] {} {title}",
                                if expanded.contains(id) { "▾" } else { "▸" }
                            ),
                            style,
                        ),
                    ]));
                } else {
                    lines.push(Line::styled(
                        title.clone(),
                        Style::default()
                            .fg(palette.title)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                if !foldable || expanded.contains(id) {
                    project_detail_nodes(
                        children,
                        expanded,
                        selected,
                        width,
                        theme,
                        depth + usize::from(*foldable),
                        preview_context,
                        lines,
                        headers,
                    );
                } else if id == "snapshot.message-preview"
                    && let Some((preview, redacted)) = children.iter().find_map(|child| match child
                    {
                        DetailNode::MessagePreview {
                            preview, redacted, ..
                        } => Some((preview.as_deref(), *redacted)),
                        _ => None,
                    })
                {
                    lines.push(collapsed_message_preview_line(
                        preview,
                        redacted,
                        preview_context,
                        width,
                        depth + 1,
                    ));
                }
                if *foldable {
                    headers[header_index].end_line = lines.len();
                }
            }
        }
    }
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
    pub(super) body: Rect,
    pub(super) scrollbar: Option<ScrollbarHitbox>,
    pub(super) up: Rect,
    pub(super) down: Rect,
    pub(super) next: Rect,
    pub(super) toggle: Rect,
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
            detail.recording_status(
                "Recorded details: unavailable (no unambiguous local rollout association)",
                self.theme,
            );
            return;
        };
        let codex_home = self.snapshot.codex_home.clone();
        let redact = self.local_redact_content;
        let saved_preview = captured_message_preview(&detail.document).map(str::to_owned);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        detail.recording_status(
            "Recorded details: loading local evidence at capture...",
            self.theme,
        );
        detail.receiver = Some(receiver);
        detail.cancel = Some(cancel);
        thread::spawn(move || {
            let data = if let Some(turn_id) = turn_id.as_deref() {
                let target = crate::session_details::MessagePreviewReadTarget {
                    thread_id: &thread_id,
                    turn_id,
                    preview: saved_preview.as_deref(),
                };
                crate::session_details::load_session_details_with_preview_in_range(
                    &codex_home,
                    &target,
                    redact,
                    starts_at,
                    Some(as_of),
                    &worker_cancel,
                )
            } else {
                crate::session_details::load_session_details_in_range(
                    &codex_home,
                    &thread_id,
                    None,
                    redact,
                    starts_at,
                    Some(as_of),
                    &worker_cancel,
                )
            };
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
                detail.set_recorded_details(data, self.theme);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                detail.receiver = None;
                detail.recording_status(
                    "Recorded details: unavailable (reader did not return evidence)",
                    self.theme,
                );
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
            detail.offset = if down {
                detail
                    .offset
                    .saturating_add(lines)
                    .min(detail.scroll_limit(capacity))
            } else {
                detail.offset.saturating_sub(lines)
            };
        }
    }

    pub(super) fn toggle_entity_detail_section(&mut self, id: &str) -> bool {
        let Some(detail) = self.entity_detail.as_mut() else {
            return false;
        };
        if !detail.headers.iter().any(|header| header.id == id) {
            return false;
        }
        detail.anchor_header(id);
        detail.selected_section = Some(id.to_owned());
        if !detail.expanded.remove(id) {
            detail.expanded.insert(id.to_owned());
        }
        detail.rebuild(detail.wrapped_width.unwrap_or(96), self.theme);
        self.scroll_drag = None;
        true
    }

    pub(super) fn toggle_entity_detail_selected(&mut self) -> bool {
        let id = self
            .entity_detail
            .as_ref()
            .and_then(|detail| detail.selected_section.clone());
        id.is_some_and(|id| self.toggle_entity_detail_section(&id))
    }

    pub(super) fn cycle_entity_detail_section(&mut self, forward: bool) {
        let Some(detail) = self
            .entity_detail
            .as_mut()
            .filter(|detail| !detail.headers.is_empty())
        else {
            return;
        };
        let current = detail
            .headers
            .iter()
            .position(|header| Some(&header.id) == detail.selected_section.as_ref());
        let count = detail.headers.len();
        let index = match (current, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(index), true) => (index + 1) % count,
            (Some(index), false) => (index + count - 1) % count,
        };
        let id = detail.headers[index].id.clone();
        if detail
            .sticky_headers(detail.offset, detail.wrapped_height)
            .iter()
            .any(|header| header.id == id)
        {
            // Selecting a pinned ancestor keeps the body at its current place.
            // Toggling it still returns to that ancestor's original heading.
            detail.header_anchor = None;
        } else {
            detail.anchor_header(&id);
        }
        detail.selected_section = Some(id);
        detail.rebuild(detail.wrapped_width.unwrap_or(96), self.theme);
        self.scroll_drag = None;
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
                        && selected.id == summary_thread_node_id(&project.key, &session.thread_id)
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
                            && selected.id
                                == summary_turn_node_id(&project.key, &session.thread_id, &turn.key)
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
    document: Vec<DetailNode>,
    path: Vec<usize>,
    theme: Theme,
    redacted: bool,
}

impl DetailLines {
    fn new(app: &App) -> Self {
        Self {
            document: Vec::new(),
            path: Vec::new(),
            theme: app.theme,
            redacted: app.local_redact_content,
        }
    }

    fn section(&mut self, title: &str) {
        self.path.clear();
        self.document.push(DetailNode::Section {
            id: format!("snapshot.{title}"),
            title: title.to_owned(),
            foldable: !matches!(title, "Overview" | "Usage"),
            children: Vec::new(),
        });
        self.path.push(self.document.len() - 1);
    }

    fn children(&mut self) -> &mut Vec<DetailNode> {
        let mut children = &mut self.document;
        for &index in &self.path {
            let DetailNode::Section { children: next, .. } = &mut children[index] else {
                unreachable!("the builder only enters sections");
            };
            children = next;
        }
        children
    }

    fn subsection(&mut self, id: String, title: String) {
        let children = self.children();
        children.push(DetailNode::Section {
            id,
            title: terminal_safe_text(&title),
            foldable: true,
            children: Vec::new(),
        });
        let index = children.len() - 1;
        self.path.push(index);
    }

    fn end_subsection(&mut self) {
        self.path.pop();
    }

    fn field(&mut self, name: &str, value: impl std::fmt::Display) {
        self.children()
            .push(DetailNode::Lines(vec![Line::from(format!(
                "{name}: {}",
                terminal_safe_text(&value.to_string())
            ))]));
    }

    fn note(&mut self, value: &str) {
        self.children()
            .push(DetailNode::Lines(vec![Line::from(terminal_safe_text(
                value,
            ))]));
    }

    fn usage(&mut self, block: UsageBlock) {
        self.children().push(DetailNode::Usage(block));
    }

    fn message_preview(&mut self, turn_id: Option<&str>, preview: Option<&str>) {
        let redacted = self.redacted;
        self.subsection("snapshot.message-preview".into(), "Message preview".into());
        self.children().push(DetailNode::MessagePreview {
            turn_id: turn_id.map(str::to_owned),
            preview: preview.filter(|_| !redacted).map(str::to_owned),
            redacted,
        });
        self.end_subsection();
    }

    fn finish(self, title: &str) -> EntityDetailPopup {
        let mut popup = EntityDetailPopup {
            title: title.to_owned(),
            lines: Vec::new(),
            document: self.document,
            expanded: HashSet::new(),
            selected_section: None,
            headers: Vec::new(),
            section_hitboxes: Vec::new(),
            offset: 0,
            line_count: 0,
            recorded: None,
            recorded_expanded: None,
            receiver: None,
            cancel: None,
            wrapped: Vec::new(),
            wrapped_width: None,
            wrapped_height: 0,
            wrapped_headers: Vec::new(),
            header_anchor: None,
        };
        popup.rebuild(96, self.theme);
        popup
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
    lines.usage(UsageBlock::Tokens {
        label: label.to_owned(),
        usage,
    });
}

fn api_amount(
    lines: &mut DetailLines,
    prefix: &str,
    amount: ApiCostAmount,
    state: ApiCostWindowState,
) {
    lines.usage(UsageBlock::Cost {
        prefix: prefix.to_owned(),
        amount,
        state,
    });
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

fn entity_usage(
    lines: &mut DetailLines,
    app: &App,
    cumulative: Vec<(String, TokenUsage)>,
    scoped: Vec<(String, WindowUsage)>,
) {
    lines.section("Usage");
    let attribution = attribution_for_scope_with_api_long_context(
        &app.snapshot,
        app.window_scope,
        app.api_long_context_multiplier,
    );
    if let Some(window) = attribution.and_then(|value| value.window.as_ref()) {
        lines.field("Current cycle", app.window_scope.label());
        lines.field(
            "Cycle",
            format!(
                "{} → {}",
                timestamp(Some(window.starts_at)),
                timestamp(Some(window.ends_at))
            ),
        );
        if scoped.len() > 1 {
            lines.note("Own: this session · Delegated: all linked descendants · Total: combined");
        }
        lines.usage(UsageBlock::TokenComparison {
            columns: scoped
                .iter()
                .map(|(label, usage)| (label.clone(), usage.token_usage))
                .collect(),
            show_composition: true,
        });
        lines.usage(UsageBlock::QuotaComparison {
            columns: scoped.clone(),
            account_used_percent: window.used_percent,
            long_context: app.api_long_context_multiplier,
        });
        lines.usage(UsageBlock::CostComparison {
            columns: scoped
                .iter()
                .map(|(label, usage)| (label.clone(), usage.api_equivalent_cost))
                .collect(),
            state: api_cost_window_state(window_analysis(&app.snapshot, app.window_scope)),
        });
        let matches_cycle = cumulative.len() == scoped.len()
            && cumulative.iter().zip(&scoped).all(
                |((label, usage), (scope_label, scope_usage))| {
                    label == scope_label && *usage == scope_usage.token_usage
                },
            );
        lines.subsection(
            "usage.lifetime".into(),
            if matches_cycle {
                "Lifetime usage (same as current cycle)".into()
            } else {
                "Lifetime usage".into()
            },
        );
        lines.note("All observed cumulative tokens for this entity, across cycles.");
        lines.usage(UsageBlock::TokenComparison {
            columns: cumulative,
            show_composition: false,
        });
        lines.end_subsection();
    } else {
        lines.note("Cumulative usage (selected cycle unavailable)");
        lines.usage(UsageBlock::TokenComparison {
            columns: cumulative,
            show_composition: true,
        });
    }
    lines.subsection(
        "usage.calculation".into(),
        "Usage calculation details".into(),
    );
    lines.field("Selected scope", app.window_scope.label());
    lines.field(
        "EST Longx",
        if app.api_long_context_multiplier {
            "enabled"
        } else {
            "disabled"
        },
    );
    if let Some(attribution) = attribution {
        lines.field("Attribution method", &attribution.method);
        lines.field(
            "External activity possible",
            attribution.external_activity_possible,
        );
        lines.note("External activity possible expresses uncertainty; it does not report detected activity.");
    }
    lines.note("Cached input is split from input; reasoning is split from output. Each composition percentage uses the same total-token denominator.");
    lines.note("Usage samples are token-delta records and may contain several model requests.");
    lines.end_subsection();
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
    let own_window = detail_task_window_usage(app, task);
    let mut delegated_window = WindowUsage::default();
    for child in &children {
        add_usage(&mut delegated_window, detail_task_window_usage(app, child));
    }
    let mut total_window = own_window;
    add_usage(&mut total_window, delegated_window);
    entity_usage(
        &mut lines,
        app,
        vec![
            ("Own".into(), task.token_usage),
            ("Delegated".into(), delegated),
            ("Total".into(), total),
        ],
        vec![
            ("Own".into(), own_window),
            ("Delegated".into(), delegated_window),
            ("Total".into(), total_window),
        ],
    );
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
        lines.subsection(
            format!("snapshot.Related.{}", child.thread_id),
            format!(
                "{} · {} tokens",
                child.title, child.token_usage.total_tokens
            ),
        );
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
        lines.end_subsection();
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
    lines.message_preview(Some(&turn.turn_id), turn.message_preview.as_deref());
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
    entity_usage(
        &mut lines,
        app,
        vec![("Own".into(), turn.token_usage)],
        vec![("Own".into(), detail_turn_window_usage(app, turn))],
    );
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
    lines.note("Message preview is collapsed by default. Expanding it reads the matching user message from captured local evidence; unavailable or truncated content is labelled. The saved snapshot preview retains up to 72 characters.");
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
    lines.note("TOKEN% is the share of observed tokens in the selected cycle and configured sources. Estimated quota is a low-confidence account-gauge projection, not official per-session billing.");
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
                && selected.id == summary_thread_node_id(&project.key, &session.thread_id)
            {
                return Some(summary_entity_detail(app, cache, project, session, None));
            }
            for turn in &session.turns {
                if selected.kind == SummaryRowKind::Turn
                    && selected.id
                        == summary_turn_node_id(&project.key, &session.thread_id, &turn.key)
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
        lines.message_preview(
            turn.key
                .exact_turn_id()
                .and_then(|id| raw_local_history_identity(app, id)),
            turn.message_preview.as_deref(),
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

/// Continuations stay inside their nested section instead of returning to the
/// root margin when a long call ID, path or output line wraps.
fn wrapped_detail_line(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    let Some(first) = line.spans.first() else {
        return wrapped_lines(std::slice::from_ref(line), width);
    };
    let indent = first
        .content
        .bytes()
        .take_while(|byte| *byte == b' ')
        .count();
    if indent == 0 || indent >= usize::from(width) {
        return wrapped_lines(std::slice::from_ref(line), width);
    }
    let mut body = line.clone();
    body.spans[0].content = first.content[indent..].to_owned().into();
    let mut wrapped = wrapped_lines(&[body], width - indent as u16);
    for row in &mut wrapped {
        row.spans
            .insert(0, Span::styled(" ".repeat(indent), first.style));
    }
    wrapped
}

/// Pinned copies take one row. The original heading still wraps in full in the
/// scrollable document, and shortcut styling survives Unicode-safe truncation.
fn sticky_detail_line(line: &Line<'static>, width: u16) -> Line<'static> {
    if line.width() <= usize::from(width) {
        return line.clone();
    }
    let mut result = Line::default().style(line.style);
    if width == 0 {
        return result;
    }
    let available = usize::from(width).saturating_sub(1);
    let mut used = 0;
    for span in &line.spans {
        let mut chunk = String::new();
        for grapheme in span.content.graphemes(true) {
            let next = UnicodeWidthStr::width(grapheme);
            if used + next > available {
                if !chunk.is_empty() {
                    result.spans.push(Span::styled(chunk, span.style));
                }
                result.spans.push(Span::styled("…", span.style));
                return result;
            }
            chunk.push_str(grapheme);
            used += next;
        }
        if !chunk.is_empty() {
            result.spans.push(Span::styled(chunk, span.style));
        }
    }
    result
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
        area.width.saturating_sub(4).min(140)
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
    let capacity = usize::from(content.height);
    if detail.wrapped_width != Some(content.width) || detail.wrapped_height != capacity {
        let anchor = detail
            .header_anchor
            .take()
            .map(|(id, row)| DetailScrollAnchor::Header { id, row })
            .or_else(|| detail.resize_anchor());
        detail.rebuild(content.width, app.theme);
        detail.wrapped.clear();
        detail.wrapped_headers.clear();
        let mut line_offsets = Vec::with_capacity(detail.lines.len() + 1);
        for line in &detail.lines {
            line_offsets.push(detail.wrapped.len());
            detail
                .wrapped
                .extend(wrapped_detail_line(line, content.width));
        }
        line_offsets.push(detail.wrapped.len());
        detail
            .wrapped_headers
            .extend(detail.headers.iter().map(|header| WrappedDetailHeader {
                id: header.id.clone(),
                range: line_offsets[header.line]..line_offsets[header.line + 1],
                section_end: line_offsets[header.end_line],
                line: header.line,
                width: detail.lines[header.line].width(),
            }));
        if let Some(anchor) = anchor {
            let (id, row, body_offset) = match anchor {
                DetailScrollAnchor::Header { id, row } => (id, row, None),
                DetailScrollAnchor::Body { id, offset } => (id, 0, Some(offset)),
            };
            if let Some(header) = detail.wrapped_headers.iter().find(|header| header.id == id) {
                detail.offset = if let Some(offset) = body_offset {
                    header
                        .range
                        .end
                        .saturating_add(offset)
                        .min(header.section_end.saturating_sub(1))
                } else {
                    // Leave room for ancestors above the selected heading.
                    let body = capacity.saturating_sub(EntityDetailPopup::sticky_limit(capacity));
                    let row = row.min(body.saturating_sub(header.range.len().min(body)));
                    header.range.start.saturating_sub(row)
                };
            }
        }
        detail.wrapped_width = Some(content.width);
        detail.wrapped_height = capacity;
    }
    detail.line_count = detail.wrapped.len();
    let max_offset = detail.scroll_limit(capacity);
    detail.offset = detail.offset.min(max_offset);
    let sticky = detail.sticky_headers(detail.offset, capacity);
    let pinned_count = sticky.len() as u16;
    let body = Rect::new(
        content.x,
        content.y.saturating_add(pinned_count),
        content.width,
        content.height.saturating_sub(pinned_count),
    );
    let body_capacity = usize::from(body.height);
    let pinned: Vec<_> = sticky
        .iter()
        .map(|header| {
            let line = sticky_detail_line(&detail.lines[header.line], content.width);
            (header.id.clone(), line)
        })
        .collect();
    let visible = detail
        .wrapped
        .iter()
        .skip(detail.offset)
        .take(body_capacity)
        .cloned()
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().fg(palette.foreground)),
        body,
    );
    if !pinned.is_empty() {
        frame.render_widget(
            Paragraph::new(
                pinned
                    .iter()
                    .map(|(_, line)| line.clone())
                    .collect::<Vec<_>>(),
            )
            .style(
                Style::default()
                    .fg(palette.foreground)
                    .bg(palette.gauge_track),
            ),
            Rect::new(content.x, content.y, content.width, pinned_count),
        );
    }
    let scrollbar = scrollbar_geometry(
        Rect::new(content.right(), content.y, 1, content.height),
        max_offset.saturating_add(body_capacity),
        body_capacity,
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
        body,
        scrollbar,
        ..EntityDetailHitbox::default()
    };
    let has_sections = !detail.headers.is_empty();
    detail.section_hitboxes.clear();
    for (row, (id, line)) in pinned.iter().enumerate() {
        detail.section_hitboxes.push((
            id.clone(),
            Rect::new(
                content.x,
                content.y.saturating_add(row as u16),
                line.width().min(usize::from(content.width)) as u16,
                1,
            ),
        ));
    }
    for header in &detail.wrapped_headers {
        let start = header.range.start.max(detail.offset);
        let end = header
            .range
            .end
            .min(detail.offset.saturating_add(body_capacity));
        if start < end {
            let width = if header.range.len() == 1 {
                header.width.min(usize::from(content.width)) as u16
            } else {
                content.width
            };
            let rect = Rect::new(
                body.x,
                body.y.saturating_add((start - detail.offset) as u16),
                width,
                (end - start) as u16,
            );
            detail.section_hitboxes.push((header.id.clone(), rect));
        }
    }
    let mut spans = Vec::new();
    let mut x = controls.x;
    let full = controls.width >= if has_sections { 55 } else { 26 };
    let mut specs: Vec<(&str, &str, bool)> = Vec::new();
    let scroll_controls = !has_sections || controls.width >= 21;
    if scroll_controls && controls.width >= 13 {
        specs.push(("↑", if full { " Up" } else { "" }, detail.offset > 0));
        specs.push((
            "↓",
            if full { " Down" } else { "" },
            detail.offset < max_offset,
        ));
    }
    if has_sections && controls.width >= 14 {
        specs.push(("Tab", if full { " Next" } else { "" }, true));
    }
    if has_sections && controls.width >= 7 {
        specs.push((
            "↵",
            if full { " Toggle" } else { "" },
            detail.selected_section.is_some(),
        ));
    }
    specs.push(("←", if full { " Back" } else { "" }, true));
    for (key, suffix, active) in specs {
        let leading = if spans.is_empty() {
            ""
        } else if full {
            "  "
        } else {
            " "
        };
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
            "Tab" => hitbox.next = control,
            "↵" => hitbox.toggle = control,
            _ => hitbox.back = control,
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), controls);
    hitbox
}
