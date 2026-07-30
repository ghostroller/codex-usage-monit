use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};

const PERF_LOG_SCHEMA_VERSION: u32 = 4;
pub const PERF_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Per-stage timings for one refresh. Sibling stages may overlap when account
/// and local collection run in parallel, so these values are not additive.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshStageMetrics {
    pub discover_us: u64,
    pub cache_load_us: u64,
    pub parse_us: u64,
    pub cache_save_us: u64,
    pub reduce_us: u64,
    pub materialize_us: u64,
    pub account_us: u64,
    pub snapshot_derive_us: u64,
    pub window_analysis_us: u64,
    pub sort_us: u64,
    pub apply_us: u64,
}

/// Aggregate, content-free diagnostics for one completed refresh.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshMetrics {
    pub duration_us: u64,
    pub account_refreshed: bool,
    pub changed: bool,
    pub reduced_rebuilt: bool,
    pub discovery_full_scan: bool,
    pub discovery_cache_hit: bool,
    pub discovery_invalidated: bool,
    pub discovery_probed_files: u64,
    pub discovery_probed_dirs: u64,
    pub selected_files: u64,
    pub selected_bytes: u64,
    pub parsed_lines: u64,
    pub cached_events: u64,
    pub foreign_baseline_events: u64,
    pub reparsed_files: u64,
    pub tail_parsed_files: u64,
    pub tail_parsed_bytes: u64,
    pub full_parsed_files: u64,
    pub reused_files: u64,
    pub incrementally_reduced_threads: u64,
    pub full_rebuild: bool,
    pub tasks: u64,
    pub turns: u64,
    pub calls: u64,
    pub stages: RefreshStageMetrics,
}

/// Content-free timing and volume diagnostics for one history persistence pass.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryMetrics {
    pub duration_us: u64,
    pub stage_us: u64,
    pub record_us: u64,
    pub record_performed: bool,
    pub load_us: u64,
    pub load_performed: bool,
    pub shards_written: u64,
    pub shards_skipped: u64,
    pub shards_pruned: u64,
    pub quota_points: u64,
    pub local_buckets: u64,
    pub weekly_local_points: u64,
    pub warnings: u64,
    pub read_only: bool,
}

impl HistoryMetrics {
    pub fn with_durations(duration: Duration, record: Duration, load: Option<Duration>) -> Self {
        Self {
            duration_us: duration_us(duration),
            record_us: duration_us(record),
            load_us: load.map(duration_us).unwrap_or_default(),
            load_performed: load.is_some(),
            ..Self::default()
        }
    }
}

