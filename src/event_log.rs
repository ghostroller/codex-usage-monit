//! Persistent application diagnostics, separate from content-free timing traces.
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use clap::ValueEnum;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::diagnostic_log::{DIAGNOSTIC_LOG_MAX_BYTES, JsonlWriter, prune_event_sessions};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    #[default]
    Warn,
    Info,
    Debug,
}

#[derive(Clone, Default)]
pub struct EventLog {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    path: Option<PathBuf>,
    level: LogLevel,
    run_id: String,
    home: Option<String>,
    managed_session: bool,
    state: Mutex<State>,
}

struct State {
    writer: Option<JsonlWriter>,
    error: Option<String>,
    previous: BTreeMap<&'static str, BTreeMap<String, (LogLevel, &'static str)>>,
    sequence: u64,
}

#[derive(Default)]
struct RecordContext<'a> {
    kind: Option<&'static str>,
    status: Option<&'static str>,
    operation: Option<&'a str>,
    subject: Option<&'a str>,
    diagnostic: Option<(&'a str, &'static str)>,
    // A recovery closes an already visible diagnostic even at warn/error level.
    filter_level: Option<LogLevel>,
}

impl fmt::Debug for EventLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventLog")
            .field("path", &self.path())
            .finish()
    }
}

impl EventLog {
    pub(crate) fn default_path() -> Option<PathBuf> {
        let state = crate::ui_state::default_ui_state_path()?;
        Some(state.parent()?.join("logs").join(format!(
            "tui-{}-{}.jsonl",
            Utc::now().format("%Y%m%d%H%M%S%3f"),
            std::process::id()
        )))
    }

