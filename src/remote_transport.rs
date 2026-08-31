//! Bounded, one-shot OpenSSH transport for explicit remote requests.
//!
//! This module does not schedule hosts and never reads SSH configuration on
//! its own. A caller must provide one already allowlisted host alias. The
//! argv and remote command are fixed; all dynamic protocol input travels in a
//! framed stdin request.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
#[cfg(windows)]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
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
    CREATE_SUSPENDED, GetCurrentProcess, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

use crate::remote_agent::current_accepted_revisions;
use crate::remote_protocol::{
    EmptyRemotePayload, ProbeRequest, REMOTE_PROTOCOL_VERSION, RemoteExportRequest,
    RemoteExportRequestBody, RemoteExportResponse, RemoteExportResponseBody, RemoteFailure,
    RemoteFrameLimits, RemotePagePayload, RemoteProtocolError, SourceGeneration,
    decode_remote_frame, decoded_remote_frame_payload_len, encode_remote_frame,
};
use crate::source_history::RedactionProfile;

pub const DEFAULT_REMOTE_AGENT_EXECUTABLE: &str = "codex-usage-monit";
const SSH_REMOTE_AGENT_ARGUMENTS: [&str; 2] = ["remote-agent", "export"];
const SSH_OPTIONS: &[&str] = &[
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=yes",
    "-o",
    "ConnectionAttempts=1",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=2",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForkAfterAuthentication=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "Tunnel=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "StdinNull=no",
];
const MAX_STDERR_BYTES: usize = 32 * 1024;
const MAX_DIAGNOSTIC_CHARS: usize = 512;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const PIPE_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PIPE_CLEANUP_EOF_GRACE: Duration = Duration::from_millis(100);
const PIPE_CLEANUP_STOP_GRACE: Duration = Duration::from_millis(100);
const TUI_PROCESS_TREE_TOKEN_ENV: &str = "CODEX_USAGE_MONIT_TUI_PROCESS_TREE_TOKEN";
const TUI_PROCESS_TREE_PARENT_ENV: &str = "CODEX_USAGE_MONIT_TUI_PROCESS_TREE_PARENT";
const TUI_PROCESS_TREE_TOKEN_BYTES: usize = 32;

pub type RemoteProbeResponse = RemoteExportResponse<EmptyRemotePayload, EmptyRemotePayload>;

/// One-shot capability passed only from the TUI parent to its isolated helper.
///
/// The hidden CLI option is intentionally insufficient on its own. The helper
/// must receive the same random value through its inherited environment, name
/// the real parent process, and prove that the OS containment boundary exists.
/// A failed or malformed check always falls back to an independently owned SSH
/// process tree.
pub(crate) struct TuiProcessTreeInheritanceContract {
    token: String,
    parent_process_id: u32,
}

impl TuiProcessTreeInheritanceContract {
    pub(crate) fn generate() -> io::Result<Self> {
        let mut random = [0_u8; TUI_PROCESS_TREE_TOKEN_BYTES];
        getrandom::fill(&mut random).map_err(|error| {
            io::Error::other(format!(
                "could not create the TUI helper process-tree capability: {error}"
            ))
        })?;
        if random.iter().all(|byte| *byte == 0) {
            return Err(io::Error::other(
                "secure random provider returned an unusable TUI helper capability",
            ));
        }
        let mut token = String::with_capacity(TUI_PROCESS_TREE_TOKEN_BYTES * 2);
        for byte in random {
            use std::fmt::Write as _;
            write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(Self {
            token,
            parent_process_id: std::process::id(),
        })
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        command
            .arg("--inherit-remote-process-tree")
            .arg(&self.token);
        self.apply_environment(command);
    }

    fn apply_environment(&self, command: &mut Command) {
        command.env(TUI_PROCESS_TREE_TOKEN_ENV, &self.token).env(
            TUI_PROCESS_TREE_PARENT_ENV,
            self.parent_process_id.to_string(),
        );
    }

    #[cfg(all(test, unix))]
    pub(crate) fn apply_environment_for_test(&self, command: &mut Command) {
        self.apply_environment(command);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn token_for_test(&self) -> &str {
        &self.token
    }
}

pub(crate) fn tui_process_tree_inheritance_is_authorized(requested: Option<&str>) -> bool {
    let environment_token = env::var_os(TUI_PROCESS_TREE_TOKEN_ENV);
    let environment_parent = env::var_os(TUI_PROCESS_TREE_PARENT_ENV);
    validate_tui_process_tree_inheritance(
        requested,
        environment_token.as_deref(),
        environment_parent.as_deref(),
        current_parent_process_id(),
        current_process_has_tui_containment(),
    )
}

pub(crate) fn validate_tui_process_tree_inheritance(
    requested: Option<&str>,
    environment_token: Option<&OsStr>,
    environment_parent: Option<&OsStr>,
    actual_parent_process_id: Option<u32>,
    has_tui_containment: bool,
) -> bool {
    let Some(requested) = requested.filter(|token| valid_tui_process_tree_token(token)) else {
        return false;
    };
    let Some(environment_token) = environment_token.and_then(OsStr::to_str) else {
        return false;
    };
    let Some(expected_parent_process_id) = environment_parent
        .and_then(OsStr::to_str)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value != 0)
    else {
        return false;
    };
    has_tui_containment
        && requested == environment_token
        && actual_parent_process_id == Some(expected_parent_process_id)
}

fn valid_tui_process_tree_token(token: &str) -> bool {
    token.len() == TUI_PROCESS_TREE_TOKEN_BYTES * 2
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && token.bytes().any(|byte| byte != b'0')
}

#[cfg(unix)]
fn current_parent_process_id() -> Option<u32> {
    u32::try_from(unsafe { libc::getppid() })
        .ok()
        .filter(|pid| *pid != 0)
}

#[cfg(unix)]
fn current_process_has_tui_containment() -> bool {
    unsafe { libc::getpgrp() == libc::getpid() }
}

#[cfg(windows)]
fn current_parent_process_id() -> Option<u32> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let current = std::process::id();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..PROCESSENTRY32W::default()
    };
    let mut found = None;
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        if entry.th32ProcessID == current {
            found = Some(entry.th32ParentProcessID).filter(|pid| *pid != 0);
            break;
        }
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    found
}

#[cfg(windows)]
fn current_process_has_tui_containment() -> bool {
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut in_job = 0;
    (unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) }) != 0
        && in_job != 0
}

#[cfg(not(any(unix, windows)))]
fn current_parent_process_id() -> Option<u32> {
    None
}

#[cfg(not(any(unix, windows)))]
fn current_process_has_tui_containment() -> bool {
    false
}

/// Environment used for one-shot system OpenSSH children.
///
/// Interactive callers leave `path` unset and retain the process environment.
/// The Windows recorder supplies the PATH captured when its scheduled task was
/// installed. In that case the SSH executable is resolved from the saved PATH
/// before spawn, and the same PATH is installed in the child so configured
/// `ProxyCommand` and other OpenSSH helpers see the identical search path.
#[derive(Clone, Default)]
pub struct SshCommandEnvironment {
    path: Option<OsString>,
    inherit_parent_process_tree: bool,
    cancellation: Option<Arc<AtomicBool>>,
}

impl SshCommandEnvironment {
    pub fn new(path: Option<OsString>) -> Self {
        Self {
            path,
            inherit_parent_process_tree: false,
            cancellation: None,
        }
    }

    /// Used only by the TUI's already-isolated helper CLI. SSH must remain in
    /// the outer process group / Job Object instead of creating a nested
    /// cancellation boundary. Unix helpers which deliberately leave that
    /// group are handled as bounded, explicitly diagnosed residuals.
    pub(crate) fn inheriting_parent_process_tree(path: Option<OsString>) -> Self {
        Self {
            path,
            inherit_parent_process_tree: true,
            cancellation: None,
        }
    }

    /// Installs a process-local cancellation flag for an interactive request
    /// or recorder worker. The flag is deliberately not serialized or
    /// forwarded to SSH; it only lets the bounded parent wait tear down its
    /// owned process group after a stop/signal becomes a safe notification.
    pub(crate) fn with_cancellation(mut self, cancellation: Arc<AtomicBool>) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    fn resolve_program(&self) -> io::Result<PathBuf> {
        let program = default_ssh_program();
        let Some(path) = self.path.as_deref() else {
            return Ok(program);
        };
        resolve_program_from_path(&program, path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "system SSH was not found in the saved service PATH",
            )
        })
    }

    fn apply(&self, command: &mut Command) {
        // The one-shot TUI capability is consumed by the helper CLI and must
        // never be forwarded into OpenSSH, ProxyCommand, or remote processes.
        command
            .env_remove(TUI_PROCESS_TREE_TOKEN_ENV)
            .env_remove(TUI_PROCESS_TREE_PARENT_ENV);
        if let Some(path) = self.path.as_deref() {
            command.env("PATH", path);
        }
    }

    pub(crate) fn owns_process_tree(&self) -> bool {
        !self.inherit_parent_process_tree
    }

    fn cancellation_requested(&self) -> bool {
        self.cancellation
            .as_deref()
            .is_some_and(|requested| requested.load(Ordering::Acquire))
    }
}

/// Establishes a process-lifetime Windows Job before a direct CLI/recorder can
/// spawn OpenSSH. Children inherit this outer kill-on-close Job atomically at
/// CreateProcess time, closing the otherwise unavoidable gap before the
/// transport attaches its narrower per-request Job. The raw handle is kept in
/// the process handle table intentionally and is closed by the OS at exit.
#[cfg(windows)]
pub(crate) fn ensure_current_process_remote_containment() -> io::Result<()> {
    static OUTER_JOB: OnceLock<Result<(), String>> = OnceLock::new();
    match OUTER_JOB.get_or_init(|| {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(format!(
                "could not create the process-level remote Job: {}",
                io::Error::last_os_error()
            ));
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(job) };
            return Err(format!(
                "could not configure the process-level remote Job: {error}"
            ));
        }
        if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(job) };
            return Err(format!(
                "could not attach this process to the process-level remote Job: {error}"
            ));
        }
        // Do not close `job`: kill-on-close must remain armed for the complete
        // process lifetime, including abrupt console/service termination.
        Ok(())
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(io::Error::other(message.clone())),
    }
}

