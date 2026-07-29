use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::domain::{TaskRecord, TaskStatus};

const ENV_BIN: &str = "/usr/bin/env";
const PANE_NAME_MAX_WIDTH: usize = 48;
const PROCESS_MESSAGE_MAX_WIDTH: usize = 512;

/// Everything needed to decide whether a selected task can be resumed. The
/// owned form is intentional: the TUI moves this value to a launch worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResumeTarget {
    pub(crate) thread_id: String,
    pub(crate) title: String,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) source: Option<String>,
    pub(crate) parent_thread_id: Option<String>,
    pub(crate) status: TaskStatus,
    pub(crate) archived: bool,
}

impl ResumeTarget {
    pub(crate) fn from_task(task: &TaskRecord) -> Self {
        Self {
            thread_id: task.thread_id.clone(),
            title: task.title.clone(),
            cwd: task.cwd.clone(),
            source: task.source.clone(),
            parent_thread_id: task.parent_thread_id.clone(),
            status: task.status,
            archived: task.archived,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EligibilityError {
    InvalidThreadId,
    Subagent,
    Active(TaskStatus),
    Archived,
    MissingCwd,
    RelativeCwd(PathBuf),
    CwdNotFound(PathBuf),
    CwdNotDirectory(PathBuf),
    CwdUnavailable { path: PathBuf, kind: io::ErrorKind },
}

impl fmt::Display for EligibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidThreadId => write!(formatter, "Task has no resumable thread id"),
            Self::Subagent => write!(formatter, "Resume the parent task, then use /agent"),
            Self::Active(status) => write!(
                formatter,
                "Live attach is unavailable while the task is {}",
                status.label()
            ),
            Self::Archived => write!(formatter, "Unarchive the task before resuming it"),
            Self::MissingCwd => write!(formatter, "Task has no recorded working directory"),
            Self::RelativeCwd(path) => write!(
                formatter,
                "Task working directory is not absolute: {}",
                path.display()
            ),
            Self::CwdNotFound(path) => write!(
                formatter,
                "Task working directory no longer exists: {}",
                path.display()
            ),
            Self::CwdNotDirectory(path) => write!(
                formatter,
                "Task working directory is not a directory: {}",
                path.display()
            ),
            Self::CwdUnavailable { path, kind } => write!(
                formatter,
                "Task working directory is unavailable ({kind:?}): {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for EligibilityError {}

pub(crate) fn check_eligibility(target: &ResumeTarget) -> Result<&Path, EligibilityError> {
    let cwd = check_eligibility_without_cwd_probe(target)?;
    match fs::metadata(cwd) {
        Ok(metadata) if metadata.is_dir() => Ok(cwd),
        Ok(_) => Err(EligibilityError::CwdNotDirectory(cwd.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(EligibilityError::CwdNotFound(cwd.to_path_buf()))
        }
        Err(error) => Err(EligibilityError::CwdUnavailable {
            path: cwd.to_path_buf(),
            kind: error.kind(),
        }),
    }
}

/// Performs the render-safe portion of resume eligibility checking. It never
/// touches the filesystem, so the TUI can use it to style the Open control.
pub(crate) fn check_eligibility_without_cwd_probe(
    target: &ResumeTarget,
) -> Result<&Path, EligibilityError> {
    if !is_canonical_thread_uuid(&target.thread_id) {
        return Err(EligibilityError::InvalidThreadId);
    }
    if target
        .source
        .as_deref()
        .is_some_and(|source| source.eq_ignore_ascii_case("subagent"))
    {
        return Err(EligibilityError::Subagent);
    }
    if target.archived {
        return Err(EligibilityError::Archived);
    }
    if target.status.is_active() {
        return Err(EligibilityError::Active(target.status));
    }

    let cwd = target.cwd.as_deref().ok_or(EligibilityError::MissingCwd)?;
    if !cwd.is_absolute() {
        return Err(EligibilityError::RelativeCwd(cwd.to_path_buf()));
    }
    Ok(cwd)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchContext {
    pub(crate) codex_home: PathBuf,
    pub(crate) codex_bin: Option<PathBuf>,
    pub(crate) path: OsString,
    pub(crate) monitor_cwd: PathBuf,
    pub(crate) in_zellij: bool,
}

impl LaunchContext {
    pub(crate) fn capture(
        codex_home: PathBuf,
        codex_bin: Option<PathBuf>,
    ) -> Result<Self, PrepareError> {
        let monitor_cwd = env::current_dir().map_err(PrepareError::CurrentDirectory)?;
        let path = env::var_os("PATH")
            .filter(|path| !path.is_empty())
            .ok_or(PrepareError::MissingPath)?;
        Ok(Self {
            codex_home,
            codex_bin,
            path,
            monitor_cwd,
            in_zellij: env::var_os("ZELLIJ").is_some(),
        })
    }

    #[cfg(test)]
    fn new(
        codex_home: PathBuf,
        codex_bin: Option<PathBuf>,
        path: OsString,
        monitor_cwd: PathBuf,
        in_zellij: bool,
    ) -> Self {
        Self {
            codex_home,
            codex_bin,
            path,
            monitor_cwd,
            in_zellij,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ZellijOptions {
    pub(crate) floating: bool,
    pub(crate) width_percent: u8,
    pub(crate) height_percent: u8,
    pub(crate) close_on_exit: bool,
}

impl Default for ZellijOptions {
    fn default() -> Self {
        Self {
            floating: true,
            width_percent: 90,
            height_percent: 90,
            close_on_exit: false,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PrepareError {
    Eligibility(EligibilityError),
    NotInZellij,
    MissingPath,
    CurrentDirectory(io::Error),
    InvalidMonitorCwd(PathBuf),
    InvalidCodexHome(PathBuf),
    UnrepresentableShellCommand,
    InvalidDimension { name: &'static str, value: u8 },
    ExecutableNotFound(&'static str),
    ExecutableUnavailable { name: &'static str, path: PathBuf },
}

impl fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eligibility(error) => error.fmt(formatter),
            Self::NotInZellij => write!(
                formatter,
                "Open requires codex-usage-monit to run inside Zellij"
            ),
            Self::MissingPath => write!(formatter, "The monitor process has no usable PATH"),
            Self::CurrentDirectory(error) => {
                write!(
                    formatter,
                    "Could not read the monitor working directory: {error}"
                )
            }
            Self::InvalidMonitorCwd(path) => write!(
                formatter,
                "Monitor working directory is unavailable: {}",
                path.display()
            ),
            Self::InvalidCodexHome(path) => {
                write!(formatter, "Codex home is unavailable: {}", path.display())
            }
            Self::UnrepresentableShellCommand => write!(
                formatter,
                "Resume command contains non-UTF-8 or control characters and cannot be copied"
            ),
            Self::InvalidDimension { name, value } => {
                write!(
                    formatter,
                    "Zellij {name} must be between 1 and 100, got {value}"
                )
            }
            Self::ExecutableNotFound(name) => {
                write!(formatter, "Could not find executable `{name}` in PATH")
            }
            Self::ExecutableUnavailable { name, path } => write!(
                formatter,
                "Configured `{name}` executable is unavailable: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Eligibility(error) => Some(error),
            Self::CurrentDirectory(error) => Some(error),
            _ => None,
        }
    }
}

impl From<EligibilityError> for PrepareError {
    fn from(error: EligibilityError) -> Self {
        Self::Eligibility(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandPlan {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResumeCommandPlan {
    pub(crate) thread_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) command: CommandPlan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResumeCopyPlan {
    pub(crate) thread_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) codex_home: PathBuf,
    pub(crate) command: CommandPlan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ZellijLaunchPlan {
    pub(crate) thread_id: String,
    pub(crate) pane_name: String,
    pub(crate) zellij_bin: PathBuf,
    pub(crate) command: CommandPlan,
}

pub(crate) fn prepare_resume_command(
    target: &ResumeTarget,
    context: &LaunchContext,
) -> Result<ResumeCommandPlan, PrepareError> {
    let cwd = check_eligibility(target)?.to_path_buf();

    // A relative --codex-home is relative to the monitor, not the target cwd.
    let codex_home = absolute_from(&context.codex_home, &context.monitor_cwd);
    if !codex_home.is_dir() {
        return Err(PrepareError::InvalidCodexHome(codex_home));
    }
    let codex_bin = resolve_executable(
        "codex",
        context.codex_bin.as_deref(),
        &context.path,
        &context.monitor_cwd,
    )?;
    let mut args = vec![
        env_assignment("PATH", &context.path),
        env_assignment("CODEX_HOME", codex_home.as_os_str()),
        codex_bin.as_os_str().to_owned(),
    ];
    args.extend(resume_arguments(&cwd, &target.thread_id));

    Ok(ResumeCommandPlan {
        thread_id: target.thread_id.clone(),
        cwd,
        command: CommandPlan {
            program: PathBuf::from(ENV_BIN),
            args,
        },
    })
}

pub(crate) fn prepare_resume_copy_command(
    target: &ResumeTarget,
    context: &LaunchContext,
) -> Result<ResumeCopyPlan, PrepareError> {
    let cwd = check_eligibility(target)?.to_path_buf();
    let codex_home = absolute_from(&context.codex_home, &context.monitor_cwd);
    if !codex_home.is_dir() {
        return Err(PrepareError::InvalidCodexHome(codex_home));
    }
    let codex_bin = match context.codex_bin.as_deref() {
        Some(path) => resolve_executable("codex", Some(path), &context.path, &context.monitor_cwd)?,
        None => PathBuf::from("codex"),
    };

    Ok(ResumeCopyPlan {
        thread_id: target.thread_id.clone(),
        cwd: cwd.clone(),
        codex_home,
        command: CommandPlan {
            program: codex_bin,
            args: resume_arguments(&cwd, &target.thread_id),
        },
    })
}

pub(crate) fn render_resume_command(plan: &ResumeCopyPlan) -> Result<String, PrepareError> {
    if cfg!(windows) {
        render_powershell_resume_command(plan)
    } else {
        render_posix_resume_command(plan)
    }
}

fn render_posix_resume_command(plan: &ResumeCopyPlan) -> Result<String, PrepareError> {
    let codex_home = posix_shell_word(plan.codex_home.as_os_str())?;
    let command = std::iter::once(plan.command.program.as_os_str())
        .chain(plan.command.args.iter().map(OsString::as_os_str))
        .map(posix_shell_word)
        .collect::<Result<Vec<_>, _>>()
        .map(|words| words.join(" "))?;
    Ok(format!("CODEX_HOME={codex_home} {command}"))
}

fn render_powershell_resume_command(plan: &ResumeCopyPlan) -> Result<String, PrepareError> {
    let codex_home = powershell_word(plan.codex_home.as_os_str())?;
    let command = std::iter::once(plan.command.program.as_os_str())
        .chain(plan.command.args.iter().map(OsString::as_os_str))
        .map(powershell_word)
        .collect::<Result<Vec<_>, _>>()
        .map(|words| words.join(" "))?;
    Ok(format!(
        "& {{ param($codexHome) $previous = $env:CODEX_HOME; try {{ $env:CODEX_HOME = $codexHome; & {command} }} finally {{ $env:CODEX_HOME = $previous }} }} {codex_home}"
    ))
}

pub(crate) fn prepare_zellij_launch(
    target: &ResumeTarget,
    context: &LaunchContext,
    options: &ZellijOptions,
) -> Result<ZellijLaunchPlan, PrepareError> {
    let zellij_bin = prepare_zellij_focus(context)?;
    validate_percentage("width", options.width_percent)?;
    validate_percentage("height", options.height_percent)?;
    let resume = prepare_resume_command(target, context)?;
    let pane_name = pane_name(&target.thread_id, &target.title);

    let mut args = vec![OsString::from("action"), OsString::from("new-pane")];
    if options.floating {
        args.extend([
            OsString::from("--floating"),
            OsString::from("--width"),
            OsString::from(format!("{}%", options.width_percent)),
            OsString::from("--height"),
            OsString::from(format!("{}%", options.height_percent)),
            OsString::from("--near-current-pane"),
        ]);
    }
    if options.close_on_exit {
        args.push(OsString::from("--close-on-exit"));
    }
    args.extend([
        OsString::from("--name"),
        OsString::from(&pane_name),
        OsString::from("--cwd"),
        resume.cwd.as_os_str().to_owned(),
        OsString::from("--"),
        resume.command.program.as_os_str().to_owned(),
    ]);
    args.extend(resume.command.args);

    Ok(ZellijLaunchPlan {
        thread_id: target.thread_id.clone(),
        pane_name,
        zellij_bin: zellij_bin.clone(),
        command: CommandPlan {
            program: zellij_bin,
            args,
        },
    })
}

fn posix_shell_word(value: &OsStr) -> Result<String, PrepareError> {
    let value = representable_shell_text(value)?;
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
                )
        })
    {
        return Ok(value.to_owned());
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

fn powershell_word(value: &OsStr) -> Result<String, PrepareError> {
    let value = representable_shell_text(value)?;
    Ok(format!("'{}'", value.replace('\'', "''")))
}

fn representable_shell_text(value: &OsStr) -> Result<&str, PrepareError> {
    let value = value
        .to_str()
        .ok_or(PrepareError::UnrepresentableShellCommand)?;
    if value
        .chars()
        .any(|character| character.is_control() || is_bidi_control(character))
    {
        return Err(PrepareError::UnrepresentableShellCommand);
    }
    Ok(value)
}

fn resume_arguments(cwd: &Path, thread_id: &str) -> Vec<OsString> {
    vec![
        OsString::from("resume"),
        OsString::from("--cd"),
        cwd.as_os_str().to_owned(),
        OsString::from(thread_id),
    ]
}

/// Resolves the current-session Zellij client without touching task, cwd, or
/// Codex state. This lets a known pane remain focusable while its task runs.
pub(crate) fn prepare_zellij_focus(context: &LaunchContext) -> Result<PathBuf, PrepareError> {
    if !context.in_zellij {
        return Err(PrepareError::NotInZellij);
    }
    if !context.monitor_cwd.is_absolute() || !context.monitor_cwd.is_dir() {
        return Err(PrepareError::InvalidMonitorCwd(context.monitor_cwd.clone()));
    }
    if context.path.is_empty() {
        return Err(PrepareError::MissingPath);
    }
    resolve_executable("zellij", None, &context.path, &context.monitor_cwd)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PaneId(String);

impl PaneId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        let digits = value.strip_prefix("terminal_")?;
        (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| Self(value.to_owned()))
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchResult {
    Created { pane_id: PaneId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusResult {
    Focused,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZellijOperation {
    NewPane,
    ListPanes,
    FocusPane,
}

impl fmt::Display for ZellijOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NewPane => "create Zellij pane",
            Self::ListPanes => "list Zellij panes",
            Self::FocusPane => "focus Zellij pane",
        })
    }
}

#[derive(Debug)]
pub(crate) enum ZellijError {
    Spawn {
        operation: ZellijOperation,
        source: io::Error,
    },
    Rejected {
        operation: ZellijOperation,
        code: Option<i32>,
        stderr: String,
    },
    InvalidPaneId(String),
    InvalidPaneList(String),
}

impl fmt::Display for ZellijError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { operation, source } => {
                write!(formatter, "Could not {operation}: {source}")
            }
            Self::Rejected {
                operation,
                code,
                stderr,
            } => {
                write!(
                    formatter,
                    "Zellij could not {operation} (exit {}): {stderr}",
                    code.map_or_else(|| "signal".to_owned(), |code| code.to_string())
                )
            }
            Self::InvalidPaneId(output) => {
                write!(formatter, "Zellij returned an invalid pane id: {output}")
            }
            Self::InvalidPaneList(message) => {
                write!(formatter, "Zellij returned an invalid pane list: {message}")
            }
        }
    }
}

impl std::error::Error for ZellijError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub(crate) fn execute_zellij_launch(plan: &ZellijLaunchPlan) -> Result<LaunchResult, ZellijError> {
    let output = execute_command(&plan.command, ZellijOperation::NewPane)?;
    let pane_id = parse_created_pane_id(&output.stdout)?;
    Ok(LaunchResult::Created { pane_id })
}

pub(crate) fn list_panes(zellij_bin: &Path) -> Result<Vec<PaneId>, ZellijError> {
    let plan = CommandPlan {
        program: zellij_bin.to_path_buf(),
        args: ["action", "list-panes", "--all", "--json"]
            .into_iter()
            .map(OsString::from)
            .collect(),
    };
    let output = execute_command(&plan, ZellijOperation::ListPanes)?;
    parse_listed_pane_ids(&output.stdout)
}

/// Returns `Missing` instead of invoking focus when the pane has already been
/// deleted. Callers can then remove their in-memory thread-to-pane mapping.
pub(crate) fn focus_existing_pane(
    zellij_bin: &Path,
    pane_id: &PaneId,
) -> Result<FocusResult, ZellijError> {
    if !list_panes(zellij_bin)?.contains(pane_id) {
        return Ok(FocusResult::Missing);
    }
    let plan = CommandPlan {
        program: zellij_bin.to_path_buf(),
        args: vec![
            OsString::from("action"),
            OsString::from("focus-pane-id"),
            OsString::from(pane_id.as_str()),
        ],
    };
    execute_command(&plan, ZellijOperation::FocusPane)?;
    Ok(FocusResult::Focused)
}

fn execute_command(
    plan: &CommandPlan,
    operation: ZellijOperation,
) -> Result<std::process::Output, ZellijError> {
    let output = Command::new(&plan.program)
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| ZellijError::Spawn { operation, source })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(ZellijError::Rejected {
            operation,
            code: output.status.code(),
            stderr: process_message(&output.stderr),
        })
    }
}

fn parse_created_pane_id(stdout: &[u8]) -> Result<PaneId, ZellijError> {
    let output = std::str::from_utf8(stdout)
        .map_err(|_| ZellijError::InvalidPaneId("non-UTF-8 output".to_owned()))?;
    PaneId::parse(output.trim()).ok_or_else(|| ZellijError::InvalidPaneId(process_message(stdout)))
}

fn parse_listed_pane_ids(stdout: &[u8]) -> Result<Vec<PaneId>, ZellijError> {
    let value: Value = serde_json::from_slice(stdout)
        .map_err(|error| ZellijError::InvalidPaneList(error.to_string()))?;
    let mut pane_ids = BTreeSet::new();
    collect_pane_ids(&value, &mut pane_ids);
    Ok(pane_ids.into_iter().collect())
}

fn collect_pane_ids(value: &Value, pane_ids: &mut BTreeSet<PaneId>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_pane_ids(value, pane_ids);
            }
        }
        Value::Object(object) => {
            let pane_id = object
                .get("pane_id")
                .or_else(|| object.get("paneId"))
                .or_else(|| {
                    object
                        .contains_key("is_plugin")
                        .then(|| object.get("id"))
                        .flatten()
                })
                .or_else(|| {
                    object
                        .contains_key("isPlugin")
                        .then(|| object.get("id"))
                        .flatten()
                });
            if let Some(value) = pane_id
                && let Some(pane_id) = pane_id_from_json(value, object)
            {
                pane_ids.insert(pane_id);
            }
            for value in object.values() {
                collect_pane_ids(value, pane_ids);
            }
        }
        _ => {}
    }
}

fn pane_id_from_json(value: &Value, object: &Map<String, Value>) -> Option<PaneId> {
    if let Some(value) = value.as_str() {
        if let Some(pane_id) = PaneId::parse(value) {
            return Some(pane_id);
        }
        if value.bytes().all(|byte| byte.is_ascii_digit()) {
            return terminal_pane_id(value, object);
        }
        return None;
    }
    value
        .as_u64()
        .and_then(|value| terminal_pane_id(&value.to_string(), object))
}

fn terminal_pane_id(digits: &str, object: &Map<String, Value>) -> Option<PaneId> {
    let is_plugin = object
        .get("is_plugin")
        .or_else(|| object.get("isPlugin"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (!is_plugin).then(|| PaneId(format!("terminal_{digits}")))
}

fn is_canonical_thread_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
    })
}

fn validate_percentage(name: &'static str, value: u8) -> Result<(), PrepareError> {
    if (1..=100).contains(&value) {
        Ok(())
    } else {
        Err(PrepareError::InvalidDimension { name, value })
    }
}

pub(crate) fn resolve_executable(
    name: &'static str,
    override_path: Option<&Path>,
    path: &OsStr,
    monitor_cwd: &Path,
) -> Result<PathBuf, PrepareError> {
    if let Some(path) = override_path {
        let path = absolute_from(path, monitor_cwd);
        if !is_executable_file(&path) {
            return Err(PrepareError::ExecutableUnavailable { name, path });
        }
        return fs::canonicalize(&path)
            .map_err(|_| PrepareError::ExecutableUnavailable { name, path });
    }

    for directory in env::split_paths(path) {
        let directory = absolute_from(&directory, monitor_cwd);
        for candidate in executable_candidates(&directory, name) {
            if is_executable_file(&candidate)
                && let Ok(candidate) = fs::canonicalize(candidate)
            {
                return Ok(candidate);
            }
        }
    }
    Err(PrepareError::ExecutableNotFound(name))
}

fn executable_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    let base = directory.join(name);
    #[cfg(not(windows))]
    {
        vec![base]
    }
    #[cfg(windows)]
    {
        let mut candidates = vec![base.clone()];
        if base.extension().is_none() {
            let path_ext =
                env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
            for extension in path_ext.to_string_lossy().split(';') {
                let extension = extension.trim().trim_start_matches('.');
                if !extension.is_empty() {
                    candidates.push(base.with_extension(extension));
                }
            }
        }
        candidates
    }
}

fn absolute_from(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
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

fn env_assignment(name: &str, value: &OsStr) -> OsString {
    let mut assignment = OsString::from(name);
    assignment.push("=");
    assignment.push(value);
    assignment
}

fn pane_name(thread_id: &str, title: &str) -> String {
    let short_id = thread_id.get(..8).unwrap_or(thread_id);
    let title = sanitize_text(title);
    let title = if title.trim().is_empty() {
        "Untitled task"
    } else {
        title.trim()
    };
    truncate_display_width(&format!("codex {short_id} - {title}"), PANE_NAME_MAX_WIDTH)
}

fn process_message(bytes: &[u8]) -> String {
    let message = String::from_utf8_lossy(bytes);
    let message = sanitize_text(&message);
    let message = message.trim();
    if message.is_empty() {
        "no error output".to_owned()
    } else {
        truncate_display_width(message, PROCESS_MESSAGE_MAX_WIDTH)
    }
}

fn sanitize_text(value: &str) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_control() || is_bidi_control(character) {
            pending_space |= character.is_whitespace();
            continue;
        }
        if character.is_whitespace() {
            pending_space = !result.is_empty();
            continue;
        }
        if pending_space {
            result.push(' ');
            pending_space = false;
        }
        result.push(character);
    }
    result
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn truncate_display_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    let suffix = "...";
    let content_width = max_width.saturating_sub(suffix.len());
    let mut result = String::new();
    let mut width = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push_str(&suffix[..suffix.len().min(max_width)]);
    result
}

#[cfg(test)]
mod tests;
