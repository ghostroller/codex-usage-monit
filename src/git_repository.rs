//! Best-effort, content-free Git project evidence.
//!
//! The collector invokes the system `git` executable directly (never through
//! a shell), bounds every invocation, and returns only a SHA-256 fingerprint
//! plus a repository-relative workspace root. Raw remotes and absolute paths
//! never leave this module.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

use crate::remote_protocol::GitRepositoryFingerprint;

const FINGERPRINT_DOMAIN: &[u8] = b"codex-usage-monit/git-remote/v1\0";
const FINGERPRINT_PREFIX: &str = "git-sha256-v1-";
const MAX_GIT_WORKSPACES_PER_COLLECTION: usize = 256;
const MAX_GIT_OUTPUT_BYTES: usize = 16 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_millis(750);
const GIT_COLLECTION_BUDGET: Duration = Duration::from_millis(2_500);
const GIT_EVIDENCE_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const GIT_READER_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const GIT_PROCESS_REAP_TIMEOUT: Duration = Duration::from_millis(100);
const PERSISTENT_CACHE_FORMAT_VERSION: u32 = 1;
const PERSISTENT_CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const PERSISTENT_CACHE_MAX_ENTRIES: usize = 32_768;
const PERSISTENT_CACHE_KEY_DOMAIN: &[u8] = b"codex-usage-monit/git-workspace-cache/v1\0";
const PERSISTENT_CACHE_KEY_PREFIX: &str = "cwd-sha256-v1-";

