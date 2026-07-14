use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

const STARTUP_LOG_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupEvent {
    pub stage: String,
    pub started_us: u64,
    pub duration_us: u64,
    pub finished_us: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupReport {
    pub schema_version: u32,
    pub started_at: DateTime<Utc>,
    pub total_duration_us: u64,
    pub events: Vec<StartupEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_error: Option<String>,
}

#[derive(Clone, Default)]
pub struct StartupTrace {
    inner: Option<Arc<TraceInner>>,
}

struct TraceInner {
    origin: Instant,
    started_at: DateTime<Utc>,
    active: AtomicBool,
    incremental_log: bool,
    state: Mutex<TraceState>,
}

struct TraceState {
    events: Vec<StartupEvent>,
    writer: Option<BufWriter<File>>,
    log_error: Option<String>,
    stopped_us: Option<u64>,
}

impl fmt::Debug for StartupTrace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartupTrace")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl StartupTrace {
    pub fn enabled(origin: Instant, log_path: Option<&Path>) -> Result<Self> {
        let started_at = SystemTime::now()
            .checked_sub(origin.elapsed())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(Utc::now);
        let mut writer = match log_path {
            Some(path) => {
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "could not create startup-log directory {}",
                            parent.display()
                        )
                    })?;
                }
                Some(BufWriter::new(File::create(path).with_context(|| {
                    format!("could not create startup log {}", path.display())
                })?))
            }
            None => None,
        };
        if let Some(writer) = writer.as_mut() {
            serde_json::to_writer(
                &mut *writer,
                &json!({
                    "schemaVersion": STARTUP_LOG_SCHEMA_VERSION,
                    "event": "trace_start",
                    "startedAt": started_at,
                    "pid": std::process::id(),
                }),
            )
            .context("could not initialize startup log")?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }

        Ok(Self {
            inner: Some(Arc::new(TraceInner {
                origin,
                started_at,
                active: AtomicBool::new(true),
                incremental_log: writer.is_some(),
                state: Mutex::new(TraceState {
                    events: Vec::new(),
                    writer,
                    log_error: None,
                    stopped_us: None,
                }),
            })),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn is_active(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.active.load(Ordering::Acquire))
    }

    pub fn span(&self, stage: &'static str) -> StartupSpan {
        if !self.is_active() {
            return StartupSpan {
                trace: StartupTrace::default(),
                stage,
                started: None,
                finished: false,
            };
        }
        let started = Instant::now();
        if self
            .inner
            .as_ref()
            .is_some_and(|inner| inner.incremental_log)
        {
            self.write_marker(json!({
                "schemaVersion": STARTUP_LOG_SCHEMA_VERSION,
                "event": "stage_start",
                "stage": stage,
                "atUs": self.offset_us(started),
            }));
        }
        StartupSpan {
            trace: self.clone(),
            stage,
            started: Some(started),
            finished: false,
        }
    }

    pub fn record(&self, stage: &'static str, started: Instant, detail: impl Into<String>) {
        self.record_interval(stage, started, Instant::now(), detail);
    }

    pub fn record_with(
        &self,
        stage: &'static str,
        started: Instant,
        detail: impl FnOnce() -> String,
    ) {
        if self.is_active() {
            self.record(stage, started, detail());
        }
    }

    pub fn record_interval(
        &self,
        stage: &'static str,
        started: Instant,
        finished: Instant,
        detail: impl Into<String>,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if !inner.active.load(Ordering::Acquire) {
            return;
        }
        let event = StartupEvent {
            stage: stage.to_string(),
            started_us: duration_us(started.saturating_duration_since(inner.origin)),
            duration_us: duration_us(finished.saturating_duration_since(started)),
            finished_us: duration_us(finished.saturating_duration_since(inner.origin)),
            detail: detail.into(),
        };
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.active.load(Ordering::Acquire) {
            return;
        }
        write_log_value(
            &mut state,
            &json!({
                "schemaVersion": STARTUP_LOG_SCHEMA_VERSION,
                "event": "stage_finish",
                "stage": event.stage,
                "startedUs": event.started_us,
                "durationUs": event.duration_us,
                "finishedUs": event.finished_us,
                "detail": event.detail,
            }),
        );
        state.events.push(event);
    }

    pub fn finish(&self, stage: &'static str, detail: impl Into<String>) {
        let Some(inner) = &self.inner else {
            return;
        };
        if !inner.active.load(Ordering::Acquire) {
            return;
        }
        self.record(stage, inner.origin, detail);
        self.stop();
    }

    pub fn stop(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        if !inner.active.swap(false, Ordering::AcqRel) {
            return;
        }
        let stopped_us = duration_us(inner.origin.elapsed());
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stopped_us = Some(stopped_us);
        write_log_value(
            &mut state,
            &json!({
                "schemaVersion": STARTUP_LOG_SCHEMA_VERSION,
                "event": "trace_finish",
                "totalDurationUs": stopped_us,
            }),
        );
        state.writer = None;
    }

    pub fn report(&self) -> StartupReport {
        let Some(inner) = &self.inner else {
            return StartupReport {
                schema_version: STARTUP_LOG_SCHEMA_VERSION,
                started_at: Utc::now(),
                total_duration_us: 0,
                events: Vec::new(),
                log_error: None,
            };
        };
        let state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut events = state.events.clone();
        events.sort_by(|left, right| {
            is_startup_summary(&left.stage)
                .cmp(&is_startup_summary(&right.stage))
                .then_with(|| left.started_us.cmp(&right.started_us))
                .then_with(|| left.finished_us.cmp(&right.finished_us))
                .then_with(|| left.stage.cmp(&right.stage))
        });
        StartupReport {
            schema_version: STARTUP_LOG_SCHEMA_VERSION,
            started_at: inner.started_at,
            total_duration_us: state
                .stopped_us
                .unwrap_or_else(|| duration_us(inner.origin.elapsed())),
            events,
            log_error: state.log_error.clone(),
        }
    }

    pub fn render_text(&self) -> String {
        let report = self.report();
        let mut output = String::new();
        let _ = writeln!(output, "Cold-start profile");
        let _ = writeln!(output, "started: {}", report.started_at.to_rfc3339());
        let _ = writeln!(
            output,
            "total:   {}",
            format_duration_us(report.total_duration_us)
        );
        let _ = writeln!(
            output,
            "note:    nested spans overlap; durations are not additive"
        );
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "{:>11}  {:>11}  {:<30}  DETAIL",
            "START", "DURATION", "STAGE"
        );
        for event in report.events {
            let _ = writeln!(
                output,
                "{:>11}  {:>11}  {:<30}  {}",
                format_duration_us(event.started_us),
                format_duration_us(event.duration_us),
                event.stage,
                event.detail
            );
        }
        if let Some(error) = report.log_error {
            let _ = writeln!(output, "\nstartup log warning: {error}");
        }
        output.trim_end().to_string()
    }

    pub fn render_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.report()).context("could not serialize startup profile")
    }

    fn offset_us(&self, instant: Instant) -> u64 {
        self.inner
            .as_ref()
            .map(|inner| duration_us(instant.saturating_duration_since(inner.origin)))
            .unwrap_or(0)
    }

    fn write_marker(&self, value: Value) {
        let Some(inner) = &self.inner else {
            return;
        };
        if !inner.active.load(Ordering::Acquire) {
            return;
        }
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.active.load(Ordering::Acquire) {
            write_log_value(&mut state, &value);
        }
    }
}

