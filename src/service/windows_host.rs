//! Version-bound Windows launch adapters. The main executable embeds the host,
//! so the existing signed/checksummed release remains one executable.
use super::*;
use anyhow::ensure;

const HOST_NAME: &str = "recorder-host.exe";
const COMPONENTS_NAME: &str = "windows-components.json";
const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WindowsRecorderHost {
    pub executable: PathBuf,
    pub sha256: String,
    pub recorder_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostArtifact {
    file: String,
    sha256: String,
    size: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Components {
    schema_version: u32,
    product: String,
    version: String,
    build_id: String,
    target: String,
    binary_sha256: String,
    recorder_host: HostArtifact,
}

#[cfg(windows)]
pub(crate) fn expected_host_bytes() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/recorder-host.exe"))
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn image_bytes(path: &Path) -> Result<Vec<u8>> {
    read_private_regular_file_bounded(path, MAX_IMAGE_BYTES, "Windows managed component")
        .with_context(|| format!("could not validate managed component {}", path.display()))
}

fn read_components(directory: &Path) -> Result<Components> {
    let bytes = read_private_regular_file_bounded(
        &directory.join(COMPONENTS_NAME),
        16 * 1024,
        "Windows component identity",
    )?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// False means a legacy single-binary version. Partial or untrusted components
/// are errors, so pruning never treats them as disposable known files.
pub(crate) fn validate_windows_version_components(
    directory: &Path,
    expected_build_id: &str,
) -> Result<bool> {
    let host = directory.join(HOST_NAME);
    let metadata = directory.join(COMPONENTS_NAME);
    let exists = |path: &Path| -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    };
    match (exists(&host)?, exists(&metadata)?) {
        (false, false) => return Ok(false),
        (true, true) => {}
        _ => bail!(
            "service_host_incomplete: component extraction is incomplete; repair before pruning"
        ),
    }
    let components = read_components(directory)?;
    ensure!(
        components.schema_version == 1
            && components.product == "codex-usage-monit"
            && components.build_id == expected_build_id
            && valid_hash(&components.build_id)
            && components.target.ends_with("-pc-windows-msvc")
            && !components.version.is_empty()
            && components.recorder_host.file == HOST_NAME
            && valid_hash(&components.recorder_host.sha256)
            && valid_hash(&components.binary_sha256),
        "service_host_invalid: unsupported component identity"
    );
    let bytes = image_bytes(&host)?;
    ensure!(
        components.recorder_host.size == bytes.len() as u64
            && hash(&bytes) == components.recorder_host.sha256,
        "service_host_changed: recorder host does not match its component identity"
    );
    ensure!(
        hash(&image_bytes(&directory.join("codex-usage-monit.exe"))?) == components.binary_sha256,
        "service_host_changed: recorder executable differs from its component identity"
    );
    Ok(true)
}

pub(crate) fn remove_windows_version_components(
    directory: &Path,
    expected_build_id: &str,
) -> Result<()> {
    remove_components_with(directory, expected_build_id, |path| fs::remove_file(path))
}

fn remove_components_with(
    directory: &Path,
    expected_build_id: &str,
    mut remove: impl FnMut(&Path) -> io::Result<()>,
) -> Result<()> {
    if validate_windows_version_components(directory, expected_build_id)? {
        let host = directory.join(HOST_NAME);
        let metadata = directory.join(COMPONENTS_NAME);
        let bytes = image_bytes(&host)?;
        remove(&host)?;
        if let Err(error) = remove(&metadata) {
            // A sharing violation on the second file must leave a retryable
            // component pair. Restore only the bytes verified before deletion;
            // never replace a different file that appeared in the meantime.
            if !host.try_exists()? {
                write_private_atomically(&host, &bytes)
                    .context("service_host_cleanup_partial: could not restore the verified host after metadata deletion failed")?;
            }
            ensure!(
                image_bytes(&host)? == bytes,
                "service_host_cleanup_conflict: component changed during cleanup; retained metadata was not deleted"
            );
            return Err(error).context(
                "service_host_cleanup_failed: verified components restored; retry pruning",
            );
        }
    }
    Ok(())
}

/// Extract only into the immutable copy of this very executable. An updater
/// cannot accidentally put its host into an older recorder's version directory.
#[cfg(windows)]
pub(crate) fn ensure_host_for(executable: &Path) -> Result<WindowsRecorderHost> {
    let executable = fs::canonicalize(executable)?;
    let directory = executable
        .parent()
        .context("managed recorder lacks a parent directory")?;
    let versions = fs::canonicalize(crate::update::installation_root()?.join("versions"))?;
    ensure!(
        directory.parent() == Some(versions.as_path())
            && executable.file_name() == Some(OsStr::new("codex-usage-monit.exe")),
        "service_host_unmanaged: install the recorder into the managed version store first"
    );
    let binary_sha256 = hash(&image_bytes(&executable)?);
    // current_exe may be a manually downloaded candidate outside the private
    // store; verify its bytes without imposing private-store ACLs on that path.
    let current = fs::read(env::current_exe()?)?;
    ensure!(
        hash(&current) == binary_sha256,
        "service_host_build_mismatch: only this executable's immutable copy can receive its embedded host"
    );
    let expected = expected_host_bytes();
    ensure!(
        !expected.is_empty(),
        "service_host_missing: this build lacks the embedded Windows host"
    );
    let host = directory.join(HOST_NAME);
    let sha256 = hash(expected);
    match image_bytes(&host) {
        Ok(bytes) => ensure!(
            hash(&bytes) == sha256,
            "service_host_conflict: refusing to overwrite a different recorder host"
        ),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            write_private_atomically(&host, expected)?;
        }
        Err(error) => return Err(error),
    }
    let components = Components {
        schema_version: 1,
        product: "codex-usage-monit".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        build_id: env!("MONIT_BUILD_ID").into(),
        target: env!("MONIT_BUILD_TARGET").into(),
        binary_sha256: binary_sha256.clone(),
        recorder_host: HostArtifact {
            file: HOST_NAME.into(),
            sha256: sha256.clone(),
            size: expected.len() as u64,
        },
    };
    let metadata = directory.join(COMPONENTS_NAME);
    if metadata.try_exists()? {
        validate_windows_version_components(directory, env!("MONIT_BUILD_ID"))?;
    } else {
        write_private_atomically(&metadata, &serde_json::to_vec_pretty(&components)?)?;
    }
    let selected = WindowsRecorderHost {
        executable: host,
        sha256,
        recorder_sha256: binary_sha256,
    };
    validate_host(&executable, &selected)?;
    Ok(selected)
}

pub(super) fn validate_host(recorder: &Path, host: &WindowsRecorderHost) -> Result<()> {
    ensure!(
        host.executable.is_absolute()
            && recorder.is_absolute()
            && host.executable.parent() == recorder.parent()
            && host.executable.file_name() == Some(OsStr::new(HOST_NAME))
            && recorder.file_name() == Some(OsStr::new("codex-usage-monit.exe"))
            && valid_hash(&host.sha256)
            && valid_hash(&host.recorder_sha256),
        "service_host_invalid: host must be the verified recorder's sibling component"
    );
    let directory = recorder.parent().context("recorder has no directory")?;
    let components = read_components(directory)?;
    ensure!(
        validate_windows_version_components(directory, &components.build_id)?
            && components.recorder_host.sha256 == host.sha256
            && components.binary_sha256 == host.recorder_sha256,
        "service_host_invalid: task and component identities differ"
    );
    Ok(())
}

pub(super) fn prepare(options: &ServiceOptions, start_required: bool) -> Result<ServiceOptions> {
    #[allow(unused_mut)]
    let mut options = options.clone();
    #[cfg(windows)]
    {
        if start_required {
            require_interactive_session()?;
        }
        options.executable = fs::canonicalize(&options.executable)?;
        options.windows_host = Some(ensure_host_for(&options.executable)?);
    }
    #[cfg(not(windows))]
    let _ = start_required;
    Ok(options)
}

pub(super) fn host_log_file(options: &ServiceOptions) -> PathBuf {
    options.status_file.with_extension("host.log")
}

pub(super) fn task_command(options: &ServiceOptions) -> &Path {
    options
        .windows_host
        .as_ref()
        .map_or(options.executable.as_path(), |host| {
            host.executable.as_path()
        })
}

pub(super) fn task_arguments(options: &ServiceOptions) -> String {
    let normal = windows_recorder_arguments(options);
    let Some(host) = &options.windows_host else {
        return normal;
    };
    let prefix = [
        OsString::from("--host-sha256"),
        OsString::from(&host.sha256),
        OsString::from("--recorder-executable"),
        options.executable.as_os_str().to_owned(),
        OsString::from("--recorder-sha256"),
        OsString::from(&host.recorder_sha256),
        OsString::from("--log-file"),
        host_log_file(options).into_os_string(),
        OsString::from("--"),
    ];
    format!(
        "{} {normal}",
        prefix
            .iter()
            .map(|arg| quote_windows_argument(arg))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// Decode only the exact application-owned wrapper contract. The caller still
/// verifies the semantic recorder argv, full XML and trusted fingerprint.
pub(super) struct ImportedTask {
    pub recorder: String,
    pub arguments: Vec<String>,
    pub host: Option<WindowsRecorderHost>,
    pub log: Option<PathBuf>,
}

pub(super) fn unwrap_task(command: &str, mut args: Vec<String>) -> Result<ImportedTask> {
    if !args.first().is_some_and(|arg| arg == "--host-sha256") {
        return Ok(ImportedTask {
            recorder: command.into(),
            arguments: args,
            host: None,
            log: None,
        });
    }
    ensure!(
        args.len() > 9
            && args[2] == "--recorder-executable"
            && args[4] == "--recorder-sha256"
            && args[6] == "--log-file"
            && args[8] == "--",
        "service_host_invalid: unexpected wrapper arguments"
    );
    let host = WindowsRecorderHost {
        executable: command.into(),
        sha256: args[1].clone(),
        recorder_sha256: args[5].clone(),
    };
    let recorder = args[3].clone();
    let log = PathBuf::from(&args[7]);
    validate_host(Path::new(&recorder), &host)?;
    let inner = args.split_off(9);
    Ok(ImportedTask {
        recorder,
        arguments: inner,
        host: Some(host),
        log: Some(log),
    })
}

#[cfg(windows)]
pub(super) fn require_interactive_session() -> Result<()> {
    ensure!(
        has_interactive_session()?,
        "service_waiting_for_logon: the current user's interactive Windows session is required; no recorder registration was changed"
    );
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn require_interactive_session() -> Result<()> {
    Ok(())
}

/// WTS enumerates console and disconnected RDP sessions. Match their account
/// SID, not the SSH token's session ID or a user-controlled environment value.
#[cfg(windows)]
pub(super) fn has_interactive_session() -> Result<bool> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{LookupAccountNameW, SidTypeUser};
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_SESSION_INFOW, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSQuerySessionInformationW, WTSUserName,
    };
    fn text(session: u32, field: i32) -> Result<String> {
        let mut buffer = ptr::null_mut();
        let mut size = 0;
        unsafe {
            if WTSQuerySessionInformationW(ptr::null_mut(), session, field, &mut buffer, &mut size)
                == 0
            {
                return Err(io::Error::last_os_error().into());
            }
            let units = if size == 0 {
                &[]
            } else {
                ensure!(
                    !buffer.is_null(),
                    "service_session_unverifiable: empty WTS text buffer"
                );
                std::slice::from_raw_parts(buffer, size as usize / 2)
            };
            let value =
                String::from_utf16_lossy(units.split(|unit| *unit == 0).next().unwrap_or_default());
            WTSFreeMemory(buffer.cast());
            Ok(value)
        }
    }
    fn sid(account: &str) -> Result<String> {
        let account: Vec<u16> = account.encode_utf16().chain(Some(0)).collect();
        let mut sid_size = 0;
        let mut domain_size = 0;
        let mut use_type = SidTypeUser;
        unsafe {
            LookupAccountNameW(
                ptr::null(),
                account.as_ptr(),
                ptr::null_mut(),
                &mut sid_size,
                ptr::null_mut(),
                &mut domain_size,
                &mut use_type,
            );
        }
        ensure!(
            sid_size > 0 && sid_size < 65536 && domain_size < 65536,
            "service_session_unverifiable: invalid account SID size"
        );
        let mut sid = vec![0usize; (sid_size as usize).div_ceil(std::mem::size_of::<usize>())];
        let mut domain = vec![0u16; domain_size as usize];
        unsafe {
            if LookupAccountNameW(
                ptr::null(),
                account.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut sid_size,
                domain.as_mut_ptr(),
                &mut domain_size,
                &mut use_type,
            ) == 0
            {
                return Err(io::Error::last_os_error().into());
            }
            let mut formatted = ptr::null_mut();
            if ConvertSidToStringSidW(sid.as_mut_ptr().cast(), &mut formatted) == 0 {
                return Err(io::Error::last_os_error().into());
            }
            let mut length = 0;
            while *formatted.add(length) != 0 {
                length += 1;
            }
            let result = String::from_utf16_lossy(std::slice::from_raw_parts(formatted, length));
            LocalFree(formatted.cast());
            Ok(result)
        }
    }
    let current = windows_current_user_sid()?;
    let mut sessions: *mut WTS_SESSION_INFOW = ptr::null_mut();
    let mut count = 0;
    unsafe {
        if WTSEnumerateSessionsW(ptr::null_mut(), 0, 1, &mut sessions, &mut count) == 0 {
            return Err(io::Error::last_os_error())
                .context("service_session_unverifiable: could not enumerate interactive sessions");
        }
    }
    let result = (|| -> Result<bool> {
        let entries = if count == 0 {
            &[]
        } else {
            ensure!(
                !sessions.is_null(),
                "service_session_unverifiable: empty WTS session buffer"
            );
            unsafe { std::slice::from_raw_parts(sessions, count as usize) }
        };
        let mut unverified = false;
        for session in entries {
            if !(0..=5).contains(&session.State) {
                continue;
            }
            let Ok(user) = text(session.SessionId, WTSUserName) else {
                unverified = true;
                continue;
            };
            if user.is_empty() {
                continue;
            }
            let Ok(domain) = text(session.SessionId, WTSDomainName) else {
                unverified = true;
                continue;
            };
            let account = if domain.is_empty() {
                user
            } else {
                format!("{domain}\\{user}")
            };
            match sid(&account) {
                Ok(sid) if sid == current => return Ok(true),
                Ok(_) => {}
                Err(_) => unverified = true,
            }
        }
        ensure!(
            !unverified,
            "service_session_unverifiable: could not resolve every potential interactive session; recorder registration was not changed"
        );
        Ok(false)
    })();
    unsafe {
        WTSFreeMemory(sessions.cast());
    }
    result
}

#[cfg(not(windows))]
pub(super) fn has_interactive_session() -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, WindowsRecorderHost) {
        let directory = tempfile::tempdir().unwrap();
        create_private_directory(directory.path()).unwrap();
        let binary = b"verified business binary";
        let host = b"verified GUI host";
        write_private_atomically(&directory.path().join("codex-usage-monit.exe"), binary).unwrap();
        write_private_atomically(&directory.path().join(HOST_NAME), host).unwrap();
        let metadata = Components {
            schema_version: 1,
            product: "codex-usage-monit".into(),
            version: "1.0.0".into(),
            build_id: "a".repeat(64),
            target: "x86_64-pc-windows-msvc".into(),
            binary_sha256: hash(binary),
            recorder_host: HostArtifact {
                file: HOST_NAME.into(),
                sha256: hash(host),
                size: host.len() as u64,
            },
        };
        write_private_atomically(
            &directory.path().join(COMPONENTS_NAME),
            &serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let identity = WindowsRecorderHost {
            executable: directory.path().join(HOST_NAME),
            sha256: hash(host),
            recorder_sha256: hash(binary),
        };
        (directory, identity)
    }

    #[test]
    fn component_inventory_rejects_partial_or_changed_images_before_pruning() {
        let (directory, identity) = fixture();
        assert!(validate_windows_version_components(directory.path(), &"a".repeat(64)).unwrap());
        assert!(validate_windows_version_components(directory.path(), &"b".repeat(64)).is_err());
        validate_host(&directory.path().join("codex-usage-monit.exe"), &identity).unwrap();
        write_private_atomically(&identity.executable, b"changed host").unwrap();
        assert!(remove_windows_version_components(directory.path(), &"a".repeat(64)).is_err());
        assert!(identity.executable.exists());
        fs::remove_file(&identity.executable).unwrap();
        assert!(validate_windows_version_components(directory.path(), &"a".repeat(64)).is_err());
        fs::remove_file(directory.path().join(COMPONENTS_NAME)).unwrap();
        assert!(!validate_windows_version_components(directory.path(), &"a".repeat(64)).unwrap());
    }

    #[test]
    fn component_cleanup_restores_a_retryable_pair_if_metadata_is_locked() {
        let (directory, _) = fixture();
        let error = remove_components_with(directory.path(), &"a".repeat(64), |path| {
            if path.file_name() == Some(OsStr::new(COMPONENTS_NAME)) {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fixture metadata sharing violation",
                ))
            } else {
                fs::remove_file(path)
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("retry pruning"));
        assert!(validate_windows_version_components(directory.path(), &"a".repeat(64)).unwrap());
        remove_windows_version_components(directory.path(), &"a".repeat(64)).unwrap();
        assert!(!validate_windows_version_components(directory.path(), &"a".repeat(64)).unwrap());
    }

    #[test]
    fn hosted_task_identity_binds_host_and_keeps_the_recorder_semantics() {
        let (directory, host) = fixture();
        let mut options = ServiceOptions::new(
            directory.path().join("codex-usage-monit.exe"),
            directory.path().join("codex"),
            directory.path().join("history"),
            directory.path().join("status.json"),
            None,
        );
        options.offline = true;
        let semantic_identity = options.service_definition_id();
        let direct = windows_task_xml(&options, "S-1-5-21-1234");
        let direct_fingerprint =
            canonical_windows_task_fingerprint(direct.as_bytes(), "S-1-5-21-1234").unwrap();
        options.windows_host = Some(host);
        let hosted = windows_task_xml(&options, "S-1-5-21-1234");
        verify_windows_task_xml_matches_options(hosted.as_bytes(), &options, "S-1-5-21-1234")
            .unwrap();
        assert_eq!(semantic_identity, options.service_definition_id());
        assert_ne!(
            direct_fingerprint,
            canonical_windows_task_fingerprint(hosted.as_bytes(), "S-1-5-21-1234").unwrap()
        );
        let changed = hosted.replace(
            "--host-sha256 ",
            &format!("--host-sha256 {}", "0".repeat(64)),
        );
        assert!(
            verify_windows_task_xml_matches_options(changed.as_bytes(), &options, "S-1-5-21-1234")
                .is_err()
        );
        assert!(hosted.contains("<WorkingDirectory>"));
        assert!(hosted.contains("status.host.log"));
    }

    #[cfg(windows)]
    #[test]
    fn hosted_import_unwraps_only_verified_sibling_and_exact_log_arguments() {
        let (directory, host) = fixture();
        let mut options = ServiceOptions::new(
            directory.path().join("codex-usage-monit.exe"),
            directory.path().join("O'Brien 用户"),
            directory.path().join("history"),
            directory.path().join("status.json"),
            None,
        );
        options.offline = true;
        options.windows_host = Some(host.clone());
        let arguments = upgrade::windows_arguments(&task_arguments(&options)).unwrap();
        let imported = unwrap_task(host.executable.to_str().unwrap(), arguments).unwrap();
        assert_eq!(Path::new(&imported.recorder), options.executable);
        assert_eq!(imported.host, Some(host));
        assert_eq!(imported.log, Some(host_log_file(&options)));
        assert_eq!(
            imported.arguments,
            upgrade::windows_arguments(&windows_recorder_arguments(&options)).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn embedded_host_reports_same_build_and_rejects_invalid_invocations() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join(HOST_NAME);
        fs::write(&host, expected_host_bytes()).unwrap();
        let result = crate::bounded_process::output(
            Command::new(&host).arg("--host-info"),
            std::time::Duration::from_secs(15),
            4096,
        )
        .unwrap();
        assert!(result.status.success(), "{}", output_detail(&result));
        let identity: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(identity["buildId"], env!("MONIT_BUILD_ID"));
        assert_eq!(identity["target"], env!("MONIT_BUILD_TARGET"));
        let result = crate::bounded_process::output(
            Command::new(&host).arg("--not-a-recorder"),
            std::time::Duration::from_secs(15),
            4096,
        )
        .unwrap();
        assert_eq!(result.status.code(), Some(1));
    }

    #[cfg(windows)]
    #[test]
    fn embedded_host_hides_console_propagates_exit_and_owns_descendants() {
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join(HOST_NAME);
        let recorder = directory.path().join("codex-usage-monit.exe");
        fs::write(&host, expected_host_bytes()).unwrap();
        fs::copy(env::current_exe().unwrap(), &recorder).unwrap();
        let recorder_hash = hash(&fs::read(&recorder).unwrap());
        let log = directory.path().join("recorder.log");
        let command = |mode: &str| {
            let mut command = Command::new(&host);
            command
                .args([
                    "--host-sha256",
                    &hash(expected_host_bytes()),
                    "--recorder-executable",
                ])
                .arg(&recorder)
                .args(["--recorder-sha256", &recorder_hash, "--log-file"])
                .arg(&log)
                .args([
                    "--",
                    "--exact",
                    "service::windows_host::tests::hosted_child_fixture",
                    "--nocapture",
                    "--test-threads=1",
                    "--",
                    "record",
                    "--foreground",
                    "--service-cutover-protocol",
                    "source-aware-v2",
                ])
                .env("MONIT_RECORDER_HOST_TEST_MODE", mode)
                .env("MONIT_RECORDER_HOST_TEST_ROOT", directory.path());
            command
        };
        let result =
            crate::bounded_process::output(&mut command("exit"), Duration::from_secs(30), 4096)
                .unwrap();
        assert_eq!(
            result.status.code(),
            Some(37),
            "{}; log={}",
            output_detail(&result),
            fs::read_to_string(&log).unwrap_or_default()
        );
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = ChildGuard(
            command("tree")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let ready = directory.path().join("tree-ready");
        let deadline = Instant::now() + Duration::from_secs(30);
        let pids = loop {
            if let Ok(text) = fs::read_to_string(&ready)
                && let Some(text) = text.strip_suffix('\n')
            {
                let values = text
                    .split(',')
                    .map(str::parse::<u32>)
                    .collect::<std::result::Result<Vec<_>, _>>();
                if let Ok(values) = values
                    && values.len() == 2
                    && values.iter().all(|pid| *pid > 0)
                {
                    break values;
                }
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "host exited before process-tree readiness: {}",
                fs::read_to_string(&log).unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "host process tree failed to become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for ProcessHandle {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let handles = pids
            .iter()
            .map(|pid| {
                let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, *pid) };
                assert!(!handle.is_null(), "could not observe fixture process {pid}");
                ProcessHandle(handle)
            })
            .collect::<Vec<_>>();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        for handle in handles {
            assert_eq!(
                unsafe { WaitForSingleObject(handle.0, 10_000) },
                0,
                "a supervised descendant survived host termination"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn hosted_child_fixture() {
        use std::time::{Duration, Instant};
        let Ok(mode) = env::var("MONIT_RECORDER_HOST_TEST_MODE") else {
            return;
        };
        let root = PathBuf::from(env::var_os("MONIT_RECORDER_HOST_TEST_ROOT").unwrap());
        if !unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null() {
            std::process::exit(91);
        }
        if mode == "exit" {
            let mut byte = [0u8];
            if std::io::stdin().read(&mut byte).unwrap() != 0 {
                std::process::exit(92);
            }
            std::process::exit(37);
        }
        if mode == "grandchild" {
            fs::write(
                root.join("grandchild-ready"),
                format!("{}\n", std::process::id()),
            )
            .unwrap();
        } else {
            let mut descendant = Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "service::windows_host::tests::hosted_child_fixture",
                    "--nocapture",
                ])
                .env("MONIT_RECORDER_HOST_TEST_MODE", "grandchild")
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if fs::read_to_string(root.join("grandchild-ready"))
                    .is_ok_and(|text| text == format!("{}\n", descendant.id()))
                {
                    break;
                }
                if Instant::now() >= deadline {
                    std::process::exit(93);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            fs::write(
                root.join("tree-ready"),
                format!("{},{}\n", std::process::id(), descendant.id()),
            )
            .unwrap();
            // Keep a conventional owner/wait even though the test terminates
            // the enclosing Job. An unexpected descendant exit is a failure.
            let _ = descendant.wait();
            std::process::exit(94);
        }
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}
