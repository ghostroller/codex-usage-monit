use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic_file::replace_file;
use crate::history::history_namespace;

const SERVICE_LABEL: &str = "com.ghostroller.codex-usage-monit.recorder";
const SYSTEMD_UNIT: &str = "codex-usage-monit-recorder.service";
const WINDOWS_TASK_PREFIX: &str = r"\CodexUsageMonitRecorder";
const STATUS_SCHEMA_VERSION: u32 = 2;
const LEGACY_RECORDER_STALE_SECONDS: u64 = 12 * 60;
const RECORDER_STALE_GRACE_SECONDS: u64 = 2 * 60;
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Installed,
    Running,
    Stopped,
    Unknown,
}

impl ServiceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not installed",
            Self::Installed => "installed",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatus {
    pub platform: &'static str,
    pub state: ServiceState,
    pub installed: bool,
    pub running: bool,
    pub registration_path: Option<PathBuf>,
    pub last_history_heartbeat: Option<DateTime<Utc>>,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceOptions {
    pub executable: PathBuf,
    pub codex_home: PathBuf,
    pub codex_bin: Option<PathBuf>,
    pub history_dir: PathBuf,
    pub status_file: PathBuf,
    pub perf_log: Option<PathBuf>,
    pub environment_path: Option<OsString>,
    pub lookback_days: i64,
    pub max_files: usize,
    pub active_grace_minutes: u64,
    pub offline: bool,
    pub redact_content: bool,
    pub no_rollout_cache: bool,
}

impl ServiceOptions {
    pub fn new(
        executable: PathBuf,
        codex_home: PathBuf,
        history_dir: PathBuf,
        status_file: PathBuf,
        perf_log: Option<PathBuf>,
    ) -> Self {
        Self {
            executable,
            codex_home,
            codex_bin: None,
            history_dir,
            status_file,
            perf_log,
            environment_path: env::var_os("PATH"),
            lookback_days: 7,
            max_files: 500,
            active_grace_minutes: 5,
            offline: false,
            redact_content: false,
            no_rollout_cache: false,
        }
    }

    pub fn recorder_arguments(&self) -> Vec<OsString> {
        let mut arguments = vec![
            OsString::from("--codex-home"),
            self.codex_home.as_os_str().to_owned(),
        ];
        if let Some(codex_bin) = self.codex_bin.as_deref() {
            arguments.push(OsString::from("--codex-bin"));
            arguments.push(codex_bin.as_os_str().to_owned());
        }
        arguments.extend([
            OsString::from("--days"),
            OsString::from(self.lookback_days.to_string()),
            OsString::from("--max-files"),
            OsString::from(self.max_files.to_string()),
            OsString::from("--active-grace-minutes"),
            OsString::from(self.active_grace_minutes.to_string()),
        ]);
        if self.offline {
            arguments.push(OsString::from("--offline"));
        }
        if self.redact_content {
            arguments.push(OsString::from("--redact-content"));
        }
        if self.no_rollout_cache {
            arguments.push(OsString::from("--no-rollout-cache"));
        }
        if let Some(perf_log) = self.perf_log.as_deref() {
            arguments.push(OsString::from("--perf-log"));
            arguments.push(perf_log.as_os_str().to_owned());
        }
        arguments.extend([
            OsString::from("record"),
            OsString::from("--foreground"),
            OsString::from("--history-dir"),
            self.history_dir.as_os_str().to_owned(),
            OsString::from("--status-file"),
            self.status_file.as_os_str().to_owned(),
        ]);
        arguments
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatusFile {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_namespace: Option<String>,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub last_attempt_at: DateTime<Utc>,
    pub last_history_heartbeat: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval_seconds: Option<u64>,
}

impl RecorderStatusFile {
    pub fn started(now: DateTime<Utc>, history_namespace: String) -> Self {
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            history_namespace: Some(history_namespace),
            pid: std::process::id(),
            started_at: now,
            last_attempt_at: now,
            last_history_heartbeat: None,
            last_error: None,
            heartbeat_interval_seconds: None,
        }
    }

    pub fn started_with_interval(
        now: DateTime<Utc>,
        history_namespace: String,
        heartbeat_interval_seconds: u64,
    ) -> Self {
        Self {
            heartbeat_interval_seconds: Some(heartbeat_interval_seconds.max(1)),
            ..Self::started(now, history_namespace)
        }
    }

    pub fn record_success(&mut self, now: DateTime<Utc>) {
        self.last_attempt_at = now;
        self.last_history_heartbeat = Some(now);
        self.last_error = None;
    }

    pub fn record_heartbeat(&mut self, now: DateTime<Utc>) {
        self.last_attempt_at = now;
        self.last_history_heartbeat = Some(now);
    }

    pub fn record_error(&mut self, now: DateTime<Utc>, error: impl Into<String>) {
        self.last_attempt_at = now;
        self.last_error = Some(error.into());
    }

    pub fn record_degraded(&mut self, now: DateTime<Utc>, error: impl Into<String>) {
        self.last_attempt_at = now;
        self.last_history_heartbeat = Some(now);
        self.last_error = Some(error.into());
    }

    pub fn heartbeat_is_recent(&self, now: DateTime<Utc>) -> bool {
        self.last_history_heartbeat.is_some_and(|heartbeat| {
            let age_seconds = now.signed_duration_since(heartbeat).num_seconds();
            let stale_after_seconds =
                self.heartbeat_interval_seconds
                    .map_or(LEGACY_RECORDER_STALE_SECONDS, |interval| {
                        interval
                            .saturating_add(RECORDER_STALE_GRACE_SECONDS)
                            .max(LEGACY_RECORDER_STALE_SECONDS)
                    });
            let stale_after_seconds = i64::try_from(stale_after_seconds).unwrap_or(i64::MAX);
            age_seconds >= -Duration::minutes(1).num_seconds() && age_seconds <= stale_after_seconds
        })
    }
}

pub fn default_status_file(history_dir: &Path) -> PathBuf {
    history_dir
        .parent()
        .unwrap_or(history_dir)
        .join("recorder-status.json")
}

pub fn read_recorder_status(path: &Path) -> io::Result<Option<RecorderStatusFile>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let status = serde_json::from_slice::<RecorderStatusFile>(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if status.schema_version > STATUS_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recorder status uses future schema version {}",
                status.schema_version
            ),
        ));
    }
    Ok(Some(status))
}