#[cfg(not(windows))]
pub(crate) fn ensure_current_process_remote_containment() -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Debug)]
pub struct RemoteProbeOptions {
    pub timeout: Duration,
    /// Negotiated compressed response-payload bound. Automatic health probes
    /// use the protocol minimum so a paused source can never turn a liveness
    /// check into an accidental data-page-sized transfer.
    pub max_response_bytes: usize,
    pub check_state_writable: bool,
    pub check_rollout_readable: bool,
    pub redaction_profile: RedactionProfile,
    pub expected_source: Option<SourceGeneration>,
}

impl Default for RemoteProbeOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(45),
            max_response_bytes: RemoteFrameLimits::default().max_encoded_bytes,
            check_state_writable: true,
            check_rollout_readable: true,
            redaction_profile: RedactionProfile::Redacted,
            expected_source: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemoteProbeReport {
    pub response: RemoteProbeResponse,
    pub elapsed: Duration,
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub stderr_bytes: usize,
}

/// Timings and exact framed byte counts for one explicit remote exchange.
///
/// The generic payload types keep aggregate deltas and future fact pages
/// statically separate while sharing the same bounded process runner.
#[derive(Clone, Debug)]
pub struct RemoteExchangeReport<D = EmptyRemotePayload, F = EmptyRemotePayload> {
    pub response: RemoteExportResponse<D, F>,
    pub elapsed: Duration,
    pub request_bytes: usize,
    pub response_bytes: usize,
    /// Exact JSON payload bytes after decompression, authenticated by the
    /// successfully decoded frame header.
    pub response_decoded_bytes: usize,
    pub stderr_bytes: usize,
}

#[derive(Debug)]
pub enum RemoteTransportError {
    InvalidHost(String),
    InvalidTimeout,
    InvalidResponseLimit,
    Spawn {
        error: io::Error,
        cleanup_error: Option<io::Error>,
    },
    ProcessIsolation {
        error: io::Error,
        cleanup_error: Option<io::Error>,
    },
    RequestWrite(io::Error),
    Wait {
        error: io::Error,
        cleanup_error: Option<io::Error>,
    },
    Timeout {
        timeout: Duration,
        cleanup_error: Option<io::Error>,
    },
    Cancelled {
        cleanup_error: Option<io::Error>,
    },
    OutputRead(io::Error),
    StdoutLimitExceeded {
        cleanup_error: Option<io::Error>,
    },
    StderrLimitExceeded {
        cleanup_error: Option<io::Error>,
    },
    ExitFailure {
        code: Option<i32>,
        diagnostic: String,
    },
    Protocol(RemoteProtocolError),
    Remote(RemoteFailure),
    UnexpectedResponse,
    WorkerPanicked,
    ProcessCleanup {
        error: io::Error,
        cleanup_error: Option<io::Error>,
    },
}

impl RemoteTransportError {
    /// Returns true when the owned SSH process tree could not be proven fully
    /// reclaimed. Automatic callers must stop retrying this host: a helper
    /// from user SSH configuration (for example a ProxyCommand which created
    /// a new session) may still be alive and holding inherited descriptors.
    pub fn process_containment_uncertain(&self) -> bool {
        match self {
            Self::Spawn { cleanup_error, .. }
            | Self::ProcessIsolation { cleanup_error, .. }
            | Self::Wait { cleanup_error, .. }
            | Self::Timeout { cleanup_error, .. }
            | Self::Cancelled { cleanup_error }
            | Self::StdoutLimitExceeded { cleanup_error }
            | Self::StderrLimitExceeded { cleanup_error } => cleanup_error.is_some(),
            Self::ProcessCleanup { .. } => true,
            _ => false,
        }
    }
}

impl fmt::Display for RemoteTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHost(message) => write!(formatter, "invalid SSH host alias: {message}"),
            Self::InvalidTimeout => formatter.write_str("remote request timeout must be non-zero"),
            Self::InvalidResponseLimit => {
                formatter.write_str("remote probe response limit does not fit the protocol field")
            }
            Self::Spawn {
                error,
                cleanup_error,
            } => {
                write!(formatter, "could not start system SSH: {error}")?;
                write_cleanup_suffix(formatter, cleanup_error)
            }
            Self::ProcessIsolation {
                error,
                cleanup_error,
            } => {
                write!(
                    formatter,
                    "could not isolate the system SSH process tree: {error}"
                )?;
                write_cleanup_suffix(formatter, cleanup_error)
            }
            Self::RequestWrite(error) => {
                write!(formatter, "could not write the remote request: {error}")
            }
            Self::Wait {
                error,
                cleanup_error,
            } => {
                write!(formatter, "could not wait for system SSH: {error}")?;
                write_cleanup_suffix(formatter, cleanup_error)
            }
            Self::Timeout {
                timeout,
                cleanup_error,
            } => {
                write!(
                    formatter,
                    "remote request timed out after {:.1}s",
                    timeout.as_secs_f64()
                )?;
                if let Some(error) = cleanup_error {
                    write!(formatter, "; process cleanup also failed: {error}")?;
                }
                Ok(())
            }
            Self::Cancelled { cleanup_error } => {
                formatter.write_str("remote request was interrupted")?;
                if let Some(error) = cleanup_error {
                    write!(formatter, "; process cleanup also failed: {error}")?;
                }
                Ok(())
            }
            Self::OutputRead(error) => {
                write!(formatter, "could not read system SSH output: {error}")
            }
            Self::StdoutLimitExceeded { cleanup_error } => {
                formatter.write_str("remote request stdout exceeded the protocol frame limit")?;
                write_cleanup_suffix(formatter, cleanup_error)
            }
            Self::StderrLimitExceeded { cleanup_error } => {
                formatter.write_str("remote request stderr exceeded the diagnostic limit")?;
                write_cleanup_suffix(formatter, cleanup_error)
            }
            Self::ExitFailure { code, diagnostic } => {
                write!(formatter, "system SSH exited with status {code:?}")?;
                if !diagnostic.is_empty() {
                    write!(formatter, ": {diagnostic}")?;
                }
                Ok(())
            }
            Self::Protocol(error) => write!(formatter, "invalid remote protocol response: {error}"),
            Self::Remote(failure) => {
                write!(
                    formatter,
                    "remote request failed ({:?}): {}",
                    failure.kind, failure.message
                )
            }
            Self::UnexpectedResponse => {
                formatter.write_str("remote returned a data page to a probe request")
            }
            Self::WorkerPanicked => formatter.write_str("remote transport worker panicked"),
            Self::ProcessCleanup {
                error,
                cleanup_error,
            } => {
                write!(
                    formatter,
                    "could not clean up the system SSH process tree: {error}"
                )?;
                write_cleanup_suffix(formatter, cleanup_error)
            }
        }
    }
}

impl std::error::Error for RemoteTransportError {}

fn write_cleanup_suffix(
    formatter: &mut fmt::Formatter<'_>,
    cleanup_error: &Option<io::Error>,
) -> fmt::Result {
    if let Some(error) = cleanup_error {
        write!(formatter, "; process cleanup also failed: {error}")?;
    }
    Ok(())
}

