//! Opt-in, content-free trace logging for expensive and external operations.
//!
//! Unlike the aggregate performance log, this stream records individual
//! operation boundaries. Callers must use the typed [`TraceFields`] builder:
//! it deliberately has no free-form value API, so paths, commands, message
//! bodies, credentials, and raw remote identifiers cannot be written by
//! accident.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const TRACE_LOG_SCHEMA_VERSION: u32 = 1;
const OPAQUE_ID_HEX_BYTES: usize = 8;

static PROCESS_TRACE_LOG: OnceLock<RwLock<TraceLog>> = OnceLock::new();

#[derive(Clone, Default)]
pub struct TraceLog {
    inner: Option<Arc<TraceInner>>,
}

struct TraceInner {
    origin: Instant,
    active: AtomicBool,
    next_span_id: AtomicU64,
    state: Mutex<TraceState>,
}

struct TraceState {
    writer: Option<Box<dyn Write + Send>>,
    log_error: Option<String>,
    active_spans: BTreeMap<u64, ActiveSpan>,
}

#[derive(Clone, Copy)]
struct ActiveSpan {
    stage: &'static str,
    started: Instant,
}

impl fmt::Debug for TraceLog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TraceLog")
            .field("enabled", &self.is_enabled())
            .field("log_error", &self.log_error())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceOutcome {
    Ok,
    Partial,
    Error,
    Timeout,
    Cancelled,
    Skipped,
    Abandoned,
}

impl TraceOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::Abandoned => "abandoned",
        }
    }
}

/// A deliberately restricted set of content-free trace attributes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceFields {
    values: Map<String, Value>,
}

impl TraceFields {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bool(mut self, key: &'static str, value: bool) -> Self {
        self.values.insert(key.to_owned(), Value::Bool(value));
        self
    }

    pub fn u64(mut self, key: &'static str, value: u64) -> Self {
        self.values.insert(key.to_owned(), value.into());
        self
    }

    pub fn usize(self, key: &'static str, value: usize) -> Self {
        self.u64(key, u64::try_from(value).unwrap_or(u64::MAX))
    }

    pub fn duration_ms(self, key: &'static str, value: Duration) -> Self {
        self.u64(key, u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }

    /// Records one compile-time-controlled classification label.
    pub fn label(mut self, key: &'static str, value: &'static str) -> Self {
        self.values
            .insert(key.to_owned(), Value::String(value.to_owned()));
        self
    }

    /// Records a stable, one-way summary instead of the supplied identifier.
    pub fn opaque(mut self, key: &'static str, value: impl AsRef<[u8]>) -> Self {
        let digest = Sha256::digest(value.as_ref());
        let mut encoded = String::with_capacity(OPAQUE_ID_HEX_BYTES * 2);
        for byte in digest.iter().take(OPAQUE_ID_HEX_BYTES) {
            use std::fmt::Write as _;
            let _ = write!(encoded, "{byte:02x}");
        }
        self.values.insert(key.to_owned(), Value::String(encoded));
        self
    }

    fn into_value(self) -> Value {
        Value::Object(self.values)
    }
}

impl TraceLog {
    /// Creates or truncates a private JSONL trace. Initialization errors turn
    /// the logger into a disabled value and remain observable via `log_error`.
    pub fn enabled(path: &Path) -> Self {
        match open_writer(path) {
            Ok(writer) => Self::enabled_with_writer(writer),
            Err(error) => Self::disabled_with_error(error.to_string()),
        }
    }

    fn enabled_with_writer(writer: Box<dyn Write + Send>) -> Self {
        let trace = Self {
            inner: Some(Arc::new(TraceInner {
                origin: Instant::now(),
                active: AtomicBool::new(true),
                next_span_id: AtomicU64::new(1),
                state: Mutex::new(TraceState {
                    writer: Some(writer),
                    log_error: None,
                    active_spans: BTreeMap::new(),
                }),
            })),
        };
        trace.write(json!({
            "schemaVersion": TRACE_LOG_SCHEMA_VERSION,
            "event": "trace_start",
            "at": Utc::now(),
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "targetOs": std::env::consts::OS,
            "targetArch": std::env::consts::ARCH,
            "debugBuild": cfg!(debug_assertions),
        }));
        trace
    }