pub fn write_recorder_status(path: &Path, status: &RecorderStatusFile) -> io::Result<()> {
    let mut contents = serde_json::to_vec_pretty(status)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');
    write_private_atomically(path, &contents)
}

pub fn install(options: &ServiceOptions) -> Result<ServiceStatus> {
    validate_options(options)?;
    let platform = current_platform();
    if platform == Platform::Unsupported {
        bail!(
            "background service installation is unsupported on this platform; run `codex-usage-monit record --foreground` under a supervisor"
        );
    }
    remove_status_file(&options.status_file)?;
    match platform {
        Platform::MacOs => install_launchd(options)?,
        Platform::Linux => install_systemd(options)?,
        Platform::Windows => install_windows_task(options)?,
        Platform::Unsupported => unreachable!("unsupported platforms returned before installation"),
    }
    status(options)
}

pub fn status(options: &ServiceOptions) -> Result<ServiceStatus> {
    let recorder = read_recorder_status(&options.status_file).with_context(|| {
        format!(
            "could not read recorder status {}",
            options.status_file.display()
        )
    })?;
    let heartbeat = recorder
        .as_ref()
        .and_then(|status| status.last_history_heartbeat);
    let expected_namespace = history_namespace(&options.codex_home);
    let namespace_mismatch = recorder
        .as_ref()
        .and_then(|status| status.history_namespace.as_deref())
        .filter(|namespace| *namespace != expected_namespace)
        .map(str::to_string);
    let heartbeat_running = recorder
        .as_ref()
        .is_some_and(|status| status.heartbeat_is_recent(Utc::now()))
        && namespace_mismatch.is_none();
    let mut service_status = match current_platform() {
        Platform::MacOs => launchd_status(options, heartbeat, heartbeat_running),
        Platform::Linux => systemd_status(options, heartbeat, heartbeat_running),
        Platform::Windows => windows_task_status(options, heartbeat, heartbeat_running),
        Platform::Unsupported => Ok(ServiceStatus {
            platform: "unsupported",
            state: ServiceState::Unknown,
            installed: false,
            running: false,
            registration_path: None,
            last_history_heartbeat: heartbeat,
            detail: "service management is unsupported; use record --foreground".to_string(),
        }),
    }?;
    if let Some(namespace) = namespace_mismatch {
        service_status.detail.push_str(&format!(
            "; recorder targets history namespace {namespace}, expected {expected_namespace}"
        ));
    }
    if let Some(error) = recorder.and_then(|status| status.last_error) {
        service_status
            .detail
            .push_str(&format!("; last recorder error: {error}"));
    }
    Ok(service_status)
}

pub fn uninstall(options: &ServiceOptions) -> Result<ServiceStatus> {
    match current_platform() {
        Platform::MacOs => uninstall_launchd(options)?,
        Platform::Linux => uninstall_systemd(options)?,
        Platform::Windows => uninstall_windows_task(options)?,
        Platform::Unsupported => bail!("background service management is unsupported"),
    }
    remove_status_file(&options.status_file)?;
    status(options)
}

fn remove_status_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not remove recorder status {}", path.display()));
        }
    }
    Ok(())
}