impl From<RemoteProtocolError> for RemoteTransportError {
    fn from(error: RemoteProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// Executes one explicit probe against one allowlisted SSH alias.
///
/// No scheduler calls this function yet. In particular, constructing a
/// remotes config or starting the normal application cannot reach this path.
pub fn probe_remote(
    ssh_host: &str,
    options: &RemoteProbeOptions,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    probe_remote_with_agent_executable(ssh_host, DEFAULT_REMOTE_AGENT_EXECUTABLE, options)
}

pub fn probe_remote_with_agent_executable(
    ssh_host: &str,
    agent_executable: &str,
    options: &RemoteProbeOptions,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    probe_remote_with_program_and_agent_executable(
        default_ssh_program(),
        ssh_host,
        agent_executable,
        options,
    )
}

pub fn probe_remote_with_environment(
    ssh_host: &str,
    options: &RemoteProbeOptions,
    environment: &SshCommandEnvironment,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    probe_remote_with_agent_executable_and_environment(
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        options,
        environment,
    )
}

pub fn probe_remote_with_agent_executable_and_environment(
    ssh_host: &str,
    agent_executable: &str,
    options: &RemoteProbeOptions,
    environment: &SshCommandEnvironment,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    validate_ssh_host(ssh_host)?;
    if environment.cancellation_requested() {
        return Err(RemoteTransportError::Cancelled {
            cleanup_error: None,
        });
    }
    let program = environment
        .resolve_program()
        .map_err(|error| RemoteTransportError::Spawn {
            error,
            cleanup_error: None,
        })?;
    probe_remote_with_program_agent_executable_and_environment(
        program,
        ssh_host,
        agent_executable,
        options,
        environment,
    )
}

#[cfg(test)]
fn probe_remote_with_program(
    ssh_program: PathBuf,
    ssh_host: &str,
    options: &RemoteProbeOptions,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    probe_remote_with_program_and_agent_executable(
        ssh_program,
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        options,
    )
}

fn probe_remote_with_program_and_agent_executable(
    ssh_program: PathBuf,
    ssh_host: &str,
    agent_executable: &str,
    options: &RemoteProbeOptions,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    probe_remote_with_program_agent_executable_and_environment(
        ssh_program,
        ssh_host,
        agent_executable,
        options,
        &SshCommandEnvironment::default(),
    )
}

#[cfg(test)]
fn probe_remote_with_program_and_environment(
    ssh_program: PathBuf,
    ssh_host: &str,
    options: &RemoteProbeOptions,
    environment: &SshCommandEnvironment,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    probe_remote_with_program_agent_executable_and_environment(
        ssh_program,
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        options,
        environment,
    )
}

fn probe_remote_with_program_agent_executable_and_environment(
    ssh_program: PathBuf,
    ssh_host: &str,
    agent_executable: &str,
    options: &RemoteProbeOptions,
    environment: &SshCommandEnvironment,
) -> Result<RemoteProbeReport, RemoteTransportError> {
    let request = probe_request(options)?;
    let report: RemoteExchangeReport =
        exchange_remote_with_program_agent_executable_and_environment(
            ssh_program,
            ssh_host,
            agent_executable,
            &request,
            options.timeout,
            environment,
        )?;
    match &report.response.result {
        RemoteExportResponseBody::Probe(_) => {}
        RemoteExportResponseBody::Failure(failure) => {
            return Err(RemoteTransportError::Remote(failure.clone()));
        }
        RemoteExportResponseBody::Delta { .. }
        | RemoteExportResponseBody::FactSnapshot { .. }
        | RemoteExportResponseBody::FactDelta { .. } => {
            return Err(RemoteTransportError::UnexpectedResponse);
        }
    }
    Ok(RemoteProbeReport {
        response: report.response,
        elapsed: report.elapsed,
        request_bytes: report.request_bytes,
        response_bytes: report.response_bytes,
        stderr_bytes: report.stderr_bytes,
    })
}

/// Executes one already-built request against one explicitly selected SSH
/// alias. This function never enumerates SSH configuration or schedules other
/// hosts; the caller remains responsible for allowlist and identity policy.
/// A valid framed `Failure` is returned in the report so synchronization can
/// verify its envelope before acting on recoverable cursor-expiry signals.
pub fn exchange_remote<D, F>(
    ssh_host: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_agent_executable(
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        request,
        timeout,
    )
}

pub fn exchange_remote_with_agent_executable<D, F>(
    ssh_host: &str,
    agent_executable: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_program_and_agent_executable(
        default_ssh_program(),
        ssh_host,
        agent_executable,
        request,
        timeout,
    )
}

pub fn exchange_remote_with_environment<D, F>(
    ssh_host: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    environment: &SshCommandEnvironment,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_agent_executable_and_environment(
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        request,
        timeout,
        environment,
    )
}

pub fn exchange_remote_with_agent_executable_and_environment<D, F>(
    ssh_host: &str,
    agent_executable: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    environment: &SshCommandEnvironment,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    validate_ssh_host(ssh_host)?;
    if environment.cancellation_requested() {
        return Err(RemoteTransportError::Cancelled {
            cleanup_error: None,
        });
    }
    let program = environment
        .resolve_program()
        .map_err(|error| RemoteTransportError::Spawn {
            error,
            cleanup_error: None,
        })?;
    exchange_remote_with_program_agent_executable_and_environment(
        program,
        ssh_host,
        agent_executable,
        request,
        timeout,
        environment,
    )
}

/// Executes one request with a caller-supplied decoded-response limit. Fact
/// synchronization passes its remaining run budget here so an oversized next
/// page is rejected from the authenticated frame header before decompression
/// and JSON allocation.
pub fn exchange_remote_with_frame_limits<D, F>(
    ssh_host: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    response_limits: RemoteFrameLimits,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_frame_limits_and_agent_executable(
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        request,
        timeout,
        response_limits,
    )
}

pub fn exchange_remote_with_frame_limits_and_agent_executable<D, F>(
    ssh_host: &str,
    agent_executable: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    response_limits: RemoteFrameLimits,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_frame_limits_and_agent_executable_and_environment(
        ssh_host,
        agent_executable,
        request,
        timeout,
        response_limits,
        &SshCommandEnvironment::default(),
    )
}

pub fn exchange_remote_with_frame_limits_and_environment<D, F>(
    ssh_host: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    response_limits: RemoteFrameLimits,
    environment: &SshCommandEnvironment,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_frame_limits_and_agent_executable_and_environment(
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        request,
        timeout,
        response_limits,
        environment,
    )
}

pub fn exchange_remote_with_frame_limits_and_agent_executable_and_environment<D, F>(
    ssh_host: &str,
    agent_executable: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    response_limits: RemoteFrameLimits,
    environment: &SshCommandEnvironment,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    validate_ssh_host(ssh_host)?;
    let program = environment
        .resolve_program()
        .map_err(|error| RemoteTransportError::Spawn {
            error,
            cleanup_error: None,
        })?;
    exchange_remote_with_program_and_limits(
        program,
        ssh_host,
        agent_executable,
        request,
        timeout,
        response_limits,
        environment,
    )
}

#[cfg(test)]
fn exchange_remote_with_program<D, F>(
    ssh_program: PathBuf,
    ssh_host: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_program_and_agent_executable(
        ssh_program,
        ssh_host,
        DEFAULT_REMOTE_AGENT_EXECUTABLE,
        request,
        timeout,
    )
}

fn exchange_remote_with_program_and_agent_executable<D, F>(
    ssh_program: PathBuf,
    ssh_host: &str,
    agent_executable: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_program_agent_executable_and_environment(
        ssh_program,
        ssh_host,
        agent_executable,
        request,
        timeout,
        &SshCommandEnvironment::default(),
    )
}

fn exchange_remote_with_program_agent_executable_and_environment<D, F>(
    ssh_program: PathBuf,
    ssh_host: &str,
    agent_executable: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    environment: &SshCommandEnvironment,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    exchange_remote_with_program_and_limits(
        ssh_program,
        ssh_host,
        agent_executable,
        request,
        timeout,
        RemoteFrameLimits::default(),
        environment,
    )
}

fn exchange_remote_with_program_and_limits<D, F>(
    ssh_program: PathBuf,
    ssh_host: &str,
    agent_executable: &str,
    request: &RemoteExportRequest,
    timeout: Duration,
    response_limits: RemoteFrameLimits,
    environment: &SshCommandEnvironment,
) -> Result<RemoteExchangeReport<D, F>, RemoteTransportError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    validate_ssh_host(ssh_host)?;
    if timeout.is_zero() {
        return Err(RemoteTransportError::InvalidTimeout);
    }
    if environment.cancellation_requested() {
        return Err(RemoteTransportError::Cancelled {
            cleanup_error: None,
        });
    }

    let frame = encode_remote_frame(request, RemoteFrameLimits::default())?;
    let request_bytes = frame.len();
    let started = Instant::now();
    let deadline = started
        .checked_add(timeout)
        .ok_or(RemoteTransportError::InvalidTimeout)?;
    let mut command = build_ssh_command(&ssh_program, ssh_host, agent_executable);
    environment.apply(&mut command);
    configure_process_tree(&mut command, environment.owns_process_tree());
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| RemoteTransportError::Spawn {
            error,
            cleanup_error: None,
        })?;
    let mut process_tree = match attach_process_tree(&mut child, environment.owns_process_tree()) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            let cleanup_error = kill_and_reap_bounded(child);
            return Err(RemoteTransportError::ProcessIsolation {
                error,
                cleanup_error,
            });
        }
    };

    let Some(stdin) = child.stdin.take() else {
        let cleanup_error = terminate_and_reap_bounded(process_tree, child);
        return Err(RemoteTransportError::Spawn {
            error: io::Error::other("system SSH stdin pipe is unavailable"),
            cleanup_error,
        });
    };
    let Some(stdout) = child.stdout.take() else {
        drop(stdin);
        let cleanup_error = terminate_and_reap_bounded(process_tree, child);
        return Err(RemoteTransportError::Spawn {
            error: io::Error::other("system SSH stdout pipe is unavailable"),
            cleanup_error,
        });
    };
    let Some(stderr) = child.stderr.take() else {
        drop(stdin);
        drop(stdout);
        let cleanup_error = terminate_and_reap_bounded(process_tree, child);
        return Err(RemoteTransportError::Spawn {
            error: io::Error::other("system SSH stderr pipe is unavailable"),
            cleanup_error,
        });
    };
    if let Err(error) = configure_transport_pipes_nonblocking(&stdin, &stdout, &stderr) {
        drop(stdin);
        drop(stdout);
        drop(stderr);
        let cleanup_error = terminate_and_reap_bounded(process_tree, child);
        return Err(RemoteTransportError::Spawn {
            error,
            cleanup_error,
        });
    }

    let io_stop = Arc::new(AtomicBool::new(false));
    let output_limit = Arc::new(AtomicU8::new(OutputLimit::NONE));
    let io_control = IoWorkerControl {
        deadline,
        stop: Arc::clone(&io_stop),
    };
    let input_control = io_control.clone();
    let input_worker = spawn_worker(move || write_request(stdin, &frame, &input_control));
    let stdout_limit = response_limits
        .max_encoded_bytes
        .min(request.max_page_bytes as usize)
        .saturating_add(20);
    let stdout_control = io_control.clone();
    let stdout_output_limit = Arc::clone(&output_limit);
    let stdout_worker = spawn_worker(move || {
        drain_bounded_until(
            stdout,
            stdout_limit,
            &stdout_control,
            &stdout_output_limit,
            OutputLimit::Stdout,
        )
    });
    let stderr_output_limit = Arc::clone(&output_limit);
    let stderr_worker = spawn_worker(move || {
        drain_bounded_until(
            stderr,
            MAX_STDERR_BYTES,
            &io_control,
            &stderr_output_limit,
            OutputLimit::Stderr,
        )
    });

    let status = match wait_until(&mut child, deadline, environment, &output_limit) {
        Ok(ChildWaitOutcome::Exited(status)) => status,
        Ok(ChildWaitOutcome::OutputLimitExceeded(limit)) => {
            let cleanup_error = combine_cleanup_errors(
                terminate_and_reap_bounded(process_tree, child),
                stop_pipe_workers_bounded(&input_worker, &stdout_worker, &stderr_worker, &io_stop),
            );
            return Err(limit.transport_error(cleanup_error));
        }
        Ok(ChildWaitOutcome::TimedOut) => {
            let cleanup_error = combine_cleanup_errors(
                terminate_and_reap_bounded(process_tree, child),
                stop_pipe_workers_bounded(&input_worker, &stdout_worker, &stderr_worker, &io_stop),
            );
            return Err(RemoteTransportError::Timeout {
                timeout,
                cleanup_error,
            });
        }
        Ok(ChildWaitOutcome::Cancelled) => {
            let cleanup_error = combine_cleanup_errors(
                terminate_and_reap_bounded(process_tree, child),
                stop_pipe_workers_bounded(&input_worker, &stdout_worker, &stderr_worker, &io_stop),
            );
            return Err(RemoteTransportError::Cancelled { cleanup_error });
        }
        Err(error) => {
            let cleanup_error = combine_cleanup_errors(
                terminate_and_reap_bounded(process_tree, child),
                stop_pipe_workers_bounded(&input_worker, &stdout_worker, &stderr_worker, &io_stop),
            );
            return Err(RemoteTransportError::Wait {
                error,
                cleanup_error,
            });
        }
    };

    // A successful primary process can still leave a ProxyCommand or other
    // configured descendant holding our pipes. Reap the tracked tree and use
    // the original end-to-end deadline for worker completion; no pipe holder
    // may turn a bounded probe into an unbounded join.
    if let Err(error) = process_tree.terminate(&mut child) {
        let cleanup_error =
            stop_pipe_workers_bounded(&input_worker, &stdout_worker, &stderr_worker, &io_stop);
        return Err(RemoteTransportError::ProcessCleanup {
            error,
            cleanup_error,
        });
    }
    if let Some(error) = reported_output_limit_error(
        &output_limit,
        &input_worker,
        &stdout_worker,
        &stderr_worker,
        &io_stop,
    ) {
        return Err(error);
    }
    let input_result = match receive_worker(&input_worker, deadline, timeout, "stdin") {
        Ok(result) => result,
        Err(error) => {
            if let Some(limit_error) = reported_output_limit_error(
                &output_limit,
                &input_worker,
                &stdout_worker,
                &stderr_worker,
                &io_stop,
            ) {
                return Err(limit_error);
            }
            return Err(error);
        }
    };
    if let Some(error) = reported_output_limit_error(
        &output_limit,
        &input_worker,
        &stdout_worker,
        &stderr_worker,
        &io_stop,
    ) {
        return Err(error);
    }
    if let Err(error) = input_result {
        if error.kind() == io::ErrorKind::TimedOut {
            return Err(RemoteTransportError::Timeout {
                timeout,
                cleanup_error: Some(error),
            });
        }
        return Err(RemoteTransportError::RequestWrite(error));
    }
    let stdout_result = match receive_worker(&stdout_worker, deadline, timeout, "stdout") {
        Ok(result) => result,
        Err(error) => {
            if let Some(limit_error) = reported_output_limit_error(
                &output_limit,
                &input_worker,
                &stdout_worker,
                &stderr_worker,
                &io_stop,
            ) {
                return Err(limit_error);
            }
            return Err(error);
        }
    };
    if let Some(error) = reported_output_limit_error(
        &output_limit,
        &input_worker,
        &stdout_worker,
        &stderr_worker,
        &io_stop,
    ) {
        return Err(error);
    }
    let stdout = match stdout_result {
        Ok(stdout) => stdout,
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            return Err(RemoteTransportError::Timeout {
                timeout,
                cleanup_error: Some(error),
            });
        }
        Err(error) => return Err(RemoteTransportError::OutputRead(error)),
    };
    let stderr_result = match receive_worker(&stderr_worker, deadline, timeout, "stderr") {
        Ok(result) => result,
        Err(error) => {
            if let Some(limit_error) = reported_output_limit_error(
                &output_limit,
                &input_worker,
                &stdout_worker,
                &stderr_worker,
                &io_stop,
            ) {
                return Err(limit_error);
            }
            return Err(error);
        }
    };
    if let Some(error) = reported_output_limit_error(
        &output_limit,
        &input_worker,
        &stdout_worker,
        &stderr_worker,
        &io_stop,
    ) {
        return Err(error);
    }
    let stderr = match stderr_result {
        Ok(stderr) => stderr,
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            return Err(RemoteTransportError::Timeout {
                timeout,
                cleanup_error: Some(error),
            });
        }
        Err(error) => return Err(RemoteTransportError::OutputRead(error)),
    };
    if stdout.exceeded {
        return Err(RemoteTransportError::StdoutLimitExceeded {
            cleanup_error: None,
        });
    }
    if stderr.exceeded {
        return Err(RemoteTransportError::StderrLimitExceeded {
            cleanup_error: None,
        });
    }
    ensure_success(status, &stderr.bytes)?;

    let response: RemoteExportResponse<D, F> = decode_remote_frame(&stdout.bytes, response_limits)?;
    let response_decoded_bytes = decoded_remote_frame_payload_len(&stdout.bytes)?;
    response.validate_for_request(request)?;
    Ok(RemoteExchangeReport {
        response,
        elapsed: started.elapsed(),
        request_bytes,
        response_bytes: stdout.bytes.len(),
        response_decoded_bytes,
        stderr_bytes: stderr.bytes.len(),
    })
}