    fn disabled_with_error(error: String) -> Self {
        Self {
            inner: Some(Arc::new(TraceInner {
                origin: Instant::now(),
                active: AtomicBool::new(false),
                next_span_id: AtomicU64::new(1),
                state: Mutex::new(TraceState {
                    writer: None,
                    log_error: Some(error),
                    active_spans: BTreeMap::new(),
                }),
            })),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.active.load(Ordering::Acquire))
    }

    pub fn log_error(&self) -> Option<String> {
        self.inner
            .as_ref()?
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .log_error
            .clone()
    }

    pub fn span(&self, stage: &'static str, fields: TraceFields) -> TraceSpan {
        self.span_with(stage, || fields)
    }

    pub fn span_with(
        &self,
        stage: &'static str,
        fields: impl FnOnce() -> TraceFields,
    ) -> TraceSpan {
        let Some(inner) = self
            .inner
            .as_ref()
            .filter(|inner| inner.active.load(Ordering::Acquire))
        else {
            return TraceSpan::disabled(stage);
        };
        let span_id = inner.next_span_id.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let fields = fields();
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.active.load(Ordering::Acquire) {
            return TraceSpan::disabled(stage);
        }
        let value = json!({
            "schemaVersion": TRACE_LOG_SCHEMA_VERSION,
            "event": "span_start",
            "at": Utc::now(),
            "atUs": duration_us(inner.origin.elapsed()),
            "spanId": span_id,
            "stage": stage,
            "fields": fields.into_value(),
        });
        if !write_json_line(&mut state, &value) {
            inner.active.store(false, Ordering::Release);
            return TraceSpan::disabled(stage);
        }
        state
            .active_spans
            .insert(span_id, ActiveSpan { stage, started });
        drop(state);
        TraceSpan {
            log: self.clone(),
            stage,
            span_id: Some(span_id),
            started: Some(started),
            finished: false,
        }
    }

    pub fn finish(&self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        if !inner.active.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (span_id, span) in std::mem::take(&mut state.active_spans) {
            let value = json!({
                "schemaVersion": TRACE_LOG_SCHEMA_VERSION,
                "event": "span_finish",
                "at": Utc::now(),
                "atUs": duration_us(inner.origin.elapsed()),
                "spanId": span_id,
                "stage": span.stage,
                "durationUs": duration_us(span.started.elapsed()),
                "outcome": TraceOutcome::Abandoned.as_str(),
                "fields": TraceFields::new().into_value(),
            });
            write_json_line(&mut state, &value);
        }
        let value = json!({
            "schemaVersion": TRACE_LOG_SCHEMA_VERSION,
            "event": "trace_finish",
            "at": Utc::now(),
            "atUs": duration_us(inner.origin.elapsed()),
        });
        write_json_line(&mut state, &value);
        if let Some(writer) = state.writer.as_mut()
            && let Err(error) = writer.flush()
        {
            state.log_error = Some(error.to_string());
            state.writer = None;
            return;
        }
        // Close the lifecycle while holding the same mutex used by `write`.
        // A worker which observed `active=true` before `finish` but reaches
        // the mutex afterwards must not append events after `trace_finish`.
        state.writer = None;
    }

    fn finish_span_with(
        &self,
        stage: &'static str,
        span_id: u64,
        started: Instant,
        outcome: TraceOutcome,
        fields: impl FnOnce() -> TraceFields,
    ) {
        let Some(inner) = self
            .inner
            .as_ref()
            .filter(|inner| inner.active.load(Ordering::Acquire))
        else {
            return;
        };
        let state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.active.load(Ordering::Acquire) || !state.active_spans.contains_key(&span_id) {
            return;
        }
        drop(state);
        let fields = fields();
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.active.load(Ordering::Acquire) || state.active_spans.remove(&span_id).is_none() {
            return;
        }
        let value = json!({
            "schemaVersion": TRACE_LOG_SCHEMA_VERSION,
            "event": "span_finish",
            "at": Utc::now(),
            "atUs": duration_us(inner.origin.elapsed()),
            "spanId": span_id,
            "stage": stage,
            "durationUs": duration_us(started.elapsed()),
            "outcome": outcome.as_str(),
            "fields": fields.into_value(),
        });
        if !write_json_line(&mut state, &value) {
            inner.active.store(false, Ordering::Release);
        }
    }