type GitCommandRunner = fn(&Path, &[&str], Duration) -> io::Result<Vec<u8>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GitEvidenceCollectionStats {
    pub(crate) commands: u64,
    pub(crate) workspaces: u64,
    pub(crate) cache_hits: u64,
    pub(crate) budget_exhausted: u64,
    pub(crate) elapsed_millis: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistentGitEvidenceCache {
    format_version: u32,
    entries: BTreeMap<String, PersistentGitEvidenceEntry>,
}

impl Default for PersistentGitEvidenceCache {
    fn default() -> Self {
        Self {
            format_version: PERSISTENT_CACHE_FORMAT_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistentGitEvidenceEntry {
    checked_at: DateTime<Utc>,
    evidence: PersistentGitEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum PersistentGitEvidence {
    ConfirmedNonRepository,
    Repository {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<GitRepositoryFingerprint>,
        repository_relative_workspace_root: String,
    },
}

struct PersistentGitEvidenceState {
    path: PathBuf,
    cache: PersistentGitEvidenceCache,
    dirty: bool,
    protected_keys: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GitProjectEvidence {
    /// The probe could not produce an authoritative result within its bounds.
    /// Callers must preserve any previously verified evidence.
    Unavailable,
    /// Git and the filesystem authoritatively confirmed that the workspace is
    /// not inside a repository. Callers must clear stale repository evidence.
    ConfirmedNonRepository,
    /// Git found the repository. A missing fingerprint authoritatively means
    /// that the repository currently has no usable `remote.origin.url`.
    Repository {
        fingerprint: Option<GitRepositoryFingerprint>,
        repository_relative_workspace_root: String,
    },
}

#[derive(Clone)]
enum CachedRepositoryFingerprint {
    Present(GitRepositoryFingerprint),
    ConfirmedAbsent,
}

/// Reusable resolver with a fresh budget for each collection. Exact workspace
/// and repository lookups with successful or confirmed-absent results are
/// cached for a short TTL. Transient failures are deduplicated only within the
/// current collection; excess distinct workspaces degrade to missing evidence.
pub(crate) struct GitProjectEvidenceResolver {
    workspace_cache: HashMap<PathBuf, (GitProjectEvidence, Instant)>,
    repository_fingerprints: HashMap<PathBuf, (CachedRepositoryFingerprint, Instant)>,
    collection_workspace_misses: HashSet<PathBuf>,
    inspected_workspaces: usize,
    deadline: Instant,
    command_timeout: Duration,
    runner: GitCommandRunner,
    persistent: Option<PersistentGitEvidenceState>,
    wall_clock: fn() -> DateTime<Utc>,
    collection_started: Instant,
    command_attempts: u64,
    cache_hits: u64,
    budget_exhausted: u64,
}

impl Default for GitProjectEvidenceResolver {
    fn default() -> Self {
        Self {
            workspace_cache: HashMap::new(),
            repository_fingerprints: HashMap::new(),
            collection_workspace_misses: HashSet::new(),
            inspected_workspaces: 0,
            deadline: Instant::now() + GIT_COLLECTION_BUDGET,
            command_timeout: GIT_COMMAND_TIMEOUT,
            runner: bounded_git_output,
            persistent: None,
            wall_clock: Utc::now,
            collection_started: Instant::now(),
            command_attempts: 0,
            cache_hits: 0,
            budget_exhausted: 0,
        }
    }
}

impl GitProjectEvidenceResolver {
    pub(crate) fn begin_collection(&mut self) {
        self.collection_workspace_misses.clear();
        self.inspected_workspaces = 0;
        self.deadline = Instant::now() + GIT_COLLECTION_BUDGET;
        self.collection_started = Instant::now();
        self.command_attempts = 0;
        self.cache_hits = 0;
        self.budget_exhausted = 0;
    }

    pub(crate) fn with_persistent_cache(path: PathBuf) -> Self {
        let cache = read_persistent_git_cache(&path).unwrap_or_default();
        let mut resolver = Self::default();
        resolver.persistent = Some(PersistentGitEvidenceState {
            path,
            cache,
            dirty: false,
            protected_keys: HashSet::new(),
        });
        resolver
    }

    pub(crate) fn collection_stats(&self) -> GitEvidenceCollectionStats {
        GitEvidenceCollectionStats {
            commands: self.command_attempts,
            workspaces: self.inspected_workspaces as u64,
            cache_hits: self.cache_hits,
            budget_exhausted: self.budget_exhausted,
            elapsed_millis: u64::try_from(self.collection_started.elapsed().as_millis())
                .unwrap_or(u64::MAX),
        }
    }

    pub(crate) fn inspect(&mut self, canonical_workspace: &Path) -> GitProjectEvidence {
        let now = Instant::now();
        if let Some((cached, expires_at)) = self.workspace_cache.get(canonical_workspace)
            && *expires_at > now
        {
            self.cache_hits = self.cache_hits.saturating_add(1);
            return cached.clone();
        }
        self.workspace_cache.remove(canonical_workspace);
        if self
            .collection_workspace_misses
            .contains(canonical_workspace)
        {
            return GitProjectEvidence::Unavailable;
        }
        if self.inspected_workspaces >= MAX_GIT_WORKSPACES_PER_COLLECTION {
            self.budget_exhausted = self.budget_exhausted.saturating_add(1);
            return GitProjectEvidence::Unavailable;
        }
        let persistent_key = persistent_workspace_cache_key(canonical_workspace);
        if let Some((evidence, remaining)) = self.fresh_persistent_evidence(&persistent_key) {
            self.cache_hits = self.cache_hits.saturating_add(1);
            self.workspace_cache.insert(
                canonical_workspace.to_path_buf(),
                (evidence.clone(), Instant::now() + remaining),
            );
            return evidence;
        }
        self.inspected_workspaces = self.inspected_workspaces.saturating_add(1);

        let inspection = self.inspect_uncached(canonical_workspace);
        let evidence = match inspection {
            GitInspection::Evidence(evidence) => evidence,
            GitInspection::ConfirmedAbsent => GitProjectEvidence::ConfirmedNonRepository,
            GitInspection::Unavailable => {
                self.collection_workspace_misses
                    .insert(canonical_workspace.to_path_buf());
                return GitProjectEvidence::Unavailable;
            }
            GitInspection::BudgetExhausted => {
                self.budget_exhausted = self.budget_exhausted.saturating_add(1);
                self.collection_workspace_misses
                    .insert(canonical_workspace.to_path_buf());
                return GitProjectEvidence::Unavailable;
            }
        };
        // Successful probes and confirmed non-repositories are cached. A
        // timeout, launch error, permission failure, unexpected exit, or
        // exhausted budget never reaches this insertion and gets a fresh
        // bounded chance in the next collection.
        self.workspace_cache.insert(
            canonical_workspace.to_path_buf(),
            (evidence.clone(), Instant::now() + GIT_EVIDENCE_CACHE_TTL),
        );
        self.remember_persistent_evidence(persistent_key, &evidence);
        evidence
    }

    fn fresh_persistent_evidence(&self, key: &str) -> Option<(GitProjectEvidence, Duration)> {
        let persistent = self.persistent.as_ref()?;
        let entry = persistent.cache.entries.get(key)?;
        let age = (self.wall_clock)().signed_duration_since(entry.checked_at);
        let ttl = ChronoDuration::from_std(GIT_EVIDENCE_CACHE_TTL).ok()?;
        if age < ChronoDuration::zero() || age >= ttl {
            return None;
        }
        let remaining = (ttl - age).to_std().ok()?;
        Some((entry.evidence.clone().into_runtime(), remaining))
    }

    fn remember_persistent_evidence(&mut self, key: String, evidence: &GitProjectEvidence) {
        let Some(persistent_evidence) = PersistentGitEvidence::from_runtime(evidence) else {
            return;
        };
        let Some(persistent) = self.persistent.as_mut() else {
            return;
        };
        persistent.cache.entries.insert(
            key.clone(),
            PersistentGitEvidenceEntry {
                checked_at: (self.wall_clock)(),
                evidence: persistent_evidence,
            },
        );
        persistent.protected_keys.insert(key);
        persistent.dirty = true;
    }

    fn inspect_uncached(&mut self, canonical_workspace: &Path) -> GitInspection {
        if !canonical_workspace.is_absolute() {
            return GitInspection::Unavailable;
        }
        let Some(timeout) = self.next_command_timeout() else {
            return GitInspection::BudgetExhausted;
        };
        self.command_attempts = self.command_attempts.saturating_add(1);
        let root_output = match (self.runner)(
            canonical_workspace,
            &["rev-parse", "--show-toplevel"],
            timeout,
        ) {
            Ok(output) => output,
            Err(error) => {
                return if git_command_exit_code(&error).is_some()
                    && filesystem_confirms_no_git_marker(canonical_workspace).unwrap_or(false)
                {
                    GitInspection::ConfirmedAbsent
                } else {
                    GitInspection::Unavailable
                };
            }
        };
        let Ok(repository_root) = canonicalize_git_path(&root_output) else {
            return GitInspection::Unavailable;
        };
        let Some(relative_root) = repository_relative_root(&repository_root, canonical_workspace)
        else {
            return GitInspection::Unavailable;
        };

        let now = Instant::now();
        let fingerprint = if let Some((cached, expires_at)) =
            self.repository_fingerprints.get(&repository_root)
            && *expires_at > now
        {
            cached.clone()
        } else {
            self.repository_fingerprints.remove(&repository_root);
            let Some(timeout) = self.next_command_timeout() else {
                return GitInspection::BudgetExhausted;
            };
            self.command_attempts = self.command_attempts.saturating_add(1);
            let discovered = match (self.runner)(
                &repository_root,
                &["config", "--local", "--get", "remote.origin.url"],
                timeout,
            ) {
                Ok(output) => single_utf8_line(&output)
                    .and_then(|remote| normalized_git_remote(&remote))
                    .and_then(|remote| fingerprint_git_remote(&remote))
                    .map_or(
                        CachedRepositoryFingerprint::ConfirmedAbsent,
                        CachedRepositoryFingerprint::Present,
                    ),
                Err(error) if git_command_exit_code(&error) == Some(1) => {
                    // `git config --get` exits unsuccessfully when the key is
                    // absent. The repository root was already verified, so
                    // this is an authoritative absence rather than a timeout.
                    CachedRepositoryFingerprint::ConfirmedAbsent
                }
                Err(_) => return GitInspection::Unavailable,
            };
            self.repository_fingerprints.insert(
                repository_root.clone(),
                (discovered.clone(), Instant::now() + GIT_EVIDENCE_CACHE_TTL),
            );
            discovered
        };
        let fingerprint = match fingerprint {
            CachedRepositoryFingerprint::Present(fingerprint) => Some(fingerprint),
            CachedRepositoryFingerprint::ConfirmedAbsent => None,
        };

        GitInspection::Evidence(GitProjectEvidence::Repository {
            fingerprint,
            repository_relative_workspace_root: relative_root,
        })
    }

    fn next_command_timeout(&self) -> Option<Duration> {
        let remaining = self.deadline.checked_duration_since(Instant::now())?;
        (!remaining.is_zero()).then(|| remaining.min(self.command_timeout))
    }

    #[cfg(test)]
    fn with_runner_and_budget(
        runner: GitCommandRunner,
        total_budget: Duration,
        command_timeout: Duration,
    ) -> Self {
        let mut resolver = Self::default();
        resolver.deadline = Instant::now() + total_budget;
        resolver.command_timeout = command_timeout;
        resolver.runner = runner;
        resolver
    }
}

enum GitInspection {
    Evidence(GitProjectEvidence),
    ConfirmedAbsent,
    Unavailable,
    BudgetExhausted,
}

impl PersistentGitEvidence {
    fn from_runtime(evidence: &GitProjectEvidence) -> Option<Self> {
        match evidence {
            GitProjectEvidence::Unavailable => None,
            GitProjectEvidence::ConfirmedNonRepository => Some(Self::ConfirmedNonRepository),
            GitProjectEvidence::Repository {
                fingerprint,
                repository_relative_workspace_root,
            } => Some(Self::Repository {
                fingerprint: fingerprint.clone(),
                repository_relative_workspace_root: repository_relative_workspace_root.clone(),
            }),
        }
    }

    fn into_runtime(self) -> GitProjectEvidence {
        match self {
            Self::ConfirmedNonRepository => GitProjectEvidence::ConfirmedNonRepository,
            Self::Repository {
                fingerprint,
                repository_relative_workspace_root,
            } => GitProjectEvidence::Repository {
                fingerprint,
                repository_relative_workspace_root,
            },
        }
    }

    fn validate(&self) -> io::Result<()> {
        if let Self::Repository {
            repository_relative_workspace_root,
            ..
        } = self
            && !valid_persistent_repository_relative_root(repository_relative_workspace_root)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "persistent Git evidence has an invalid repository-relative root",
            ));
        }
        Ok(())
    }
}

impl Drop for GitProjectEvidenceResolver {
    fn drop(&mut self) {
        let Some(persistent) = self.persistent.as_mut() else {
            return;
        };
        if persistent.dirty && persist_git_evidence_cache(persistent).is_ok() {
            persistent.dirty = false;
        }
    }
}

fn read_persistent_git_cache(path: &Path) -> io::Result<PersistentGitEvidenceCache> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error),
    };
    crate::cache::validate_private_cache_file(path, &file)?;
    if file.metadata()?.len() > PERSISTENT_CACHE_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persistent Git evidence cache exceeds its byte bound",
        ));
    }
    let mut contents = Vec::new();
    file.by_ref()
        .take(PERSISTENT_CACHE_MAX_BYTES.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > PERSISTENT_CACHE_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persistent Git evidence cache exceeds its byte bound",
        ));
    }
    let cache: PersistentGitEvidenceCache = serde_json::from_slice(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    validate_persistent_git_cache(&cache)?;
    Ok(cache)
}

