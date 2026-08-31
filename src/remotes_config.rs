use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Arc, atomic::AtomicBool};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_file::replace_file;
use crate::remote_protocol::SourceGeneration;

pub const REMOTES_CONFIG_VERSION: u32 = 1;
pub const DEFAULT_ACTIVE_INTERVAL_SECONDS: u64 = 60;
pub const DEFAULT_IDLE_INTERVAL_SECONDS: u64 = 300;
pub const DEFAULT_REMOTE_AGENT_EXECUTABLE: &str = "codex-usage-monit";

const APP_DIRECTORY: &str = "codex-usage-monit";
const CONFIG_DIRECTORY_ENV: &str = "CODEX_USAGE_MONIT_CONFIG_DIR";
const CONFIG_FILE: &str = "remotes.json";
const LOCK_FILE: &str = "remotes.lock";
const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;
const MAX_HOSTS: usize = 256;
const MAX_HOST_ID_BYTES: usize = 64;
const MAX_SSH_HOST_BYTES: usize = 255;
const MAX_AGENT_EXECUTABLE_BYTES: usize = 512;
const MIN_ACTIVE_INTERVAL_SECONDS: u64 = 30;
const MAX_ACTIVE_INTERVAL_SECONDS: u64 = 3_600;
const MIN_IDLE_INTERVAL_SECONDS: u64 = 60;
const MAX_IDLE_INTERVAL_SECONDS: u64 = 86_400;
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The per-host configuration state. Global auto-sync is deliberately not
/// included: turning it off preserves the user's per-host selections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteHostState {
    ConfiguredUnpaired,
    PairedDisabled,
    PairedEnabled,
}

impl RemoteHostState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfiguredUnpaired => "configured-unpaired",
            Self::PairedDisabled => "paired-disabled",
            Self::PairedEnabled => "paired-enabled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteHostConfig {
    id: String,
    ssh_host: String,
    /// Executable invoked by the remote SSH transport. This is a single,
    /// shell-safe command token rather than an arbitrary command line.
    agent_executable: String,
    /// Opts this host into automatic scheduling once the global switch is on.
    /// Pairing and successful tests never change this field.
    sync_enabled: bool,
    /// Content is redacted before export unless the user explicitly opts out.
    redact_content: bool,
    /// Stable remote identity and exporter generation pinned by an explicit
    /// pairing operation. A node-id rotation must be visible as a mismatch.
    expected_source: Option<SourceGeneration>,
    /// The source released by the most recent explicit unpair. This is not an
    /// active connection pin and never authorizes synchronization; retaining
    /// it lets a later remove apply its source-history policy without guessing
    /// from a mutable display label. A successful pair consumes this reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_source: Option<SourceGeneration>,
}

impl RemoteHostConfig {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn ssh_host(&self) -> &str {
        &self.ssh_host
    }

    pub fn agent_executable(&self) -> &str {
        &self.agent_executable
    }

    pub fn sync_enabled(&self) -> bool {
        self.sync_enabled
    }

    pub fn redact_content(&self) -> bool {
        self.redact_content
    }

    pub fn expected_source(&self) -> Option<&SourceGeneration> {
        self.expected_source.as_ref()
    }

    /// Returns the detached source retained solely for a subsequent remove.
    /// This value is never an active pairing or automatic-sync credential.
    pub fn previous_source(&self) -> Option<&SourceGeneration> {
        self.previous_source.as_ref()
    }

    pub fn state(&self) -> RemoteHostState {
        match (&self.expected_source, self.sync_enabled) {
            (None, _) => RemoteHostState::ConfiguredUnpaired,
            (Some(_), false) => RemoteHostState::PairedDisabled,
            (Some(_), true) => RemoteHostState::PairedEnabled,
        }
    }

    pub fn is_paired(&self) -> bool {
        self.expected_source.is_some()
    }