impl RefreshMetrics {
    pub fn with_duration(duration: Duration) -> Self {
        Self {
            duration_us: duration_us(duration),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessSample {
    #[serde(skip_serializing_if = "Option::is_none")]
    resident_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    virtual_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    physical_footprint_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_physical_footprint_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_resident_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_cpu_time_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_cpu_time_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pageins: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    io_read_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    io_written_bytes: Option<u64>,
}

#[derive(Clone, Default)]
pub struct PerfLog {
    inner: Option<Arc<PerfInner>>,
}

struct PerfInner {
    origin: Instant,
    active: AtomicBool,
    next_sample_us: AtomicU64,
    draw_count: AtomicU64,
    draw_total_us: AtomicU64,
    draw_max_us: AtomicU64,
    refresh_count: AtomicU64,
    refresh_total_us: AtomicU64,
    refresh_max_us: AtomicU64,
    history_count: AtomicU64,
    history_total_us: AtomicU64,
    history_max_us: AtomicU64,
    event_wakeups: AtomicU64,
    sample_lock: Mutex<()>,
    state: Mutex<PerfState>,
}

struct PerfState {
    writer: Option<Box<dyn Write + Send>>,
    last_sample_us: u64,
    latest_refresh: Option<RefreshMetrics>,
    log_error: Option<String>,
}

impl fmt::Debug for PerfLog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PerfLog")
            .field("enabled", &self.is_enabled())
            .field("log_error", &self.log_error())
            .finish()
    }
}

impl PerfLog {
    /// Creates or truncates a runtime performance log. Initialization failures
    /// disable logging and remain observable through `log_error`; they never
    /// prevent the application from starting.
    pub fn enabled(path: &Path) -> Self {
        let writer = open_writer(path);
        match writer {
            Ok(writer) => Self::enabled_with_writer(writer),
            Err(error) => Self::disabled_with_error(error.to_string()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.active.load(Ordering::Acquire))
    }

    pub fn log_error(&self) -> Option<String> {
        let inner = self.inner.as_ref()?;
        inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .log_error
            .clone()
    }

    /// Adds one frame to the current 30-second aggregation window.
    pub fn record_draw(&self, duration: Duration) {
        let Some(inner) = self.active_inner() else {
            return;
        };
        let elapsed_us = duration_us(duration);
        saturating_add(&inner.draw_count, 1);
        saturating_add(&inner.draw_total_us, elapsed_us);
        inner.draw_max_us.fetch_max(elapsed_us, Ordering::Relaxed);
        self.maybe_sample_inner(inner);
    }

    /// Appends one content-free refresh record and adds it to the current
    /// 30-second aggregation window. The writer is flushed by the periodic
    /// sample rather than by every refresh.
    pub fn record_refresh(&self, metrics: RefreshMetrics) {
        let Some(inner) = self.active_inner() else {
            return;
        };
        let refresh_duration_us = metrics.duration_us;
        saturating_add(&inner.refresh_count, 1);
        saturating_add(&inner.refresh_total_us, refresh_duration_us);
        inner
            .refresh_max_us
            .fetch_max(refresh_duration_us, Ordering::Relaxed);
        {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.latest_refresh = Some(metrics.clone());
            if !write_json_line(
                &mut state,
                &json!({
                    "schemaVersion": PERF_LOG_SCHEMA_VERSION,
                    "event": "refresh",
                    "at": Utc::now(),
                    "atUs": duration_us(inner.origin.elapsed()),
                    "metrics": metrics,
                }),
            ) && state.log_error.is_some()
            {
                inner.active.store(false, Ordering::Release);
                return;
            }
        }
        self.maybe_sample_inner(inner);
    }

    /// Appends one history record/load event without retaining paths or usage values.
    pub fn record_history(&self, metrics: HistoryMetrics) {
        let Some(inner) = self.active_inner() else {
            return;
        };
        record_history_runtime_inner(inner, metrics.duration_us);
        {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !write_json_line(
                &mut state,
                &json!({
                    "schemaVersion": PERF_LOG_SCHEMA_VERSION,
                    "event": "history",
                    "at": Utc::now(),
                    "atUs": duration_us(inner.origin.elapsed()),
                    "metrics": metrics,
                }),
            ) && state.log_error.is_some()
            {
                inner.active.store(false, Ordering::Release);
                return;
            }
        }
        self.maybe_sample_inner(inner);
    }

    /// Adds a history stage/flush/load pass to the aggregate sample without
    /// writing a per-pass JSON event.
    pub fn record_history_runtime(&self, duration: Duration) {
        let Some(inner) = self.active_inner() else {
            return;
        };
        record_history_runtime_inner(inner, duration_us(duration));
        self.maybe_sample_inner(inner);
    }

    pub fn record_event_wakeup(&self) {
        let Some(inner) = self.active_inner() else {
            return;
        };
        saturating_add(&inner.event_wakeups, 1);
        self.maybe_sample_inner(inner);
    }

    /// Emits a sample once the fixed sampling interval has elapsed.
    pub fn maybe_sample(&self) {
        let Some(inner) = self.active_inner() else {
            return;
        };
        self.maybe_sample_inner(inner);
    }

    /// Immediately emits one aggregate sample. Primarily useful for orderly
    /// shutdown and tests; normal runtime logging should use `maybe_sample`.
    pub fn sample_now(&self) {
        let Some(inner) = self.active_inner() else {
            return;
        };
        let at_us = duration_us(inner.origin.elapsed());
        self.write_sample(inner, at_us);
        inner.next_sample_us.store(
            at_us.saturating_add(duration_us(PERF_SAMPLE_INTERVAL)),
            Ordering::Release,
        );
    }

    /// Flushes the final partial window and writes `perf_stop`. Safe to call
    /// repeatedly and concurrently with late worker completion.
    pub fn finish(&self) {
        let Some(inner) = self.inner.as_deref() else {
            return;
        };
        if !inner.active.swap(false, Ordering::AcqRel) {
            return;
        }
        let at_us = duration_us(inner.origin.elapsed());
        self.write_sample(inner, at_us);
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if write_json_line(
            &mut state,
            &json!({
                "schemaVersion": PERF_LOG_SCHEMA_VERSION,
                "event": "perf_stop",
                "at": Utc::now(),
                "atUs": at_us,
            }),
        ) && let Some(writer) = state.writer.as_mut()
            && let Err(error) = writer.flush()
        {
            disable_after_error(inner, &mut state, error.to_string());
        }
        state.writer = None;
    }

    fn active_inner(&self) -> Option<&PerfInner> {
        self.inner
            .as_deref()
            .filter(|inner| inner.active.load(Ordering::Acquire))
    }

    fn enabled_with_writer(mut writer: Box<dyn Write + Send>) -> Self {
        let origin = Instant::now();
        let started_at = Utc::now();
        let start = json!({
            "schemaVersion": PERF_LOG_SCHEMA_VERSION,
            "event": "perf_start",
            "startedAt": started_at,
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "sampleIntervalSeconds": PERF_SAMPLE_INTERVAL.as_secs(),
        });
        let start_result = (|| -> std::io::Result<()> {
            serde_json::to_writer(&mut *writer, &start).map_err(std::io::Error::other)?;
            writer.write_all(b"\n")?;
            writer.flush()
        })();
        match start_result {
            Ok(()) => Self {
                inner: Some(Arc::new(PerfInner {
                    origin,
                    active: AtomicBool::new(true),
                    next_sample_us: AtomicU64::new(duration_us(PERF_SAMPLE_INTERVAL)),
                    draw_count: AtomicU64::new(0),
                    draw_total_us: AtomicU64::new(0),
                    draw_max_us: AtomicU64::new(0),
                    refresh_count: AtomicU64::new(0),
                    refresh_total_us: AtomicU64::new(0),
                    refresh_max_us: AtomicU64::new(0),
                    history_count: AtomicU64::new(0),
                    history_total_us: AtomicU64::new(0),
                    history_max_us: AtomicU64::new(0),
                    event_wakeups: AtomicU64::new(0),
                    sample_lock: Mutex::new(()),
                    state: Mutex::new(PerfState {
                        writer: Some(writer),
                        last_sample_us: 0,
                        latest_refresh: None,
                        log_error: None,
                    }),
                })),
            },
            Err(error) => Self::disabled_with_error(error.to_string()),
        }
    }

    fn disabled_with_error(error: String) -> Self {
        Self {
            inner: Some(Arc::new(PerfInner {
                origin: Instant::now(),
                active: AtomicBool::new(false),
                next_sample_us: AtomicU64::new(u64::MAX),
                draw_count: AtomicU64::new(0),
                draw_total_us: AtomicU64::new(0),
                draw_max_us: AtomicU64::new(0),
                refresh_count: AtomicU64::new(0),
                refresh_total_us: AtomicU64::new(0),
                refresh_max_us: AtomicU64::new(0),
                history_count: AtomicU64::new(0),
                history_total_us: AtomicU64::new(0),
                history_max_us: AtomicU64::new(0),
                event_wakeups: AtomicU64::new(0),
                sample_lock: Mutex::new(()),
                state: Mutex::new(PerfState {
                    writer: None,
                    last_sample_us: 0,
                    latest_refresh: None,
                    log_error: Some(error),
                }),
            })),
        }
    }

    fn maybe_sample_inner(&self, inner: &PerfInner) {
        let at_us = duration_us(inner.origin.elapsed());
        let next = inner.next_sample_us.load(Ordering::Acquire);
        if at_us < next
            || inner
                .next_sample_us
                .compare_exchange(
                    next,
                    at_us.saturating_add(duration_us(PERF_SAMPLE_INTERVAL)),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        {
            return;
        }
        self.write_sample(inner, at_us);
    }

    fn write_sample(&self, inner: &PerfInner, at_us: u64) {
        let _sample_guard = inner
            .sample_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let at_us = at_us.max(duration_us(inner.origin.elapsed()));
        let draw_count = inner.draw_count.swap(0, Ordering::AcqRel);
        let draw_total_us = inner.draw_total_us.swap(0, Ordering::AcqRel);
        let draw_max_us = inner.draw_max_us.swap(0, Ordering::AcqRel);
        let refresh_count = inner.refresh_count.swap(0, Ordering::AcqRel);
        let refresh_total_us = inner.refresh_total_us.swap(0, Ordering::AcqRel);
        let refresh_max_us = inner.refresh_max_us.swap(0, Ordering::AcqRel);
        let history_count = inner.history_count.swap(0, Ordering::AcqRel);
        let history_total_us = inner.history_total_us.swap(0, Ordering::AcqRel);
        let history_max_us = inner.history_max_us.swap(0, Ordering::AcqRel);
        let event_wakeups = inner.event_wakeups.swap(0, Ordering::AcqRel);
        let process = process_sample();

        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let window_duration_us = at_us.saturating_sub(state.last_sample_us);
        state.last_sample_us = at_us;
        let value = json!({
            "schemaVersion": PERF_LOG_SCHEMA_VERSION,
            "event": "sample",
            "at": Utc::now(),
            "atUs": at_us,
            "windowDurationUs": window_duration_us,
            "draw": {
                "count": draw_count,
                "totalDurationUs": draw_total_us,
                "maxDurationUs": draw_max_us,
            },
            "refresh": {
                "count": refresh_count,
                "totalDurationUs": refresh_total_us,
                "maxDurationUs": refresh_max_us,
                "latest": state.latest_refresh,
            },
            "history": {
                "count": history_count,
                "totalDurationUs": history_total_us,
                "maxDurationUs": history_max_us,
            },
            "eventWakeups": event_wakeups,
            "process": process,
        });
        if !write_json_line(&mut state, &value) {
            if state.log_error.is_some() {
                inner.active.store(false, Ordering::Release);
            }
            return;
        }
        if let Some(writer) = state.writer.as_mut()
            && let Err(error) = writer.flush()
        {
            disable_after_error(inner, &mut state, error.to_string());
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
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(Box::new(BufWriter::new(options.open(path)?)))
}

fn write_json_line(state: &mut PerfState, value: &Value) -> bool {
    let Some(writer) = state.writer.as_mut() else {
        return false;
    };
    let result = (|| -> std::io::Result<()> {
        serde_json::to_writer(&mut **writer, value).map_err(std::io::Error::other)?;
        writer.write_all(b"\n")
    })();
    if let Err(error) = result {
        state.log_error = Some(error.to_string());
        state.writer = None;
        return false;
    }
    true
}

fn disable_after_error(inner: &PerfInner, state: &mut PerfState, error: String) {
    state.log_error = Some(error);
    state.writer = None;
    inner.active.store(false, Ordering::Release);
}

fn saturating_add(target: &AtomicU64, value: u64) {
    let _ = target.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(value))
    });
}

fn record_history_runtime_inner(inner: &PerfInner, duration_us: u64) {
    saturating_add(&inner.history_count, 1);
    saturating_add(&inner.history_total_us, duration_us);
    inner
        .history_max_us
        .fetch_max(duration_us, Ordering::Relaxed);
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(target_os = "macos")]
fn process_sample() -> ProcessSample {
    // SAFETY: `info` is correctly sized for RUSAGE_INFO_V4 and lives for the
    // duration of the call. proc_pid_rusage only writes to this buffer.
    let mut info = unsafe { std::mem::zeroed::<libc::rusage_info_v4>() };
    // SAFETY: See above. libc models Apple's opaque rusage buffer as a pointer
    // to `rusage_info_t`, so the concrete struct pointer must be cast here.
    let status = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>(),
        )
    };
    if status != 0 {
        return ProcessSample::default();
    }
    let (user_cpu_time_ns, system_cpu_time_ns) = getrusage_cpu_times();
    ProcessSample {
        resident_bytes: Some(info.ri_resident_size),
        physical_footprint_bytes: Some(info.ri_phys_footprint),
        peak_physical_footprint_bytes: Some(info.ri_lifetime_max_phys_footprint),
        user_cpu_time_ns,
        system_cpu_time_ns,
        pageins: Some(info.ri_pageins),
        io_read_bytes: Some(info.ri_diskio_bytesread),
        io_written_bytes: Some(info.ri_diskio_byteswritten),
        ..ProcessSample::default()
    }
}