    pub fn open(path: Option<PathBuf>, level: LogLevel, managed: bool) -> Self {
        if level == LogLevel::Off {
            return Self::default();
        }
        let result = path.as_deref().map_or_else(
            || {
                Err(std::io::Error::other(
                    "cannot resolve event log directory; use --log-file FILE",
                ))
            },
            |path| JsonlWriter::open_events(path, DIAGNOSTIC_LOG_MAX_BYTES),
        );
        let (writer, error) = match result {
            Ok(writer) => (Some(writer), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let log = Self {
            inner: Some(Arc::new(Inner {
                path,
                level,
                managed_session: managed,
                run_id: format!(
                    "{}-{}",
                    std::process::id(),
                    Utc::now().timestamp_nanos_opt().unwrap_or_default()
                ),
                home: std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .ok()
                    .filter(|value| !value.is_empty()),
                state: Mutex::new(State {
                    writer,
                    error,
                    previous: BTreeMap::new(),
                    sequence: 0,
                }),
            })),
        };
        log.write_metadata();
        if managed
            && log.error().is_none()
            && let Some(parent) = log.path().and_then(Path::parent)
            && let Err(error) = prune_event_sessions(parent, 20)
        {
            log.record(LogLevel::Warn, "log.retention", &error.to_string());
        }
        log
    }

    pub fn path(&self) -> Option<&Path> {
        self.inner.as_ref()?.path.as_deref()
    }

    pub fn error(&self) -> Option<String> {
        self.inner
            .as_ref()?
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error
            .clone()
    }

    pub fn is_active(&self) -> bool {
        self.inner.as_ref().is_some_and(|inner| {
            inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .writer
                .is_some()
        })
    }

    fn write_metadata(&self) {
        let Some(inner) = &self.inner else { return };
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        write(
            &mut state,
            json!({
                "schemaVersion": 1, "event": "session_start", "at": Utc::now(),
                "level": "info", "kind": "lifecycle", "status": "started", "sequence": 0,
                "runId": inner.run_id, "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"), "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH, "logLevel": inner.level,
                "managedSession": inner.managed_session,
            }),
        );
    }

    pub fn record(&self, level: LogLevel, event: &'static str, message: &str) {
        self.record_context(level, event, message, RecordContext::default());
    }

    pub fn lifecycle(&self, event: &'static str, status: &'static str, message: &str) {
        self.record_context(
            LogLevel::Info,
            event,
            message,
            RecordContext {
                kind: Some("lifecycle"),
                status: Some(status),
                ..RecordContext::default()
            },
        );
    }

    fn record_context(
        &self,
        level: LogLevel,
        event: &'static str,
        message: &str,
        context: RecordContext<'_>,
    ) {
        let Some(inner) = &self.inner else { return };
        if level == LogLevel::Off || context.filter_level.unwrap_or(level) > inner.level {
            return;
        }
        let mut message = sanitize_message(message);
        if let Some(home) = &inner.home {
            message = message
                .replace(home, "<home>")
                .replace(&home.replace('\\', "/"), "<home>");
        }
        if let Some(subject) = context.subject.filter(|value| !value.is_empty()) {
            message = message.replace(subject, "<source>");
        }
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.sequence += 1;
        let sequence = state.sequence;
        write(
            &mut state,
            json!({
                "schemaVersion": 1, "at": Utc::now(), "runId": inner.run_id,
                "pid": std::process::id(), "sequence": sequence, "level": level,
                "event": event, "message": message, "operationId": context.operation,
                "sourceId": context.subject.map(fingerprint),
                "kind": context.kind.unwrap_or(match level {
                    LogLevel::Error | LogLevel::Warn => "diagnostic",
                    LogLevel::Debug => "telemetry",
                    _ => "state",
                }),
                "status": context.status,
                "diagnosticId": context.diagnostic.map(|(id, _)| id),
                "diagnosticEvent": context.diagnostic.map(|(_, event)| event),
                "managedSession": inner.managed_session,
            }),
        );
    }

    /// Emit state changes only. Diagnostics carry a stable ID and a recovery
    /// when absent from the next complete observation. Info states do not emit
    /// recoveries. Absence means no longer observed, not independently verified.
    pub fn observe(&self, scope: &'static str, issues: &[(LogLevel, &'static str, String)]) {
        let Some(inner) = &self.inner else { return };
        let mut current = BTreeMap::new();
        let mut new = Vec::new();
        let mut resolved = Vec::new();
        {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.writer.is_none() {
                return;
            }
            let previous = state.previous.entry(scope).or_default();
            for (level, event, message) in issues.iter().take(1024) {
                if *level == LogLevel::Off || *level > inner.level {
                    continue;
                }
                let key = fingerprint(&format!("{scope}:{level:?}:{event}:{message}"));
                if current.insert(key.clone(), (*level, *event)).is_none()
                    && !previous.contains_key(&key)
                {
                    new.push((key, *level, *event, message));
                }
            }
            // A truncated observation cannot establish that omitted issues ended.
            if issues.len() <= 1024 {
                for (key, (level, event)) in previous.iter() {
                    if *level <= LogLevel::Warn && !current.contains_key(key) {
                        resolved.push((key.clone(), *level, *event));
                    }
                }
            }
            *previous = current;
        }
        for (key, level, event, message) in new {
            self.record_context(
                level,
                event,
                message,
                RecordContext {
                    status: Some(if level <= LogLevel::Warn {
                        "active"
                    } else {
                        "observed"
                    }),
                    diagnostic: (level <= LogLevel::Warn).then_some((key.as_str(), event)),
                    ..RecordContext::default()
                },
            );
        }
        for (key, original_level, event) in resolved {
            self.record_context(
                LogLevel::Info,
                "diagnostic.resolved",
                &format!("{event}: diagnostic no longer present in observed state"),
                RecordContext {
                    kind: Some("diagnostic"),
                    status: Some("resolved"),
                    diagnostic: Some((&key, event)),
                    filter_level: Some(original_level),
                    ..RecordContext::default()
                },
            );
        }
    }

    pub fn operation(&self, event: &'static str, subject: &str) -> OperationLog {
        let operation = OperationLog {
            log: self.clone(),
            event,
            subject: subject.to_owned(),
            id: format!(
                "{}-{}",
                std::process::id(),
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ),
            started: Instant::now(),
        };
        self.record_context(
            LogLevel::Info,
            event,
            "started",
            RecordContext {
                kind: Some("operation"),
                status: Some("started"),
                operation: Some(&operation.id),
                subject: Some(subject),
                ..RecordContext::default()
            },
        );
        operation
    }

    pub fn finish(&self) {
        if let Some(inner) = &self.inner {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.writer.take();
        }
    }
}

pub struct OperationLog {
    log: EventLog,
    event: &'static str,
    subject: String,
    id: String,
    started: Instant,
}

impl OperationLog {
    pub fn finish(self, level: LogLevel, message: &str) {
        self.log.record_context(
            level,
            self.event,
            &format!(
                "{message} (durationMs={})",
                self.started.elapsed().as_millis()
            ),
            RecordContext {
                kind: Some("operation"),
                status: Some(if level <= LogLevel::Warn {
                    "failed"
                } else {
                    "completed"
                }),
                operation: Some(&self.id),
                subject: Some(&self.subject),
                ..RecordContext::default()
            },
        );
    }
}