    /// Content-free stable binding for durable scheduler safety state. The
    /// digest includes the complete allowlist row but never persists the SSH
    /// alias, source pin, or other connection metadata in health storage.
    pub fn automatic_sync_fingerprint(&self) -> String {
        let encoded = serde_json::to_vec(self)
            .expect("serializing a validated remote host config cannot fail");
        let digest = Sha256::digest(encoded);
        let mut output = String::with_capacity(22 + digest.len() * 2);
        output.push_str("remote-host-sha256-v1-");
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
        }
        output
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteHostEdit {
    pub ssh_host: Option<String>,
    pub agent_executable: Option<String>,
    pub redact_content: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemotesConfig {
    version: u32,
    /// Monotonically changes whenever an in-memory configuration mutation is
    /// committed. Workers can use this to reject responses from stale config.
    config_revision: u64,
    /// Fail-closed default: merely configuring or pairing hosts never connects.
    auto_sync_enabled: bool,
    active_interval_seconds: u64,
    idle_interval_seconds: u64,
    /// The only remote connection allowlist. SSH config is never enumerated.
    hosts: Vec<RemoteHostConfig>,
}

impl Default for RemotesConfig {
    fn default() -> Self {
        Self {
            version: REMOTES_CONFIG_VERSION,
            config_revision: 0,
            auto_sync_enabled: false,
            active_interval_seconds: DEFAULT_ACTIVE_INTERVAL_SECONDS,
            idle_interval_seconds: DEFAULT_IDLE_INTERVAL_SECONDS,
            hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteHostConfigWire {
    id: String,
    ssh_host: String,
    #[serde(default = "default_remote_agent_executable")]
    agent_executable: String,
    #[serde(default)]
    sync_enabled: bool,
    #[serde(default = "default_true")]
    redact_content: bool,
    #[serde(default)]
    expected_source: Option<SourceGeneration>,
    #[serde(default)]
    previous_source: Option<SourceGeneration>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemotesConfigWire {
    version: u32,
    #[serde(default)]
    config_revision: u64,
    #[serde(default)]
    auto_sync_enabled: bool,
    #[serde(default = "default_active_interval_seconds")]
    active_interval_seconds: u64,
    #[serde(default = "default_idle_interval_seconds")]
    idle_interval_seconds: u64,
    #[serde(default)]
    hosts: Vec<RemoteHostConfigWire>,
}

impl<'de> Deserialize<'de> for RemotesConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RemotesConfigWire::deserialize(deserializer)?;
        let config = Self {
            version: wire.version,
            config_revision: wire.config_revision,
            auto_sync_enabled: wire.auto_sync_enabled,
            active_interval_seconds: wire.active_interval_seconds,
            idle_interval_seconds: wire.idle_interval_seconds,
            hosts: wire
                .hosts
                .into_iter()
                .map(|host| RemoteHostConfig {
                    id: host.id,
                    ssh_host: host.ssh_host,
                    agent_executable: host.agent_executable,
                    sync_enabled: host.sync_enabled,
                    redact_content: host.redact_content,
                    expected_source: host.expected_source,
                    previous_source: host.previous_source,
                })
                .collect(),
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemotesConfigMutation {
    AddHost {
        id: String,
        ssh_host: String,
    },
    EditHost {
        id: String,
        edit: RemoteHostEdit,
    },
    PairPin {
        id: String,
        source: SourceGeneration,
    },
    /// Clears only the pinned remote identity and disables future automatic
    /// scheduling. Durable source history is intentionally outside this
    /// connection configuration and is never deleted by this mutation.
    UnpairHost {
        id: String,
    },
    EnableHost {
        id: String,
    },
    DisableHost {
        id: String,
    },
    /// Removes only the SSH connection allowlist entry. Source history and
    /// its independent include/detach policy live outside this file.
    RemoveHost {
        id: String,
    },
    SetAutoSyncEnabled(bool),
    SetIntervals {
        active_seconds: u64,
        idle_seconds: u64,
    },
}

impl RemotesConfigMutation {
    pub fn add_host(id: impl Into<String>, ssh_host: impl Into<String>) -> Self {
        Self::AddHost {
            id: id.into(),
            ssh_host: ssh_host.into(),
        }
    }

    pub fn edit_host(id: impl Into<String>, edit: RemoteHostEdit) -> Self {
        Self::EditHost {
            id: id.into(),
            edit,
        }
    }

    pub fn pair_pin(id: impl Into<String>, source: SourceGeneration) -> Self {
        Self::PairPin {
            id: id.into(),
            source,
        }
    }

    pub fn unpair_host(id: impl Into<String>) -> Self {
        Self::UnpairHost { id: id.into() }
    }

    pub fn enable_host(id: impl Into<String>) -> Self {
        Self::EnableHost { id: id.into() }
    }

    pub fn disable_host(id: impl Into<String>) -> Self {
        Self::DisableHost { id: id.into() }
    }

    pub fn remove_host(id: impl Into<String>) -> Self {
        Self::RemoveHost { id: id.into() }
    }

    pub fn set_auto_sync_enabled(enabled: bool) -> Self {
        Self::SetAutoSyncEnabled(enabled)
    }

    pub fn set_intervals(active_seconds: u64, idle_seconds: u64) -> Self {
        Self::SetIntervals {
            active_seconds,
            idle_seconds,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotesConfigTransaction {
    expected_revision: u64,
    config: RemotesConfig,
    changed: bool,
}

impl RemotesConfigTransaction {
    pub fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    pub fn config(&self) -> &RemotesConfig {
        &self.config
    }

    pub fn into_config(self) -> RemotesConfig {
        self.config
    }

    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Applies one explicit mutation to the transaction's private candidate.
    /// Each effective mutation advances the candidate revision; no-op
    /// mutations leave it unchanged.
    pub fn apply(&mut self, mutation: RemotesConfigMutation) -> io::Result<bool> {
        let changed = self.config.apply_mutation(mutation)?;
        self.changed |= changed;
        Ok(changed)
    }
}

impl RemotesConfig {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn auto_sync_enabled(&self) -> bool {
        self.auto_sync_enabled
    }

    pub fn active_interval_seconds(&self) -> u64 {
        self.active_interval_seconds
    }

    pub fn idle_interval_seconds(&self) -> u64 {
        self.idle_interval_seconds
    }

    pub fn hosts(&self) -> &[RemoteHostConfig] {
        &self.hosts
    }

    /// Returns the explicitly paired and enabled hosts eligible for automatic
    /// synchronization, preserving their stable configuration order.
    ///
    /// Configuring or pairing a host is never sufficient on its own: both the
    /// global switch and the per-host switch must be enabled. This selection
    /// is read-only and does not probe, connect to, or otherwise mutate a
    /// remote host.
    pub fn automatic_hosts(&self) -> impl Iterator<Item = &RemoteHostConfig> {
        let auto_sync_enabled = self.auto_sync_enabled;
        self.hosts.iter().filter(move |host| {
            auto_sync_enabled && host.sync_enabled && host.expected_source.is_some()
        })
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.version != REMOTES_CONFIG_VERSION {
            return Err(invalid_config(format!(
                "unsupported remotes config version {}; expected {}",
                self.version, REMOTES_CONFIG_VERSION
            )));
        }
        validate_intervals(self.active_interval_seconds, self.idle_interval_seconds)?;
        if self.hosts.len() > MAX_HOSTS {
            return Err(invalid_config(format!(
                "hosts has {} entries; the maximum is {MAX_HOSTS}",
                self.hosts.len()
            )));
        }

        let mut host_ids = HashSet::with_capacity(self.hosts.len());
        let mut referenced_node_ids = HashSet::with_capacity(self.hosts.len());
        let mut referenced_sources = Vec::with_capacity(self.hosts.len());
        for host in &self.hosts {
            validate_host_id(&host.id)?;
            validate_ssh_host(&host.ssh_host)?;
            validate_agent_executable(&host.agent_executable)?;
            if !host_ids.insert(host.id.as_str()) {
                return Err(invalid_config(format!(
                    "duplicate remote host id {:?}",
                    host.id
                )));
            }

            if host.expected_source.is_some() && host.previous_source.is_some() {
                return Err(invalid_config(format!(
                    "remote host {:?} cannot retain a previous source while actively paired",
                    host.id
                )));
            }

            match host.expected_source.as_ref() {
                Some(source) => {
                    if referenced_sources.contains(&source) {
                        return Err(invalid_config(format!(
                            "source {source:?} is referenced by more than one remote host"
                        )));
                    }
                    if !referenced_node_ids.insert(&source.node_id) {
                        return Err(invalid_config(format!(
                            "node id {:?} is referenced by more than one remote host",
                            source.node_id
                        )));
                    }
                    referenced_sources.push(source);
                }
                None if host.sync_enabled => {
                    return Err(invalid_config(format!(
                        "remote host {:?} cannot enable sync before pairing",
                        host.id
                    )));
                }
                None => {}
            }
            if let Some(source) = host.previous_source.as_ref() {
                if referenced_sources.contains(&source) {
                    return Err(invalid_config(format!(
                        "source {source:?} is referenced by more than one remote host"
                    )));
                }
                if !referenced_node_ids.insert(&source.node_id) {
                    return Err(invalid_config(format!(
                        "node id {:?} is referenced by more than one remote host",
                        source.node_id
                    )));
                }
                referenced_sources.push(source);
            }
        }
        Ok(())
    }

    pub fn host(&self, id: &str) -> Option<&RemoteHostConfig> {
        self.hosts.iter().find(|host| host.id == id)
    }

    fn add_host(&mut self, id: impl Into<String>, ssh_host: impl Into<String>) -> io::Result<bool> {
        let id = id.into();
        let ssh_host = ssh_host.into();
        self.mutate(move |candidate| {
            if candidate.host(&id).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("remote host {id:?} already exists"),
                ));
            }
            candidate.hosts.push(RemoteHostConfig {
                id,
                ssh_host,
                agent_executable: DEFAULT_REMOTE_AGENT_EXECUTABLE.to_owned(),
                sync_enabled: false,
                redact_content: true,
                expected_source: None,
                previous_source: None,
            });
            Ok(true)
        })
    }

    /// Edits connection metadata without connecting, changing the pinned
    /// source generation, or enabling automatic synchronization.
    fn edit_host(&mut self, id: &str, edit: RemoteHostEdit) -> io::Result<bool> {
        self.mutate(|candidate| {
            let host = candidate.host_mut_or_error(id)?;
            let mut changed = false;
            if let Some(ssh_host) = edit.ssh_host {
                changed |= host.ssh_host != ssh_host;
                host.ssh_host = ssh_host;
            }
            if let Some(agent_executable) = edit.agent_executable {
                changed |= host.agent_executable != agent_executable;
                host.agent_executable = agent_executable;
            }
            if let Some(redact_content) = edit.redact_content {
                changed |= host.redact_content != redact_content;
                host.redact_content = redact_content;
            }
            Ok(changed)
        })
    }

    /// Pins the identity and generation observed during an explicit pair
    /// operation. Pairing is intentionally not an enable operation and cannot
    /// silently replace either part of an already pinned source.
    fn pair_pin(&mut self, id: &str, source: SourceGeneration) -> io::Result<bool> {
        self.mutate(|candidate| {
            let current = candidate.host_or_error(id)?.expected_source.as_ref();
            if let Some(current) = current {
                if current == &source {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "remote host {id:?} is already paired with a different source generation"
                    ),
                ));
            }
            if candidate.hosts.iter().any(|host| {
                host.id != id
                    && host
                        .expected_source
                        .as_ref()
                        .is_some_and(|expected| expected.node_id == source.node_id)
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "node id {:?} is already paired with another remote host",
                        source.node_id
                    ),
                ));
            }
            // A detached source may be retained by an unpaired alias solely so
            // `remove` can still apply its include policy. Once that source is
            // explicitly paired again, exactly one host owns it and every
            // stale lifecycle reference must be consumed atomically.
            for host in &mut candidate.hosts {
                if host
                    .previous_source
                    .as_ref()
                    .is_some_and(|previous| previous.node_id == source.node_id)
                {
                    host.previous_source = None;
                }
            }
            let host = candidate.host_mut_or_error(id)?;
            host.previous_source = None;
            host.expected_source = Some(source);
            Ok(true)
        })
    }

    /// Explicit recovery path after the remote rotates its node identity or
    /// exporter generation. This low-level mutation moves the active pin into
    /// a non-authorizing lifecycle reference and fails closed by disabling the
    /// host. The CLI must durably detach source metadata before publishing it.
    fn unpair_host(&mut self, id: &str) -> io::Result<bool> {
        self.mutate(|candidate| {
            let host = candidate.host_mut_or_error(id)?;
            let released = host.expected_source.take();
            let changed = released.is_some() || host.sync_enabled;
            if let Some(source) = released {
                host.previous_source = Some(source);
            }
            host.sync_enabled = false;
            Ok(changed)
        })
    }

    fn enable_host(&mut self, id: &str) -> io::Result<bool> {
        self.mutate(|candidate| {
            let host = candidate.host_mut_or_error(id)?;
            if !host.is_paired() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("remote host {id:?} must be paired before sync can be enabled"),
                ));
            }
            let changed = !host.sync_enabled;
            host.sync_enabled = true;
            Ok(changed)
        })
    }

    fn disable_host(&mut self, id: &str) -> io::Result<bool> {
        self.mutate(|candidate| {
            let host = candidate.host_mut_or_error(id)?;
            let changed = host.sync_enabled;
            host.sync_enabled = false;
            Ok(changed)
        })
    }

