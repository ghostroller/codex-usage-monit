//! Stable bootstrap discovery and explicit, version-isolated agent deployment.
//! This is deliberately independent from the framed data protocol. It does not
//! negotiate or translate older data protocols.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use crate::{
    private_state_store::{LockFilePolicy, LockMode, PrivateStoreLayout},
    remote_protocol::REMOTE_PROTOCOL_VERSION,
    remote_transport::{SSH_OPTIONS, SshCommandEnvironment},
};

const MAX_BINARY: u64 = 128 * 1024 * 1024;
const MAX_INFO: usize = 32 * 1024;
const TIMEOUT: Duration = Duration::from_secs(300);
const TARGETS: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];
const LAYOUT: PrivateStoreLayout = PrivateStoreLayout {
    store_name: "managed remote agents",
    data_file_name: "agent",
    data_path_name: "agent executable",
    data_subject: "agent executable",
    lock_file_name: "install.lock",
    lock_subject: "agent install lock",
    temporary_subject: "agent staging file",
    maximum_file_bytes: MAX_BINARY,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentInfo {
    pub schema_version: u32,
    pub product: String,
    pub version: String,
    pub build_id: String,
    pub target: String,
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
}

impl AgentInfo {
    pub fn local() -> Self {
        Self {
            schema_version: 1,
            product: "codex-usage-monit".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            build_id: env!("MONIT_BUILD_ID").into(),
            target: env!("MONIT_BUILD_TARGET").into(),
            protocol_version: REMOTE_PROTOCOL_VERSION,
            executable_sha256: None,
        }
    }

    pub fn with_checksum(mut self) -> Result<Self> {
        self.executable_sha256 = Some(checksum(&read_binary(&env::current_exe()?)?));
        Ok(self)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.product == "codex-usage-monit",
            "agent_info_invalid: unsupported bootstrap metadata"
        );
        ensure!(
            is_hash(&self.build_id)
                && self.version.len() <= 80
                && !self.version.is_empty()
                && self
                    .version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')),
            "agent_info_invalid: invalid build identity"
        );
        ensure!(
            TARGETS.contains(&self.target.as_str())
                || matches!(
                    self.target.as_str(),
                    "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
                ),
            "agent_target_unsupported: {}",
            self.target
        );
        Ok(())
    }

    pub fn check_protocol(&self) -> Result<()> {
        ensure!(
            self.protocol_version == REMOTE_PROTOCOL_VERSION,
            "agent_version_mismatch: remote {} protocol {} build {}; local {} requires protocol {} build {}. Use Settings [B] Deploy agent or remote deploy HOST. Old data protocols are not supported.",
            self.version,
            self.protocol_version,
            self.build_id,
            env!("CARGO_PKG_VERSION"),
            REMOTE_PROTOCOL_VERSION,
            env!("MONIT_BUILD_ID")
        );
        Ok(())
    }

    pub fn summary(&self) -> String {
        format!(
            "version={} protocol={} build={} target={}",
            self.version, self.protocol_version, self.build_id, self.target
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentManifest {
    schema_version: u32,
    agent: AgentInfo,
    file: String,
    size: u64,
    sha256: String,
}

fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn read_binary(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)
        .with_context(|| format!("agent_artifact_missing: {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= MAX_BINARY,
        "agent_artifact_invalid: binary must be a regular file under 128 MiB"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BINARY + 1).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_BINARY,
        "agent_artifact_invalid: invalid binary size"
    );
    Ok(bytes)
}

fn artifact_name(target: &str) -> String {
    format!(
        "codex-usage-monit-{target}.agent{}",
        if target.contains("windows") {
            ".exe"
        } else {
            ""
        }
    )
}
fn manifest_name(target: &str) -> String {
    format!("codex-usage-monit-{target}.agent.json")
}
fn managed_executable(info: &AgentInfo, digest: &str) -> String {
    let path = format!(
        "./.codex-usage-monit-agents/{}/{}/{}/codex-usage-monit{}",
        &info.build_id[..32],
        info.target,
        &digest[..16],
        if info.target.contains("windows") {
            ".exe"
        } else {
            ""
        }
    );
    if info.target.contains("windows") {
        path.replace('/', "\\")
    } else {
        path
    }
}

/// Runs only after official download or explicit development upload verification.
/// Publication never replaces
/// a different build or the user's global binary/recorder installation.
pub(crate) fn install_self(expected_sha256: &str) -> Result<String> {
    ensure!(
        is_hash(expected_sha256),
        "agent_checksum_invalid: expected SHA-256 must be lowercase hex"
    );
    let bytes = read_binary(&env::current_exe()?)?;
    ensure!(
        checksum(&bytes) == expected_sha256,
        "agent_checksum_mismatch: candidate executable changed"
    );
    install_bytes(&env::current_dir()?, &AgentInfo::local(), &bytes)
}

fn install_bytes(base: &Path, info: &AgentInfo, bytes: &[u8]) -> Result<String> {
    info.validate()?;
    let relative = managed_executable(info, &checksum(bytes));
    let path = base.join(relative.replace('\\', "/").trim_start_matches("./"));
    let root = base.join(".codex-usage-monit-agents");
    let parent = path.parent().expect("managed path parent");
    LAYOUT.create_directory_beneath(&root, parent)?;
    let _lock = LAYOUT.open_lock(parent, LockMode::Exclusive, LockFilePolicy::Create)?;
    match LAYOUT.read_bounded(&path) {
        Ok(existing) => ensure!(
            checksum(&existing) == checksum(bytes),
            "agent_install_conflict: this build path already contains a different binary; existing agent was preserved"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            LAYOUT.write_atomically(&path, bytes)?
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(relative)
}

#[cfg(test)]
type CommandRunner<'a> = &'a dyn Fn(&Command) -> Result<Output>;

pub(crate) struct AgentConnection<'a> {
    host: &'a str,
    environment: &'a SshCommandEnvironment,
    deadline: Instant,
    #[cfg(test)]
    runner: Option<CommandRunner<'a>>,
}

impl<'a> AgentConnection<'a> {
    pub fn new(host: &'a str, environment: &'a SshCommandEnvironment) -> Result<Self> {
        crate::remotes_config::validate_remote_host_input("agent", host, "codex-usage-monit")
            .map_err(anyhow::Error::msg)?;
        ensure!(
            host.bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b'-' | b'@')),
            "agent_host_invalid: deployment requires an ASCII SSH alias containing only letters, digits, '.', '_', '-' or '@'"
        );
        Ok(Self {
            host,
            environment,
            deadline: Instant::now() + TIMEOUT,
            #[cfg(test)]
            runner: None,
        })
    }

    fn output(&self, command: &mut Command, timeout: Duration, limit: usize) -> Result<Output> {
        self.output_with_stdin(command, timeout, limit, Stdio::null())
    }

    fn output_with_stdin(
        &self,
        command: &mut Command,
        timeout: Duration,
        limit: usize,
        stdin: Stdio,
    ) -> Result<Output> {
        let remaining = self
            .deadline
            .saturating_duration_since(Instant::now())
            .min(timeout);
        ensure!(
            !remaining.is_zero(),
            "agent_operation_timeout: exceeded five-minute operation budget"
        );
        self.environment.apply(command);
        #[cfg(test)]
        if let Some(runner) = self.runner {
            return runner(command);
        }
        crate::bounded_process::output_cancellable_with_stdin(
            command,
            remaining,
            limit,
            stdin,
            || self.environment.cancellation_requested(),
        )
        .context("agent_command_failed")
    }

    fn ssh(&self, script: &str) -> Result<Output> {
        let mut command = Command::new(self.environment.resolve_program()?);
        command
            .args(SSH_OPTIONS)
            .arg("--")
            .arg(self.host)
            .arg(script);
        self.output(&mut command, Duration::from_secs(45), MAX_INFO)
    }

    pub fn inspect(&self, executable: &str, sha256: bool) -> Result<Option<AgentInfo>> {
        crate::remotes_config::validate_agent_executable(executable).map_err(anyhow::Error::msg)?;
        let output = self.ssh(&format!(
            "{executable} remote-agent info{}",
            if sha256 { " --sha256" } else { "" }
        ))?;
        if !output.status.success() {
            let diagnostic = safe_diagnostic(&output.stderr);
            if output.status.code() != Some(255)
                && (output.status.code() == Some(127)
                    || diagnostic.contains("unrecognized subcommand")
                    || diagnostic.contains("unexpected argument 'info'")
                    || diagnostic.contains("not recognized")
                    || diagnostic.contains("command not found")
                    || diagnostic.contains("No such file or directory"))
            {
                return Ok(None);
            }
            bail!(
                "agent_discovery_failed: SSH exit {:?}: {diagnostic}",
                output.status.code()
            );
        }
        let info: AgentInfo = serde_json::from_slice(&output.stdout)
            .context("agent_info_invalid: expected bootstrap JSON (check shell startup output)")?;
        info.validate()?;
        Ok(Some(info))
    }

    fn target(&self, executable: &str) -> Result<String> {
        if let Some(info) = self.inspect(executable, false)? {
            // Linux release agents are static musl binaries even when the
            // existing installation came from a native GNU development build.
            return Ok(info.target.replace("-linux-gnu", "-linux-musl"));
        }
        let output = self.ssh("uname -s -m")?;
        if output.status.success() {
            return target_from_uname(&String::from_utf8_lossy(&output.stdout));
        }
        ensure!(
            output.status.code() != Some(255),
            "agent_platform_failed: {}",
            safe_diagnostic(&output.stderr)
        );
        // EncodedCommand is accepted by both cmd.exe and PowerShell SSH shells.
        let output = self.ssh(&powershell("[Console]::Write([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString())"))?;
        ensure!(
            output.status.success(),
            "agent_platform_failed: {}",
            safe_diagnostic(&output.stderr)
        );
        ensure!(
            String::from_utf8_lossy(&output.stdout).trim() == "X64",
            "agent_target_unsupported: Windows agent currently requires x64"
        );
        Ok("x86_64-pc-windows-msvc".into())
    }

    /// Official mode downloads directly on the SSH host. No local artifact,
    /// environment override, current-executable copy or upload fallback exists.
    pub fn deploy(&self, executable: &str) -> Result<String> {
        let target = self.target(executable)?;
        let mut required = AgentInfo::local();
        required.target = target;
        let stage = format!(".codex-usage-monit-release-{}", nonce()?);
        let windows = required.target.contains("windows");
        let candidate = if windows {
            format!(".\\{stage}\\agent.exe")
        } else {
            format!("./{stage}/agent")
        };
        let result = (|| {
            let local = LocalStaging::new()?;
            let input_path = local.0.join("bootstrap");
            LAYOUT.write_atomically(&input_path, release_script(&required, &stage)?.as_bytes())?;
            let mut command = Command::new(self.environment.resolve_program()?);
            command
                .args(SSH_OPTIONS)
                .arg("--")
                .arg(self.host)
                .arg(if windows {
                    powershell("& ([scriptblock]::Create([Console]::In.ReadToEnd()))")
                } else {
                    "python3 -".into()
                });
            let output = self.output_with_stdin(
                &mut command,
                TIMEOUT,
                MAX_INFO,
                Stdio::from(fs::File::open(input_path)?),
            )?;
            successful(&output, "agent_release_prepare_failed")?;
            let manifest: AgentManifest = serde_json::from_slice(&output.stdout)
                .context("agent_release_invalid: remote bootstrap returned invalid metadata")?;
            validate_manifest(&manifest, &required.target)?;
            if !output.stderr.is_empty() {
                eprintln!("warning: {}", safe_diagnostic(&output.stderr));
            }
            self.install_candidate(&candidate, &required, &manifest.sha256)
        })();
        let cleanup = if windows {
            powershell(&format!(
                "$ErrorActionPreference='Stop'; if (Test-Path -LiteralPath './{stage}') {{ foreach ($n in @('agent.exe','manifest.json')) {{ $p=Join-Path './{stage}' $n; if (Test-Path -LiteralPath $p) {{ Remove-Item -LiteralPath $p -Force }} }}; [IO.Directory]::Delete((Join-Path (Get-Location).Path '{stage}'),$false) }}"
            ))
        } else {
            format!(
                "if [ -d ./{stage} ]; then rm -f ./{stage}/agent ./{stage}/manifest.json && rmdir ./{stage}; fi"
            )
        };
        if self
            .ssh(&cleanup)
            .and_then(|output| successful(&output, "agent_release_cleanup_failed"))
            .is_err()
        {
            eprintln!(
                "warning: agent_release_cleanup_failed: staging directory {stage} may remain on the remote host"
            );
        }
        result
    }

    /// Explicit development-only upload. The operator trusts the supplied
    /// local build; matching metadata/checksums are not publisher authentication.
    pub fn deploy_dev(&self, executable: &str, bundle: &Path) -> Result<String> {
        let target = self.target(executable)?;
        let staging = LocalStaging::new()?;
        let (path, digest) = self.artifact(&target, bundle, &staging.0)?;
        let mut required = AgentInfo::local();
        required.target = target.clone();
        let upload = format!(
            ".codex-usage-monit-upload-{}{}",
            nonce()?,
            if target.contains("windows") {
                ".exe"
            } else {
                ".bin"
            }
        );
        let result = (|| {
            let ssh = self.environment.resolve_program()?;
            let scp = ssh.with_file_name(if cfg!(windows) { "scp.exe" } else { "scp" });
            let mut command = Command::new(scp);
            command
                .arg("-q")
                .args(&SSH_OPTIONS[1..])
                .arg("--")
                .arg(&path)
                .arg(format!("{}:{upload}", self.host));
            let output = self.output(&mut command, Duration::from_secs(180), MAX_INFO)?;
            successful(&output, "agent_upload_failed")?;
            if !target.contains("windows") {
                successful(
                    &self.ssh(&format!("chmod 700 ./{upload}"))?,
                    "agent_upload_failed",
                )?;
            }
            let candidate = if target.contains("windows") {
                format!(".\\{upload}")
            } else {
                format!("./{upload}")
            };
            self.install_candidate(&candidate, &required, &digest)
        })();
        // Remove only this operation's unpredictable staging file. A failed
        // cleanup must not hide the primary error; no recursive deletion.
        let cleanup = if target.contains("windows") {
            powershell(&format!(
                "$ErrorActionPreference='Stop'; if (Test-Path -LiteralPath './{upload}') {{ Remove-Item -LiteralPath './{upload}' -Force }}"
            ))
        } else {
            format!("rm -f ./{upload}")
        };
        if self
            .ssh(&cleanup)
            .and_then(|output| successful(&output, "agent_upload_cleanup_failed"))
            .is_err()
        {
            eprintln!(
                "warning: agent_upload_cleanup_failed: staging file {upload} may remain in the remote SSH working directory"
            );
        }
        result
    }

    fn install_candidate(
        &self,
        candidate: &str,
        required: &AgentInfo,
        digest: &str,
    ) -> Result<String> {
        let output = self.ssh(&format!(
            "{candidate} remote-agent install --sha256 {digest}"
        ))?;
        successful(&output, "agent_install_failed")?;
        let installed = managed_executable(required, digest);
        let actual = self
            .inspect(&installed, true)?
            .context("agent_verification_failed: installed agent has no bootstrap metadata")?;
        verify_match(&actual, required, digest)?;
        Ok(installed)
    }

    fn artifact(
        &self,
        target: &str,
        directory: &Path,
        staging: &Path,
    ) -> Result<(PathBuf, String)> {
        let manifest = read_manifest(&directory.join(manifest_name(target)))?;
        validate_manifest(&manifest, target)?;
        let bytes = read_binary(&directory.join(&manifest.file))?;
        ensure!(
            bytes.len() as u64 == manifest.size && checksum(&bytes) == manifest.sha256,
            "agent_checksum_mismatch: artifact differs from its manifest"
        );
        let path = staging.join("verified-agent.bin");
        LAYOUT.write_atomically(&path, &bytes)?;
        Ok((path, manifest.sha256))
    }
}

fn release_script(required: &AgentInfo, stage: &str) -> Result<String> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(required)?);
    if required.target.contains("windows") {
        Ok(format!(
            "{}\ntry {{ $e=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')) | ConvertFrom-Json; Invoke-ReleasePreparation $e '{stage}' | ConvertTo-Json -Depth 8 -Compress; exit 0 }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 1 }}\n",
            include_str!("remote_agent_manager/release_bootstrap.ps1")
        ))
    } else {
        Ok(format!(
            "{}\nimport base64, sys\ntry:\n    print(json.dumps(prepare_release(json.loads(base64.b64decode('{encoded}')), '{stage}')))\nexcept Exception as error:\n    print(str(error), file=sys.stderr)\n    sys.exit(1)\n",
            include_str!("remote_agent_manager/release_bootstrap.py")
        ))
    }
}