    fn write(&self, value: Value) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        if !inner.active.load(Ordering::Acquire) {
            return;
        }
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.active.load(Ordering::Acquire) {
            return;
        }
        if !write_json_line(&mut state, &value) && state.log_error.is_some() {
            inner.active.store(false, Ordering::Release);
        }
    }
}

/// Installs the per-process logger for low-level helpers which cannot safely
/// grow a configuration parameter (for example the bounded Git runner).
/// Replacing it is supported for in-process command harnesses.
pub(crate) fn set_process_trace_log(log: TraceLog) {
    let slot = PROCESS_TRACE_LOG.get_or_init(|| RwLock::new(TraceLog::default()));
    *slot
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = log;
}

pub(crate) fn process_trace_log() -> TraceLog {
    PROCESS_TRACE_LOG
        .get()
        .map(|slot| {
            slot.read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
        .unwrap_or_default()
}

pub struct TraceSpan {
    log: TraceLog,
    stage: &'static str,
    span_id: Option<u64>,
    started: Option<Instant>,
    finished: bool,
}

impl TraceSpan {
    fn disabled(stage: &'static str) -> Self {
        Self {
            log: TraceLog::default(),
            stage,
            span_id: None,
            started: None,
            finished: false,
        }
    }

    pub fn finish(self, outcome: TraceOutcome, fields: TraceFields) {
        self.finish_with(outcome, || fields);
    }

    pub fn finish_with(mut self, outcome: TraceOutcome, fields: impl FnOnce() -> TraceFields) {
        self.finished = true;
        if let (Some(span_id), Some(started)) = (self.span_id, self.started) {
            self.log
                .finish_span_with(self.stage, span_id, started, outcome, fields);
        }
    }
}

impl Drop for TraceSpan {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let (Some(span_id), Some(started)) = (self.span_id, self.started) {
            self.log.finish_span_with(
                self.stage,
                span_id,
                started,
                TraceOutcome::Abandoned,
                TraceFields::new,
            );
        }
    }
}

fn open_writer(path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut options = OpenOptions::new();
        options.create(true).write(true);
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path)?;
        ensure_regular_file(&file)?;
        lock_trace_file(&file)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.set_len(0)?;
        return Ok(Box::new(BufWriter::new(file)));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        // OPEN_REPARSE_POINT makes CreateFile return a handle to the link or
        // reparse point itself instead of following it. Deliberately omit
        // `truncate(true)`: truncating as part of open would modify a link's
        // target before the opened object can be checked. Once the handle and
        // current path are both known to be ordinary files, set_len performs
        // the requested truncation on that validated handle.
        let mut options = OpenOptions::new();
        options
            .create(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        ensure_regular_file(&file)?;
        ensure_windows_non_reparse_trace_file(path, &file)?;
        lock_trace_file(&file)?;
        file.set_len(0)?;
        return Ok(Box::new(BufWriter::new(file)));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut options = OpenOptions::new();
        options.create(true).write(true);
        let file = options.open(path)?;
        ensure_regular_file(&file)?;
        lock_trace_file(&file)?;
        file.set_len(0)?;
        Ok(Box::new(BufWriter::new(file)))
    }
}

fn lock_trace_file(file: &File) -> std::io::Result<()> {
    match fs2::FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(()),
        Err(error) if trace_lock_is_contended(&error) => Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "trace log is already in use by another process",
        )),
        Err(error) => Err(error),
    }
}

fn trace_lock_is_contended(error: &std::io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
}

#[cfg(windows)]
fn ensure_windows_non_reparse_trace_file(path: &Path, file: &File) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let opened = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || current.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !current.file_type().is_file()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "trace log must not be a symbolic link or reparse point",
        ));
    }
    Ok(())
}

fn ensure_regular_file(file: &File) -> std::io::Result<()> {
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "trace log must be a regular file",
        ));
    }
    Ok(())
}

