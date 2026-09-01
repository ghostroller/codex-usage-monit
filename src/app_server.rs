use std::collections::BTreeMap;
#[cfg(windows)]
use std::env;
use std::ffi::OsStr;
use std::io::{self, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
#[cfg(windows)]
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

use crate::bounded_io::{BoundedLine, read_bounded_line};
use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, AccountTokenUsage, CreditsSnapshot, DailyTokenBucket, LimitBucket,
    LimitWindow, Provenance, RateLimitResetCredit, RateLimitResetCreditsSnapshot,
};
use crate::startup::StartupTrace;
use crate::trace::{TraceFields, TraceLog, TraceOutcome};

const INITIALIZE_ID: u64 = 1;
const RATE_LIMITS_ID: u64 = 2;
const ACCOUNT_USAGE_ID: u64 = 3;
const STDERR_LIMIT: usize = 32 * 1024;
const STDERR_DIAGNOSTIC_LIMIT: usize = 2 * 1024;
const RPC_ERROR_MESSAGE_LIMIT: usize = 512;
// Account quota payloads are normally tiny; leave generous headroom for
// future usage detail without allowing an unbounded JSON value or queue.
const APP_SERVER_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const APP_SERVER_READER_QUEUE: usize = 2;
const APP_SERVER_MAX_PROTOCOL_WARNINGS: usize = 32;
const APP_SERVER_MAX_RESET_CREDIT_DETAILS: usize = 4_096;
const APP_SERVER_READER_JOIN_GRACE: Duration = Duration::from_millis(250);
// `account/usage/read` is optional analytics metadata. Recent Codex versions
// can take many seconds to answer it even after fresh quota/reset data is
// available. Give an already-in-flight response a small chance to arrive, but
// never let it hold the account snapshot until the primary RPC deadline.
const APP_SERVER_OPTIONAL_USAGE_GRACE: Duration = Duration::from_millis(100);
#[cfg(windows)]
const DESKTOP_CLI_RESOURCE_DIAGNOSTIC: &str = "Codex Desktop packaged resource";
#[cfg(windows)]
const DESKTOP_PACKAGE_PREFIX: &str = "OpenAI.Codex_";

enum ReaderEvent {
    Message(Value),
    Malformed(String),
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderTask {
    Stdout,
    Stderr,
}

#[derive(Default)]
struct ProtocolWarnings {
    retained: Vec<String>,
    suppressed: usize,
}

#[derive(Debug)]
struct AppServerResponseTimeout;

impl std::fmt::Display for AppServerResponseTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("codex app-server response timed out")
    }
}

impl std::error::Error for AppServerResponseTimeout {}

#[derive(Debug)]
struct AppServerReaderDisconnected;

impl std::fmt::Display for AppServerReaderDisconnected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("codex app-server stdout reader disconnected")
    }
}

impl std::error::Error for AppServerReaderDisconnected {}

fn app_server_trace_outcome(error: &anyhow::Error) -> TraceOutcome {
    if error.is::<AppServerResponseTimeout>() {
        TraceOutcome::Timeout
    } else {
        TraceOutcome::Error
    }
}

fn app_server_trace_error_kind(error: &anyhow::Error, fallback: &'static str) -> &'static str {
    if error.is::<AppServerResponseTimeout>() {
        "timeout"
    } else if error.is::<AppServerReaderDisconnected>() {
        "disconnected"
    } else {
        fallback
    }
}

impl ProtocolWarnings {
    fn push(&mut self, warning: String) {
        if self.retained.len() < APP_SERVER_MAX_PROTOCOL_WARNINGS {
            self.retained.push(warning);
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }

    fn len(&self) -> usize {
        self.retained.len()
            + usize::from(
                self.suppressed > 0 && self.retained.len() < APP_SERVER_MAX_PROTOCOL_WARNINGS,
            )
    }

    fn into_messages(mut self) -> Vec<String> {
        if self.suppressed > 0 {
            if self.retained.len() == APP_SERVER_MAX_PROTOCOL_WARNINGS {
                self.retained.pop();
                self.suppressed = self.suppressed.saturating_add(1);
            }
            self.retained.push(format!(
                "suppressed {} additional codex app-server protocol warnings",
                self.suppressed
            ));
        }
        self.retained
    }
}

struct ChildGuard {
    child: Child,
    process_tree: ProcessTree,
    startup_trace: StartupTrace,
    trace_log: TraceLog,
    reaped: bool,
}

impl ChildGuard {
    fn terminate_and_reap(&mut self) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }

        // Own the spans here rather than in `Drop`: normal callers explicitly
        // reap before reader-thread cleanup, so a Drop-owned span only measured
        // a later no-op. This boundary now includes process-group/Job
        // termination and the blocking wait which actually reaps the child.
        let shutdown_span = self.startup_trace.span("app_server.shutdown");
        let shutdown_trace = self
            .trace_log
            .span_with("app_server.process.shutdown", TraceFields::new);
        let terminate_error = self.process_tree.terminate(&mut self.child).err();
        let wait_error = self.child.wait().map(|_| ()).err();
        self.reaped = true;

        let error_kind = shutdown_error_kind(terminate_error.is_some(), wait_error.is_some());
        if let Some(error_kind) = error_kind {
            shutdown_span.finish(format!("status=error kind={error_kind}"));
            shutdown_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new()
                    .label("errorKind", error_kind)
                    .bool("terminateFailed", terminate_error.is_some())
                    .bool("waitFailed", wait_error.is_some())
            });
            Err(combine_shutdown_errors(terminate_error, wait_error))
        } else {
            shutdown_span.finish("status=reaped");
            shutdown_trace.finish_with(TraceOutcome::Ok, TraceFields::new);
            Ok(())
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.terminate_and_reap();
    }
}

fn shutdown_error_kind(terminate_failed: bool, wait_failed: bool) -> Option<&'static str> {
    match (terminate_failed, wait_failed) {
        (false, false) => None,
        (true, false) => Some("terminate"),
        (false, true) => Some("wait"),
        (true, true) => Some("terminate_and_wait"),
    }
}

fn combine_shutdown_errors(
    terminate_error: Option<io::Error>,
    wait_error: Option<io::Error>,
) -> io::Error {
    match (terminate_error, wait_error) {
        (Some(terminate), Some(wait)) => io::Error::new(
            terminate.kind(),
            format!("app-server termination failed: {terminate}; child wait failed: {wait}"),
        ),
        (Some(error), None) => error,
        (None, Some(error)) => error,
        (None, None) => io::Error::other("app-server cleanup failed without an error"),
    }
}

#[cfg(unix)]
struct ProcessTree {
    process_group: libc::pid_t,
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command) {
    command.process_group(0);
}

#[cfg(unix)]
fn attach_process_tree(child: &mut Child) -> io::Result<ProcessTree> {
    Ok(ProcessTree {
        process_group: child.id() as libc::pid_t,
    })
}

#[cfg(unix)]
impl ProcessTree {
    fn terminate(&mut self, child: &mut Child) -> io::Result<()> {
        // Every ordinary wrapper and descendant inherits this group. Killing
        // it also closes stdout/stderr copies held outside the direct child.
        let killed_group = unsafe { libc::kill(-self.process_group, libc::SIGKILL) } == 0;
        if killed_group {
            return Ok(());
        }

        let group_error = io::Error::last_os_error();
        let child_running = child.try_wait()?.is_none();
        if child_running {
            child.kill()?;
        }
        // ESRCH plus an already-exited direct child means the process group no
        // longer exists. Other group failures remain observable even when the
        // direct-child fallback succeeded because descendants may have escaped
        // cleanup.
        if group_error.raw_os_error() == Some(libc::ESRCH) && !child_running {
            Ok(())
        } else {
            Err(group_error)
        }
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: HANDLE,
}

#[cfg(windows)]
fn configure_process_tree(command: &mut Command) {
    command.creation_flags(CREATE_SUSPENDED);
}

#[cfg(windows)]
fn attach_process_tree(child: &mut Child) -> io::Result<ProcessTree> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(error);
    }

    let assigned = unsafe { AssignProcessToJobObject(job, child.as_raw_handle().cast()) };
    if assigned == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(error);
    }
    let process_tree = ProcessTree { job };
    resume_suspended_child(child)?;
    Ok(process_tree)
}

#[cfg(windows)]
fn resume_suspended_child(child: &Child) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        let mut has_entry = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        let mut resumed_threads = 0_usize;
        while has_entry {
            if entry.th32OwnerProcessID == child.id() {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let resume_result = unsafe { ResumeThread(thread) };
                unsafe {
                    CloseHandle(thread);
                }
                if resume_result == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed_threads = resumed_threads.saturating_add(1);
            }
            has_entry = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        if resumed_threads == 0 {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "could not find the suspended app-server primary thread",
            ))
        } else {
            Ok(())
        }
    })();
    unsafe {
        CloseHandle(snapshot);
    }
    result
}