fn probe_request(
    options: &RemoteProbeOptions,
) -> Result<RemoteExportRequest, RemoteTransportError> {
    let max_page_bytes = u32::try_from(options.max_response_bytes)
        .map_err(|_| RemoteTransportError::InvalidResponseLimit)?;
    Ok(RemoteExportRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        client_version: env!("CARGO_PKG_VERSION").parse()?,
        expected_source: options.expected_source.clone(),
        redaction_profile: options.redaction_profile,
        max_page_bytes,
        accepted_revisions: current_accepted_revisions(),
        request: RemoteExportRequestBody::Probe(ProbeRequest {
            check_state_writable: options.check_state_writable,
            check_rollout_readable: options.check_rollout_readable,
        }),
    })
}

fn default_ssh_program() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("ssh.exe")
    } else {
        PathBuf::from("ssh")
    }
}

fn resolve_program_from_path(program: &Path, path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path).find_map(|directory| {
        let candidate = directory.join(program);
        if is_executable_file(&candidate) {
            fs::canonicalize(candidate).ok()
        } else {
            None
        }
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn build_ssh_command(program: &Path, ssh_host: &str, agent_executable: &str) -> Command {
    let mut command = Command::new(program);
    command.args(SSH_OPTIONS);
    command.arg("--");
    command.arg(ssh_host);
    command.arg(agent_executable);
    command.args(SSH_REMOTE_AGENT_ARGUMENTS);
    command
}

fn validate_ssh_host(ssh_host: &str) -> Result<(), RemoteTransportError> {
    if ssh_host.is_empty() || ssh_host.len() > 255 {
        return Err(RemoteTransportError::InvalidHost(
            "alias must contain between 1 and 255 bytes".to_string(),
        ));
    }
    if ssh_host.starts_with('-')
        || ssh_host.chars().any(char::is_whitespace)
        || ssh_host.chars().any(char::is_control)
    {
        return Err(RemoteTransportError::InvalidHost(
            "alias must not begin with '-' or contain whitespace/control characters".to_string(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn configure_transport_pipes_nonblocking(
    stdin: &ChildStdin,
    stdout: &ChildStdout,
    stderr: &ChildStderr,
) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    for (label, descriptor) in [
        ("stdin", stdin.as_raw_fd()),
        ("stdout", stdout.as_raw_fd()),
        ("stderr", stderr.as_raw_fd()),
    ] {
        // SAFETY: each descriptor is owned by the corresponding live Child*
        // handle for the duration of both calls.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags == -1 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("could not inspect system SSH {label} pipe flags: {error}"),
            ));
        }
        // SAFETY: the descriptor remains live and F_SETFL only updates its
        // status flags. Preserve every existing flag while adding O_NONBLOCK.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("could not make system SSH {label} pipe interruptible: {error}"),
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_transport_pipes_nonblocking(
    _stdin: &ChildStdin,
    _stdout: &ChildStdout,
    _stderr: &ChildStderr,
) -> io::Result<()> {
    Ok(())
}

#[derive(Clone)]
struct IoWorkerControl {
    deadline: Instant,
    stop: Arc<AtomicBool>,
}

impl IoWorkerControl {
    fn check(&self, operation: &str) -> io::Result<()> {
        if self.stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("SSH {operation} worker stopped after process cleanup"),
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "SSH {operation} exceeded the transport deadline; an escaped ProxyCommand descendant may still hold an inherited pipe"
                ),
            ));
        }
        Ok(())
    }

    fn wait_to_retry(&self, operation: &str) -> io::Result<()> {
        self.check(operation)?;
        thread::sleep(
            PIPE_WORKER_POLL_INTERVAL.min(self.deadline.saturating_duration_since(Instant::now())),
        );
        self.check(operation)
    }
}

fn write_request(mut stdin: impl Write, frame: &[u8], control: &IoWorkerControl) -> io::Result<()> {
    let mut written = 0_usize;
    while written < frame.len() {
        control.check("stdin write")?;
        match stdin.write(&frame[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "could not write the complete remote request",
                ));
            }
            Ok(count) => written = written.saturating_add(count),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                control.wait_to_retry("stdin write")?;
            }
            Err(error) => return Err(error),
        }
    }
    stdin.flush()
}

struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum OutputLimit {
    Stdout = 1,
    Stderr = 2,
}

impl OutputLimit {
    const NONE: u8 = 0;

    fn load(signal: &AtomicU8) -> Option<Self> {
        match signal.load(Ordering::Acquire) {
            value if value == Self::Stdout as u8 => Some(Self::Stdout),
            value if value == Self::Stderr as u8 => Some(Self::Stderr),
            _ => None,
        }
    }