fn write(state: &mut State, value: serde_json::Value) {
    let Some(writer) = state.writer.as_mut() else {
        return;
    };
    if let Err(error) = writer.write_json_line(&value).and_then(|()| writer.flush()) {
        state.error = Some(error.to_string());
        state.writer = None;
    }
}

pub(crate) fn fingerprint(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..16].to_owned()
}

/// Keep bounded error text; omit lines containing common credential formats.
/// Callers pass diagnostic messages only, never payloads or session contents.
fn sanitize_message(message: &str) -> String {
    let bounded = message.chars().take(8192).collect::<String>();
    let mut private_key = false;
    let mut lines = Vec::new();
    for line in bounded.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("-----begin") && lower.contains("private key") {
            private_key = true;
        }
        let sensitive = private_key || contains_credentials(&lower);
        if lower.contains("-----end") && lower.contains("private key") {
            private_key = false;
        }
        if sensitive {
            lines.push("[redacted sensitive diagnostic]".to_owned());
        } else {
            lines.push(
                line.chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect::<String>(),
            );
        }
    }
    let mut result = lines.join(" ");
    if message.chars().count() > 8192 {
        result.push_str(" [truncated]");
    }
    result
}

fn contains_credentials(lower: &str) -> bool {
    if lower.contains("bearer ") {
        return true;
    }
    for key in [
        "authorization",
        "password",
        "access_token",
        "refresh_token",
        "api_key",
        "api-key",
        "apikey",
        "token",
        "secret",
    ] {
        for (index, _) in lower.match_indices(key) {
            let tail = lower[index + key.len()..].trim_start_matches([' ', '\t', '"', '\'']);
            if tail.starts_with([':', '=']) {
                return true;
            }
        }
    }
    lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .any(|token| {
            ["sk-", "ghp_", "github_pat_", "glpat-"]
                .iter()
                .any(|prefix| token.starts_with(prefix))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn event_log_flushes_live_appends_and_filters_levels() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs/events.jsonl");
        let log = EventLog::open(Some(path.clone()), LogLevel::Warn, false);
        assert_eq!(log.error(), None);
        log.record(LogLevel::Info, "hidden", "not requested");
        log.record(
            LogLevel::Warn,
            "history.warning",
            "state directory DACL grants access",
        );
        // Reading while the Windows TUI is still writing must work.
        assert_eq!(records(&path).len(), 2);
        let other = EventLog::open(Some(path.clone()), LogLevel::Warn, false);
        assert!(other.error().is_some());
        log.finish();
        let reopened = EventLog::open(Some(path.clone()), LogLevel::Error, false);
        reopened.record(LogLevel::Warn, "hidden", "filtered");
        reopened.record(
            LogLevel::Error,
            "remote.test",
            "system SSH exited with code 127",
        );
        reopened.finish();
        let records = records(&path);
        assert_eq!(records.len(), 4);
        assert_eq!(records[1]["event"], "history.warning");
        assert_eq!(records[3]["level"], "error");
        assert_ne!(records[0]["runId"], records[2]["runId"]);
        let off = temp.path().join("off.jsonl");
        EventLog::open(Some(off.clone()), LogLevel::Off, false).record(
            LogLevel::Error,
            "off",
            "ignored",
        );
        assert!(!off.exists());
    }

    #[test]
    fn event_log_deduplicates_refreshes_but_records_recurrence_and_operations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let log = EventLog::open(Some(path.clone()), LogLevel::Info, false);
        let issues = vec![(LogLevel::Warn, "history.warning", "ACL denied".to_owned())];
        log.observe("tui", &issues);
        log.observe("tui", &issues);
        log.observe("tui", &[]);
        log.observe("tui", &issues);
        for _ in 0..2 {
            log.operation("remote.test", "private-host")
                .finish(LogLevel::Error, "private-host: command not found");
        }
        let records = records(&path);
        assert_eq!(
            records
                .iter()
                .filter(|row| row["event"] == "history.warning")
                .count(),
            2
        );
        let operations: Vec<_> = records
            .iter()
            .filter(|row| row["event"] == "remote.test")
            .collect();
        assert_eq!(operations.len(), 4);
        assert_eq!(operations[0]["operationId"], operations[1]["operationId"]);
        assert_ne!(operations[1]["operationId"], operations[2]["operationId"]);
        assert_eq!(operations[0]["sourceId"], operations[3]["sourceId"]);
        assert_eq!(operations[0]["kind"], "operation");
        assert_eq!(operations[0]["status"], "started");
        assert_eq!(operations[1]["status"], "failed");
        let recovery = records
            .iter()
            .find(|row| row["event"] == "diagnostic.resolved")
            .unwrap();
        assert_eq!(recovery["diagnosticId"], records[1]["diagnosticId"]);
        assert_eq!(recovery["diagnosticEvent"], "history.warning");
        assert_eq!(recovery["status"], "resolved");
        assert!(
            !std::fs::read_to_string(path)
                .unwrap()
                .contains("private-host")
        );
    }

    #[test]
    fn event_log_recovery_closes_visible_diagnostics_without_promoting_filtered_states() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let log = EventLog::open(Some(path.clone()), LogLevel::Warn, false);
        let issues = [
            (LogLevel::Warn, "remote.history", "unavailable".to_owned()),
            (
                LogLevel::Info,
                "remote.auto_sync",
                "global-disabled".to_owned(),
            ),
        ];
        log.observe("tui", &issues);
        log.observe("tui", &[]);
        log.observe("tui", &[]);
        log.observe("tui", &issues);
        let rows = records(&path);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1]["status"], "active");
        assert_eq!(rows[2]["level"], "info");
        assert_eq!(rows[2]["event"], "diagnostic.resolved");
        assert_eq!(rows[1]["diagnosticId"], rows[2]["diagnosticId"]);
        assert_eq!(rows[1]["diagnosticId"], rows[3]["diagnosticId"]);

        let path = temp.path().join("info.jsonl");
        let info = EventLog::open(Some(path.clone()), LogLevel::Info, false);
        info.observe("states", &issues[1..]);
        info.observe("states", &[]);
        assert_eq!(
            records(&path).len(),
            2,
            "normal state changes are not recoveries"
        );
        assert_eq!(records(&path)[1]["kind"], "state");
    }

    #[test]
    fn event_log_redacts_credentials_and_bounds_unicode_messages() {
        let message = "SSH failed: command not found\nAuthorization: Bearer secret-value\npassword=secret-password\n{\"access_token\":\"secret-token\"}\n-----BEGIN OPENSSH PRIVATE KEY-----\nkey-material\n-----END OPENSSH PRIVATE KEY-----\nexit 127";
        let sanitized = sanitize_message(message);
        for secret in [
            "secret-value",
            "secret-password",
            "secret-token",
            "key-material",
        ] {
            assert!(!sanitized.contains(secret));
        }
        assert!(sanitized.contains("command not found"));
        assert!(sanitized.contains("exit 127"));
        let useful = "task-cache failed: Permission denied (publickey,password)";
        assert_eq!(sanitize_message(useful), useful);
        assert!(!sanitize_message("key sk-secret-value").contains("sk-secret-value"));
        assert!(!sanitize_message("\u{1b}[31merror\0").contains('\u{1b}'));
        let huge = sanitize_message(&"错".repeat(10000));
        assert!(huge.ends_with("[truncated]"));
        assert!(huge.len() < 25000);
    }

    #[test]
    fn event_log_retention_skips_active_files_and_unrelated_names() {
        let temp = tempfile::tempdir().unwrap();
        let live = temp.path().join("tui-000-1.jsonl");
        let active = EventLog::open(Some(live.clone()), LogLevel::Warn, true);
        for index in 1..=22 {
            let path = temp.path().join(format!("tui-{index:03}-1.jsonl"));
            EventLog::open(Some(path), LogLevel::Warn, true).finish();
        }
        let unrelated = temp.path().join("tui-not-ours.jsonl");
        std::fs::write(&unrelated, "preserve").unwrap();
        prune_event_sessions(temp.path(), 20).unwrap();
        assert!(live.exists());
        assert!(unrelated.exists());
        assert!(!temp.path().join("tui-001-1.jsonl").exists());
        active.finish();
    }

    #[test]
    fn event_log_reports_initialization_and_write_errors_without_panicking() {
        let temp = tempfile::tempdir().unwrap();
        let disabled = EventLog::open(Some(temp.path().to_owned()), LogLevel::Warn, false);
        assert!(disabled.error().is_some());
        disabled.record(LogLevel::Error, "ignored", "unavailable");
        struct Fails;
        impl std::io::Write for Fails {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = EventLog::open(
            Some(temp.path().join("events.jsonl")),
            LogLevel::Warn,
            false,
        );
        log.inner.as_ref().unwrap().state.lock().unwrap().writer =
            Some(JsonlWriter::from_stream(Box::new(Fails)));
        log.record(LogLevel::Error, "test", "failure");
        assert_eq!(log.error().as_deref(), Some("disk full"));
        assert!(!log.is_active());
    }
}
