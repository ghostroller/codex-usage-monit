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