    fn report_first(self, signal: &AtomicU8) {
        let _ =
            signal.compare_exchange(Self::NONE, self as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    fn transport_error(self, cleanup_error: Option<io::Error>) -> RemoteTransportError {
        match self {
            Self::Stdout => RemoteTransportError::StdoutLimitExceeded { cleanup_error },
            Self::Stderr => RemoteTransportError::StderrLimitExceeded { cleanup_error },
        }
    }
}

fn drain_bounded_until(
    mut reader: impl Read,
    limit: usize,
    control: &IoWorkerControl,
    output_limit: &AtomicU8,
    stream: OutputLimit,
) -> io::Result<BoundedOutput> {
    let mut output = BoundedOutput {
        bytes: Vec::with_capacity(limit.min(64 * 1024)),
        exceeded: false,
    };
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        control.check("pipe drain")?;
        let remaining = limit.saturating_sub(output.bytes.len());
        // Read only enough to fill the retained prefix plus one sentinel byte.
        // This makes the transport discover overflow without consuming an
        // otherwise-discarded 16 KiB chunk beyond the negotiated reservation.
        let read_limit = remaining.saturating_add(1).min(buffer.len());
        let read = match reader.read(&mut buffer[..read_limit]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                control.wait_to_retry("pipe drain")?;
                continue;
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(output);
        }
        let retained = remaining.min(read);
        output.bytes.extend_from_slice(&buffer[..retained]);
        if retained != read {
            output.exceeded = true;
            stream.report_first(output_limit);
            // Stop the sibling pipe workers immediately as well. The parent
            // wait observes `output_limit` and tears down the owned process
            // tree; this flag prevents Unix's nonblocking stdin/other-output
            // loops from consuming any more data in the meantime.
            control.stop.store(true, Ordering::Release);
            return Ok(output);
        }
    }
}

#[cfg(test)]
fn drain_bounded(reader: impl Read, limit: usize) -> io::Result<BoundedOutput> {
    let output_limit = AtomicU8::new(OutputLimit::NONE);
    drain_bounded_until(
        reader,
        limit,
        &IoWorkerControl {
            deadline: Instant::now() + Duration::from_secs(1),
            stop: Arc::new(AtomicBool::new(false)),
        },
        &output_limit,
        OutputLimit::Stdout,
    )
}

enum ChildWaitOutcome {
    Exited(ExitStatus),
    OutputLimitExceeded(OutputLimit),
    TimedOut,
    Cancelled,
}

fn wait_until(
    child: &mut Child,
    deadline: Instant,
    environment: &SshCommandEnvironment,
    output_limit: &AtomicU8,
) -> io::Result<ChildWaitOutcome> {
    loop {
        if let Some(limit) = OutputLimit::load(output_limit) {
            return Ok(ChildWaitOutcome::OutputLimitExceeded(limit));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(ChildWaitOutcome::Exited(status));
        }
        if environment.cancellation_requested() {
            return Ok(ChildWaitOutcome::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(ChildWaitOutcome::TimedOut);
        }
        thread::sleep(WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn spawn_worker<T, F>(operation: F) -> mpsc::Receiver<io::Result<T>>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(operation());
    });
    receiver
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PipeWorkerCleanupState {
    Complete,
    DeadlineResidual,
    Pending,
}

fn worker_cleanup_state_by<T>(
    worker: &mpsc::Receiver<io::Result<T>>,
    deadline: Instant,
) -> PipeWorkerCleanupState {
    match worker.try_recv() {
        Ok(Err(error)) if error.kind() == io::ErrorKind::TimedOut => {
            return PipeWorkerCleanupState::DeadlineResidual;
        }
        Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {
            return PipeWorkerCleanupState::Complete;
        }
        Err(mpsc::TryRecvError::Empty) => {}
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return PipeWorkerCleanupState::Pending;
    }
    match worker.recv_timeout(remaining) {
        Ok(Err(error)) if error.kind() == io::ErrorKind::TimedOut => {
            PipeWorkerCleanupState::DeadlineResidual
        }
        Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => PipeWorkerCleanupState::Complete,
        Err(mpsc::RecvTimeoutError::Timeout) => PipeWorkerCleanupState::Pending,
    }
}

/// Gives ordinary process-group/Job termination a short chance to close every
/// inherited pipe. If that does not happen, interruptible Unix workers are
/// stopped explicitly and every platform returns a durable diagnostic instead
/// of silently treating a possible escaped ProxyCommand as fully contained.
fn stop_pipe_workers_bounded<I, O, E>(
    input_worker: &mpsc::Receiver<io::Result<I>>,
    stdout_worker: &mpsc::Receiver<io::Result<O>>,
    stderr_worker: &mpsc::Receiver<io::Result<E>>,
    stop: &Arc<AtomicBool>,
) -> Option<io::Error> {
    let eof_deadline = Instant::now() + PIPE_CLEANUP_EOF_GRACE;
    let mut input_state = worker_cleanup_state_by(input_worker, eof_deadline);
    let mut stdout_state = worker_cleanup_state_by(stdout_worker, eof_deadline);
    let mut stderr_state = worker_cleanup_state_by(stderr_worker, eof_deadline);
    let states = [input_state, stdout_state, stderr_state];
    if states
        .iter()
        .all(|state| *state != PipeWorkerCleanupState::Pending)
    {
        return states
            .contains(&PipeWorkerCleanupState::DeadlineResidual)
            .then(|| {
                io::Error::other(
                    "system SSH pipes reached the transport deadline during process cleanup; an escaped ProxyCommand descendant may still be running",
                )
            });
    }

    stop.store(true, Ordering::Release);
    let stop_deadline = Instant::now() + PIPE_CLEANUP_STOP_GRACE;
    if input_state == PipeWorkerCleanupState::Pending {
        input_state = worker_cleanup_state_by(input_worker, stop_deadline);
    }
    if stdout_state == PipeWorkerCleanupState::Pending {
        stdout_state = worker_cleanup_state_by(stdout_worker, stop_deadline);
    }
    if stderr_state == PipeWorkerCleanupState::Pending {
        stderr_state = worker_cleanup_state_by(stderr_worker, stop_deadline);
    }
    let detail = if [input_state, stdout_state, stderr_state]
        .iter()
        .all(|state| *state != PipeWorkerCleanupState::Pending)
    {
        "system SSH pipes remained open after its isolated process tree was terminated; an escaped ProxyCommand descendant may still be running"
    } else {
        "system SSH pipe workers did not stop within the bounded cleanup grace; an escaped ProxyCommand descendant may still be running and holding inherited pipes"
    };
    Some(io::Error::other(detail))
}

fn reported_output_limit_error<I, O, E>(
    output_limit: &AtomicU8,
    input_worker: &mpsc::Receiver<io::Result<I>>,
    stdout_worker: &mpsc::Receiver<io::Result<O>>,
    stderr_worker: &mpsc::Receiver<io::Result<E>>,
    stop: &Arc<AtomicBool>,
) -> Option<RemoteTransportError> {
    let limit = OutputLimit::load(output_limit)?;
    let cleanup_error = stop_pipe_workers_bounded(input_worker, stdout_worker, stderr_worker, stop);
    Some(limit.transport_error(cleanup_error))
}

fn receive_worker<T>(
    worker: &mpsc::Receiver<io::Result<T>>,
    deadline: Instant,
    timeout: Duration,
    stream: &'static str,
) -> Result<io::Result<T>, RemoteTransportError> {
    match worker.try_recv() {
        Ok(result) => return Ok(result),
        Err(mpsc::TryRecvError::Disconnected) => {
            return Err(RemoteTransportError::WorkerPanicked);
        }
        Err(mpsc::TryRecvError::Empty) => {}
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(RemoteTransportError::Timeout {
            timeout,
            cleanup_error: Some(pipe_drain_residual_error(stream)),
        });
    }
    match worker.recv_timeout(remaining) {
        Ok(result) => Ok(result),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(RemoteTransportError::Timeout {
            timeout,
            cleanup_error: Some(pipe_drain_residual_error(stream)),
        }),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(RemoteTransportError::WorkerPanicked),
    }
}

fn pipe_drain_residual_error(stream: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "system SSH {stream} did not close before the transport deadline; an escaped ProxyCommand descendant may still be running"
        ),
    )
}

/// Keeps process termination itself non-blocking. Callers may additionally use
/// the small, fixed pipe-cleanup grace above to distinguish normal EOF from a
/// ProxyCommand descendant which escaped the owned process group/Job. Reaping
/// an unusually stuck primary child is handed to a detached worker instead of
/// calling unbounded `Child::wait` here.
fn terminate_and_reap_bounded(
    mut process_tree: ProcessTree,
    mut child: Child,
) -> Option<io::Error> {
    let terminate_error = process_tree.terminate(&mut child).err();
    let reap_error = reap_child_bounded(child).err();
    combine_cleanup_errors(terminate_error, reap_error)
}

fn kill_and_reap_bounded(mut child: Child) -> Option<io::Error> {
    let kill_error = match child.try_wait() {
        Ok(Some(_)) => None,
        Ok(None) => child.kill().err(),
        Err(error) => Some(error),
    };
    let reap_error = reap_child_bounded(child).err();
    combine_cleanup_errors(kill_error, reap_error)
}

fn reap_child_bounded(mut child: Child) -> io::Result<()> {
    match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            thread::Builder::new()
                .name("codex-usage-monit-ssh-reaper".to_owned())
                .spawn(move || {
                    let _ = child.wait();
                })?;
            Ok(())
        }
        Err(error) => {
            let kind = error.kind();
            let message = error.to_string();
            let spawn_result = thread::Builder::new()
                .name("codex-usage-monit-ssh-reaper".to_owned())
                .spawn(move || {
                    let _ = child.wait();
                });
            match spawn_result {
                Ok(_) => Err(error),
                Err(spawn_error) => Err(io::Error::new(
                    kind,
                    format!(
                        "could not poll child before background reaping: {message}; could not start reaper: {spawn_error}"
                    ),
                )),
            }
        }
    }
}