fn validate_options(options: &ServiceOptions) -> Result<()> {
    if !options.executable.is_absolute() {
        bail!("service executable path must be absolute");
    }
    if !is_executable_file(&options.executable) {
        bail!(
            "service executable is unavailable or not executable: {}",
            options.executable.display()
        );
    }
    if !options.codex_home.is_absolute() {
        bail!("Codex home must be absolute for background service registration");
    }
    if let Some(codex_bin) = options.codex_bin.as_deref() {
        if !codex_bin.is_absolute() {
            bail!("Codex executable path must be absolute for background service registration");
        }
        if !is_executable_file(codex_bin) {
            bail!(
                "Codex executable is unavailable or not executable: {}",
                codex_bin.display()
            );
        }
    } else if !options.offline {
        bail!("Codex executable must be resolved for an online background recorder");
    }
    if !options.history_dir.is_absolute() || !options.status_file.is_absolute() {
        bail!("service state paths must be absolute");
    }
    let mut definition_paths = vec![
        ("service executable", &options.executable),
        ("Codex home", &options.codex_home),
        ("history directory", &options.history_dir),
        ("recorder status", &options.status_file),
    ];
    if let Some(codex_bin) = options.codex_bin.as_ref() {
        definition_paths.push(("Codex executable", codex_bin));
    }
    for (label, path) in definition_paths {
        if path.to_str().is_none() {
            bail!("{label} cannot be represented in a service definition");
        }
    }
    if options
        .perf_log
        .as_deref()
        .is_some_and(|path| path.to_str().is_none())
    {
        bail!("performance log path cannot be represented in a service definition");
    }
    if options
        .environment_path
        .as_deref()
        .is_some_and(|path| path.to_str().is_none())
    {
        bail!("PATH cannot be represented in a service definition");
    }
    Ok(())
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
    Windows,
    Unsupported,
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unsupported
    }
}