    fn remove_host(&mut self, id: &str) -> io::Result<bool> {
        self.mutate(|candidate| {
            let Some(index) = candidate.hosts.iter().position(|host| host.id == id) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("remote host {id:?} is not configured"),
                ));
            };
            candidate.hosts.remove(index);
            Ok(true)
        })
    }

    fn set_auto_sync_enabled(&mut self, enabled: bool) -> io::Result<bool> {
        self.mutate(|candidate| {
            let changed = candidate.auto_sync_enabled != enabled;
            candidate.auto_sync_enabled = enabled;
            Ok(changed)
        })
    }

    fn set_intervals(&mut self, active_seconds: u64, idle_seconds: u64) -> io::Result<bool> {
        self.mutate(|candidate| {
            let changed = candidate.active_interval_seconds != active_seconds
                || candidate.idle_interval_seconds != idle_seconds;
            candidate.active_interval_seconds = active_seconds;
            candidate.idle_interval_seconds = idle_seconds;
            Ok(changed)
        })
    }

    fn host_or_error(&self, id: &str) -> io::Result<&RemoteHostConfig> {
        self.host(id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("remote host {id:?} is not configured"),
            )
        })
    }

    fn host_mut_or_error(&mut self, id: &str) -> io::Result<&mut RemoteHostConfig> {
        self.hosts
            .iter_mut()
            .find(|host| host.id == id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("remote host {id:?} is not configured"),
                )
            })
    }

    fn mutate(
        &mut self,
        operation: impl FnOnce(&mut RemotesConfig) -> io::Result<bool>,
    ) -> io::Result<bool> {
        self.validate()?;
        let mut candidate = self.clone();
        if !operation(&mut candidate)? {
            return Ok(false);
        }
        candidate.config_revision = candidate
            .config_revision
            .checked_add(1)
            .ok_or_else(|| invalid_config("configRevision cannot advance beyond u64::MAX"))?;
        candidate.validate()?;
        *self = candidate;
        Ok(true)
    }

    fn apply_mutation(&mut self, mutation: RemotesConfigMutation) -> io::Result<bool> {
        match mutation {
            RemotesConfigMutation::AddHost { id, ssh_host } => self.add_host(id, ssh_host),
            RemotesConfigMutation::EditHost { id, edit } => self.edit_host(&id, edit),
            RemotesConfigMutation::PairPin { id, source } => self.pair_pin(&id, source),
            RemotesConfigMutation::UnpairHost { id } => self.unpair_host(&id),
            RemotesConfigMutation::EnableHost { id } => self.enable_host(&id),
            RemotesConfigMutation::DisableHost { id } => self.disable_host(&id),
            RemotesConfigMutation::RemoveHost { id } => self.remove_host(&id),
            RemotesConfigMutation::SetAutoSyncEnabled(enabled) => {
                self.set_auto_sync_enabled(enabled)
            }
            RemotesConfigMutation::SetIntervals {
                active_seconds,
                idle_seconds,
            } => self.set_intervals(active_seconds, idle_seconds),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemotesConfigStore {
    path: Option<PathBuf>,
    /// Per-store fault injection used to prove that a pre-commit side effect
    /// cannot leave a removed host published with an included source. Clones
    /// intentionally share the one-shot flag so concurrent tests exercise the
    /// same durable store boundary.
    #[cfg(test)]
    fail_next_precommitted_write: Arc<AtomicBool>,
}

pub(crate) enum TryCurrentHost<R> {
    Current(R),
    Busy,
    Changed,
}

impl Default for RemotesConfigStore {
    fn default() -> Self {
        Self::discover()
    }
}

impl RemotesConfigStore {
    pub fn discover() -> Self {
        Self {
            path: default_remotes_config_path(),
            #[cfg(test)]
            fail_next_precommitted_write: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            #[cfg(test)]
            fail_next_precommitted_write: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn load(&self) -> io::Result<RemotesConfig> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        let _lock = open_locked_lock_file(parent, LockMode::Shared)?;
        read_config(path)
    }

    /// Runs one short local mutation only while an exact host snapshot remains
    /// current.  The shared config lock linearizes the mutation against
    /// remove, unpair, redaction, and target edits; callers must never perform
    /// network I/O or an unbounded wait inside `operation`.
    pub(crate) fn with_current_host<R>(
        &self,
        expected_revision: u64,
        expected_host: &RemoteHostConfig,
        operation: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<R> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        let _lock = open_locked_lock_file(parent, LockMode::Shared)?;
        let current = read_config(path)?;
        ensure_expected_revision(expected_revision, current.config_revision)?;
        ensure_expected_host(&current, expected_host)?;
        operation()
    }

    /// Nonblocking exact-host fence used after expensive remote-page staging.
    /// A concurrent config mutation is never waited on while another subsystem
    /// lock is held: the caller releases its staged resources and retries.
    pub(crate) fn try_with_current_host<R>(
        &self,
        expected_revision: u64,
        expected_host: &RemoteHostConfig,
        operation: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<TryCurrentHost<R>> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        let Some(_lock) = try_open_locked_lock_file(parent, LockMode::Shared)? else {
            return Ok(TryCurrentHost::Busy);
        };
        let current = read_config(path)?;
        if current.config_revision != expected_revision
            || current.host(expected_host.id()) != Some(expected_host)
        {
            return Ok(TryCurrentHost::Changed);
        }
        operation().map(TryCurrentHost::Current)
    }

    /// Runs one destructive source-lifecycle operation only while no current
    /// SSH configuration pins the source identity.
    ///
    /// The exclusive allowlist lock is intentionally retained for the whole
    /// callback. This prevents a concurrent pair from attaching the source
    /// after the detached check but before its retained local state is
    /// removed. Callers must follow the global lock order
    /// `remotes config -> v2 history writer` and must not perform network I/O.
    pub(crate) fn with_unattached_source<R>(
        &self,
        source_id: &crate::source_identity::NodeId,
        operation: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<R> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;
        let current = load_or_create_locked(path)?;
        if let Some(host) = current.hosts().iter().find(|host| {
            host.expected_source()
                .is_some_and(|source| &source.node_id == source_id)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "remote source {source_id} is still attached to configured host {:?}; remove that host before purging retained history",
                    host.id()
                ),
            ));
        }
        operation()
    }

    /// Loads the allowlist or creates a fail-closed default. A concurrent first
    /// writer wins; this method never overwrites a config created in the race.
    pub fn load_or_create(&self) -> io::Result<RemotesConfig> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;
        load_or_create_locked(path)
    }

    /// Opens an optimistic transaction over the current config. The sidecar
    /// lock is intentionally not retained while the caller edits the
    /// candidate; [`Self::commit`] performs a revision compare-and-swap under
    /// an exclusive lock.
    pub fn begin_transaction(&self) -> io::Result<RemotesConfigTransaction> {
        let config = self.load_or_create()?;
        Ok(RemotesConfigTransaction {
            expected_revision: config.config_revision,
            config,
            changed: false,
        })
    }

    /// Commits a transaction only when the on-disk revision still matches its
    /// base revision. This is the sole whole-config write API: callers cannot
    /// bypass validation or accidentally perform a blind last-writer-wins
    /// replacement.
    pub fn commit(&self, transaction: RemotesConfigTransaction) -> io::Result<RemotesConfig> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;

        let current = read_config(path)?;
        ensure_expected_revision(transaction.expected_revision, current.config_revision)?;
        transaction.config.validate()?;

        if !transaction.changed {
            return Ok(current);
        }
        if transaction.config.config_revision <= transaction.expected_revision {
            return Err(invalid_config(
                "a changed remotes config transaction must advance configRevision",
            ));
        }

        let contents = serialize_config(&transaction.config)?;
        write_private_atomically(path, &contents)?;
        Ok(transaction.config)
    }

    /// Applies a single mutation with revision compare-and-swap semantics.
    /// This is convenient for CLI and settings actions that already rendered a
    /// particular revision. A stale action is rejected rather than replayed on
    /// newer user choices.
    pub fn update(
        &self,
        expected_revision: u64,
        mutation: RemotesConfigMutation,
    ) -> io::Result<RemotesConfig> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;

        let mut current = load_or_create_locked(path)?;
        ensure_expected_revision(expected_revision, current.config_revision)?;
        if current.apply_mutation(mutation)? {
            let contents = serialize_config(&current)?;
            write_private_atomically(path, &contents)?;
        }
        Ok(current)
    }

    /// Applies a configuration mutation under the exclusive allowlist lock,
    /// but publishes a caller-provided durable side effect first.
    ///
    /// This is the narrow cross-store primitive used by remote source
    /// lifecycle operations. The callback receives the pre-mutation config
    /// and the validated candidate, and runs while the exclusive config lock
    /// is still held. A callback or config-publish failure therefore leaves
    /// the old allowlist in place. Callbacks must follow the global lock order
    /// `remotes config -> v2 history writer`, must not try to acquire any
    /// remotes config lock recursively, and must never perform network I/O.
    pub(crate) fn update_after_precommit<R>(
        &self,
        expected_revision: u64,
        mutation: RemotesConfigMutation,
        precommit: impl FnOnce(&RemotesConfig, &RemotesConfig) -> io::Result<R>,
    ) -> io::Result<(RemotesConfig, R)> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;

        let mut candidate = load_or_create_locked(path)?;
        ensure_expected_revision(expected_revision, candidate.config_revision)?;
        let previous = candidate.clone();
        let changed = candidate.apply_mutation(mutation)?;
        let result = precommit(&previous, &candidate)?;
        if changed {
            let contents = serialize_config(&candidate)?;
            #[cfg(test)]
            if self
                .fail_next_precommitted_write
                .swap(false, Ordering::SeqCst)
            {
                return Err(io::Error::other(
                    "injected remotes config publish failure after precommit",
                ));
            }
            write_private_atomically(path, &contents)?;
        }
        Ok((candidate, result))
    }

    #[cfg(test)]
    pub(crate) fn fail_next_precommitted_write_for_test(&self) {
        self.fail_next_precommitted_write
            .store(true, Ordering::SeqCst);
    }

    /// Pins a probe result only if the exact host entry that was probed is
    /// still current. The revision and host comparison happen while holding
    /// the same exclusive lock as the write, so even a non-cooperating writer
    /// that rewrites the JSON without advancing `configRevision` cannot bind a
    /// response from one SSH target to another target's configuration.
    #[cfg(test)]
    fn pair_if_current(
        &self,
        expected_revision: u64,
        expected_host: &RemoteHostConfig,
        source: SourceGeneration,
    ) -> io::Result<RemotesConfig> {
        self.pair_if_current_checked(expected_revision, expected_host, source, || Ok(()))
    }

    /// Pairs only after a bounded local precondition succeeds while the
    /// exclusive allowlist lock is held. This is used to fence a durable
    /// source-purge intent against reattachment; callers must not perform
    /// network I/O or wait on a lock ordered before the remotes config lock.
    pub(crate) fn pair_if_current_checked(
        &self,
        expected_revision: u64,
        expected_host: &RemoteHostConfig,
        source: SourceGeneration,
        precondition: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<RemotesConfig> {
        let path = self.required_path()?;
        let parent = config_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;

        let mut current = load_or_create_locked(path)?;
        ensure_expected_revision(expected_revision, current.config_revision)?;
        ensure_expected_host(&current, expected_host)?;
        precondition()?;
        if current.apply_mutation(RemotesConfigMutation::pair_pin(
            expected_host.id().to_owned(),
            source,
        ))? {
            let contents = serialize_config(&current)?;
            write_private_atomically(path, &contents)?;
        }
        Ok(current)
    }

    fn required_path(&self) -> io::Result<&Path> {
        self.path.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no user-level configuration directory is available",
            )
        })
    }
}