fn validate_persistent_git_cache(cache: &PersistentGitEvidenceCache) -> io::Result<()> {
    if cache.format_version != PERSISTENT_CACHE_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported persistent Git evidence cache version",
        ));
    }
    if cache.entries.len() > PERSISTENT_CACHE_MAX_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persistent Git evidence cache has too many entries",
        ));
    }
    for (key, entry) in &cache.entries {
        let suffix = key
            .strip_prefix(PERSISTENT_CACHE_KEY_PREFIX)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "persistent Git evidence cache key has an invalid prefix",
                )
            })?;
        if suffix.len() != 64
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "persistent Git evidence cache key is invalid",
            ));
        }
        entry.evidence.validate()?;
    }
    Ok(())
}

fn persist_git_evidence_cache(state: &mut PersistentGitEvidenceState) -> io::Result<()> {
    trim_persistent_git_cache(
        &mut state.cache,
        &state.protected_keys,
        PERSISTENT_CACHE_MAX_ENTRIES,
    )?;
    validate_persistent_git_cache(&state.cache)?;
    let encoded = encode_persistent_git_cache_with_limit(
        &mut state.cache,
        &state.protected_keys,
        PERSISTENT_CACHE_MAX_BYTES,
    )?;
    crate::cache::write_private_atomically(&state.path, &encoded)
}

fn trim_persistent_git_cache(
    cache: &mut PersistentGitEvidenceCache,
    protected_keys: &HashSet<String>,
    maximum_entries: usize,
) -> io::Result<()> {
    let remove_count = cache.entries.len().saturating_sub(maximum_entries);
    let oldest = ordered_unprotected_cache_keys(cache, protected_keys);
    if oldest.len() < remove_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "new persistent Git evidence alone exceeds its entry bound",
        ));
    }
    for key in oldest.into_iter().take(remove_count) {
        cache.entries.remove(&key);
    }
    Ok(())
}

fn encode_persistent_git_cache_with_limit(
    cache: &mut PersistentGitEvidenceCache,
    protected_keys: &HashSet<String>,
    maximum_bytes: u64,
) -> io::Result<Vec<u8>> {
    let candidates = ordered_unprotected_cache_keys(cache, protected_keys);
    let mut next_candidate = 0_usize;
    loop {
        let encoded = serde_json::to_vec(&*cache)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let encoded_bytes = encoded.len() as u64;
        if encoded_bytes <= maximum_bytes {
            return Ok(encoded);
        }
        let average_entry_bytes = encoded_bytes
            .checked_div(cache.entries.len().max(1) as u64)
            .unwrap_or(1)
            .max(1);
        let excess = encoded_bytes.saturating_sub(maximum_bytes);
        let wanted = excess
            .saturating_add(average_entry_bytes.saturating_sub(1))
            .checked_div(average_entry_bytes)
            .unwrap_or(1)
            .max(1) as usize;
        let available = candidates.len().saturating_sub(next_candidate);
        let removing = wanted.min(available);
        if removing == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "new persistent Git evidence alone exceeds its byte bound",
            ));
        }
        for key in &candidates[next_candidate..next_candidate + removing] {
            cache.entries.remove(key);
        }
        next_candidate = next_candidate.saturating_add(removing);
    }
}

fn ordered_unprotected_cache_keys(
    cache: &PersistentGitEvidenceCache,
    protected_keys: &HashSet<String>,
) -> Vec<String> {
    let mut entries = cache
        .entries
        .iter()
        .filter(|(key, _)| !protected_keys.contains(*key))
        .map(|(key, entry)| (entry.checked_at, key.clone()))
        .collect::<Vec<_>>();
    entries.sort();
    entries.into_iter().map(|(_, key)| key).collect()
}

fn persistent_workspace_cache_key(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(PERSISTENT_CACHE_KEY_DOMAIN);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = path.as_os_str().as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        digest.update((wide.len() as u64).to_be_bytes());
        for unit in wide {
            digest.update(unit.to_le_bytes());
        }
    }
    format!(
        "{PERSISTENT_CACHE_KEY_PREFIX}{}",
        lower_hex(&digest.finalize())
    )
}

fn valid_persistent_repository_relative_root(root: &str) -> bool {
    root == "."
        || (!root.is_empty()
            && root.len() <= 2 * 1024
            && !root.starts_with('/')
            && !root.contains('\\')
            && !root.chars().any(char::is_control)
            && root
                .split('/')
                .all(|component| !component.is_empty() && !matches!(component, "." | "..")))
}

fn filesystem_confirms_no_git_marker(workspace: &Path) -> io::Result<bool> {
    for ancestor in workspace.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn bounded_git_output(cwd: &Path, arguments: &[&str], timeout: Duration) -> io::Result<Vec<u8>> {
    bounded_command_output(OsStr::new("git"), cwd, arguments, timeout)
}

#[derive(Debug)]
struct GitCommandExit(i32);

impl std::fmt::Display for GitCommandExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "git command exited with status {}", self.0)
    }
}

impl std::error::Error for GitCommandExit {}

