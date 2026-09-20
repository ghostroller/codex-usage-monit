//! Explicit administrator-managed SCM services. This store never shares the
//! current-user install receipt, launcher pointer, or Task Scheduler trust.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, BufRead, Read},
    path::{Path, PathBuf},
    process::Command,
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_CANNOT_ACCEPT_CTRL,
        ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE,
    },
    System::Services::*,
};

mod process;
mod runtime;
mod security;

const RECEIPT: &str = "machine-receipt.json";
const MAX_METADATA: u64 = 128 * 1024;
const MAX_BINARY: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineRecorderOptions {
    pub codex_home: PathBuf,
    pub history_dir: PathBuf,
    pub status_file: PathBuf,
    pub codex_bin: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    pub remotes_config_file: Option<PathBuf>,
    pub project_mapping_file: Option<PathBuf>,
    pub offline: bool,
    pub redact_content: bool,
    pub no_rollout_cache: bool,
    pub lookback_days: u32,
    pub max_files: usize,
    pub active_grace_minutes: u64,
    pub environment_path: Option<String>,
}

pub struct MachineInstallOptions {
    pub name: String,
    pub account: String,
    pub recorder: MachineRecorderOptions,
    /// Configure automatic boot startup; false leaves a demand-start service.
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineAction {
    Status,
    Start,
    Stop,
    Restart,
    Upgrade,
    Uninstall,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineReport {
    pub backend: &'static str,
    pub name: String,
    pub account: String,
    pub account_sid: String,
    pub installation_root: PathBuf,
    pub executable: PathBuf,
    pub version: String,
    pub build_id: String,
    pub automatic_start: bool,
    pub manager_state: String,
    pub manager_pid: Option<u32>,
    pub recorder_pid: Option<u32>,
    pub healthy: bool,
    pub last_history_heartbeat: Option<DateTime<Utc>>,
    pub last_exit_code: u32,
    pub phase: String,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MachineVersion {
    version: String,
    build_id: String,
    sha256: String,
    executable: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingUpgrade {
    previous: MachineVersion,
    target: MachineVersion,
    was_running: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MachineReceipt {
    schema_version: u32,
    owner: String,
    name: String,
    account: String,
    account_sid: String,
    root: PathBuf,
    selected: MachineVersion,
    minimum_updater_version: String,
    recorder: MachineRecorderOptions,
    automatic_start: bool,
    phase: String,
    pending: Option<PendingUpgrade>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "machine_name_invalid: use 1-64 letters, digits, '-' or '_'"
    );
    Ok(())
}

fn root_for(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(security::machine_base()?.join(name))
}

/// Machine processes do not consult a current-user launcher pointer. Restrict
/// this exception to the fixed Program Files store and its protected ACLs.
pub(crate) fn is_machine_executable(executable: &Path) -> Result<bool> {
    let base = security::machine_base()?;
    if !base.exists() {
        return Ok(false);
    }
    let base = base.canonicalize()?;
    let executable = executable.canonicalize()?;
    let Ok(relative) = executable.strip_prefix(&base) else {
        return Ok(false);
    };
    let components: Vec<_> = relative.components().collect();
    ensure!(
        components.len() == 4
            && components[1].as_os_str() == "versions"
            && components[3].as_os_str() == "codex-usage-monit.exe",
        "machine_executable_invalid: unexpected executable in protected machine store"
    );
    validate_name(
        components[0]
            .as_os_str()
            .to_str()
            .context("machine_name_invalid")?,
    )?;
    for path in executable
        .ancestors()
        .take_while(|path| path.starts_with(&base))
    {
        security::validate_machine_path(path)?;
    }
    Ok(true)
}

fn validate_recorder(options: &MachineRecorderOptions, account_sid: &str) -> Result<()> {
    ensure!(
        options.lookback_days > 0 && options.max_files > 0,
        "machine_options_invalid: days/max-files must be positive"
    );
    ensure!(
        options.offline || options.codex_bin.is_some(),
        "machine_codex_required: online machine recording requires an explicit --codex-bin"
    );
    for path in [
        &options.codex_home,
        &options.history_dir,
        &options.status_file,
    ] {
        ensure!(
            path.is_absolute(),
            "machine_path_invalid: explicit absolute Codex/history/status paths are required"
        );
        let existing = path
            .ancestors()
            .find(|path| path.exists())
            .context("machine_path_invalid: no existing ancestor")?;
        security::validate_runtime_path(existing, account_sid).with_context(|| format!(
            "machine_runtime_path_untrusted: {}; provision this service account's profile and private data access before installation", path.display()))?;
    }
    for path in [
        &options.codex_bin,
        &options.config_dir,
        &options.remotes_config_file,
        &options.project_mapping_file,
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            path.is_absolute(),
            "machine_path_invalid: optional paths must also be absolute"
        );
        crate::source_identity::reject_windows_reparse_components(path, "machine recorder option")?;
    }
    for path in [
        &options.history_dir,
        options
            .status_file
            .parent()
            .context("machine_status_invalid: status requires a parent")?,
    ]
    .into_iter()
    .chain(options.config_dir.as_deref())
    {
        security::validate_private_output(path, account_sid, true)
            .with_context(|| format!("machine_private_output_untrusted: {}", path.display()))?;
    }
    for path in [
        &options.status_file,
        &options.status_file.with_extension("scm.log"),
    ] {
        security::validate_private_output(path, account_sid, false)?;
    }
    if let Some(path) = &options.codex_bin {
        security::validate_executable_path(path, account_sid)?;
    }
    ensure!(
        options
            .environment_path
            .as_ref()
            .is_none_or(|path| !path.contains('\0')),
        "machine_options_invalid: PATH contains NUL"
    );
    Ok(())
}

struct MachineLock(fs::File);
impl Drop for MachineLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}
fn lock(root: &Path, account_sid: &str) -> Result<MachineLock> {
    security::validate_machine_path(root)?;
    let path = root.join("machine.lock");
    let file = if path.exists() {
        security::validate_machine_path(&path)?;
        fs::OpenOptions::new().read(true).write(true).open(&path)?
    } else {
        security::create_file(&path, account_sid)?
    };
    fs2::FileExt::try_lock_exclusive(&file)
        .context("machine_busy: another machine service mutation is active")?;
    Ok(MachineLock(file))
}

fn save(receipt: &MachineReceipt) -> Result<()> {
    security::write_file(
        &receipt.root.join(RECEIPT),
        &receipt.account_sid,
        &serde_json::to_vec_pretty(receipt)?,
    )
}

fn load(name: &str) -> Result<MachineReceipt> {
    let root = root_for(name)?;
    security::validate_machine_path(&root)?;
    let receipt: MachineReceipt =
        serde_json::from_slice(&security::read_file(&root.join(RECEIPT), MAX_METADATA)?)?;
    ensure!(
        receipt.schema_version == 1
            && receipt.owner == "machine"
            && receipt.name == name
            && receipt.root == root,
        "machine_receipt_invalid: ownership, root or service identity differs"
    );
    ensure!(
        receipt.account_sid == security::account_sid(&receipt.account)?,
        "machine_account_changed: configured account now resolves to another SID"
    );
    validate_version(&receipt, &receipt.selected)?;
    if let Some(pending) = &receipt.pending {
        validate_version(&receipt, &pending.previous)?;
        validate_version(&receipt, &pending.target)?;
    }
    Ok(receipt)
}

fn validate_version(receipt: &MachineReceipt, version: &MachineVersion) -> Result<()> {
    ensure!(
        crate::release::is_hash(&version.sha256) && crate::release::is_hash(&version.build_id),
        "machine_version_invalid: invalid checksum/build"
    );
    ensure!(
        version
            .version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
            && !version.version.is_empty()
            && version.version.len() <= 80,
        "machine_version_invalid: invalid version"
    );
    let expected = receipt
        .root
        .join("versions")
        .join(format!("{}-{}", version.version, version.sha256))
        .join("codex-usage-monit.exe");
    ensure!(
        version.executable == expected,
        "machine_version_invalid: executable escaped immutable machine storage"
    );
    security::validate_machine_path(version.executable.parent().unwrap())?;
    ensure!(
        digest(&security::read_file(&version.executable, MAX_BINARY)?) == version.sha256,
        "machine_checksum_mismatch: registered machine executable changed"
    );
    Ok(())
}

fn prepare_version(root: &Path, account_sid: &str) -> Result<MachineVersion> {
    let current = std::env::current_exe()?;
    let file = fs::File::open(&current)?;
    ensure!(
        file.metadata()?.len() <= MAX_BINARY,
        "machine_binary_invalid: executable too large"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BINARY + 1).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_BINARY,
        "machine_binary_invalid: executable too large"
    );
    let info = crate::remote_agent_manager::AgentInfo::local();
    let sha256 = digest(&bytes);
    let versions = root.join("versions");
    security::create_directory(&versions, account_sid)?;
    let directory = versions.join(format!("{}-{sha256}", info.version));
    security::create_directory(&directory, account_sid)?;
    let executable = directory.join("codex-usage-monit.exe");
    if executable.exists() {
        ensure!(
            digest(&security::read_file(&executable, MAX_BINARY)?) == sha256,
            "machine_version_conflict: immutable bytes differ"
        );
    } else {
        security::write_file(&executable, account_sid, &bytes)?;
    }
    let output = crate::bounded_process::output(
        Command::new(&executable).args(["remote-agent", "info", "--sha256"]),
        Duration::from_secs(30),
        MAX_METADATA as usize,
    )?;
    let actual: crate::remote_agent_manager::AgentInfo = serde_json::from_slice(&output.stdout)
        .context("machine_candidate_invalid: identity is unavailable")?;
    ensure!(
        output.status.success()
            && actual.build_id == info.build_id
            && actual.version == info.version
            && actual.target == info.target
            && actual.protocol_version == info.protocol_version
            && actual.executable_sha256.as_deref() == Some(&sha256),
        "machine_candidate_invalid: copied executable identity differs"
    );
    Ok(MachineVersion {
        version: info.version,
        build_id: info.build_id,
        sha256,
        executable,
    })
}

struct Secret(Vec<u16>);
impl Drop for Secret {
    fn drop(&mut self) {
        for value in &mut self.0 {
            unsafe { ptr::write_volatile(value, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}
fn password_from_stdin(enabled: bool) -> Result<Secret> {
    ensure!(
        enabled,
        "machine_password_required: provide --password-stdin; passwords are never accepted in command arguments"
    );
    read_password(io::stdin().lock())
}

fn read_password(reader: impl BufRead) -> Result<Secret> {
    let mut bytes = Vec::new();
    reader.take(8193).read_until(b'\n', &mut bytes)?;
    let result = (|| {
        ensure!(
            bytes.len() <= 8192,
            "machine_password_invalid: password is oversized"
        );
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        let text = std::str::from_utf8(&bytes)
            .context("machine_password_invalid: expected UTF-8 stdin")?;
        ensure!(
            !text.is_empty(),
            "machine_password_invalid: empty passwords are not supported"
        );
        Ok(Secret(security::wide(text)?))
    })();
    for byte in &mut bytes {
        unsafe { ptr::write_volatile(byte, 0) };
    }
    result
}

struct ServiceHandle(SC_HANDLE);
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}
fn manager(create: bool) -> Result<ServiceHandle> {
    let access = SC_MANAGER_CONNECT | if create { SC_MANAGER_CREATE_SERVICE } else { 0 };
    let handle = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), access) };
    ensure!(
        !handle.is_null(),
        "machine_manager_unavailable: {}",
        io::Error::last_os_error()
    );
    Ok(ServiceHandle(handle))
}
fn open_service(
    manager: &ServiceHandle,
    name: &str,
    mutate: bool,
) -> Result<Option<ServiceHandle>> {
    let name = security::wide(name)?;
    let access = if mutate {
        SERVICE_ALL_ACCESS
    } else {
        SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS
    };
    let handle = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
            return Ok(None);
        }
        return Err(error).context("machine_service_unavailable");
    }
    Ok(Some(ServiceHandle(handle)))
}

fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        if character == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        result.push(character);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}
fn image_path(receipt: &MachineReceipt, version: &MachineVersion) -> String {
    [
        version.executable.to_string_lossy().into_owned(),
        "service".into(),
        "machine".into(),
        "run".into(),
        "--name".into(),
        receipt.name.clone(),
        "--config".into(),
        receipt.root.join(RECEIPT).to_string_lossy().into_owned(),
    ]
    .iter()
    .map(|argument| quote(argument))
    .collect::<Vec<_>>()
    .join(" ")
}

fn query_state(service: &ServiceHandle) -> Result<SERVICE_STATUS_PROCESS> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut length = 0;
    ensure!(
        unsafe {
            QueryServiceStatusEx(
                service.0,
                SC_STATUS_PROCESS_INFO,
                (&raw mut status).cast(),
                std::mem::size_of_val(&status) as u32,
                &mut length,
            )
        } != 0,
        "machine_status_failed: {}",
        io::Error::last_os_error()
    );
    Ok(status)
}
fn verify_registration(service: &ServiceHandle, receipt: &MachineReceipt) -> Result<()> {
    let mut needed = 0;
    unsafe { QueryServiceConfigW(service.0, ptr::null_mut(), 0, &mut needed) };
    ensure!(
        needed > 0 && needed <= 65536,
        "machine_registration_unverifiable: invalid configuration size"
    );
    let mut buffer = vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    ensure!(
        unsafe { QueryServiceConfigW(service.0, buffer.as_mut_ptr().cast(), needed, &mut needed) }
            != 0,
        "machine_registration_unverifiable: {}",
        io::Error::last_os_error()
    );
    let configuration = unsafe { &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
    let command = unsafe { security::read_wide(configuration.lpBinaryPathName) }?;
    let account = unsafe { security::read_wide(configuration.lpServiceStartName) }?;
    let expected = image_path(receipt, &receipt.selected);
    let retained = receipt
        .pending
        .as_ref()
        .map(|pending| image_path(receipt, &pending.previous));
    ensure!(
        (command == expected || retained.as_deref() == Some(&command))
            && configuration.dwServiceType == SERVICE_WIN32_OWN_PROCESS
            && (configuration.dwStartType
                == (if receipt.automatic_start {
                    SERVICE_AUTO_START
                } else {
                    SERVICE_DEMAND_START
                })
                || (configuration.dwStartType == SERVICE_DISABLED
                    && (receipt.pending.is_some() || receipt.phase == "uninstalling")))
            && security::account_sid(&account)? == receipt.account_sid,
        "machine_registration_changed: SCM definition differs from its protected receipt; existing service preserved"
    );
    Ok(())
}

fn wait_state(service: &ServiceHandle, expected: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = query_state(service)?;
        if status.dwCurrentState == expected {
            return Ok(());
        }
        if expected == SERVICE_RUNNING && status.dwCurrentState == SERVICE_STOPPED {
            bail!(
                "machine_start_failed: service stopped (Win32 {}, service {}); check service account logon rights, data access and recorder log",
                status.dwWin32ExitCode,
                status.dwServiceSpecificExitCode
            );
        }
        ensure!(
            Instant::now() < deadline,
            "machine_timeout: service did not reach requested state"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[derive(Debug, PartialEq, Eq)]
enum StopStep {
    Complete,
    Wait,
    RequestStop,
}

fn stop_step(state: u32) -> StopStep {
    match state {
        SERVICE_STOPPED => StopStep::Complete,
        // START_PENDING deliberately advertises no accepted controls. Waiting
        // also handles a previously sent STOP without issuing it repeatedly.
        SERVICE_START_PENDING | SERVICE_STOP_PENDING => StopStep::Wait,
        _ => StopStep::RequestStop,
    }
}

fn stop_service(service: &ServiceHandle) -> Result<()> {
    // Startup readiness is bounded at 90s; allow its failure cleanup or the
    // subsequent 25s graceful stop before diagnosing a stuck SCM process.
    let deadline = Instant::now() + Duration::from_secs(130);
    loop {
        match stop_step(query_state(service)?.dwCurrentState) {
            StopStep::Complete => return Ok(()),
            StopStep::Wait => {}
            StopStep::RequestStop => {
                let mut ignored = SERVICE_STATUS::default();
                if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut ignored) } == 0 {
                    let error = io::Error::last_os_error();
                    if !matches!(error.raw_os_error(), Some(code) if code == ERROR_SERVICE_NOT_ACTIVE as i32 || code == ERROR_SERVICE_CANNOT_ACCEPT_CTRL as i32)
                    {
                        return Err(error).context("machine_stop_failed");
                    }
                    // State may have changed since QueryServiceStatusEx.
                    // Re-query within the same deadline rather than failing
                    // upgrade/uninstall during a legitimate transition.
                }
            }
        }
        ensure!(
            Instant::now() < deadline,
            "machine_stop_timeout: service did not finish starting/stopping within 130 seconds"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn start_service(service: &ServiceHandle) -> Result<()> {
    if unsafe { StartServiceW(service.0, 0, ptr::null()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
            return Err(error).context("machine_start_failed: verify the named account password and Log on as a service right");
        }
    }
    // Our ServiceMain reports RUNNING only after the exact child has persisted
    // a new history heartbeat; a successful StartService call is insufficient.
    wait_state(service, SERVICE_RUNNING, Duration::from_secs(100))
}

fn configure_recovery(service: &ServiceHandle) -> Result<()> {
    let mut actions = [30_000, 60_000, 120_000].map(|delay| SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: delay,
    });
    let settings = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: ptr::null_mut(),
        lpCommand: ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    let non_crash = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: 1,
    };
    unsafe {
        ensure!(
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_FAILURE_ACTIONS,
                (&raw const settings).cast()
            ) != 0,
            "machine_recovery_configuration_failed: {}",
            io::Error::last_os_error()
        );
        ensure!(
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
                (&raw const non_crash).cast()
            ) != 0,
            "machine_recovery_configuration_failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

fn configure_startup(service: &ServiceHandle, startup: u32) -> Result<()> {
    ensure!(
        unsafe {
            ChangeServiceConfigW(
                service.0,
                SERVICE_NO_CHANGE,
                startup,
                SERVICE_NO_CHANGE,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
            )
        } != 0,
        "machine_startup_configuration_failed: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

fn prepare_upgrade(
    receipt: &mut MachineReceipt,
    target: &MachineVersion,
    was_running: bool,
) -> Result<()> {
    crate::update::ensure_not_downgrade(
        &target.version,
        &target.build_id,
        &receipt.minimum_updater_version,
        Some(&receipt.selected.build_id),
        false,
    )?;
    if let Some(pending) = &receipt.pending {
        ensure!(
            pending.target == *target,
            "machine_update_pending: resume with the previously prepared executable"
        );
    } else {
        receipt.pending = Some(PendingUpgrade {
            previous: receipt.selected.clone(),
            target: target.clone(),
            was_running,
        });
    }
    receipt.phase = "prepared".into();
    Ok(())
}

fn select_upgrade(receipt: &mut MachineReceipt) -> Result<()> {
    receipt.selected = receipt
        .pending
        .as_ref()
        .context("machine_update_missing: no prepared upgrade")?
        .target
        .clone();
    receipt
        .minimum_updater_version
        .clone_from(&receipt.selected.version);
    receipt.phase = "replacing".into();
    Ok(())
}

pub fn install(options: MachineInstallOptions, password_stdin: bool) -> Result<MachineReport> {
    security::require_administrator()?;
    validate_name(&options.name)?;
    let account_sid = security::account_sid(&options.account)?;
    validate_recorder(&options.recorder, &account_sid)?;
    let password = password_from_stdin(password_stdin)?;
    let manager = manager(true)?;
    if let Some(service) = open_service(&manager, &options.name, true)? {
        let mut receipt = load(&options.name).context(
            "machine_service_exists: no verified receipt owns this existing registration",
        )?;
        let _lock = lock(&receipt.root, &receipt.account_sid)?;
        receipt = load(&options.name)?;
        verify_registration(&service, &receipt)?;
        ensure!(
            receipt.phase == "installing"
                && receipt.pending.is_none()
                && receipt.account_sid == account_sid
                && receipt.recorder == options.recorder
                && receipt.automatic_start == options.enabled,
            "machine_service_exists: only the identical incomplete installation can resume; use upgrade/start/stop for installed services"
        );
        let account = security::wide(&receipt.account)?;
        ensure!(
            unsafe {
                ChangeServiceConfigW(
                    service.0,
                    SERVICE_NO_CHANGE,
                    SERVICE_NO_CHANGE,
                    SERVICE_NO_CHANGE,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    account.as_ptr(),
                    password.0.as_ptr(),
                    ptr::null(),
                )
            } != 0,
            "machine_install_resume_failed: {}",
            io::Error::last_os_error()
        );
        drop(password);
        configure_recovery(&service)?;
        if receipt.automatic_start {
            start_service(&service)?;
        }
        receipt.phase = "complete".into();
        save(&receipt)?;
        return report(&receipt, Some(&service), None);
    }
    let base = security::machine_base()?;
    security::create_directory(&base, "BU")?;
    let root = root_for(&options.name)?;
    security::create_directory(&root, &account_sid)?;
    let _lock = lock(&root, &account_sid)?;
    let previous = if root.join(RECEIPT).exists() {
        let previous = load(&options.name)?;
        ensure!(
            matches!(
                previous.phase.as_str(),
                "installing" | "uninstalled" | "uninstalling"
            ) && previous.account_sid == account_sid
                && previous.pending.is_none(),
            "machine_receipt_exists: this retained installation requires its original account and explicit recovery"
        );
        Some(previous)
    } else {
        None
    };
    let selected = prepare_version(&root, &account_sid)?;
    if let Some(previous) = previous {
        crate::update::ensure_not_downgrade(
            &selected.version,
            &selected.build_id,
            &previous.minimum_updater_version,
            Some(&previous.selected.build_id),
            false,
        )?;
    }
    let mut receipt = MachineReceipt {
        schema_version: 1,
        owner: "machine".into(),
        name: options.name,
        account: options.account,
        account_sid,
        root,
        minimum_updater_version: selected.version.clone(),
        selected,
        recorder: options.recorder,
        automatic_start: options.enabled,
        phase: "installing".into(),
        pending: None,
    };
    save(&receipt)?;
    let name = security::wide(&receipt.name)?;
    let display = security::wide(&format!("Codex usage recorder ({})", receipt.name))?;
    let command = security::wide(&image_path(&receipt, &receipt.selected))?;
    let account = security::wide(&receipt.account)?;
    let service = unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            if receipt.automatic_start {
                SERVICE_AUTO_START
            } else {
                SERVICE_DEMAND_START
            },
            SERVICE_ERROR_NORMAL,
            command.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            account.as_ptr(),
            password.0.as_ptr(),
        )
    };
    drop(password);
    ensure!(
        !service.is_null(),
        "machine_install_failed: {}; prepared protected receipt retained for recovery",
        io::Error::last_os_error()
    );
    let service = ServiceHandle(service);
    verify_registration(&service, &receipt)?;
    configure_recovery(&service)?;
    if receipt.automatic_start {
        start_service(&service)?;
    }
    receipt.phase = "complete".into();
    save(&receipt)?;
    report(&receipt, Some(&service), None)
}

pub fn operate(name: &str, action: MachineAction) -> Result<MachineReport> {
    if action != MachineAction::Status {
        security::require_administrator()?;
    }
    let mut receipt = load(name)?;
    let _lock = if action == MachineAction::Status {
        None
    } else {
        Some(lock(&receipt.root, &receipt.account_sid)?)
    };
    // Re-read after taking the lock; another installer may have completed while
    // this caller was opening the receipt.
    if action != MachineAction::Status {
        receipt = load(name)?;
    }
    let manager = manager(false)?;
    let service = open_service(&manager, name, action != MachineAction::Status)?;
    if let Some(service) = &service {
        verify_registration(service, &receipt)?;
    }
    if action == MachineAction::Status {
        return report(&receipt, service.as_ref(), None);
    }
    if service.is_none() && action == MachineAction::Uninstall {
        receipt.phase = "uninstalled".into();
        receipt.pending = None;
        save(&receipt)?;
        return report(&receipt, None, Some("Service registration is absent. Protected versions and all user data are retained.".into()));
    }
    let service = service.context("machine_service_missing: protected receipt retained; use explicit uninstall before reinstalling")?;
    ensure!(
        receipt.pending.is_none()
            || matches!(
                action,
                MachineAction::Upgrade | MachineAction::Stop | MachineAction::Uninstall
            ),
        "machine_update_pending: resume the same upgrade before starting the recorder"
    );
    match action {
        MachineAction::Start => {
            start_service(&service)?;
            if receipt.phase == "installing" {
                receipt.phase = "complete".into();
                save(&receipt)?;
            }
        }
        MachineAction::Stop => stop_service(&service)?,
        MachineAction::Restart => {
            stop_service(&service)?;
            start_service(&service)?;
        }
        MachineAction::Upgrade => {
            let target = prepare_version(&receipt.root, &receipt.account_sid)?;
            let was_running = matches!(
                query_state(&service)?.dwCurrentState,
                SERVICE_RUNNING | SERVICE_START_PENDING
            );
            prepare_upgrade(&mut receipt, &target, was_running)?;
            save(&receipt)?;
            // A queued SCM recovery must not restart the old ImagePath between
            // observing STOPPED and selecting the new writer. Keep startup
            // disabled through this entire transition; retries move forward.
            configure_startup(&service, SERVICE_DISABLED)?;
            stop_service(&service)?;
            select_upgrade(&mut receipt)?;
            save(&receipt)?;
            let command = security::wide(&image_path(&receipt, &receipt.selected))?;
            ensure!(
                unsafe {
                    ChangeServiceConfigW(
                        service.0,
                        SERVICE_NO_CHANGE,
                        SERVICE_NO_CHANGE,
                        SERVICE_NO_CHANGE,
                        command.as_ptr(),
                        ptr::null(),
                        ptr::null_mut(),
                        ptr::null(),
                        ptr::null(),
                        ptr::null(),
                        ptr::null(),
                    )
                } != 0,
                "machine_upgrade_failed: {}; candidate retained for forward recovery",
                io::Error::last_os_error()
            );
            verify_registration(&service, &receipt)?;
            configure_startup(
                &service,
                if receipt.automatic_start {
                    SERVICE_AUTO_START
                } else {
                    SERVICE_DEMAND_START
                },
            )?;
            if receipt.pending.as_ref().unwrap().was_running {
                start_service(&service)?;
            }
            receipt.phase = "complete".into();
            receipt.pending = None;
            save(&receipt)?;
        }
        MachineAction::Uninstall => {
            receipt.phase = "uninstalling".into();
            save(&receipt)?;
            configure_startup(&service, SERVICE_DISABLED)?;
            stop_service(&service)?;
            ensure!(
                unsafe { DeleteService(service.0) } != 0,
                "machine_uninstall_failed: {}",
                io::Error::last_os_error()
            );
            drop(service);
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match open_service(&manager, name, false) {
                    Ok(None) => break,
                    Ok(Some(handle)) => drop(handle),
                    Err(error)
                        if error
                            .downcast_ref::<io::Error>()
                            .and_then(io::Error::raw_os_error)
                            == Some(1072) => {}
                    Err(error) => return Err(error),
                }
                ensure!(
                    Instant::now() < deadline,
                    "machine_uninstall_pending: SCM is waiting for another open service handle; retry uninstall later"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            receipt.phase = "uninstalled".into();
            receipt.pending = None;
            save(&receipt)?;
            return report(&receipt, None, Some("Service registration removed. Protected application versions and all recorder/user data are retained.".into()));
        }
        MachineAction::Status => unreachable!(),
    }
    report(&receipt, Some(&service), None)
}

fn report(
    receipt: &MachineReceipt,
    service: Option<&ServiceHandle>,
    diagnostic: Option<String>,
) -> Result<MachineReport> {
    let state = service.map(query_state).transpose()?;
    let manager_state = match state.as_ref().map(|state| state.dwCurrentState) {
        None => "not_installed",
        Some(SERVICE_STOPPED) => "stopped",
        Some(SERVICE_RUNNING) => "running",
        Some(SERVICE_START_PENDING) => "starting",
        Some(SERVICE_STOP_PENDING) => "stopping",
        _ => "other",
    };
    let status = runtime::inspect_heartbeat(receipt).ok().flatten();
    let fresh = status.as_ref().is_some_and(|status| {
        status.build_id.as_deref() == Some(&receipt.selected.build_id)
            && status.last_history_heartbeat.is_some_and(|at| {
                let age = Utc::now().signed_duration_since(at).num_seconds();
                (-5..=2 * status.heartbeat_interval_seconds.unwrap_or(60).min(3600) as i64 + 30)
                    .contains(&age)
            })
    });
    Ok(MachineReport {
        backend: "windows_scm",
        name: receipt.name.clone(),
        account: receipt.account.clone(),
        account_sid: receipt.account_sid.clone(),
        installation_root: receipt.root.clone(),
        executable: receipt.selected.executable.clone(),
        version: receipt.selected.version.clone(),
        build_id: receipt.selected.build_id.clone(),
        automatic_start: receipt.automatic_start,
        manager_state: manager_state.into(),
        manager_pid: state
            .as_ref()
            .and_then(|state| (state.dwProcessId != 0).then_some(state.dwProcessId)),
        recorder_pid: status.as_ref().map(|status| status.pid),
        healthy: manager_state == "running" && fresh,
        last_history_heartbeat: status.and_then(|status| status.last_history_heartbeat),
        last_exit_code: state.as_ref().map_or(0, |state| state.dwWin32ExitCode),
        phase: receipt.phase.clone(),
        diagnostic,
    })
}

pub fn run(name: &str, config: &Path) -> Result<i32> {
    let receipt = load(name)?;
    ensure!(
        config == receipt.root.join(RECEIPT),
        "machine_config_invalid: unexpected configuration path"
    );
    ensure!(
        security::current_sid()? == receipt.account_sid,
        "machine_account_mismatch: service is running as another account"
    );
    ensure!(
        std::env::current_exe()?.canonicalize()? == receipt.selected.executable.canonicalize()?,
        "machine_version_mismatch: service process is not the selected protected version"
    );
    runtime::dispatch(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scm_stop_waits_through_startup_and_existing_stop_without_extra_controls() {
        let normal = [
            SERVICE_START_PENDING,
            SERVICE_START_PENDING,
            SERVICE_RUNNING,
            SERVICE_STOP_PENDING,
            SERVICE_STOP_PENDING,
            SERVICE_STOPPED,
        ];
        let actions: Vec<_> = normal.into_iter().map(stop_step).collect();
        assert_eq!(
            actions,
            [
                StopStep::Wait,
                StopStep::Wait,
                StopStep::RequestStop,
                StopStep::Wait,
                StopStep::Wait,
                StopStep::Complete
            ]
        );
        // Startup can fail by itself, or a previous caller can already have
        // requested STOP. Both complete without attempting a forbidden control.
        for sequence in [
            vec![SERVICE_START_PENDING, SERVICE_STOPPED],
            vec![SERVICE_STOP_PENDING, SERVICE_STOPPED],
            vec![SERVICE_STOPPED],
        ] {
            let actions: Vec<_> = sequence.into_iter().map(stop_step).collect();
            assert!(!actions.contains(&StopStep::RequestStop));
            assert_eq!(actions.last(), Some(&StopStep::Complete));
        }
    }
    #[test]
    fn machine_names_cannot_escape_program_files_or_inject_commands() {
        for name in ["../other", "a\\b", "a b", "a\"b", "", "service;bad"] {
            assert!(validate_name(name).is_err());
        }
        validate_name("codex-user_01").unwrap();
    }
    #[test]
    fn windows_machine_argument_quoting_preserves_backslashes_and_quotes() {
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("a\\"), "\"a\\\\\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn service_password_accepts_unicode_stdin_and_rejects_invalid_input() {
        let secret = read_password(io::Cursor::new("密码 with spaces\r\n".as_bytes())).unwrap();
        assert_eq!(
            String::from_utf16(&secret.0[..secret.0.len() - 1]).unwrap(),
            "密码 with spaces"
        );
        for bytes in [
            vec![],
            b"\r\n".to_vec(),
            b"a\0b\n".to_vec(),
            vec![b'x'; 8193],
            vec![0xff],
        ] {
            assert!(read_password(io::Cursor::new(bytes)).is_err());
        }
    }

    fn recorder_fixture() -> MachineRecorderOptions {
        MachineRecorderOptions {
            codex_home: r"C:\Users\service\我的 Codex".into(),
            history_dir: r"C:\Users\service\history".into(),
            status_file: r"C:\Users\service\status.json".into(),
            codex_bin: None,
            config_dir: None,
            remotes_config_file: None,
            project_mapping_file: None,
            offline: true,
            redact_content: true,
            no_rollout_cache: false,
            lookback_days: 7,
            max_files: 500,
            active_grace_minutes: 5,
            environment_path: None,
        }
    }

    #[test]
    fn scm_upgrade_recovery_retains_candidate_floor_and_original_running_state() {
        let version = |number: &str, byte: char| MachineVersion {
            version: number.into(),
            build_id: byte.to_string().repeat(64),
            sha256: byte.to_string().repeat(64),
            executable: PathBuf::from(format!(
                r"C:\Program Files\machine\{number}\codex-usage-monit.exe"
            )),
        };
        let old = version("0.5.0", 'a');
        let target = version("0.6.0", 'b');
        let other = version("0.7.0", 'c');
        for running in [false, true] {
            let mut receipt = MachineReceipt {
                schema_version: 1,
                owner: "machine".into(),
                name: "recorder".into(),
                account: r"PC\recorder".into(),
                account_sid: "S-1-5-21-1-2-3-1001".into(),
                root: r"C:\Program Files\machine".into(),
                selected: old.clone(),
                minimum_updater_version: old.version.clone(),
                recorder: recorder_fixture(),
                automatic_start: true,
                phase: "complete".into(),
                pending: None,
            };
            prepare_upgrade(&mut receipt, &target, running).unwrap();
            assert_eq!(receipt.selected, old);
            select_upgrade(&mut receipt).unwrap();
            // This is the durable state before changing ImagePath. Crashing
            // here must never authorize the old writer as a fallback.
            let mut retained: MachineReceipt =
                serde_json::from_slice(&serde_json::to_vec(&receipt).unwrap()).unwrap();
            assert_eq!(retained.selected, target);
            assert_eq!(retained.minimum_updater_version, "0.6.0");
            assert!(prepare_upgrade(&mut retained, &old, false).is_err());
            assert!(prepare_upgrade(&mut retained, &other, false).is_err());
            prepare_upgrade(&mut retained, &target, !running).unwrap();
            assert_eq!(retained.pending.as_ref().unwrap().was_running, running);
            assert_eq!(retained.pending.as_ref().unwrap().previous, old);
            assert_eq!(retained.selected, target);
        }
    }

    #[test]
    fn machine_recorder_configuration_rejects_arbitrary_commands_and_credentials() {
        let options = recorder_fixture();
        let value = serde_json::to_value(&options).unwrap();
        assert_eq!(
            serde_json::from_value::<MachineRecorderOptions>(value.clone()).unwrap(),
            options
        );
        for key in ["password", "executable", "arguments", "serviceDefinitionId"] {
            let mut changed = value.clone();
            changed[key] = "untrusted".into();
            assert!(serde_json::from_value::<MachineRecorderOptions>(changed).is_err());
        }
    }
}