fn ensure_expected_revision(expected: u64, actual: u64) -> io::Result<()> {
    if expected == actual {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "stale remotes config revision {expected}; current revision is {actual}; reload before retrying"
        ),
    ))
}

fn ensure_expected_host(
    current: &RemotesConfig,
    expected_host: &RemoteHostConfig,
) -> io::Result<()> {
    let Some(actual_host) = current.host(expected_host.id()) else {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "stale remote host {:?}; the probed host was removed; reload before retrying",
                expected_host.id()
            ),
        ));
    };
    if actual_host != expected_host {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "stale remote host {:?}; its configuration changed without a revision advance; reload before retrying",
                expected_host.id()
            ),
        ));
    }
    Ok(())
}

pub fn default_remotes_config_path() -> Option<PathBuf> {
    resolve_remotes_config_path(
        nonempty_env(CONFIG_DIRECTORY_ENV).as_deref(),
        nonempty_env("XDG_CONFIG_HOME").as_deref(),
        nonempty_env("HOME").as_deref(),
        nonempty_env("LOCALAPPDATA").as_deref(),
        nonempty_env("USERPROFILE").as_deref(),
        current_platform(),
    )
}

fn default_true() -> bool {
    true
}

fn default_active_interval_seconds() -> u64 {
    DEFAULT_ACTIVE_INTERVAL_SECONDS
}

fn default_idle_interval_seconds() -> u64 {
    DEFAULT_IDLE_INTERVAL_SECONDS
}

fn default_remote_agent_executable() -> String {
    DEFAULT_REMOTE_AGENT_EXECUTABLE.to_owned()
}

fn serialize_config(config: &RemotesConfig) -> io::Result<Vec<u8>> {
    config.validate()?;
    let mut contents = serde_json::to_vec_pretty(config)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');
    Ok(contents)
}

fn deserialize_config(contents: &[u8]) -> io::Result<RemotesConfig> {
    let version = serde_json::from_slice::<VersionProbe>(contents)
        .map_err(|error| invalid_config(format!("invalid remotes config: {error}")))?;
    let Some(version) = version.version else {
        return Err(invalid_config("remotes config is missing version"));
    };
    if version != REMOTES_CONFIG_VERSION {
        return Err(invalid_config(format!(
            "unsupported remotes config version {version}; expected {REMOTES_CONFIG_VERSION}"
        )));
    }

    let config: RemotesConfig = serde_json::from_slice(contents)
        .map_err(|error| invalid_config(format!("invalid remotes config: {error}")))?;
    config.validate()?;
    Ok(config)
}

#[derive(Debug, Deserialize)]
struct VersionProbe {
    version: Option<u32>,
}

fn validate_intervals(active: u64, idle: u64) -> io::Result<()> {
    if !(MIN_ACTIVE_INTERVAL_SECONDS..=MAX_ACTIVE_INTERVAL_SECONDS).contains(&active) {
        return Err(invalid_config(format!(
            "activeIntervalSeconds must be between {MIN_ACTIVE_INTERVAL_SECONDS} and {MAX_ACTIVE_INTERVAL_SECONDS}"
        )));
    }
    if !(MIN_IDLE_INTERVAL_SECONDS..=MAX_IDLE_INTERVAL_SECONDS).contains(&idle) {
        return Err(invalid_config(format!(
            "idleIntervalSeconds must be between {MIN_IDLE_INTERVAL_SECONDS} and {MAX_IDLE_INTERVAL_SECONDS}"
        )));
    }
    if idle < active {
        return Err(invalid_config(
            "idleIntervalSeconds must be greater than or equal to activeIntervalSeconds",
        ));
    }
    Ok(())
}

fn validate_host_id(id: &str) -> io::Result<()> {
    if id.is_empty() || id.len() > MAX_HOST_ID_BYTES {
        return Err(invalid_config(format!(
            "host id must contain between 1 and {MAX_HOST_ID_BYTES} bytes"
        )));
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_alphanumeric()
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_alphanumeric() && *byte != b'-' && *byte != b'_')
    {
        return Err(invalid_config(
            "host id must start with an ASCII letter or digit and contain only ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

fn validate_ssh_host(ssh_host: &str) -> io::Result<()> {
    if ssh_host.is_empty() || ssh_host.len() > MAX_SSH_HOST_BYTES {
        return Err(invalid_config(format!(
            "sshHost must contain between 1 and {MAX_SSH_HOST_BYTES} bytes"
        )));
    }
    if ssh_host.starts_with('-') {
        return Err(invalid_config("sshHost must not start with '-'"));
    }
    if ssh_host.chars().any(char::is_control) {
        return Err(invalid_config(
            "sshHost must not contain control characters",
        ));
    }
    if ssh_host.chars().any(char::is_whitespace) {
        return Err(invalid_config("sshHost must not contain whitespace"));
    }
    Ok(())
}

fn validate_agent_executable(agent_executable: &str) -> io::Result<()> {
    if agent_executable.is_empty() || agent_executable.len() > MAX_AGENT_EXECUTABLE_BYTES {
        return Err(invalid_config(format!(
            "agentExecutable must contain between 1 and {MAX_AGENT_EXECUTABLE_BYTES} bytes"
        )));
    }
    if agent_executable.bytes().any(|byte| {
        !byte.is_ascii_alphanumeric()
            && !matches!(byte, b'/' | b'.' | b'_' | b':' | b'+' | b'~' | b'-')
    }) {
        return Err(invalid_config(
            "agentExecutable may contain only ASCII letters, digits, '/', '.', '_', ':', '+', '~', and '-'",
        ));
    }
    Ok(())
}

fn invalid_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Windows,
    Unix,
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

fn resolve_remotes_config_path(
    override_directory: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    user_profile: Option<&Path>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(directory) = nonempty_path(override_directory) {
        return Some(directory.join(CONFIG_FILE));
    }
    if let Some(directory) = nonempty_path(xdg_config_home) {
        return Some(directory.join(APP_DIRECTORY).join(CONFIG_FILE));
    }

    let directory = match platform {
        Platform::MacOs => nonempty_path(home).map(|path| path.join("Library/Application Support")),
        Platform::Windows => nonempty_path(local_app_data)
            .map(Path::to_path_buf)
            .or_else(|| nonempty_path(user_profile).map(|path| path.join("AppData").join("Local"))),
        Platform::Unix => nonempty_path(home).map(|path| path.join(".config")),
    }?;
    Some(directory.join(APP_DIRECTORY).join(CONFIG_FILE))
}

fn nonempty_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn load_or_create_locked(path: &Path) -> io::Result<RemotesConfig> {
    match read_config(path) {
        Ok(config) => Ok(config),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let default = RemotesConfig::default();
            let contents = serialize_config(&default)?;
            if create_private_atomically(path, &contents)? {
                Ok(default)
            } else {
                // A writer that does not use our sidecar lock may have won the
                // create race. Its file is authoritative only after the same
                // strict validation as every normal read.
                read_config(path)
            }
        }
        Err(error) => Err(error),
    }
}

fn read_config(path: &Path) -> io::Result<RemotesConfig> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_config_metadata(&path_metadata)?;

    let mut file = open_config_file(path)?;
    let opened_metadata = file.metadata()?;
    validate_config_metadata(&opened_metadata)?;
    ensure_opened_file_matches_path(
        path,
        &file,
        &path_metadata,
        &opened_metadata,
        "remotes config",
    )?;
    if opened_metadata.len() > MAX_CONFIG_FILE_BYTES {
        return Err(invalid_config("remotes config file is too large"));
    }
    ensure_private_file(path, &file, &opened_metadata, "remotes config file")?;

    let mut contents = Vec::with_capacity(opened_metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(invalid_config("remotes config file is too large"));
    }
    deserialize_config(&contents)
}

fn open_config_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    options
        .open(path)
        .map_err(|error| map_nofollow_error(error, "remotes config path"))
}

fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    if contents.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(invalid_config("remotes config file is too large"));
    }
    let parent = config_parent(path);
    create_private_directory(parent)?;

    let file_name = path.file_name().unwrap_or_else(|| OsStr::new(CONFIG_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_published_private_file(path, "remotes config file")?;
        sync_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Publishes the initial default without racing an independently created user
/// configuration. `Ok(false)` means another writer won the race.
fn create_private_atomically(path: &Path, contents: &[u8]) -> io::Result<bool> {
    if contents.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(invalid_config("remotes config file is too large"));
    }
    let parent = config_parent(path);
    create_private_directory(parent)?;

    let file_name = path.file_name().unwrap_or_else(|| OsStr::new(CONFIG_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                validate_published_private_file(path, "remotes config file")?;
                fs::remove_file(&temporary)?;
                sync_directory(parent)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_file(parent: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&temporary) {
            Ok(file) => {
                let validation = (|| {
                    let metadata = file.metadata()?;
                    validate_config_metadata(&metadata)?;
                    ensure_private_file(
                        &temporary,
                        &file,
                        &metadata,
                        "remotes config temporary file",
                    )
                })();
                match validation {
                    Ok(()) => return Ok((temporary, file)),
                    Err(error) => {
                        drop(file);
                        let _ = fs::remove_file(&temporary);
                        return Err(error);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique remotes config temporary file",
    ))
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match validate_private_directory(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
    }
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_config(
            "remotes config directory must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_config(
            "remotes config directory path must be a directory",
        ));
    }
    ensure_private_directory(path, &metadata, "remotes config directory")
}

fn validate_config_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_config(
            "remotes config path must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_config("remotes config path must be a regular file"));
    }
    Ok(())
}

fn validate_lock_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_config(
            "remotes config lock must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_config("remotes config lock must be a regular file"));
    }
    ensure_private_path(metadata, "remotes config lock")
}

fn validate_published_private_file(path: &Path, subject: &str) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_config_metadata(&path_metadata)?;
    let file = open_config_file(path)?;
    let opened_metadata = file.metadata()?;
    validate_config_metadata(&opened_metadata)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &opened_metadata, subject)?;
    ensure_private_file(path, &file, &opened_metadata, subject)
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_path(metadata, subject)
}