fn safe_diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .take(1200)
        .collect()
}
fn successful(output: &Output, stage: &str) -> Result<()> {
    ensure!(
        output.status.success(),
        "{stage}: SSH/command exit {:?}: {}",
        output.status.code(),
        safe_diagnostic(&output.stderr)
    );
    Ok(())
}
fn powershell(script: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "powershell.exe -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}
fn target_from_uname(text: &str) -> Result<String> {
    match text.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["Darwin", "arm64" | "aarch64"] => Ok("aarch64-apple-darwin".into()),
        ["Darwin", "x86_64"] => Ok("x86_64-apple-darwin".into()),
        ["Linux", "aarch64" | "arm64"] => Ok("aarch64-unknown-linux-musl".into()),
        ["Linux", "x86_64"] => Ok("x86_64-unknown-linux-musl".into()),
        [system, "x86_64"]
            if system.starts_with("MINGW")
                || system.starts_with("MSYS")
                || system.starts_with("CYGWIN") =>
        {
            Ok("x86_64-pc-windows-msvc".into())
        }
        _ => bail!("agent_target_unsupported: unsupported uname OS/architecture"),
    }
}
fn read_manifest(path: &Path) -> Result<AgentManifest> {
    let file = fs::File::open(path).context("agent_artifact_missing: manifest unavailable")?;
    ensure!(
        file.metadata()?.is_file(),
        "agent_manifest_invalid: expected a regular manifest file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_INFO as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_INFO,
        "agent_manifest_invalid: manifest too large"
    );
    serde_json::from_slice(&bytes).context("agent_manifest_invalid")
}
fn validate_manifest(manifest: &AgentManifest, target: &str) -> Result<()> {
    manifest.agent.validate()?;
    ensure!(
        manifest.schema_version == 1
            && manifest.file == artifact_name(target)
            && manifest.size > 0
            && manifest.size <= MAX_BINARY
            && is_hash(&manifest.sha256),
        "agent_manifest_invalid: invalid artifact metadata"
    );
    let mut required = AgentInfo::local();
    required.target = target.into();
    ensure!(
        manifest.agent.build_id == required.build_id
            && manifest.agent.protocol_version == required.protocol_version
            && manifest.agent.target == required.target
            && manifest.agent.version == required.version,
        "agent_artifact_mismatch: artifact {} does not match required {}. Build/package the same source; old protocol compatibility is intentionally unavailable",
        manifest.agent.summary(),
        required.summary()
    );
    Ok(())
}
fn verify_match(actual: &AgentInfo, required: &AgentInfo, digest: &str) -> Result<()> {
    ensure!(
        actual.build_id == required.build_id
            && actual.version == required.version
            && actual.target == required.target
            && actual.protocol_version == required.protocol_version
            && actual.executable_sha256.as_deref() == Some(digest),
        "agent_verification_failed: installed agent identity or checksum differs; configuration was not changed"
    );
    Ok(())
}
fn nonce() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("agent_random_failed: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
struct LocalStaging(PathBuf);
impl LocalStaging {
    fn new() -> Result<Self> {
        let root = env::temp_dir().join(format!("monit-agent-{}", nonce()?));
        LAYOUT.create_directory_beneath(&root, &root)?;
        Ok(Self(root))
    }
}
impl Drop for LocalStaging {
    fn drop(&mut self) {
        // This fresh private directory contains only fixed-name download and
        // staging files. Do not recursively traverse a changed directory tree.
        if let Ok(entries) = fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn output(code: u32, stdout: &[u8], stderr: &[u8]) -> Output {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw({
                #[cfg(windows)]
                {
                    code
                }
                #[cfg(unix)]
                {
                    (code as i32) << 8
                }
            }),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn manifest(bytes: &[u8]) -> AgentManifest {
        let info = AgentInfo::local();
        AgentManifest {
            schema_version: 1,
            file: artifact_name(&info.target),
            agent: info,
            size: bytes.len() as u64,
            sha256: checksum(bytes),
        }
    }

    #[test]
    fn bootstrap_is_stable_json_and_rejects_protocol_drift_even_at_same_version() {
        let local = AgentInfo::local();
        local.validate().unwrap();
        let mut remote: AgentInfo =
            serde_json::from_slice(&serde_json::to_vec(&local).unwrap()).unwrap();
        remote.protocol_version -= 1;
        let error = remote.check_protocol().unwrap_err().to_string();
        assert!(error.contains("agent_version_mismatch"));
        assert!(error.contains("Old data protocols are not supported"));
        remote.schema_version = 2;
        assert!(remote.validate().is_err());
    }

    #[test]
    fn platform_probe_recognizes_git_for_windows_uname_without_treating_it_as_linux() {
        for system in [
            "MINGW64_NT-10.0-26200",
            "MSYS_NT-10.0-26200",
            "CYGWIN_NT-10.0",
        ] {
            assert_eq!(
                target_from_uname(&format!("{system} x86_64")).unwrap(),
                "x86_64-pc-windows-msvc"
            );
        }
        assert!(target_from_uname("unknown x86_64").is_err());
    }

    #[test]
    fn artifacts_must_match_source_build_target_version_protocol_and_checksum() {
        let mut data = manifest(b"agent");
        validate_manifest(&data, &AgentInfo::local().target).unwrap();
        data.agent.build_id = "f".repeat(64);
        assert!(
            validate_manifest(&data, &AgentInfo::local().target)
                .unwrap_err()
                .to_string()
                .contains("agent_artifact_mismatch")
        );
        for change in ["target", "version", "protocol", "path"] {
            let mut data = manifest(b"agent");
            match change {
                "target" => data.agent.target = "aarch64-apple-darwin".into(),
                "version" => data.agent.version = "0.0.1".into(),
                "protocol" => data.agent.protocol_version -= 1,
                _ => data.file = "../other.exe".into(),
            }
            assert!(validate_manifest(&data, "x86_64-pc-windows-msvc").is_err());
        }
        let mut installed = AgentInfo::local();
        installed.executable_sha256 = Some(checksum(b"changed"));
        assert!(verify_match(&installed, &AgentInfo::local(), &checksum(b"agent")).is_err());
    }

    #[test]
    fn install_is_immutable_idempotent_and_keeps_other_builds() {
        let directory = tempfile::tempdir().unwrap();
        let info = AgentInfo::local();
        let first = install_bytes(directory.path(), &info, b"first").unwrap();
        assert_eq!(
            first,
            install_bytes(directory.path(), &info, b"first").unwrap()
        );
        let second = install_bytes(directory.path(), &info, b"second build profile").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(directory.path().join(&first)).unwrap(), b"first");
        let mut different = info.clone();
        different.build_id = "0".repeat(64);
        let third = install_bytes(directory.path(), &different, b"third").unwrap();
        assert_ne!(first, third);
        let first_path = directory.path().join(&first);
        fs::write(&first_path, b"tampered").unwrap();
        assert!(
            install_bytes(directory.path(), &info, b"first")
                .unwrap_err()
                .to_string()
                .contains("agent_install_conflict")
        );
        assert_eq!(fs::read(first_path).unwrap(), b"tampered");
    }

    #[cfg(windows)]
    #[test]
    fn deployment_checks_every_stage_before_returning_a_verified_path() {
        let directory = tempfile::tempdir().unwrap();
        let data = manifest(b"test binary");
        fs::write(directory.path().join(&data.file), b"test binary").unwrap();
        fs::write(
            directory.path().join(manifest_name(&data.agent.target)),
            serde_json::to_vec(&data).unwrap(),
        )
        .unwrap();
        let environment = SshCommandEnvironment::default();
        for failure in ["none", "upload", "install", "checksum", "identity"] {
            let calls = RefCell::new(Vec::new());
            let runner = |command: &Command| {
                let args = command
                    .get_args()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ");
                calls.borrow_mut().push(args.clone());
                assert!(args.contains("StrictHostKeyChecking=yes"));
                assert!(args.contains("BatchMode=yes"));
                if command.get_program().to_string_lossy().contains("scp") {
                    return Ok(output(
                        if failure == "upload" { 1 } else { 0 },
                        b"",
                        b"transfer",
                    ));
                }
                if args.contains("old-agent remote-agent info") {
                    // A legacy agent can still identify the platform through
                    // the separate bootstrap OS probe, never through v4 data.
                    return Ok(output(64, b"", b"unrecognized subcommand 'info'"));
                }
                if args.contains("uname -s -m") {
                    #[cfg(windows)]
                    {
                        return Ok(output(1, b"", b"not recognized"));
                    }
                    #[cfg(target_os = "macos")]
                    {
                        return Ok(output(
                            0,
                            if cfg!(target_arch = "aarch64") {
                                b"Darwin arm64"
                            } else {
                                b"Darwin x86_64"
                            },
                            b"",
                        ));
                    }
                    #[cfg(target_os = "linux")]
                    {
                        return Ok(output(
                            0,
                            if cfg!(target_arch = "aarch64") {
                                b"Linux aarch64"
                            } else {
                                b"Linux x86_64"
                            },
                            b"",
                        ));
                    }
                }
                if args.contains("remote-agent install") {
                    return Ok(output(
                        if failure == "install" { 1 } else { 0 },
                        b"installed",
                        b"install failure",
                    ));
                }
                if args.contains("remote-agent info --sha256") {
                    let mut info = AgentInfo::local();
                    info.executable_sha256 = Some(if failure == "checksum" {
                        "0".repeat(64)
                    } else {
                        data.sha256.clone()
                    });
                    if failure == "identity" {
                        info.build_id = "0".repeat(64);
                    }
                    return Ok(output(0, &serde_json::to_vec(&info).unwrap(), b""));
                }
                Ok(output(0, b"X64", b""))
            };
            let mut connection = AgentConnection::new("test-host", &environment).unwrap();
            connection.runner = Some(&runner);
            let result = connection.deploy_dev("old-agent", directory.path());
            if failure == "none" {
                assert_eq!(
                    result.unwrap(),
                    managed_executable(&data.agent, &data.sha256)
                );
            } else {
                assert!(result.is_err(), "{failure}");
            }
            let calls = calls.borrow();
            assert!(calls.iter().any(|call| call.contains("remote-agent info")));
            if failure == "upload" {
                assert!(
                    !calls
                        .iter()
                        .any(|call| call.contains("remote-agent install"))
                );
            }
            assert!(
                calls.last().unwrap().contains("EncodedCommand")
                    || calls.last().unwrap().contains("rm -f")
            );
        }
    }

    #[test]
    fn official_release_deployment_uses_ssh_only_and_never_falls_back_to_upload() {
        for target in ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"] {
            for failure in ["none", "missing", "download", "mismatch", "verification"] {
                let mut data = manifest(b"official binary");
                data.agent.target = target.into();
                data.file = artifact_name(target);
                let calls = RefCell::new(Vec::new());
                let bootstrap = if target.contains("windows") {
                    powershell("& ([scriptblock]::Create([Console]::In.ReadToEnd()))")
                } else {
                    "python3 -".into()
                };
                let runner = |command: &Command| {
                    assert!(!command.get_program().to_string_lossy().contains("scp"));
                    assert!(!command.get_program().to_string_lossy().contains("curl"));
                    let args = command
                        .get_args()
                        .map(|arg| arg.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(" ");
                    assert!(args.contains("StrictHostKeyChecking=yes"));
                    calls.borrow_mut().push(args.clone());
                    if args.ends_with("old-agent remote-agent info") {
                        return Ok(output(0, &serde_json::to_vec(&data.agent).unwrap(), b""));
                    }
                    if args.ends_with(&bootstrap) {
                        if matches!(failure, "missing" | "download") {
                            return Ok(output(
                                1,
                                b"",
                                if failure == "missing" {
                                    b"agent_release_unavailable: 404"
                                } else {
                                    b"agent_release_download_failed: offline"
                                },
                            ));
                        }
                        let mut returned: AgentManifest =
                            serde_json::from_slice(&serde_json::to_vec(&data).unwrap()).unwrap();
                        if failure == "mismatch" {
                            returned.agent.build_id = "0".repeat(64);
                        }
                        return Ok(output(0, &serde_json::to_vec(&returned).unwrap(), b""));
                    }
                    if args.contains("remote-agent info --sha256") {
                        let mut info = data.agent.clone();
                        info.executable_sha256 = Some(if failure == "verification" {
                            "0".repeat(64)
                        } else {
                            data.sha256.clone()
                        });
                        return Ok(output(0, &serde_json::to_vec(&info).unwrap(), b""));
                    }
                    Ok(output(0, b"", b""))
                };
                let environment = SshCommandEnvironment::default();
                let mut connection = AgentConnection::new("test-host", &environment).unwrap();
                connection.runner = Some(&runner);
                let result = connection.deploy("old-agent");
                assert_eq!(
                    result.is_ok(),
                    failure == "none",
                    "{target}: {failure}: {result:?}"
                );
                let installed = calls
                    .borrow()
                    .iter()
                    .any(|call| call.contains("remote-agent install"));
                assert_eq!(installed, matches!(failure, "none" | "verification"));
                if failure == "none" {
                    assert_eq!(
                        result.unwrap(),
                        managed_executable(&data.agent, &data.sha256)
                    );
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn official_bootstrap_runs_over_stdin_in_both_windows_shells() {
        // Exercise the actual encoded-command trampoline and embedded entrypoint,
        // not just a dot-sourced function. The fixture is deliberately not executable.
        for shell in ["powershell.exe", "pwsh.exe"] {
            for corrupt in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let data = manifest(b"downloaded bytes, never executed during preparation");
                fs::write(
                    directory.path().join("fixture.json"),
                    serde_json::to_vec(&data).unwrap(),
                )
                .unwrap();
                fs::write(
                    directory.path().join("fixture.bin"),
                    if corrupt {
                        b"corrupt".as_slice()
                    } else {
                        b"downloaded bytes, never executed during preparation".as_slice()
                    },
                )
                .unwrap();
                let stage = format!(".codex-usage-monit-release-{}", "1".repeat(32));
                let script = release_script(&data.agent, &stage).unwrap().replace(
                    "\ntry { $e=",
                    "\nfunction Receive-ReleaseAsset { param($Url,$Destination,$Maximum); $fixture=if ($Url.EndsWith('.json')) {'fixture.json'} else {'fixture.bin'}; [IO.File]::Copy((Join-Path (Get-Location).Path $fixture),$Destination) }\ntry { $e=",
                );
                let input = directory.path().join("input");
                fs::write(&input, script).unwrap();
                let trampoline = powershell("& ([scriptblock]::Create([Console]::In.ReadToEnd()))");
                let mut command = Command::new(shell);
                command
                    .args(trampoline.split_whitespace().skip(1))
                    .current_dir(directory.path());
                let output = crate::bounded_process::output_cancellable_with_stdin(
                    &mut command,
                    Duration::from_secs(30),
                    MAX_INFO,
                    Stdio::from(fs::File::open(input).unwrap()),
                    || false,
                )
                .unwrap();
                assert_eq!(
                    output.status.success(),
                    !corrupt,
                    "{shell}: {}",
                    safe_diagnostic(&output.stderr)
                );
                if corrupt {
                    assert!(safe_diagnostic(&output.stderr).contains("agent_checksum_mismatch"));
                    assert!(!directory.path().join(&stage).exists());
                } else {
                    let actual: AgentManifest = serde_json::from_slice(&output.stdout).unwrap();
                    validate_manifest(&actual, &data.agent.target).unwrap();
                    assert_eq!(actual.sha256, data.sha256);
                    assert_eq!(
                        fs::read(directory.path().join(stage).join("agent.exe")).unwrap(),
                        b"downloaded bytes, never executed during preparation"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_metadata_and_authentication_failures_never_trigger_deployment_fallback() {
        let environment = SshCommandEnvironment::default();
        for (code, stdout, stderr) in [
            (255, "", "Permission denied (publickey)"),
            (0, "shell greeting", ""),
            (1, "", "Permission denied"),
        ] {
            let calls = RefCell::new(0);
            let runner = |_: &Command| {
                *calls.borrow_mut() += 1;
                Ok(output(code, stdout.as_bytes(), stderr.as_bytes()))
            };
            let mut connection = AgentConnection::new("test-host", &environment).unwrap();
            connection.runner = Some(&runner);
            assert!(connection.target("agent").is_err());
            assert_eq!(*calls.borrow(), 1);
        }
        for host in ["-option", "dev;whoami", "dev:other", "dev/path"] {
            assert!(AgentConnection::new(host, &environment).is_err());
        }
    }

    #[test]
    fn corrupt_or_wrong_build_bundles_never_upload() {
        let directory = tempfile::tempdir().unwrap();
        let mut data = manifest(b"agent");
        fs::write(directory.path().join(&data.file), b"wrong").unwrap();
        let environment = SshCommandEnvironment::default();
        let connection = AgentConnection::new("test-host", &environment).unwrap();
        for wrong_build in [false, true] {
            if wrong_build {
                data.agent.build_id = "0".repeat(64);
            }
            fs::write(
                directory.path().join(manifest_name(&data.agent.target)),
                serde_json::to_vec(&data).unwrap(),
            )
            .unwrap();
            let staging = LocalStaging::new().unwrap();
            let error = connection
                .artifact(&data.agent.target, directory.path(), &staging.0)
                .unwrap_err();
            assert!(error.to_string().contains(if wrong_build {
                "agent_artifact_mismatch"
            } else {
                "agent_checksum_mismatch"
            }));
        }
    }
}
