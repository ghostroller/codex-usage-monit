//! Forward-recoverable replacement of an existing, current-user recorder.
//! The candidate binary runs this operation, never the recorder being stopped.
use super::*;
use std::time::{Duration, Instant};

const JOURNAL: &str = "recorder-upgrade.json";
const JOURNAL_VERSION: u32 = 1;
const READY_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpgradeJournal {
    schema_version: u32,
    build_id: String,
    target: ServiceOptions,
    previous_fingerprint: String,
    enabled: bool,
    prepared_at: DateTime<Utc>,
    phase: String,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceUpgradeReport {
    pub outcome: String,
    pub build_id: String,
    pub enabled: bool,
    pub pid: Option<u32>,
    pub last_history_heartbeat: Option<DateTime<Utc>>,
    pub diagnostic: Option<String>,
}

struct Registration {
    options: ServiceOptions,
    fingerprint: String,
    enabled: bool,
}

pub(super) fn mutation_lock(root: &Path) -> Result<RecorderInstanceLockGuard> {
    match try_acquire_recorder_instance_lock(&root.join("upgrade-transaction/history-v1"))? {
        TryRecorderInstanceLock::Acquired(guard) => Ok(guard),
        TryRecorderInstanceLock::Busy => {
            bail!("service_upgrade_busy: another service mutation is active")
        }
    }
}

pub(super) fn clear_journal(root: &Path) -> Result<()> {
    match fs::remove_file(root.join(JOURNAL)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn save_journal(root: &Path, journal: &UpgradeJournal) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(journal)?;
    if bytes.len() > SERVICE_DEFINITION_MAX_BYTES as usize {
        bail!(
            "service_upgrade_state_invalid: retained configuration exceeds the recoverable journal limit"
        );
    }
    write_private_atomically(&root.join(JOURNAL), &bytes)?;
    Ok(())
}

fn read_journal(root: &Path) -> Result<Option<UpgradeJournal>> {
    let bytes = match read_private_regular_file_bounded(
        &root.join(JOURNAL),
        SERVICE_DEFINITION_MAX_BYTES,
        "recorder upgrade journal",
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let journal: UpgradeJournal = serde_json::from_slice(&bytes)?;
    if journal.schema_version != JOURNAL_VERSION
        || !matches!(
            journal.phase.as_str(),
            "prepared" | "replacing" | "awaiting_heartbeat" | "complete" | "failed"
        )
    {
        bail!("service_upgrade_state_invalid: unsupported upgrade journal");
    }
    Ok(Some(journal))
}

/// Existing registrations only: absence is a successful no-op. Retries use the
/// saved configuration even when a failed replacement removed the old task.
pub fn upgrade_registered_recorder() -> Result<ServiceUpgradeReport> {
    let root = service_coordination_root()?;
    let _guard = mutation_lock(&root)?;
    let build_id = env!("MONIT_BUILD_ID").to_string();
    let executable = env::current_exe()?;
    let saved = read_journal(&root)?;
    let existing =
        read_registration_during_upgrade(saved.as_ref().filter(|j| j.phase != "complete"))?;
    let mut journal = match saved.filter(|j| j.phase != "complete") {
        Some(mut journal) => {
            // Forward repair by a later candidate is allowed only if the
            // registration is still the saved old or attempted new definition.
            validate_resume_registration(&journal, existing.as_ref())?;
            if journal.build_id == build_id
                && existing
                    .as_ref()
                    .is_some_and(|r| r.enabled == journal.enabled)
                && existing.as_ref().map(|r| r.fingerprint.as_str())
                    == Some(target_fingerprint(&journal.target)?.as_str())
            {
                let status = if journal.enabled {
                    ready_status(&journal, true)?
                } else {
                    None
                };
                if !journal.enabled || status.is_some() {
                    journal.phase = "complete".into();
                    journal.last_error = None;
                    save_journal(&root, &journal)?;
                    return Ok(report(&journal, status));
                }
            }
            journal.previous_fingerprint = existing
                .as_ref()
                .map_or(journal.previous_fingerprint, |r| r.fingerprint.clone());
            journal.target.executable = executable;
            journal.build_id = build_id.clone();
            journal
        }
        None => {
            let Some(existing) = existing else {
                return Ok(ServiceUpgradeReport {
                    outcome: "not_installed".into(),
                    build_id,
                    enabled: false,
                    pid: None,
                    last_history_heartbeat: None,
                    diagnostic: None,
                });
            };
            let mut target = existing.options;
            target.executable = executable;
            let journal = UpgradeJournal {
                schema_version: JOURNAL_VERSION,
                build_id: build_id.clone(),
                target,
                previous_fingerprint: existing.fingerprint,
                enabled: existing.enabled,
                prepared_at: Utc::now(),
                phase: "prepared".into(),
                last_error: None,
            };
            if target_fingerprint(&journal.target)? == journal.previous_fingerprint {
                if !journal.enabled {
                    return Ok(report(&journal, None));
                }
                if let Some(status) = ready_status(&journal, false)? {
                    return Ok(report(&journal, Some(status)));
                }
            }
            journal
        }
    };
    validate_options(&journal.target)?;
    journal.phase = "prepared".into();
    journal.last_error = None;
    save_journal(&root, &journal)?;
    eprintln!("service.upgrade: prepared build {}", journal.build_id);
    execute_upgrade(&root, &mut journal, replace_registration, wait_ready)
}

fn execute_upgrade(
    root: &Path,
    journal: &mut UpgradeJournal,
    replace: impl FnOnce(&UpgradeJournal) -> Result<()>,
    ready: impl FnOnce(&UpgradeJournal) -> Result<RecorderStatusFile>,
) -> Result<ServiceUpgradeReport> {
    journal.last_error = None;
    let result: Result<ServiceUpgradeReport> = (|| {
        journal.phase = "replacing".into();
        save_journal(root, journal)?;
        replace(journal)?;
        journal.phase = "awaiting_heartbeat".into();
        save_journal(root, journal)?;
        let status = if journal.enabled {
            Some(ready(journal)?)
        } else {
            None
        };
        journal.phase = "complete".into();
        save_journal(root, journal)?;
        Ok(report(journal, status))
    })();
    if let Err(error) = &result {
        journal.phase = "failed".into();
        journal.last_error = Some(format!("{error:#}"));
        save_journal(root, journal).context("could not persist failed recorder upgrade")?;
    }
    result.context("service_upgrade_failed: retry deployment or run this candidate's `service upgrade`; prior data and upgrade configuration are retained")
}

fn report(journal: &UpgradeJournal, status: Option<RecorderStatusFile>) -> ServiceUpgradeReport {
    ServiceUpgradeReport {
        outcome: if journal.enabled { "ready" } else { "disabled" }.into(),
        build_id: journal.build_id.clone(),
        enabled: journal.enabled,
        pid: status.as_ref().map(|s| s.pid),
        last_history_heartbeat: status.as_ref().and_then(|s| s.last_history_heartbeat),
        diagnostic: status.and_then(|s| s.last_error),
    }
}

fn validate_resume_registration(
    journal: &UpgradeJournal,
    existing: Option<&Registration>,
) -> Result<()> {
    if let Some(existing) = existing
        && existing.fingerprint != journal.previous_fingerprint
        && existing.fingerprint != target_fingerprint(&journal.target)?
    {
        bail!("service_upgrade_conflict: registration changed since the interrupted upgrade");
    }
    Ok(())
}

fn target_fingerprint(options: &ServiceOptions) -> Result<String> {
    match current_platform() {
        Platform::MacOs => Ok(service_definition_fingerprint(
            launchd_plist(options).as_bytes(),
        )),
        Platform::Linux => Ok(service_definition_fingerprint(
            systemd_unit(options).as_bytes(),
        )),
        Platform::Windows => {
            let sid = windows_current_user_sid()?;
            canonical_windows_task_fingerprint(windows_task_xml(options, &sid).as_bytes(), &sid)
        }
        Platform::Unsupported => bail!("service upgrade is unsupported"),
    }
}

fn replace_registration(journal: &UpgradeJournal) -> Result<()> {
    let options = &journal.target;
    replace_service_checked(
        options,
        || {
            let current = read_registration_during_upgrade(Some(journal))?;
            validate_resume_registration(journal, current.as_ref())
        },
        || {
            disable_registration()?;
            request_graceful_stop(options)?;
            match current_platform() {
                Platform::MacOs => quiesce_launchd_for_install(options),
                Platform::Linux => quiesce_systemd_for_install(options),
                Platform::Windows => quiesce_windows_task_for_install(options),
                Platform::Unsupported => bail!("service upgrade is unsupported"),
            }
        },
        || match current_platform() {
            Platform::MacOs if !journal.enabled => {
                write_private_atomically(
                    &launchd_registration_path()?,
                    launchd_plist(options).as_bytes(),
                )?;
                Ok(())
            }
            Platform::MacOs => install_launchd(options),
            Platform::Linux => install_systemd(options),
            Platform::Windows => install_windows_task(options),
            Platform::Unsupported => bail!("service upgrade is unsupported"),
        },
        || match current_platform() {
            Platform::MacOs => cleanup_launchd_registration(),
            Platform::Linux => cleanup_systemd_registration(),
            Platform::Windows => cleanup_windows_task_registration(),
            Platform::Unsupported => bail!("service upgrade is unsupported"),
        },
        || persist_current_service_definition_marker(options),
        || {
            if !journal.enabled {
                return Ok(());
            }
            match current_platform() {
                Platform::MacOs => start_launchd(),
                Platform::Linux => start_systemd(),
                Platform::Windows => start_windows_task(),
                Platform::Unsupported => bail!("service upgrade is unsupported"),
            }
        },
    )
}

fn disable_registration() -> Result<()> {
    if read_registration_definition()?.is_none() {
        return Ok(());
    }
    match current_platform() {
        Platform::MacOs => run_checked(
            Command::new("launchctl")
                .args(["disable", &format!("{}/{SERVICE_LABEL}", launchd_domain())]),
            "disable recorder before upgrade",
        ),
        Platform::Linux => run_checked(
            Command::new("systemctl").args(["--user", "disable", SYSTEMD_UNIT]),
            "disable recorder before upgrade",
        ),
        Platform::Windows => {
            let task = windows_task_name(&windows_current_user_sid()?);
            if !windows_task_is_installed_with(&task, &mut run_windows_task_operation)? {
                return Ok(());
            }
            run_checked(
                Command::new("schtasks.exe").args(["/Change", "/TN", &task, "/DISABLE"]),
                "disable recorder before upgrade",
            )
        }
        Platform::Unsupported => bail!("service upgrade is unsupported"),
    }
}

fn ready_status(journal: &UpgradeJournal, require_new: bool) -> Result<Option<RecorderStatusFile>> {
    let Some(status) = read_recorder_status(&journal.target.status_file)? else {
        return Ok(None);
    };
    if !heartbeat_matches(&status, journal, require_new) {
        return Ok(None);
    }
    let observed = super::status(&journal.target)?;
    if !observed.running || !observed.heartbeat_recent {
        return Ok(None);
    }
    Ok(Some(status))
}

fn heartbeat_matches(
    status: &RecorderStatusFile,
    journal: &UpgradeJournal,
    require_new: bool,
) -> bool {
    status.build_id.as_deref() == Some(journal.build_id.as_str())
        && status.service_definition_id.as_deref()
            == Some(journal.target.service_definition_id().as_str())
        && status.source_aware_v2_epoch().is_some()
        && (!require_new || status.started_at >= journal.prepared_at)
        && status
            .last_history_heartbeat
            .is_some_and(|at| at >= status.started_at)
        && status.writer_may_be_active(Utc::now())
}

fn wait_ready(journal: &UpgradeJournal) -> Result<RecorderStatusFile> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = ready_status(journal, true)? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!(
                "service_start_timeout: new recorder did not publish a verified history heartbeat; inspect service status and retry service upgrade"
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StopRequest {
    pid: u32,
    started_at: DateTime<Utc>,
    build_id: String,
}

pub(crate) fn recorder_stop_requested(path: &Path, status: &RecorderStatusFile) -> Result<bool> {
    let bytes = match read_private_regular_file_bounded(
        &path.with_extension("stop.json"),
        4096,
        "recorder stop request",
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let request: StopRequest = serde_json::from_slice(&bytes)?;
    Ok(request.pid == status.pid
        && request.started_at == status.started_at
        && status.build_id.as_deref() == Some(request.build_id.as_str()))
}

fn request_graceful_stop(options: &ServiceOptions) -> Result<()> {
    let Some(status) = read_recorder_status(&options.status_file)? else {
        return Ok(());
    };
    let Some(build_id) = status.build_id else {
        return Ok(());
    }; // Older recorders need manager stop.
    let request = StopRequest {
        pid: status.pid,
        started_at: status.started_at,
        build_id,
    };
    write_private_atomically(
        &options.status_file.with_extension("stop.json"),
        &serde_json::to_vec(&request)?,
    )?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let TryRecorderInstanceLock::Acquired(_) =
            try_acquire_recorder_instance_lock(&options.history_dir)?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Ok(());
        } // Manager stop plus WAL recovery is the bounded fallback.
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn launchd_disabled() -> Result<bool> {
    let result = run_launchd_operation(LaunchdOperation::PrintDisabled {
        domain: launchd_domain(),
    })?;
    if !result.success {
        bail!("could not inspect launchd enabled state: {}", result.detail);
    }
    let values = result
        .detail
        .lines()
        .filter_map(|line| line.trim().split_once("=>"))
        .filter_map(|(label, value)| {
            (label.trim().trim_matches('"') == SERVICE_LABEL).then_some(value.trim())
        })
        .collect::<Vec<_>>();
    match values.as_slice() {
        [] | ["enabled"] => Ok(false),
        ["disabled"] => Ok(true),
        _ => bail!("ambiguous launchd enabled state"),
    }
}

pub(super) fn verify_disabled_launchd_is_unloaded() -> Result<()> {
    if !launchd_disabled()? {
        bail!("recorder is not disabled");
    }
    let result = run_launchd_operation(LaunchdOperation::PrintService {
        target: format!("{}/{SERVICE_LABEL}", launchd_domain()),
    })?;
    if result.success || !launchd_print_reports_missing(&result.detail) {
        bail!("disabled recorder is not proven unloaded");
    }
    Ok(())
}

fn read_registration() -> Result<Option<Registration>> {
    let fingerprint = match current_user_service_definition_observation()? {
        ServiceDefinitionObservation::Absent => return Ok(None),
        ServiceDefinitionObservation::Fingerprint(value) => value,
        ServiceDefinitionObservation::Unverifiable(detail) => {
            bail!("service_upgrade_unverifiable: {detail}")
        }
    };
    let registration =
        read_registration_definition()?.context("registered service definition disappeared")?;
    if registration.fingerprint != fingerprint {
        bail!("service_upgrade_conflict: service definition changed during discovery");
    }
    Ok(Some(registration))
}

fn recovery_intent_is_proven(root: &Path, journal: &UpgradeJournal) -> Result<bool> {
    let bytes = match read_private_regular_file_bounded(
        &root.join(RECORDER_CUTOVER_BLOCKER_FILE),
        SERVICE_TRUST_MARKER_MAX_BYTES,
        "interrupted service cutover blocker",
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let blocker: RecorderCutoverBlocker = serde_json::from_slice(&bytes)?;
    Ok(
        blocker.schema_version == RECORDER_CUTOVER_BLOCKER_SCHEMA_VERSION
            && blocker.blocked_at >= journal.prepared_at
            && blocker.platform == format!("{:?}", current_platform()).to_ascii_lowercase()
            && journal.phase != "complete",
    )
}

fn read_registration_during_upgrade(
    journal: Option<&UpgradeJournal>,
) -> Result<Option<Registration>> {
    match read_registration() {
        Ok(value) => Ok(value),
        Err(original) => recover_interrupted_registration(
            &service_coordination_root()?,
            journal,
            original,
            read_registration_definition,
            prove_interrupted_registration_inactive,
        ),
    }
}

fn recover_interrupted_registration(
    root: &Path,
    journal: Option<&UpgradeJournal>,
    original: anyhow::Error,
    read_definition: impl FnOnce() -> Result<Option<Registration>>,
    prove_inactive: impl FnOnce() -> Result<()>,
) -> Result<Option<Registration>> {
    let Some(journal) = journal else {
        return Err(original);
    };
    if !recovery_intent_is_proven(root, journal)? {
        return Err(original);
    }
    let Some(registration) = read_definition()? else {
        return Err(original);
    };
    validate_resume_registration(journal, Some(&registration))?;
    // Disk publication and manager loading are separate crash points. An exact
    // journal-bound definition can be recovered only without an active writer.
    prove_inactive()?;
    Ok(Some(registration))
}

fn prove_interrupted_registration_inactive() -> Result<()> {
    match current_platform() {
        Platform::MacOs => {
            let result = run_launchd_operation(LaunchdOperation::PrintService {
                target: format!("{}/{SERVICE_LABEL}", launchd_domain()),
            })?;
            if result.success || !launchd_print_reports_missing(&result.detail) {
                bail!(
                    "service_upgrade_unverifiable: interrupted registration is not proven unloaded"
                );
            }
        }
        Platform::Linux => {
            let definition =
                run_systemd_quiesce_operation(SystemdQuiesceOperation::InspectDefinition)?;
            verify_systemd_definition_disabled(&definition)?;
            let active = run_systemd_quiesce_operation(SystemdQuiesceOperation::VerifyInactive)?;
            verify_systemd_inactive(&active)?;
        }
        Platform::Windows | Platform::Unsupported => bail!(
            "service_upgrade_unverifiable: registration cannot be recovered without manager verification"
        ),
    }
    Ok(())
}

fn read_registration_definition() -> Result<Option<Registration>> {
    let read = |path: &Path| -> Result<Option<Vec<u8>>> {
        match read_private_regular_file_bounded(
            path,
            SERVICE_DEFINITION_MAX_BYTES,
            "recorder definition",
        ) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    };
    let (arguments, environment, enabled, fingerprint) = match current_platform() {
        Platform::MacOs => {
            let Some(bytes) = read(&launchd_registration_path()?)? else {
                return Ok(None);
            };
            (
                launchd_definition_arguments(&bytes)?,
                launchd_definition_environment_path(&bytes)?,
                !launchd_disabled()?,
                service_definition_fingerprint(&bytes),
            )
        }
        Platform::Linux => {
            let Some(bytes) = read(&systemd_registration_path()?)? else {
                return Ok(None);
            };
            let output = run_service_command(Command::new("systemctl").args([
                "--user",
                "is-enabled",
                SYSTEMD_UNIT,
            ]))?;
            let enabled = match String::from_utf8_lossy(&output.stdout).trim() {
                "enabled" => true,
                "disabled" => false,
                _ => bail!("service_upgrade_unverifiable: unsupported systemd enablement state"),
            };
            (
                systemd_definition_arguments(&bytes)?,
                systemd_definition_environment_path(&bytes)?,
                enabled,
                service_definition_fingerprint(&bytes),
            )
        }
        Platform::Windows => {
            let sid = windows_current_user_sid()?;
            let task_name = windows_task_name(&sid);
            if !windows_task_is_installed_with(&task_name, &mut run_windows_task_operation)? {
                return Ok(None);
            }
            let (status, bytes) = windows_task_xml_bounded(&task_name)?;
            if !status.success() {
                bail!("service_upgrade_unverifiable: task export failed");
            }
            let document = decode_windows_task_xml(&bytes)?;
            let task = parse_windows_task_xml(&document)?;
            verify_windows_task_structure(&task, &sid, false)?;
            let mut args = windows_arguments(task.text("Task/Actions/Exec/Arguments")?)?;
            let environment = if args
                .first()
                .is_some_and(|a| a.starts_with("--service-path="))
            {
                Some(
                    args.remove(0)
                        .trim_start_matches("--service-path=")
                        .to_string(),
                )
            } else {
                None
            };
            args.insert(0, task.text("Task/Actions/Exec/Command")?.to_string());
            (
                args,
                environment,
                task.text_or_default("Task/Settings/Enabled", "true")? == "true",
                canonical_windows_task_fingerprint(&bytes, &sid)?,
            )
        }
        Platform::Unsupported => bail!("service upgrade is unsupported"),
    };
    let options = parse_registered_options(&arguments, environment)?;
    if target_fingerprint(&options)? != fingerprint {
        bail!(
            "service_upgrade_unverifiable: registration is not an exact application-managed definition"
        );
    }
    Ok(Some(Registration {
        options,
        fingerprint,
        enabled,
    }))
}

pub fn registered_status(fallback: &ServiceOptions) -> Result<ServiceStatus> {
    match read_registration()? {
        Some(registration) => super::status(&registration.options),
        None => super::status(fallback),
    }
}

fn parse_registered_options(
    args: &[String],
    environment: Option<String>,
) -> Result<ServiceOptions> {
    let executable = args
        .first()
        .context("recorder definition has no executable")?;
    let mut values = std::collections::BTreeMap::<&str, &str>::new();
    let mut flags = std::collections::BTreeSet::new();
    let mut words = args[1..].iter();
    while let Some(key) = words.next() {
        if matches!(
            key.as_str(),
            "record" | "--foreground" | "--offline" | "--redact-content" | "--no-rollout-cache"
        ) {
            if !flags.insert(key.as_str()) {
                bail!("duplicate recorder flag");
            }
        } else {
            let value = words.next().context("recorder argument lacks its value")?;
            if values.insert(key, value).is_some() {
                bail!("duplicate recorder option");
            }
        }
    }
    let required = |name: &str| {
        values
            .get(name)
            .copied()
            .with_context(|| format!("registered recorder lacks {name}"))
    };
    let mut options = ServiceOptions::new(
        PathBuf::from(executable),
        PathBuf::from(required("--codex-home")?),
        PathBuf::from(required("--history-dir")?),
        PathBuf::from(required("--status-file")?),
        values.get("--perf-log").map(PathBuf::from),
    );
    options.codex_bin = values.get("--codex-bin").map(PathBuf::from);
    options.trace_log = values.get("--trace-log").map(PathBuf::from);
    options.remotes_config_file = values.get("--service-remotes-config").map(PathBuf::from);
    options.project_mapping_file = values
        .get("--service-project-mapping-file")
        .map(PathBuf::from);
    options.lookback_days = required("--days")?.parse()?;
    options.max_files = required("--max-files")?.parse()?;
    options.active_grace_minutes = required("--active-grace-minutes")?.parse()?;
    options.offline = flags.contains("--offline");
    options.redact_content = flags.contains("--redact-content");
    options.no_rollout_cache = flags.contains("--no-rollout-cache");
    options.environment_path = environment.map(OsString::from);
    let expected = options
        .recorder_arguments()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    // An exact round trip rejects unknown options, extra actions and altered
    // identities rather than silently discarding installation-time semantics.
    if args[1..] != expected {
        bail!("service_upgrade_unverifiable: unsupported or altered recorder arguments");
    }
    Ok(options)
}

#[cfg(windows)]
fn windows_arguments(text: &str) -> Result<Vec<String>> {
    use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};
    let line = format!("recorder.exe {text}")
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut count = 0;
    // SAFETY: the input is terminated; the API returns count valid strings in
    // one allocation, released after copying all arguments.
    unsafe {
        let argv = CommandLineToArgvW(line.as_ptr(), &mut count);
        if argv.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let result = (1..count)
            .map(|index| {
                let value = *argv.add(index as usize);
                let mut length = 0;
                while *value.add(length) != 0 {
                    length += 1;
                }
                String::from_utf16(std::slice::from_raw_parts(value, length))
                    .context("non-UTF-16 task argument")
            })
            .collect();
        LocalFree(argv.cast());
        result
    }
}

#[cfg(not(windows))]
fn windows_arguments(_text: &str) -> Result<Vec<String>> {
    bail!("Windows task arguments require Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, UpgradeJournal) {
        let temp = tempfile::tempdir().unwrap();
        // tempfile inherits the process umask; an existing state root must
        // explicitly satisfy the same private-directory contract as production.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        recorder_coordination::prepare_recorder_lock_state_root(temp.path()).unwrap();
        let mut options = ServiceOptions::new(
            temp.path().join("版本 old/app.exe"),
            temp.path().join("codex home"),
            temp.path().join("history-v1"),
            temp.path().join("custom status.json"),
            Some(temp.path().join("perf.jsonl")),
        );
        options.offline = true;
        options.redact_content = true;
        options.no_rollout_cache = true;
        options.lookback_days = 21;
        options.max_files = 700;
        options.trace_log = Some(temp.path().join("trace.jsonl"));
        options.remotes_config_file = Some(temp.path().join("custom/remotes.json"));
        options.project_mapping_file = Some(temp.path().join("custom/projects.json"));
        options.environment_path = Some(OsString::from("path with space;quote\";trailing\\"));
        let journal = UpgradeJournal {
            schema_version: JOURNAL_VERSION,
            build_id: env!("MONIT_BUILD_ID").into(),
            previous_fingerprint: "0".repeat(64),
            target: options,
            enabled: true,
            prepared_at: Utc::now(),
            phase: "prepared".into(),
            last_error: None,
        };
        (temp, journal)
    }

    fn heartbeat(journal: &UpgradeJournal) -> RecorderStatusFile {
        let mut status = RecorderStatusFile::started(Utc::now(), "history".into());
        status.bind_source_aware_v2(2).unwrap();
        status.service_definition_id = Some(journal.target.service_definition_id());
        status.record_success(Utc::now());
        status
    }

    #[test]
    fn registration_import_preserves_every_option_and_rejects_unknown_or_changed_arguments() {
        let (_temp, journal) = fixture();
        let options = journal.target;
        let mut args = vec![options.executable.to_string_lossy().into_owned()];
        args.extend(
            options
                .recorder_arguments()
                .iter()
                .map(|v| v.to_string_lossy().into_owned()),
        );
        let environment = options
            .environment_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        assert_eq!(
            parse_registered_options(&args, environment.clone()).unwrap(),
            options
        );
        let last = args.len() - 1;
        args[last].push_str("-changed");
        assert!(parse_registered_options(&args, environment.clone()).is_err());
        args[last] = options.status_file.to_string_lossy().into_owned();
        args.extend(["--unknown-option".into(), "value".into()]);
        assert!(parse_registered_options(&args, environment).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_argument_import_round_trips_quotes_unicode_and_path() {
        let (_temp, journal) = fixture();
        let mut arguments =
            windows_arguments(&windows_recorder_arguments(&journal.target)).unwrap();
        let path = arguments
            .remove(0)
            .strip_prefix("--service-path=")
            .unwrap()
            .to_string();
        arguments.insert(0, journal.target.executable.to_string_lossy().into_owned());
        assert_eq!(
            parse_registered_options(&arguments, Some(path)).unwrap(),
            journal.target
        );
    }

    #[test]
    fn successful_manager_start_is_not_a_success_without_a_new_matching_heartbeat() {
        let (_temp, journal) = fixture();
        let good = heartbeat(&journal);
        assert!(heartbeat_matches(&good, &journal, true));
        let mut bad = good.clone();
        bad.build_id = Some("0".repeat(64));
        assert!(!heartbeat_matches(&bad, &journal, true));
        bad = good.clone();
        bad.service_definition_id = None;
        assert!(!heartbeat_matches(&bad, &journal, true));
        bad = good.clone();
        bad.last_history_heartbeat = None;
        assert!(!heartbeat_matches(&bad, &journal, true));
        bad = good.clone();
        bad.started_at = journal.prepared_at - chrono::Duration::hours(1);
        assert!(!heartbeat_matches(&bad, &journal, true));
        bad = good;
        bad.record_degraded(Utc::now(), "remote host offline");
        assert!(
            heartbeat_matches(&bad, &journal, true),
            "SSH failure must not roll back healthy local collection"
        );
    }

    #[test]
    fn replacement_failure_keeps_configuration_for_forward_recovery_after_task_removal() {
        let (temp, mut journal) = fixture();
        let original = journal.target.clone();
        let error = execute_upgrade(
            temp.path(),
            &mut journal,
            |_| bail!("injected registration failure"),
            |_| panic!("must not wait after replacement failure"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected registration failure"));
        let mut recovered = read_journal(temp.path()).unwrap().unwrap();
        assert_eq!(recovered.phase, "failed");
        assert_eq!(recovered.target, original);
        validate_resume_registration(&recovered, None).unwrap();
        let result = execute_upgrade(
            temp.path(),
            &mut recovered,
            |_| Ok(()),
            |j| Ok(heartbeat(j)),
        )
        .unwrap();
        assert_eq!(result.outcome, "ready");
        assert_eq!(
            read_journal(temp.path()).unwrap().unwrap().phase,
            "complete"
        );
        assert!(
            read_journal(temp.path())
                .unwrap()
                .unwrap()
                .last_error
                .is_none()
        );
    }

    #[test]
    fn disabled_registration_is_replaced_without_start_or_heartbeat_wait() {
        let (temp, mut journal) = fixture();
        journal.enabled = false;
        let result = execute_upgrade(
            temp.path(),
            &mut journal,
            |j| {
                assert!(!j.enabled);
                Ok(())
            },
            |_| panic!("disabled task must not be started"),
        )
        .unwrap();
        assert_eq!(result.outcome, "disabled");
        assert!(!result.enabled);
        assert!(result.pid.is_none());
    }

    #[test]
    fn failed_readiness_is_saved_and_never_reported_as_a_completed_upgrade() {
        let (temp, mut journal) = fixture();
        assert!(
            execute_upgrade(
                temp.path(),
                &mut journal,
                |_| Ok(()),
                |_| bail!("service_start_timeout: injected missing heartbeat")
            )
            .is_err()
        );
        let saved = read_journal(temp.path()).unwrap().unwrap();
        assert_eq!(saved.phase, "failed");
        assert!(saved.last_error.unwrap().contains("service_start_timeout"));
        assert_eq!(saved.target, journal.target);
    }

    #[test]
    fn stale_upgrade_preflight_does_not_disable_or_cleanup_the_changed_registration() {
        let (temp, mut journal) = fixture();
        let root = temp.path().join("coordination");
        journal.target.service_coordination_root_override = Some(root.clone());
        let result = replace_service_checked(
            &journal.target,
            || bail!("service_upgrade_conflict"),
            || panic!("must not stop a changed service"),
            || panic!("must not register"),
            || panic!("must not clean up"),
            || panic!("must not publish trust"),
            || panic!("must not start"),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("service_upgrade_conflict")
        );
        assert!(!root.join(RECORDER_CUTOVER_BLOCKER_FILE).exists());
    }

    #[test]
    fn a_changed_service_definition_cannot_be_overwritten_by_a_saved_upgrade() {
        let (_temp, journal) = fixture();
        let changed = Registration {
            options: journal.target.clone(),
            fingerprint: "1".repeat(64),
            enabled: false,
        };
        assert!(validate_resume_registration(&journal, Some(&changed)).is_err());
    }

    #[test]
    fn stop_requests_are_bound_to_the_process_instance_not_only_pid() {
        let (temp, journal) = fixture();
        let status = heartbeat(&journal);
        let path = temp.path().join("status.json");
        let mut request = StopRequest {
            pid: status.pid,
            started_at: status.started_at,
            build_id: status.build_id.clone().unwrap(),
        };
        write_private_atomically(
            &path.with_extension("stop.json"),
            &serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        assert!(recorder_stop_requested(&path, &status).unwrap());
        request.started_at -= chrono::Duration::seconds(1);
        write_private_atomically(
            &path.with_extension("stop.json"),
            &serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        assert!(!recorder_stop_requested(&path, &status).unwrap());
    }
    #[test]
    fn interrupted_registration_recovery_requires_its_durable_cutover_intent() {
        let (temp, mut journal) = fixture();
        assert!(!recovery_intent_is_proven(temp.path(), &journal).unwrap());
        let mut blocker = RecorderCutoverBlocker {
            schema_version: RECORDER_CUTOVER_BLOCKER_SCHEMA_VERSION,
            blocked_at: journal.prepared_at - chrono::Duration::seconds(1),
            platform: format!("{:?}", current_platform()).to_ascii_lowercase(),
            reason: "service replacement is in progress".into(),
        };
        let path = temp.path().join(RECORDER_CUTOVER_BLOCKER_FILE);
        write_private_atomically(&path, &serde_json::to_vec(&blocker).unwrap()).unwrap();
        assert!(!recovery_intent_is_proven(temp.path(), &journal).unwrap());
        blocker.blocked_at = journal.prepared_at + chrono::Duration::seconds(1);
        write_private_atomically(&path, &serde_json::to_vec(&blocker).unwrap()).unwrap();
        assert!(recovery_intent_is_proven(temp.path(), &journal).unwrap());
        journal.phase = "complete".into();
        assert!(!recovery_intent_is_proven(temp.path(), &journal).unwrap());
    }

    #[test]
    fn crash_after_definition_write_recovers_only_the_saved_inactive_registration() {
        let (temp, journal) = fixture();
        let original = || anyhow::anyhow!("loaded manager definition differs");
        let definition = || {
            Ok(Some(Registration {
                options: journal.target.clone(),
                fingerprint: target_fingerprint(&journal.target).unwrap(),
                enabled: journal.enabled,
            }))
        };
        assert!(
            recover_interrupted_registration(
                temp.path(),
                Some(&journal),
                original(),
                || panic!("no durable intent: must not bypass strict inspection"),
                || panic!("must not inspect fallback")
            )
            .is_err()
        );
        let blocker = RecorderCutoverBlocker {
            schema_version: RECORDER_CUTOVER_BLOCKER_SCHEMA_VERSION,
            blocked_at: journal.prepared_at,
            platform: format!("{:?}", current_platform()).to_ascii_lowercase(),
            reason: "replacement interrupted".into(),
        };
        write_private_atomically(
            &temp.path().join(RECORDER_CUTOVER_BLOCKER_FILE),
            &serde_json::to_vec(&blocker).unwrap(),
        )
        .unwrap();
        assert!(
            recover_interrupted_registration(
                temp.path(),
                Some(&journal),
                original(),
                definition,
                || bail!("unknown job still loaded")
            )
            .is_err()
        );
        assert!(
            recover_interrupted_registration(
                temp.path(),
                Some(&journal),
                original(),
                || Ok(Some(Registration {
                    options: journal.target.clone(),
                    fingerprint: "1".repeat(64),
                    enabled: true
                })),
                || panic!("independently changed definition must be rejected")
            )
            .is_err()
        );
        let recovered = recover_interrupted_registration(
            temp.path(),
            Some(&journal),
            original(),
            definition,
            || Ok(()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(recovered.options, journal.target);
        assert_eq!(recovered.enabled, journal.enabled);
    }

    #[test]
    fn an_oversized_configuration_is_rejected_before_an_unreadable_journal_is_written() {
        let (temp, mut journal) = fixture();
        journal.target.environment_path = Some(OsString::from(
            "x".repeat(SERVICE_DEFINITION_MAX_BYTES as usize),
        ));
        assert!(save_journal(temp.path(), &journal).is_err());
        assert!(read_journal(temp.path()).unwrap().is_none());
    }
}