fn write_json_line(state: &mut TraceState, value: &Value) -> bool {
    let Some(writer) = state.writer.as_mut() else {
        return false;
    };
    let result = (|| -> std::io::Result<()> {
        serde_json::to_writer(&mut **writer, value).map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
        writer.flush()
    })();
    if let Err(error) = result {
        state.log_error = Some(error.to_string());
        state.writer = None;
        return false;
    }
    true
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn disabled_trace_does_not_evaluate_or_write() {
        let trace = TraceLog::default();
        let span = trace.span_with("disabled", || {
            panic!("disabled trace evaluated start fields")
        });
        span.finish_with(TraceOutcome::Ok, || {
            panic!("disabled trace evaluated finish fields")
        });
        trace.finish();
        assert!(!trace.is_enabled());
        assert_eq!(trace.log_error(), None);
    }

    #[test]
    fn trace_records_typed_content_free_spans() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("nested/trace.jsonl");
        let trace = TraceLog::enabled(&path);
        trace
            .span(
                "remote.ssh.exchange",
                TraceFields::new()
                    .opaque("hostId", "secret.example")
                    .duration_ms("timeoutMs", Duration::from_secs(2))
                    .usize("requestBytes", 12),
            )
            .finish(
                TraceOutcome::Ok,
                TraceFields::new().usize("responseBytes", 34),
            );
        trace.finish();

        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("remote.ssh.exchange"));
        assert!(contents.contains("requestBytes"));
        assert!(contents.contains("responseBytes"));
        assert!(!contents.contains("secret.example"));
        assert!(
            contents
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
    }

    #[test]
    fn unfinished_span_is_reported_as_abandoned() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("trace.jsonl");
        let trace = TraceLog::enabled(&path);
        drop(trace.span("work", TraceFields::new()));
        trace.finish();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("\"outcome\":\"abandoned\""));
    }

    #[test]
    fn trace_finish_is_the_terminal_event() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("trace.jsonl");
        let trace = TraceLog::enabled(&path);
        let active = trace.span("active.work", TraceFields::new());
        trace.finish();
        active.finish_with(TraceOutcome::Ok, || {
            panic!("a span completed after trace_finish evaluated its fields")
        });
        trace
            .span("late.work", TraceFields::new())
            .finish(TraceOutcome::Ok, TraceFields::new());

        let contents = fs::read_to_string(path).unwrap();
        let last = contents.lines().last().expect("trace finish record");
        let value: Value = serde_json::from_str(last).unwrap();
        assert_eq!(value["event"], "trace_finish");
        assert!(contents.contains("active.work"));
        assert!(contents.contains("\"outcome\":\"abandoned\""));
        assert!(!contents.contains("late.work"));
    }

    #[test]
    fn active_trace_file_cannot_be_truncated_by_a_second_logger() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("trace.jsonl");
        let first = TraceLog::enabled(&path);
        assert!(first.is_enabled());
        first
            .span("first.work", TraceFields::new())
            .finish(TraceOutcome::Ok, TraceFields::new());

        let refused = TraceLog::enabled(&path);
        assert!(!refused.is_enabled());
        assert!(
            refused
                .log_error()
                .is_some_and(|error| error.contains("already in use"))
        );
        assert!(fs::read_to_string(&path).unwrap().contains("first.work"));

        first.finish();
        let replacement = TraceLog::enabled(&path);
        assert!(replacement.is_enabled());
        replacement.finish();
    }

    #[cfg(unix)]
    #[test]
    fn trace_log_is_private_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempdir().unwrap();
        let path = temp.path().join("trace.jsonl");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let trace = TraceLog::enabled(&path);
        assert!(trace.is_enabled());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        trace.finish();

        let target = temp.path().join("target.jsonl");
        fs::write(&target, b"do not truncate").unwrap();
        let link = temp.path().join("link.jsonl");
        symlink(&target, &link).unwrap();
        let refused = TraceLog::enabled(&link);
        assert!(!refused.is_enabled());
        assert!(refused.log_error().is_some());
        assert_eq!(fs::read(&target).unwrap(), b"do not truncate");
    }

    #[cfg(windows)]
    #[test]
    fn trace_log_rejects_windows_symlinks_without_truncating_the_target() {
        use std::os::windows::fs::symlink_file;

        let temp = tempdir().unwrap();
        let target = temp.path().join("target.jsonl");
        fs::write(&target, b"do not truncate").unwrap();
        let link = temp.path().join("link.jsonl");
        if let Err(error) = symlink_file(&target, &link) {
            // Creating symlinks requires either Developer Mode or the
            // SeCreateSymbolicLink privilege on older Windows runners.
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("could not create Windows trace symlink fixture: {error}");
        }

        let refused = TraceLog::enabled(&link);
        assert!(!refused.is_enabled());
        assert!(refused.log_error().is_some());
        assert_eq!(fs::read(&target).unwrap(), b"do not truncate");
    }
}