#[cfg(windows)]
impl ProcessTree {
    fn terminate(&mut self, child: &mut Child) -> io::Result<()> {
        let mut errors = Vec::new();
        let mut job_terminated = false;
        if !self.job.is_null() {
            // Close is also a kill-on-close fallback if explicit termination
            // races with process shutdown, but retain both API failures so the
            // trace does not report a false successful cleanup.
            if unsafe { TerminateJobObject(self.job, 1) } == 0 {
                errors.push(("job_terminate", io::Error::last_os_error()));
            } else {
                job_terminated = true;
            }
            if unsafe { CloseHandle(self.job) } == 0 {
                errors.push(("job_close", io::Error::last_os_error()));
            } else {
                self.job = std::ptr::null_mut();
            }
        }
        if !job_terminated {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = child.kill() {
                        errors.push(("child_kill", error));
                    }
                }
                Err(error) => errors.push(("child_status", error)),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            let kind = errors[0].1.kind();
            let detail = errors
                .into_iter()
                .map(|(stage, error)| format!("{stage}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            Err(io::Error::new(kind, detail))
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if !self.job.is_null() {
            unsafe {
                CloseHandle(self.job);
            }
        }
    }
}

/// Fetches the read-only account quota and token-activity snapshot from Codex.
///
/// Authentication remains owned by `codex app-server`; this adapter does not read
/// `auth.json` and does not call any write or control methods.
pub fn fetch_account_snapshot(config: &CollectConfig) -> Result<AccountSnapshot> {
    fetch_account_snapshot_inner(config, None)
}

/// Collects account data while assigning the caller's logical snapshot time.
///
/// Snapshot orchestration starts local and remote collection from one boundary;
/// keeping that boundary avoids manufacturing local zero-coverage while a slow
/// App Server request is still in flight. Standalone callers use the public
/// function above and retain response-completion timestamps.
pub(crate) fn fetch_account_snapshot_as_of(
    config: &CollectConfig,
    as_of: DateTime<Utc>,
) -> Result<AccountSnapshot> {
    fetch_account_snapshot_inner(config, Some(as_of))
}

fn fetch_account_snapshot_inner(
    config: &CollectConfig,
    snapshot_as_of: Option<DateTime<Utc>>,
) -> Result<AccountSnapshot> {
    let operation_trace = config
        .trace_log
        .span_with("app_server.account_snapshot", || {
            TraceFields::new()
                .bool("offline", config.offline)
                .duration_ms("timeoutMs", config.app_server_timeout)
                .label(
                    "executableSource",
                    if config.codex_bin.is_some() {
                        "explicit"
                    } else {
                        "discovered"
                    },
                )
        });
    let total_span = config.startup_trace.span("app_server.total");
    if config.offline {
        total_span.finish("status=offline");
        operation_trace.finish_with(TraceOutcome::Skipped, || {
            TraceFields::new().label("reason", "offline")
        });
        return Ok(AccountSnapshot {
            warnings: vec!["Codex app-server collection is disabled in offline mode".to_string()],
            ..AccountSnapshot::default()
        });
    }

    let spawn_span = config.startup_trace.span("app_server.spawn");
    let process_trace = config.trace_log.span_with("app_server.process.spawn", || {
        TraceFields::new().label("operation", "account_read")
    });
    let mut command = match codex_command(config).context("failed to resolve a runnable Codex CLI")
    {
        Ok(command) => command,
        Err(error) => {
            spawn_span.finish("status=error kind=resolve");
            process_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", "resolve")
            });
            total_span.finish("status=error kind=resolve");
            operation_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", "resolve")
            });
            return Err(error);
        }
    };
    if let Some(path) = config.app_server_path.as_deref() {
        command.env("PATH", path);
    }
    configure_process_tree(&mut command);
    let program = command.get_program().to_owned();
    let spawn_result = command
        .arg("app-server")
        .env("CODEX_HOME", &config.codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| app_server_spawn_error(&program, error))
        .context("failed to spawn `codex app-server`");
    let mut spawned_child = match spawn_result {
        Ok(child) => {
            process_trace.finish_with(TraceOutcome::Ok, TraceFields::new);
            child
        }
        Err(error) => {
            process_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", "spawn")
            });
            spawn_span.finish("status=error kind=spawn");
            total_span.finish("status=error kind=spawn");
            operation_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", "spawn")
            });
            return Err(error);
        }
    };

    let containment_trace = config
        .trace_log
        .span_with("app_server.process.attach", TraceFields::new);
    let process_tree = match attach_process_tree(&mut spawned_child) {
        Ok(process_tree) => {
            containment_trace.finish_with(TraceOutcome::Ok, TraceFields::new);
            process_tree
        }
        Err(source) => {
            let _ = spawned_child.kill();
            let _ = spawned_child.wait();
            containment_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", "process_isolation")
            });
            spawn_span.finish("status=error kind=process_isolation");
            total_span.finish("status=error kind=process_isolation");
            operation_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", "process_isolation")
            });
            return Err(source).context("failed to contain the codex app-server process tree");
        }
    };
    spawn_span.finish("command=codex_app_server");

    let io_span = config.startup_trace.span("app_server.io_setup");
    let io_trace = config
        .trace_log
        .span_with("app_server.process.io_setup", || {
            TraceFields::new().duration_ms("timeoutMs", config.app_server_timeout)
        });
    let mut child = ChildGuard {
        child: spawned_child,
        process_tree,
        startup_trace: config.startup_trace.clone(),
        trace_log: config.trace_log.clone(),
        reaped: false,
    };
    let io_setup = (|| {
        let stdin = child
            .child
            .stdin
            .take()
            .context("codex app-server did not expose stdin")
            .map_err(|error| ("stdin_pipe", error))?;
        let stdout = child
            .child
            .stdout
            .take()
            .context("codex app-server did not expose stdout")
            .map_err(|error| ("stdout_pipe", error))?;
        let stderr = child
            .child
            .stderr
            .take()
            .context("codex app-server did not expose stderr")
            .map_err(|error| ("stderr_pipe", error))?;
        let deadline = Instant::now()
            .checked_add(config.app_server_timeout)
            .context("app-server timeout exceeds the platform's supported range")
            .map_err(|error| ("deadline", error))?;
        Ok((stdin, stdout, stderr, deadline))
    })();
    let (stdin, stdout, stderr, deadline) = match io_setup {
        Ok(setup) => {
            io_span.finish_with(|| {
                format!(
                    "status=ok timeout_ms={}",
                    config.app_server_timeout.as_millis()
                )
            });
            io_trace.finish_with(TraceOutcome::Ok, TraceFields::new);
            setup
        }
        Err((error_kind, error)) => {
            io_span.finish(format!("status=error kind={error_kind}"));
            io_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", error_kind)
            });
            let cleanup_failed = child.terminate_and_reap().is_err();
            total_span.finish(format!("status=error kind={error_kind}"));
            operation_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new()
                    .label("errorKind", error_kind)
                    .bool("cleanupFailed", cleanup_failed)
            });
            return Err(error);
        }
    };

    let (reader_tx, reader_rx) = mpsc::sync_channel(APP_SERVER_READER_QUEUE);
    let (reader_done_tx, reader_done_rx) = mpsc::channel();
    let stdout_done_tx = reader_done_tx.clone();
    let stdout_reader = thread::spawn(move || {
        read_stdout(stdout, reader_tx);
        let _ = stdout_done_tx.send(ReaderTask::Stdout);
    });

    let stderr_output = Arc::new(Mutex::new(String::new()));
    let stderr_writer = Arc::clone(&stderr_output);
    let stderr_reader = thread::spawn(move || {
        capture_stderr(stderr, stderr_writer);
        let _ = reader_done_tx.send(ReaderTask::Stderr);
    });
    let mut stdin = stdin;
    let mut protocol_warnings = ProtocolWarnings::default();

    let result = (|| {
        let initialize_span = config.startup_trace.span("app_server.initialize");
        let initialize_trace = config.trace_log.span_with("app_server.rpc.initialize", || {
            TraceFields::new().duration_ms("timeoutMs", config.app_server_timeout)
        });
        let initialize_result = (|| -> Result<()> {
            write_message(
                &mut stdin,
                &json!({
                    "method": "initialize",
                    "id": INITIALIZE_ID,
                    "params": {
                        "clientInfo": {
                            "name": "codex-usage-monit",
                            "title": "Codex Usage Monitor",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "capabilities": null
                    }
                }),
            )
            .context("failed to initialize codex app-server")?;

            match wait_for_response(
                INITIALIZE_ID,
                "initialize",
                deadline,
                &reader_rx,
                &mut protocol_warnings,
                &stderr_output,
            )? {
                Ok(_) => Ok(()),
                Err(message) => bail!("codex app-server initialize failed: {message}"),
            }
        })();
        if let Err(error) = initialize_result {
            let outcome = app_server_trace_outcome(&error);
            let error_kind = app_server_trace_error_kind(&error, "rpc");
            initialize_span.finish(format!("status=error kind={error_kind}"));
            initialize_trace.finish_with(outcome, || {
                TraceFields::new().label("errorKind", error_kind)
            });
            return Err(error);
        }
        initialize_span.finish("status=ok");
        initialize_trace.finish_with(TraceOutcome::Ok, TraceFields::new);

        let account_span = config.startup_trace.span("app_server.account_reads");
        let account_trace = config
            .trace_log
            .span_with("app_server.rpc.account_reads", || {
                TraceFields::new()
                    .usize("requestCount", 1)
                    .duration_ms("timeoutMs", config.app_server_timeout)
            });
        let request_result = (|| -> Result<()> {
            write_message(&mut stdin, &json!({ "method": "initialized" }))?;
            write_message(
                &mut stdin,
                &json!({ "method": "account/rateLimits/read", "id": RATE_LIMITS_ID }),
            )?;
            Ok(())
        })();
        if let Err(error) = request_result {
            account_span.finish("status=error kind=write");
            account_trace.finish_with(TraceOutcome::Error, || {
                TraceFields::new().label("errorKind", "write")
            });
            return Err(error);
        }

        let rate_limits;
        let rate_limits_rpc_category;
        loop {
            let event =
                match recv_event(deadline, &reader_rx, "account rate limits", &stderr_output) {
                    Ok(event) => event,
                    Err(error) => {
                        let outcome = app_server_trace_outcome(&error);
                        let error_kind = app_server_trace_error_kind(&error, "receive");
                        account_span.finish(format!("status=error kind={error_kind}"));
                        account_trace.finish_with(outcome, || {
                            TraceFields::new().label("errorKind", error_kind)
                        });
                        return Err(error);
                    }
                };
            match event {
                ReaderEvent::Message(message) => {
                    if response_id(&message) == Some(RATE_LIMITS_ID) {
                        rate_limits_rpc_category = rpc_error_category(&message);
                        rate_limits = response_payload(&message);
                        break;
                    }
                }
                ReaderEvent::Malformed(message) => {
                    push_protocol_warning(&mut protocol_warnings, message);
                }
                ReaderEvent::Eof => {
                    account_span.finish("status=error kind=eof");
                    account_trace.finish_with(TraceOutcome::Error, || {
                        TraceFields::new().label("errorKind", "eof")
                    });
                    bail!(
                        "codex app-server closed stdout before returning the account snapshot{}",
                        stderr_suffix(&stderr_output)
                    );
                }
            }
        }
        let rate_limits_rpc_error = rate_limits.is_err();
        let account_reads_partial = protocol_warnings.len() > 0 || rate_limits_rpc_error;
        let account_usage =
            fetch_optional_account_usage(config, &mut stdin, deadline, &reader_rx, &stderr_output);
        account_span.finish_with(|| {
            format!(
                "status={} rate_limits={} usage={} warnings={}",
                if account_reads_partial {
                    "partial"
                } else {
                    "ok"
                },
                true,
                account_usage.is_some(),
                protocol_warnings.len()
            )
        });
        account_trace.finish_with(
            if account_reads_partial {
                TraceOutcome::Partial
            } else {
                TraceOutcome::Ok
            },
            || {
                let fields = TraceFields::new()
                    .bool("rateLimits", true)
                    .bool("usage", account_usage.is_some())
                    .bool("rateLimitsRpcError", rate_limits_rpc_error)
                    .usize("protocolWarnings", protocol_warnings.len());
                if let Some(category) = rate_limits_rpc_category {
                    fields.label("rateLimitsRpcCategory", category)
                } else {
                    fields
                }
            },
        );

        let parse_span = config.startup_trace.span("app_server.parse_responses");
        let mut snapshot = AccountSnapshot::default();
        match rate_limits {
            Ok(result) => {
                let as_of = snapshot_as_of.unwrap_or_else(Utc::now);
                match parse_rate_limits_result(&result, as_of) {
                    Ok(limits) => snapshot.limits = limits,
                    Err(error) => snapshot.errors.push(format!(
                        "account/rateLimits/read returned invalid data: {error:#}"
                    )),
                }
                match parse_rate_limit_reset_credits_result_lossy(
                    &result,
                    as_of,
                    &mut protocol_warnings,
                ) {
                    Ok((reset_credits, partial)) => {
                        snapshot.rate_limit_reset_credits = reset_credits;
                        snapshot.rate_limit_reset_credits_partial = partial;
                    }
                    Err(error) => {
                        snapshot.rate_limit_reset_credits_partial = true;
                        push_protocol_warning(
                            &mut protocol_warnings,
                            format!(
                                "account/rateLimits/read returned invalid reset credits: {error:#}"
                            ),
                        );
                    }
                }
            }
            Err(message) => snapshot
                .errors
                .push(format!("account/rateLimits/read failed: {message}")),
        }
        snapshot.usage = account_usage;

        parse_span.finish_with(|| {
            format!(
                "buckets={} reset_credits={} usage={} warnings={} errors={}",
                snapshot.limits.len(),
                snapshot.rate_limit_reset_credits.is_some(),
                snapshot.usage.is_some(),
                protocol_warnings.len(),
                snapshot.errors.len()
            )
        });
        snapshot.warnings = protocol_warnings.into_messages();

        Ok(snapshot)
    })();

    drop(stdin);
    let cleanup_result = child.terminate_and_reap();
    drop(reader_rx);
    finish_reader_threads(stdout_reader, stderr_reader, reader_done_rx);
    drop(child);
    let snapshot_partial = result.as_ref().is_ok_and(|snapshot| {
        !snapshot.warnings.is_empty()
            || !snapshot.errors.is_empty()
            || snapshot.rate_limit_reset_credits_partial
    });
    let operation_outcome = match &result {
        Err(error) => app_server_trace_outcome(error),
        Ok(_) if snapshot_partial || cleanup_result.is_err() => TraceOutcome::Partial,
        Ok(_) => TraceOutcome::Ok,
    };
    let status = match operation_outcome {
        TraceOutcome::Ok => "ok",
        TraceOutcome::Partial => "partial",
        TraceOutcome::Timeout => "timeout",
        _ => "error",
    };
    total_span.finish_with(|| format!("status={status}"));
    operation_trace.finish_with(operation_outcome, || {
        let mut warning_count = 0;
        let mut error_count = 0;
        let mut reset_credits_partial = false;
        if let Ok(snapshot) = &result {
            warning_count = snapshot.warnings.len();
            error_count = snapshot.errors.len();
            reset_credits_partial = snapshot.rate_limit_reset_credits_partial;
        }
        let fields = TraceFields::new()
            .bool("cleanupFailed", cleanup_result.is_err())
            .bool("snapshotPartial", snapshot_partial)
            .usize("warningCount", warning_count)
            .usize("errorCount", error_count)
            .bool("resetCreditsPartial", reset_credits_partial);
        if let Err(error) = &result {
            fields.label("errorKind", app_server_trace_error_kind(error, "operation"))
        } else {
            fields
        }
    });
    result
}

