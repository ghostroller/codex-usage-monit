//! Explicit local session deletion. Collection and account RPCs remain read-only.
use super::*;
use crate::deleted_sessions::{DeletedSessionRecord, DeletionStatus};
use crate::session_delete::{DeletionOutcome, PreparedSessionDeletion};
use unicode_segmentation::UnicodeSegmentation;

pub(super) const DELETE_ACTION_SECTION: &str = "action.delete-session";

#[derive(Clone, Debug)]
pub(super) struct SessionDeleteTarget {
    pub(super) thread_id: String,
    pub(super) title: String,
    pub(super) codex_home: PathBuf,
}

#[derive(Clone)]
pub(super) struct DeletedSessionInfo {
    pub(super) deleted_at: Option<DateTime<Utc>>,
    pub(super) captured_at: DateTime<Utc>,
}

pub(super) enum DeletePhase {
    QueuedPrepare,
    Preparing,
    Confirm(PreparedSessionDeletion),
    QueuedExecute(PreparedSessionDeletion),
    Executing,
    Failed(String),
}

pub(super) struct SessionDeleteDialog {
    pub(super) target: SessionDeleteTarget,
    pub(super) phase: DeletePhase,
}

pub(super) enum DeleteCompletion {
    Prepared(Result<Box<PreparedSessionDeletion>, String>),
    Executed(Result<DeletionOutcome, String>),
}

