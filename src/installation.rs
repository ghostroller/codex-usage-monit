//! Windows user installation metadata and PATH ownership. The executable
//! activation protocol remains owned by `update`; this receipt is deliberately
//! separate from the stable launcher's strict installation.json protocol.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::private_state_store::{LockFilePolicy, LockMode, PrivateStoreLayout};
use crate::update::{ApplyOptions, UpdateReport, UpdateScope, UpdateStatus};

const RECEIPT: &str = "install-receipt.json";
const OWNER: &str = "standalone";
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const STORE: PrivateStoreLayout = PrivateStoreLayout {
    store_name: "user installation receipt",
    data_file_name: RECEIPT,
    data_path_name: "installation receipt",
    data_subject: "installation receipt",
    lock_file_name: "install-lifecycle.lock",
    lock_subject: "installation lifecycle lock",
    temporary_subject: "installation receipt staging file",
    // A pending write holds two raw registry byte arrays. JSON's numeric byte
    // representation can be much larger than the original UTF-16 PATH.
    maximum_file_bytes: 4 * 1024 * 1024,
};

pub(crate) struct InstallOptions {
    pub version: String,
    pub bundle: Option<PathBuf>,
    /// Only for a bootstrap which already verified the candidate, or an
    /// explicitly invoked local executable. Never implies release trust.
    pub current_binary: bool,
    pub modify_path: bool,
    pub adopt: bool,
    pub allow_dev_build: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstallReceipt {
    pub schema_version: u32,
    pub installer_protocol: u32,
    pub owner: String,
    pub channel: String,
    pub user_sid: String,
    pub root: PathBuf,
    pub executable: PathBuf,
    pub state: String,
    pub path_requested: bool,
    /// Exact UTF-16 spelling actually inserted by this installer. A preexisting
    /// equivalent entry is never claimed, even when --add-to-path was requested.
    path_entry: Option<Vec<u16>>,
    #[serde(default)]
    path_empty_value_before: Option<RegistryValue>,
    pub task_identity: Option<String>,
    #[serde(default)]
    pub pending_recorder_executable: Option<PathBuf>,
    /// Also a GC reference while the user uninstall entry exists.
    #[serde(default)]
    pub uninstall_executable: Option<PathBuf>,
    #[serde(default)]
    pub display_version: String,
    pending_path: Option<PathChange>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PathReport {
    pub action: String,
    pub persistent_registered: bool,
    pub process_registered: bool,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallReport {
    pub schema_version: u32,
    pub outcome: String,
    pub receipt: InstallReceipt,
    pub path: PathReport,
    pub update: Option<UpdateReport>,
}

pub(crate) struct RecorderRegistrationResult<T> {
    pub operation: Result<T>,
    pub receipt: InstallReceipt,
    pub recovery_error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UninstallReport {
    pub schema_version: u32,
    pub outcome: String,
    pub executable: Option<PathBuf>,
    pub path: Option<PathReport>,
    pub data_preserved: bool,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DoctorReport {
    pub schema_version: u32,
    pub root: PathBuf,
    pub receipt: Option<InstallReceipt>,
    pub path: Option<PathReport>,
    pub update: UpdateStatus,
    pub diagnostics: Vec<String>,
}

/// Kept raw so other PATH entries, including unpaired UTF-16 and expansion
/// references, survive round trips. Unsupported registry types fail closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegistryValue {
    kind: u32,
    bytes: Vec<u8>,
}

impl RegistryValue {
    fn units(&self) -> Result<Vec<u16>> {
        ensure!(
            matches!(self.kind, REG_SZ | REG_EXPAND_SZ) && self.bytes.len().is_multiple_of(2),
            "install_path_invalid: user PATH must be a UTF-16 REG_SZ or REG_EXPAND_SZ"
        );
        let mut units: Vec<u16> = self
            .bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        if units.last() == Some(&0) {
            units.pop();
        }
        ensure!(
            !units.contains(&0),
            "install_path_invalid: user PATH contains embedded NULs"
        );
        Ok(units)
    }

    fn from_units(kind: u32, units: &[u16]) -> Self {
        Self {
            kind,
            bytes: units
                .iter()
                .copied()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PathChange {
    before: Option<RegistryValue>,
    after: Option<RegistryValue>,
    owned_after: Option<Vec<u16>>,
    #[serde(default)]
    empty_value_after: Option<RegistryValue>,
}

trait UserPath {
    fn read(&self) -> Result<Option<RegistryValue>>;
    /// Re-read immediately before writing. Windows does not provide an atomic
    /// registry CAS: arbitrary external editors cannot be serialized by our
    /// lifecycle lock. A detected concurrent edit is never overwritten.
    fn compare_write(
        &mut self,
        before: &Option<RegistryValue>,
        after: &Option<RegistryValue>,
    ) -> Result<bool>;
    fn broadcast(&self) -> Result<()>;
}

fn normalized_entry(units: &[u16]) -> Option<String> {
    let raw = String::from_utf16(units).ok()?;
    let raw = raw.trim();
    let raw = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    let mut normalized = path_for_shell(raw).replace('/', "\\").to_lowercase();
    while normalized.len() > 3 && normalized.ends_with('\\') {
        normalized.pop();
    }
    Some(normalized)
}

// fs::canonicalize returns a verbatim path on Windows. Command lookup should
// register the normal drive/UNC spelling and match a preexisting normal entry.
fn path_for_shell(value: &str) -> String {
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(value).into()
    }
}

fn includes_path(value: &Option<RegistryValue>, entry: &[u16]) -> Result<bool> {
    let desired = normalized_entry(entry);
    Ok(value
        .as_ref()
        .map(RegistryValue::units)
        .transpose()?
        .unwrap_or_default()
        .split(|c| *c == u16::from(b';'))
        .any(|part| normalized_entry(part) == desired))
}

fn add_path(before: Option<RegistryValue>, entry: &[u16]) -> Result<Option<PathChange>> {
    if includes_path(&before, entry)? {
        return Ok(None);
    }
    let mut units = before
        .as_ref()
        .map(RegistryValue::units)
        .transpose()?
        .unwrap_or_default();
    // Append one complete segment, preserving existing separators/empty entries.
    if !units.is_empty() {
        units.push(u16::from(b';'));
    }
    units.extend_from_slice(entry);
    Ok(Some(PathChange {
        after: Some(RegistryValue::from_units(
            before.as_ref().map_or(REG_EXPAND_SZ, |v| v.kind),
            &units,
        )),
        empty_value_after: before
            .as_ref()
            .filter(|v| v.units().is_ok_and(|u| u.is_empty()))
            .cloned(),
        before,
        owned_after: Some(entry.to_vec()),
    }))
}

fn remove_owned_path(before: Option<RegistryValue>, owned: &[u16]) -> Result<Option<PathChange>> {
    let Some(value) = &before else {
        return Ok(None);
    };
    let units = value.units()?;
    let mut segments: Vec<&[u16]> = units.split(|c| *c == u16::from(b';')).collect();
    let matches: Vec<usize> = segments
        .iter()
        .enumerate()
        .filter_map(|(i, s)| (*s == owned).then_some(i))
        .collect();
    ensure!(
        matches.len() <= 1,
        "install_path_ownership_ambiguous: duplicate owned PATH entries; no entries removed"
    );
    let Some(index) = matches.first() else {
        return Ok(None);
    };
    segments.remove(*index);
    let units = segments.join(&u16::from(b';'));
    Ok(Some(PathChange {
        after: if segments.is_empty() {
            None
        } else {
            Some(RegistryValue::from_units(value.kind, &units))
        },
        before,
        owned_after: None,
        empty_value_after: None,
    }))
}

fn directory_entry(root: &Path) -> Result<Vec<u16>> {
    let directory = root.join("bin");
    let value = directory
        .to_str()
        .context("install_path_invalid: installation directory is not Unicode")?;
    ensure!(
        !value.contains([';', '\0', '\r', '\n', '"']),
        "install_path_invalid: directory cannot be a PATH segment"
    );
    Ok(path_for_shell(value).encode_utf16().collect())
}

fn process_includes(entry: &[u16]) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|p| {
        normalized_entry(&p.to_string_lossy().encode_utf16().collect::<Vec<_>>())
            == normalized_entry(entry)
    })
}

fn path_report(backend: &impl UserPath, root: &Path, action: &str) -> Result<PathReport> {
    let entry = directory_entry(root)?;
    let persistent_registered = includes_path(&backend.read()?, &entry)?;
    let process_registered = process_includes(&entry);
    Ok(PathReport {
        action: action.into(),
        persistent_registered,
        process_registered,
        diagnostic: persistent_registered.then(|| "Persistent user PATH is registered. Existing terminals may need to be reopened; use Get-Command codex-usage-monit -All and where.exe codex-usage-monit to check command precedence.".into()),
    })
}

fn read_receipt(root: &Path, sid: &str) -> Result<Option<InstallReceipt>> {
    let path = root.join(RECEIPT);
    let bytes = match STORE.read_bounded(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let receipt: InstallReceipt =
        serde_json::from_slice(&bytes).context("install_receipt_invalid")?;
    ensure!(
        receipt.schema_version == 1
            && receipt.installer_protocol == 1
            && receipt.root == root
            && receipt.user_sid == sid
            && receipt.executable == root.join("bin/codex-usage-monit.exe")
            && matches!(
                receipt.state.as_str(),
                "installed" | "uninstall_pending" | "uninstalled"
            ),
        "install_receipt_invalid: unsupported protocol, location or Windows identity"
    );
    if let Some(owned) = &receipt.path_entry {
        ensure!(
            *owned == directory_entry(root)?,
            "install_receipt_invalid: PATH ownership differs from install root"
        );
    }
    if let Some(empty) = &receipt.path_empty_value_before {
        ensure!(
            empty.units()?.is_empty() && receipt.path_entry.is_some(),
            "install_receipt_invalid: invalid empty PATH ownership"
        );
    }
    if let Some(change) = &receipt.pending_path {
        if let Some(owned) = &change.owned_after {
            ensure!(
                *owned == directory_entry(root)?,
                "install_receipt_invalid: pending PATH ownership differs"
            );
        }
        for value in [&change.before, &change.after].into_iter().flatten() {
            value.units()?;
        }
        if let Some(empty) = &change.empty_value_after {
            ensure!(
                empty.units()?.is_empty() && change.owned_after.is_some(),
                "install_receipt_invalid: invalid pending empty PATH ownership"
            );
        }
    }
    for executable in receipt
        .uninstall_executable
        .iter()
        .chain(receipt.pending_recorder_executable.iter())
    {
        ensure!(
            executable.starts_with(root.join("versions"))
                && executable.file_name() == Some(std::ffi::OsStr::new("codex-usage-monit.exe"))
                && !executable
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
            "install_receipt_invalid: uninstall executable is outside version storage"
        );
    }
    Ok(Some(receipt))
}

fn write_receipt(receipt: &InstallReceipt) -> Result<()> {
    STORE.write_atomically(
        &receipt.root.join(RECEIPT),
        &serde_json::to_vec_pretty(receipt)?,
    )?;
    Ok(())
}

fn ensure_owner(receipt: &InstallReceipt) -> Result<()> {
    ensure!(
        receipt.owner == OWNER,
        "install_external_owner: this installation is owned by {}; use its original installer",
        receipt.owner
    );
    Ok(())
}

/// Called by the updater before activation. Never takes the lifecycle lock:
/// install already holds it while invoking the updater in a child process.
#[cfg(windows)]
pub(crate) fn ownership_preflight() -> Result<()> {
    require_windows()?;
    let root = crate::update::installation_root()?;
    ownership_preflight_at(&root)
}

/// The updater repeats this check after acquiring its own update lock, closing
/// the window between discovery and an uninstall starting to unregister things.
#[cfg(windows)]
pub(crate) fn ownership_preflight_at(root: &Path) -> Result<()> {
    require_windows()?;
    if let Some(receipt) = read_receipt(root, &user_sid()?)? {
        ensure_owner(&receipt)?;
        ensure!(
            receipt.state != "uninstall_pending",
            "install_uninstall_pending: finish uninstall or explicitly run install before updating"
        );
        ensure!(
            receipt.pending_recorder_executable.is_none(),
            "install_recorder_pending: run install repair to finish recorder registration before updating"
        );
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn referenced_executables() -> Result<Vec<PathBuf>> {
    let root = crate::update::installation_root()?;
    Ok(read_receipt(&root, &user_sid()?)?
        .map(|r| {
            r.uninstall_executable
                .into_iter()
                .chain(r.pending_recorder_executable)
                .collect()
        })
        .unwrap_or_default())
}

fn recover_path(receipt: &mut InstallReceipt, backend: &mut impl UserPath) -> Result<()> {
    let Some(change) = receipt.pending_path.clone() else {
        return Ok(());
    };
    let actual = backend.read()?;
    if actual != change.after {
        ensure!(
            actual == change.before && backend.compare_write(&change.before, &change.after)?,
            "install_path_conflict: PATH changed during interrupted registration; leaving current PATH unchanged"
        );
    }
    receipt.path_entry = change.owned_after;
    receipt.path_empty_value_before = change.empty_value_after;
    receipt.pending_path = None;
    write_receipt(receipt)?;
    backend.broadcast()?;
    Ok(())
}

fn change_path(
    receipt: &mut InstallReceipt,
    backend: &mut impl UserPath,
    add: bool,
) -> Result<PathReport> {
    recover_path(receipt, backend)?;
    let entry = directory_entry(&receipt.root)?;
    for _ in 0..8 {
        let before = backend.read()?;
        let change = if add {
            add_path(before, &entry)?
        } else if let Some(owned) = &receipt.path_entry {
            let mut change = remove_owned_path(before, owned)?;
            if let Some(change) = change.as_mut().filter(|c| c.after.is_none()) {
                change.after = receipt.path_empty_value_before.clone();
            }
            change
        } else {
            None
        };
        let Some(change) = change else {
            if !add {
                receipt.path_entry = None;
                receipt.path_empty_value_before = None;
                write_receipt(receipt)?;
            }
            return path_report(backend, &receipt.root, "unchanged");
        };
        receipt.pending_path = Some(change.clone());
        write_receipt(receipt)?;
        if !backend.compare_write(&change.before, &change.after)? {
            receipt.pending_path = None;
            write_receipt(receipt)?;
            continue;
        }
        receipt.path_entry = change.owned_after;
        receipt.path_empty_value_before = change.empty_value_after;
        receipt.pending_path = None;
        write_receipt(receipt)?;
        backend.broadcast()?;
        return path_report(
            backend,
            &receipt.root,
            if add { "added" } else { "removed" },
        );
    }
    bail!("install_path_conflict: user PATH changed repeatedly; retry installation repair")
}

fn require_windows() -> Result<()> {
    ensure!(
        cfg!(windows),
        "install_platform_unsupported: user installation lifecycle currently requires Windows"
    );
    Ok(())
}

fn user_sid() -> Result<String> {
    #[cfg(windows)]
    {
        crate::service::windows_current_user_sid()
    }
    #[cfg(not(windows))]
    {
        bail!("install_platform_unsupported: Windows user identity is unavailable")
    }
}

pub(crate) fn install(options: InstallOptions) -> Result<InstallReport> {
    require_windows()?;
    ensure!(
        !options.current_binary || options.bundle.is_none(),
        "install_options_invalid: current-binary cannot be combined with bundle"
    );
    let root = crate::update::installation_root()?;
    STORE.create_directory_beneath(&root, &root)?;
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    let sid = user_sid()?;
    let mut previous = read_receipt(&root, &sid)?;
    if let Some(receipt) = &previous {
        ensure_owner(receipt)?;
    }
    if let Some(receipt) = previous.as_mut() {
        recover_recorder(receipt)?;
    }
    let mut backend = native::NativePath::open()?;
    if options.modify_path {
        includes_path(&backend.read()?, &directory_entry(&root)?)?;
    }
    if let Some(receipt) = previous.as_mut().filter(|r| r.state == "uninstall_pending") {
        // This is an explicit reinstall while holding the lifecycle lock. An
        // ordinary update may not make this choice on the user's behalf.
        recover_path(receipt, &mut backend)?;
        crate::update::with_installation_lock(|| {
            receipt.state = "installed".into();
            write_receipt(receipt)
        })?;
    }
    // The explicit user bin prevents discovery of an unrelated machine or
    // package-manager command from redirecting this installation.
    let apply = ApplyOptions {
        scope: UpdateScope::Node,
        install_dir: Some(root.join("bin")),
        adopt: options.adopt,
        allow_dev_build: options.allow_dev_build,
    };
    let update = if options.current_binary {
        crate::update::apply(apply)?
    } else {
        crate::remote_agent_manager::update_local(
            &options.version,
            options.bundle.as_deref(),
            &apply,
        )?
    };
    ensure!(
        update.outcome == "complete",
        "install_activation_incomplete: {}; PATH has not been registered",
        update.outcome
    );
    ensure!(
        update.cli.executable.as_deref() == Some(root.join("bin/codex-usage-monit.exe").as_path()),
        "install_activation_invalid: updater selected another command entry"
    );
    let mut receipt = previous.unwrap_or_else(|| InstallReceipt {
        schema_version: 1,
        installer_protocol: 1,
        owner: OWNER.into(),
        channel: if options.bundle.is_some() {
            "bundle"
        } else if options.current_binary {
            "local"
        } else {
            "release"
        }
        .into(),
        user_sid: sid,
        executable: root.join("bin/codex-usage-monit.exe"),
        root,
        state: "installed".into(),
        path_requested: options.modify_path,
        path_entry: None,
        path_empty_value_before: None,
        task_identity: None,
        pending_recorder_executable: None,
        uninstall_executable: None,
        display_version: String::new(),
        pending_path: None,
    });
    receipt.state = "installed".into();
    receipt.path_requested = options.modify_path;
    receipt.uninstall_executable = Some(update.executable.clone());
    receipt.display_version = update.version.clone();
    write_receipt(&receipt)?;
    // --no-modify-path never removes an existing registration on reinstall.
    let path = if options.modify_path {
        change_path(&mut receipt, &mut backend, true)?
    } else {
        path_report(&backend, &receipt.root, "not_requested")?
    };
    native::register_uninstall(&receipt)?;
    Ok(InstallReport {
        schema_version: 1,
        outcome: "complete".into(),
        receipt,
        path,
        update: Some(update),
    })
}

pub(crate) fn repair() -> Result<InstallReport> {
    require_windows()?;
    let root = crate::update::installation_root()?;
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    let mut receipt =
        read_receipt(&root, &user_sid()?)?.context("install_receipt_missing: run install first")?;
    ensure_owner(&receipt)?;
    ensure!(
        receipt.state == "installed",
        "install_uninstall_pending: finish uninstall or explicitly install again"
    );
    recover_recorder(&mut receipt)?;
    let mut backend = native::NativePath::open()?;
    recover_path(&mut receipt, &mut backend)?;
    let executable = crate::update::repair_cli()?;
    ensure!(
        executable == receipt.executable,
        "install_entry_conflict: another entry is registered"
    );
    let path = if receipt.path_requested {
        change_path(&mut receipt, &mut backend, true)?
    } else {
        path_report(&backend, &root, "not_requested")?
    };
    native::register_uninstall(&receipt)?;
    Ok(InstallReport {
        schema_version: 1,
        outcome: "complete".into(),
        receipt,
        path,
        update: None,
    })
}

/// The CLI supplies the existing trusted service-uninstall path. It is called
/// only for a task that this install explicitly recorded as its own.
pub(crate) fn uninstall(unregister: impl FnOnce(&str) -> Result<()>) -> Result<UninstallReport> {
    require_windows()?;
    let root = crate::update::installation_root()?;
    if !root.exists() {
        return Ok(UninstallReport {
            schema_version: 1,
            outcome: "not_installed".into(),
            executable: None,
            path: None,
            data_preserved: true,
            diagnostic: None,
        });
    }
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    let Some(mut receipt) = read_receipt(&root, &user_sid()?)? else {
        bail!(
            "install_receipt_missing: refusing to unregister an installation without ownership evidence"
        )
    };
    ensure_owner(&receipt)?;
    recover_recorder(&mut receipt)?;
    let mut backend = native::NativePath::open()?;
    recover_path(&mut receipt, &mut backend)?;
    crate::update::with_installation_lock(|| {
        receipt.state = "uninstall_pending".into();
        write_receipt(&receipt)
    })?;
    let path = change_path(&mut receipt, &mut backend, false)?;
    unregister_owned_task(&mut receipt, unregister)?;
    let removed = crate::update::unregister_cli(&receipt.executable)?;
    if removed {
        native::unregister_uninstall(&receipt)?;
        receipt.state = "uninstalled".into();
        receipt.uninstall_executable = None;
        write_receipt(&receipt)?;
    }
    Ok(UninstallReport {
        schema_version: 1, outcome: if removed { "complete" } else { "pending" }.into(),
        executable: Some(receipt.executable), path: Some(path), data_preserved: true,
        diagnostic: (!removed).then(|| "The command is still in use. PATH and owned background registration were removed; close existing sessions and rerun uninstall from the selected executable shown by doctor. Versions, configuration and history are retained.".into()),
    })
}

fn unregister_owned_task(
    receipt: &mut InstallReceipt,
    unregister: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    if let Some(identity) = &receipt.task_identity {
        unregister(identity)?;
        receipt.task_identity = None;
        write_receipt(receipt)?;
    }
    Ok(())
}

pub(crate) fn with_recorder_registration<T>(
    expected: &Path,
    callback: impl FnOnce() -> Result<T>,
) -> Result<RecorderRegistrationResult<T>> {
    require_windows()?;
    let root = crate::update::installation_root()?;
    let _lock = STORE.open_lock(&root, LockMode::Exclusive, LockFilePolicy::Create)?;
    let mut receipt = read_receipt(&root, &user_sid()?)?.context("install_receipt_missing")?;
    ensure_owner(&receipt)?;
    ensure!(receipt.state == "installed", "install_not_active");
    recover_recorder(&mut receipt)?;
    let expected = expected
        .canonicalize()
        .context("install_recorder_invalid: expected executable is missing")?;
    ensure!(
        expected.starts_with(root.join("versions"))
            && expected.file_name() == Some(std::ffi::OsStr::new("codex-usage-monit.exe")),
        "install_recorder_invalid: expected an immutable installed executable"
    );
    crate::update::with_installation_lock(|| {
        receipt.pending_recorder_executable = Some(expected);
        write_receipt(&receipt)
    })?;
    let operation = callback();
    let recovery_error = recover_recorder(&mut receipt).err().map(|e| e.to_string());
    Ok(RecorderRegistrationResult {
        operation,
        receipt,
        recovery_error,
    })
}

fn recover_recorder(receipt: &mut InstallReceipt) -> Result<()> {
    recover_recorder_using(receipt, crate::service::trusted_registration_identity)
}

fn recover_recorder_using(
    receipt: &mut InstallReceipt,
    inspect: impl FnOnce(&Path) -> Result<Option<String>>,
) -> Result<()> {
    let Some(expected) = receipt.pending_recorder_executable.as_deref() else {
        return Ok(());
    };
    // The service layer distinguishes genuine absence from an untrusted or
    // changed definition. Only absence or full identity proof clears pending.
    let identity = inspect(expected)?;
    let mut recovered = receipt.clone();
    recovered.task_identity = identity;
    recovered.pending_recorder_executable = None;
    write_receipt(&recovered)?;
    *receipt = recovered;
    Ok(())
}

pub(crate) fn doctor() -> Result<DoctorReport> {
    require_windows()?;
    let root = crate::update::installation_root()?;
    let update = crate::update::inspect()?;
    let mut diagnostics = Vec::new();
    let receipt = match read_receipt(&root, &user_sid()?) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(error.to_string());
            None
        }
    };
    let path = match native::NativePath::open()
        .and_then(|backend| path_report(&backend, &root, "inspected"))
    {
        Ok(value) => Some(value),
        Err(error) => {
            diagnostics.push(error.to_string());
            None
        }
    };
    if receipt.as_ref().is_some_and(|r| r.pending_path.is_some()) {
        diagnostics.push("Interrupted PATH registration: run install repair.".into());
    }
    if receipt
        .as_ref()
        .is_some_and(|r| r.pending_recorder_executable.is_some())
    {
        diagnostics.push("Interrupted recorder registration: run install repair; changed or untrusted task definitions will not be claimed.".into());
    }
    if receipt
        .as_ref()
        .is_some_and(|r| r.state == "uninstall_pending")
    {
        diagnostics.push("Uninstall is pending; close existing sessions and run uninstall again from a version executable.".into());
    }
    if update.cli.shadowed {
        diagnostics
            .push("Another command precedes the managed executable in this process PATH.".into());
    }
    diagnostics.push("PowerShell aliases/functions belong to the calling shell; inspect Get-Command codex-usage-monit -All and where.exe codex-usage-monit there.".into());
    Ok(DoctorReport {
        schema_version: 1,
        root,
        receipt,
        path,
        update,
        diagnostics,
    })
}

#[cfg(windows)]
mod native;

#[cfg(not(windows))]
mod native {
    use super::*;
    pub(super) struct NativePath;
    impl NativePath {
        pub(super) fn open() -> Result<Self> {
            bail!("Windows user PATH is unavailable")
        }
    }
    pub(super) fn register_uninstall(_: &InstallReceipt) -> Result<()> {
        bail!("Windows uninstall registry is unavailable")
    }
    pub(super) fn unregister_uninstall(_: &InstallReceipt) -> Result<()> {
        bail!("Windows uninstall registry is unavailable")
    }
    impl UserPath for NativePath {
        fn read(&self) -> Result<Option<RegistryValue>> {
            bail!("Windows user PATH is unavailable")
        }
        fn compare_write(
            &mut self,
            _: &Option<RegistryValue>,
            _: &Option<RegistryValue>,
        ) -> Result<bool> {
            bail!("Windows user PATH is unavailable")
        }
        fn broadcast(&self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