fn finish_reader_threads(
    stdout_reader: thread::JoinHandle<()>,
    stderr_reader: thread::JoinHandle<()>,
    reader_done_rx: mpsc::Receiver<ReaderTask>,
) {
    let deadline = Instant::now()
        .checked_add(APP_SERVER_READER_JOIN_GRACE)
        .unwrap_or_else(Instant::now);
    let mut stdout_done = false;
    let mut stderr_done = false;
    while !stdout_done || !stderr_done {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match reader_done_rx.recv_timeout(remaining) {
            Ok(ReaderTask::Stdout) => stdout_done = true,
            Ok(ReaderTask::Stderr) => stderr_done = true,
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Joining only threads that announced completion keeps ordinary shutdown
    // deterministic. A custom wrapper can deliberately move a pipe-owning
    // descendant outside our process group; detaching that blocked reader is
    // safer than allowing an external process to defeat the RPC timeout.
    if stdout_done {
        let _ = stdout_reader.join();
    }
    if stderr_done {
        let _ = stderr_reader.join();
    }
}

fn codex_command(config: &CollectConfig) -> Result<Command> {
    if let Some(codex_bin) = config.codex_bin.as_deref() {
        return Ok(Command::new(codex_bin));
    }
    #[cfg(not(windows))]
    {
        Ok(Command::new("codex"))
    }
    #[cfg(windows)]
    {
        let path = config
            .app_server_path
            .clone()
            .or_else(|| env::var_os("PATH"))
            .unwrap_or_default();
        let monitor_cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        resolve_automatic_windows_codex_cli(&path, &monitor_cwd).map(Command::new)
    }
}

#[cfg(windows)]
pub(crate) fn installed_windows_codex_cli() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os("LOCALAPPDATA")?)
        .join("OpenAI")
        .join("Codex")
        .join("bin")
        .join("codex.exe");
    path.is_file().then_some(path)
}

#[cfg(windows)]
pub(crate) fn resolve_automatic_windows_codex_cli(
    path: &OsStr,
    monitor_cwd: &Path,
) -> Result<PathBuf> {
    resolve_automatic_windows_codex_cli_with_installed(
        path,
        monitor_cwd,
        installed_windows_codex_cli(),
    )
}

#[cfg(windows)]
pub(crate) fn resolve_automatic_windows_codex_cli_with_installed(
    path: &OsStr,
    monitor_cwd: &Path,
    installed: Option<PathBuf>,
) -> Result<PathBuf> {
    match crate::session_launch::resolve_executable("codex", None, path, monitor_cwd) {
        Ok(discovered) if !is_desktop_codex_resource(&discovered) => Ok(discovered),
        Ok(packaged) => {
            if let Some(installed) = installed {
                return Ok(installed);
            }
            crate::session_launch::resolve_executable_matching(
                "codex",
                path,
                monitor_cwd,
                |candidate| !is_desktop_codex_resource(candidate),
            )
            .map_err(|_| anyhow!(desktop_cli_resource_message(&packaged)))
        }
        Err(error) => installed.ok_or_else(|| anyhow::Error::new(error)),
    }
}

#[cfg(windows)]
fn desktop_cli_resource_message(path: &Path) -> String {
    format!(
        "found the {DESKTOP_CLI_RESOURCE_DIAGNOSTIC} at {}; Windows cannot launch it as a standalone Codex CLI. Install and sign in to the Codex CLI, use --codex-bin to select a runnable `codex.cmd` or `codex.exe`, or use --offline to monitor local rollout data",
        path.display()
    )
}

fn app_server_spawn_error(_program: &OsStr, error: io::Error) -> anyhow::Error {
    #[cfg(windows)]
    {
        let path = Path::new(_program);
        if error.kind() == io::ErrorKind::PermissionDenied
            && error.raw_os_error() == Some(5)
            && is_desktop_codex_resource(path)
        {
            return anyhow!("{}: {error}", desktop_cli_resource_message(path));
        }
    }

    error.into()
}

#[cfg(windows)]
fn is_desktop_codex_resource(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    if !path_component_equals(file_name, "codex") && !path_component_equals(file_name, "codex.exe")
    {
        return false;
    }

    let Some(resources) = path.parent() else {
        return false;
    };
    let Some(app) = resources.parent() else {
        return false;
    };
    let Some(package) = app.parent() else {
        return false;
    };
    let Some(windows_apps) = package.parent() else {
        return false;
    };

    path_component_equals(resources.file_name().unwrap_or_default(), "resources")
        && path_component_equals(app.file_name().unwrap_or_default(), "app")
        && package
            .file_name()
            .map(|name| name.to_string_lossy())
            .and_then(|name| name.get(..DESKTOP_PACKAGE_PREFIX.len()).map(str::to_owned))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(DESKTOP_PACKAGE_PREFIX))
        && path_component_equals(windows_apps.file_name().unwrap_or_default(), "WindowsApps")
}