fn git_command_exit_code(error: &io::Error) -> Option<i32> {
    error
        .get_ref()?
        .downcast_ref::<GitCommandExit>()
        .map(|status| status.0)
}

fn bounded_command_output(
    program: &OsStr,
    cwd: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("GIT_OPTIONAL_LOCKS", "0");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_SYSTEM",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        command.env_remove(name);
    }
    configure_git_process_tree(&mut command);

    let mut child = command.spawn()?;
    let mut process_tree = match attach_git_process_tree(&mut child) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            kill_and_reap_git_child(&mut child)?;
            return Err(error);
        }
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_and_reap_git_child(&mut process_tree, &mut child)?;
        return Err(io::Error::other("git stdout pipe was not created"));
    };
    #[cfg(unix)]
    {
        bounded_command_output_unix(child, process_tree, stdout, timeout)
    }
    #[cfg(windows)]
    {
        bounded_command_output_windows(child, process_tree, stdout, timeout)
    }
}

#[cfg(unix)]
fn bounded_command_output_unix(
    mut child: Child,
    mut process_tree: GitProcessTree,
    mut stdout: ChildStdout,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    if let Err(error) = set_nonblocking(&stdout) {
        terminate_and_reap_git_child(&mut process_tree, &mut child)?;
        return Err(error);
    }
    let deadline = Instant::now() + timeout;
    let mut output = Vec::with_capacity(MAX_GIT_OUTPUT_BYTES.min(4096));
    let mut stdout_closed = false;
    loop {
        if !stdout_closed {
            stdout_closed = match drain_nonblocking_git_stdout(&mut stdout, &mut output) {
                Ok(closed) => closed,
                Err(error) => {
                    terminate_and_reap_git_child(&mut process_tree, &mut child)?;
                    drop(stdout);
                    return Err(error);
                }
            };
        }
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                terminate_and_reap_git_child(&mut process_tree, &mut child)?;
                return Err(error);
            }
        };
        if let Some(status) = status {
            terminate_and_reap_git_child(&mut process_tree, &mut child)?;
            let drain_deadline = deadline.min(Instant::now() + GIT_READER_DRAIN_TIMEOUT);
            while !stdout_closed && Instant::now() < drain_deadline {
                stdout_closed = match drain_nonblocking_git_stdout(&mut stdout, &mut output) {
                    Ok(closed) => closed,
                    Err(error) => {
                        drop(stdout);
                        return Err(error);
                    }
                };
                if !stdout_closed {
                    thread::sleep(GIT_POLL_INTERVAL);
                }
            }
            if !stdout_closed {
                // An escaped process may still own the write end. Closing our
                // nonblocking read end is sufficient: no reader thread or FD
                // remains inside this process after the bounded drain.
                drop(stdout);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "git stdout remained open after process-tree cleanup",
                ));
            }
            if !status.success() {
                return Err(status.code().map_or_else(
                    || io::Error::other("git command terminated without an exit status"),
                    |code| io::Error::other(GitCommandExit(code)),
                ));
            }
            return Ok(output);
        }
        if Instant::now() >= deadline {
            terminate_and_reap_git_child(&mut process_tree, &mut child)?;
            drop(stdout);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "git command exceeded the collection timeout",
            ));
        }
        thread::sleep(GIT_POLL_INTERVAL);
    }
}

fn terminate_and_reap_git_child(
    process_tree: &mut GitProcessTree,
    child: &mut Child,
) -> io::Result<()> {
    process_tree.terminate(child);
    kill_and_reap_git_child(child)
}

fn kill_and_reap_git_child(child: &mut Child) -> io::Result<()> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => return Err(error),
    }
    let kill_error = child.kill().err();
    let deadline = Instant::now() + GIT_PROCESS_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(GIT_POLL_INTERVAL),
            Ok(None) => {
                return Err(kill_error.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "git primary did not exit within the cleanup bound",
                    )
                }));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn set_nonblocking(stdout: &ChildStdout) -> io::Result<()> {
    let descriptor = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn drain_nonblocking_git_stdout(
    stdout: &mut ChildStdout,
    output: &mut Vec<u8>,
) -> io::Result<bool> {
    let mut buffer = [0_u8; 4096];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                output.extend_from_slice(&buffer[..read]);
                if output.len() > MAX_GIT_OUTPUT_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "git output exceeded the collection limit",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn bounded_command_output_windows(
    mut child: Child,
    mut process_tree: GitProcessTree,
    mut stdout: ChildStdout,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut output = Vec::with_capacity(MAX_GIT_OUTPUT_BYTES.min(4096));
    let mut stdout_closed = false;
    loop {
        if !stdout_closed {
            stdout_closed = match drain_peeked_git_stdout(&mut stdout, &mut output) {
                Ok(closed) => closed,
                Err(error) => {
                    terminate_and_reap_git_child(&mut process_tree, &mut child)?;
                    drop(stdout);
                    return Err(error);
                }
            };
        }

        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                terminate_and_reap_git_child(&mut process_tree, &mut child)?;
                return Err(error);
            }
        };
        if let Some(status) = status {
            // The primary process may exit while a helper it spawned still
            // owns the stdout pipe. Always close the isolated process tree
            // before the final bounded, nonblocking pipe drain.
            terminate_and_reap_git_child(&mut process_tree, &mut child)?;
            let drain_deadline = deadline.min(Instant::now() + GIT_READER_DRAIN_TIMEOUT);
            while !stdout_closed && Instant::now() < drain_deadline {
                stdout_closed = drain_peeked_git_stdout(&mut stdout, &mut output)?;
                if !stdout_closed {
                    thread::sleep(GIT_POLL_INTERVAL);
                }
            }
            if !stdout_closed {
                // A breakaway helper can remain outside the Job object and
                // retain the write end. We own no reader thread: dropping the
                // read handle here leaves no task or descriptor behind.
                drop(stdout);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "git stdout remained open after process-tree cleanup",
                ));
            }
            if !status.success() {
                return Err(status.code().map_or_else(
                    || io::Error::other("git command terminated without an exit status"),
                    |code| io::Error::other(GitCommandExit(code)),
                ));
            }
            return Ok(output);
        }
        if Instant::now() >= deadline {
            terminate_and_reap_git_child(&mut process_tree, &mut child)?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "git command exceeded the collection timeout",
            ));
        }
        thread::sleep(GIT_POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn drain_peeked_git_stdout(stdout: &mut ChildStdout, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0_u8; 4096];
    loop {
        let mut available = 0_u32;
        if unsafe {
            PeekNamedPipe(
                stdout.as_raw_handle().cast(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return if windows_pipe_is_closed(&error) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        if available == 0 {
            return Ok(false);
        }
        if output.len() >= MAX_GIT_OUTPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "git output exceeded the collection limit",
            ));
        }
        let remaining_with_sentinel = MAX_GIT_OUTPUT_BYTES
            .saturating_add(1)
            .saturating_sub(output.len());
        let requested = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(buffer.len())
            .min(remaining_with_sentinel);
        match stdout.read(&mut buffer[..requested]) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                output.extend_from_slice(&buffer[..read]);
                if output.len() > MAX_GIT_OUTPUT_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "git output exceeded the collection limit",
                    ));
                }
            }
            Err(error) if windows_pipe_is_closed(&error) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn windows_pipe_is_closed(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| {
            matches!(
                code,
                ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED
            )
        })
}