fn install_launchd(options: &ServiceOptions) -> Result<()> {
    let path = launchd_registration_path()?;
    write_private_atomically(&path, launchd_plist(options).as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    let domain = launchd_domain();
    let _ = Command::new("launchctl")
        .args(["bootout", &domain])
        .arg(&path)
        .output();
    run_checked(
        Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&path),
        "launchctl bootstrap",
    )?;
    run_checked(
        Command::new("launchctl").args(["enable", &format!("{domain}/{SERVICE_LABEL}")]),
        "launchctl enable",
    )?;
    run_checked(
        Command::new("launchctl").args(["kickstart", "-k", &format!("{domain}/{SERVICE_LABEL}")]),
        "launchctl kickstart",
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LaunchdOperation {
    BootoutRegistration { domain: String, path: PathBuf },
    BootoutService { target: String },
    PrintService { target: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchdOperationResult {
    success: bool,
    detail: String,
}

fn run_launchd_operation(operation: LaunchdOperation) -> Result<LaunchdOperationResult> {
    let (mut command, description) = match operation {
        LaunchdOperation::BootoutRegistration { domain, path } => {
            let mut command = Command::new("launchctl");
            command.args(["bootout", &domain]).arg(path);
            (command, "launchctl bootout registration")
        }
        LaunchdOperation::BootoutService { target } => {
            let mut command = Command::new("launchctl");
            command.args(["bootout", &target]);
            (command, "launchctl bootout service")
        }
        LaunchdOperation::PrintService { target } => {
            let mut command = Command::new("launchctl");
            command.args(["print", &target]);
            (command, "launchctl print service")
        }
    };
    let output = command
        .output()
        .with_context(|| format!("could not run {description}"))?;
    let success = output.status.success();
    Ok(LaunchdOperationResult {
        success,
        detail: if success {
            String::new()
        } else {
            output_detail(&output)
        },
    })
}

fn uninstall_launchd(_options: &ServiceOptions) -> Result<()> {
    let path = launchd_registration_path()?;
    let domain = launchd_domain();
    uninstall_launchd_registration(&path, &domain, run_launchd_operation)
}

fn uninstall_launchd_registration(
    path: &Path,
    domain: &str,
    mut run: impl FnMut(LaunchdOperation) -> Result<LaunchdOperationResult>,
) -> Result<()> {
    let target = format!("{domain}/{SERVICE_LABEL}");
    let mut failures = Vec::new();
    let registration_booted_out = if path.exists() {
        let result = run(LaunchdOperation::BootoutRegistration {
            domain: domain.to_string(),
            path: path.to_path_buf(),
        })?;
        if !result.success {
            failures.push(format!("registration bootout failed: {}", result.detail));
        }
        result.success
    } else {
        false
    };
    if !registration_booted_out {
        let result = run(LaunchdOperation::BootoutService {
            target: target.clone(),
        })?;
        if !result.success {
            failures.push(format!("service bootout failed: {}", result.detail));
        }
    }

    let inspection = run(LaunchdOperation::PrintService {
        target: target.clone(),
    })?;
    if inspection.success {
        let detail = std::iter::once(
            "launchctl reported success, but the service remains loaded".to_string(),
        )
        .chain(failures)
        .collect::<Vec<_>>()
        .join("; ");
        bail!("could not unload launchd service {target}: {detail}");
    }
    if !launchd_print_reports_missing(&inspection.detail) {
        bail!(
            "could not verify that launchd service {target} was unloaded: {}",
            inspection.detail
        );
    }

    if path.exists() {
        fs::remove_file(path).with_context(|| format!("could not remove {}", path.display()))?;
    }
    Ok(())
}

fn launchd_print_reports_missing(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "could not find service",
        "service not found",
        "no such process",
        "service is not loaded",
    ]
    .iter()
    .any(|message| detail.contains(message))
}

fn launchd_status(
    _options: &ServiceOptions,
    heartbeat: Option<DateTime<Utc>>,
    heartbeat_running: bool,
) -> Result<ServiceStatus> {
    let path = launchd_registration_path()?;
    let installed = path.is_file();
    let domain = launchd_domain();
    let output = Command::new("launchctl")
        .args(["print", &format!("{domain}/{SERVICE_LABEL}")])
        .output()
        .context("could not run launchctl print")?;
    let manager_running = launchd_output_reports_running(&output);
    Ok(service_status(
        "macos-launchd",
        installed,
        installed && manager_running,
        Some(path),
        heartbeat,
        manager_running,
        heartbeat_running,
    ))
}

fn launchd_output_reports_running(output: &Output) -> bool {
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == "state = running")
}

fn launchd_registration_path() -> Result<PathBuf> {
    let home = nonempty_env("HOME").ok_or_else(|| anyhow!("HOME is unavailable"))?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist")))
}

fn launchd_domain() -> String {
    #[cfg(unix)]
    {
        // SAFETY: geteuid takes no arguments and has no memory safety preconditions.
        format!("gui/{}", unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        "gui/0".to_string()
    }
}

fn launchd_plist(options: &ServiceOptions) -> String {
    let mut arguments = vec![options.executable.as_os_str().to_owned()];
    arguments.extend(options.recorder_arguments());
    let arguments = arguments
        .iter()
        .map(|argument| {
            format!(
                "        <string>{}</string>",
                xml_escape(&argument.to_string_lossy())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let environment = options.environment_path.as_ref().map_or_else(String::new, |path| {
        format!(
            "    <key>EnvironmentVariables</key>\n    <dict>\n        <key>PATH</key>\n        <string>{}</string>\n    </dict>\n",
            xml_escape(&path.to_string_lossy())
        )
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}
    </array>
{environment}    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>ThrottleInterval</key>
    <integer>30</integer>
</dict>
</plist>
"#
    )
}

fn install_systemd(options: &ServiceOptions) -> Result<()> {
    let path = systemd_registration_path()?;
    write_private_atomically(&path, systemd_unit(options).as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    run_checked(
        Command::new("systemctl").args(["--user", "daemon-reload"]),
        "systemctl --user daemon-reload",
    )?;
    run_checked(
        Command::new("systemctl").args(["--user", "enable", SYSTEMD_UNIT]),
        "systemctl --user enable",
    )?;
    run_checked(
        Command::new("systemctl").args(["--user", "restart", SYSTEMD_UNIT]),
        "systemctl --user restart",
    )
}

fn uninstall_systemd(_options: &ServiceOptions) -> Result<()> {
    let path = systemd_registration_path()?;
    let output = Command::new("systemctl")
        .args(["--user", "disable", "--now", SYSTEMD_UNIT])
        .output()
        .context("could not run systemctl --user disable --now")?;
    if !output.status.success() && path.exists() {
        let detail = output_detail(&output);
        if !detail.contains("does not exist") && !detail.contains("not loaded") {
            bail!("systemctl --user disable --now failed: {detail}");
        }
    }
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
    }
    run_checked(
        Command::new("systemctl").args(["--user", "daemon-reload"]),
        "systemctl --user daemon-reload",
    )
}

fn systemd_status(
    _options: &ServiceOptions,
    heartbeat: Option<DateTime<Utc>>,
    heartbeat_running: bool,
) -> Result<ServiceStatus> {
    let path = systemd_registration_path()?;
    let installed = path.is_file();
    let output = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT])
        .output()
        .context("could not run systemctl --user is-active")?;
    let manager_running = output.status.success();
    Ok(service_status(
        "linux-systemd-user",
        installed,
        installed && manager_running,
        Some(path),
        heartbeat,
        manager_running,
        heartbeat_running,
    ))
}

fn systemd_registration_path() -> Result<PathBuf> {
    if let Some(config_home) = nonempty_env("XDG_CONFIG_HOME") {
        return Ok(config_home.join("systemd/user").join(SYSTEMD_UNIT));
    }
    let home = nonempty_env("HOME").ok_or_else(|| anyhow!("HOME is unavailable"))?;
    Ok(home.join(".config/systemd/user").join(SYSTEMD_UNIT))
}

fn systemd_unit(options: &ServiceOptions) -> String {
    let command = std::iter::once(options.executable.as_os_str())
        .chain(options.recorder_arguments().iter().map(OsString::as_os_str))
        .map(systemd_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let environment = options
        .environment_path
        .as_ref()
        .map_or_else(String::new, |path| {
            format!(
                "Environment={}\n",
                systemd_environment_quote(OsStr::new(&format!("PATH={}", path.to_string_lossy())))
            )
        });
    format!(
        r#"[Unit]
Description=Codex usage history recorder
After=default.target

[Service]
Type=simple
{environment}
ExecStart={command}
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
"#
    )
}

fn install_windows_task(options: &ServiceOptions) -> Result<()> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    if windows_task_is_installed(&task_name)? {
        end_windows_task(&task_name);
    }
    register_windows_task(options, &task_name, &user_sid)?;
    run_checked(
        Command::new("schtasks.exe").args(["/Run", "/TN", &task_name]),
        "schtasks /Run",
    )
}

fn uninstall_windows_task(_options: &ServiceOptions) -> Result<()> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    if !windows_task_is_installed(&task_name)? {
        return Ok(());
    }
    end_windows_task(&task_name);
    let output = Command::new("schtasks.exe")
        .args(["/Delete", "/TN", &task_name, "/F"])
        .output()
        .context("could not run schtasks /Delete")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("schtasks /Delete failed: {}", output_detail(&output))
    }
}

fn windows_task_status(
    _options: &ServiceOptions,
    heartbeat: Option<DateTime<Utc>>,
    heartbeat_running: bool,
) -> Result<ServiceStatus> {
    let user_sid = windows_current_user_sid()?;
    let task_name = windows_task_name(&user_sid);
    let installed = windows_task_is_installed(&task_name)?;
    let mut status = service_status(
        "windows-task-scheduler",
        installed,
        installed && heartbeat_running,
        None,
        heartbeat,
        false,
        heartbeat_running,
    );
    if installed && heartbeat.is_none() {
        status.state = ServiceState::Installed;
        status.detail = "registered; waiting for the first recorder heartbeat".to_string();
    }
    Ok(status)
}

fn windows_task_is_installed(task_name: &str) -> Result<bool> {
    Ok(Command::new("schtasks.exe")
        .args(["/Query", "/TN", task_name])
        .output()
        .context("could not run schtasks /Query")?
        .status
        .success())
}

fn end_windows_task(task_name: &str) {
    let _ = Command::new("schtasks.exe")
        .args(["/End", "/TN", task_name])
        .output();
}

fn register_windows_task(options: &ServiceOptions, task_name: &str, user_sid: &str) -> Result<()> {
    let parent = options
        .status_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let (xml_path, mut file) = create_temporary_file(parent, OsStr::new("recorder-task.xml"))
        .context("could not create temporary Task Scheduler XML")?;
    let xml = windows_task_xml(options, user_sid);
    let command_xml_path = xml_path.clone();
    let registration_result = (move || -> Result<()> {
        file.write_all(xml.as_bytes())
            .context("could not write temporary Task Scheduler XML")?;
        file.sync_all()
            .context("could not flush temporary Task Scheduler XML")?;
        drop(file);
        let mut command = Command::new("schtasks.exe");
        command
            .args(["/Create", "/TN", task_name, "/XML"])
            .arg(&command_xml_path)
            .arg("/F");
        run_checked(&mut command, "schtasks /Create /XML")
    })();
    let cleanup_result = fs::remove_file(&xml_path);
    registration_result?;
    if let Err(error) = cleanup_result
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error)
            .with_context(|| format!("could not remove temporary {}", xml_path.display()));
    }
    Ok(())
}

