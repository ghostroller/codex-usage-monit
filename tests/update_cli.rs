//! Exercise the actual entry executable and managed launcher using isolated
//! installation roots. These cases never invoke the real service manager.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn command(home: &Path, binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("HOME", home)
        .env("LOCALAPPDATA", home.join("local"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("CODEX_USAGE_MONIT_STATE_DIR", home.join("state"))
        .env("CODEX_USAGE_MONIT_CONFIG_DIR", home.join("config"))
        .env("CODEX_USAGE_MONIT_CACHE_DIR", home.join("cache"));
    command
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_codex-usage-monit"))
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn update_help_and_status_do_not_create_an_installation() {
    let home = tempfile::tempdir().unwrap();
    let help = success(
        command(home.path(), &binary())
            .args(["update", "--help"])
            .output()
            .unwrap(),
    );
    assert!(help.contains("--scope"));
    assert!(help.contains("--adopt"));
    let status: Value = serde_json::from_str(&success(
        command(home.path(), &binary())
            .args(["update", "status", "--format", "json"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert!(status["versions"].as_array().unwrap().is_empty());
    assert!(!Path::new(status["root"].as_str().unwrap()).exists());
}

#[test]
fn installed_candidate_and_stable_launcher_report_the_selected_build() {
    let home = tempfile::tempdir().unwrap();
    let bytes = fs::read(binary()).unwrap();
    let sha = format!("{:x}", Sha256::digest(&bytes));
    let installed = success(
        command(home.path(), &binary())
            .args(["remote-agent", "install", "--sha256", &sha])
            .output()
            .unwrap(),
    );
    let installed = PathBuf::from(installed.trim());
    assert!(installed.is_absolute());
    assert!(installed.components().any(|p| p.as_os_str() == "versions"));
    assert_eq!(fs::read(&installed).unwrap(), bytes);
    let version_dir = installed.parent().unwrap();
    let root = version_dir.parent().unwrap().parent().unwrap();
    let selected: Value =
        serde_json::from_slice(&fs::read(version_dir.join("build.json")).unwrap()).unwrap();
    let entry_dir = home.path().join("bin");
    fs::create_dir(&entry_dir).unwrap();
    let entry = entry_dir.join(if cfg!(windows) {
        "codex-usage-monit.exe"
    } else {
        "codex-usage-monit"
    });
    fs::copy(&installed, &entry).unwrap();
    let entry = entry.canonicalize().unwrap();
    let registration = serde_json::json!({"schemaVersion":1,"executable":entry,"launcherSha256":sha,"selected":selected});
    let registration_path = root.join("installation.json");
    fs::write(
        &registration_path,
        serde_json::to_vec(&registration).unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&registration_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let direct = success(
        command(home.path(), &installed)
            .args(["remote-agent", "info", "--sha256"])
            .output()
            .unwrap(),
    );
    let proxied = success(
        command(home.path(), &entry)
            .args(["remote-agent", "info", "--sha256"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&direct).unwrap(),
        serde_json::from_str::<Value>(&proxied).unwrap()
    );
    let status: Value = serde_json::from_str(&success(
        command(home.path(), &entry)
            .args(["update", "status", "--format", "json"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(
        PathBuf::from(status["executable"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        installed.canonicalize().unwrap(),
        "the launcher must actually transfer execution to the selected version"
    );
    let version = success(command(home.path(), &entry).arg("-V").output().unwrap());
    assert_eq!(
        version.trim(),
        format!("codex-usage-monit {}", env!("CARGO_PKG_VERSION"))
    );
}

#[cfg(windows)]
#[test]
fn running_portable_launcher_with_different_bytes_passes_real_proxy_contract() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let candidate_bytes = fs::read(binary()).unwrap();
    let candidate_sha = format!("{:x}", Sha256::digest(&candidate_bytes));
    let installed = PathBuf::from(
        success(
            command(home.path(), &binary())
                .args(["remote-agent", "install", "--sha256", &candidate_sha])
                .output()
                .unwrap(),
        )
        .trim(),
    );
    let root = installed
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let portable_dir = home.path().join("portable with spaces");
    fs::create_dir(&portable_dir).unwrap();
    let portable = portable_dir.join("codex-usage-monit.exe");
    // A PE overlay changes the full executable checksum without replacing the
    // actual application/launcher implementation with a scripted imitation.
    let mut portable_bytes = candidate_bytes;
    portable_bytes.extend_from_slice(b"older portable payload fixture");
    fs::write(&portable, &portable_bytes).unwrap();
    assert_ne!(
        format!("{:x}", Sha256::digest(&portable_bytes)),
        candidate_sha
    );
    struct HeldApplication(std::process::Child);
    impl Drop for HeldApplication {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    // Export waits for a framed request while its pipe remains open. It does
    // not install a service or touch usage data before receiving that request.
    let mut held = HeldApplication(
        command(home.path(), &portable)
            .args(["remote-agent", "export"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // CreateProcess has already mapped the image when spawn returns; this
    // checks the relevant OS state without timing-dependent sleeps.
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&portable)
            .is_err()
    );
    assert!(
        codex_usage_monit::update::verify_launcher_compatibility(&portable, &installed).unwrap()
    );
    assert!(held.0.try_wait().unwrap().is_none());
    assert_eq!(fs::read(&portable).unwrap(), portable_bytes);
    assert!(
        !root.join("installation.json").exists(),
        "probe must not register the live CLI"
    );
    assert!(
        !root.join("update-journal.json").exists(),
        "probe must not start a live update"
    );
    assert!(fs::read_dir(root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".launcher-probe-")
    }));
}

#[cfg(windows)]
#[test]
fn actual_executable_without_the_launcher_contract_is_not_retained() {
    let home = tempfile::tempdir().unwrap();
    let bytes = fs::read(binary()).unwrap();
    let sha = format!("{:x}", Sha256::digest(&bytes));
    let installed = PathBuf::from(
        success(
            command(home.path(), &binary())
                .args(["remote-agent", "install", "--sha256", &sha])
                .output()
                .unwrap(),
        )
        .trim(),
    );
    // The actual libtest executable has no launcher dispatcher. It must not be
    // accepted merely because it is a valid, user-owned Windows application.
    assert!(
        !codex_usage_monit::update::verify_launcher_compatibility(
            &std::env::current_exe().unwrap(),
            &installed,
        )
        .unwrap()
    );
}

/// Run at a migration checkpoint with an earlier independently built executable.
/// Keeping this opt-in avoids pretending that a PE overlay is a second build.
#[cfg(windows)]
#[test]
#[ignore = "requires CODEX_USAGE_MONIT_PREVIOUS_TEST_BINARY from a different trusted build"]
fn different_build_running_portable_launcher_keeps_proxy_protocol() {
    use std::process::Stdio;
    let previous = PathBuf::from(
        std::env::var_os("CODEX_USAGE_MONIT_PREVIOUS_TEST_BINARY")
            .expect("provide a trusted earlier build"),
    );
    let home = tempfile::tempdir().unwrap();
    let info = |executable: &Path| -> Value {
        serde_json::from_str(&success(
            command(home.path(), executable)
                .args(["remote-agent", "info", "--sha256"])
                .output()
                .unwrap(),
        ))
        .unwrap()
    };
    let before = info(&previous);
    let after = info(&binary());
    assert_ne!(
        before["buildId"], after["buildId"],
        "this checkpoint needs two actual builds"
    );
    assert_eq!(before["target"], after["target"]);
    let installed = PathBuf::from(
        success(
            command(home.path(), &binary())
                .args([
                    "remote-agent",
                    "install",
                    "--sha256",
                    after["executableSha256"].as_str().unwrap(),
                ])
                .output()
                .unwrap(),
        )
        .trim(),
    );
    let directory = home.path().join("运行中的旧 CLI");
    fs::create_dir(&directory).unwrap();
    let portable = directory.join("codex-usage-monit.exe");
    fs::copy(&previous, &portable).unwrap();
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut running = Running(
        command(home.path(), &portable)
            .args(["remote-agent", "export"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&portable)
            .is_err()
    );
    assert!(
        codex_usage_monit::update::verify_launcher_compatibility(&portable, &installed).unwrap()
    );
    assert!(running.0.try_wait().unwrap().is_none());
    assert_eq!(
        info(&portable)["buildId"],
        before["buildId"],
        "the probe must not change the live selection"
    );
    assert_eq!(fs::read(portable).unwrap(), fs::read(previous).unwrap());
}