#[derive(Default)]
pub(super) struct SessionManagementState {
    pub(super) config: Option<CollectConfig>,
    pub(super) deleted: HashMap<String, DeletedSessionInfo>,
    pub(super) expanded: bool,
    pub(super) force_local_refresh: bool,
    pub(super) dialog: Option<SessionDeleteDialog>,
    pub(super) receiver: Option<Receiver<DeleteCompletion>>,
    pub(super) load_error: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct SessionDeleteHitbox {
    pub(super) confirm: Rect,
    pub(super) cancel: Rect,
}

pub(super) fn load_records(
    config: &CollectConfig,
    rollout_cache: &Arc<Mutex<RolloutCache>>,
) -> Result<Vec<DeletedSessionRecord>, String> {
    load_records_with_reconcile(config, rollout_cache, |config| {
        crate::session_delete::reconcile_pending_deletions(config)
            .map_err(|error| error.to_string())
    })
}

pub(super) fn load_records_with_reconcile(
    config: &CollectConfig,
    rollout_cache: &Arc<Mutex<RolloutCache>>,
    reconcile: impl FnOnce(&CollectConfig) -> Result<Vec<String>, String>,
) -> Result<Vec<DeletedSessionRecord>, String> {
    let mut records = crate::deleted_sessions::load_deleted_sessions(config)
        .map_err(|error| error.to_string())?;
    if !config.offline {
        let mut warnings = Vec::new();
        let needs_reconciliation = {
            let cache = rollout_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            records.iter().any(|record| {
                matches!(
                    record.status,
                    DeletionStatus::Pending | DeletionStatus::Unknown | DeletionStatus::Partial
                ) && !cache.has_live_thread(&record.thread_id)
            })
        };
        if needs_reconciliation {
            // This may start a read-only App Server inventory. Never hold the
            // shared rollout-cache mutex while waiting for that process.
            match reconcile(config) {
                Ok(recovered) if !recovered.is_empty() => {
                    match crate::deleted_sessions::load_deleted_sessions(config) {
                        Ok(reloaded) => records = reloaded,
                        Err(error) => warnings.push((LogLevel::Warn, "session_delete.reconciliation", format!("Read-only deletion reconciliation could not reload retained records: {error}"))),
                    }
                    rollout_cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
                        .invalidate_after_session_mutation();
                }
                Ok(_) => {}
                Err(error) => warnings.push((LogLevel::Warn, "session_delete.reconciliation", format!("Read-only deletion reconciliation failed; retained records are unchanged: {error}"))),
            }
        }
        config
            .event_log
            .observe("session_delete.reconciliation", &warnings);
    }
    let cache = rollout_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    records.retain(|record| {
        record.status != DeletionStatus::Deleted || !cache.has_live_thread(&record.thread_id)
    });
    Ok(records)
}

impl App {
    pub(super) fn deleted_thread_identity<'a>(&self, task: &'a TaskRecord) -> Option<&'a str> {
        if task.thread_id.starts_with("remote:")
            || task.thread_id.starts_with("logical-thread:")
            || task
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("remote:"))
        {
            return None;
        }
        let raw = if let Some((raw, scope)) = task.thread_id.rsplit_once('@') {
            if self
                .history_local_source_id
                .as_ref()
                .is_none_or(|local| local.as_str() != scope)
            {
                return None;
            }
            raw
        } else {
            task.thread_id.as_str()
        };
        self.session_management
            .deleted
            .contains_key(raw)
            .then_some(raw)
    }

    pub(super) fn is_deleted_task(&self, task: &TaskRecord) -> bool {
        self.deleted_thread_identity(task).is_some()
    }

    pub(super) fn selected_deleted_task(&self) -> bool {
        self.selected_task_record()
            .is_some_and(|task| self.is_deleted_task(task))
    }

    pub(super) fn apply_deleted_session_records(
        &mut self,
        records: Result<Vec<DeletedSessionRecord>, String>,
    ) {
        match records {
            Ok(records) => {
                self.session_management.deleted = records
                    .into_iter()
                    .filter(|record| record.status == DeletionStatus::Deleted)
                    .map(|record| {
                        (
                            record.thread_id,
                            DeletedSessionInfo {
                                deleted_at: record.deleted_at,
                                captured_at: record.captured_at,
                            },
                        )
                    })
                    .collect();
                self.session_management.load_error = None;
            }
            Err(error) => self.session_management.load_error = Some(error),
        }
    }

    pub(super) fn deleted_task_root_label(&self, matching: usize) -> String {
        format!(
            "[↵] {} 由 monit 删除的会话({matching})",
            if self.session_management.expanded {
                "▾"
            } else {
                "▸"
            }
        )
    }

    pub(super) fn deleted_task_label(&self, task: &TaskRecord, width: usize) -> String {
        let raw = self
            .deleted_thread_identity(task)
            .unwrap_or(&task.thread_id);
        let safe = terminal_safe_text(raw);
        let graphemes = safe.graphemes(true).collect::<Vec<_>>();
        let id = if graphemes.len() > 15 {
            format!(
                "{}…{}",
                graphemes[..8].concat(),
                graphemes[graphemes.len() - 6..].concat()
            )
        } else {
            safe
        };
        let title = terminal_safe_text(&task.title);
        let full = format!("Deleted · {title} [{id}]");
        if UnicodeWidthStr::width(full.as_str()) <= width {
            return full;
        }
        let suffix = format!(" [{id}]");
        let suffix_width = UnicodeWidthStr::width(suffix.as_str());
        if width > suffix_width {
            return format!(
                "{}{}",
                truncate_display_text(&title, width - suffix_width),
                suffix
            );
        }
        if UnicodeWidthStr::width(id.as_str()) <= width {
            return id;
        }
        // Preserve the differing UUID tail when the terminal cannot fit both
        // ends. UUIDv7 sessions can share their leading timestamp bytes.
        let mut tail = String::new();
        let mut used = 0;
        for grapheme in id.graphemes(true).rev() {
            let next = UnicodeWidthStr::width(grapheme);
            if used + next > width {
                break;
            }
            tail.insert_str(0, grapheme);
            used += next;
        }
        tail
    }

    pub(super) fn insert_deleted_task_rows(&self, rows: &mut Vec<TaskListRow>) {
        let query = self.task_search.to_lowercase();
        let mut by_thread = BTreeMap::new();
        for (index, task) in self.snapshot.tasks.iter().enumerate() {
            if !self.task_matches_filter_query(task, &query) {
                continue;
            }
            if let Some(raw) = self.deleted_thread_identity(task) {
                let record = &self.session_management.deleted[raw];
                let candidate = (index, record);
                match by_thread.get(raw) {
                    None => {
                        by_thread.insert(raw, candidate);
                    }
                    Some((previous, _))
                        if self.snapshot.tasks[*previous].thread_id != raw
                            && task.thread_id == raw =>
                    {
                        by_thread.insert(raw, candidate);
                    }
                    _ => {}
                }
            }
        }
        let mut deleted = by_thread.into_values().collect::<Vec<_>>();
        deleted.sort_by(|(left_index, left), (right_index, right)| {
            right.deleted_at.cmp(&left.deleted_at).then_with(|| {
                self.snapshot.tasks[*left_index]
                    .thread_id
                    .cmp(&self.snapshot.tasks[*right_index].thread_id)
            })
        });
        if deleted.is_empty() {
            return;
        }
        let latest_deleted_at = deleted[0].1.deleted_at;
        let mut insert_at = 0;
        // A normal tree's newest descendant ranks the whole branch, including
        // collapsed descendants. Insert only between complete top-level trees.
        for group in rows.chunk_by(|_, next| next.depth > 0) {
            let latest_activity = group
                .iter()
                .flat_map(|row| {
                    std::iter::once(row.index).chain(row.hidden_descendants.iter().copied())
                })
                .filter_map(|index| {
                    self.snapshot
                        .tasks
                        .get(index)
                        .and_then(|task| task.updated_at)
                })
                .max();
            if latest_activity < latest_deleted_at {
                break;
            }
            insert_at += group.len();
        }
        let mut deleted_rows = vec![TaskListRow {
            kind: TaskRowKind::DeletedRoot,
            index: DELETED_TASK_ROOT,
            prefix: String::new(),
            depth: 0,
            has_children: true,
            collapsed: !self.session_management.expanded,
            hidden_descendants: deleted.iter().map(|(index, _)| *index).collect(),
        }];
        if self.session_management.expanded {
            let count = deleted.len();
            for (position, (index, _)) in deleted.into_iter().enumerate() {
                deleted_rows.push(TaskListRow {
                    kind: TaskRowKind::DeletedSession,
                    index,
                    prefix: if position + 1 == count {
                        "└─ "
                    } else {
                        "├─ "
                    }
                    .into(),
                    depth: 1,
                    has_children: false,
                    collapsed: false,
                    hidden_descendants: Vec::new(),
                });
            }
        }
        rows.splice(insert_at..insert_at, deleted_rows);
    }

    pub(super) fn activate_selected_task(&mut self) {
        if self.selected_task == DELETED_TASK_ROOT {
            self.set_task_collapsed(DELETED_TASK_ROOT, self.session_management.expanded);
        } else if self.selected_deleted_task() {
            self.open_entity_detail();
        } else {
            self.focus_turns();
        }
    }

    pub(super) fn session_delete_visible(&self) -> bool {
        self.session_management.dialog.is_some()
    }

    pub(super) fn session_delete_busy(&self) -> bool {
        self.session_management.receiver.is_some()
            || self
                .session_management
                .dialog
                .as_ref()
                .is_some_and(|dialog| {
                    matches!(
                        dialog.phase,
                        DeletePhase::QueuedPrepare | DeletePhase::QueuedExecute(_)
                    )
                })
    }

    pub(super) fn entity_delete_available(&self) -> bool {
        self.entity_detail
            .as_ref()
            .is_some_and(|detail| detail.delete_target.is_some())
            && !self.session_delete_visible()
            && !self.session_delete_busy()
            && self.session_management.load_error.is_none()
            && self
                .session_management
                .config
                .as_ref()
                .is_some_and(|config| !config.offline)
    }

    pub(super) fn request_session_delete(&mut self) -> bool {
        if !self.entity_delete_available() {
            return false;
        }
        let target = self
            .entity_detail
            .as_ref()
            .and_then(|detail| detail.delete_target.clone())
            .unwrap();
        if self
            .session_management
            .config
            .as_ref()
            .is_none_or(|config| config.codex_home != target.codex_home)
        {
            return false;
        }
        self.session_management.dialog = Some(SessionDeleteDialog {
            target,
            phase: DeletePhase::QueuedPrepare,
        });
        self.session_delete_hitbox = None;
        self.scroll_drag = None;
        true
    }

    pub(super) fn cancel_session_delete(&mut self) {
        if self
            .session_management
            .dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.phase, DeletePhase::Executing))
        {
            return;
        }
        self.session_management.dialog = None;
        // Preparation is read-only. Dropping its receiver never authorizes execution.
        self.session_management.receiver = None;
        self.session_delete_hitbox = None;
    }

    pub(super) fn confirm_session_delete(&mut self) {
        if self
            .session_delete_hitbox
            .is_none_or(|hitbox| hitbox.confirm.is_empty())
        {
            return;
        }
        let Some(dialog) = self.session_management.dialog.as_mut() else {
            return;
        };
        if let DeletePhase::Confirm(prepared) = &dialog.phase {
            dialog.phase = DeletePhase::QueuedExecute(prepared.clone());
            self.session_delete_hitbox = None;
            self.close_entity_detail();
        }
    }

    /// Called by the run loop after prior refresh completions have been applied.
    /// The target and configuration are never reconstructed from selection.
    pub(super) fn start_session_delete_work(&mut self) {
        if self.worker_running || self.session_management.receiver.is_some() {
            return;
        }
        let Some(config) = self.session_management.config.clone() else {
            return;
        };
        let Some(dialog) = self.session_management.dialog.as_mut() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        match &dialog.phase {
            DeletePhase::QueuedPrepare => {
                let thread_id = dialog.target.thread_id.clone();
                dialog.phase = DeletePhase::Preparing;
                thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::session_delete::prepare_session_delete(&config, &thread_id)
                            .map(Box::new)
                            .map_err(|error| error.to_string())
                    }))
                    .unwrap_or_else(|_| {
                        Err("Session verification worker failed; no deletion was requested.".into())
                    });
                    let _ = sender.send(DeleteCompletion::Prepared(result));
                });
            }
            DeletePhase::QueuedExecute(prepared) => {
                let prepared = prepared.clone();
                dialog.phase = DeletePhase::Executing;
                thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                        crate::session_delete::execute_session_delete(&config, prepared).map_err(|error| error.to_string())
                    )).unwrap_or_else(|_| Err("Deletion worker failed; the result must be verified. Do not retry automatically.".into()));
                    let _ = sender.send(DeleteCompletion::Executed(result));
                });
            }
            _ => return,
        }
        self.session_management.receiver = Some(receiver);
    }

    pub(super) fn poll_session_delete(&mut self) -> bool {
        let result = match self
            .session_management
            .receiver
            .as_ref()
            .map(Receiver::try_recv)
        {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Disconnected)) => DeleteCompletion::Executed(Err(
                "Session worker disconnected; deletion result is unverified.".into(),
            )),
            _ => return false,
        };
        self.session_management.receiver = None;
        match result {
            DeleteCompletion::Prepared(Ok(prepared)) => {
                if let Some(dialog) = self.session_management.dialog.as_mut() {
                    if prepared.root_thread_id == dialog.target.thread_id {
                        dialog.phase = DeletePhase::Confirm(*prepared);
                    } else {
                        dialog.phase = DeletePhase::Failed(
                            "Session identity changed; deletion was not requested.".into(),
                        );
                    }
                }
            }
            DeleteCompletion::Prepared(Err(error)) | DeleteCompletion::Executed(Err(error)) => {
                if let Some(dialog) = self.session_management.dialog.as_mut() {
                    dialog.phase = DeletePhase::Failed(error);
                }
                self.session_management.force_local_refresh = true;
                self.summary_cache = None;
            }
            DeleteCompletion::Executed(Ok(outcome)) => {
                self.close_entity_detail();
                self.session_management.dialog = None;
                self.session_delete_hitbox = None;
                self.session_management.force_local_refresh = true;
                self.summary_cache = None;
                self.set_open_notice(
                    format!(
                        "Deleted {} session(s); retained usage is being refreshed",
                        outcome.deleted_threads.len()
                    ),
                    OpenNoticeTone::Success,
                );
            }
        }
        true
    }
}