#[cfg(windows)]
fn path_component_equals(value: &OsStr, expected: &str) -> bool {
    value.to_string_lossy().eq_ignore_ascii_case(expected)
}

/// Parses the `result` object returned by `account/rateLimits/read`.
pub fn parse_rate_limits_result(result: &Value, as_of: DateTime<Utc>) -> Result<Vec<LimitBucket>> {
    let result = unwrap_result(result);
    let object = result
        .as_object()
        .context("rate-limit result must be an object")?;

    let mut snapshots = BTreeMap::<String, &Value>::new();
    match object.get("rateLimitsByLimitId") {
        Some(Value::Object(by_id)) => {
            for (limit_id, snapshot) in by_id {
                if !snapshot.is_null() {
                    snapshots.insert(limit_id.clone(), snapshot);
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => bail!("rateLimitsByLimitId must be an object or null"),
    }

    if snapshots.is_empty() {
        if let Some(snapshot) = object.get("rateLimits").filter(|value| !value.is_null()) {
            let key = optional_string(
                snapshot
                    .as_object()
                    .context("rateLimits must be an object")?,
                "limitId",
            )?
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".to_string());
            snapshots.insert(key, snapshot);
        } else if object.contains_key("primary") || object.contains_key("secondary") {
            let key = optional_string(object, "limitId")?
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "default".to_string());
            snapshots.insert(key, result);
        }
    }

    if snapshots.is_empty() {
        bail!("rate-limit result did not contain any limit snapshots");
    }

    snapshots
        .into_iter()
        .map(|(fallback_id, snapshot)| parse_limit_bucket(snapshot, &fallback_id, as_of))
        .collect()
}

/// Parses the reset opportunities returned alongside `account/rateLimits/read`.
pub fn parse_rate_limit_reset_credits_result(
    result: &Value,
    as_of: DateTime<Utc>,
) -> Result<Option<RateLimitResetCreditsSnapshot>> {
    let Some(reset_credits) = rate_limit_reset_credits_object(result)? else {
        return Ok(None);
    };
    let available_count = required_u64(reset_credits, "availableCount")
        .context("rateLimitResetCredits.availableCount is invalid")?;
    if available_count > i64::MAX as u64 {
        bail!("rateLimitResetCredits.availableCount exceeds the protocol int64 range");
    }
    let credits = optional_reset_credits(reset_credits)?;

    Ok(Some(RateLimitResetCreditsSnapshot {
        available_count,
        credits,
        provenance: Provenance::ServerSnapshot,
        as_of,
    }))
}

fn parse_rate_limit_reset_credits_result_lossy(
    result: &Value,
    as_of: DateTime<Utc>,
    warnings: &mut ProtocolWarnings,
) -> Result<(Option<RateLimitResetCreditsSnapshot>, bool)> {
    let Some(reset_credits) = rate_limit_reset_credits_object(result)? else {
        return Ok((None, false));
    };
    let available_count = required_u64(reset_credits, "availableCount")
        .context("rateLimitResetCredits.availableCount is invalid")?;
    if available_count > i64::MAX as u64 {
        bail!("rateLimitResetCredits.availableCount exceeds the protocol int64 range");
    }

    let mut partial = false;
    let credits = match optional_reset_credit_values(reset_credits) {
        Ok(None) => None,
        Ok(Some(values)) => {
            let retained = values.len().min(APP_SERVER_MAX_RESET_CREDIT_DETAILS);
            let mut credits = Vec::with_capacity(retained);
            for (index, value) in values.iter().take(retained).enumerate() {
                match parse_reset_credit(value, index) {
                    Ok(credit) => credits.push(credit),
                    Err(error) => {
                        partial = true;
                        push_protocol_warning(
                            warnings,
                            format!(
                                "account/rateLimits/read ignored invalid reset credit detail: {error:#}"
                            ),
                        );
                    }
                }
            }
            if values.len() > retained {
                partial = true;
                push_protocol_warning(
                    warnings,
                    format!(
                        "account/rateLimits/read ignored {} reset credit details exceeding the {APP_SERVER_MAX_RESET_CREDIT_DETAILS}-detail limit",
                        values.len() - retained
                    ),
                );
            }
            Some(credits)
        }
        Err(error) => {
            partial = true;
            push_protocol_warning(
                warnings,
                format!("account/rateLimits/read ignored invalid reset credit detail: {error:#}"),
            );
            None
        }
    };

    Ok((
        Some(RateLimitResetCreditsSnapshot {
            available_count,
            credits,
            provenance: Provenance::ServerSnapshot,
            as_of,
        }),
        partial,
    ))
}

fn rate_limit_reset_credits_object(result: &Value) -> Result<Option<&Map<String, Value>>> {
    let result = unwrap_result(result);
    let object = result
        .as_object()
        .context("rate-limit result must be an object")?;
    let Some(value) = object.get("rateLimitResetCredits") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_object()
        .map(Some)
        .context("rateLimitResetCredits must be an object or null")
}

/// Parses the `result` object returned by `account/usage/read`.
pub fn parse_account_usage_result(result: &Value) -> Result<AccountTokenUsage> {
    let result = unwrap_result(result);
    let object = result
        .as_object()
        .context("account-usage result must be an object")?;
    let summary = object
        .get("summary")
        .and_then(Value::as_object)
        .context("account-usage result is missing summary")?;

    let daily_usage_buckets = match object.get("dailyUsageBuckets") {
        Some(Value::Array(buckets)) => buckets
            .iter()
            .enumerate()
            .map(|(index, bucket)| {
                let bucket = bucket
                    .as_object()
                    .with_context(|| format!("dailyUsageBuckets[{index}] must be an object"))?;
                Ok(DailyTokenBucket {
                    start_date: required_string(bucket, "startDate").with_context(|| {
                        format!("dailyUsageBuckets[{index}].startDate is invalid")
                    })?,
                    tokens: required_u64(bucket, "tokens")
                        .with_context(|| format!("dailyUsageBuckets[{index}].tokens is invalid"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => bail!("dailyUsageBuckets must be an array or null"),
    };

    Ok(AccountTokenUsage {
        lifetime_tokens: optional_u64(summary, "lifetimeTokens")?,
        peak_daily_tokens: optional_u64(summary, "peakDailyTokens")?,
        longest_running_turn_sec: optional_u64(summary, "longestRunningTurnSec")?,
        current_streak_days: optional_u64(summary, "currentStreakDays")?,
        longest_streak_days: optional_u64(summary, "longestStreakDays")?,
        daily_usage_buckets,
    })
}

fn parse_limit_bucket(
    value: &Value,
    fallback_id: &str,
    as_of: DateTime<Utc>,
) -> Result<LimitBucket> {
    let object = value
        .as_object()
        .context("rate-limit snapshot must be an object")?;
    let limit_id = optional_string(object, "limitId")?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_id.to_string());

    Ok(LimitBucket {
        limit_id,
        limit_name: optional_string(object, "limitName")?,
        plan_type: optional_string(object, "planType")?,
        primary: optional_window(object, "primary")?,
        secondary: optional_window(object, "secondary")?,
        credits: optional_credits(object)?,
        rate_limit_reached_type: optional_string(object, "rateLimitReachedType")?,
        provenance: Provenance::ServerSnapshot,
        as_of,
    })
}

fn optional_window(object: &Map<String, Value>, key: &str) -> Result<Option<LimitWindow>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let window = value
        .as_object()
        .with_context(|| format!("{key} must be an object or null"))?;
    let used_percent = required_f64(window, "usedPercent")
        .with_context(|| format!("{key}.usedPercent is invalid"))?;
    if !used_percent.is_finite() {
        bail!("{key}.usedPercent must be finite");
    }

    Ok(Some(LimitWindow::new(
        used_percent,
        optional_i64(window, "windowDurationMins")?,
        optional_timestamp(window, "resetsAt")?,
    )))
}

fn optional_credits(object: &Map<String, Value>) -> Result<Option<CreditsSnapshot>> {
    let Some(value) = object.get("credits") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let credits = value
        .as_object()
        .context("credits must be an object or null")?;
    Ok(Some(CreditsSnapshot {
        has_credits: required_bool(credits, "hasCredits")?,
        unlimited: required_bool(credits, "unlimited")?,
        balance: optional_scalar_string(credits, "balance")?,
    }))
}

fn optional_reset_credits(
    object: &Map<String, Value>,
) -> Result<Option<Vec<RateLimitResetCredit>>> {
    let Some(credits) = optional_reset_credit_values(object)? else {
        return Ok(None);
    };
    if credits.len() > APP_SERVER_MAX_RESET_CREDIT_DETAILS {
        bail!(
            "rateLimitResetCredits.credits contains {} entries, exceeding the {APP_SERVER_MAX_RESET_CREDIT_DETAILS}-detail limit",
            credits.len()
        );
    }
    credits
        .iter()
        .enumerate()
        .map(|(index, credit)| parse_reset_credit(credit, index))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn optional_reset_credit_values(object: &Map<String, Value>) -> Result<Option<&[Value]>> {
    let Some(value) = object.get("credits") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let credits = value
        .as_array()
        .context("rateLimitResetCredits.credits must be an array or null")?;
    Ok(Some(credits))
}

fn parse_reset_credit(value: &Value, index: usize) -> Result<RateLimitResetCredit> {
    let path = format!("rateLimitResetCredits.credits[{index}]");
    let object = value
        .as_object()
        .with_context(|| format!("{path} must be an object"))?;

    match object.get("id") {
        Some(Value::String(_)) => {}
        _ => bail!("{path}.id is required and must be a string"),
    }

    let granted_at = optional_unix_seconds_timestamp(object, "grantedAt")
        .with_context(|| format!("{path}.grantedAt is invalid"))?
        .with_context(|| format!("{path}.grantedAt is required"))?;
    let expires_at = optional_unix_seconds_timestamp(object, "expiresAt")
        .with_context(|| format!("{path}.expiresAt is invalid"))?;
    let status =
        required_string(object, "status").with_context(|| format!("{path}.status is invalid"))?;
    let reset_type = required_string(object, "resetType")
        .with_context(|| format!("{path}.resetType is invalid"))?;

    Ok(RateLimitResetCredit {
        granted_at,
        expires_at,
        status,
        reset_type,
        title: optional_string(object, "title")
            .with_context(|| format!("{path}.title is invalid"))?,
        description: optional_string(object, "description")
            .with_context(|| format!("{path}.description is invalid"))?,
    })
}

fn optional_unix_seconds_timestamp(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<DateTime<Utc>>> {
    match object.get(key) {
        Some(Value::Number(value)) => {
            let seconds = value
                .as_i64()
                .ok_or_else(|| anyhow!("{key} must be Unix seconds as an int64"))?;
            DateTime::from_timestamp(seconds, 0)
                .map(Some)
                .ok_or_else(|| anyhow!("{key} is outside the supported timestamp range"))
        }
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be Unix seconds as an integer or null"),
    }
}

fn unwrap_result(value: &Value) -> &Value {
    value.get("result").unwrap_or(value)
}

fn response_id(message: &Value) -> Option<u64> {
    if message.get("method").is_some() {
        return None;
    }
    message.get("id").and_then(Value::as_u64)
}

fn response_payload(message: &Value) -> std::result::Result<Value, String> {
    if let Some(error) = message.get("error") {
        return Err(format_rpc_error(error));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| "response contained neither result nor error".to_string())
}

fn rpc_error_category(message: &Value) -> Option<&'static str> {
    let error = message.get("error")?;
    let Some(code) = error.get("code").and_then(Value::as_i64) else {
        return Some("uncategorized");
    };
    Some(match code {
        -32700 => "parse_error",
        -32600 => "invalid_request",
        -32601 => "method_not_found",
        -32602 => "invalid_params",
        -32603 => "internal_error",
        -32099..=-32000 => "server_error",
        _ => "application_error",
    })
}

fn shorter_deadline(overall_deadline: Instant, grace: Duration) -> Instant {
    Instant::now()
        .checked_add(grace)
        .map_or(overall_deadline, |grace_deadline| {
            overall_deadline.min(grace_deadline)
        })
}

fn fetch_optional_account_usage(
    config: &CollectConfig,
    stdin: &mut impl Write,
    overall_deadline: Instant,
    receiver: &mpsc::Receiver<ReaderEvent>,
    stderr: &Arc<Mutex<String>>,
) -> Option<AccountTokenUsage> {
    let trace = config
        .trace_log
        .span_with("app_server.rpc.optional_usage", || {
            TraceFields::new().duration_ms("graceMs", APP_SERVER_OPTIONAL_USAGE_GRACE)
        });
    if write_message(
        stdin,
        &json!({ "method": "account/usage/read", "id": ACCOUNT_USAGE_ID }),
    )
    .is_err()
    {
        trace.finish_with(TraceOutcome::Partial, || {
            TraceFields::new().label("partialReason", "write")
        });
        return None;
    }

    let deadline = shorter_deadline(overall_deadline, APP_SERVER_OPTIONAL_USAGE_GRACE);
    let mut malformed_frames = 0_usize;
    let mut ignored_messages = 0_usize;
    loop {
        let event = match recv_event(deadline, receiver, "optional account usage", stderr) {
            Ok(event) => event,
            Err(error) => {
                let error_kind = app_server_trace_error_kind(&error, "receive");
                trace.finish_with(app_server_trace_outcome(&error), || {
                    TraceFields::new()
                        .label("partialReason", error_kind)
                        .usize("malformedFrames", malformed_frames)
                        .usize("ignoredMessages", ignored_messages)
                });
                return None;
            }
        };
        match event {
            ReaderEvent::Message(message) if response_id(&message) == Some(ACCOUNT_USAGE_ID) => {
                let rpc_category = rpc_error_category(&message);
                return match response_payload(&message) {
                    Ok(result) => match parse_account_usage_result(&result) {
                        Ok(usage) => {
                            trace.finish_with(TraceOutcome::Ok, || {
                                TraceFields::new()
                                    .bool("response", true)
                                    .usize("malformedFrames", malformed_frames)
                                    .usize("ignoredMessages", ignored_messages)
                            });
                            Some(usage)
                        }
                        Err(_) => {
                            trace.finish_with(TraceOutcome::Partial, || {
                                TraceFields::new()
                                    .label("partialReason", "invalid_payload")
                                    .usize("malformedFrames", malformed_frames)
                                    .usize("ignoredMessages", ignored_messages)
                            });
                            None
                        }
                    },
                    Err(message) => {
                        let unsupported = optional_usage_rpc_unavailable(&message);
                        trace.finish_with(
                            if unsupported {
                                TraceOutcome::Skipped
                            } else {
                                TraceOutcome::Partial
                            },
                            || {
                                let fields = TraceFields::new()
                                    .label(
                                        "partialReason",
                                        if unsupported {
                                            "unsupported"
                                        } else {
                                            "rpc_error"
                                        },
                                    )
                                    .bool("unsupported", unsupported)
                                    .usize("malformedFrames", malformed_frames)
                                    .usize("ignoredMessages", ignored_messages);
                                if let Some(category) = rpc_category {
                                    fields.label("rpcCategory", category)
                                } else {
                                    fields
                                }
                            },
                        );
                        None
                    }
                };
            }
            ReaderEvent::Message(_) => {
                ignored_messages = ignored_messages.saturating_add(1);
            }
            ReaderEvent::Malformed(_) => {
                malformed_frames = malformed_frames.saturating_add(1);
            }
            ReaderEvent::Eof => {
                trace.finish_with(TraceOutcome::Partial, || {
                    TraceFields::new()
                        .label("partialReason", "eof")
                        .usize("malformedFrames", malformed_frames)
                        .usize("ignoredMessages", ignored_messages)
                });
                return None;
            }
        }
    }
}

fn format_rpc_error(error: &Value) -> String {
    let Some(object) = error.as_object() else {
        return compact_diagnostic_text(&error.to_string(), RPC_ERROR_MESSAGE_LIMIT);
    };
    let message = compact_diagnostic_text(
        object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error"),
        RPC_ERROR_MESSAGE_LIMIT,
    );
    match object.get("code") {
        Some(code) => format!("{message} (code {code})"),
        None => message,
    }
}

fn optional_usage_rpc_unavailable(message: &str) -> bool {
    message.contains("(code -32601)")
        || (message.contains("(code -32600)")
            && message.contains("account/usage/read")
            && message.to_ascii_lowercase().contains("unknown variant"))
}

fn write_message(stdin: &mut impl Write, message: &Value) -> Result<()> {
    serde_json::to_writer(&mut *stdin, message).context("failed to encode app-server request")?;
    stdin
        .write_all(b"\n")
        .context("failed to write app-server request")?;
    stdin.flush().context("failed to flush app-server request")
}

fn wait_for_response(
    id: u64,
    operation: &str,
    deadline: Instant,
    receiver: &mpsc::Receiver<ReaderEvent>,
    warnings: &mut ProtocolWarnings,
    stderr: &Arc<Mutex<String>>,
) -> Result<std::result::Result<Value, String>> {
    loop {
        match recv_event(deadline, receiver, operation, stderr)? {
            ReaderEvent::Message(message) if response_id(&message) == Some(id) => {
                return Ok(response_payload(&message));
            }
            ReaderEvent::Message(_) => {}
            ReaderEvent::Malformed(message) => push_protocol_warning(warnings, message),
            ReaderEvent::Eof => {
                bail!(
                    "codex app-server closed stdout while waiting for {operation}{}",
                    stderr_suffix(stderr)
                );
            }
        }
    }
}

fn recv_event(
    deadline: Instant,
    receiver: &mpsc::Receiver<ReaderEvent>,
    operation: &str,
    stderr: &Arc<Mutex<String>>,
) -> Result<ReaderEvent> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| app_server_timeout_error(operation, stderr))?;
    receiver
        .recv_timeout(remaining)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => app_server_timeout_error(operation, stderr),
            mpsc::RecvTimeoutError::Disconnected => anyhow::Error::new(AppServerReaderDisconnected)
                .context(format!(
                    "codex app-server stdout reader stopped while waiting for {operation}{}",
                    stderr_suffix(stderr)
                )),
        })
}

fn app_server_timeout_error(operation: &str, stderr: &Arc<Mutex<String>>) -> anyhow::Error {
    anyhow::Error::new(AppServerResponseTimeout).context(format!(
        "timed out waiting for codex app-server {operation}{}",
        stderr_suffix(stderr)
    ))
}

fn push_protocol_warning(warnings: &mut ProtocolWarnings, warning: String) {
    warnings.push(warning);
}

fn read_stdout(stdout: impl Read, sender: mpsc::SyncSender<ReaderEvent>) {
    read_stdout_with_limit(stdout, sender, APP_SERVER_MAX_FRAME_BYTES);
}

fn read_stdout_with_limit(
    stdout: impl Read,
    sender: mpsc::SyncSender<ReaderEvent>,
    max_frame_bytes: usize,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        match read_bounded_line(&mut reader, &mut line, max_frame_bytes) {
            Ok(BoundedLine::Eof) => {
                let _ = sender.send(ReaderEvent::Eof);
                break;
            }
            Ok(BoundedLine::TooLong) => {
                if sender
                    .send(ReaderEvent::Malformed(format!(
                        "ignored oversized codex app-server output frame exceeding the {max_frame_bytes}-byte limit"
                    )))
                    .is_err()
                {
                    break;
                }
            }
            Ok(BoundedLine::Line) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let event = match serde_json::from_slice(&line) {
                    Ok(value) => ReaderEvent::Message(value),
                    Err(error) => ReaderEvent::Malformed(format!(
                        "ignored malformed codex app-server output: {error}"
                    )),
                };
                if sender.send(event).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(ReaderEvent::Malformed(format!(
                    "failed to read codex app-server stdout: {error}"
                )));
                let _ = sender.send(ReaderEvent::Eof);
                break;
            }
        }
    }
}

fn capture_stderr(mut stderr: impl Read, output: Arc<Mutex<String>>) {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let available = STDERR_LIMIT.saturating_sub(captured.len());
                captured.extend_from_slice(&buffer[..count.min(available)]);
                if let Ok(mut destination) = output.lock() {
                    *destination = compact_diagnostic_text(
                        &String::from_utf8_lossy(&captured),
                        STDERR_DIAGNOSTIC_LIMIT,
                    );
                }
            }
        }
    }
}

