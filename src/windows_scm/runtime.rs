//! Native SCM lifecycle; all recorder data is opened as the service account.
use super::*;
use std::{
    ffi::OsString,
    io::{Seek, SeekFrom, Write},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};
use windows_service::{
    define_windows_service,
    service::{ServiceControl, ServiceControlAccept},
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};

static CONFIG: OnceLock<MachineReceipt> = OnceLock::new();
static STOP: AtomicBool = AtomicBool::new(false);
static EXIT: AtomicU32 = AtomicU32::new(0);

pub(super) fn dispatch(receipt: MachineReceipt) -> Result<i32> {
    ensure!(
        !matches!(receipt.phase.as_str(), "uninstalling" | "uninstalled"),
        "machine_uninstall_pending: recorder startup is disabled"
    );
    let name = receipt.name.clone();
    CONFIG.set(receipt).map_err(|_| {
        anyhow::anyhow!("machine_dispatch_duplicate: SCM dispatcher already initialized")
    })?;
    service_dispatcher::start(name, ffi_service_main)
        .context("machine_dispatch_failed: this internal entry must be launched by SCM")?;
    Ok(EXIT.load(Ordering::SeqCst) as i32)
}

fn control(control: ServiceControl) -> ServiceControlHandlerResult {
    match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            STOP.store(true, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    }
}

fn publish_state(
    handle: &ServiceStatusHandle,
    state: ServiceState,
    checkpoint: u32,
    error: u32,
) -> Result<()> {
    handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: if error == 0 {
                ServiceExitCode::NO_ERROR
            } else {
                ServiceExitCode::ServiceSpecific(error)
            },
            checkpoint,
            wait_hint: if matches!(
                state,
                ServiceState::StartPending | ServiceState::StopPending
            ) {
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            },
            process_id: None,
        })
        .context("machine_status_publish_failed")
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    let Some(receipt) = CONFIG.get() else {
        return;
    };
    let Ok(handle) = service_control_handler::register(&receipt.name, control) else {
        EXIT.store(1, Ordering::SeqCst);
        return;
    };
    let result = std::panic::catch_unwind(|| supervise(receipt, &handle));
    let code = match result {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            log_failure(receipt, &format!("{error:#}"));
            1
        }
        Err(_) => {
            log_failure(receipt, "SCM recorder supervisor panicked");
            1
        }
    };
    EXIT.store(code, Ordering::SeqCst);
    let _ = publish_state(&handle, ServiceState::Stopped, 0, code);
}

fn log_failure(receipt: &MachineReceipt, message: &str) {
    if let Ok(mut file) = crate::windows_private_directory::open_private_file(
        &receipt.recorder.status_file.with_extension("scm.log"),
    ) {
        let _ = file.seek(SeekFrom::End(0));
        let _ = writeln!(file, "{} {message}", Utc::now());
    }
}