fn combine_cleanup_errors(
    first: Option<io::Error>,
    second: Option<io::Error>,
) -> Option<io::Error> {
    match (first, second) {
        (None, None) => None,
        (Some(error), None) | (None, Some(error)) => Some(error),
        (Some(first), Some(second)) => Some(io::Error::new(
            first.kind(),
            format!("{first}; background reaping also failed: {second}"),
        )),
    }
}

fn ensure_success(status: ExitStatus, stderr: &[u8]) -> Result<(), RemoteTransportError> {
    if status.success() {
        return Ok(());
    }
    Err(RemoteTransportError::ExitFailure {
        code: status.code(),
        diagnostic: sanitize_diagnostic(stderr),
    })
}

fn sanitize_diagnostic(bytes: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    let mut output = String::new();
    for character in decoded.chars() {
        if output.chars().count() >= MAX_DIAGNOSTIC_CHARS {
            break;
        }
        if character.is_control() {
            if matches!(character, '\n' | '\r' | '\t') && !output.ends_with(' ') {
                output.push(' ');
            }
            continue;
        }
        output.push(character);
    }
    output.trim().to_string()
}

#[cfg(unix)]
enum ProcessTree {
    OwnedProcessGroup(libc::pid_t),
    Inherited,
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command, owned: bool) {
    use std::os::unix::process::CommandExt;
    if owned {
        command.process_group(0);
    }
}

#[cfg(unix)]
fn attach_process_tree(child: &mut Child, owned: bool) -> io::Result<ProcessTree> {
    Ok(if owned {
        ProcessTree::OwnedProcessGroup(child.id() as libc::pid_t)
    } else {
        ProcessTree::Inherited
    })
}

#[cfg(unix)]
impl ProcessTree {
    fn terminate(&mut self, child: &mut Child) -> io::Result<()> {
        let process_tree = std::mem::replace(self, Self::Inherited);
        let Self::OwnedProcessGroup(process_group) = process_tree else {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            return child.kill();
        };
        // The child is created as the leader of a fresh process group above.
        if unsafe { libc::kill(-process_group, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let group_error = io::Error::last_os_error();
        if group_error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        match child.kill() {
            Ok(()) => Err(io::Error::new(
                group_error.kind(),
                format!(
                    "could not terminate SSH process group: {group_error}; the primary child was terminated separately"
                ),
            )),
            Err(child_error) => Err(io::Error::new(
                group_error.kind(),
                format!(
                    "could not terminate SSH process group: {group_error}; could not terminate primary child: {child_error}"
                ),
            )),
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        let Self::OwnedProcessGroup(process_group) = self else {
            return;
        };
        // This is an unwind/error-path backstop. Normal completion consumes
        // the owned group in `terminate`, so a recycled PID cannot be hit by
        // a later Drop.
        let _ = unsafe { libc::kill(-*process_group, libc::SIGKILL) };
    }
}

#[cfg(windows)]
enum ProcessTree {
    OwnedJob(HANDLE),
    Inherited,
}

#[cfg(windows)]
fn configure_process_tree(command: &mut Command, owned: bool) {
    if owned {
        command.creation_flags(CREATE_SUSPENDED);
    }
}

#[cfg(windows)]
fn attach_process_tree(child: &mut Child, owned: bool) -> io::Result<ProcessTree> {
    if !owned {
        return Ok(ProcessTree::Inherited);
    }
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
        unsafe { CloseHandle(job) };
        return Err(error);
    }
    if unsafe { AssignProcessToJobObject(job, child.as_raw_handle().cast()) } == 0 {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(error);
    }
    let process_tree = ProcessTree::OwnedJob(job);
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
        let mut resumed = 0_usize;
        while has_entry {
            if entry.th32OwnerProcessID == child.id() {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let result = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if result == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed = resumed.saturating_add(1);
            }
            has_entry = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        if resumed == 0 {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "could not find the suspended system SSH primary thread",
            ))
        } else {
            Ok(())
        }
    })();
    unsafe { CloseHandle(snapshot) };
    result
}