fn compact_diagnostic_text(value: &str, max_chars: usize) -> String {
    let stripped = strip_ansi_sequences(value);
    let normalized = stripped
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut compact = normalized
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    compact.push_str("...");
    compact
}

fn strip_ansi_sequences(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            if !character.is_control() || matches!(character, '\n' | '\t') {
                output.push(character);
            }
            continue;
        }

        match chars.next() {
            Some('[') => {
                for character in chars.by_ref() {
                    if ('@'..='~').contains(&character) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut previous_escape = false;
                for character in chars.by_ref() {
                    if character == '\u{7}' || (previous_escape && character == '\\') {
                        break;
                    }
                    previous_escape = character == '\u{1b}';
                }
            }
            Some(_) | None => {}
        }
    }
    output
}

fn stderr_suffix(stderr: &Arc<Mutex<String>>) -> String {
    let output = stderr
        .lock()
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if output.is_empty() {
        String::new()
    } else {
        format!(": {output}")
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be a string or null"),
    }
}

fn optional_scalar_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Number(value)) => Ok(Some(value.to_string())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be a string, number, or null"),
    }
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String> {
    optional_string(object, key)?.ok_or_else(|| anyhow!("{key} is required"))
}

fn required_bool(object: &Map<String, Value>, key: &str) -> Result<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("{key} must be a boolean"))
}

