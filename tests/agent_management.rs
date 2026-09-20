use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn command(root: &Path) -> Command {
    isolated_command(root, env!("CARGO_BIN_EXE_codex-usage-monit"))
}

fn isolated_command(root: &Path, executable: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(root)
        .env("HOME", root)
        .env("LOCALAPPDATA", root.join("local"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("CODEX_USAGE_MONIT_STATE_DIR", root.join("state"))
        .env("CODEX_USAGE_MONIT_CONFIG_DIR", root.join("config"))
        .env("CODEX_USAGE_MONIT_CACHE_DIR", root.join("cache"));
    command
}

fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn bootstrap_info_is_independent_of_state_catalog_and_usage_protocol() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("config")).unwrap();
    fs::write(
        root.path().join("config/model-catalog.json"),
        b"invalid JSON",
    )
    .unwrap();
    let info: Value = serde_json::from_slice(&success(
        command(root.path())
            .args(["remote-agent", "info", "--sha256"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(info["schemaVersion"], 1);
    assert_eq!(
        info["protocolVersion"],
        codex_usage_monit::remote_protocol::REMOTE_PROTOCOL_VERSION
    );
    assert_eq!(info["buildId"].as_str().unwrap().len(), 64);
    let bytes = fs::read(env!("CARGO_BIN_EXE_codex-usage-monit")).unwrap();
    assert_eq!(
        info["executableSha256"],
        format!("{:x}", Sha256::digest(bytes))
    );
    assert!(
        !root.path().join("state").exists(),
        "info must not create a source identity"
    );
}

#[test]
fn self_install_checks_bytes_and_can_execute_its_immutable_copy() {
    let root = tempfile::Builder::new()
        .prefix("agent install with spaces ")
        .tempdir()
        .unwrap();
    let bytes = fs::read(env!("CARGO_BIN_EXE_codex-usage-monit")).unwrap();
    let digest = format!("{:x}", Sha256::digest(bytes));
    let rejected = command(root.path())
        .args(["remote-agent", "install", "--sha256", &"0".repeat(64)])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!root.path().join(".codex-usage-monit-agents").exists());
    let install = || {
        String::from_utf8(success(
            command(root.path())
                .args(["remote-agent", "install", "--sha256", &digest])
                .output()
                .unwrap(),
        ))
        .unwrap()
        .trim()
        .to_owned()
    };
    let installed = install();
    assert_eq!(installed, install());
    let output = isolated_command(root.path(), &installed)
        .args(["remote-agent", "info", "--sha256"])
        .output()
        .unwrap();
    let info: Value = serde_json::from_slice(&success(output)).unwrap();
    assert_eq!(info["executableSha256"], digest);
    #[cfg(windows)]
    {
        // OpenSSH's default Windows shell is cmd.exe. Forward slash paths
        // beginning ./ are parsed as a command plus options and do not work.
        let output = isolated_command(root.path(), "cmd.exe")
            .args([
                "/d",
                "/s",
                "/c",
                &format!("\"\"{installed}\" remote-agent info\""),
            ])
            .output()
            .unwrap();
        let from_shell: Value = serde_json::from_slice(&success(output)).unwrap();
        assert_eq!(from_shell["buildId"], info["buildId"]);
        let output = isolated_command(root.path(), "powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("& '{}' remote-agent info", installed.replace('\'', "''")),
            ])
            .output()
            .unwrap();
        let from_shell: Value = serde_json::from_slice(&success(output)).unwrap();
        assert_eq!(from_shell["buildId"], info["buildId"]);
    }
    assert!(!root.path().join("state").exists());
}