fn windows_task_xml(options: &ServiceOptions, user_sid: &str) -> String {
    let command = xml_escape(&options.executable.to_string_lossy());
    let arguments = xml_escape(&windows_recorder_arguments(options));
    let user_sid = xml_escape(user_sid);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Continuously record Codex usage history for the current user.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user_sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="RecorderUser">
      <UserId>{user_sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>255</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="RecorderUser">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn windows_recorder_arguments(options: &ServiceOptions) -> String {
    let mut arguments = Vec::new();
    if let Some(path) = options.environment_path.as_ref() {
        arguments.push(OsString::from("--service-path"));
        arguments.push(path.clone());
    }
    arguments.extend(options.recorder_arguments());
    arguments
        .iter()
        .map(OsString::as_os_str)
        .map(quote_windows_argument)
        .collect::<Vec<_>>()
        .join(" ")
}

fn windows_task_name(user_sid: &str) -> String {
    format!(
        "{WINDOWS_TASK_PREFIX}-{:016x}",
        stable_hash(user_sid.as_bytes())
    )
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(windows)]
fn windows_current_user_sid() -> Result<String> {
    use std::mem;
    use std::ptr;
    use std::slice;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::core::PWSTR;

    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token points to writable storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error()).context("could not open the current process token");
    }
    let result = (|| -> Result<String> {
        let mut required = 0_u32;
        // SAFETY: a zero-length query with a null buffer asks Windows for the required size.
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(io::Error::last_os_error())
                .context("could not size the current user token");
        }
        let word_size = mem::size_of::<usize>();
        let words = usize::try_from(required)
            .unwrap_or(usize::MAX)
            .saturating_add(word_size - 1)
            / word_size;
        let mut buffer = vec![0_usize; words];
        // SAFETY: buffer is aligned for TOKEN_USER and has at least `required` writable bytes.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error())
                .context("could not read the current user token");
        }
        // SAFETY: GetTokenInformation(TokenUser) initialized the buffer as TOKEN_USER.
        let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut string_sid: PWSTR = ptr::null_mut();
        // SAFETY: token_user contains the SID owned by the token buffer and string_sid is writable.
        if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut string_sid) } == 0 {
            return Err(io::Error::last_os_error())
                .context("could not format the current user SID");
        }
        let mut length = 0_usize;
        // SAFETY: ConvertSidToStringSidW returns a valid null-terminated UTF-16 allocation.
        unsafe {
            while *string_sid.add(length) != 0 {
                length += 1;
            }
        }
        // SAFETY: the allocation contains `length` initialized UTF-16 code units.
        let sid = String::from_utf16_lossy(unsafe { slice::from_raw_parts(string_sid, length) });
        // SAFETY: ConvertSidToStringSidW allocated string_sid with LocalAlloc.
        unsafe {
            LocalFree(string_sid.cast());
        }
        Ok(sid)
    })();
    // SAFETY: token was returned by OpenProcessToken and has not yet been closed.
    unsafe {
        CloseHandle(token);
    }
    result
}