#[cfg(unix)]
struct GitProcessTree(Option<libc::pid_t>);

#[cfg(unix)]
fn configure_git_process_tree(command: &mut Command) {
    command.process_group(0);
}

#[cfg(unix)]
fn attach_git_process_tree(child: &mut Child) -> io::Result<GitProcessTree> {
    Ok(GitProcessTree(Some(child.id() as libc::pid_t)))
}

#[cfg(unix)]
impl GitProcessTree {
    fn terminate(&mut self, child: &mut Child) {
        if let Some(process_group) = self.0.take() {
            let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        }
        // The primary can call setsid(2) and leave the process group that was
        // assigned immediately after spawn. Always check and kill the exact
        // child as well so the subsequent reap cannot wait on a live escapee.
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }
}

#[cfg(unix)]
impl Drop for GitProcessTree {
    fn drop(&mut self) {
        if let Some(process_group) = self.0.take() {
            let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
struct GitProcessTree(HANDLE);

#[cfg(windows)]
fn configure_git_process_tree(command: &mut Command) {
    command.creation_flags(CREATE_SUSPENDED);
}

#[cfg(windows)]
fn attach_git_process_tree(child: &mut Child) -> io::Result<GitProcessTree> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(error);
    }
    if unsafe { AssignProcessToJobObject(job, child.as_raw_handle().cast()) } == 0 {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(error);
    }
    let process_tree = GitProcessTree(job);
    if let Err(error) = resume_suspended_git_child(child) {
        drop(process_tree);
        return Err(error);
    }
    Ok(process_tree)
}

#[cfg(windows)]
fn resume_suspended_git_child(child: &Child) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        let mut has_entry = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        let mut resumed = 0_usize;
        while has_entry {
            if entry.th32OwnerProcessID == child.id() {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let result = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if result == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed = resumed.saturating_add(1);
            }
            has_entry = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        if resumed == 0 {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "could not find the suspended Git primary thread",
            ))
        } else {
            Ok(())
        }
    })();
    unsafe { CloseHandle(snapshot) };
    result
}

#[cfg(windows)]
impl GitProcessTree {
    fn terminate(&mut self, child: &mut Child) {
        if !self.0.is_null() {
            unsafe {
                TerminateJobObject(self.0, 1);
                CloseHandle(self.0);
            }
            self.0 = std::ptr::null_mut();
        }
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }
}

#[cfg(windows)]
impl Drop for GitProcessTree {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

#[cfg(unix)]
fn canonicalize_git_path(output: &[u8]) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let bytes = single_line_bytes(output)?;
    let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
    std::fs::canonicalize(path)
}

#[cfg(not(unix))]
fn canonicalize_git_path(output: &[u8]) -> io::Result<PathBuf> {
    let value = single_utf8_line(output)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "git path is not UTF-8"))?;
    std::fs::canonicalize(value)
}

fn single_line_bytes(output: &[u8]) -> io::Result<&[u8]> {
    let output = output.strip_suffix(b"\n").unwrap_or(output);
    let output = output.strip_suffix(b"\r").unwrap_or(output);
    if output.is_empty()
        || output.contains(&b'\n')
        || output.contains(&b'\r')
        || output.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "git returned an invalid single-line value",
        ));
    }
    Ok(output)
}

fn single_utf8_line(output: &[u8]) -> Option<String> {
    std::str::from_utf8(single_line_bytes(output).ok()?)
        .ok()
        .map(str::to_owned)
}

fn repository_relative_root(repository_root: &Path, workspace: &Path) -> Option<String> {
    let relative = workspace.strip_prefix(repository_root).ok()?;
    if relative.as_os_str().is_empty() {
        return Some(".".to_owned());
    }
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return None;
        };
        let component = component.to_str()?;
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        components.push(component);
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn normalized_git_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.is_empty()
        || remote
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return None;
    }
    let cutoff = remote
        .char_indices()
        .filter(|(_, character)| matches!(character, '?' | '#'))
        .map(|(index, _)| index)
        .min()
        .unwrap_or(remote.len());
    let remote = &remote[..cutoff];

    let (scheme, authority, path) = if let Some((scheme, rest)) = remote.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "ssh" | "git" | "http" | "https") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        (Some(scheme), authority, path)
    } else {
        let (authority, path) = split_scp_like_remote(remote)?;
        if authority.contains('/')
            || authority.len() == 1 && authority.as_bytes()[0].is_ascii_alphabetic()
        {
            return None;
        }
        (None, authority, path)
    };
    if scheme.is_none() && path.contains(':') {
        // Unbracketed IPv6 is ambiguous with scp's host:path separator.
        return None;
    }

    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() || authority.contains(char::is_whitespace) {
        return None;
    }
    let mut authority = authority.to_ascii_lowercase();
    match scheme.as_deref() {
        Some("ssh") if authority.ends_with(":22") => authority.truncate(authority.len() - 3),
        Some("https") if authority.ends_with(":443") => authority.truncate(authority.len() - 4),
        Some("http") if authority.ends_with(":80") => authority.truncate(authority.len() - 3),
        Some("git") if authority.ends_with(":9418") => authority.truncate(authority.len() - 5),
        _ => {}
    }
    if !valid_git_authority(&authority) {
        return None;
    }

    let mut path = path.trim_matches('/').to_owned();
    if path.to_ascii_lowercase().ends_with(".git") {
        path.truncate(path.len() - 4);
    }
    path = path.trim_end_matches('/').to_owned();
    if path.is_empty()
        || path.contains(char::is_whitespace)
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return None;
    }
    Some(format!("{authority}/{path}"))
}

fn split_scp_like_remote(remote: &str) -> Option<(&str, &str)> {
    let mut in_brackets = false;
    for (index, character) in remote.char_indices() {
        match character {
            '[' if !in_brackets => in_brackets = true,
            '[' | ']' if character == '[' || !in_brackets => return None,
            ']' => in_brackets = false,
            ':' if !in_brackets => {
                let (authority, path) = remote.split_at(index);
                return Some((authority, path.strip_prefix(':')?));
            }
            _ => {}
        }
    }
    None
}