fn required_f64(object: &Map<String, Value>, key: &str) -> Result<f64> {
    match object.get(key) {
        Some(Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| anyhow!("{key} is outside the supported numeric range")),
        Some(Value::String(value)) => value
            .parse::<f64>()
            .with_context(|| format!("{key} must be numeric")),
        _ => bail!("{key} is required and must be numeric"),
    }
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match object.get(key) {
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be a non-negative integer")),
        Some(Value::String(value)) => value
            .parse::<u64>()
            .map(Some)
            .with_context(|| format!("{key} must be a non-negative integer")),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be a non-negative integer or null"),
    }
}

fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64> {
    optional_u64(object, key)?.ok_or_else(|| anyhow!("{key} is required"))
}

fn optional_i64(object: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match object.get(key) {
        Some(Value::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be an integer")),
        Some(Value::String(value)) => value
            .parse::<i64>()
            .map(Some)
            .with_context(|| format!("{key} must be an integer")),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be an integer or null"),
    }
}

fn optional_timestamp(object: &Map<String, Value>, key: &str) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    if let Some(value) = value.as_str() {
        if let Ok(seconds) = value.parse::<i64>() {
            return timestamp_from_integer(seconds, key).map(Some);
        }
        return DateTime::parse_from_rfc3339(value)
            .map(|timestamp| Some(timestamp.with_timezone(&Utc)))
            .with_context(|| format!("{key} must be Unix seconds or an RFC 3339 timestamp"));
    }
    let seconds = value
        .as_i64()
        .ok_or_else(|| anyhow!("{key} must be Unix seconds or null"))?;
    timestamp_from_integer(seconds, key).map(Some)
}

fn timestamp_from_integer(value: i64, key: &str) -> Result<DateTime<Utc>> {
    let (seconds, nanos) = if value.unsigned_abs() >= 1_000_000_000_000 {
        (
            value.div_euclid(1_000),
            value.rem_euclid(1_000) as u32 * 1_000_000,
        )
    } else {
        (value, 0)
    };
    DateTime::from_timestamp(seconds, nanos)
        .ok_or_else(|| anyhow!("{key} is outside the supported timestamp range"))
}