#[cfg(not(windows))]
fn windows_current_user_sid() -> Result<String> {
    bail!("the current Windows user SID is unavailable on this platform")
}

fn service_status(
    platform: &'static str,
    installed: bool,
    running: bool,
    registration_path: Option<PathBuf>,
    heartbeat: Option<DateTime<Utc>>,
    manager_reports_running: bool,
    heartbeat_recent: bool,
) -> ServiceStatus {
    let state = if !installed {
        ServiceState::NotInstalled
    } else if running {
        ServiceState::Running
    } else {
        ServiceState::Stopped
    };
    let detail = if manager_reports_running && heartbeat_recent {
        "service manager reports the recorder active; recent history heartbeat observed".to_string()
    } else if manager_reports_running {
        "service manager reports the recorder active, but no recent history heartbeat was observed"
            .to_string()
    } else if running {
        "recent recorder heartbeat observed".to_string()
    } else if installed && heartbeat_recent {
        "registered manager is not active; the last heartbeat is still recent".to_string()
    } else if installed {
        "registered, but no recent recorder heartbeat was observed".to_string()
    } else {
        "no recorder registration found".to_string()
    };
    ServiceStatus {
        platform,
        state,
        installed,
        running,
        registration_path,
        last_history_heartbeat: heartbeat,
        detail,
    }
}

fn run_checked(command: &mut Command, description: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("could not run {description}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("{description} failed: {}", output_detail(&output))
    }
}

fn output_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stdout
    }
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote(value: &OsStr) -> String {
    systemd_quote_inner(value, true)
}

fn systemd_environment_quote(value: &OsStr) -> String {
    systemd_quote_inner(value, false)
}

fn systemd_quote_inner(value: &OsStr, escape_dollar: bool) -> String {
    let value = value.to_string_lossy();
    let mut escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('%', "%%");
    if escape_dollar {
        escaped = escaped.replace('$', "$$");
    }
    format!("\"{escaped}\"")
}

/// Quotes one argv element according to the CommandLineToArgvW/CRT rules.
fn quote_windows_argument(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return value.into_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0_usize;
    for character in value.chars() {
        if character == '\\' {
            backslashes += 1;
            continue;
        }
        if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2).saturating_add(1)));
            quoted.push('"');
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(character);
        }
        backslashes = 0;
    }
    quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2)));
    quoted.push('"');
    quoted
}

fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("service"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        sync_directory(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_file(parent: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique service temporary file",
    ))
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;

    fn options(root: &Path) -> ServiceOptions {
        let mut options = ServiceOptions::new(
            root.join("bin/codex usage monit.exe"),
            root.join("Codex Home"),
            root.join("State Dir/history-v1"),
            root.join("State Dir/recorder-status.json"),
            Some(root.join("State Dir/perf log.jsonl")),
        );
        options.codex_bin = Some(root.join("Codex & $% tools/codex.cmd"));
        options.environment_path = Some(OsString::from("/opt/codex & tools/bin:/usr/bin"));
        options.lookback_days = 11;
        options.max_files = 777;
        options.active_grace_minutes = 9;
        options.redact_content = true;
        options.no_rollout_cache = true;
        options
    }

    #[test]
    fn recorder_arguments_preserve_paths_as_distinct_argv_elements() {
        let root = Path::new("/tmp/root with spaces");
        let arguments = options(root).recorder_arguments();
        assert_eq!(arguments[0], "--codex-home");
        assert_eq!(arguments[1], root.join("Codex Home").as_os_str());
        assert!(arguments.contains(&OsString::from("--codex-bin")));
        assert!(arguments.contains(&root.join("Codex & $% tools/codex.cmd").into_os_string()));
        assert!(arguments.windows(2).any(|pair| pair == ["--days", "11"]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--max-files", "777"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--active-grace-minutes", "9"])
        );
        assert!(arguments.contains(&OsString::from("--redact-content")));
        assert!(arguments.contains(&OsString::from("--no-rollout-cache")));
        assert!(arguments.contains(&OsString::from("record")));
        assert!(arguments.contains(&root.join("State Dir/history-v1").into_os_string()));
        assert!(arguments.contains(&root.join("State Dir/perf log.jsonl").into_os_string()));
    }

    #[test]
    fn launchd_and_systemd_render_escaped_argument_arrays() {
        let root = Path::new("/tmp/a & b");
        let options = options(root);
        let plist = launchd_plist(&options);
        assert!(plist.contains("/tmp/a &amp; b"));
        assert!(plist.contains("Codex &amp; $% tools/codex.cmd"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("/opt/codex &amp; tools/bin:/usr/bin"));
        let unit = systemd_unit(&options);
        assert!(unit.contains("ExecStart=\"/tmp/a & b/bin/codex usage monit.exe\""));
        assert!(unit.contains("Codex & $$%% tools/codex.cmd"));
        assert!(unit.contains("Environment=\"PATH=/opt/codex & tools/bin:/usr/bin\""));
        assert!(unit.contains("Restart=on-failure"));
        assert!(!unit.contains("/bin/sh"));
        assert_eq!(
            systemd_quote(OsStr::new("/tmp/$cash/%profile")),
            r#""/tmp/$$cash/%%profile""#
        );
    }

    #[test]
    fn windows_command_line_quotes_spaces_quotes_and_trailing_slashes() {
        assert_eq!(
            quote_windows_argument(OsStr::new(r"C:\Program Files\monit.exe")),
            r#""C:\Program Files\monit.exe""#
        );
        assert_eq!(
            quote_windows_argument(OsStr::new(r#"a "quoted" value"#)),
            r#""a \"quoted\" value""#
        );
        assert_eq!(
            quote_windows_argument(OsStr::new(r"C:\path with space\")),
            r#""C:\path with space\\""#
        );
        let options = options(Path::new(r"C:\Users\A B"));
        let arguments = windows_recorder_arguments(&options);
        assert!(arguments.starts_with(
            r#"--service-path "/opt/codex & tools/bin:/usr/bin" --codex-home "C:\Users\A B/"#
        ));
        assert!(arguments.contains(r#"--codex-bin "C:\Users\A B/Codex & $% tools/codex.cmd""#));
        assert!(arguments.contains("--days 11 --max-files 777 --active-grace-minutes 9"));
        assert!(arguments.contains("--redact-content --no-rollout-cache"));
        assert!(arguments.contains("record --foreground"));
        let xml = windows_task_xml(&options, "S-1-5-21-1234");
        assert!(xml.contains(r#"<Command>C:\Users\A B/bin/codex usage monit.exe</Command>"#));
        assert!(xml.contains(r#"--service-path &quot;/opt/codex &amp; tools/bin:/usr/bin&quot;"#));
        assert!(xml.contains("Codex &amp; $% tools/codex.cmd"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert_eq!(
            windows_task_name("S-1-5-21-1234"),
            windows_task_name("S-1-5-21-1234")
        );
        assert_ne!(
            windows_task_name("S-1-5-21-1234"),
            windows_task_name("S-1-5-21-5678")
        );

        let mut inherited_path = options;
        inherited_path.environment_path = None;
        assert!(!windows_recorder_arguments(&inherited_path).contains("--service-path"));
    }

    #[test]
    fn offline_recorder_preserves_offline_mode_without_requiring_codex() {
        let root = Path::new("/tmp/offline");
        let mut options = options(root);
        options.offline = true;
        options.codex_bin = None;
        let arguments = options.recorder_arguments();
        assert!(arguments.contains(&OsString::from("--offline")));
        assert!(!arguments.contains(&OsString::from("--codex-bin")));
    }

    #[test]
    fn status_file_round_trips_atomically_and_tracks_freshness() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/recorder-status.json");
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        let mut status = RecorderStatusFile::started(now, "test-history".to_string());
        status.record_success(now + Duration::minutes(1));
        write_recorder_status(&path, &status).unwrap();
        assert_eq!(read_recorder_status(&path).unwrap(), Some(status.clone()));
        assert!(status.heartbeat_is_recent(now + Duration::minutes(12)));
        assert!(!status.heartbeat_is_recent(now + Duration::minutes(14)));
        assert!(!status.heartbeat_is_recent(now - Duration::minutes(2)));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        status.record_degraded(now + Duration::minutes(2), "account collection is stale");
        assert_eq!(
            status.last_history_heartbeat,
            Some(now + Duration::minutes(2))
        );
        assert_eq!(
            status.last_error.as_deref(),
            Some("account collection is stale")
        );
        status.record_heartbeat(now + Duration::minutes(3));
        assert_eq!(
            status.last_error.as_deref(),
            Some("account collection is stale")
        );
        status.record_success(now + Duration::minutes(4));
        assert!(status.last_error.is_none());
    }

    #[test]
    fn status_file_v2_tracks_long_intervals_and_reads_v1_files() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recorder-status.json");
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        let mut status =
            RecorderStatusFile::started_with_interval(now, "test-history".to_string(), 3_600);
        status.record_success(now);
        write_recorder_status(&path, &status).unwrap();

        let loaded = read_recorder_status(&path).unwrap().unwrap();
        assert_eq!(loaded.schema_version, 2);
        assert_eq!(loaded.heartbeat_interval_seconds, Some(3_600));
        assert!(loaded.heartbeat_is_recent(now + Duration::minutes(62)));
        assert!(!loaded.heartbeat_is_recent(now + Duration::seconds(3_721)));

        let v1 = serde_json::json!({
            "schemaVersion": 1,
            "historyNamespace": "test-history",
            "pid": 42,
            "startedAt": now,
            "lastAttemptAt": now,
            "lastHistoryHeartbeat": now,
            "lastError": null
        });
        fs::write(&path, serde_json::to_vec(&v1).unwrap()).unwrap();
        let loaded = read_recorder_status(&path).unwrap().unwrap();
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.heartbeat_interval_seconds, None);
        assert!(loaded.heartbeat_is_recent(now + Duration::minutes(12)));
        assert!(!loaded.heartbeat_is_recent(now + Duration::seconds(721)));
    }

    #[test]
    fn launchd_uninstall_falls_back_to_the_service_target_and_is_idempotent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("missing.plist");
        let domain = "gui/501";
        let target = format!("{domain}/{SERVICE_LABEL}");
        let mut operations = Vec::new();

        uninstall_launchd_registration(&path, domain, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                LaunchdOperation::BootoutService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "service is not loaded".to_string(),
                },
                LaunchdOperation::PrintService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "Could not find service in domain".to_string(),
                },
                LaunchdOperation::BootoutRegistration { .. } => {
                    panic!("a missing registration must not be booted out by path")
                }
            })
        })
        .unwrap();

        assert_eq!(
            operations,
            vec![
                LaunchdOperation::BootoutService {
                    target: target.clone(),
                },
                LaunchdOperation::PrintService { target },
            ]
        );
    }

    #[test]
    fn launchd_uninstall_preserves_registration_when_the_job_remains_loaded() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recorder.plist");
        fs::write(&path, "registration").unwrap();
        let domain = "gui/501";
        let mut operations = Vec::new();

        let error = uninstall_launchd_registration(&path, domain, |operation| {
            operations.push(operation.clone());
            Ok(match operation {
                LaunchdOperation::PrintService { .. } => LaunchdOperationResult {
                    success: true,
                    detail: String::new(),
                },
                LaunchdOperation::BootoutRegistration { .. }
                | LaunchdOperation::BootoutService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "permission denied".to_string(),
                },
            })
        })
        .unwrap_err();

        assert!(error.to_string().contains("service remains loaded"));
        assert!(error.to_string().contains("permission denied"));
        assert!(path.exists());
        assert!(matches!(
            operations.as_slice(),
            [
                LaunchdOperation::BootoutRegistration { .. },
                LaunchdOperation::BootoutService { .. },
                LaunchdOperation::PrintService { .. }
            ]
        ));
    }

    #[test]
    fn launchd_uninstall_preserves_registration_when_inspection_fails() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("recorder.plist");
        fs::write(&path, "registration").unwrap();

        let error = uninstall_launchd_registration(&path, "gui/501", |operation| {
            Ok(match operation {
                LaunchdOperation::BootoutRegistration { .. } => LaunchdOperationResult {
                    success: true,
                    detail: String::new(),
                },
                LaunchdOperation::PrintService { .. } => LaunchdOperationResult {
                    success: false,
                    detail: "permission denied".to_string(),
                },
                LaunchdOperation::BootoutService { .. } => {
                    panic!("a successful registration bootout needs no target fallback")
                }
            })
        })
        .unwrap_err();

        assert!(error.to_string().contains("could not verify"));
        assert!(error.to_string().contains("permission denied"));
        assert!(path.exists());
    }

    #[test]
    fn service_status_distinguishes_registration_and_heartbeat() {
        let now = Utc::now();
        let stopped = service_status("test", true, false, None, Some(now), false, true);
        assert_eq!(stopped.state, ServiceState::Stopped);
        assert!(stopped.detail.contains("manager is not active"));
        let running = service_status("test", true, true, None, Some(now), false, true);
        assert_eq!(running.state, ServiceState::Running);
        let missing = service_status("test", false, false, None, None, false, false);
        assert_eq!(missing.state, ServiceState::NotInstalled);
    }
}