fn valid_git_authority(authority: &str) -> bool {
    if authority.is_empty()
        || authority.contains(char::is_whitespace)
        || !authority.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        })
    {
        return false;
    }
    if let Some(address) = authority.strip_prefix('[') {
        let Some((address, suffix)) = address.split_once(']') else {
            return false;
        };
        return !address.is_empty()
            && !address.contains(['[', ']'])
            && !suffix.contains(['[', ']'])
            && (suffix.is_empty()
                || suffix.strip_prefix(':').is_some_and(|port| {
                    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
                }));
    }
    if authority.contains(['[', ']']) {
        return false;
    }
    match authority.split_once(':') {
        None => true,
        Some((host, port)) => {
            !host.is_empty()
                && !port.is_empty()
                && !port.contains(':')
                && port.bytes().all(|byte| byte.is_ascii_digit())
        }
    }
}

fn fingerprint_git_remote(normalized_remote: &str) -> Option<GitRepositoryFingerprint> {
    let mut digest = Sha256::new();
    digest.update(FINGERPRINT_DOMAIN);
    digest.update(
        u64::try_from(normalized_remote.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(normalized_remote.as_bytes());
    let value = format!("{FINGERPRINT_PREFIX}{}", lower_hex(&digest.finalize()));
    GitRepositoryFingerprint::from_str(&value).ok()
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    static HUNG_RUNNER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static MISSING_REMOTE_RUNNER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FAILED_REMOTE_RUNNER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static PERSISTENT_RUNNER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static PERSISTENT_RUNNER_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn hung_runner(_cwd: &Path, _arguments: &[&str], timeout: Duration) -> io::Result<Vec<u8>> {
        HUNG_RUNNER_CALLS.fetch_add(1, Ordering::SeqCst);
        thread::sleep(timeout);
        Err(io::Error::new(io::ErrorKind::TimedOut, "fixture timeout"))
    }

    fn missing_remote_runner(
        cwd: &Path,
        arguments: &[&str],
        _timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        MISSING_REMOTE_RUNNER_CALLS.fetch_add(1, Ordering::SeqCst);
        if arguments.first() == Some(&"rev-parse") {
            let mut output = cwd.as_os_str().as_encoded_bytes().to_vec();
            output.push(b'\n');
            Ok(output)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "fixture has no origin remote",
            ))
        }
    }

    fn failed_remote_runner(
        cwd: &Path,
        arguments: &[&str],
        _timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        FAILED_REMOTE_RUNNER_CALLS.fetch_add(1, Ordering::SeqCst);
        if arguments.first() == Some(&"rev-parse") {
            let mut output = cwd.as_os_str().as_encoded_bytes().to_vec();
            output.push(b'\n');
            Ok(output)
        } else {
            Err(io::Error::other(GitCommandExit(128)))
        }
    }

    fn persistent_success_runner(
        cwd: &Path,
        arguments: &[&str],
        _timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        PERSISTENT_RUNNER_CALLS.fetch_add(1, Ordering::SeqCst);
        if arguments.first() == Some(&"rev-parse") {
            let mut output = cwd.as_os_str().as_encoded_bytes().to_vec();
            output.push(b'\n');
            Ok(output)
        } else {
            Ok(b"git@github.com:OpenAI/codex.git\n".to_vec())
        }
    }

    fn immediate_unavailable_runner(
        _cwd: &Path,
        _arguments: &[&str],
        _timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        PERSISTENT_RUNNER_CALLS.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "fixture unavailable",
        ))
    }

    #[test]
    fn common_network_remote_spellings_share_one_fingerprint() {
        let remotes = [
            "git@GitHub.COM:OpenAI/codex.git",
            "ssh://git@github.com:22/OpenAI/codex.git",
            "https://user:secret@github.com:443/OpenAI/codex.git?token=secret#fragment",
            "git://github.com:9418/OpenAI/codex",
        ];
        let normalized = remotes
            .into_iter()
            .map(normalized_git_remote)
            .collect::<Option<Vec<_>>>()
            .unwrap();
        assert!(
            normalized
                .iter()
                .all(|value| value == "github.com/OpenAI/codex")
        );
        let fingerprints = normalized
            .iter()
            .map(|remote| fingerprint_git_remote(remote).unwrap())
            .collect::<Vec<_>>();
        assert!(fingerprints.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn scp_ipv6_and_url_ipv6_spellings_share_one_fingerprint() {
        let remotes = [
            "git@[2001:DB8::1]:OpenAI/codex.git",
            "ssh://git@[2001:db8::1]:22/OpenAI/codex.git",
        ];
        let normalized = remotes
            .into_iter()
            .map(normalized_git_remote)
            .collect::<Option<Vec<_>>>()
            .unwrap();
        assert_eq!(normalized[0], "[2001:db8::1]/OpenAI/codex");
        assert_eq!(normalized[0], normalized[1]);
        assert_eq!(
            fingerprint_git_remote(&normalized[0]),
            fingerprint_git_remote(&normalized[1])
        );
    }

    #[test]
    fn local_ambiguous_and_malformed_remotes_are_not_fingerprinted() {
        for remote in [
            "/srv/repository.git",
            "file:///srv/repository.git",
            "C:\\repository.git",
            "D:/repository.git",
            "git@[2001:db8::1:owner/repository.git",
            "git@2001:db8::1:owner/repository.git",
            "ssh://git@[2001:db8::1/OpenAI/codex.git",
            "ssh://github.com:not-a-port/OpenAI/codex.git",
            "https://github.com/owner/../secret.git",
            "https://github.com/",
            "https://github.com/owner/repo.git\nsecond",
        ] {
            assert_eq!(normalized_git_remote(remote), None, "{remote}");
        }
    }

    #[test]
    fn repository_root_is_represented_without_ambiguity() {
        let root = Path::new("/repo");
        assert_eq!(repository_relative_root(root, root).as_deref(), Some("."));
        assert_eq!(
            repository_relative_root(root, Path::new("/repo/crates/core")).as_deref(),
            Some("crates/core")
        );
        assert_eq!(repository_relative_root(root, Path::new("/other")), None);
    }

    #[test]
    fn verified_repository_without_origin_is_distinct_from_unavailable_probe() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        let mut resolver = GitProjectEvidenceResolver::default();
        resolver.begin_collection();
        let evidence = resolver.inspect(&canonical);
        assert_eq!(
            evidence,
            GitProjectEvidence::Repository {
                fingerprint: None,
                repository_relative_workspace_root: ".".to_owned(),
            }
        );

        std::fs::rename(
            directory.path().join(".git"),
            directory.path().join(".git-hidden"),
        )
        .unwrap();
        resolver.begin_collection();
        assert_eq!(resolver.inspect(&canonical), evidence);
    }

    #[test]
    fn exhausted_collection_budget_prevents_additional_git_spawns() {
        HUNG_RUNNER_CALLS.store(0, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let mut resolver = GitProjectEvidenceResolver::with_runner_and_budget(
            hung_runner,
            Duration::from_millis(45),
            Duration::from_millis(20),
        );
        let started = Instant::now();
        for index in 0..16 {
            assert!(
                resolver.inspect(&directory.path().join(format!("workspace-{index}")))
                    == GitProjectEvidence::Unavailable
            );
        }
        let calls_after_budget = HUNG_RUNNER_CALLS.load(Ordering::SeqCst);
        assert!((2..=3).contains(&calls_after_budget));
        assert!(started.elapsed() < Duration::from_millis(150));

        for index in 16..32 {
            assert!(
                resolver.inspect(&directory.path().join(format!("workspace-{index}")))
                    == GitProjectEvidence::Unavailable
            );
        }
        assert_eq!(
            HUNG_RUNNER_CALLS.load(Ordering::SeqCst),
            calls_after_budget,
            "no git runner may start after the collection deadline"
        );
    }

    #[test]
    fn transient_missing_git_command_is_deduplicated_only_within_one_collection() {
        MISSING_REMOTE_RUNNER_CALLS.store(0, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let mut resolver = GitProjectEvidenceResolver::with_runner_and_budget(
            missing_remote_runner,
            Duration::from_secs(1),
            Duration::from_millis(100),
        );
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::Unavailable
        );
        assert_eq!(MISSING_REMOTE_RUNNER_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::Unavailable
        );
        assert_eq!(MISSING_REMOTE_RUNNER_CALLS.load(Ordering::SeqCst), 2);

        resolver.begin_collection();
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::Unavailable
        );
        assert_eq!(
            MISSING_REMOTE_RUNNER_CALLS.load(Ordering::SeqCst),
            4,
            "a transient launch failure must get a fresh chance next collection"
        );
    }

    #[test]
    fn nonzero_config_failure_is_unavailable_not_confirmed_absent() {
        FAILED_REMOTE_RUNNER_CALLS.store(0, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        let mut resolver = GitProjectEvidenceResolver::with_runner_and_budget(
            failed_remote_runner,
            Duration::from_secs(1),
            Duration::from_millis(100),
        );
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::Unavailable
        );
        assert_eq!(FAILED_REMOTE_RUNNER_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::Unavailable
        );
        assert_eq!(FAILED_REMOTE_RUNNER_CALLS.load(Ordering::SeqCst), 2);

        resolver.begin_collection();
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::Unavailable
        );
        assert_eq!(
            FAILED_REMOTE_RUNNER_CALLS.load(Ordering::SeqCst),
            4,
            "an unexpected nonzero exit must be retried next collection"
        );
    }

    #[test]
    fn confirmed_non_repository_is_cached_across_collection_refreshes() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        let mut resolver = GitProjectEvidenceResolver::default();
        resolver.begin_collection();
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::ConfirmedNonRepository
        );

        // A newly appearing marker remains hidden only for the documented
        // short negative TTL, proving the first result was confirmed/cached.
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        resolver.begin_collection();
        assert_eq!(
            resolver.inspect(&canonical),
            GitProjectEvidence::ConfirmedNonRepository
        );
    }

    #[test]
    fn persistent_cache_reuses_hashed_workspace_evidence_across_resolvers() {
        let _guard = PERSISTENT_RUNNER_TEST_LOCK.lock().unwrap();
        PERSISTENT_RUNNER_CALLS.store(0, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("private-workspace-name");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let cache_path = directory.path().join("state/git-evidence-cache-v1.json");

        let expected = {
            let mut resolver =
                GitProjectEvidenceResolver::with_persistent_cache(cache_path.clone());
            resolver.runner = persistent_success_runner;
            resolver.begin_collection();
            let evidence = resolver.inspect(&workspace);
            assert!(matches!(evidence, GitProjectEvidence::Repository { .. }));
            let stats = resolver.collection_stats();
            assert_eq!(stats.commands, 2);
            assert_eq!(stats.workspaces, 1);
            evidence
        };
        let encoded = std::fs::read_to_string(&cache_path).unwrap();
        assert!(!encoded.contains("private-workspace-name"));
        assert!(!encoded.contains(workspace.to_string_lossy().as_ref()));

        let mut rebuilt = GitProjectEvidenceResolver::with_persistent_cache(cache_path);
        rebuilt.runner = persistent_success_runner;
        rebuilt.begin_collection();
        assert_eq!(rebuilt.inspect(&workspace), expected);
        let stats = rebuilt.collection_stats();
        assert_eq!(stats.commands, 0);
        assert_eq!(stats.workspaces, 0);
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(PERSISTENT_RUNNER_CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn persistent_cache_allows_work_after_the_per_collection_workspace_cap() {
        let _guard = PERSISTENT_RUNNER_TEST_LOCK.lock().unwrap();
        PERSISTENT_RUNNER_CALLS.store(0, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let cache_path = directory.path().join("state/git-evidence-cache-v1.json");
        let mut workspaces = Vec::new();
        for index in 0..=MAX_GIT_WORKSPACES_PER_COLLECTION {
            let path = directory.path().join(format!("workspace-{index:03}"));
            std::fs::create_dir(&path).unwrap();
            workspaces.push(std::fs::canonicalize(path).unwrap());
        }
        {
            let mut resolver =
                GitProjectEvidenceResolver::with_persistent_cache(cache_path.clone());
            resolver.runner = persistent_success_runner;
            resolver.begin_collection();
            for workspace in &workspaces {
                let _ = resolver.inspect(workspace);
            }
            assert_eq!(
                resolver.inspect(workspaces.last().unwrap()),
                GitProjectEvidence::Unavailable
            );
        }

        let mut rebuilt = GitProjectEvidenceResolver::with_persistent_cache(cache_path);
        rebuilt.runner = persistent_success_runner;
        rebuilt.begin_collection();
        for workspace in &workspaces[..MAX_GIT_WORKSPACES_PER_COLLECTION] {
            assert!(matches!(
                rebuilt.inspect(workspace),
                GitProjectEvidence::Repository { .. }
            ));
        }
        assert!(matches!(
            rebuilt.inspect(workspaces.last().unwrap()),
            GitProjectEvidence::Repository { .. }
        ));
        let stats = rebuilt.collection_stats();
        assert_eq!(stats.cache_hits, MAX_GIT_WORKSPACES_PER_COLLECTION as u64);
        assert_eq!(stats.workspaces, 1);
        assert_eq!(stats.commands, 2);
    }

    #[test]
    fn expired_probe_failure_preserves_persistent_evidence_for_retry() {
        let _guard = PERSISTENT_RUNNER_TEST_LOCK.lock().unwrap();
        PERSISTENT_RUNNER_CALLS.store(0, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let cache_path = directory.path().join("state/git-evidence-cache-v1.json");
        {
            let mut resolver =
                GitProjectEvidenceResolver::with_persistent_cache(cache_path.clone());
            resolver.runner = persistent_success_runner;
            resolver.begin_collection();
            assert!(matches!(
                resolver.inspect(&workspace),
                GitProjectEvidence::Repository { .. }
            ));
        }
        let mut cache = read_persistent_git_cache(&cache_path).unwrap();
        let entry = cache.entries.values_mut().next().unwrap();
        entry.checked_at = Utc::now() - ChronoDuration::minutes(6);
        crate::cache::write_private_atomically(&cache_path, &serde_json::to_vec(&cache).unwrap())
            .unwrap();
        let before = std::fs::read(&cache_path).unwrap();

        {
            let mut resolver =
                GitProjectEvidenceResolver::with_persistent_cache(cache_path.clone());
            resolver.runner = immediate_unavailable_runner;
            resolver.begin_collection();
            assert_eq!(
                resolver.inspect(&workspace),
                GitProjectEvidence::Unavailable
            );
        }
        assert_eq!(std::fs::read(&cache_path).unwrap(), before);
    }

    #[test]
    fn persistent_cache_rejects_expired_and_future_checked_at_values() {
        let _guard = PERSISTENT_RUNNER_TEST_LOCK.lock().unwrap();
        PERSISTENT_RUNNER_CALLS.store(0, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let cache_path = directory.path().join("state/git-evidence-cache-v1.json");
        let mut paths = Vec::new();
        for name in ["fresh", "expired", "future"] {
            let path = directory.path().join(name);
            std::fs::create_dir(&path).unwrap();
            paths.push(std::fs::canonicalize(path).unwrap());
        }
        let now = Utc::now();
        let evidence = PersistentGitEvidence::ConfirmedNonRepository;
        let cache = PersistentGitEvidenceCache {
            format_version: PERSISTENT_CACHE_FORMAT_VERSION,
            entries: BTreeMap::from([
                (
                    persistent_workspace_cache_key(&paths[0]),
                    PersistentGitEvidenceEntry {
                        checked_at: now,
                        evidence: evidence.clone(),
                    },
                ),
                (
                    persistent_workspace_cache_key(&paths[1]),
                    PersistentGitEvidenceEntry {
                        checked_at: now - ChronoDuration::minutes(6),
                        evidence: evidence.clone(),
                    },
                ),
                (
                    persistent_workspace_cache_key(&paths[2]),
                    PersistentGitEvidenceEntry {
                        checked_at: now + ChronoDuration::hours(1),
                        evidence,
                    },
                ),
            ]),
        };
        crate::cache::write_private_atomically(&cache_path, &serde_json::to_vec(&cache).unwrap())
            .unwrap();

        let mut resolver = GitProjectEvidenceResolver::with_persistent_cache(cache_path);
        resolver.runner = immediate_unavailable_runner;
        resolver.begin_collection();
        assert_eq!(
            resolver.inspect(&paths[0]),
            GitProjectEvidence::ConfirmedNonRepository
        );
        assert_eq!(resolver.inspect(&paths[1]), GitProjectEvidence::Unavailable);
        assert_eq!(resolver.inspect(&paths[2]), GitProjectEvidence::Unavailable);
        let stats = resolver.collection_stats();
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(stats.commands, 2);
    }

    #[test]
    fn persistent_cache_evicts_oldest_unprotected_entries_to_fit_byte_bound() {
        let key = |digit: char| {
            format!(
                "{PERSISTENT_CACHE_KEY_PREFIX}{}",
                digit.to_string().repeat(64)
            )
        };
        let oldest = key('1');
        let middle = key('2');
        let newly_checked_after_clock_rollback = key('3');
        let entry = |checked_at| PersistentGitEvidenceEntry {
            checked_at,
            evidence: PersistentGitEvidence::Repository {
                fingerprint: None,
                repository_relative_workspace_root: "segment/".to_owned() + &"x".repeat(256),
            },
        };
        let mut cache = PersistentGitEvidenceCache {
            format_version: PERSISTENT_CACHE_FORMAT_VERSION,
            entries: BTreeMap::from([
                (oldest.clone(), entry(Utc::now() - ChronoDuration::hours(2))),
                (middle.clone(), entry(Utc::now() - ChronoDuration::hours(1))),
                (
                    newly_checked_after_clock_rollback.clone(),
                    entry(Utc::now() - ChronoDuration::hours(3)),
                ),
            ]),
        };
        let mut two_entries = cache.clone();
        two_entries.entries.remove(&oldest);
        let maximum = serde_json::to_vec(&two_entries).unwrap().len() as u64;
        let encoded = encode_persistent_git_cache_with_limit(
            &mut cache,
            &HashSet::from([newly_checked_after_clock_rollback.clone()]),
            maximum,
        )
        .unwrap();
        assert_eq!(encoded.len() as u64, maximum);
        assert!(!cache.entries.contains_key(&oldest));
        assert!(cache.entries.contains_key(&middle));
        assert!(
            cache
                .entries
                .contains_key(&newly_checked_after_clock_rollback)
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_descendants_that_inherit_stdout() {
        let directory = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let error = bounded_command_output(
            OsStr::new("sh"),
            directory.path(),
            &["-c", "sleep 5 & printf ready; wait"],
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn escaped_session_holding_stdout_does_not_leak_a_reader() {
        if !Command::new("python3")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let script = concat!(
            "import os,time\n",
            "pid=os.fork()\n",
            "if pid == 0:\n",
            " os.setsid()\n",
            " time.sleep(0.4)\n",
            " os._exit(0)\n",
            "print('ready', end='', flush=True)\n",
        );
        let started = Instant::now();
        let error = bounded_command_output(
            OsStr::new("python3"),
            directory.path(),
            &["-c", script],
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    #[cfg(unix)]
    #[test]
    fn missing_process_group_still_kills_and_reaps_the_exact_primary() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let mut process_tree = GitProcessTree(Some(libc::pid_t::MAX));
        let started = Instant::now();
        terminate_and_reap_git_child(&mut process_tree, &mut child).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[cfg(windows)]
    #[test]
    fn inherited_stdout_after_primary_exit_never_leaves_a_blocked_reader() {
        let directory = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let result = bounded_command_output(
            OsStr::new("cmd.exe"),
            directory.path(),
            &["/D", "/C", "start \"\" /B ping -n 3 127.0.0.1 >NUL"],
            Duration::from_millis(250),
        );
        assert!(
            result.is_ok()
                || result
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_stdout_is_terminated_and_reaped_on_every_attempt() {
        let directory = tempfile::tempdir().unwrap();
        for attempt in 0..3 {
            let pid_path = directory.path().join(format!("pid-{attempt}"));
            let pid_path_text = pid_path.to_string_lossy().into_owned();
            let error = bounded_command_output(
                OsStr::new("sh"),
                directory.path(),
                &[
                    "-c",
                    "echo $$ > \"$0\"; yes x | head -c 20000; wait",
                    &pid_path_text,
                ],
                Duration::from_millis(500),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            let pid: libc::pid_t = std::fs::read_to_string(pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }
    }
}
