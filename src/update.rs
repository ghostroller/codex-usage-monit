//! Target-local activation shared by local updates and SSH deployments.
//!
//! Versions are immutable. The public command is a stable launcher whose private
//! registration selects a version; running Windows launchers need not be replaced.
//! Recorder replacement retains its separate service journal and writer fences.

use std::env;
use std::ffi::OsStr;
#[cfg(any(unix, test))]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::private_state_store::{LockFilePolicy, LockMode, PrivateStoreLayout};
use crate::remote_agent_manager::AgentInfo;
use crate::service::ServiceUpgradeReport;

const MAX_BINARY: u64 = 128 * 1024 * 1024;
const MAX_METADATA: u64 = 64 * 1024;
const REGISTRATION: &str = "installation.json";
const JOURNAL: &str = "update-journal.json";
const VERSION_METADATA: &str = "build.json";
const BINARY_NAME: &str = if cfg!(windows) {
    "codex-usage-monit.exe"
} else {
    "codex-usage-monit"
};
const STORE: PrivateStoreLayout = PrivateStoreLayout {
    store_name: "application installation",
    data_file_name: "installation",
    data_path_name: "installation metadata",
    data_subject: "installation file",
    lock_file_name: "update.lock",
    lock_subject: "application update lock",
    temporary_subject: "application update staging file",
    maximum_file_bytes: MAX_METADATA,
};
const BINARIES: PrivateStoreLayout = PrivateStoreLayout {
    maximum_file_bytes: MAX_BINARY,
    ..STORE
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateScope {
    /// Update the exporter and any existing recorder; preserve the CLI entry.
    Sync,
    /// Also update the application-managed command-line entry.
    Node,
}

impl UpdateScope {
    fn argument(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Node => "node",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApplyOptions {
    pub scope: UpdateScope,
    pub install_dir: Option<PathBuf>,
    pub adopt: bool,
    #[serde(default)]
    pub allow_dev_build: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CliUpdateReport {
    pub outcome: String,
    pub executable: Option<PathBuf>,
    /// Resolution in this process's PATH, not a claim about every login shell.
    pub resolved_executable: Option<PathBuf>,
    pub shadowed: bool,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpdateReport {
    pub schema_version: u32,
    pub outcome: String,
    pub build_id: String,
    pub version: String,
    pub executable: PathBuf,
    pub scope: UpdateScope,
    pub recorder: ServiceUpgradeReport,
    pub cli: CliUpdateReport,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstalledVersion {
    pub version: String,
    pub build_id: String,
    pub target: String,
    pub sha256: String,
    pub executable: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateStatus {
    pub schema_version: u32,
    pub root: PathBuf,
    pub executable: PathBuf,
    pub source: String,
    pub version: String,
    pub build_id: String,
    pub cli: CliUpdateReport,
    pub versions: Vec<InstalledVersion>,
    pub journal_phase: Option<String>,
    pub last_update: Option<UpdateReport>,
}

pub(crate) struct PruneOptions {
    pub versions: Vec<String>,
    pub apply: bool,
    pub acknowledge_unreferenced: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PruneVersion {
    pub id: String,
    pub executable: Option<PathBuf>,
    pub action: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PruneReport {
    pub schema_version: u32,
    pub applied: bool,
    pub versions: Vec<PruneVersion>,
    /// Historical deployment directories are inventory only, never deleted.
    pub legacy_roots: Vec<PathBuf>,
}

/// Keep this launcher contract stable: an old launcher must read new pointers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Installation {
    schema_version: u32,
    executable: PathBuf,
    launcher_sha256: String,
    selected: InstalledVersion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CliPlan {
    executable: PathBuf,
    previous_sha256: Option<String>,
    launcher_sha256: String,
    previous_registration: Option<Installation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateJournal {
    schema_version: u32,
    target: InstalledVersion,
    scope: UpdateScope,
    phase: String,
    cli_plan: Option<CliPlan>,
    report: UpdateReport,
}

pub(crate) fn installation_root() -> Result<PathBuf> {
    let path = if cfg!(windows) {
        env_path("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?
    } else if cfg!(target_os = "macos") {
        env_path("HOME")
            .context("HOME is unavailable")?
            .join("Library/Application Support")
    } else {
        env_path("XDG_DATA_HOME")
            .or_else(|| env_path("HOME").map(|p| p.join(".local/share")))
            .context("HOME and XDG_DATA_HOME are unavailable")?
    };
    ensure!(
        path.is_absolute(),
        "update_root_invalid: application data directory must be absolute"
    );
    normalized_absolute(&path.join("codex-usage-monit"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn default_install_dir(root: &Path) -> Result<PathBuf> {
    if cfg!(windows) {
        Ok(root.join("bin"))
    } else {
        Ok(env_path("HOME")
            .context("HOME is unavailable")?
            .join(".local/bin"))
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 80
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

fn version_path(root: &Path, version: &str, sha256: &str) -> Result<PathBuf> {
    ensure!(
        valid_version(version) && valid_hash(sha256),
        "update_identity_invalid: invalid version or checksum"
    );
    Ok(root
        .join("versions")
        .join(format!("{version}-{sha256}"))
        .join(BINARY_NAME))
}

fn validate_version(root: &Path, installed: &InstalledVersion) -> Result<()> {
    ensure!(
        valid_hash(&installed.build_id),
        "update_identity_invalid: invalid build ID"
    );
    ensure!(
        installed.executable == version_path(root, &installed.version, &installed.sha256)?,
        "update_path_invalid: executable is outside its immutable version directory"
    );
    ensure!(
        STORE.directory_exists_beneath(
            root,
            installed
                .executable
                .parent()
                .context("missing version directory")?
        )?,
        "update_version_missing: immutable version is missing"
    );
    ensure!(
        digest(&BINARIES.read_bounded(&installed.executable)?) == installed.sha256,
        "update_checksum_mismatch: immutable executable changed"
    );
    Ok(())
}

fn read_external_binary(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "update_entry_invalid: executable must be a regular file"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0,
            "update_entry_invalid: executable must not be a reparse point"
        );
    }
    ensure!(
        metadata.len() > 0 && metadata.len() <= MAX_BINARY,
        "update_entry_invalid: executable exceeds the 128 MiB limit"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_BINARY + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_BINARY,
        "update_entry_invalid: executable size changed"
    );
    Ok(bytes)
}

fn executable_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn install_bytes(root: &Path, info: &AgentInfo, bytes: &[u8]) -> Result<InstalledVersion> {
    ensure!(
        info.schema_version == 1
            && info.product == "codex-usage-monit"
            && valid_hash(&info.build_id),
        "update_identity_invalid: unsupported application identity"
    );
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_BINARY,
        "update_binary_invalid: executable exceeds the 128 MiB limit"
    );
    let sha256 = digest(bytes);
    let executable = version_path(root, &info.version, &sha256)?;
    let parent = executable
        .parent()
        .context("missing version directory")?
        .to_path_buf();
    STORE.create_directory_beneath(root, &parent)?;
    let _lock = STORE.open_lock(root, LockMode::Exclusive, LockFilePolicy::Create)?;
    match BINARIES.read_bounded(&executable) {
        Ok(existing) => ensure!(
            digest(&existing) == sha256,
            "update_install_conflict: immutable version differs; existing file preserved"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            BINARIES.write_atomically(&executable, bytes)?;
            executable_permissions(&executable)?;
        }
        Err(error) => return Err(error.into()),
    }
    let installed = InstalledVersion {
        version: info.version.clone(),
        build_id: info.build_id.clone(),
        target: info.target.clone(),
        sha256,
        executable,
    };
    let metadata = parent.join(VERSION_METADATA);
    if let Some(existing) = read_json::<InstalledVersion>(&metadata)? {
        ensure!(
            existing == installed,
            "update_install_conflict: existing version metadata differs"
        );
    } else {
        write_json(&metadata, &installed)?;
    }
    Ok(installed)
}

/// Install this complete executable without replacing any existing CLI/service.
pub(crate) fn install_current() -> Result<PathBuf> {
    let root = installation_root()?;
    let info = AgentInfo::local();
    let installed = install_bytes(&root, &info, &read_external_binary(&env::current_exe()?)?)?;
    // Reading current_exe's path may race an external in-place replacement. Ask
    // the installed copy for its identity before accepting it as this build.
    let mut command = Command::new(&installed.executable);
    command.args(["remote-agent", "info", "--sha256"]);
    let output = crate::bounded_process::output(
        &mut command,
        Duration::from_secs(30),
        MAX_METADATA as usize,
    )?;
    ensure!(
        output.status.success(),
        "update_candidate_invalid: installed executable did not report its identity"
    );
    let actual: AgentInfo = serde_json::from_slice(&output.stdout)
        .context("update_candidate_invalid: invalid executable identity")?;
    ensure!(
        actual.version == info.version
            && actual.build_id == info.build_id
            && actual.target == info.target
            && actual.protocol_version == info.protocol_version
            && actual.executable_sha256.as_deref() == Some(&installed.sha256),
        "update_candidate_invalid: executable changed while this process was running"
    );
    Ok(installed.executable)
}

pub(crate) fn apply(options: ApplyOptions) -> Result<UpdateReport> {
    #[cfg(windows)]
    crate::installation::ownership_preflight()?;
    ensure!(
        options.scope == UpdateScope::Node || (options.install_dir.is_none() && !options.adopt),
        "update_scope_invalid: install-dir/adopt require node scope"
    );
    let installed = install_current()?;
    if env::current_exe()?.canonicalize()? != installed.canonicalize()? {
        let mut command = Command::new(&installed);
        command.args([
            "update",
            "apply",
            "--scope",
            options.scope.argument(),
            "--format",
            "json",
        ]);
        if let Some(directory) = &options.install_dir {
            command.arg("--install-dir").arg(directory);
        }
        if options.adopt {
            command.arg("--adopt");
        }
        if options.allow_dev_build {
            command.arg("--allow-dev-build");
        }
        let output = crate::bounded_process::output(
            &mut command,
            Duration::from_secs(210),
            MAX_METADATA as usize,
        )?;
        if let Ok(report) = serde_json::from_slice::<UpdateReport>(&output.stdout) {
            validate_apply_report(
                &report,
                &AgentInfo::local(),
                &installed,
                options.scope,
                output.status.code(),
            )?;
            return Ok(report);
        }
        bail!(
            "update_candidate_failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let root = installation_root()?;
    let target: InstalledVersion = read_json(
        &installed
            .parent()
            .context("missing version parent")?
            .join(VERSION_METADATA),
    )?
    .context("update_version_missing: metadata is missing")?;
    for recorder in crate::service::inspect_update_recorder()? {
        ensure_not_downgrade(
            &target.version,
            &target.build_id,
            &recorder.version,
            recorder.build_id.as_deref(),
            options.allow_dev_build,
        )?;
    }
    apply_at(
        &root,
        target,
        &options,
        &default_install_dir(&root)?,
        env::var_os("PATH").as_deref(),
        || crate::service::upgrade_registered_recorder_for_update(options.allow_dev_build),
    )
}

pub(crate) fn validate_apply_report(
    report: &UpdateReport,
    expected: &AgentInfo,
    executable: &Path,
    scope: UpdateScope,
    exit_code: Option<i32>,
) -> Result<()> {
    let expected_exit = match report.outcome.as_str() {
        "complete" => 0,
        "partial" => 2,
        "failed" => 1,
        _ => bail!("update_report_invalid: unexpected outcome"),
    };
    ensure!(
        report.schema_version == 1
            && report.build_id == expected.build_id
            && report.version == expected.version
            && report.executable == executable
            && report.scope == scope
            && exit_code == Some(expected_exit),
        "update_report_invalid: candidate identity, scope or exit status differs"
    );
    ensure!(
        report.recorder.build_id == expected.build_id,
        "update_report_invalid: recorder belongs to another build"
    );
    if report.outcome == "complete" || report.outcome == "partial" {
        ensure!(
            matches!(
                report.recorder.outcome.as_str(),
                "ready" | "disabled" | "not_installed"
            ) && report.recorder.enabled == (report.recorder.outcome == "ready"),
            "update_report_invalid: recorder is not ready"
        );
        if report.recorder.enabled {
            ensure!(
                report.recorder.pid.is_some_and(|pid| pid > 0)
                    && report.recorder.last_history_heartbeat.is_some(),
                "update_report_invalid: recorder heartbeat is missing"
            );
        }
    }
    match scope {
        UpdateScope::Sync => ensure!(
            report.cli.outcome == "not_requested"
                && report.cli.executable.is_none()
                && report.outcome != "partial",
            "update_report_invalid: sync scope unexpectedly changed the CLI"
        ),
        UpdateScope::Node if report.outcome == "complete" => ensure!(
            report.cli.outcome == "updated" && report.cli.executable.is_some(),
            "update_report_invalid: requested CLI activation is incomplete"
        ),
        UpdateScope::Node if report.outcome == "partial" => ensure!(
            report.cli.outcome == "failed" && report.cli.executable.is_some(),
            "update_report_invalid: partial update lacks its CLI failure"
        ),
        _ => {}
    }
    Ok(())
}

fn apply_at(
    root: &Path,
    target: InstalledVersion,
    options: &ApplyOptions,
    default_directory: &Path,
    path: Option<&OsStr>,
    upgrade_recorder: impl FnOnce() -> Result<ServiceUpgradeReport>,
) -> Result<UpdateReport> {
    let _lock = STORE.open_lock(root, LockMode::Exclusive, LockFilePolicy::Create)?;
    #[cfg(windows)]
    crate::installation::ownership_preflight_at(root)?;
    validate_version(root, &target)?;
    if let Some(registration) = read_installation(root)? {
        ensure_not_downgrade(
            &target.version,
            &target.build_id,
            &registration.selected.version,
            Some(&registration.selected.build_id),
            options.allow_dev_build,
        )?;
    }
    let previous: Option<UpdateJournal> = read_json(&root.join(JOURNAL))?;
    let mut journal = if let Some(previous) = previous.filter(|j| j.phase != "complete") {
        ensure!(
            previous.schema_version == 1
                && previous.target == target
                && previous.scope == options.scope,
            "update_pending: retry the previous candidate and scope before starting another update"
        );
        if let Some(requested) = &options.install_dir {
            let desired = normalized_absolute(requested)?.join(BINARY_NAME);
            ensure!(
                previous
                    .cli_plan
                    .as_ref()
                    .is_some_and(|p| p.executable == desired),
                "update_pending: install directory differs from the retained update"
            );
        }
        if let Some(plan) = &previous.cli_plan {
            validate_cli_plan(root, plan, &target)?;
        }
        previous
    } else {
        let cli_plan = if options.scope == UpdateScope::Node {
            Some(plan_cli(root, options, default_directory, path, &target)?)
        } else {
            None
        };
        let cli = cli_report(
            cli_plan.as_ref().map(|p| p.executable.clone()),
            path,
            if cli_plan.is_some() {
                "pending"
            } else {
                "not_requested"
            },
        );
        UpdateJournal {
            schema_version: 1,
            target: target.clone(),
            scope: options.scope,
            phase: "prepared".into(),
            cli_plan,
            report: UpdateReport {
                schema_version: 1,
                outcome: "pending".into(),
                build_id: target.build_id.clone(),
                version: target.version.clone(),
                executable: target.executable.clone(),
                scope: options.scope,
                recorder: ServiceUpgradeReport {
                    outcome: "pending".into(),
                    build_id: target.build_id.clone(),
                    enabled: false,
                    pid: None,
                    last_history_heartbeat: None,
                    diagnostic: None,
                },
                cli,
                diagnostic: None,
            },
        }
    };
    // A first Windows migration may need to replace an ordinary executable.
    // Check and retain write access before stopping the recorder: a running
    // image cannot be opened this way, and this handle prevents a new image
    // mapping until publication has completed. Compatible launchers need no
    // replacement and therefore remain usable throughout the update.
    let _replacement_guard =
        preflight_cli_replacement(journal.cli_plan.as_ref(), &target, options)?;
    journal.phase = "prepared".into();
    journal.report.diagnostic = None;
    journal.report.recorder.outcome = "pending".into();
    journal.report.recorder.diagnostic = None;
    if journal.cli_plan.is_some() {
        journal.report.cli.outcome = "pending".into();
    }
    write_json(&root.join(JOURNAL), &journal)?;
    let result = (|| -> Result<()> {
        journal.phase = "upgrading_recorder".into();
        write_json(&root.join(JOURNAL), &journal)?;
        let recorder = upgrade_recorder()?;
        ensure!(
            recorder.build_id == target.build_id
                && matches!(
                    recorder.outcome.as_str(),
                    "ready" | "disabled" | "not_installed"
                ),
            "update_recorder_invalid: unexpected recorder result"
        );
        journal.report.recorder = recorder;
        journal.phase = "activating_cli".into();
        write_json(&root.join(JOURNAL), &journal)?;
        if let Some(plan) = &journal.cli_plan {
            activate_cli(root, plan, &target)?;
            journal.report.cli = cli_report(Some(plan.executable.clone()), path, "updated");
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            journal.phase = "complete".into();
            journal.report.outcome = "complete".into();
        }
        Err(error) => {
            journal.phase = "failed".into();
            let recorder_finished = matches!(
                journal.report.recorder.outcome.as_str(),
                "ready" | "disabled" | "not_installed"
            );
            journal.report.outcome = if recorder_finished {
                "partial"
            } else {
                "failed"
            }
            .into();
            let diagnostic = format!(
                "{error:#}; retry this candidate's update apply with the same scope to continue; old versions and service configuration are retained"
            );
            journal.report.diagnostic = Some(diagnostic.clone());
            if recorder_finished {
                journal.report.cli.outcome = "failed".into();
                journal.report.cli.diagnostic = Some(diagnostic);
            } else {
                journal.report.recorder.outcome = "failed".into();
                journal.report.recorder.diagnostic = Some(diagnostic);
            }
        }
    }
    write_json(&root.join(JOURNAL), &journal)?;
    Ok(journal.report)
}

fn normalized_absolute(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    ensure!(
        !absolute
            .components()
            .any(|c| matches!(c, Component::ParentDir)),
        "update_path_invalid: parent traversal is not accepted"
    );
    // Canonicalize the existing prefix, including parent symlinks, while
    // retaining missing final components for a first installation. Ownership
    // checks and launcher current_exe matching must use the same physical path.
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .context("update_path_invalid: no existing ancestor")?
                        .to_os_string(),
                );
                existing = existing
                    .parent()
                    .context("update_path_invalid: no existing ancestor")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut canonical = existing.canonicalize()?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

/// Package versions order releases; source build hashes never imply recency.
pub(crate) fn ensure_not_downgrade(
    target_version: &str,
    target_build: &str,
    current_version: &str,
    current_build: Option<&str>,
    allow_dev_build: bool,
) -> Result<()> {
    use std::cmp::Ordering;
    let ordering = compare_versions(target_version, current_version)?;
    ensure!(
        ordering != Ordering::Less,
        "update_downgrade_blocked: target {target_version} is older than installed {current_version}; update this center before changing the shared node"
    );
    ensure!(
        ordering != Ordering::Equal
            || current_build.is_none_or(|build| build == target_build)
            || allow_dev_build,
        "update_build_conflict: equal package versions have different source builds; only an explicit development bundle may replace a same-version build"
    );
    Ok(())
}

fn compare_versions(left: &str, right: &str) -> Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    fn parts(version: &str) -> Result<([u64; 3], Option<&str>)> {
        let version = version.split('+').next().context("missing version")?;
        let (main, pre) = version
            .split_once('-')
            .map_or((version, None), |(main, pre)| (main, Some(pre)));
        let words = main.split('.').collect::<Vec<_>>();
        ensure!(
            words.len() == 3,
            "update_version_invalid: expected semantic package version"
        );
        let mut numeric = [0; 3];
        for (slot, word) in numeric.iter_mut().zip(words) {
            ensure!(
                !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()),
                "update_version_invalid: invalid numeric version"
            );
            *slot = word
                .parse()
                .context("update_version_invalid: version component overflow")?;
        }
        if let Some(pre) = pre {
            ensure!(
                !pre.is_empty()
                    && pre.split('.').all(|part| !part.is_empty()
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
                "update_version_invalid: invalid prerelease"
            );
        }
        Ok((numeric, pre))
    }
    let (left, left_pre) = parts(left)?;
    let (right, right_pre) = parts(right)?;
    let main = left.cmp(&right);
    if main != Ordering::Equal {
        return Ok(main);
    }
    let (left, right) = match (left_pre, right_pre) {
        (None, None) => return Ok(Ordering::Equal),
        (None, Some(_)) => return Ok(Ordering::Greater),
        (Some(_), None) => return Ok(Ordering::Less),
        (Some(left), Some(right)) => (left, right),
    };
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        let (a, b) = match (left.next(), right.next()) {
            (None, None) => return Ok(Ordering::Equal),
            (None, Some(_)) => return Ok(Ordering::Less),
            (Some(_), None) => return Ok(Ordering::Greater),
            (Some(a), Some(b)) => (a, b),
        };
        let a_numeric = a.bytes().all(|c| c.is_ascii_digit());
        let b_numeric = b.bytes().all(|c| c.is_ascii_digit());
        let comparison = match (a_numeric, b_numeric) {
            (true, true) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => a.cmp(b),
        };
        if comparison != Ordering::Equal {
            return Ok(comparison);
        }
    }
}

fn package_manager_path(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|c| {
            if let Component::Normal(s) = c {
                s.to_str().map(str::to_ascii_lowercase)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    components
        .iter()
        .any(|p| matches!(p.as_str(), "cellar" | "caskroom" | "scoop" | "chocolatey"))
        || components
            .windows(2)
            .any(|p| p == [".cargo", "bin"] || p == ["homebrew", "bin"])
        || path.ancestors().take(3).any(|directory| {
            directory.join(".crates.toml").is_file() || directory.join(".crates2.json").is_file()
        })
}

fn validate_install_directory(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "update_install_dir_invalid: CLI directory must be a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o022 == 0,
            "update_install_dir_untrusted: CLI directory must be owned by this user and not writable by others"
        );
    }
    #[cfg(windows)]
    crate::source_identity::validate_windows_private_directory(directory, "CLI install directory")?;
    Ok(())
}

fn entry_hash(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "update_entry_unmanaged: refusing to replace a symlink, directory or special file; choose a separate install directory"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() }
                        && metadata.permissions().mode() & 0o022 == 0,
                    "update_entry_untrusted: CLI executable is not privately owned"
                );
            }
            Ok(Some(digest(&read_external_binary(path)?)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_installation(root: &Path) -> Result<Option<Installation>> {
    let Some(value) = read_json::<Installation>(&root.join(REGISTRATION))? else {
        return Ok(None);
    };
    ensure!(
        value.schema_version == 1
            && value.executable.is_absolute()
            && value.executable.file_name() == Some(OsStr::new(BINARY_NAME))
            && valid_hash(&value.launcher_sha256),
        "update_registration_invalid: invalid launcher registration"
    );
    ensure!(
        value.selected.executable
            == version_path(root, &value.selected.version, &value.selected.sha256)?,
        "update_registration_invalid: target is outside version storage"
    );
    Ok(Some(value))
}

fn plan_cli(
    root: &Path,
    options: &ApplyOptions,
    default_directory: &Path,
    path: Option<&OsStr>,
    target: &InstalledVersion,
) -> Result<CliPlan> {
    let existing = read_installation(root)?;
    // Preserve the PATH entry itself, not its resolved file target: a package
    // manager or user-owned symlink must still fail the entry ownership check.
    // An explicit directory or an existing registration takes precedence.
    let discovered = if options.install_dir.is_none() && existing.is_none() {
        resolve_path_entry(path)
    } else {
        None
    };
    if let Some(discovered) = &discovered {
        ensure!(
            discovered
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(BINARY_NAME)),
            "update_entry_wrapper: PATH resolves {}; wrappers cannot be adopted as an executable. Choose --install-dir explicitly, then review PATH precedence with Get-Command codex-usage-monit -All",
            discovered.display()
        );
    }
    let directory = normalized_absolute(
        options
            .install_dir
            .as_deref()
            .or_else(|| existing.as_ref().and_then(|r| r.executable.parent()))
            .or_else(|| discovered.as_deref().and_then(Path::parent))
            .unwrap_or(default_directory),
    )?;
    ensure!(
        !directory.starts_with(root.join("versions"))
            && !directory.components().any(|component| {
                matches!(component, Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".codex-usage-monit-agents"))
            }),
        "update_entry_conflict: CLI entry cannot replace an immutable managed version or legacy agent; choose a separate command directory"
    );
    ensure!(
        !package_manager_path(&directory),
        "update_external_install: package-manager installation is not managed here; update it through its original installer or choose another directory"
    );
    if directory.exists() {
        validate_install_directory(&directory)?;
    }
    let executable = directory.join(BINARY_NAME);
    let previous_sha256 = entry_hash(&executable)?;
    let launcher_sha256 = if let Some(registration) = &existing {
        ensure!(
            registration.executable == executable,
            "update_entry_conflict: another CLI entry is registered; reuse its directory"
        );
        ensure!(
            previous_sha256.as_deref() == Some(&registration.launcher_sha256),
            "update_entry_conflict: the managed CLI was replaced outside the updater; existing file preserved"
        );
        registration.launcher_sha256.clone()
    } else {
        ensure!(
            previous_sha256.is_none() || options.adopt,
            "update_entry_unmanaged: existing CLI is not managed; use --adopt to explicitly migrate this user-owned file or choose another install directory"
        );
        #[cfg(windows)]
        let compatible = if let Some(previous) = &previous_sha256 {
            previous != &target.sha256
                && verify_compatible_launcher(root, &executable, previous, target)?
        } else {
            false
        };
        #[cfg(not(windows))]
        let compatible = false;
        if compatible {
            previous_sha256.clone().expect("verified existing launcher")
        } else {
            target.sha256.clone()
        }
    };
    Ok(CliPlan {
        executable,
        previous_sha256,
        launcher_sha256,
        previous_registration: existing,
    })
}

/// Prove the old executable understands the frozen launcher contract without
/// changing the live registration. Version strings alone are not evidence: a
/// portable build may have different capabilities at the same package version.
#[cfg(windows)]
fn verify_compatible_launcher(
    root: &Path,
    executable: &Path,
    previous_sha256: &str,
    target: &InstalledVersion,
) -> Result<bool> {
    let bytes = read_external_binary(executable)?;
    ensure!(
        digest(&bytes) == previous_sha256,
        "update_entry_conflict: old CLI changed before its launcher compatibility check"
    );
    if !has_windows_pe_header(&bytes) {
        return Ok(false);
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("update_probe_failed: {e}"))?;
    let base = root.join(format!(".launcher-probe-{}", digest(&random)));
    let probe_root = base.join("codex-usage-monit");
    STORE.create_directory_beneath(root, &probe_root)?;
    struct Cleanup {
        files: Vec<PathBuf>,
        directories: Vec<PathBuf>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            // Delete only files created by this probe. Unexpected files are
            // retained rather than recursively deleting executable-owned data.
            for path in &self.files {
                let _ = fs::remove_file(path);
            }
            for path in &self.directories {
                let _ = fs::remove_dir(path);
            }
        }
    }
    let selected_path = version_path(&probe_root, &target.version, &target.sha256)?;
    let version_dir = selected_path
        .parent()
        .context("missing probe version directory")?;
    let probe_entry = probe_root.join(BINARY_NAME);
    let _cleanup = Cleanup {
        files: vec![
            probe_entry.clone(),
            probe_root.join(REGISTRATION),
            probe_root.join("update.lock"),
            selected_path.clone(),
            version_dir.join(VERSION_METADATA),
        ],
        directories: vec![
            version_dir.to_path_buf(),
            probe_root.join("versions"),
            probe_root.clone(),
            base.clone(),
        ],
    };
    let mut target_info = AgentInfo::local();
    target_info.version.clone_from(&target.version);
    target_info.build_id.clone_from(&target.build_id);
    target_info.target.clone_from(&target.target);
    let selected = install_bytes(
        &probe_root,
        &target_info,
        &BINARIES.read_bounded(&target.executable)?,
    )?;
    BINARIES.write_atomically(&probe_entry, &bytes)?;
    write_json(
        &probe_root.join(REGISTRATION),
        &Installation {
            schema_version: 1,
            executable: probe_entry.clone(),
            launcher_sha256: previous_sha256.into(),
            selected: selected.clone(),
        },
    )?;
    let mut command = Command::new(&probe_entry);
    command
        .env("LOCALAPPDATA", &base)
        .args(["remote-agent", "info", "--sha256"]);
    // CreateProcess itself can block on a malformed-image system dialog before
    // the bounded runner has a Child to poll/terminate. Suppress loader dialogs
    // for this thread only; never change another thread's process-wide mode.
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetThreadErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
        SetThreadErrorMode,
    };
    struct ErrorMode(u32);
    impl Drop for ErrorMode {
        fn drop(&mut self) {
            unsafe { SetThreadErrorMode(self.0, std::ptr::null_mut()) };
        }
    }
    let previous_mode = unsafe { GetThreadErrorMode() };
    ensure!(
        unsafe {
            SetThreadErrorMode(
                previous_mode
                    | SEM_FAILCRITICALERRORS
                    | SEM_NOGPFAULTERRORBOX
                    | SEM_NOOPENFILEERRORBOX,
                std::ptr::null_mut(),
            )
        } != 0,
        "update_probe_failed: cannot suppress executable-loader dialogs: {}",
        io::Error::last_os_error()
    );
    let _mode = ErrorMode(previous_mode);
    let Ok(output) = crate::bounded_process::output(
        &mut command,
        Duration::from_secs(15),
        MAX_METADATA as usize,
    ) else {
        return Ok(false);
    };
    let Ok(actual) = serde_json::from_slice::<AgentInfo>(&output.stdout) else {
        return Ok(false);
    };
    Ok(output.status.success()
        && actual.schema_version == 1
        && actual.product == "codex-usage-monit"
        && actual.version == target.version
        && actual.build_id == target.build_id
        && actual.target == target.target
        && actual.protocol_version == AgentInfo::local().protocol_version
        && actual.executable_sha256.as_deref() == Some(&selected.sha256))
}

#[cfg(windows)]
fn has_windows_pe_header(bytes: &[u8]) -> bool {
    if bytes.get(..2) != Some(b"MZ") {
        return false;
    }
    let Some(offset) = bytes.get(60..64) else {
        return false;
    };
    let offset = u32::from_le_bytes(offset.try_into().expect("four byte PE offset")) as usize;
    offset.checked_add(24).is_some_and(|end| {
        bytes
            .get(offset..end)
            .is_some_and(|header| header[..4] == *b"PE\0\0")
    })
}

/// Check a trusted Windows executable's compatibility with a prepared managed
/// version. This starts a copy of the explicitly supplied executable in an
/// isolated installation root; callers must establish trust before invoking
/// it. In particular, command discovery/doctor must not execute unknown files.
/// The live CLI registration, recorder and executable bytes are unchanged.
#[cfg(windows)]
pub fn verify_launcher_compatibility(
    executable: &Path,
    prepared_executable: &Path,
) -> Result<bool> {
    let prepared_executable = prepared_executable.canonicalize()?;
    let version_dir = prepared_executable
        .parent()
        .context("missing version directory")?;
    let root = version_dir
        .parent()
        .and_then(Path::parent)
        .context("missing installation root")?;
    STORE.validate_state_root(root)?;
    let target: InstalledVersion = read_json(&version_dir.join(VERSION_METADATA))?
        .context("update_version_missing: metadata is missing")?;
    ensure!(
        target.executable == prepared_executable,
        "update_path_invalid: prepared executable differs from its metadata"
    );
    validate_version(root, &target)?;
    let previous_sha256 =
        entry_hash(executable)?.context("update_entry_missing: launcher is missing")?;
    if previous_sha256 == target.sha256 {
        return Ok(true);
    }
    verify_compatible_launcher(root, executable, &previous_sha256, &target)
}

fn preflight_cli_replacement(
    plan: Option<&CliPlan>,
    target: &InstalledVersion,
    options: &ApplyOptions,
) -> Result<Option<fs::File>> {
    #[cfg(windows)]
    if let Some(plan) = plan {
        let actual = entry_hash(&plan.executable)?;
        if actual.is_some() && actual.as_deref() != Some(&plan.launcher_sha256) {
            let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "''"));
            let recovery = format!(
                "& {} update apply --scope node --install-dir {} --adopt{} --format json",
                quote(&target.executable),
                quote(
                    plan.executable
                        .parent()
                        .context("CLI entry has no parent")?
                ),
                if options.allow_dev_build {
                    " --allow-dev-build"
                } else {
                    ""
                },
            );
            let file = OpenOptions::new().write(true).open(&plan.executable).with_context(|| {
                format!(
                    "update_entry_busy: cannot prepare the old CLI for replacement; the recorder has not been changed by this attempt. Close processes using {} and run this prepared candidate from PowerShell: {recovery}",
                    plan.executable.display()
                )
            })?;
            ensure!(
                entry_hash(&plan.executable)? == actual,
                "update_entry_conflict: old CLI changed during replacement preflight"
            );
            return Ok(Some(file));
        }
    }
    #[cfg(not(windows))]
    let _ = (plan, target, options);
    Ok(None)
}

fn validate_cli_plan(root: &Path, plan: &CliPlan, target: &InstalledVersion) -> Result<()> {
    let actual = entry_hash(&plan.executable)?;
    ensure!(
        actual == plan.previous_sha256 || actual.as_deref() == Some(&plan.launcher_sha256),
        "update_entry_conflict: CLI entry changed after update preparation; existing file preserved"
    );
    let registration = read_installation(root)?;
    let desired = Installation {
        schema_version: 1,
        executable: plan.executable.clone(),
        launcher_sha256: plan.launcher_sha256.clone(),
        selected: target.clone(),
    };
    ensure!(
        registration == plan.previous_registration || registration.as_ref() == Some(&desired),
        "update_entry_conflict: CLI registration changed after update preparation"
    );
    Ok(())
}

fn activate_cli(root: &Path, plan: &CliPlan, target: &InstalledVersion) -> Result<()> {
    validate_cli_plan(root, plan, target)?;
    let directory = plan
        .executable
        .parent()
        .context("CLI entry has no parent")?;
    if !directory.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
        }
        #[cfg(windows)]
        crate::windows_private_directory::create_dir_all(directory)?;
    }
    validate_install_directory(directory)?;
    let actual = entry_hash(&plan.executable)?;
    if actual.as_deref() != Some(&plan.launcher_sha256) {
        ensure!(
            plan.previous_registration.is_none(),
            "update_entry_conflict: managed launcher disappeared"
        );
        if let Some(previous) = &plan.previous_sha256 {
            let backup_dir = root.join("adopted-cli");
            STORE.create_directory_beneath(root, &backup_dir)?;
            let backup = backup_dir.join(previous);
            let bytes = read_external_binary(&plan.executable)?;
            ensure!(
                digest(&bytes) == *previous,
                "update_entry_conflict: old CLI changed before backup"
            );
            if backup.exists() {
                ensure!(
                    digest(&BINARIES.read_bounded(&backup)?) == *previous,
                    "update_backup_conflict: retained adoption backup differs"
                );
            } else {
                BINARIES.write_atomically(&backup, &bytes)?;
            }
        }
        let bytes = BINARIES.read_bounded(&target.executable)?;
        ensure!(
            digest(&bytes) == plan.launcher_sha256,
            "update_checksum_mismatch: candidate changed before CLI publication"
        );
        publish_entry(&plan.executable, &bytes, plan.previous_sha256.as_deref())?;
    }
    let registration = Installation {
        schema_version: 1,
        executable: plan.executable.clone(),
        launcher_sha256: plan.launcher_sha256.clone(),
        selected: target.clone(),
    };
    write_json(&root.join(REGISTRATION), &registration)?;
    validate_cli_plan(root, plan, target)?;
    ensure!(
        entry_hash(&plan.executable)?.as_deref() == Some(&plan.launcher_sha256),
        "update_entry_conflict: published launcher changed"
    );
    validate_version(root, target)
}

fn publish_entry(path: &Path, bytes: &[u8], expected: Option<&str>) -> Result<()> {
    let directory = path.parent().context("CLI entry has no parent")?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("update_staging_failed: {e}"))?;
    let temporary = directory.join(format!(".codex-usage-monit-{}.tmp", digest(&random)));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        validate_install_directory(directory)?;
        ensure!(
            entry_hash(path)?.as_deref() == expected,
            "update_entry_conflict: CLI changed before atomic replacement"
        );
        crate::atomic_file::replace_file(&temporary, path).context("update_entry_busy: could not replace the CLI entry; on Windows close old CLI/TUI processes before the first --adopt migration")?;
        #[cfg(unix)]
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn resolve_path_entry(path: Option<&OsStr>) -> Option<PathBuf> {
    resolve_path_entry_with_extensions(path, env::var_os("PATHEXT").as_deref())
}

fn resolve_path_entry_with_extensions(
    path: Option<&OsStr>,
    extensions: Option<&OsStr>,
) -> Option<PathBuf> {
    #[cfg(windows)]
    let names = extensions
        .unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD"))
        .to_string_lossy()
        .split(';')
        .map(str::trim)
        .filter(|extension| {
            extension.starts_with('.')
                && extension.len() > 1
                && extension.len() <= 16
                && extension[1..]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(|extension| format!("codex-usage-monit{extension}"))
        .collect::<Vec<_>>();
    #[cfg(not(windows))]
    let names = {
        let _ = extensions;
        vec![BINARY_NAME.to_owned()]
    };
    env::split_paths(path?).find_map(|directory| {
        names.iter().find_map(|name| {
            let candidate = directory.join(name);
            let metadata = fs::metadata(&candidate).ok()?;
            if !metadata.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            std::path::absolute(candidate).ok()
        })
    })
}

fn resolve_in_path(path: Option<&OsStr>) -> Option<PathBuf> {
    resolve_path_entry(path)?.canonicalize().ok()
}

fn current_executable() -> Result<PathBuf> {
    env::current_exe()?
        .canonicalize()
        .context("update_executable_unavailable: could not resolve the running executable")
}

fn cli_report(executable: Option<PathBuf>, path: Option<&OsStr>, outcome: &str) -> CliUpdateReport {
    let resolved_executable = resolve_in_path(path);
    let shadowed = executable.as_ref().is_some_and(|entry| {
        resolved_executable
            .as_ref()
            .is_some_and(|resolved| entry != resolved)
    });
    let diagnostic = if shadowed {
        Some("This process's PATH resolves a different codex-usage-monit. Adjust PATH or invoke the managed CLI by its absolute path; interactive shell PATH may differ.".into())
    } else if executable.is_some() && resolved_executable.is_none() {
        Some("The managed CLI directory is not in this process's PATH. Add it to your shell PATH; no shell profiles were modified.".into())
    } else {
        None
    };
    CliUpdateReport {
        outcome: outcome.into(),
        executable,
        resolved_executable,
        shadowed,
        diagnostic,
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match STORE.read_bounded(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .context("update_metadata_invalid: invalid installation metadata")?,
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA,
        "update_metadata_invalid: metadata is too large"
    );
    STORE.write_atomically(path, &bytes)?;
    Ok(())
}

pub(crate) fn inspect() -> Result<UpdateStatus> {
    let root = installation_root()?;
    let executable = current_executable()?;
    let info = AgentInfo::local();
    let registration = if root.exists() {
        STORE.validate_state_root(&root)?;
        read_installation(&root)?
    } else {
        None
    };
    let source = if executable.starts_with(root.join("versions")) {
        "managed_version"
    } else if registration
        .as_ref()
        .is_some_and(|r| r.executable == executable)
    {
        "managed_cli"
    } else if package_manager_path(&executable) {
        "package_manager"
    } else {
        "unmanaged"
    };
    let mut versions = Vec::new();
    let versions_root = root.join("versions");
    if STORE.directory_exists_beneath(&root, &versions_root)? {
        for entry in fs::read_dir(&versions_root)?.take(1024) {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(version) =
                read_json::<InstalledVersion>(&entry.path().join(VERSION_METADATA))?
            {
                validate_version(&root, &version)?;
                versions.push(version);
            }
        }
        versions.sort_by(|a, b| a.executable.cmp(&b.executable));
    }
    let journal: Option<UpdateJournal> = read_json(&root.join(JOURNAL))?;
    Ok(UpdateStatus {
        schema_version: 1,
        root,
        executable,
        source: source.into(),
        version: info.version,
        build_id: info.build_id,
        cli: cli_report(
            registration.map(|r| r.executable),
            env::var_os("PATH").as_deref(),
            "inspected",
        ),
        versions,
        journal_phase: journal.as_ref().map(|j| j.phase.clone()),
        last_update: journal.map(|j| j.report),
    })
}

/// Remove only the registered command entry, retaining versions and user data.
/// A running Windows launcher returns false so the installer can report a
/// pending uninstall without discarding the ownership needed for a retry.
#[cfg(windows)]
pub(crate) fn unregister_cli(expected_executable: &Path) -> Result<bool> {
    let root = installation_root()?;
    if !root.exists() {
        return Ok(true);
    }
    STORE.validate_state_root(&root)?;
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    unregister_cli_at(&root, expected_executable)
}

#[cfg(windows)]
pub(crate) fn with_installation_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let root = installation_root()?;
    STORE.validate_state_root(&root)?;
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    let journal: Option<UpdateJournal> = read_json(&root.join(JOURNAL))?;
    ensure!(
        journal.is_none_or(|journal| journal.phase == "complete"),
        "update_pending: finish the retained application update before changing installation ownership"
    );
    operation()
}

#[cfg(not(windows))]
pub(crate) fn with_installation_lock<T>(_operation: impl FnOnce() -> Result<T>) -> Result<T> {
    bail!("installation_platform_unsupported: this installation lifecycle requires Windows")
}

#[cfg(not(windows))]
pub(crate) fn unregister_cli(_expected_executable: &Path) -> Result<bool> {
    bail!("installation_platform_unsupported: this installation lifecycle requires Windows")
}

#[cfg(not(windows))]
pub(crate) fn repair_cli() -> Result<PathBuf> {
    bail!("installation_platform_unsupported: this installation lifecycle requires Windows")
}

/// Return the verified version selected by the managed CLI, independently of
/// the calling executable. Repairing a recorder from an older bootstrap must
/// not silently replace a newer CLI selection with that bootstrap's build.
#[cfg(windows)]
pub(crate) fn selected_cli_executable() -> Result<PathBuf> {
    let root = installation_root()?;
    STORE.validate_state_root(&root)?;
    let _lock = STORE.open_lock(&root, LockMode::Shared, LockFilePolicy::Existing)?;
    crate::installation::ownership_preflight_at(&root)?;
    selected_cli_executable_at(&root)
}

#[cfg(not(windows))]
pub(crate) fn selected_cli_executable() -> Result<PathBuf> {
    bail!("installation_platform_unsupported: this installation lifecycle requires Windows")
}

#[cfg(windows)]
fn selected_cli_executable_at(root: &Path) -> Result<PathBuf> {
    let journal: Option<UpdateJournal> = read_json(&root.join(JOURNAL))?;
    ensure!(
        journal.is_none_or(|journal| journal.phase == "complete"),
        "update_pending: finish the retained application update before using its selected CLI"
    );
    let registration = read_installation(root)?
        .context("update_entry_unmanaged: install a managed CLI before repairing its recorder")?;
    ensure!(
        entry_hash(&registration.executable)?.as_deref() == Some(&registration.launcher_sha256),
        "update_entry_conflict: registered launcher is missing or changed; repair the owned CLI before repairing its recorder"
    );
    validate_version(root, &registration.selected)?;
    Ok(registration.selected.executable)
}

#[cfg(windows)]
fn unregister_cli_at(root: &Path, expected_executable: &Path) -> Result<bool> {
    let journal: Option<UpdateJournal> = read_json(&root.join(JOURNAL))?;
    ensure!(
        journal.is_none_or(|journal| journal.phase == "complete"),
        "update_pending: finish the retained application update before uninstalling its CLI"
    );
    let expected_executable = normalized_absolute(expected_executable)?;
    let Some(registration) = read_installation(root)? else {
        ensure!(
            entry_hash(&expected_executable)?.is_none(),
            "update_entry_unmanaged: no registration owns the existing CLI; file preserved"
        );
        return Ok(true);
    };
    ensure!(
        registration.executable == expected_executable,
        "update_entry_conflict: a different CLI entry is registered; existing files preserved"
    );
    if let Some(actual) = entry_hash(&registration.executable)? {
        ensure!(
            actual == registration.launcher_sha256,
            "update_entry_conflict: the registered CLI was changed outside the installer; file preserved"
        );
        match fs::remove_file(&registration.executable) {
            Ok(()) => {}
            Err(error) if matches!(error.raw_os_error(), Some(5 | 32)) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    fs::remove_file(root.join(REGISTRATION))?;
    Ok(true)
}

/// Recreate a missing owned launcher from its verified selected version. This
/// never replaces an existing file and does not mutate recorder registration.
#[cfg(windows)]
pub(crate) fn repair_cli() -> Result<PathBuf> {
    let root = installation_root()?;
    STORE.validate_state_root(&root)?;
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    repair_cli_at(&root)
}

#[cfg(windows)]
fn repair_cli_at(root: &Path) -> Result<PathBuf> {
    let journal: Option<UpdateJournal> = read_json(&root.join(JOURNAL))?;
    ensure!(
        journal.is_none_or(|journal| journal.phase == "complete"),
        "update_pending: finish the retained application update before repairing its CLI"
    );
    let mut registration = read_installation(root)?
        .context("update_entry_unmanaged: install a managed CLI before repairing it")?;
    validate_version(root, &registration.selected)?;
    if let Some(actual) = entry_hash(&registration.executable)? {
        ensure!(
            actual == registration.launcher_sha256,
            "update_entry_conflict: the registered CLI was changed outside the installer; file preserved"
        );
        return Ok(registration.executable);
    }
    let directory = registration
        .executable
        .parent()
        .context("CLI entry has no parent")?;
    if !directory.exists() {
        crate::windows_private_directory::create_dir_all(directory)?;
    }
    validate_install_directory(directory)?;
    let bytes = BINARIES.read_bounded(&registration.selected.executable)?;
    ensure!(
        digest(&bytes) == registration.selected.sha256,
        "update_checksum_mismatch: selected version changed during repair"
    );
    // Commit the new launcher's identity before publication. If publication
    // fails the entry remains absent, so the same repair safely resumes.
    registration
        .launcher_sha256
        .clone_from(&registration.selected.sha256);
    write_json(&root.join(REGISTRATION), &registration)?;
    publish_entry(&registration.executable, &bytes, None)?;
    Ok(registration.executable)
}

pub(crate) fn prune(options: PruneOptions) -> Result<PruneReport> {
    validate_prune_options(&options)?;
    let root = installation_root()?;
    let mut legacy_roots = Vec::new();
    for base in [
        env_path(if cfg!(windows) { "USERPROFILE" } else { "HOME" }),
        env::current_dir().ok(),
    ]
    .into_iter()
    .flatten()
    {
        let legacy = base.join(".codex-usage-monit-agents");
        if legacy.exists() && !legacy_roots.contains(&legacy) {
            legacy_roots.push(legacy);
        }
    }
    if !root.exists() {
        return Ok(PruneReport {
            schema_version: 1,
            applied: false,
            versions: Vec::new(),
            legacy_roots,
        });
    }
    STORE.validate_state_root(&root)?;
    // Lock order matches activation: application update, then service mutation.
    // No direct service install/uninstall may change a reference during deletion.
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    crate::service::with_update_references(options.apply, |references| {
        let references = references.to_vec();
        #[cfg(windows)]
        let references = {
            let mut references = references;
            references.extend(crate::installation::referenced_executables()?);
            references
        };
        let mut report = prune_at(&root, &options, &current_executable()?, &references)?;
        report.legacy_roots = legacy_roots;
        Ok(report)
    })
}

fn validate_prune_options(options: &PruneOptions) -> Result<()> {
    ensure!(
        !options.apply || (!options.versions.is_empty() && options.acknowledge_unreferenced),
        "update_prune_confirmation_required: deletion needs explicit version IDs and --acknowledge-unreferenced; verify that other centers and manual processes no longer use them"
    );
    for id in &options.versions {
        let mut components = Path::new(id).components();
        ensure!(
            !id.contains(['/', '\\'])
                && matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none(),
            "update_prune_invalid: specify version directory IDs, not paths"
        );
    }
    Ok(())
}

fn prune_at(
    root: &Path,
    options: &PruneOptions,
    current_executable: &Path,
    service_references: &[PathBuf],
) -> Result<PruneReport> {
    validate_prune_options(options)?;
    let registration = read_installation(root)?;
    let journal: Option<UpdateJournal> = read_json(&root.join(JOURNAL))?;
    let mut protected = vec![current_executable.to_path_buf()];
    protected.extend_from_slice(service_references);
    if let Some(registration) = registration {
        protected.push(registration.executable);
        protected.push(registration.selected.executable);
    }
    if let Some(journal) = journal.filter(|j| j.phase != "complete") {
        protected.push(journal.target.executable);
    }
    let versions_root = root.join("versions");
    let mut versions = Vec::new();
    if STORE.directory_exists_beneath(root, &versions_root)? {
        for entry in fs::read_dir(&versions_root)?.take(1024) {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            let explicit = options.versions.contains(&id);
            let mut item = PruneVersion {
                id,
                executable: None,
                action: "unknown".into(),
                reason: "Not a verified managed version; retained.".into(),
            };
            let candidate = (|| -> Result<(InstalledVersion, bool)> {
                ensure!(entry.file_type()?.is_dir(), "not a directory");
                STORE.validate_private_directory(&entry.path())?;
                let installed =
                    read_json::<InstalledVersion>(&entry.path().join(VERSION_METADATA))?
                        .context("missing managed metadata")?;
                ensure!(
                    installed.executable.parent() == Some(entry.path().as_path()),
                    "metadata points to another version"
                );
                validate_version(root, &installed)?;
                let names = fs::read_dir(entry.path())?
                    .map(|e| e.map(|e| e.file_name()))
                    .collect::<io::Result<Vec<_>>>()?;
                #[cfg(windows)]
                let has_components = crate::service::validate_windows_version_components(
                    &entry.path(),
                    &installed.build_id,
                )?;
                #[cfg(not(windows))]
                let has_components = false;
                ensure!(
                    names.len() == (if has_components { 4 } else { 2 })
                        && names.iter().all(|name| name == OsStr::new(BINARY_NAME)
                            || name == OsStr::new(VERSION_METADATA)
                            || (has_components
                                && (name == OsStr::new("recorder-host.exe")
                                    || name == OsStr::new("windows-components.json")))),
                    "version contains unknown files"
                );
                Ok((installed, has_components))
            })();
            if let Ok((installed, has_components)) = candidate {
                item.executable = Some(installed.executable.clone());
                if protected.iter().any(|path| {
                    path == &installed.executable
                        || (has_components && path.parent() == installed.executable.parent())
                        || path.canonicalize().ok().is_some_and(|path| {
                            let executable = installed.executable.canonicalize().ok();
                            Some(&path) == executable.as_ref()
                                || (has_components
                                    && executable.as_ref().is_some_and(|executable| {
                                        path.parent() == executable.parent()
                                    }))
                        })
                }) {
                    item.action = "kept".into();
                    item.reason = "Referenced by the running updater, selected CLI, registered recorder or pending update.".into();
                } else if !options.versions.is_empty() && !explicit {
                    item.action = "kept".into();
                    item.reason = "Not explicitly selected.".into();
                } else if !options.apply {
                    item.action = "candidate".into();
                    item.reason = "No local managed reference; other centers/manual processes cannot be discovered. Explicit selection and acknowledgement are required to delete.".into();
                } else {
                    let removal = (|| -> Result<()> {
                        // Delete only the verified files. Never recursively
                        // remove a directory, even after an unexpected failure.
                        if has_components {
                            crate::service::remove_windows_version_components(
                                &entry.path(),
                                &installed.build_id,
                            )?;
                        }
                        fs::remove_file(&installed.executable)?;
                        fs::remove_file(entry.path().join(VERSION_METADATA))?;
                        fs::remove_dir(entry.path())?;
                        #[cfg(unix)]
                        File::open(&versions_root)?.sync_all()?;
                        Ok(())
                    })();
                    match removal {
                        Ok(()) => {
                            item.action = "removed".into();
                            item.reason =
                                "Explicitly selected unreferenced managed version removed.".into();
                        }
                        Err(error) => {
                            item.action = "failed".into();
                            item.reason = format!("{error:#}; remaining files were retained");
                        }
                    }
                }
            }
            versions.push(item);
        }
    }
    for id in &options.versions {
        if !versions.iter().any(|v| &v.id == id) {
            versions.push(PruneVersion {
                id: id.clone(),
                executable: None,
                action: "unknown".into(),
                reason: "Version was not found in managed storage.".into(),
            });
        }
    }
    versions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(PruneReport {
        schema_version: 1,
        applied: options.apply,
        versions,
        legacy_roots: Vec::new(),
    })
}

fn proxy_target(root: &Path, executable: &Path) -> Result<Option<PathBuf>> {
    // The target must never redispatch itself, even when the launcher and target
    // were initially copied from exactly the same executable.
    if executable.starts_with(root.join("versions")) || !root.exists() {
        return Ok(None);
    }
    STORE.validate_state_root(root)?;
    let Some(registration) = read_installation(root)? else {
        return Ok(None);
    };
    if registration.executable != executable {
        return Ok(None);
    }
    ensure!(
        entry_hash(executable)?.as_deref() == Some(&registration.launcher_sha256),
        "update_launcher_changed: registered launcher was externally replaced"
    );
    validate_version(root, &registration.selected)?;
    Ok(Some(registration.selected.executable))
}

/// Called before argument parsing. Return None for an ordinary executable.
/// Unix replaces the launcher process; Windows keeps its stable exe and waits.
pub fn maybe_run_proxy() -> Result<Option<i32>> {
    let executable = current_executable()?;
    #[cfg(windows)]
    if crate::windows_scm::is_machine_executable(&executable)? {
        return Ok(None);
    }
    let root = match installation_root() {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    let Some(target) = proxy_target(&root, &executable)? else {
        return Ok(None);
    };
    let mut command = Command::new(target);
    command.args(env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("could not launch selected application version")
    }
    #[cfg(windows)]
    {
        run_windows_proxy(&mut command).map(Some)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(Some(
            command
                .status()
                .context("could not launch selected application version")?
                .code()
                .unwrap_or(1),
        ))
    }
}

#[cfg(windows)]
unsafe extern "system" fn proxy_console_control(event: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    // The child shares the console and receives these events directly. The
    // launcher must keep waiting instead of abandoning an interactive child.
    i32::from(matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT))
}

#[cfg(windows)]
fn run_windows_proxy(command: &mut Command) -> Result<i32> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    struct Handler(bool);
    impl Drop for Handler {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: the static handler is still valid for this process.
                unsafe {
                    SetConsoleCtrlHandler(Some(proxy_console_control), 0);
                }
            }
        }
    }
    // Unlike the NULL-handler ignore flag, this per-process function is not
    // inherited by children. Their normal console cancellation stays intact.
    let installed = unsafe { SetConsoleCtrlHandler(Some(proxy_console_control), 1) } != 0;
    if !installed {
        let error = io::Error::last_os_error();
        // SSH/piped commands may have no console; there is no event to handle.
        if error.raw_os_error() != Some(ERROR_INVALID_HANDLE as i32) {
            return Err(error.into());
        }
    }
    let _handler = Handler(installed);
    Ok(command
        .status()
        .context("could not launch selected application version")?
        .code()
        .unwrap_or(1))
}

#[cfg(test)]
mod tests;