#[cfg(windows)]
fn ensure_private_file(
    path: &Path,
    file: &File,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_file(path, file, subject)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private remote configuration files are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn ensure_private_directory(
    _path: &Path,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_path(metadata, subject)
}

#[cfg(windows)]
fn ensure_private_directory(
    path: &Path,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_directory(path, subject)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(
    _path: &Path,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private remote configuration directories are unsupported on this platform",
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockMode {
    Shared,
    Exclusive,
}

fn open_lock_file(directory: &Path) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_lock_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    add_nofollow_flags(&mut options);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // Do not permit a cooperating process to delete/replace the stable
        // sidecar while another process still coordinates through its inode.
        options.share_mode(stable_lock_share_mode());
    }
    let file = options
        .open(&path)
        .map_err(|error| map_nofollow_error(error, "remotes config lock"))?;
    let opened_metadata = file.metadata()?;
    validate_lock_metadata(&opened_metadata)?;
    ensure_private_file(&path, &file, &opened_metadata, "remotes config lock")?;

    // Re-check after opening so a path swap cannot make cooperating processes
    // silently lock different inodes.
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &opened_metadata,
        "remotes config lock",
    )?;
    Ok(file)
}

/// Opens and locks the stable sidecar inode, then revalidates the directory
/// entry after the potentially blocking lock acquisition. Without the second
/// inode check, a waiter could acquire an unlinked old lock while newer
/// processes coordinate through its replacement.
fn open_locked_lock_file(directory: &Path, mode: LockMode) -> io::Result<File> {
    let file = open_lock_file(directory)?;
    lock_opened_lock_file(directory, file, mode)
}

fn try_open_locked_lock_file(directory: &Path, mode: LockMode) -> io::Result<Option<File>> {
    let file = open_lock_file(directory)?;
    let result = match mode {
        LockMode::Shared => fs2::FileExt::try_lock_shared(&file),
        LockMode::Exclusive => fs2::FileExt::try_lock_exclusive(&file),
    };
    match result {
        Ok(()) => validate_locked_lock_file(directory, file).map(Some),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

fn lock_opened_lock_file(directory: &Path, file: File, mode: LockMode) -> io::Result<File> {
    match mode {
        LockMode::Shared => fs2::FileExt::lock_shared(&file)?,
        LockMode::Exclusive => fs2::FileExt::lock_exclusive(&file)?,
    }

    validate_locked_lock_file(directory, file)
}

fn validate_locked_lock_file(directory: &Path, file: File) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(LOCK_FILE);
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    let opened_metadata = file.metadata()?;
    validate_lock_metadata(&opened_metadata)?;
    ensure_private_file(&path, &file, &opened_metadata, "remotes config lock")?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &opened_metadata,
        "remotes config lock",
    )?;
    Ok(file)
}

fn add_nofollow_flags(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // O_NOFOLLOW closes the final-component lstat/open race. O_NONBLOCK
        // ensures a FIFO swapped into place cannot hang before fstat rejects
        // it as non-regular.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(any(test, windows))]
fn stable_lock_share_mode() -> u32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        FILE_SHARE_READ | FILE_SHARE_WRITE
    }
    #[cfg(not(windows))]
    {
        // Windows SDK values, kept here so the policy has a host-independent
        // regression test without exposing it in non-test Unix builds.
        0x0000_0001 | 0x0000_0002
    }
}

fn map_nofollow_error(error: io::Error, subject: &str) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_config(format!("{subject} must not be a symbolic link"));
    }
    #[cfg(not(unix))]
    let _ = subject;
    error
}

#[cfg(unix)]
fn ensure_private_path(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // SAFETY: geteuid has no preconditions and retains no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must be owned by the current user"),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must not be accessible by group or other users"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_path(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    // Rust metadata does not expose a portable DACL check. The default
    // LOCALAPPDATA location inherits the user's private ACL. We still enforce
    // regular-file/directory and no-reparse-point checks here.
    Ok(())
}