#[cfg(target_os = "linux")]
fn process_sample() -> ProcessSample {
    let mut sample = ProcessSample::default();
    if let Ok(statm) = fs::read_to_string("/proc/self/statm") {
        let mut fields = statm.split_whitespace();
        let virtual_pages = fields.next().and_then(|value| value.parse::<u64>().ok());
        let resident_pages = fields.next().and_then(|value| value.parse::<u64>().ok());
        // SAFETY: sysconf has no pointer arguments or side effects on memory.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size > 0 {
            let page_size = page_size as u64;
            sample.virtual_bytes = virtual_pages.map(|pages| pages.saturating_mul(page_size));
            sample.resident_bytes = resident_pages.map(|pages| pages.saturating_mul(page_size));
        }
    }

    // SAFETY: `usage` is a valid writable rusage structure for RUSAGE_SELF.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    // SAFETY: See above; getrusage writes exactly one initialized structure.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
        sample.peak_resident_bytes = u64::try_from(usage.ru_maxrss)
            .ok()
            .map(|kilobytes| kilobytes.saturating_mul(1024));
        sample.user_cpu_time_ns = timeval_ns(usage.ru_utime);
        sample.system_cpu_time_ns = timeval_ns(usage.ru_stime);
    }
    sample
}

#[cfg(windows)]
fn process_sample() -> ProcessSample {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS,
    };

    // SAFETY: GetCurrentProcess returns a valid pseudo-handle for the calling process.
    let process = unsafe { GetCurrentProcess() };
    let mut sample = ProcessSample::default();

    let mut memory = PROCESS_MEMORY_COUNTERS {
        cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(u32::MAX),
        ..PROCESS_MEMORY_COUNTERS::default()
    };
    // SAFETY: memory points to a writable structure whose byte size is supplied in `cb`.
    if unsafe { GetProcessMemoryInfo(process, &mut memory, memory.cb) } != 0 {
        sample.resident_bytes = u64::try_from(memory.WorkingSetSize).ok();
        sample.peak_resident_bytes = u64::try_from(memory.PeakWorkingSetSize).ok();
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all FILETIME pointers refer to initialized writable values.
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } != 0 {
        sample.user_cpu_time_ns = Some(filetime_nanoseconds(user));
        sample.system_cpu_time_ns = Some(filetime_nanoseconds(kernel));
    }

    let mut io = IO_COUNTERS::default();
    // SAFETY: io points to a writable IO_COUNTERS structure.
    if unsafe { GetProcessIoCounters(process, &mut io) } != 0 {
        sample.io_read_bytes = Some(io.ReadTransferCount);
        sample.io_written_bytes = Some(io.WriteTransferCount);
    }

    sample
}