#[cfg(test)]
mod diagnostic_tests {
    use std::fs;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, mpsc};
    #[cfg(unix)]
    use std::time::Duration;
    use std::time::Instant;

    use chrono::Utc;
    use serde_json::{Value, json};

    use super::{
        APP_SERVER_MAX_PROTOCOL_WARNINGS, APP_SERVER_MAX_RESET_CREDIT_DETAILS, ProtocolWarnings,
        RPC_ERROR_MESSAGE_LIMIT, ReaderEvent, app_server_trace_outcome, codex_command,
        compact_diagnostic_text, fetch_account_snapshot, format_rpc_error,
        optional_usage_rpc_unavailable, parse_rate_limit_reset_credits_result,
        parse_rate_limit_reset_credits_result_lossy, read_stdout_with_limit, recv_event,
        rpc_error_category, shutdown_error_kind,
    };
    use crate::config::CollectConfig;
    use crate::trace::{TraceLog, TraceOutcome};

    fn read_trace_events(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_trace_finish<'a>(
        events: &'a [Value],
        stage: &str,
        outcome: &str,
        error_kind: Option<&str>,
    ) -> &'a Value {
        let event = events
            .iter()
            .find(|event| {
                event["event"] == "span_finish"
                    && event["stage"] == stage
                    && event["outcome"] == outcome
            })
            .unwrap_or_else(|| panic!("missing {outcome} trace finish for {stage}: {events:#?}"));
        if let Some(error_kind) = error_kind {
            assert_eq!(event["fields"]["errorKind"], error_kind);
        }
        event
    }

    #[test]
    fn explicit_codex_bin_is_passed_directly_to_the_runtime_command() {
        let codex_bin = PathBuf::from("relative tools/custom-codex.cmd");
        let config = CollectConfig {
            codex_bin: Some(codex_bin.clone()),
            ..CollectConfig::default()
        };

        let command = codex_command(&config).unwrap();

        assert_eq!(command.get_program(), codex_bin.as_os_str());
    }

    #[test]
    fn spawn_setup_failure_has_explicit_trace_outcomes_without_sensitive_paths() {
        let temp = tempfile::tempdir().unwrap();
        let trace_path = temp.path().join("trace.jsonl");
        let missing = temp.path().join("private/missing-codex");
        let trace = TraceLog::enabled(&trace_path);
        let config = CollectConfig {
            codex_bin: Some(missing.clone()),
            trace_log: trace.clone(),
            ..CollectConfig::default()
        };

        assert!(fetch_account_snapshot(&config).is_err());
        trace.finish();

        let contents = fs::read_to_string(&trace_path).unwrap();
        assert!(!contents.contains(&missing.to_string_lossy().into_owned()));
        assert!(!contents.contains("\"outcome\":\"abandoned\""));
        let events = read_trace_events(&trace_path);
        assert_trace_finish(&events, "app_server.process.spawn", "error", Some("spawn"));
        assert_trace_finish(
            &events,
            "app_server.account_snapshot",
            "error",
            Some("spawn"),
        );
    }

    #[test]
    fn shutdown_error_categories_are_stable_and_content_free() {
        assert_eq!(shutdown_error_kind(false, false), None);
        assert_eq!(shutdown_error_kind(true, false), Some("terminate"));
        assert_eq!(shutdown_error_kind(false, true), Some("wait"));
        assert_eq!(shutdown_error_kind(true, true), Some("terminate_and_wait"));
    }

    #[test]
    fn response_deadline_is_preserved_as_a_timeout_outcome() {
        let (_sender, receiver) = mpsc::channel();
        let stderr = Arc::new(Mutex::new(String::new()));
        let error = recv_event(Instant::now(), &receiver, "test response", &stderr)
            .err()
            .expect("an empty channel at its deadline must time out");

        assert!(format!("{error:#}").contains("timed out waiting"));
        assert_eq!(app_server_trace_outcome(&error), TraceOutcome::Timeout);
    }

    #[cfg(unix)]
    #[test]
    fn deadline_setup_failure_traces_real_process_shutdown_once() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let trace_path = temp.path().join("trace.jsonl");
        let fake_codex = temp.path().join("private-codex");
        fs::write(&fake_codex, b"#!/bin/sh\nsleep 5\n").unwrap();
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();
        let trace = TraceLog::enabled(&trace_path);
        let config = CollectConfig {
            codex_bin: Some(fake_codex.clone()),
            app_server_timeout: Duration::MAX,
            trace_log: trace.clone(),
            ..CollectConfig::default()
        };

        let error = fetch_account_snapshot(&config).unwrap_err();
        assert!(error.to_string().contains("timeout exceeds"));
        trace.finish();

        let contents = fs::read_to_string(&trace_path).unwrap();
        assert!(!contents.contains(&fake_codex.to_string_lossy().into_owned()));
        assert!(!contents.contains("\"outcome\":\"abandoned\""));
        let events = read_trace_events(&trace_path);
        assert_trace_finish(
            &events,
            "app_server.process.io_setup",
            "error",
            Some("deadline"),
        );
        assert_trace_finish(
            &events,
            "app_server.account_snapshot",
            "error",
            Some("deadline"),
        );
        let shutdown = assert_trace_finish(&events, "app_server.process.shutdown", "ok", None);
        assert!(shutdown["durationUs"].as_u64().is_some());
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event["event"] == "span_start"
                        && event["stage"] == "app_server.process.shutdown"
                })
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event["event"] == "span_finish"
                        && event["stage"] == "app_server.process.shutdown"
                })
                .count(),
            1
        );
        let shutdown_finish = events
            .iter()
            .position(|event| {
                event["event"] == "span_finish" && event["stage"] == "app_server.process.shutdown"
            })
            .unwrap();
        let parent_finish = events
            .iter()
            .position(|event| {
                event["event"] == "span_finish" && event["stage"] == "app_server.account_snapshot"
            })
            .unwrap();
        assert!(
            shutdown_finish < parent_finish,
            "the parent operation must cover setup-failure cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn initialize_timeout_is_traced_as_timeout_through_parent_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let trace_path = temp.path().join("trace.jsonl");
        let fake_codex = temp.path().join("private-timeout-codex");
        fs::write(&fake_codex, b"#!/bin/sh\nsleep 5\n").unwrap();
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();
        let trace = TraceLog::enabled(&trace_path);
        let config = CollectConfig {
            codex_bin: Some(fake_codex.clone()),
            app_server_timeout: Duration::from_millis(25),
            trace_log: trace.clone(),
            ..CollectConfig::default()
        };

        let error = fetch_account_snapshot(&config).unwrap_err();
        assert!(format!("{error:#}").contains("timed out waiting"));
        trace.finish();

        let contents = fs::read_to_string(&trace_path).unwrap();
        assert!(!contents.contains(&fake_codex.to_string_lossy().into_owned()));
        assert!(!contents.contains("\"outcome\":\"abandoned\""));
        let events = read_trace_events(&trace_path);
        assert_trace_finish(
            &events,
            "app_server.rpc.initialize",
            "timeout",
            Some("timeout"),
        );
        assert_trace_finish(
            &events,
            "app_server.account_snapshot",
            "timeout",
            Some("timeout"),
        );
        assert_trace_finish(&events, "app_server.process.shutdown", "ok", None);
    }

    #[cfg(unix)]
    #[test]
    fn optional_usage_timeout_is_traced_without_degrading_the_parent_snapshot() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let trace_path = temp.path().join("trace.jsonl");
        let fake_codex = temp.path().join("private-usage-timeout-codex");
        fs::write(
            &fake_codex,
            br#"#!/bin/sh
IFS= read -r initialize || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized || exit 2
IFS= read -r limits || exit 3
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"codex","primary":null}}}'
IFS= read -r usage || exit 4
sleep 5
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();
        let trace = TraceLog::enabled(&trace_path);
        let config = CollectConfig {
            codex_bin: Some(fake_codex),
            app_server_timeout: Duration::from_secs(2),
            trace_log: trace.clone(),
            ..CollectConfig::default()
        };

        let snapshot = fetch_account_snapshot(&config).unwrap();
        assert_eq!(snapshot.limits.len(), 1);
        assert!(snapshot.warnings.is_empty());
        trace.finish();

        let events = read_trace_events(&trace_path);
        assert_trace_finish(&events, "app_server.rpc.account_reads", "ok", None);
        let usage = assert_trace_finish(&events, "app_server.rpc.optional_usage", "timeout", None);
        assert_eq!(usage["fields"]["partialReason"], "timeout");
        let parent = assert_trace_finish(&events, "app_server.account_snapshot", "ok", None);
        assert_eq!(parent["fields"]["snapshotPartial"], false);
    }

    #[cfg(unix)]
    #[test]
    fn rpc_error_snapshot_is_partial_and_parent_covers_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let trace_path = temp.path().join("trace.jsonl");
        let fake_codex = temp.path().join("private-partial-codex");
        fs::write(
            &fake_codex,
            br#"#!/bin/sh
IFS= read -r initialize || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized || exit 2
IFS= read -r limits || exit 3
printf '%s\n' '{"id":2,"error":{"code":-32000,"message":"private quota failure"}}'
IFS= read -r usage || exit 4
printf '%s\n' '{"id":3,"result":{"summary":{},"dailyUsageBuckets":[]}}'
sleep 5
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();
        let trace = TraceLog::enabled(&trace_path);
        let config = CollectConfig {
            codex_bin: Some(fake_codex.clone()),
            app_server_timeout: Duration::from_secs(2),
            trace_log: trace.clone(),
            ..CollectConfig::default()
        };

        let snapshot = fetch_account_snapshot(&config).unwrap();
        assert_eq!(snapshot.errors.len(), 1);
        trace.finish();

        let contents = fs::read_to_string(&trace_path).unwrap();
        assert!(!contents.contains("private quota failure"));
        assert!(!contents.contains(&fake_codex.to_string_lossy().into_owned()));
        let events = read_trace_events(&trace_path);
        let reads = assert_trace_finish(&events, "app_server.rpc.account_reads", "partial", None);
        assert_eq!(reads["fields"]["rateLimitsRpcError"], true);
        assert_eq!(reads["fields"]["rateLimitsRpcCategory"], "server_error");
        assert_trace_finish(&events, "app_server.rpc.optional_usage", "ok", None);
        let parent = assert_trace_finish(&events, "app_server.account_snapshot", "partial", None);
        assert_eq!(parent["fields"]["cleanupFailed"], false);
        assert_eq!(parent["fields"]["snapshotPartial"], true);
        assert_eq!(parent["fields"]["errorCount"], 1);
        let shutdown_finish = events
            .iter()
            .position(|event| {
                event["event"] == "span_finish" && event["stage"] == "app_server.process.shutdown"
            })
            .unwrap();
        let parent_finish = events
            .iter()
            .position(|event| {
                event["event"] == "span_finish" && event["stage"] == "app_server.account_snapshot"
            })
            .unwrap();
        assert!(shutdown_finish < parent_finish);
    }

    #[test]
    fn unsupported_optional_usage_errors_are_recognized_without_matching_other_failures() {
        assert!(optional_usage_rpc_unavailable(
            "usage disabled (code -32601)"
        ));
        assert!(optional_usage_rpc_unavailable(
            "Invalid request: unknown variant `account/usage/read`, expected one of `initialize` (code -32600)"
        ));
        assert!(!optional_usage_rpc_unavailable(
            "Invalid request: malformed payload (code -32600)"
        ));
    }

    #[test]
    fn rpc_error_categories_are_content_free_and_stable() {
        assert_eq!(
            rpc_error_category(&json!({
                "id": 2,
                "error": { "code": -32601, "message": "private details" }
            })),
            Some("method_not_found")
        );
        assert_eq!(
            rpc_error_category(&json!({
                "id": 2,
                "error": { "code": -32042, "message": "private details" }
            })),
            Some("server_error")
        );
        assert_eq!(
            rpc_error_category(&json!({
                "id": 2,
                "error": { "message": "private details" }
            })),
            Some("uncategorized")
        );
        assert_eq!(rpc_error_category(&json!({ "id": 2, "result": {} })), None);
    }

    #[test]
    fn external_diagnostics_strip_ansi_and_bound_rpc_messages() {
        assert_eq!(
            compact_diagnostic_text(
                "\u{1b}[2m2026-08-25\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m bad config\n",
                128,
            ),
            "2026-08-25 ERROR bad config"
        );

        let error = format_rpc_error(&json!({
            "code": -32600,
            "message": "x".repeat(RPC_ERROR_MESSAGE_LIMIT + 100)
        }));
        assert!(error.ends_with("... (code -32600)"));
        assert!(error.chars().count() <= RPC_ERROR_MESSAGE_LIMIT + 16);
    }

    #[test]
    fn stdout_reader_drains_an_oversized_frame_and_recovers() {
        let input = format!("{}\n{{\"id\":7}}\n", "x".repeat(64));
        let (sender, receiver) = mpsc::sync_channel(4);

        read_stdout_with_limit(Cursor::new(input), sender, 16);

        match receiver.recv().unwrap() {
            ReaderEvent::Malformed(message) => assert!(message.contains("oversized")),
            _ => panic!("expected oversized-frame diagnostic"),
        }
        match receiver.recv().unwrap() {
            ReaderEvent::Message(message) => assert_eq!(message["id"], 7),
            _ => panic!("expected recovered JSON-RPC frame"),
        }
        assert!(matches!(receiver.recv().unwrap(), ReaderEvent::Eof));
    }

    #[test]
    fn protocol_warning_limit_reports_every_suppressed_diagnostic() {
        let mut warnings = ProtocolWarnings::default();
        for index in 0..APP_SERVER_MAX_PROTOCOL_WARNINGS + 5 {
            warnings.push(format!("warning {index}"));
        }

        let warnings = warnings.into_messages();

        assert_eq!(warnings.len(), APP_SERVER_MAX_PROTOCOL_WARNINGS);
        assert_eq!(
            warnings.last().map(String::as_str),
            Some("suppressed 6 additional codex app-server protocol warnings")
        );
    }

    #[test]
    fn lossy_reset_credit_parsing_bounds_details_and_diagnostics_during_parsing() {
        let mut warnings = ProtocolWarnings::default();
        let result = json!({
            "rateLimitResetCredits": {
                "availableCount": APP_SERVER_MAX_RESET_CREDIT_DETAILS + 100,
                "credits": vec![json!(0); APP_SERVER_MAX_RESET_CREDIT_DETAILS + 100]
            }
        });

        let (snapshot, partial) =
            parse_rate_limit_reset_credits_result_lossy(&result, Utc::now(), &mut warnings)
                .unwrap();
        let snapshot = snapshot.unwrap();
        let messages = warnings.into_messages();

        assert!(partial);
        assert_eq!(snapshot.credits.as_ref().map(Vec::len), Some(0));
        assert_eq!(messages.len(), APP_SERVER_MAX_PROTOCOL_WARNINGS);
        assert_eq!(
            messages.last().map(String::as_str),
            Some("suppressed 4066 additional codex app-server protocol warnings")
        );
        assert!(
            parse_rate_limit_reset_credits_result(&result, Utc::now())
                .unwrap_err()
                .to_string()
                .contains("exceeding the 4096-detail limit")
        );
    }

    #[test]
    fn disconnecting_a_full_reader_queue_unblocks_the_reader_thread() {
        use std::thread;
        use std::time::{Duration, Instant};

        let input = "{\"id\":1}\n".repeat(64);
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || read_stdout_with_limit(Cursor::new(input), sender, 64));
        assert!(matches!(receiver.recv().unwrap(), ReaderEvent::Message(_)));
        thread::sleep(Duration::from_millis(20));

        let started = Instant::now();
        drop(receiver);
        reader.join().unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn process_group_cleanup_closes_pipes_inherited_by_descendants() {
        use std::io::{BufRead, BufReader, Read};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        use crate::startup::StartupTrace;

        use super::{ChildGuard, attach_process_tree, configure_process_tree};

        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 5 & echo ready; wait"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_process_tree(&mut command);
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let process_tree = attach_process_tree(&mut child).unwrap();
        let mut child = ChildGuard {
            child,
            process_tree,
            startup_trace: StartupTrace::default(),
            trace_log: crate::trace::TraceLog::default(),
            reaped: false,
        };
        let mut reader = BufReader::new(stdout);
        let mut ready = String::new();
        reader.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "ready");

        let started = Instant::now();
        child.terminate_and_reap().unwrap();
        let mut remainder = Vec::new();
        reader.read_to_end(&mut remainder).unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(remainder.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn reader_cleanup_is_bounded_when_a_process_outside_the_group_keeps_the_pipe_open() {
        use std::io::{BufRead, BufReader, Read};
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        use crate::startup::StartupTrace;

        use super::{
            ChildGuard, ReaderTask, attach_process_tree, configure_process_tree,
            finish_reader_threads,
        };

        let (read_pipe, write_pipe) = UnixStream::pair().unwrap();
        let primary_stdout: OwnedFd = write_pipe.try_clone().unwrap().into();
        let escaped_stdout: OwnedFd = write_pipe.try_clone().unwrap().into();

        let mut command = Command::new("sh");
        command
            .args(["-c", "echo ready; sleep 5"])
            .stdout(Stdio::from(primary_stdout))
            .stderr(Stdio::null());
        configure_process_tree(&mut command);
        let mut primary = command.spawn().unwrap();
        let process_tree = attach_process_tree(&mut primary).unwrap();
        let mut primary = ChildGuard {
            child: primary,
            process_tree,
            startup_trace: StartupTrace::default(),
            trace_log: crate::trace::TraceLog::default(),
            reaped: false,
        };

        // This sibling models a custom wrapper that moved a descendant to a
        // different process group while leaving the app-server pipe inherited.
        let mut escaped = Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdout(Stdio::from(escaped_stdout))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        drop(write_pipe);

        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let stdout_done_tx = done_tx.clone();
        let stdout_reader = thread::spawn(move || {
            let mut reader = BufReader::new(read_pipe);
            let mut ready = String::new();
            reader.read_line(&mut ready).unwrap();
            let _ = ready_tx.send(ready);
            let mut remainder = Vec::new();
            let _ = reader.read_to_end(&mut remainder);
            let _ = stdout_done_tx.send(ReaderTask::Stdout);
        });
        let stderr_reader = thread::spawn(move || {
            let _ = done_tx.send(ReaderTask::Stderr);
        });
        assert_eq!(
            ready_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .trim(),
            "ready"
        );

        let started = Instant::now();
        primary.terminate_and_reap().unwrap();
        finish_reader_threads(stdout_reader, stderr_reader, done_rx);

        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = escaped.kill();
        let _ = escaped.wait();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::env;
    use std::fs;
    use std::io::{self, Read};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::startup::StartupTrace;

    use super::{
        ChildGuard, app_server_spawn_error, attach_process_tree, configure_process_tree,
        resolve_automatic_windows_codex_cli_with_installed,
    };

    const WINDOWS_JOB_HELPER_MARKER: &str = "CODEX_USAGE_MONIT_WINDOWS_JOB_HELPER_MARKER";

    fn desktop_resource_path() -> PathBuf {
        PathBuf::from(
            r"C:\Program Files\WINDOWSAPPS\openai.codex_26.818.0.0_x64__test\APP\RESOURCES\CODEX.EXE",
        )
    }

    #[test]
    fn desktop_resource_access_denied_gets_an_actionable_hint() {
        let path = desktop_resource_path();
        let rendered = format!(
            "{:#}",
            app_server_spawn_error(path.as_os_str(), io::Error::from_raw_os_error(5))
        );

        assert!(rendered.contains("Codex Desktop packaged resource"));
        assert!(rendered.contains(&path.display().to_string()));
        assert!(rendered.contains("--codex-bin"));
        assert!(rendered.contains("--offline"));
    }

    #[test]
    #[allow(clippy::zombie_processes)] // The helper must stay alive so the parent can prove the Job kills its grandchild.
    fn windows_job_resumes_child_and_terminates_inherited_grandchild() {
        if let Some(marker) = env::var_os(WINDOWS_JOB_HELPER_MARKER) {
            let _grandchild = Command::new("cmd.exe")
                .args(["/D", "/S", "/C", "ping -n 31 127.0.0.1"])
                .stdout(Stdio::inherit())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            fs::write(marker, b"resumed").unwrap();
            thread::sleep(Duration::from_secs(30));
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("resumed.marker");
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "windows_job_resumes_child_and_terminates_inherited_grandchild",
                "--nocapture",
            ])
            .env(WINDOWS_JOB_HELPER_MARKER, &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_process_tree(&mut command);
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let process_tree = attach_process_tree(&mut child).unwrap();
        let mut child = ChildGuard {
            child,
            process_tree,
            startup_trace: StartupTrace::default(),
            trace_log: crate::trace::TraceLog::default(),
            reaped: false,
        };

        let resume_deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < resume_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !marker.exists() {
            let _ = child.terminate_and_reap();
            panic!("suspended child did not resume and create its marker");
        }

        let started = Instant::now();
        child.terminate_and_reap().unwrap();
        let mut reader = io::BufReader::new(stdout);
        let mut remainder = Vec::new();
        reader.read_to_end(&mut remainder).unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn desktop_resource_other_permission_error_keeps_its_original_diagnostic() {
        let path = desktop_resource_path();
        let rendered = format!(
            "{:#}",
            app_server_spawn_error(
                path.as_os_str(),
                io::Error::new(io::ErrorKind::PermissionDenied, "blocked by policy"),
            )
        );

        assert!(rendered.contains("blocked by policy"));
        assert!(!rendered.contains("Codex Desktop packaged resource"));
    }

    #[test]
    fn ordinary_access_denied_is_not_reclassified_as_a_desktop_resource() {
        let path = PathBuf::from(r"C:\tools\codex.exe");
        let rendered = format!(
            "{:#}",
            app_server_spawn_error(path.as_os_str(), io::Error::from_raw_os_error(5))
        );

        assert!(!rendered.contains("Codex Desktop packaged resource"));
    }

    fn automatic_discovery_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        PathBuf,
        std::ffi::OsString,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let resources = temp
            .path()
            .join("WindowsApps")
            .join("OpenAI.Codex_26.818.0.0_x64__test")
            .join("app")
            .join("resources");
        let npm_bin = temp.path().join("npm-bin");
        let installed_bin = temp.path().join("installed-bin");
        fs::create_dir_all(&resources).unwrap();
        fs::create_dir_all(&npm_bin).unwrap();
        fs::create_dir_all(&installed_bin).unwrap();
        let desktop = resources.join("codex.exe");
        let npm = npm_bin.join("codex.cmd");
        let installed = installed_bin.join("codex.exe");
        fs::write(&desktop, b"").unwrap();
        fs::write(&npm, "@echo off\r\nexit /b 0\r\n").unwrap();
        fs::write(&installed, b"").unwrap();
        let path = env::join_paths([resources, npm_bin]).unwrap();
        (temp, desktop, npm, installed, path)
    }

    #[test]
    fn automatic_discovery_skips_desktop_resource_for_later_cmd() {
        let (temp, _desktop, npm, _installed, path) = automatic_discovery_fixture();

        let resolved =
            resolve_automatic_windows_codex_cli_with_installed(&path, temp.path(), None).unwrap();

        assert_eq!(resolved, fs::canonicalize(npm).unwrap());
    }

    #[test]
    fn automatic_discovery_prefers_installed_cli_after_desktop_resource() {
        let (temp, _desktop, _npm, installed, path) = automatic_discovery_fixture();

        let resolved = resolve_automatic_windows_codex_cli_with_installed(
            &path,
            temp.path(),
            Some(installed.clone()),
        )
        .unwrap();

        assert_eq!(resolved, installed);
    }

    #[test]
    fn automatic_discovery_keeps_a_runnable_path_cli_ahead_of_installed_fallback() {
        let (temp, _desktop, npm, installed, _path) = automatic_discovery_fixture();
        let npm_bin = npm.parent().unwrap();
        let path = env::join_paths([npm_bin]).unwrap();

        let resolved =
            resolve_automatic_windows_codex_cli_with_installed(&path, temp.path(), Some(installed))
                .unwrap();

        assert_eq!(resolved, fs::canonicalize(npm).unwrap());
    }

    #[test]
    fn automatic_discovery_rejects_desktop_resource_without_an_alternative() {
        let (temp, desktop, _npm, _installed, _path) = automatic_discovery_fixture();
        let resources = desktop.parent().unwrap();
        let path = env::join_paths([resources]).unwrap();
        // Discovery canonicalizes candidates before building the diagnostic;
        // use the same Windows path representation (including any `\\?\`
        // prefix) in the assertion.
        let expected_desktop = fs::canonicalize(&desktop).unwrap();

        let error = resolve_automatic_windows_codex_cli_with_installed(&path, temp.path(), None)
            .unwrap_err();
        let rendered = format!("{error:#}");

        assert!(rendered.contains("Codex Desktop packaged resource"));
        assert!(rendered.contains(&expected_desktop.display().to_string()));
        assert!(rendered.contains("--codex-bin"));
    }
}