pub struct StartupSpan {
    trace: StartupTrace,
    stage: &'static str,
    started: Option<Instant>,
    finished: bool,
}

impl StartupSpan {
    pub fn finish(mut self, detail: impl Into<String>) {
        self.finished = true;
        if let Some(started) = self.started {
            self.trace
                .record_interval(self.stage, started, Instant::now(), detail);
        }
    }

    pub fn finish_with(mut self, detail: impl FnOnce() -> String) {
        self.finished = true;
        if let Some(started) = self.started
            && self.trace.is_active()
        {
            self.trace
                .record_interval(self.stage, started, Instant::now(), detail());
        }
    }
}

impl Drop for StartupSpan {
    fn drop(&mut self) {
        if !self.finished
            && let Some(started) = self.started
        {
            self.trace.record(self.stage, started, "status=interrupted");
        }
    }
}

fn write_log_value(state: &mut TraceState, value: &Value) {
    if state.log_error.is_some() {
        return;
    }
    let result = match state.writer.as_mut() {
        Some(writer) => (|| -> std::io::Result<()> {
            serde_json::to_writer(&mut *writer, value).map_err(std::io::Error::other)?;
            writer.write_all(b"\n")?;
            writer.flush()
        })(),
        None => return,
    };
    if let Err(error) = result {
        state.log_error = Some(error.to_string());
        state.writer = None;
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn is_startup_summary(stage: &str) -> bool {
    matches!(
        stage,
        "startup.ready" | "startup.complete" | "startup.failed"
    )
}

fn format_duration_us(microseconds: u64) -> String {
    if microseconds < 1_000 {
        format!("{microseconds}us")
    } else if microseconds < 1_000_000 {
        format!("{:.2}ms", microseconds as f64 / 1_000.0)
    } else {
        format!("{:.3}s", microseconds as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn disabled_trace_is_a_no_op() {
        let trace = StartupTrace::default();
        trace.span("disabled").finish("ignored");
        trace
            .span("disabled.lazy")
            .finish_with(|| panic!("disabled trace evaluated a lazy detail"));
        trace.record_with("disabled.record", Instant::now(), || {
            panic!("disabled trace evaluated a lazy record")
        });
        trace.finish("startup.ready", "ignored");

        let report = trace.report();
        assert_eq!(report.total_duration_us, 0);
        assert!(report.events.is_empty());
    }

    #[test]
    fn enabled_trace_records_spans_and_incremental_jsonl() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested/startup.jsonl");
        let origin = Instant::now();
        let trace = StartupTrace::enabled(origin, Some(&path)).unwrap();

        trace.span("rollout.discover").finish("files=2");
        {
            let _interrupted = trace.span("app_server.initialize");
        }
        trace.finish("startup.ready", "backend=test");
        trace
            .span("after_finish")
            .finish_with(|| panic!("finished trace evaluated a lazy detail"));

        let report = trace.report();
        assert_eq!(
            report
                .events
                .iter()
                .map(|event| event.stage.as_str())
                .collect::<Vec<_>>(),
            ["rollout.discover", "app_server.initialize", "startup.ready"]
        );
        assert_eq!(report.events[0].detail, "files=2");
        assert_eq!(report.events[1].detail, "status=interrupted");
        assert!(report.log_error.is_none());

        let records = fs::read_to_string(path).unwrap();
        assert!(records.contains("\"event\":\"trace_start\""));
        assert!(records.contains("\"event\":\"stage_start\""));
        assert!(records.contains("\"event\":\"stage_finish\""));
        assert!(records.contains("\"event\":\"trace_finish\""));
        assert!(
            records
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
    }

    #[test]
    fn concurrent_spans_overlap_and_keep_incremental_jsonl_valid() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("startup.jsonl");
        let trace = StartupTrace::enabled(Instant::now(), Some(&path)).unwrap();
        let local_span = trace.span("snapshot.local_scan");
        let (account_started_tx, account_started_rx) = std::sync::mpsc::channel();
        let (account_release_tx, account_release_rx) = std::sync::mpsc::channel();
        let timeout = Duration::from_secs(5);

        std::thread::scope(|scope| {
            let account_trace = trace.clone();
            let account = scope.spawn(move || {
                let span = account_trace.span("snapshot.account_fetch");
                account_started_tx.send(()).unwrap();
                account_release_rx.recv_timeout(timeout).unwrap();
                span.finish("parallel=true");
            });
            let account_started = account_started_rx.recv_timeout(timeout);
            local_span.finish("files=2");
            let _ = account_release_tx.send(());
            assert!(
                account_started.is_ok(),
                "account span did not start in time"
            );
            account.join().unwrap();
        });
        trace.finish("startup.ready", "backend=test");

        let report = trace.report();
        let local = report
            .events
            .iter()
            .find(|event| event.stage == "snapshot.local_scan")
            .unwrap();
        let account = report
            .events
            .iter()
            .find(|event| event.stage == "snapshot.account_fetch")
            .unwrap();
        assert!(local.started_us <= account.finished_us);
        assert!(account.started_us <= local.finished_us);

        let records = fs::read_to_string(path).unwrap();
        assert!(
            records
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
    }

    #[test]
    fn reports_render_as_text_and_json_without_timing_thresholds() {
        let trace = StartupTrace::enabled(Instant::now(), None).unwrap();
        trace.span("snapshot.derive").finish("tasks=3 turns=4");
        trace.finish("startup.complete", "mode=one_shot");

        let text = trace.render_text();
        assert!(text.contains("snapshot.derive"));
        assert!(text.contains("tasks=3 turns=4"));

        let json: Value = serde_json::from_str(&trace.render_json().unwrap()).unwrap();
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["events"][0]["stage"], "snapshot.derive");
    }

    #[test]
    fn reports_sort_nested_spans_by_start_while_leaving_summary_last() {
        let origin = Instant::now();
        let trace = StartupTrace::enabled(origin, None).unwrap();
        trace.record_interval(
            "inner",
            origin + Duration::from_millis(2),
            origin + Duration::from_millis(3),
            "",
        );
        trace.record_interval(
            "outer",
            origin + Duration::from_millis(1),
            origin + Duration::from_millis(4),
            "",
        );
        trace.finish("startup.ready", "backend=test");

        assert_eq!(
            trace
                .report()
                .events
                .iter()
                .map(|event| event.stage.as_str())
                .collect::<Vec<_>>(),
            ["outer", "inner", "startup.ready"]
        );
    }
}