fn recorder_arguments(options: &MachineRecorderOptions) -> Vec<String> {
    let mut arguments = vec![
        "--codex-home".into(),
        options.codex_home.to_string_lossy().into_owned(),
        "--days".into(),
        options.lookback_days.to_string(),
        "--max-files".into(),
        options.max_files.to_string(),
        "--active-grace-minutes".into(),
        options.active_grace_minutes.to_string(),
    ];
    for (flag, path) in [
        ("--codex-bin", &options.codex_bin),
        ("--service-remotes-config", &options.remotes_config_file),
    ] {
        if let Some(path) = path {
            arguments.extend([flag.into(), path.to_string_lossy().into_owned()]);
        }
    }
    for (enabled, flag) in [
        (options.offline, "--offline"),
        (options.redact_content, "--redact-content"),
        (options.no_rollout_cache, "--no-rollout-cache"),
    ] {
        if enabled {
            arguments.push(flag.into());
        }
    }
    arguments.extend([
        "record".into(),
        "--foreground".into(),
        "--history-dir".into(),
        options.history_dir.to_string_lossy().into_owned(),
        "--status-file".into(),
        options.status_file.to_string_lossy().into_owned(),
    ]);
    if let Some(path) = &options.project_mapping_file {
        arguments.extend([
            "--service-project-mapping-file".into(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    arguments
}

pub(super) fn inspect_heartbeat(
    receipt: &MachineReceipt,
) -> Result<Option<crate::service::RecorderStatusFile>> {
    let path = &receipt.recorder.status_file;
    if !path.exists() {
        return Ok(None);
    }
    security::validate_runtime_path(path, &receipt.account_sid)?;
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "machine_status_invalid: status is not a regular file"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA,
        "machine_status_invalid: oversized status"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn supervise(receipt: &MachineReceipt, handle: &ServiceStatusHandle) -> Result<u32> {
    publish_state(handle, ServiceState::StartPending, 1, 0)?;
    ensure!(
        security::current_sid()? == receipt.account_sid,
        "machine_account_mismatch: runtime token differs"
    );
    validate_recorder(&receipt.recorder, &receipt.account_sid)?;
    validate_version(receipt, &receipt.selected)?;
    let environment = security::runtime_environment()?;
    let profile = &environment[0].1;
    let log_path = receipt.recorder.status_file.with_extension("scm.log");
    crate::windows_private_directory::create_dir_all(
        log_path.parent().context("status has no parent")?,
    )?;
    let mut log = crate::windows_private_directory::open_private_file(&log_path)?;
    log.seek(SeekFrom::End(0))?;
    let mut command = Command::new(&receipt.selected.executable);
    command
        .args(recorder_arguments(&receipt.recorder))
        .current_dir(profile)
        .env_remove("CODEX_USAGE_MONIT_STATE_DIR")
        .env_remove("CODEX_USAGE_MONIT_CACHE_DIR")
        .env_remove("CODEX_USAGE_MONIT_CONFIG_DIR");
    for (key, value) in environment {
        command.env(key, value);
    }
    if let Some(path) = &receipt.recorder.config_dir {
        command.env("CODEX_USAGE_MONIT_CONFIG_DIR", path);
    }
    if let Some(path) = &receipt.recorder.environment_path {
        command.env("PATH", path);
    }
    let prepared = Utc::now();
    let child = process::spawn(&command, &log).context("machine_recorder_start_failed")?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut ready = false;
    let mut stop_started = None;
    let mut checkpoint = 1_u32;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(if STOP.load(Ordering::SeqCst) {
                0
            } else {
                status.max(1)
            });
        }
        if STOP.load(Ordering::SeqCst) && stop_started.is_none() {
            stop_started = Some(Instant::now());
            // This writer runs under the service token, so the private stop
            // request is never owned by the installing administrator.
            let _ = crate::service::request_foreground_recorder_stop(
                &receipt.recorder.status_file,
                child.id(),
                &receipt.selected.build_id,
            );
        }
        if let Some(started) = stop_started {
            checkpoint = checkpoint.saturating_add(1);
            publish_state(handle, ServiceState::StopPending, checkpoint, 0)?;
            if started.elapsed() >= Duration::from_secs(25) {
                child.terminate()?;
                return Ok(0);
            }
        } else if !ready {
            let status = crate::service::read_recorder_status(&receipt.recorder.status_file)?;
            if status.as_ref().is_some_and(|status| {
                heartbeat_matches(status, child.id(), &receipt.selected.build_id, prepared)
            }) {
                ready = true;
                publish_state(handle, ServiceState::Running, 0, 0)?;
            } else {
                ensure!(
                    Instant::now() < deadline,
                    "machine_recorder_not_ready: no fresh matching persisted history heartbeat; see {}",
                    log_path.display()
                );
                checkpoint = checkpoint.saturating_add(1);
                publish_state(handle, ServiceState::StartPending, checkpoint, 0)?;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn heartbeat_matches(
    status: &crate::service::RecorderStatusFile,
    pid: u32,
    build: &str,
    prepared: DateTime<Utc>,
) -> bool {
    status.pid == pid
        && status.build_id.as_deref() == Some(build)
        && status.started_at >= prepared
        && status
            .last_history_heartbeat
            .is_some_and(|heartbeat| heartbeat >= prepared)
        && status.history_backend == Some(crate::service::RecorderHistoryBackend::SourceAwareV2)
        && status.ownership_epoch.is_some_and(|epoch| epoch > 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scm_readiness_rejects_stale_wrong_pid_and_unpersisted_status() {
        let now = Utc::now();
        let mut status = crate::service::RecorderStatusFile::started(now, "history".into());
        status.pid = 42;
        status.bind_source_aware_v2(2).unwrap();
        let build = status.build_id.clone().unwrap();
        assert!(!heartbeat_matches(&status, 42, &build, now));
        status.record_success(now);
        assert!(heartbeat_matches(&status, 42, &build, now));
        assert!(!heartbeat_matches(&status, 43, &build, now));
        assert!(!heartbeat_matches(&status, 42, "another-build", now));
        assert!(!heartbeat_matches(
            &status,
            42,
            &build,
            now + chrono::Duration::seconds(1)
        ));
    }
}
