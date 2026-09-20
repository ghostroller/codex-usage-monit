use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

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

fn success(stage: &str, command: &mut Command) -> Vec<u8> {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{stage}: could not launch {command:?}: {error}"));
    assert!(
        output.status.success(),
        "{stage}: {command:?} exited with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
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
        "bootstrap info",
        command(root.path()).args(["remote-agent", "info", "--sha256"]),
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
            "immutable installation",
            command(root.path()).args(["remote-agent", "install", "--sha256", &digest]),
        ))
        .unwrap()
        .trim()
        .to_owned()
    };
    let installed = install();
    assert_eq!(installed, install());
    let info: Value = serde_json::from_slice(&success(
        "direct immutable executable",
        isolated_command(root.path(), &installed).args(["remote-agent", "info", "--sha256"]),
    ))
    .unwrap();
    assert_eq!(info["executableSha256"], digest);
    #[cfg(windows)]
    {
        use base64::Engine;
        use std::os::windows::process::CommandExt;

        // Match the production SSH command: cmd cannot directly execute a
        // canonical \\?\ path, so invoke it through encoded PowerShell. Pass
        // cmd's command text literally rather than applying CRT argv quoting.
        let script = format!(
            "$ErrorActionPreference='Stop'; & '{}' remote-agent info; exit $LASTEXITCODE",
            installed.replace('\'', "''")
        );
        let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let from_shell: Value = serde_json::from_slice(&success(
            "cmd shell via encoded PowerShell",
            isolated_command(root.path(), "cmd.exe")
                .args(["/d", "/s", "/c"])
                .raw_arg(format!(
                    "powershell.exe -NoProfile -NonInteractive -EncodedCommand {encoded}"
                )),
        ))
        .unwrap();
        assert_eq!(from_shell["buildId"], info["buildId"]);
        let from_shell: Value = serde_json::from_slice(&success(
            "direct PowerShell literal invocation",
            isolated_command(root.path(), "powershell.exe").args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ]),
        ))
        .unwrap();
        assert_eq!(from_shell["buildId"], info["buildId"]);
    }
    assert!(!root.path().join("state").exists());
}