#[cfg(windows)]
fn filetime_nanoseconds(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32 | u64::from(value.dwLowDateTime)).saturating_mul(100)
}

#[cfg(target_os = "macos")]
fn getrusage_cpu_times() -> (Option<u64>, Option<u64>) {
    // SAFETY: `usage` is a valid writable rusage structure for RUSAGE_SELF.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    // SAFETY: See above; getrusage writes exactly one initialized structure.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return (None, None);
    }
    (timeval_ns(usage.ru_utime), timeval_ns(usage.ru_stime))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn timeval_ns(value: libc::timeval) -> Option<u64> {
    let seconds = u64::try_from(value.tv_sec).ok()?;
    let microseconds = u64::try_from(value.tv_usec).ok()?;
    Some(
        seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(microseconds.saturating_mul(1_000)),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn process_sample() -> ProcessSample {
    ProcessSample::default()
}

#[cfg(test)]
mod tests {
    use std::io;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn disabled_log_is_a_no_op() {
        let log = PerfLog::default();
        log.record_draw(Duration::from_millis(2));
        log.record_refresh(RefreshMetrics::with_duration(Duration::from_millis(7)));
        log.record_history(HistoryMetrics::with_durations(
            Duration::from_millis(3),
            Duration::from_millis(2),
            Some(Duration::from_millis(1)),
        ));
        log.record_history_runtime(Duration::from_micros(500));
        log.record_event_wakeup();
        log.sample_now();
        log.finish();

        assert!(!log.is_enabled());
        assert!(log.log_error().is_none());
    }

    #[test]
    fn writes_aggregate_samples_without_retaining_individual_frames() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested/perf.jsonl");
        let log = PerfLog::enabled(&path);
        assert!(log.is_enabled(), "{:?}", log.log_error());

        log.record_draw(Duration::from_micros(700));
        log.record_draw(Duration::from_micros(300));
        let mut refresh = RefreshMetrics::with_duration(Duration::from_millis(9));
        refresh.changed = true;
        refresh.discovery_cache_hit = true;
        refresh.discovery_probed_files = 7;
        refresh.discovery_probed_dirs = 3;
        refresh.cached_events = 42;
        refresh.foreign_baseline_events = 3;
        log.record_refresh(refresh);
        let mut history = HistoryMetrics::with_durations(
            Duration::from_millis(3),
            Duration::from_millis(2),
            Some(Duration::from_millis(1)),
        );
        history.record_performed = true;
        history.stage_us = 250;
        history.shards_written = 1;
        history.weekly_local_points = 12;
        history.local_buckets = 24;
        log.record_history(history);
        log.record_history_runtime(Duration::from_micros(500));
        log.record_event_wakeup();
        log.sample_now();
        log.finish();

        let records = fs::read_to_string(path).unwrap();
        let values = records
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values[0]["event"], "perf_start");
        assert!(
            values
                .iter()
                .all(|value| value["schemaVersion"] == PERF_LOG_SCHEMA_VERSION)
        );
        assert_eq!(values[1]["event"], "refresh");
        assert_eq!(values[1]["metrics"]["discoveryCacheHit"], true);
        assert_eq!(values[1]["metrics"]["discoveryProbedFiles"], 7);
        assert_eq!(values[1]["metrics"]["discoveryProbedDirs"], 3);
        assert_eq!(values[1]["metrics"]["cachedEvents"], 42);
        assert_eq!(values[1]["metrics"]["foreignBaselineEvents"], 3);
        assert_eq!(values[2]["event"], "history");
        assert_eq!(values[2]["metrics"]["stageUs"], 250);
        assert_eq!(values[2]["metrics"]["recordUs"], 2_000);
        assert_eq!(values[2]["metrics"]["recordPerformed"], true);
        assert_eq!(values[2]["metrics"]["loadUs"], 1_000);
        assert_eq!(values[2]["metrics"]["loadPerformed"], true);
        assert_eq!(values[2]["metrics"]["shardsWritten"], 1);
        assert_eq!(values[2]["metrics"]["weeklyLocalPoints"], 12);
        assert_eq!(values[2]["metrics"]["localBuckets"], 24);
        assert!(values[2]["metrics"].get("halfHourBuckets").is_none());
        assert_eq!(values[3]["event"], "sample");
        assert_eq!(values[3]["draw"]["count"], 2);
        assert_eq!(values[3]["draw"]["totalDurationUs"], 1_000);
        assert_eq!(values[3]["draw"]["maxDurationUs"], 700);
        assert_eq!(values[3]["refresh"]["count"], 1);
        assert_eq!(values[3]["refresh"]["latest"]["cachedEvents"], 42);
        assert_eq!(values[3]["history"]["count"], 2);
        assert_eq!(values[3]["history"]["totalDurationUs"], 3_500);
        assert_eq!(values[3]["history"]["maxDurationUs"], 3_000);
        assert_eq!(values[3]["eventWakeups"], 1);
        assert_eq!(values.last().unwrap()["event"], "perf_stop");
    }

    #[test]
    fn writer_failure_disables_logging_without_propagating() {
        struct FailOnFlush {
            flushes: usize,
        }

        impl Write for FailOnFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                if self.flushes > 1 {
                    Err(io::Error::other("simulated full disk"))
                } else {
                    Ok(())
                }
            }
        }

        let log = PerfLog::enabled_with_writer(Box::new(FailOnFlush { flushes: 0 }));
        assert!(log.is_enabled());
        log.sample_now();

        assert!(!log.is_enabled());
        assert_eq!(log.log_error().as_deref(), Some("simulated full disk"));
        log.record_draw(Duration::from_secs(1));
        log.finish();
    }

    #[test]
    fn process_sample_serializes_generic_io_field_names() {
        let value = serde_json::to_value(ProcessSample {
            io_read_bytes: Some(123),
            io_written_bytes: Some(456),
            ..ProcessSample::default()
        })
        .unwrap();

        assert_eq!(value["ioReadBytes"], 123);
        assert_eq!(value["ioWrittenBytes"], 456);
        assert!(value.get("diskReadBytes").is_none());
        assert!(value.get("diskWrittenBytes").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_process_sample_reports_current_memory() {
        let sample = process_sample();
        assert!(sample.resident_bytes.is_some_and(|bytes| bytes > 0));
        assert!(
            sample
                .physical_footprint_bytes
                .is_some_and(|bytes| bytes > 0)
        );
        assert!(
            sample
                .peak_physical_footprint_bytes
                .is_some_and(|bytes| bytes > 0)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_sample_reports_current_resources() {
        let sample = process_sample();
        assert!(sample.resident_bytes.is_some_and(|bytes| bytes > 0));
        assert!(
            sample
                .peak_resident_bytes
                .zip(sample.resident_bytes)
                .is_some_and(|(peak, current)| peak >= current)
        );
        assert!(sample.user_cpu_time_ns.is_some());
        assert!(sample.system_cpu_time_ns.is_some());
        assert!(sample.io_read_bytes.is_some());
        assert!(sample.io_written_bytes.is_some());
    }
}