#[cfg(windows)]
impl ProcessTree {
    fn terminate(&mut self, child: &mut Child) -> io::Result<()> {
        let mut cleanup_error = None;
        if let Self::OwnedJob(job) = self
            && !job.is_null()
        {
            if unsafe { TerminateJobObject(*job, 1) } == 0 {
                cleanup_error = Some(io::Error::last_os_error());
            }
            if unsafe { CloseHandle(*job) } == 0 {
                cleanup_error =
                    combine_cleanup_errors(cleanup_error, Some(io::Error::last_os_error()));
            }
            *job = std::ptr::null_mut();
        }
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                cleanup_error = combine_cleanup_errors(cleanup_error, child.kill().err());
            }
            Err(error) => {
                cleanup_error = combine_cleanup_errors(cleanup_error, Some(error));
            }
        }
        cleanup_error.map_or(Ok(()), Err)
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Self::OwnedJob(job) = self
            && !job.is_null()
        {
            unsafe { CloseHandle(*job) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::num::NonZeroU64;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    use chrono::Utc;

    use super::*;

    #[test]
    fn ssh_argv_is_fixed_and_dynamic_host_is_one_argument() {
        let command = build_ssh_command(
            Path::new("ssh-test"),
            "dev-server",
            DEFAULT_REMOTE_AGENT_EXECUTABLE,
        );
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();
        let mut expected = SSH_OPTIONS.iter().map(OsString::from).collect::<Vec<_>>();
        expected.extend(
            [
                "--",
                "dev-server",
                "codex-usage-monit",
                "remote-agent",
                "export",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(args, expected);
    }

    #[test]
    fn custom_unix_agent_executable_is_one_exact_ssh_argument() {
        let command = build_ssh_command(
            Path::new("ssh-test"),
            "dev-server",
            "~/.local/bin/codex-usage-monit",
        );
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();
        let mut expected = SSH_OPTIONS.iter().map(OsString::from).collect::<Vec<_>>();
        expected.extend(
            [
                "--",
                "dev-server",
                "~/.local/bin/codex-usage-monit",
                "remote-agent",
                "export",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(args, expected);
    }

    #[test]
    fn custom_windows_agent_executable_is_one_exact_ssh_argument() {
        let command = build_ssh_command(
            Path::new("ssh-test"),
            "windows-server",
            "C:/Users/codex/bin/codex-usage-monit.exe",
        );
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();
        let mut expected = SSH_OPTIONS.iter().map(OsString::from).collect::<Vec<_>>();
        expected.extend(
            [
                "--",
                "windows-server",
                "C:/Users/codex/bin/codex-usage-monit.exe",
                "remote-agent",
                "export",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert_eq!(args, expected);
    }

    #[cfg(windows)]
    #[test]
    fn process_level_remote_job_is_installed_once_and_contains_this_process() {
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        ensure_current_process_remote_containment().unwrap();
        ensure_current_process_remote_containment().unwrap();
        let mut in_job = 0;
        assert_ne!(
            unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) },
            0
        );
        assert_ne!(in_job, 0);
    }

    #[test]
    fn saved_path_selects_ssh_and_is_propagated_for_configured_helpers() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("saved service bin");
        std::fs::create_dir(&bin).unwrap();
        let ssh = bin.join(default_ssh_program());
        let helper = bin.join(if cfg!(windows) {
            "proxy-helper.exe"
        } else {
            "proxy-helper"
        });
        std::fs::write(&ssh, b"ssh fixture").unwrap();
        std::fs::write(&helper, b"helper fixture").unwrap();
        #[cfg(unix)]
        for path in [&ssh, &helper] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let saved_path = env::join_paths([&bin]).unwrap();
        let environment = SshCommandEnvironment::new(Some(saved_path.clone()));

        let selected_ssh = environment.resolve_program().unwrap();
        let selected_helper = resolve_program_from_path(
            Path::new(if cfg!(windows) {
                "proxy-helper.exe"
            } else {
                "proxy-helper"
            }),
            &saved_path,
        )
        .unwrap();
        let mut command =
            build_ssh_command(&selected_ssh, "dev-server", DEFAULT_REMOTE_AGENT_EXECUTABLE);
        environment.apply(&mut command);

        assert_eq!(selected_ssh, std::fs::canonicalize(ssh).unwrap());
        assert_eq!(selected_helper, std::fs::canonicalize(helper).unwrap());
        assert_eq!(command.get_program(), selected_ssh.as_os_str());
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new("PATH"))
                .and_then(|(_, value)| value),
            Some(saved_path.as_os_str())
        );
    }

    #[test]
    fn inherited_environment_does_not_override_the_child_path() {
        let environment = SshCommandEnvironment::default();
        let program = environment.resolve_program().unwrap();
        let mut command =
            build_ssh_command(&program, "dev-server", DEFAULT_REMOTE_AGENT_EXECUTABLE);
        environment.apply(&mut command);

        assert_eq!(program, default_ssh_program());
        assert!(command.get_envs().all(|(key, _)| key != OsStr::new("PATH")));
    }

    #[test]
    fn tui_helper_environment_does_not_create_a_nested_process_tree() {
        assert!(SshCommandEnvironment::new(None).owns_process_tree());
        assert!(!SshCommandEnvironment::inheriting_parent_process_tree(None).owns_process_tree());

        let mut command = Command::new("ssh");
        SshCommandEnvironment::inheriting_parent_process_tree(None).apply(&mut command);
        let removals = command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<Vec<_>>();
        assert!(removals.contains(&OsStr::new(TUI_PROCESS_TREE_TOKEN_ENV)));
        assert!(removals.contains(&OsStr::new(TUI_PROCESS_TREE_PARENT_ENV)));
    }

    #[test]
    fn saved_path_never_falls_back_to_the_recorders_current_path() {
        let directory = tempfile::tempdir().unwrap();
        let saved_path = env::join_paths([directory.path()]).unwrap();
        let environment = SshCommandEnvironment::new(Some(saved_path));

        let error = environment.resolve_program().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            !error
                .to_string()
                .contains(&directory.path().display().to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_child_path_resolves_a_proxy_command_helper() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("saved service bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("ssh.exe"), b"ssh fixture").unwrap();
        std::fs::write(bin.join("proxy-helper.exe"), b"helper fixture").unwrap();
        let saved_path = env::join_paths([&bin]).unwrap();
        let environment = SshCommandEnvironment::new(Some(saved_path));
        let selected_ssh = environment.resolve_program().unwrap();
        let mut ssh_command =
            build_ssh_command(&selected_ssh, "dev-server", DEFAULT_REMOTE_AGENT_EXECUTABLE);
        environment.apply(&mut ssh_command);
        let child_path = ssh_command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("PATH"))
            .and_then(|(_, value)| value)
            .unwrap();
        let where_exe = PathBuf::from(env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("where.exe");

        let output = Command::new(where_exe)
            .arg("proxy-helper.exe")
            .env("PATH", child_path)
            .output()
            .unwrap();

        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }

    #[test]
    fn hostile_host_never_reaches_argv() {
        for host in ["", "-oProxyCommand=bad", "host name", "host\nname"] {
            assert!(matches!(
                validate_ssh_host(host),
                Err(RemoteTransportError::InvalidHost(_))
            ));
        }

        let directory = tempfile::tempdir().unwrap();
        let environment =
            SshCommandEnvironment::new(Some(env::join_paths([directory.path()]).unwrap()));
        assert!(matches!(
            probe_remote_with_environment(
                "-oProxyCommand=bad",
                &RemoteProbeOptions::default(),
                &environment,
            ),
            Err(RemoteTransportError::InvalidHost(_))
        ));
    }

    #[test]
    fn bounded_drain_stops_after_the_first_oversized_read() {
        struct OneChunkThenPanic(bool);

        impl Read for OneChunkThenPanic {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert!(!self.0, "bounded drain continued reading after overflow");
                self.0 = true;
                assert_eq!(buffer.len(), 5, "drain must request limit plus one byte");
                buffer.copy_from_slice(b"01234");
                Ok(buffer.len())
            }
        }

        let output = drain_bounded(OneChunkThenPanic(false), 4).unwrap();
        assert_eq!(output.bytes, b"0123");
        assert!(output.exceeded);
    }

    #[test]
    fn worker_completion_is_bounded_by_the_transport_deadline() {
        let started = Instant::now();
        let worker = spawn_worker(|| {
            thread::sleep(Duration::from_millis(250));
            Ok(())
        });
        let error = receive_worker(
            &worker,
            started + Duration::from_millis(40),
            Duration::from_millis(40),
            "test",
        )
        .unwrap_err();
        assert!(matches!(error, RemoteTransportError::Timeout { .. }));
        assert!(error.process_containment_uncertain());
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[cfg(unix)]
    #[test]
    fn running_child_is_reaped_without_blocking_the_caller() {
        let child = Command::new("sh")
            .args(["-c", "sleep 0.5"])
            .spawn()
            .unwrap();
        let started = Instant::now();

        reap_child_bounded(child).unwrap();

        assert!(
            started.elapsed() < Duration::from_millis(200),
            "background reaping must not wait for the child to exit"
        );
    }

    #[test]
    fn stderr_diagnostic_is_terminal_safe_and_bounded() {
        let input = format!("bad\x1b[31m\n{}", "x".repeat(700));
        let diagnostic = sanitize_diagnostic(input.as_bytes());
        assert!(!diagnostic.as_bytes().contains(&0x1b));
        assert!(diagnostic.chars().count() <= MAX_DIAGNOSTIC_CHARS);
        assert!(!diagnostic.contains('\n'));
    }

    #[test]
    fn probe_request_does_not_scan_or_mutate_configuration() {
        let options = RemoteProbeOptions::default();
        let request = probe_request(&options).unwrap();
        assert!(request.expected_source.is_none());
        assert!(matches!(
            request.request,
            RemoteExportRequestBody::Probe(ProbeRequest {
                check_state_writable: true,
                check_rollout_readable: true
            })
        ));
    }

    #[test]
    fn probe_request_uses_the_explicit_fixed_response_bound() {
        let options = RemoteProbeOptions {
            max_response_bytes: crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES,
            ..RemoteProbeOptions::default()
        };
        let request = probe_request(&options).unwrap();
        assert_eq!(
            request.max_page_bytes,
            crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES as u32
        );

        let invalid = RemoteProbeOptions {
            max_response_bytes: usize::MAX,
            ..RemoteProbeOptions::default()
        };
        assert!(matches!(
            probe_request(&invalid),
            Err(RemoteTransportError::InvalidResponseLimit)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn one_shot_fake_ssh_round_trip_is_framed_and_bounded() {
        use crate::remote_agent::current_revisions;
        use crate::remote_protocol::{
            ProbeResult, RemoteCapability, RemoteTiming, SourceGeneration,
        };

        let directory = tempfile::tempdir().unwrap();
        let response_path = directory.path().join("response.frame");
        let script_path = directory.path().join("fake-ssh");
        let now = Utc::now();
        let response = RemoteProbeResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").parse().unwrap(),
            source: SourceGeneration {
                node_id: "node-11111111111111111111111111111111".parse().unwrap(),
                generation: NonZeroU64::new(1).unwrap(),
            },
            redaction_profile: RedactionProfile::Redacted,
            revisions: current_revisions(),
            observed_at: now,
            timing: RemoteTiming {
                remote_received_at: now,
                remote_sent_at: now,
            },
            result: RemoteExportResponseBody::Probe(ProbeResult {
                capabilities: vec![RemoteCapability::GzipFrame],
                state_writable: true,
                rollout_readable: true,
            }),
        };
        fs::write(
            &response_path,
            encode_remote_frame(&response, RemoteFrameLimits::default()).unwrap(),
        )
        .unwrap();
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\ncat '{}'\n",
                response_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();

        let report =
            probe_remote_with_program(script_path, "dev-server", &RemoteProbeOptions::default())
                .unwrap();
        assert_eq!(report.response.source, response.source);
        assert!(report.request_bytes > 20);
        assert!(report.response_bytes > 20);
        assert_eq!(report.stderr_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn saved_path_launches_ssh_and_resolves_its_proxy_helper() {
        use crate::remote_agent::current_revisions;
        use crate::remote_protocol::{
            ProbeResult, RemoteCapability, RemoteTiming, SourceGeneration,
        };

        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("service-bin");
        fs::create_dir(&bin).unwrap();
        let response_path = directory.path().join("response.frame");
        let ssh_path = bin.join("ssh");
        let helper_path = bin.join("ssh-proxy-helper");
        let now = Utc::now();
        let response = RemoteProbeResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").parse().unwrap(),
            source: SourceGeneration {
                node_id: "node-11111111111111111111111111111111".parse().unwrap(),
                generation: NonZeroU64::new(1).unwrap(),
            },
            redaction_profile: RedactionProfile::Redacted,
            revisions: current_revisions(),
            observed_at: now,
            timing: RemoteTiming {
                remote_received_at: now,
                remote_sent_at: now,
            },
            result: RemoteExportResponseBody::Probe(ProbeResult {
                capabilities: vec![RemoteCapability::GzipFrame],
                state_writable: true,
                rollout_readable: true,
            }),
        };
        fs::write(
            &response_path,
            encode_remote_frame(&response, RemoteFrameLimits::default()).unwrap(),
        )
        .unwrap();
        fs::write(&ssh_path, "#!/bin/sh\nexec ssh-proxy-helper \"$@\"\n").unwrap();
        fs::write(
            &helper_path,
            format!(
                "#!/bin/sh\n/bin/cat >/dev/null\n/bin/cat '{}'\n",
                response_path.display()
            ),
        )
        .unwrap();
        for path in [&ssh_path, &helper_path] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let environment = SshCommandEnvironment::new(Some(env::join_paths([&bin]).unwrap()));

        let report = probe_remote_with_environment(
            "dev-server",
            &RemoteProbeOptions::default(),
            &environment,
        )
        .unwrap();

        assert_eq!(report.response.source, response.source);
        assert!(report.request_bytes > 20);
        assert!(report.response_bytes > 20);
        assert_eq!(report.stderr_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn generic_exchange_accepts_a_typed_delta_page() {
        use chrono::Duration as ChronoDuration;

        use crate::remote_agent::{current_accepted_revisions, current_revisions};
        use crate::remote_protocol::{
            DeltaCursor, DeltaPage, DeltaPayload, DeltaRequest, ExportRange, RemoteDeltaCoverage,
            RemoteDeltaResponse, RemoteDeltaStats, RemoteProtocolErrorKind, RemoteTiming,
        };

        let directory = tempfile::tempdir().unwrap();
        let response_path = directory.path().join("delta-response.frame");
        let script_path = directory.path().join("fake-ssh-delta");
        let observed_at = Utc::now();
        let source = SourceGeneration {
            node_id: "node-22222222222222222222222222222222".parse().unwrap(),
            generation: NonZeroU64::new(2).unwrap(),
        };
        let range = ExportRange {
            from: observed_at - ChronoDuration::hours(1),
            to: observed_at,
        };
        let request = RemoteExportRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").parse().unwrap(),
            expected_source: Some(source.clone()),
            redaction_profile: RedactionProfile::Redacted,
            max_page_bytes: RemoteFrameLimits::default().max_encoded_bytes as u32,
            accepted_revisions: current_accepted_revisions(),
            request: RemoteExportRequestBody::Delta(DeltaRequest {
                delta_cursor: None,
                range: range.clone(),
                overlap_minutes: 0,
                include_live: false,
                known_live_revision: None,
            }),
        };
        let response = RemoteDeltaResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").parse().unwrap(),
            source,
            redaction_profile: RedactionProfile::Redacted,
            revisions: current_revisions(),
            observed_at,
            timing: RemoteTiming {
                remote_received_at: observed_at,
                remote_sent_at: observed_at,
            },
            result: RemoteExportResponseBody::Delta {
                page: DeltaPage {
                    generation: NonZeroU64::new(1).unwrap(),
                    from_sequence: 0,
                    through_sequence: 0,
                    next_delta_cursor: DeltaCursor {
                        generation: NonZeroU64::new(1).unwrap(),
                        sequence: 0,
                    },
                    has_more: false,
                },
                payload: DeltaPayload {
                    coverage: RemoteDeltaCoverage {
                        requested_range: range,
                        covered_range: None,
                        range_complete: false,
                        partial_reasons: vec!["historical_coverage_unproven".to_owned()],
                    },
                    project_descriptors: Vec::new(),
                    bucket_changes: Vec::new(),
                    session_digest_changes: Vec::new(),
                    live: None,
                    stats: RemoteDeltaStats::default(),
                    warnings: Vec::new(),
                },
            },
        };
        fs::write(
            &response_path,
            encode_remote_frame(&response, RemoteFrameLimits::default()).unwrap(),
        )
        .unwrap();
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\ncat '{}'\n",
                response_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();

        let report: RemoteExchangeReport<DeltaPayload, EmptyRemotePayload> =
            exchange_remote_with_program(
                script_path.clone(),
                "dev-server",
                &request,
                Duration::from_secs(2),
            )
            .unwrap();

        assert!(matches!(
            report.response.result,
            RemoteExportResponseBody::Delta { .. }
        ));
        assert!(report.request_bytes > 20);
        assert!(report.response_bytes > 20);

        let decoded_limit = report.response_decoded_bytes.saturating_sub(1);
        let error: RemoteTransportError =
            exchange_remote_with_program_and_limits::<DeltaPayload, EmptyRemotePayload>(
                script_path,
                "dev-server",
                DEFAULT_REMOTE_AGENT_EXECUTABLE,
                &request,
                Duration::from_secs(2),
                RemoteFrameLimits {
                    max_decoded_bytes: decoded_limit,
                    identity_threshold_bytes: decoded_limit,
                    ..RemoteFrameLimits::default()
                },
                &SshCommandEnvironment::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            RemoteTransportError::Protocol(error)
                if error.kind() == RemoteProtocolErrorKind::DecodedLimitExceeded
        ));
    }

    #[cfg(unix)]
    fn assert_process_exits_bounded(pid_path: &Path, subject: &str) {
        let pid = fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive || Instant::now() >= deadline {
                assert!(!alive, "{subject} remained alive after output overflow");
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn continuous_stdout_is_killed_at_the_negotiated_frame_limit() {
        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-ssh-stdout-overflow");
        let descendant_pid_path = directory.path().join("stdout-descendant.pid");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nsleep 30 &\necho $! > '{}'\nwhile :; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; done\n",
                descendant_pid_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        let options = RemoteProbeOptions {
            timeout: Duration::from_secs(5),
            max_response_bytes: crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES,
            ..RemoteProbeOptions::default()
        };
        let started = Instant::now();

        let error = probe_remote_with_program(script_path, "dev-server", &options).unwrap_err();

        assert!(matches!(
            error,
            RemoteTransportError::StdoutLimitExceeded { .. }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stdout overflow must not wait for the five-second exchange timeout"
        );
        assert_process_exits_bounded(&descendant_pid_path, "stdout writer descendant");
    }

    #[cfg(unix)]
    #[test]
    fn stdout_limit_wins_when_fake_ssh_exits_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-ssh-fast-stdout-overflow");
        fs::write(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\ni=0\nwhile [ \"$i\" -lt 200 ]; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; i=$((i + 1)); done\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        let options = RemoteProbeOptions {
            timeout: Duration::from_secs(5),
            max_response_bytes: crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES,
            ..RemoteProbeOptions::default()
        };

        let error = probe_remote_with_program(script_path, "dev-server", &options).unwrap_err();

        assert!(matches!(
            error,
            RemoteTransportError::StdoutLimitExceeded { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn continuous_stderr_is_killed_at_the_diagnostic_limit() {
        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-ssh-stderr-overflow");
        let descendant_pid_path = directory.path().join("stderr-descendant.pid");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nsleep 30 &\necho $! > '{}'\nwhile :; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' >&2; done\n",
                descendant_pid_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        let options = RemoteProbeOptions {
            timeout: Duration::from_secs(5),
            ..RemoteProbeOptions::default()
        };
        let request = probe_request(&options).unwrap();
        let started = Instant::now();

        let error = exchange_remote_with_program::<EmptyRemotePayload, EmptyRemotePayload>(
            script_path,
            "dev-server",
            &request,
            options.timeout,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RemoteTransportError::StderrLimitExceeded { .. }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stderr overflow must not wait for the five-second exchange timeout"
        );
        assert_process_exits_bounded(&descendant_pid_path, "stderr writer descendant");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_the_fake_ssh_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-ssh-hang");
        fs::write(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\nsleep 30 &\nwait\n",
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        let options = RemoteProbeOptions {
            timeout: Duration::from_millis(100),
            ..RemoteProbeOptions::default()
        };
        let started = Instant::now();
        let error = probe_remote_with_program(script_path, "dev-server", &options).unwrap_err();
        assert!(matches!(error, RemoteTransportError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_the_fake_ssh_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-ssh-cancel");
        let descendant_pid_path = directory.path().join("descendant.pid");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nsleep 30 &\necho $! > '{}'\nwait\n",
                descendant_pid_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancellation_worker = Arc::clone(&cancellation);
        let pid_path = descendant_pid_path.clone();
        let trigger = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !pid_path.is_file() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            cancellation_worker.store(true, Ordering::Release);
        });
        let environment =
            SshCommandEnvironment::default().with_cancellation(Arc::clone(&cancellation));
        let options = RemoteProbeOptions {
            timeout: Duration::from_secs(5),
            ..RemoteProbeOptions::default()
        };
        let started = Instant::now();

        let error = probe_remote_with_program_and_environment(
            script_path,
            "dev-server",
            &options,
            &environment,
        )
        .unwrap_err();
        trigger.join().unwrap();

        assert!(matches!(error, RemoteTransportError::Cancelled { .. }));
        assert!(!error.process_containment_uncertain());
        assert!(started.elapsed() < Duration::from_secs(2));
        let descendant_pid = fs::read_to_string(&descendant_pid_path)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = unsafe { libc::kill(descendant_pid, 0) } == 0;
            if !alive || Instant::now() >= deadline {
                assert!(!alive, "cancelled SSH descendant remained alive");
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn escaped_pipe_holder_cannot_extend_the_transport_deadline() {
        if !Command::new("perl")
            .args(["-e", "exit 0"])
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-ssh-escaped-holder");
        let holder_pid_path = directory.path().join("holder.pid");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nperl -MPOSIX -e 'POSIX::setsid(); open(my $f, q(>), q({})); print $f \"$$\\n\"; close($f); sleep 5' &\nwhile [ ! -s '{}' ]; do sleep 0.01; done\nexit 0\n",
                holder_pid_path.display(),
                holder_pid_path.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        let options = RemoteProbeOptions {
            timeout: Duration::from_millis(100),
            ..RemoteProbeOptions::default()
        };
        let started = Instant::now();

        let error = probe_remote_with_program(script_path, "dev-server", &options).unwrap_err();

        let holder_pid = fs::read_to_string(&holder_pid_path)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        unsafe {
            libc::kill(holder_pid, libc::SIGKILL);
        }
        assert!(matches!(error, RemoteTransportError::Timeout { .. }));
        assert!(error.process_containment_uncertain());
        assert!(
            error
                .to_string()
                .contains("escaped ProxyCommand descendant"),
            "escaped holder was not surfaced in the timeout diagnostic: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an escaped descendant holding stdout/stderr must not extend the deadline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_bounds_escaped_proxy_pipe_workers_and_reports_the_residual() {
        if !Command::new("perl")
            .args(["-e", "exit 0"])
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-ssh-cancel-escaped-holder");
        let holder_pid_path = directory.path().join("holder.pid");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nperl -MPOSIX -e 'POSIX::setsid(); open(my $f, q(>), q({})); print $f \"$$\\n\"; close($f); sleep 30' &\nwhile [ ! -s '{}' ]; do sleep 0.01; done\nwait\n",
                holder_pid_path.display(),
                holder_pid_path.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let trigger_cancellation = Arc::clone(&cancellation);
        let pid_path = holder_pid_path.clone();
        let trigger = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !pid_path.is_file() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            trigger_cancellation.store(true, Ordering::Release);
        });
        let environment =
            SshCommandEnvironment::default().with_cancellation(Arc::clone(&cancellation));
        let options = RemoteProbeOptions {
            timeout: Duration::from_secs(5),
            ..RemoteProbeOptions::default()
        };
        let started = Instant::now();

        let error = probe_remote_with_program_and_environment(
            script_path,
            "dev-server",
            &options,
            &environment,
        )
        .unwrap_err();
        trigger.join().unwrap();
        let holder_pid = fs::read_to_string(&holder_pid_path)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let holder_survived = unsafe { libc::kill(holder_pid, 0) } == 0;
        if holder_survived {
            unsafe {
                libc::kill(holder_pid, libc::SIGKILL);
            }
        }

        assert!(matches!(error, RemoteTransportError::Cancelled { .. }));
        assert!(error.process_containment_uncertain());
        assert!(
            holder_survived,
            "fixture did not escape the SSH process group"
        );
        assert!(
            error
                .to_string()
                .contains("escaped ProxyCommand descendant"),
            "escaped holder was not surfaced in cancellation diagnostics: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "escaped pipe workers made cancellation unbounded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_os_string_import_remains_byte_preserving() {
        use std::os::unix::ffi::OsStrExt;

        // Keep this module's argv assertions explicit about Unix OsStrs.
        assert_eq!(std::ffi::OsStr::new("ssh").as_bytes(), b"ssh");
    }
}
