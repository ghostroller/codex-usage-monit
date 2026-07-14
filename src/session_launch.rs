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
pub(crate) struct ZellijLaunchPlan {
    pub(crate) thread_id: String,
    pub(crate) pane_name: String,
    pub(crate) zellij_bin: PathBuf,
    pub(crate) command: CommandPlan,
}

pub(crate) fn prepare_zellij_launch(
    target: &ResumeTarget,
    context: &LaunchContext,
    options: &ZellijOptions,
) -> Result<ZellijLaunchPlan, PrepareError> {
    let cwd = check_eligibility(target)?;
    let zellij_bin = prepare_zellij_focus(context)?;
    validate_percentage("width", options.width_percent)?;
    validate_percentage("height", options.height_percent)?;

    // A relative --codex-home is relative to the monitor, not the new pane cwd.
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
        cwd.as_os_str().to_owned(),
        OsString::from("--"),
        OsString::from(ENV_BIN),
        env_assignment("PATH", &context.path),
        env_assignment("CODEX_HOME", codex_home.as_os_str()),
        codex_bin.as_os_str().to_owned(),
        OsString::from("resume"),
        OsString::from("--cd"),
        cwd.as_os_str().to_owned(),
        OsString::from(&target.thread_id),
    ]);

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

fn resolve_executable(
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
        let candidate = directory.join(name);
        if is_executable_file(&candidate)
            && let Ok(candidate) = fs::canonicalize(candidate)
        {
            return Ok(candidate);
        }
    }
    Err(PrepareError::ExecutableNotFound(name))
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
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const THREAD_ID: &str = "019f52ac-7a9f-7fd1-8dda-e775ef950785";

    fn target(cwd: &Path) -> ResumeTarget {
        ResumeTarget {
            thread_id: THREAD_ID.to_owned(),
            title: "Main feature implementation".to_owned(),
            cwd: Some(cwd.to_path_buf()),
            source: Some("desktop".to_owned()),
            parent_thread_id: None,
            status: TaskStatus::Completed,
            archived: false,
        }
    }

    fn executable_script(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    fn executable(path: &Path) {
        executable_script(path, "#!/bin/sh\nexit 0\n");
    }

    fn fixture() -> (TempDir, ResumeTarget, LaunchContext) {
        let temp = tempfile::tempdir().unwrap();
        let monitor_cwd = temp.path().join("monitor root");
        let task_cwd = temp.path().join("task root --quoted");
        let codex_home = monitor_cwd.join("relative home");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&monitor_cwd).unwrap();
        fs::create_dir_all(&task_cwd).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("codex"));
        executable(&bin.join("zellij"));
        let context = LaunchContext::new(
            PathBuf::from("relative home"),
            None,
            env::join_paths([&bin]).unwrap(),
            monitor_cwd,
            true,
        );
        (temp, target(&task_cwd), context)
    }

    #[test]
    fn validates_only_canonical_lowercase_uuid() {
        assert!(is_canonical_thread_uuid(THREAD_ID));
        assert!(!is_canonical_thread_uuid(
            "019F52AC-7A9F-7FD1-8DDA-E775EF950785"
        ));
        assert!(!is_canonical_thread_uuid(
            "019f52ac7a9f-7fd1-8dda-e775ef950785"
        ));
        assert!(!is_canonical_thread_uuid(
            "019f52ac-7a9f-7fd1-8dda-e775ef95078z"
        ));
        assert!(!is_canonical_thread_uuid("latest"));
    }

    #[test]
    fn eligibility_rejects_subagents_active_archived_and_bad_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let mut candidate = target(temp.path());
        assert_eq!(check_eligibility(&candidate), Ok(temp.path()));

        candidate.source = Some("SUBAGENT".to_owned());
        assert_eq!(
            check_eligibility(&candidate),
            Err(EligibilityError::Subagent)
        );
        candidate.source = Some("desktop".to_owned());
        candidate.parent_thread_id = Some("parent".to_owned());
        assert_eq!(check_eligibility(&candidate), Ok(temp.path()));
        candidate.parent_thread_id = None;
        candidate.status = TaskStatus::WaitingInput;
        assert_eq!(
            check_eligibility(&candidate),
            Err(EligibilityError::Active(TaskStatus::WaitingInput))
        );
        candidate.archived = true;
        assert_eq!(
            check_eligibility(&candidate),
            Err(EligibilityError::Archived)
        );
        candidate.archived = false;
        candidate.status = TaskStatus::Completed;
        candidate.cwd = None;
        assert_eq!(
            check_eligibility(&candidate),
            Err(EligibilityError::MissingCwd)
        );
        candidate.cwd = Some(PathBuf::from("relative"));
        assert!(matches!(
            check_eligibility(&candidate),
            Err(EligibilityError::RelativeCwd(_))
        ));
        candidate.cwd = Some(temp.path().join("missing"));
        assert!(matches!(
            check_eligibility(&candidate),
            Err(EligibilityError::CwdNotFound(_))
        ));
        let file = temp.path().join("file");
        fs::write(&file, "not a directory").unwrap();
        candidate.cwd = Some(file);
        assert!(matches!(
            check_eligibility(&candidate),
            Err(EligibilityError::CwdNotDirectory(_))
        ));
    }

    #[test]
    fn launch_plan_preserves_argv_boundaries_and_environment() {
        let (_temp, target, context) = fixture();
        let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
        assert!(plan.command.program.is_absolute());
        assert_eq!(plan.command.program, plan.zellij_bin);

        let args = &plan.command.args;
        assert_eq!(&args[..2], ["action", "new-pane"]);
        assert!(args.contains(&OsString::from("--floating")));
        assert!(args.contains(&OsString::from("90%")));
        assert!(args.contains(&target.cwd.as_ref().unwrap().as_os_str().to_owned()));

        let separator = args.iter().position(|arg| arg == "--").unwrap();
        let command = &args[separator + 1..];
        assert_eq!(command[0], ENV_BIN);
        assert_eq!(command[1], env_assignment("PATH", &context.path));
        assert_eq!(
            command[2],
            env_assignment(
                "CODEX_HOME",
                context.monitor_cwd.join("relative home").as_os_str()
            )
        );
        assert!(Path::new(&command[3]).is_absolute());
        assert_eq!(
            &command[4..],
            [
                OsString::from("resume"),
                OsString::from("--cd"),
                target.cwd.as_ref().unwrap().as_os_str().to_owned(),
                OsString::from(THREAD_ID),
            ]
        );
    }

    #[test]
    fn relative_codex_override_is_resolved_against_monitor_cwd() {
        let (_temp, target, mut context) = fixture();
        let tools = context.monitor_cwd.join("tools");
        fs::create_dir(&tools).unwrap();
        executable(&tools.join("custom-codex"));
        context.codex_bin = Some(PathBuf::from("tools/custom-codex"));

        let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
        let separator = plan
            .command
            .args
            .iter()
            .position(|arg| arg == "--")
            .unwrap();
        assert_eq!(
            PathBuf::from(&plan.command.args[separator + 4]),
            fs::canonicalize(tools.join("custom-codex")).unwrap()
        );
    }

    #[test]
    fn non_floating_and_close_on_exit_have_exact_flags() {
        let (_temp, target, context) = fixture();
        let options = ZellijOptions {
            floating: false,
            width_percent: 80,
            height_percent: 70,
            close_on_exit: true,
        };
        let plan = prepare_zellij_launch(&target, &context, &options).unwrap();
        assert!(!plan.command.args.contains(&OsString::from("--floating")));
        assert!(!plan.command.args.contains(&OsString::from("--width")));
        assert!(!plan.command.args.contains(&OsString::from("--height")));
        assert!(
            plan.command
                .args
                .contains(&OsString::from("--close-on-exit"))
        );
    }

    #[test]
    fn rejects_invalid_dimensions_and_non_zellij_context() {
        let (_temp, target, mut context) = fixture();
        let options = ZellijOptions {
            width_percent: 0,
            ..ZellijOptions::default()
        };
        assert!(matches!(
            prepare_zellij_launch(&target, &context, &options),
            Err(PrepareError::InvalidDimension {
                name: "width",
                value: 0
            })
        ));

        context.in_zellij = false;
        assert!(matches!(
            prepare_zellij_launch(&target, &context, &ZellijOptions::default()),
            Err(PrepareError::NotInZellij)
        ));
    }

    #[test]
    fn focus_preflight_does_not_require_codex_or_task_state() {
        let (_temp, _target, mut context) = fixture();
        context.codex_home = PathBuf::from("missing-home");
        context.codex_bin = Some(PathBuf::from("missing-codex"));

        let zellij = prepare_zellij_focus(&context).unwrap();
        assert!(zellij.is_absolute());
        assert_eq!(zellij.file_name().and_then(OsStr::to_str), Some("zellij"));
    }

    #[test]
    fn pane_name_removes_controls_bidi_and_truncates_by_display_width() {
        let title = "hello\n\x1b[31m\u{202e}world 主要功能".repeat(8);
        let name = pane_name(THREAD_ID, &title);
        assert!(name.starts_with("codex 019f52ac - "));
        assert!(!name.chars().any(char::is_control));
        assert!(!name.chars().any(is_bidi_control));
        assert!(UnicodeWidthStr::width(name.as_str()) <= PANE_NAME_MAX_WIDTH);
        assert!(name.ends_with("..."));
    }

    #[test]
    fn parses_only_terminal_pane_ids() {
        assert_eq!(
            parse_created_pane_id(b"terminal_42\n").unwrap().as_str(),
            "terminal_42"
        );
        assert!(parse_created_pane_id(b"plugin_42\n").is_err());
        assert!(parse_created_pane_id(b"terminal_1 extra").is_err());
    }

    #[test]
    fn parses_flat_and_nested_zellij_pane_lists() {
        let output = br#"[
          {"id":2,"is_plugin":false,"title":"shell"},
          {"id":4,"is_plugin":true,"title":"plugin"},
          {"tab":{"id":99,"panes":[
            {"id":7,"isPlugin":false},
            {"pane_id":"2","is_plugin":false},
            {"id":8,"is_plugin":true}
          ]}}
        ]"#;
        let panes = parse_listed_pane_ids(output).unwrap();
        assert_eq!(
            panes.iter().map(PaneId::as_str).collect::<Vec<_>>(),
            ["terminal_2", "terminal_7"]
        );
        assert!(parse_listed_pane_ids(b"not json").is_err());
    }

    #[test]
    fn executes_new_pane_and_focuses_an_existing_terminal() {
        let (_temp, target, context) = fixture();
        let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
        executable_script(
            &plan.zellij_bin,
            "#!/bin/sh\nif [ \"$2\" = \"new-pane\" ]; then printf 'terminal_42\\n'; exit 0; fi\nexit 2\n",
        );
        assert_eq!(
            execute_zellij_launch(&plan).unwrap(),
            LaunchResult::Created {
                pane_id: PaneId::parse("terminal_42").unwrap()
            }
        );

        executable_script(
            &plan.zellij_bin,
            "#!/bin/sh\nif [ \"$2\" = \"list-panes\" ]; then printf '[{\"id\":42,\"is_plugin\":false}]'; exit 0; fi\nif [ \"$2\" = \"focus-pane-id\" ] && [ \"$3\" = \"terminal_42\" ]; then exit 0; fi\nexit 3\n",
        );
        assert_eq!(
            focus_existing_pane(&plan.zellij_bin, &PaneId::parse("terminal_42").unwrap()).unwrap(),
            FocusResult::Focused
        );
    }

    #[test]
    fn missing_panes_and_rejected_actions_are_structured() {
        let (_temp, target, context) = fixture();
        let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
        executable_script(
            &plan.zellij_bin,
            "#!/bin/sh\nif [ \"$2\" = \"list-panes\" ]; then printf '[]'; exit 0; fi\nprintf 'bad\\noutput\\033[31m' >&2\nexit 7\n",
        );
        assert_eq!(
            focus_existing_pane(&plan.zellij_bin, &PaneId::parse("terminal_99").unwrap()).unwrap(),
            FocusResult::Missing
        );
        let error = execute_zellij_launch(&plan).unwrap_err().to_string();
        assert!(error.contains("exit 7"));
        assert!(error.contains("bad output[31m"));
        assert!(!error.contains('\n'));
        assert!(!error.contains('\u{1b}'));
    }
}