#[cfg(unix)]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
    path_metadata: &fs::Metadata,
    opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if path_metadata.dev() == opened_metadata.dev() && path_metadata.ino() == opened_metadata.ino()
    {
        Ok(())
    } else {
        Err(invalid_config(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn ensure_opened_file_matches_path(
    path: &Path,
    opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let current = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;

    if windows_file_identity(&current)? == windows_file_identity(opened_file)? {
        Ok(())
    } else {
        Err(invalid_config(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the raw handle remains owned and live for this call;
    // GetFileInformationByHandle initializes the output on success and does
    // not retain either pointer.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API reported success, so the complete structure is
    // initialized.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, file_index))
}

#[cfg(not(any(unix, windows)))]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // replace_file uses a write-through move on Windows; std does not expose a
    // portable directory fsync handle for the create-only hard-link path.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use tempfile::tempdir;

    const NODE_ONE: &str = "node-0123456789abcdef0123456789abcdef";
    const NODE_TWO: &str = "node-fedcba9876543210fedcba9876543210";

    fn source(node_id: &str, generation: u64) -> SourceGeneration {
        SourceGeneration {
            node_id: node_id.parse().unwrap(),
            generation: NonZeroU64::new(generation).unwrap(),
        }
    }

    #[test]
    fn defaults_are_fail_closed_and_have_no_hosts() {
        assert_eq!(
            RemotesConfig::default(),
            RemotesConfig {
                version: REMOTES_CONFIG_VERSION,
                config_revision: 0,
                auto_sync_enabled: false,
                active_interval_seconds: 60,
                idle_interval_seconds: 300,
                hosts: Vec::new(),
            }
        );
        RemotesConfig::default().validate().unwrap();
    }

    #[test]
    fn destructive_source_operation_refuses_any_current_pin() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let config = store.load_or_create().unwrap();
        let config = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        let pinned = source(NODE_ONE, 1);
        store
            .update(
                config.config_revision(),
                RemotesConfigMutation::pair_pin("dev", pinned.clone()),
            )
            .unwrap();

        let mut called = false;
        let error = store
            .with_unattached_source(&pinned.node_id, || {
                called = true;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!called);
        assert_eq!(
            store.load().unwrap().host("dev").unwrap().expected_source(),
            Some(&pinned)
        );
    }

    #[test]
    fn windows_stable_lock_share_mode_excludes_delete() {
        const FILE_SHARE_READ_FOR_TEST: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE_FOR_TEST: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE_FOR_TEST: u32 = 0x0000_0004;

        let mode = stable_lock_share_mode();
        assert_eq!(
            mode & (FILE_SHARE_READ_FOR_TEST | FILE_SHARE_WRITE_FOR_TEST),
            FILE_SHARE_READ_FOR_TEST | FILE_SHARE_WRITE_FOR_TEST
        );
        assert_eq!(mode & FILE_SHARE_DELETE_FOR_TEST, 0);
    }

    #[test]
    fn missing_opt_in_fields_remain_disabled() {
        let config = deserialize_config(br#"{"version":1}"#).unwrap();
        assert_eq!(config, RemotesConfig::default());

        let config = deserialize_config(
            br#"{
  "version": 1,
  "hosts": [{"id": "dev", "sshHost": "dev-alias"}]
}"#,
        )
        .unwrap();
        assert!(!config.auto_sync_enabled);
        assert_eq!(config.hosts[0].state(), RemoteHostState::ConfiguredUnpaired);
        assert!(config.hosts[0].redact_content);
        assert_eq!(
            config.hosts[0].agent_executable(),
            DEFAULT_REMOTE_AGENT_EXECUTABLE
        );
    }

    #[test]
    fn agent_executable_accepts_only_one_shell_safe_command_token() {
        for executable in [
            "codex-usage-monit",
            "~/.local/bin/codex-usage-monit",
            "/opt/codex/bin/codex-usage-monit",
            "C:/Users/dev/bin/codex_usage_monit.exe",
            "agent+debug:1",
        ] {
            validate_agent_executable(executable).unwrap();
        }

        for executable in [
            "",
            "codex usage monit",
            "codex\tusage",
            "codex\nusage",
            "codex;whoami",
            "codex&&whoami",
            "codex|whoami",
            "codex>output",
            "codex<input",
            "codex$(whoami)",
            "codex`whoami`",
            "codex\\usage",
            "codex*",
            "codex?",
            "codex\"usage",
            "codex'usage",
            "codex使用",
        ] {
            assert!(
                validate_agent_executable(executable).is_err(),
                "unsafe executable {executable:?} was accepted"
            );
        }

        assert!(validate_agent_executable(&"a".repeat(MAX_AGENT_EXECUTABLE_BYTES)).is_ok());
        assert!(validate_agent_executable(&"a".repeat(MAX_AGENT_EXECUTABLE_BYTES + 1)).is_err());
    }

    #[test]
    fn host_operations_are_explicit_and_revisioned() {
        let mut config = RemotesConfig::default();
        assert!(config.add_host("dev", "dev-alias").unwrap());
        assert_eq!(config.config_revision, 1);
        assert_eq!(
            config.host("dev").unwrap().state(),
            RemoteHostState::ConfiguredUnpaired
        );
        assert!(!config.auto_sync_enabled);
        assert!(!config.host("dev").unwrap().sync_enabled);
        assert!(config.host("dev").unwrap().redact_content);
        assert_eq!(
            config.host("dev").unwrap().agent_executable(),
            DEFAULT_REMOTE_AGENT_EXECUTABLE
        );

        let error = config.enable_host("dev").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(config.config_revision, 1);

        let paired_source = source(NODE_ONE, 7);
        assert!(config.pair_pin("dev", paired_source.clone()).unwrap());
        assert_eq!(config.config_revision, 2);
        assert_eq!(
            config.host("dev").unwrap().state(),
            RemoteHostState::PairedDisabled
        );
        assert!(!config.pair_pin("dev", paired_source.clone()).unwrap());
        assert_eq!(config.config_revision, 2);
        assert!(!config.host("dev").unwrap().sync_enabled());
        assert_eq!(
            config.host("dev").unwrap().expected_source(),
            Some(&paired_source)
        );
        assert_ne!(
            config.host("dev").unwrap().expected_source(),
            Some(&source(NODE_ONE, 8)),
            "a node-id rotation or exporter generation change must be detectable"
        );

        assert!(config.enable_host("dev").unwrap());
        assert_eq!(config.config_revision, 3);
        assert_eq!(
            config.host("dev").unwrap().state(),
            RemoteHostState::PairedEnabled
        );
        assert!(!config.enable_host("dev").unwrap());
        assert!(!config.auto_sync_enabled);

        assert!(config.set_auto_sync_enabled(true).unwrap());
        assert_eq!(config.config_revision, 4);
        assert!(config.disable_host("dev").unwrap());
        assert_eq!(config.config_revision, 5);
        assert!(config.auto_sync_enabled);
        assert_eq!(
            config.host("dev").unwrap().state(),
            RemoteHostState::PairedDisabled
        );
    }

    #[test]
    fn automatic_hosts_require_both_opt_ins_and_pairing_in_config_order() {
        let mut config = RemotesConfig::default();
        assert!(config.automatic_hosts().next().is_none());

        config.add_host("unpaired", "unpaired-alias").unwrap();
        config.add_host("disabled", "disabled-alias").unwrap();
        config.add_host("first", "first-alias").unwrap();
        config.add_host("second", "second-alias").unwrap();

        config.pair_pin("disabled", source(NODE_ONE, 1)).unwrap();
        config.pair_pin("first", source(NODE_TWO, 2)).unwrap();
        config.enable_host("first").unwrap();
        config
            .pair_pin("second", source("node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 3))
            .unwrap();
        config.enable_host("second").unwrap();

        assert!(
            config.automatic_hosts().next().is_none(),
            "the global switch is fail-closed even for paired, enabled hosts"
        );

        config.set_auto_sync_enabled(true).unwrap();
        assert_eq!(
            config
                .automatic_hosts()
                .map(RemoteHostConfig::id)
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "unpaired and per-host-disabled entries are excluded without reordering"
        );

        config.disable_host("first").unwrap();
        assert_eq!(
            config
                .automatic_hosts()
                .map(RemoteHostConfig::id)
                .collect::<Vec<_>>(),
            vec!["second"]
        );
    }

    #[test]
    fn edit_preserves_pairing_and_enable_state() {
        let mut config = RemotesConfig::default();
        config.add_host("dev", "old-alias").unwrap();
        let paired_source = source(NODE_ONE, 3);
        config.pair_pin("dev", paired_source.clone()).unwrap();
        config.enable_host("dev").unwrap();
        let before = config.config_revision;

        assert!(
            config
                .edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: Some("new-alias".to_owned()),
                        agent_executable: Some("~/.local/bin/codex-usage-monit".to_owned()),
                        redact_content: Some(false),
                    },
                )
                .unwrap()
        );
        let host = config.host("dev").unwrap();
        assert_eq!(host.ssh_host, "new-alias");
        assert_eq!(host.agent_executable(), "~/.local/bin/codex-usage-monit");
        assert_eq!(host.expected_source(), Some(&paired_source));
        assert!(host.sync_enabled);
        assert!(!host.redact_content);
        assert_eq!(config.config_revision, before + 1);

        assert!(
            !config
                .edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: Some("new-alias".to_owned()),
                        agent_executable: Some("~/.local/bin/codex-usage-monit".to_owned()),
                        redact_content: Some(false),
                    },
                )
                .unwrap()
        );
        assert_eq!(config.config_revision, before + 1);
    }

    #[test]
    fn invalid_mutations_leave_the_original_unchanged() {
        let mut config = RemotesConfig::default();
        config.add_host("dev", "dev-alias").unwrap();
        let before = config.clone();

        assert!(
            config
                .edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: Some("-oProxyCommand=bad".to_owned()),
                        agent_executable: None,
                        redact_content: None,
                    },
                )
                .is_err()
        );
        assert_eq!(config, before);

        assert!(config.set_intervals(1, 300).is_err());
        assert_eq!(config, before);

        for agent_executable in ["", "codex usage", "codex;whoami"] {
            assert!(
                config
                    .edit_host(
                        "dev",
                        RemoteHostEdit {
                            agent_executable: Some(agent_executable.to_owned()),
                            ..RemoteHostEdit::default()
                        },
                    )
                    .is_err()
            );
            assert_eq!(config, before);
        }
    }

    #[test]
    fn a_source_generation_cannot_be_silently_replaced_or_shared() {
        let mut config = RemotesConfig::default();
        config.add_host("one", "one-alias").unwrap();
        config.add_host("two", "two-alias").unwrap();
        let original = source(NODE_ONE, 1);
        config.pair_pin("one", original.clone()).unwrap();
        let before = config.clone();

        for replacement in [source(NODE_ONE, 2), source(NODE_TWO, 1)] {
            let error = config.pair_pin("one", replacement).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(config, before);
        }

        for duplicate in [original, source(NODE_ONE, 9)] {
            let error = config.pair_pin("two", duplicate).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(config, before);
        }
    }

    #[test]
    fn rotated_identity_requires_explicit_unpair_before_repairing() {
        let mut config = RemotesConfig::default();
        config.add_host("dev", "dev-alias").unwrap();
        let original = source(NODE_ONE, 1);
        let rotated = source(NODE_TWO, 2);
        config.pair_pin("dev", original.clone()).unwrap();
        config.enable_host("dev").unwrap();
        let before_rejected_pair = config.clone();

        let error = config.pair_pin("dev", rotated.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(config, before_rejected_pair);

        assert!(config.unpair_host("dev").unwrap());
        let unpaired = config.host("dev").unwrap();
        assert_eq!(unpaired.state(), RemoteHostState::ConfiguredUnpaired);
        assert!(!unpaired.sync_enabled());
        assert_eq!(unpaired.expected_source(), None);
        assert_eq!(unpaired.previous_source(), Some(&original));

        assert!(config.pair_pin("dev", rotated.clone()).unwrap());
        let repaired = config.host("dev").unwrap();
        assert_eq!(repaired.state(), RemoteHostState::PairedDisabled);
        assert_eq!(repaired.expected_source(), Some(&rotated));
        assert_eq!(repaired.previous_source(), None);
        assert!(!repaired.sync_enabled());
        assert!(!config.unpair_host("missing").is_ok());
    }

    #[test]
    fn unpair_mutation_is_atomic_revisioned_and_idempotent() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        let paired = store
            .update(
                configured.config_revision(),
                RemotesConfigMutation::pair_pin("dev", source(NODE_ONE, 7)),
            )
            .unwrap();
        let enabled = store
            .update(
                paired.config_revision(),
                RemotesConfigMutation::enable_host("dev"),
            )
            .unwrap();

        let unpaired = store
            .update(
                enabled.config_revision(),
                RemotesConfigMutation::unpair_host("dev"),
            )
            .unwrap();
        assert_eq!(unpaired.config_revision(), enabled.config_revision() + 1);
        assert_eq!(
            unpaired.host("dev").unwrap().state(),
            RemoteHostState::ConfiguredUnpaired
        );
        assert!(!unpaired.host("dev").unwrap().sync_enabled());
        assert_eq!(
            unpaired.host("dev").unwrap().previous_source(),
            Some(&source(NODE_ONE, 7))
        );

        let persisted = store.load().unwrap();
        assert_eq!(persisted, unpaired);
        let json = fs::read_to_string(store.path().unwrap()).unwrap();
        assert!(json.contains("\"previousSource\""));
        assert!(json.contains("\"expectedSource\": null"));

        let no_op = store
            .update(
                unpaired.config_revision(),
                RemotesConfigMutation::unpair_host("dev"),
            )
            .unwrap();
        assert_eq!(no_op, unpaired);
    }

    #[test]
    fn pairing_consumes_detached_source_references_from_every_alias() {
        let mut config = RemotesConfig::default();
        config.add_host("old", "old-alias").unwrap();
        config.add_host("new", "new-alias").unwrap();
        let source = source(NODE_ONE, 7);
        config.pair_pin("old", source.clone()).unwrap();
        config.unpair_host("old").unwrap();
        assert_eq!(config.host("old").unwrap().previous_source(), Some(&source));

        config.pair_pin("new", source.clone()).unwrap();

        assert_eq!(config.host("old").unwrap().previous_source(), None);
        assert_eq!(config.host("new").unwrap().expected_source(), Some(&source));
        assert_eq!(config.host("new").unwrap().previous_source(), None);
        config.validate().unwrap();
    }

    #[test]
    fn removing_a_connection_is_explicit_revisioned_and_releases_its_node_pin() {
        let mut config = RemotesConfig::default();
        config.add_host("old", "old-alias").unwrap();
        let old_source = source(NODE_ONE, 4);
        config.pair_pin("old", old_source.clone()).unwrap();
        config.enable_host("old").unwrap();
        let before_remove = config.config_revision();

        assert!(config.remove_host("old").unwrap());
        assert_eq!(config.config_revision(), before_remove + 1);
        assert!(config.host("old").is_none());
        assert!(!config.auto_sync_enabled());

        let missing = config.remove_host("old").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert_eq!(config.config_revision(), before_remove + 1);

        // Removing the allowlist entry does not reserve its old pin forever;
        // durable source history remains independently keyed by NODE_ONE.
        config.add_host("replacement", "new-alias").unwrap();
        assert!(config.pair_pin("replacement", old_source).unwrap());
    }

    #[test]
    fn remove_host_mutation_round_trips_through_the_public_transaction_api() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();

        let removed = store
            .update(
                configured.config_revision(),
                RemotesConfigMutation::remove_host("dev"),
            )
            .unwrap();
        assert!(removed.hosts().is_empty());
        assert_eq!(store.load().unwrap(), removed);
    }

    #[test]
    fn deserialize_rejects_unknown_unsafe_and_inconsistent_fields() {
        for contents in [
            br#"{"version":1,"unknown":true}"#.as_slice(),
            br#"{"version":1,"hosts":[{"id":"dev","sshHost":"-bad"}]}"#.as_slice(),
            br#"{"version":1,"hosts":[{"id":"../dev","sshHost":"dev"}]}"#.as_slice(),
            br#"{"version":1,"hosts":[{"id":"dev","sshHost":"dev","syncEnabled":true}]}"#
                .as_slice(),
            br#"{"version":1,"hosts":[{"id":"dev","sshHost":"dev","expectedSource":{"nodeId":"node-ABCDEFABCDEFABCDEFABCDEFABCDEFAB","generation":1}}]}"#
                .as_slice(),
            br#"{"version":1,"hosts":[{"id":"dev","sshHost":"dev","expectedSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":0}}]}"#
                .as_slice(),
            br#"{"version":1,"hosts":[{"id":"dev","sshHost":"dev","expectedNodeId":"node-0123456789abcdef0123456789abcdef"}]}"#
                .as_slice(),
            br#"{"version":1,"activeIntervalSeconds":60,"idleIntervalSeconds":30}"#.as_slice(),
        ] {
            let error = deserialize_config(contents).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn deserialize_requires_the_current_version_and_unique_sources() {
        for contents in [
            br#"{}"#.as_slice(),
            br#"{"version":2}"#.as_slice(),
            br#"{
  "version": 1,
  "hosts": [
    {"id":"dev", "sshHost":"one"},
    {"id":"dev", "sshHost":"two"}
  ]
}"#
            .as_slice(),
            br#"{
  "version": 1,
  "hosts": [
    {"id":"one", "sshHost":"one", "expectedSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":1}},
    {"id":"two", "sshHost":"two", "expectedSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":1}}
  ]
}"#
                .as_slice(),
            br#"{
  "version": 1,
  "hosts": [
    {"id":"one", "sshHost":"one", "expectedSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":1}},
    {"id":"two", "sshHost":"two", "expectedSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":2}}
  ]
}"#
                .as_slice(),
            br#"{
  "version": 1,
  "hosts": [
    {"id":"old", "sshHost":"old", "previousSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":1}},
    {"id":"new", "sshHost":"new", "expectedSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":2}}
  ]
}"#
                .as_slice(),
            br#"{
  "version": 1,
  "hosts": [
    {"id":"dev", "sshHost":"dev", "expectedSource":{"nodeId":"node-0123456789abcdef0123456789abcdef","generation":1}, "previousSource":{"nodeId":"node-fedcba9876543210fedcba9876543210","generation":1}}
  ]
}"#
                .as_slice(),
        ] {
            let error = deserialize_config(contents).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn transaction_round_trips_camel_case_and_atomically_replaces() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/remotes.json");
        let store = RemotesConfigStore::new(path.clone());
        let default = store.load_or_create().unwrap();
        assert_eq!(default, RemotesConfig::default());

        let mut transaction = store.begin_transaction().unwrap();
        assert_eq!(transaction.expected_revision(), 0);
        assert!(
            transaction
                .apply(RemotesConfigMutation::add_host("dev", "dev-alias"))
                .unwrap()
        );
        assert!(
            transaction
                .apply(RemotesConfigMutation::edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: Some("dev-renamed".to_owned()),
                        agent_executable: Some("/opt/codex/bin/codex-usage-monit".to_owned()),
                        redact_content: Some(false),
                    },
                ))
                .unwrap()
        );
        assert!(
            transaction
                .apply(RemotesConfigMutation::pair_pin("dev", source(NODE_ONE, 11)))
                .unwrap()
        );
        assert!(
            transaction
                .apply(RemotesConfigMutation::enable_host("dev"))
                .unwrap()
        );
        assert!(
            transaction
                .apply(RemotesConfigMutation::set_auto_sync_enabled(true))
                .unwrap()
        );
        assert!(
            transaction
                .apply(RemotesConfigMutation::set_intervals(120, 600))
                .unwrap()
        );
        assert!(
            transaction
                .apply(RemotesConfigMutation::disable_host("dev"))
                .unwrap()
        );
        let changed = store.commit(transaction).unwrap();
        assert_eq!(store.load().unwrap(), changed);
        assert!(changed.auto_sync_enabled());
        assert_eq!(changed.active_interval_seconds(), 120);
        assert_eq!(changed.idle_interval_seconds(), 600);
        let host = changed.host("dev").unwrap();
        assert_eq!(host.ssh_host(), "dev-renamed");
        assert_eq!(host.agent_executable(), "/opt/codex/bin/codex-usage-monit");
        assert!(!host.redact_content());
        assert!(!host.sync_enabled());
        assert_eq!(host.expected_source(), Some(&source(NODE_ONE, 11)));

        let json = fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"autoSyncEnabled\": true"));
        assert!(json.contains("\"configRevision\": 7"));
        assert!(json.contains("\"sshHost\": \"dev-renamed\""));
        assert!(json.contains("\"agentExecutable\": \"/opt/codex/bin/codex-usage-monit\""));
        assert!(json.contains("\"expectedSource\""));
        assert!(json.contains(&format!("\"nodeId\": \"{NODE_ONE}\"")));
        assert!(json.contains("\"generation\": 11"));
        assert!(!json.contains("expectedNodeId"));
        assert!(!json.contains("auto_sync_enabled"));
        assert_eq!(
            fs::read_dir(directory.path().join("nested"))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn stale_transactions_and_updates_cannot_overwrite_newer_choices() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("private/remotes.json");
        let store = RemotesConfigStore::new(path);

        let mut first = store.begin_transaction().unwrap();
        let mut stale = store.begin_transaction().unwrap();
        first
            .apply(RemotesConfigMutation::add_host("first", "first-alias"))
            .unwrap();
        let committed = store.commit(first).unwrap();
        assert_eq!(committed.config_revision(), 1);

        stale
            .apply(RemotesConfigMutation::add_host("stale", "stale-alias"))
            .unwrap();
        let error = store.commit(stale).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        let error = store
            .update(0, RemotesConfigMutation::set_auto_sync_enabled(true))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        let current = store.load().unwrap();
        assert!(current.host("first").is_some());
        assert!(current.host("stale").is_none());
        assert!(!current.auto_sync_enabled());
        assert_eq!(current.config_revision(), 1);
    }

    #[test]
    fn concurrent_compare_and_swap_has_exactly_one_winner() {
        let directory = tempdir().unwrap();
        let store = Arc::new(RemotesConfigStore::new(
            directory.path().join("private").join(CONFIG_FILE),
        ));
        let barrier = Arc::new(Barrier::new(8));
        let workers = (0..8)
            .map(|index| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    store.update(
                        0,
                        RemotesConfigMutation::add_host(
                            format!("host-{index}"),
                            format!("alias-{index}"),
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();

        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .all(|error| error.kind() == io::ErrorKind::WouldBlock)
        );

        let current = store.load().unwrap();
        assert_eq!(current.config_revision(), 1);
        assert_eq!(current.hosts().len(), 1);
        assert!(!current.auto_sync_enabled());
        assert!(!current.hosts()[0].sync_enabled());
    }

    #[test]
    fn pair_rejects_same_revision_host_rewrite_under_the_write_lock() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "probed-alias"),
            )
            .unwrap();
        let probed_host = configured.host("dev").unwrap().clone();

        // Model a manual/non-cooperating rewrite: the target changes while the
        // writer incorrectly leaves configRevision untouched.
        let mut rewritten = configured.clone();
        rewritten.hosts[0].ssh_host = "replacement-alias".to_owned();
        rewritten.validate().unwrap();
        let path = store.path().unwrap();
        write_private_atomically(path, &serialize_config(&rewritten).unwrap()).unwrap();

        let error = store
            .pair_if_current(
                configured.config_revision(),
                &probed_host,
                source(NODE_ONE, 9),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("changed without a revision"));

        let current = store.load().unwrap();
        let host = current.host("dev").unwrap();
        assert_eq!(host.ssh_host(), "replacement-alias");
        assert_eq!(host.expected_source(), None);
    }

    #[test]
    fn pair_checked_does_not_publish_when_local_source_precondition_fails() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let initial = store.load_or_create().unwrap();
        let configured = store
            .update(
                initial.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        let host = configured.host("dev").unwrap().clone();

        let error = store
            .pair_if_current_checked(
                configured.config_revision(),
                &host,
                source(NODE_ONE, 9),
                || {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "purge pending",
                    ))
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let current = store.load().unwrap();
        assert_eq!(current.config_revision(), configured.config_revision());
        assert_eq!(current.host("dev").unwrap().expected_source(), None);
    }

    #[test]
    fn no_op_commit_still_checks_its_revision() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("private").join(CONFIG_FILE));
        let stale = store.begin_transaction().unwrap();
        store
            .update(0, RemotesConfigMutation::add_host("dev", "dev-alias"))
            .unwrap();
        assert_eq!(
            store.commit(stale).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn single_update_is_revision_checked_and_no_ops_do_not_advance_it() {
        let directory = tempdir().unwrap();
        let store = RemotesConfigStore::new(directory.path().join("private").join(CONFIG_FILE));
        let config = store
            .update(0, RemotesConfigMutation::add_host("dev", "dev-alias"))
            .unwrap();
        assert_eq!(config.config_revision(), 1);
        let config = store
            .update(1, RemotesConfigMutation::disable_host("dev"))
            .unwrap();
        assert_eq!(config.config_revision(), 1);
        assert!(!config.auto_sync_enabled());
    }

    #[test]
    fn load_does_not_rewrite_a_malformed_or_future_config() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("remotes.json");
        let store = RemotesConfigStore::new(path.clone());

        for original in ["not-json", r#"{"version":99}"#] {
            write_private_test_file(&path, original.as_bytes());
            assert!(store.load_or_create().is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn oversized_config_is_rejected_before_deserialization() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        write_private_test_file(&path, &vec![b' '; MAX_CONFIG_FILE_BYTES as usize + 1]);

        let error = RemotesConfigStore::new(path).load().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn path_resolution_honors_overrides_and_platform_fallbacks() {
        let override_directory = Path::new("/override");
        let xdg = Path::new("/xdg-config");
        let home = Path::new("/home/user");
        let local = Path::new("C:/Users/user/AppData/Local");

        assert_eq!(
            resolve_remotes_config_path(
                Some(override_directory),
                Some(xdg),
                Some(home),
                Some(local),
                None,
                Platform::MacOs,
            ),
            Some(override_directory.join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_remotes_config_path(None, Some(xdg), Some(home), None, None, Platform::Unix,),
            Some(xdg.join(APP_DIRECTORY).join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_remotes_config_path(None, None, Some(home), None, None, Platform::MacOs),
            Some(
                home.join("Library/Application Support")
                    .join(APP_DIRECTORY)
                    .join(CONFIG_FILE)
            )
        );
        assert_eq!(
            resolve_remotes_config_path(None, None, Some(home), None, None, Platform::Unix),
            Some(home.join(".config").join(APP_DIRECTORY).join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_remotes_config_path(None, None, None, Some(local), None, Platform::Windows,),
            Some(local.join(APP_DIRECTORY).join(CONFIG_FILE))
        );
        assert_eq!(
            resolve_remotes_config_path(None, None, None, None, None, Platform::Unix),
            None
        );
    }

    #[test]
    fn windows_path_resolution_falls_back_to_user_profile_local_app_data() {
        let user_profile = Path::new("C:/Users/developer");
        assert_eq!(
            resolve_remotes_config_path(
                None,
                None,
                None,
                None,
                Some(user_profile),
                Platform::Windows,
            ),
            Some(
                user_profile
                    .join("AppData")
                    .join("Local")
                    .join(APP_DIRECTORY)
                    .join(CONFIG_FILE)
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_and_replaced_config_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let config_directory = directory.path().join("private");
        let path = config_directory.join(CONFIG_FILE);
        let store = RemotesConfigStore::new(path.clone());
        let config = store.load_or_create().unwrap();

        assert_eq!(
            fs::metadata(&config_directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(config_directory.join(LOCK_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        store
            .update(
                config.config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_config_and_lock_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target.json");
        let path = directory.path().join(CONFIG_FILE);
        write_private_test_file(&target, br#"{"version":1}"#);
        symlink(&target, &path).unwrap();
        assert_eq!(
            RemotesConfigStore::new(path).load().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let second = tempdir().unwrap();
        let lock_target = second.path().join("lock-target");
        write_private_test_file(&lock_target, b"");
        symlink(&lock_target, second.path().join(LOCK_FILE)).unwrap();
        assert_eq!(
            RemotesConfigStore::new(second.path().join(CONFIG_FILE))
                .load_or_create()
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!second.path().join(CONFIG_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn waiter_rejects_a_lock_inode_replaced_while_it_was_blocked() {
        use std::sync::mpsc;
        use std::time::Duration;

        let directory = tempdir().unwrap();
        let config_directory = directory.path().join("private");
        let store = RemotesConfigStore::new(config_directory.join(CONFIG_FILE));
        store.load_or_create().unwrap();

        let holder = open_lock_file(&config_directory).unwrap();
        fs2::FileExt::lock_exclusive(&holder).unwrap();

        let (opened_sender, opened_receiver) = mpsc::channel();
        let waiter_directory = config_directory.clone();
        let waiter = thread::spawn(move || {
            let opened = open_lock_file(&waiter_directory).unwrap();
            opened_sender.send(()).unwrap();
            lock_opened_lock_file(&waiter_directory, opened, LockMode::Shared)
        });
        opened_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter did not open the original lock inode");

        let lock_path = config_directory.join(LOCK_FILE);
        fs::rename(&lock_path, config_directory.join("displaced-remotes.lock")).unwrap();
        write_private_test_file(&lock_path, b"");
        drop(holder);

        let error = waiter.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed while"));

        // A fresh operation coordinates through the replacement and remains
        // usable; only the waiter holding the displaced inode is rejected.
        assert_eq!(store.load().unwrap(), RemotesConfig::default());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_config_is_rejected_without_opening_it_for_a_blocking_read() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let directory = tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        set_mode(directory.path(), 0o700);
        let path_bytes = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path_bytes is a valid NUL-terminated path and mkfifo retains
        // no pointer after returning.
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);

        let error = RemotesConfigStore::new(path).load().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn permissive_config_lock_and_directory_permissions_fail_closed() {
        let config_case = tempdir().unwrap();
        let config_path = config_case.path().join(CONFIG_FILE);
        write_private_test_file(&config_path, br#"{"version":1}"#);
        set_mode(&config_path, 0o644);
        assert_eq!(
            RemotesConfigStore::new(config_path)
                .load()
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        let lock_case = tempdir().unwrap();
        let lock_path = lock_case.path().join(LOCK_FILE);
        write_private_test_file(&lock_path, b"");
        set_mode(&lock_path, 0o644);
        assert_eq!(
            RemotesConfigStore::new(lock_case.path().join(CONFIG_FILE))
                .load_or_create()
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        let directory_case = tempdir().unwrap();
        set_mode(directory_case.path(), 0o755);
        assert_eq!(
            RemotesConfigStore::new(directory_case.path().join(CONFIG_FILE))
                .load_or_create()
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    fn write_private_test_file(path: &Path, contents: &[u8]) {
        let parent = config_parent(path);
        fs::create_dir_all(parent).unwrap();
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            set_mode(parent, 0o700);
            set_mode(path, 0o600);
        }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }
}