pub(super) fn render_session_delete(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
) -> SessionDeleteHitbox {
    let width = area.width.saturating_sub(4).clamp(1, 76).min(area.width);
    let height = area.height.saturating_sub(2).clamp(1, 17).min(area.height);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = panel("Delete session", app.theme).style(app.theme.base_style());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let Some(dialog) = app.session_management.dialog.as_ref() else {
        return SessionDeleteHitbox::default();
    };
    let compact = inner.width < 37;
    let mut lines = vec![
        Line::from(truncate_display_text(
            &terminal_safe_text(&dialog.target.title),
            usize::from(inner.width),
        )),
        Line::from(format!(
            "Thread: {}",
            terminal_safe_text(&dialog.target.thread_id)
        )),
    ];
    let (mut confirm, cancel) = match &dialog.phase {
        DeletePhase::QueuedPrepare | DeletePhase::Preparing => {
            lines.push(Line::from("Reading local usage for confirmation…"));
            (false, true)
        }
        DeletePhase::Confirm(prepared) => {
            lines.extend([
                Line::from(format!(
                    "Sessions: {} (+{} spawned)",
                    prepared.thread_ids.len(),
                    prepared.thread_ids.len().saturating_sub(1)
                )),
                Line::from(format!(
                    "Captured tokens: {}",
                    format_tokens(prepared.total_tokens)
                )),
                Line::from(if compact {
                    "Delete permanently?"
                } else {
                    "Delete Codex sessions permanently?"
                }),
                Line::from(if compact {
                    "Usage retained. No undo."
                } else {
                    "Usage and short identification are retained in monit. No undo."
                }),
            ]);
            (true, true)
        }
        DeletePhase::QueuedExecute(_) => {
            lines.push(Line::from(
                "Waiting for the current refresh before deletion…",
            ));
            (false, true)
        }
        DeletePhase::Executing => {
            lines.push(Line::from("Deleting… Waiting for a verified result."));
            (false, false)
        }
        DeletePhase::Failed(error) => {
            lines.push(Line::from(terminal_safe_text(error)));
            lines.push(Line::from(
                "No automatic retry. Check the result before another request.",
            ));
            (false, true)
        }
    };
    confirm &= inner.width >= 18 && inner.height >= 8;
    let controls = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }),
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
    let mut spans = Vec::new();
    let mut x = controls.x;
    let mut hitbox = SessionDeleteHitbox::default();
    for (key, suffix, active) in [
        ("↵", if compact { "" } else { " Delete" }, confirm),
        ("←", if compact { "" } else { " Cancel" }, cancel),
    ] {
        let leading = if spans.is_empty() { "" } else { "  " };
        let rect = append_summary_control(
            &mut spans,
            controls,
            &mut x,
            SummaryControlSpec {
                leading,
                shortcut: key.into(),
                suffix,
                selected: false,
                shortcuts_active: active,
                theme: app.theme,
            },
        );
        if key == "↵" {
            hitbox.confirm = if confirm { rect } else { Rect::default() };
        } else {
            hitbox.cancel = if cancel { rect } else { Rect::default() };
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), controls);
    hitbox
}
