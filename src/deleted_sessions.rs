//! Durable, content-free evidence saved before monit requests session deletion.
//!
//! This database is user data, not reconstructable history. Ordinary history
//! rebuilding and cache garbage collection deliberately never visit it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::CollectConfig;
use crate::domain::{
    AgentInteraction, Confidence, Provenance, RolloutDataset, TaskRecord, TaskStatus, TokenUsage,
    TurnRecord, TurnStatus, UsageCall, terminal_safe_text,
};
use crate::file_lock::FileLock;
use crate::history::{default_history_root, history_namespace};
use crate::private_state_store::{LockFilePolicy, LockMode, PrivateStoreLayout};

const APPLICATION_ID: i64 = 0x4355_4d44;
const SCHEMA_VERSION: i64 = 1;
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const MAX_READ_BYTES: usize = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000;
const MAX_CALLS: usize = 500_000;
const MAX_PENDING_RECEIPTS: usize = 512;

#[derive(Clone, Copy)]
struct LedgerReadBudget {
    records: usize,
    record_bytes: usize,
    total_bytes: usize,
}

const LEDGER_READ_BUDGET: LedgerReadBudget = LedgerReadBudget {
    records: MAX_RECORDS,
    record_bytes: MAX_RECORD_BYTES,
    total_bytes: MAX_READ_BYTES,
};

const LAYOUT: PrivateStoreLayout = PrivateStoreLayout {
    store_name: "deleted-session ledger",
    data_file_name: "ledger.sqlite3",
    data_path_name: "deleted-session ledger database",
    data_subject: "deleted-session ledger database",
    lock_file_name: "ledger.lock",
    lock_subject: "deleted-session ledger lock",
    temporary_subject: "deleted-session ledger temporary file",
    maximum_file_bytes: MAX_READ_BYTES as u64,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeletionStatus {
    Pending,
    Deleted,
    Unknown,
    Partial,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingDeletionReceipt {
    pub(crate) operation_id: String,
    pub(crate) thread_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeletedUsageCall {
    pub timestamp: DateTime<Utc>,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub usage_event_id: Option<String>,
    pub usage_event_identity_exact: bool,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub tokens: TokenUsage,
    pub request_usage_exact: bool,
}

impl From<&UsageCall> for DeletedUsageCall {
    fn from(call: &UsageCall) -> Self {
        Self {
            timestamp: call.timestamp,
            thread_id: call.thread_id.clone(),
            turn_id: call.turn_id.clone(),
            usage_event_id: call.usage_event_id.clone(),
            usage_event_identity_exact: call.usage_event_identity_exact,
            model: call.model.clone(),
            service_tier: call.service_tier.clone(),
            tokens: call.tokens,
            request_usage_exact: call.request_usage_exact,
        }
    }
}

impl DeletedUsageCall {
    fn to_call(&self) -> UsageCall {
        UsageCall {
            timestamp: self.timestamp,
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            usage_event_id: self.usage_event_id.clone(),
            usage_event_identity_exact: self.usage_event_identity_exact,
            model: self.model.clone(),
            service_tier: self.service_tier.clone(),
            tokens: self.tokens,
            request_usage_exact: self.request_usage_exact,
        }
    }

    fn key(&self) -> Result<String> {
        if let Some(id) = &self.usage_event_id {
            return Ok(format!("event:{id}"));
        }
        // The fallback identifies the same retained observation, never proves
        // a native request identity or cross-source equivalence.
        Ok(format!(
            "observation:{:x}",
            Sha256::digest(serde_json::to_vec(self)?)
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeletedTurn {
    pub turn_id: String,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub status: TurnStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeletedSessionRecord {
    pub thread_id: String,
    pub title: String,
    pub parent_thread_id: Option<String>,
    /// Calculation-only project attribution. Deleted-session UI must not
    /// expand this into an ordinary basic-details snapshot.
    #[serde(default, with = "crate::exact_json::optional_pathbuf_lossy")]
    pub cwd: Option<PathBuf>,
    pub source: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub captured_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub status: DeletionStatus,
    pub calls: Vec<DeletedUsageCall>,
    pub turns: Vec<DeletedTurn>,
    pub agent_interactions: Vec<AgentInteraction>,
}

impl DeletedSessionRecord {
    pub(crate) fn from_dataset(
        thread_id: &str,
        title: &str,
        captured_at: DateTime<Utc>,
        dataset: &RolloutDataset,
    ) -> Result<Self> {
        let task = dataset
            .tasks
            .iter()
            .find(|task| task.thread_id == thread_id)
            .context("target session is absent from complete usage capture")?;
        let result = Self {
            thread_id: thread_id.to_owned(),
            title: short_title(title),
            parent_thread_id: task.parent_thread_id.clone(),
            cwd: task.cwd.clone(),
            source: task.source.clone(),
            created_at: task.created_at,
            updated_at: task.updated_at,
            captured_at,
            deleted_at: None,
            status: DeletionStatus::Pending,
            calls: dataset
                .calls
                .iter()
                .filter(|call| call.thread_id == thread_id)
                .map(DeletedUsageCall::from)
                .collect(),
            turns: dataset
                .turns
                .iter()
                .filter(|turn| turn.thread_id == thread_id)
                .map(|turn| DeletedTurn {
                    turn_id: turn.turn_id.clone(),
                    model: turn.model.clone(),
                    service_tier: turn.service_tier.clone(),
                    started_at: turn.started_at,
                    completed_at: turn.completed_at,
                    duration_ms: turn.duration_ms,
                    status: turn.status,
                })
                .collect(),
            agent_interactions: dataset
                .agent_interactions
                .iter()
                .filter(|edge| {
                    edge.parent_thread_id == thread_id || edge.child_thread_id == thread_id
                })
                .cloned()
                .collect(),
        };
        result.validate()?;
        Ok(result)
    }

    #[cfg(test)]
    pub(crate) fn token_usage(&self) -> TokenUsage {
        let mut usage = TokenUsage::default();
        for call in &self.calls {
            usage.add_assign(call.tokens);
        }
        usage
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.thread_id.is_empty() && self.thread_id.len() <= 256,
            "invalid deleted-session thread identity"
        );
        ensure!(
            self.calls.len() <= MAX_CALLS && self.turns.len() <= MAX_CALLS,
            "deleted-session evidence exceeds its record limit"
        );
        ensure!(
            !self
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("remote:")),
            "a local deletion ledger cannot accept a remote source"
        );
        ensure!(
            self.title.len() <= 4096 && !self.title.chars().any(char::is_control),
            "invalid deleted-session short title"
        );
        let mut keys = HashSet::new();
        for call in &self.calls {
            ensure!(
                call.thread_id == self.thread_id && call.timestamp <= self.captured_at,
                "deleted-session call violates its frozen owner or time boundary"
            );
            ensure!(
                keys.insert(call.key()?),
                "duplicate deleted-session usage event"
            );
            for text in [
                call.turn_id.as_deref(),
                call.model.as_deref(),
                call.service_tier.as_deref(),
                call.usage_event_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                ensure!(
                    text.len() <= 4096 && !text.chars().any(char::is_control),
                    "invalid deleted-session calculation identity"
                );
            }
        }
        Ok(())
    }
}

fn short_title(title: &str) -> String {
    let safe = terminal_safe_text(title);
    let normalized = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.graphemes(true).take(80).collect()
}

fn location(config: &CollectConfig) -> Result<(PathBuf, PathBuf, String)> {
    let root = match &config.deleted_sessions_dir {
        Some(root) => root.clone(),
        None => default_history_root()
            .and_then(|path| {
                path.parent()
                    .map(|parent| parent.join("deleted-sessions-v1"))
            })
            .context("cannot resolve the deleted-session ledger state directory")?,
    };
    let root = std::path::absolute(root)?;
    let home = fs::canonicalize(&config.codex_home).or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            std::path::absolute(&config.codex_home)
        } else {
            Err(error)
        }
    })?;
    let namespace = history_namespace(&home);
    #[cfg(unix)]
    let binding = {
        use std::os::unix::ffi::OsStrExt;
        format!("{:x}", Sha256::digest(home.as_os_str().as_bytes()))
    };
    #[cfg(not(unix))]
    let binding = format!("{:x}", Sha256::digest(namespace.as_bytes()));
    Ok((root.clone(), root.join(namespace), binding))
}

/// Serialize whole delete operations without retaining a SQLite transaction
/// across an App Server call. Ledger reads/writes use a distinct short lock.
pub(crate) fn acquire_operation_lock(config: &CollectConfig) -> Result<FileLock> {
    let (root, directory, _) = location(config)?;
    LAYOUT.create_directory_beneath(&root, &directory)?;
    Ok(PrivateStoreLayout {
        lock_file_name: "operation.lock",
        ..LAYOUT
    }
    .open_lock(&directory, LockMode::Exclusive, LockFilePolicy::Create)?)
}

/// Validate existing recovery files before SQLite can consume or overwrite them.
/// Repeat before commit because a transaction can create new side files. The
/// rollback journal must remain removable by SQLite, so no guard survives this
/// check; the enclosing private namespace and ledger lock serialize its owners.
fn validate_side_files(directory: &Path) -> Result<()> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let path = directory.join(format!("{}{suffix}", LAYOUT.data_file_name));
        if !LAYOUT.data_file_exists(&path)? {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Do not open and close another fd for SQLite's files: that can
            // release this process's POSIX locks on an active transaction.
            ensure!(
                fs::symlink_metadata(&path)?.nlink() == 1,
                "deleted-session side files must have exactly one hard link"
            );
        }
        #[cfg(windows)]
        validate_windows_side_file(&path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_side_file(path: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle,
    };

    let open = || {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    };
    let validate = |file: &fs::File| -> Result<BY_HANDLE_FILE_INFORMATION> {
        crate::source_identity::validate_windows_private_file(
            path,
            file,
            "deleted-session database side file",
        )?;
        ensure!(
            file.metadata()?.is_file(),
            "deleted-session side file must be a regular file"
        );
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: file owns the live handle and information is a writable,
        // correctly aligned output. This query does not transfer ownership.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(
            information.nNumberOfLinks == 1,
            "deleted-session side files must have exactly one hard link"
        );
        Ok(information)
    };
    // Deny deletion only while checking both handles. Keeping either handle
    // across SQLite recovery or COMMIT would prevent journal cleanup.
    let guard = open()?;
    let before = validate(&guard)?;
    let current = open()?;
    let after = validate(&current)?;
    ensure!(
        before.dwVolumeSerialNumber == after.dwVolumeSerialNumber
            && before.nFileIndexHigh == after.nFileIndexHigh
            && before.nFileIndexLow == after.nFileIndexLow,
        "deleted-session side file pathname changed"
    );
    Ok(())
}

fn with_database<T>(
    config: &CollectConfig,
    write: bool,
    f: impl FnOnce(&mut Connection) -> Result<T>,
) -> Result<Option<T>> {
    let (root, directory, binding) = location(config)?;
    if write {
        LAYOUT.create_directory_beneath(&root, &directory)?;
    } else if !LAYOUT.directory_exists_beneath(&root, &directory)? {
        return Ok(None);
    }
    // SQLite NOFOLLOW also rejects symlinked ancestors such as macOS /var.
    // Canonicalize only after the private namespace has been validated.
    let directory = fs::canonicalize(directory)?;
    let path = directory.join(LAYOUT.data_file_name);
    if !write && !LAYOUT.data_file_exists(&path)? {
        return Ok(None);
    }
    let _lock = LAYOUT.open_lock(&directory, LockMode::Exclusive, LockFilePolicy::Create)?;
    if write && !LAYOUT.data_file_exists(&path)? {
        LAYOUT.write_atomically(&path, b"")?;
    }
    LAYOUT.data_file_exists(&path)?;
    validate_side_files(&directory)?;
    let before = fs::symlink_metadata(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.nlink() == 1,
            "deleted-session database must have exactly one hard link"
        );
    }
    #[cfg(windows)]
    let identity_guard = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let guard = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        crate::source_identity::validate_windows_private_file(
            &path,
            &guard,
            "deleted-session database",
        )?;
        guard
    };
    // Existing user data may have a hot rollback journal after a crash. Open
    // read/write (never CREATE) even for a logical read so SQLite can recover
    // it under the private lock; a read must never reset or rebuild this file.
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut connection = Connection::open_with_flags(&path, flags)
        .context("could not open deleted-session user-data ledger")?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let validate = |connection: &Connection| -> Result<()> {
        LAYOUT.validate_private_directory(&directory)?;
        LAYOUT.data_file_exists(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current = fs::symlink_metadata(&path)?;
            ensure!(
                (before.dev(), before.ino()) == (current.dev(), current.ino())
                    && current.nlink() == 1,
                "deleted-session database pathname changed"
            );
            let mut moved = 0i32;
            // Inspect SQLite's own descriptor; opening/closing another Unix
            // database fd here would release this process's POSIX locks.
            let result = unsafe {
                rusqlite::ffi::sqlite3_file_control(
                    connection.handle(),
                    c"main".as_ptr(),
                    rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                    (&mut moved as *mut i32).cast(),
                )
            };
            ensure!(
                result == rusqlite::ffi::SQLITE_OK && moved == 0,
                "opened deleted-session database was replaced"
            );
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            crate::source_identity::validate_windows_private_file(
                &path,
                &identity_guard,
                "deleted-session database",
            )?;
            crate::source_history::database::validate_windows_sqlite_handle(
                connection,
                &identity_guard,
            )?;
            let current = fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)?;
            crate::source_identity::validate_windows_private_file(
                &path,
                &current,
                "deleted-session database",
            )?;
            crate::source_history::database::validate_windows_sqlite_handle(connection, &current)?;
        }
        validate_side_files(&directory)?;
        Ok(())
    };
    validate(&connection)?;
    if write {
        let application: i64 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        if application == 0 {
            ensure!(
                before.len() == 0,
                "unrecognized nonempty deleted-session database; it was not modified"
            );
            connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
            let transaction = connection.transaction()?;
            transaction.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE operations (id TEXT PRIMARY KEY, prepared_at TEXT NOT NULL); CREATE TABLE outcomes (operation_id TEXT NOT NULL REFERENCES operations(id), thread_id TEXT NOT NULL, status TEXT NOT NULL, at TEXT NOT NULL, PRIMARY KEY(operation_id,thread_id)); CREATE TABLE sessions (thread_id TEXT PRIMARY KEY, operation_id TEXT NOT NULL REFERENCES operations(id), payload BLOB NOT NULL);")?;
            transaction.execute(
                "INSERT INTO metadata VALUES ('home_binding', ?1)",
                [&binding],
            )?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            validate(&transaction)?;
            transaction.commit()?;
        }
    }
    let application: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        application == APPLICATION_ID && version == SCHEMA_VERSION,
        "deleted-session ledger has an unsupported format; saved usage was not discarded"
    );
    let saved_binding: String = connection.query_row(
        "SELECT value FROM metadata WHERE key='home_binding'",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        binding == saved_binding,
        "deleted-session ledger belongs to another Codex home"
    );
    if write {
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;")?;
    }
    let result = f(&mut connection).and_then(|value| {
        validate(&connection)?;
        if write {
            connection.execute_batch("COMMIT")?;
        }
        Ok(value)
    });
    if result.is_err() && write {
        let _ = connection.execute_batch("ROLLBACK");
    }
    result.map(Some)
}

pub(crate) fn prepare_delete(
    config: &CollectConfig,
    operation_id: &str,
    records: &[DeletedSessionRecord],
) -> Result<()> {
    prepare_delete_with_budget(config, operation_id, records, LEDGER_READ_BUDGET)
}

fn prepare_delete_with_budget(
    config: &CollectConfig,
    operation_id: &str,
    records: &[DeletedSessionRecord],
    budget: LedgerReadBudget,
) -> Result<()> {
    ensure!(
        !operation_id.is_empty() && operation_id.len() <= 256 && !records.is_empty(),
        "invalid deletion operation"
    );
    let mut payloads = Vec::new();
    let mut ids = HashSet::new();
    let mut bytes = 0usize;
    for record in records {
        ensure!(
            ids.insert(&record.thread_id),
            "duplicate member in deletion operation"
        );
        let mut record = record.clone();
        record.status = DeletionStatus::Pending;
        record.deleted_at = None;
        record.title = if config.redact_content {
            "[redacted]".into()
        } else {
            short_title(&record.title)
        };
        record.validate()?;
        let payload = serde_json::to_vec(&record)?;
        bytes = bytes.saturating_add(payload.len());
        ensure!(
            payload.len() <= MAX_RECORD_BYTES && bytes <= MAX_READ_BYTES,
            "deletion evidence exceeds the durable capture budget; no deletion is permitted"
        );
        payloads.push(record);
    }
    with_database(config, true, |connection| {
        let transaction = connection;
        // The ledger lock and BEGIN IMMEDIATE cover both the capacity check
        // and publication. A separate read could admit concurrent operations
        // whose combined unresolved members exceed the reconciliation budget.
        let mut pending_ids = pending_deletion_receipts_in_database(transaction)?
            .into_iter()
            .map(|receipt| receipt.thread_id)
            .collect::<HashSet<_>>();
        pending_ids.extend(payloads.iter().map(|record| record.thread_id.clone()));
        ensure!(
            pending_ids.len() <= MAX_PENDING_RECEIPTS,
            "unresolved deletion member budget exceeded; verify pending results before deleting more sessions"
        );
        transaction.execute(
            "INSERT INTO operations(id,prepared_at) VALUES (?1,?2)",
            params![operation_id, Utc::now().to_rfc3339()],
        )?;
        for mut record in payloads {
            let previous: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT payload FROM sessions WHERE thread_id=?1",
                    [&record.thread_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                ensure!(
                    previous.len() <= MAX_RECORD_BYTES,
                    "saved deletion evidence exceeds its record limit"
                );
                let previous: DeletedSessionRecord = serde_json::from_slice(&previous)?;
                merge_saved_record(&previous, &mut record)?;
            }
            let payload = serde_json::to_vec(&record)?;
            ensure!(
                payload.len() <= MAX_RECORD_BYTES,
                "merged deletion evidence exceeds its durable record budget"
            );
            transaction.execute("INSERT INTO sessions(thread_id,operation_id,payload) VALUES (?1,?2,?3) ON CONFLICT(thread_id) DO UPDATE SET operation_id=excluded.operation_id,payload=excluded.payload", params![record.thread_id,operation_id,payload])?;
            transaction.execute("INSERT INTO outcomes(operation_id,thread_id,status,at) VALUES (?1,?2,'pending',?3)", params![operation_id,record.thread_id,record.captured_at.to_rfc3339()])?;
        }
        validate_ledger_read_budget(transaction, budget)?;
        Ok(())
    })?;
    Ok(())
}

pub(crate) fn set_deletion_outcome(
    config: &CollectConfig,
    operation_id: &str,
    thread_id: &str,
    status: DeletionStatus,
    at: DateTime<Utc>,
) -> Result<()> {
    set_deletion_outcome_with_budget(
        config,
        operation_id,
        thread_id,
        status,
        at,
        LEDGER_READ_BUDGET,
    )
}

fn set_deletion_outcome_with_budget(
    config: &CollectConfig,
    operation_id: &str,
    thread_id: &str,
    status: DeletionStatus,
    at: DateTime<Utc>,
    budget: LedgerReadBudget,
) -> Result<()> {
    with_database(config, true, |connection| {
        let transaction = connection;
        let payload: Vec<u8> = transaction.query_row(
            "SELECT payload FROM sessions WHERE thread_id=?1 AND operation_id=?2",
            params![thread_id, operation_id],
            |row| row.get(0),
        )?;
        ensure!(
            payload.len() <= MAX_RECORD_BYTES,
            "deleted-session record exceeds its read limit"
        );
        let mut record: DeletedSessionRecord = serde_json::from_slice(&payload)?;
        record.status = status;
        record.deleted_at = (status == DeletionStatus::Deleted).then_some(at);
        record.validate()?;
        transaction.execute(
            "UPDATE sessions SET payload=?3 WHERE thread_id=?1 AND operation_id=?2",
            params![thread_id, operation_id, serde_json::to_vec(&record)?],
        )?;
        transaction.execute(
            "UPDATE outcomes SET status=?3,at=?4 WHERE thread_id=?1 AND operation_id=?2",
            params![
                thread_id,
                operation_id,
                serde_json::to_string(&status)?,
                at.to_rfc3339()
            ],
        )?;
        validate_ledger_read_budget(transaction, budget)?;
        Ok(())
    })?;
    Ok(())
}

fn validate_ledger_read_budget(connection: &Connection, budget: LedgerReadBudget) -> Result<()> {
    // Validate the resulting ledger inside the write transaction. Incoming
    // batches alone do not account for old records, merged events, or fields
    // added when recording an outcome after the delete RPC has completed.
    let (records, total_bytes, record_bytes): (i64, i64, i64) = connection.query_row(
        "SELECT count(*),COALESCE(sum(length(payload)),0),COALESCE(max(length(payload)),0) FROM sessions",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let records = usize::try_from(records).context("invalid deletion ledger record count")?;
    let total_bytes = usize::try_from(total_bytes).context("invalid deletion ledger total size")?;
    let record_bytes =
        usize::try_from(record_bytes).context("invalid deletion ledger record size")?;
    ensure!(
        records <= budget.records,
        "deleted-session ledger would exceed its record-count read budget; operation was not committed"
    );
    ensure!(
        record_bytes <= budget.record_bytes,
        "deleted-session ledger would exceed its per-record read budget; operation was not committed"
    );
    ensure!(
        total_bytes <= budget.total_bytes,
        "deleted-session ledger would exceed its total-byte read budget; operation was not committed"
    );
    Ok(())
}

fn merge_saved_record(
    previous: &DeletedSessionRecord,
    incoming: &mut DeletedSessionRecord,
) -> Result<()> {
    previous.validate()?;
    ensure!(
        previous.thread_id == incoming.thread_id,
        "saved deletion owner changed"
    );
    ensure!(
        previous.cwd.is_none() || incoming.cwd.is_none() || previous.cwd == incoming.cwd,
        "restored session changed its project binding; deletion statistics cannot be merged safely"
    );
    let mut calls: HashMap<String, DeletedUsageCall> = previous
        .calls
        .iter()
        .map(|call| Ok((call.key()?, call.clone())))
        .collect::<Result<_>>()?;
    for call in &incoming.calls {
        match calls.get(&call.key()?) {
            Some(previous) => ensure!(
                previous == call,
                "a restored session conflicts with an already saved usage event; deletion refused"
            ),
            None => {
                calls.insert(call.key()?, call.clone());
            }
        }
    }
    incoming.calls = calls.into_values().collect();
    incoming.calls.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.usage_event_id.cmp(&right.usage_event_id))
    });
    for turn in &previous.turns {
        if !incoming
            .turns
            .iter()
            .any(|current| current.turn_id == turn.turn_id)
        {
            incoming.turns.push(turn.clone());
        }
    }
    for edge in &previous.agent_interactions {
        if !incoming.agent_interactions.contains(edge) {
            incoming.agent_interactions.push(edge.clone());
        }
    }
    incoming.captured_at = incoming.captured_at.max(previous.captured_at);
    incoming.created_at = incoming.created_at.or(previous.created_at);
    incoming.updated_at = incoming.updated_at.max(previous.updated_at);
    incoming.cwd = incoming.cwd.clone().or_else(|| previous.cwd.clone());
    incoming.source = incoming.source.clone().or_else(|| previous.source.clone());
    incoming.parent_thread_id = incoming
        .parent_thread_id
        .clone()
        .or_else(|| previous.parent_thread_id.clone());
    incoming.validate()
}

pub(crate) fn load_deleted_sessions(config: &CollectConfig) -> Result<Vec<DeletedSessionRecord>> {
    Ok(with_database(config, false, |connection| {
        let mut statement = connection.prepare("SELECT thread_id,length(payload),payload FROM sessions ORDER BY thread_id")?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            let len = usize::try_from(row.get::<_, i64>(1)?).context("invalid deleted-session evidence length")?;
            bytes = bytes.saturating_add(len);
            ensure!(len <= MAX_RECORD_BYTES && bytes <= MAX_READ_BYTES && records.len() < MAX_RECORDS, "deleted-session ledger exceeds the bounded read limit; statistics are unavailable rather than silently omitted");
            let payload: Vec<u8> = row.get(2)?;
            let mut record: DeletedSessionRecord = serde_json::from_slice(&payload).context("invalid deleted-session usage evidence")?;
            let id: String = row.get(0)?;
            ensure!(record.thread_id == id, "deleted-session ledger identity mismatch");
            record.validate()?;
            if config.redact_content { record.title = "[redacted]".into(); }
            records.push(record);
        }
        Ok(records)
    })?.unwrap_or_default())
}

/// Return only the current operation's unresolved members. Historical receipts
/// are audit evidence, never authority to update a later attempt for the same
/// thread. The caller holds operation.lock across this query and reconciliation.
pub(crate) fn pending_deletion_receipts(
    config: &CollectConfig,
) -> Result<Vec<PendingDeletionReceipt>> {
    Ok(with_database(config, false, pending_deletion_receipts_in_database)?.unwrap_or_default())
}

fn pending_deletion_receipts_in_database(
    connection: &mut Connection,
) -> Result<Vec<PendingDeletionReceipt>> {
    let mut statement = connection.prepare(
        "SELECT s.thread_id,s.operation_id,length(s.payload),s.payload,o.id,r.status \
             FROM sessions s \
             LEFT JOIN operations o ON o.id=s.operation_id \
             LEFT JOIN outcomes r ON r.operation_id=s.operation_id AND r.thread_id=s.thread_id \
             ORDER BY s.thread_id",
    )?;
    let mut rows = statement.query([])?;
    let mut receipts = Vec::new();
    let mut bytes = 0usize;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        let len = usize::try_from(row.get::<_, i64>(2)?)
            .context("invalid pending deletion evidence length")?;
        bytes = bytes.saturating_add(len);
        count += 1;
        ensure!(
            len <= MAX_RECORD_BYTES && bytes <= MAX_READ_BYTES && count <= MAX_RECORDS,
            "pending deletion evidence exceeds the bounded read limit; reconciliation refused"
        );
        let thread_id: String = row.get(0)?;
        let operation_id: String = row.get(1)?;
        ensure!(
            !operation_id.is_empty()
                && operation_id.len() <= 256
                && !operation_id.chars().any(char::is_control),
            "invalid current deletion operation identity"
        );
        let parent: Option<String> = row.get(4)?;
        ensure!(
            parent.as_deref() == Some(operation_id.as_str()),
            "current deletion operation is missing from the ledger"
        );
        let payload: Vec<u8> = row.get(3)?;
        let record: DeletedSessionRecord =
            serde_json::from_slice(&payload).context("invalid pending deletion usage evidence")?;
        record.validate()?;
        ensure!(
            record.thread_id == thread_id,
            "pending deletion thread identity mismatch"
        );
        let outcome: Option<String> = row.get(5)?;
        let outcome = outcome.context("current deletion operation has no member outcome")?;
        // Version 1 prepare stored bare `pending`; outcome updates stored
        // JSON enum strings. Accept both existing representations without
        // rewriting the journal during a read-only reconciliation query.
        let status: DeletionStatus = serde_json::from_str(&outcome)
            .or_else(|_| serde_json::from_value(serde_json::Value::String(outcome.clone())))
            .context("invalid current deletion member outcome")?;
        ensure!(
            status == record.status,
            "current deletion member status disagrees with saved usage evidence"
        );
        if matches!(
            status,
            DeletionStatus::Pending | DeletionStatus::Unknown | DeletionStatus::Partial
        ) {
            ensure!(
                receipts.len() < MAX_PENDING_RECEIPTS,
                "more than 512 pending deletion receipts; reconciliation refused without truncation"
            );
            receipts.push(PendingDeletionReceipt {
                operation_id,
                thread_id,
            });
        }
    }
    Ok(receipts)
}

pub(crate) fn ledger_fingerprint(config: &CollectConfig) -> Result<Option<String>> {
    let (root, directory, _) = location(config)?;
    if !LAYOUT.directory_exists_beneath(&root, &directory)? {
        return Ok(None);
    }
    let path = directory.join(LAYOUT.data_file_name);
    if !LAYOUT.data_file_exists(&path)? {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(&path)?;
    Ok(Some(format!(
        "{}:{}:{:?}",
        path.display(),
        metadata.len(),
        metadata.modified()?
    )))
}

/// Adds saved Own events to the same physical source before normal window,
/// pricing and history computation. The raw rollout cache remains untouched.
pub(crate) fn overlay_deleted_sessions(
    dataset: &mut RolloutDataset,
    records: &[DeletedSessionRecord],
    as_of: DateTime<Utc>,
) -> Result<()> {
    let mut calls = HashMap::new();
    for (index, call) in dataset.calls.iter().enumerate() {
        calls.insert(
            (call.thread_id.clone(), DeletedUsageCall::from(call).key()?),
            index,
        );
    }
    let mut touched = HashSet::new();
    for record in records.iter().filter(|record| record.captured_at <= as_of) {
        touched.insert(record.thread_id.clone());
        for saved in record.calls.iter().filter(|call| call.timestamp <= as_of) {
            let key = (saved.thread_id.clone(), saved.key()?);
            if let Some(index) = calls.get(&key) {
                if DeletedUsageCall::from(&dataset.calls[*index]) != *saved {
                    dataset.stats.skipped_lines = dataset.stats.skipped_lines.saturating_add(1);
                    dataset.warnings.push(format!("saved deletion usage conflicts with a live event for {}; live evidence retained", record.thread_id));
                }
            } else {
                calls.insert(key, dataset.calls.len());
                dataset.calls.push(saved.to_call());
            }
        }
        if !dataset
            .tasks
            .iter()
            .any(|task| task.thread_id == record.thread_id)
        {
            dataset.tasks.push(TaskRecord {
                thread_id: record.thread_id.clone(),
                parent_thread_id: record.parent_thread_id.clone(),
                archived: false,
                title: record.title.clone(),
                cwd: record.cwd.clone(),
                source: record.source.clone(),
                created_at: record.created_at,
                updated_at: record.updated_at,
                status: if record.status == DeletionStatus::Deleted {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Unknown
                },
                status_provenance: Provenance::LocalExact,
                status_confidence: Confidence::High,
                token_usage: TokenUsage::default(),
                turn_count: record.turns.len(),
                window_token_usage: TokenUsage::default(),
                local_token_share_percent: 0.0,
                estimated_quota_percent: 0.0,
                quota_confidence: Confidence::Unknown,
                api_equivalent_cost: None,
            });
        }
        for turn in &record.turns {
            if !dataset.turns.iter().any(|existing| {
                existing.thread_id == record.thread_id && existing.turn_id == turn.turn_id
            }) {
                dataset.turns.push(TurnRecord {
                    thread_id: record.thread_id.clone(),
                    turn_id: turn.turn_id.clone(),
                    model: turn.model.clone(),
                    reasoning_effort: None,
                    service_tier: turn.service_tier.clone(),
                    message_preview: None,
                    started_at: turn.started_at,
                    completed_at: turn.completed_at,
                    duration_ms: turn.duration_ms,
                    status: turn.status,
                    token_usage: TokenUsage::default(),
                    window_token_usage: TokenUsage::default(),
                    local_token_share_percent: 0.0,
                    estimated_quota_percent: 0.0,
                    quota_confidence: Confidence::Unknown,
                    api_equivalent_cost: None,
                });
            }
        }
        for edge in &record.agent_interactions {
            if !dataset.agent_interactions.contains(edge) {
                dataset.agent_interactions.push(edge.clone());
            }
        }
    }
    let mut totals = HashMap::<String, TokenUsage>::new();
    let mut turns = HashMap::<(String, String), TokenUsage>::new();
    for call in dataset
        .calls
        .iter()
        .filter(|call| touched.contains(&call.thread_id))
    {
        totals
            .entry(call.thread_id.clone())
            .or_default()
            .add_assign(call.tokens);
        if let Some(turn) = &call.turn_id {
            turns
                .entry((call.thread_id.clone(), turn.clone()))
                .or_default()
                .add_assign(call.tokens);
        }
    }
    for task in &mut dataset.tasks {
        if touched.contains(&task.thread_id) {
            task.token_usage = totals.get(&task.thread_id).copied().unwrap_or_default();
        }
    }
    for turn in &mut dataset.turns {
        if touched.contains(&turn.thread_id) {
            turn.token_usage = turns
                .get(&(turn.thread_id.clone(), turn.turn_id.clone()))
                .copied()
                .unwrap_or_default();
        }
    }
    dataset.calls.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone};

    mod pending_capacity_tests;
    mod side_file_tests;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).unwrap()
    }

    fn config(directory: &tempfile::TempDir, home: &str) -> CollectConfig {
        let codex_home = directory.path().join(home);
        fs::create_dir_all(&codex_home).unwrap();
        CollectConfig {
            codex_home,
            deleted_sessions_dir: Some(directory.path().join("user-data")),
            offline: true,
            ..CollectConfig::default()
        }
    }

    fn record(id: &str, event: &str, tokens: u64) -> DeletedSessionRecord {
        DeletedSessionRecord {
            thread_id: id.into(),
            title: "Short deleted session".into(),
            parent_thread_id: None,
            cwd: None,
            source: Some("cli".into()),
            created_at: Some(at() - ChronoDuration::days(2)),
            updated_at: Some(at() - ChronoDuration::seconds(1)),
            captured_at: at(),
            deleted_at: None,
            status: DeletionStatus::Pending,
            calls: vec![DeletedUsageCall {
                timestamp: at() - ChronoDuration::hours(6),
                thread_id: id.into(),
                turn_id: Some("turn".into()),
                usage_event_id: Some(event.into()),
                usage_event_identity_exact: true,
                model: Some("gpt-5.6-sol".into()),
                service_tier: None,
                tokens: TokenUsage {
                    input_tokens: tokens,
                    total_tokens: tokens,
                    ..TokenUsage::default()
                },
                request_usage_exact: true,
            }],
            turns: vec![DeletedTurn {
                turn_id: "turn".into(),
                model: Some("gpt-5.6-sol".into()),
                service_tier: None,
                started_at: Some(at() - ChronoDuration::hours(6)),
                completed_at: Some(at() - ChronoDuration::hours(5)),
                duration_ms: Some(3_600_000),
                status: TurnStatus::Completed,
            }],
            agent_interactions: Vec::new(),
        }
    }

    #[test]
    fn deleted_sessions_prepare_is_durable_before_any_outcome_and_survives_history_rebuild() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        assert!(load_deleted_sessions(&config).unwrap().is_empty());
        prepare_delete(&config, "operation", &[record("thread", "event", 42)]).unwrap();
        let pending = load_deleted_sessions(&config).unwrap();
        assert_eq!(pending[0].status, DeletionStatus::Pending);
        assert_eq!(pending[0].token_usage().total_tokens, 42);
        set_deletion_outcome(
            &config,
            "operation",
            "thread",
            DeletionStatus::Deleted,
            at(),
        )
        .unwrap();
        for name in ["history-v2", "rollout-cache"] {
            let root = directory.path().join(name);
            fs::create_dir(&root).unwrap();
            fs::write(root.join("disposable"), b"cache").unwrap();
            fs::remove_dir_all(root).unwrap();
        }
        let restarted = load_deleted_sessions(&config).unwrap();
        assert_eq!(restarted[0].status, DeletionStatus::Deleted);
        assert_eq!(restarted[0].deleted_at, Some(at()));
        assert_eq!(restarted[0].token_usage().total_tokens, 42);
    }

    #[test]
    fn deleted_sessions_repeated_prepare_keeps_old_events_and_operation_outcomes() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        prepare_delete(&config, "first", &[record("thread", "old", 40)]).unwrap();
        set_deletion_outcome(&config, "first", "thread", DeletionStatus::Deleted, at()).unwrap();
        prepare_delete(&config, "second", &[record("thread", "new", 7)]).unwrap();
        let saved = load_deleted_sessions(&config).unwrap();
        assert_eq!(saved[0].token_usage().total_tokens, 47);
        assert_eq!(saved[0].calls.len(), 2);
        with_database(&config, false, |connection| {
            let outcomes: i64 =
                connection.query_row("SELECT count(*) FROM outcomes", [], |row| row.get(0))?;
            assert_eq!(outcomes, 2);
            Ok(())
        })
        .unwrap();
        assert!(prepare_delete(&config, "conflict", &[record("thread", "old", 999)]).is_err());
        assert_eq!(
            load_deleted_sessions(&config).unwrap()[0]
                .token_usage()
                .total_tokens,
            47
        );
        with_database(&config, false, |connection| {
            let operations: i64 =
                connection.query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?;
            assert_eq!(
                operations, 2,
                "the failed prepare rolls back its operation too"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn deleted_sessions_pending_receipts_follow_only_the_current_operation_and_status() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        assert!(pending_deletion_receipts(&config).unwrap().is_empty());
        prepare_delete(&config, "old", &[record("repeated", "old-event", 40)]).unwrap();
        set_deletion_outcome(&config, "old", "repeated", DeletionStatus::Unknown, at()).unwrap();
        prepare_delete(
            &config,
            "current",
            &[
                record("repeated", "new-event", 7),
                record("unknown", "event", 1),
                record("partial", "event", 1),
                record("failed", "event", 1),
                record("deleted", "event", 1),
            ],
        )
        .unwrap();
        for (id, status) in [
            ("unknown", DeletionStatus::Unknown),
            ("partial", DeletionStatus::Partial),
            ("failed", DeletionStatus::Failed),
            ("deleted", DeletionStatus::Deleted),
        ] {
            set_deletion_outcome(&config, "current", id, status, at()).unwrap();
        }
        let receipts = pending_deletion_receipts(&config).unwrap();
        assert_eq!(
            receipts,
            vec![
                PendingDeletionReceipt {
                    operation_id: "current".into(),
                    thread_id: "partial".into()
                },
                PendingDeletionReceipt {
                    operation_id: "current".into(),
                    thread_id: "repeated".into()
                },
                PendingDeletionReceipt {
                    operation_id: "current".into(),
                    thread_id: "unknown".into()
                },
            ]
        );
        assert_eq!(
            load_deleted_sessions(&config)
                .unwrap()
                .iter()
                .find(|record| record.thread_id == "repeated")
                .unwrap()
                .token_usage()
                .total_tokens,
            47
        );
        with_database(&config, false, |connection| {
            let old: String = connection.query_row(
                "SELECT status FROM outcomes WHERE operation_id='old' AND thread_id='repeated'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                serde_json::from_str::<DeletionStatus>(&old)?,
                DeletionStatus::Unknown,
                "reading current receipts must not rewrite historical attempts"
            );
            Ok(())
        })
        .unwrap();
        assert!(
            set_deletion_outcome(&config, "old", "repeated", DeletionStatus::Deleted, at())
                .is_err()
        );
        assert_eq!(pending_deletion_receipts(&config).unwrap(), receipts);
    }

    #[test]
    fn deleted_sessions_pending_receipts_check_member_consistency_and_home_binding() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        let other = super::tests::config(&directory, "other-home");
        prepare_delete(&config, "operation", &[record("thread", "event", 42)]).unwrap();
        set_deletion_outcome(
            &config,
            "operation",
            "thread",
            DeletionStatus::Unknown,
            at(),
        )
        .unwrap();
        assert!(pending_deletion_receipts(&other).unwrap().is_empty());
        with_database(&config, true, |connection| {
            connection.execute(
                "UPDATE outcomes SET status='unknown' WHERE operation_id='operation'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            pending_deletion_receipts(&config).unwrap().len(),
            1,
            "bare and JSON-encoded version-1 status strings remain readable"
        );
        with_database(&config, true, |connection| {
            connection.execute(
                "UPDATE outcomes SET status='failed' WHERE operation_id='operation'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(
            pending_deletion_receipts(&config).is_err(),
            "a mismatched current outcome cannot authorize reconciliation"
        );
        with_database(&config, true, |connection| {
            connection.execute("DELETE FROM outcomes WHERE operation_id='operation'", [])?;
            Ok(())
        })
        .unwrap();
        assert!(
            pending_deletion_receipts(&config).is_err(),
            "an absent current member receipt fails closed"
        );
    }

    #[test]
    fn deleted_sessions_pending_receipts_enforce_the_complete_512_member_budget() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        let records = (0..MAX_PENDING_RECEIPTS)
            .map(|index| record(&format!("thread-{index:03}"), "event", 1))
            .collect::<Vec<_>>();
        prepare_delete(&config, "operation", &records).unwrap();
        assert_eq!(
            pending_deletion_receipts(&config).unwrap().len(),
            MAX_PENDING_RECEIPTS
        );
        // Emulate a ledger written before preparation enforced this bound.
        // Current callers must never publish this unreconcilable state.
        with_database(&config, true, |connection| {
            connection.execute(
                "INSERT INTO operations VALUES ('overflow', ?1)",
                [at().to_rfc3339()],
            )?;
            connection.execute(
                "INSERT INTO sessions VALUES ('extra', 'overflow', ?1)",
                [serde_json::to_vec(&record("extra", "event", 1))?],
            )?;
            connection.execute(
                "INSERT INTO outcomes VALUES ('overflow', 'extra', 'pending', ?1)",
                [at().to_rfc3339()],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(
            pending_deletion_receipts(&config).is_err(),
            "513 pending members must not silently return a truncated list"
        );
        set_deletion_outcome(&config, "overflow", "extra", DeletionStatus::Failed, at()).unwrap();
        assert_eq!(
            pending_deletion_receipts(&config).unwrap().len(),
            MAX_PENDING_RECEIPTS,
            "the pending budget counts unresolved members, not completed or failed history"
        );
    }

    #[test]
    fn deleted_sessions_overlay_deduplicates_same_source_and_preserves_original_time_and_own_usage()
    {
        let parent = record("parent", "parent-event", 10);
        let mut child = record("child", "child-event", 20);
        child.parent_thread_id = Some("parent".into());
        let mut dataset = RolloutDataset::default();
        dataset.calls.push(parent.calls[0].to_call());
        overlay_deleted_sessions(&mut dataset, &[parent.clone(), child.clone()], at()).unwrap();
        overlay_deleted_sessions(&mut dataset, &[parent, child], at()).unwrap();
        assert_eq!(dataset.calls.len(), 2);
        assert_eq!(
            dataset
                .tasks
                .iter()
                .find(|task| task.thread_id == "parent")
                .unwrap()
                .token_usage
                .total_tokens,
            10
        );
        assert_eq!(
            dataset
                .tasks
                .iter()
                .find(|task| task.thread_id == "child")
                .unwrap()
                .token_usage
                .total_tokens,
            20
        );
        assert_eq!(
            dataset
                .calls
                .iter()
                .map(|call| call.tokens.total_tokens)
                .sum::<u64>(),
            30
        );
        assert!(
            dataset
                .calls
                .iter()
                .all(|call| call.timestamp == at() - ChronoDuration::hours(6))
        );
        assert!(
            dataset
                .calls
                .iter()
                .all(|call| call.timestamp < at() - ChronoDuration::hours(5)),
            "deleting now cannot move six-hour-old events into the current five-hour window"
        );
        assert!(
            dataset
                .tasks
                .iter()
                .all(|task| task.source.as_deref() == Some("cli"))
        );
        assert!(
            dataset
                .turns
                .iter()
                .all(|turn| turn.message_preview.is_none())
        );
    }

    #[test]
    fn deleted_sessions_home_binding_and_redaction_do_not_mix_same_uuid() {
        let directory = tempfile::tempdir().unwrap();
        let mut first = config(&directory, "first-home");
        let second = config(&directory, "second-home");
        first.redact_content = true;
        let mut secret = record("same-thread", "event", 19);
        secret.title = "PRIVATE-TITLE-MUST-NOT-BE-SAVED".into();
        prepare_delete(&first, "operation", &[secret]).unwrap();
        assert!(load_deleted_sessions(&second).unwrap().is_empty());
        first.redact_content = false;
        let records = load_deleted_sessions(&first).unwrap();
        assert_eq!(records[0].title, "[redacted]");
        assert_eq!(records[0].token_usage().total_tokens, 19);
        let (_, directory, _) = location(&first).unwrap();
        let bytes = fs::read(directory.join("ledger.sqlite3")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE-TITLE-MUST-NOT-BE-SAVED"));
    }

    #[test]
    fn deleted_sessions_bad_database_fails_closed_without_creating_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        let (root, path, _) = location(&config).unwrap();
        LAYOUT.create_directory_beneath(&root, &path).unwrap();
        let database = path.join("ledger.sqlite3");
        LAYOUT
            .write_atomically(&database, b"not a database")
            .unwrap();
        assert!(load_deleted_sessions(&config).is_err());
        assert!(prepare_delete(&config, "operation", &[record("thread", "event", 1)]).is_err());
        assert_eq!(fs::read(database).unwrap(), b"not a database");
    }

    #[test]
    fn deleted_sessions_logical_read_recovers_a_hot_journal() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        prepare_delete(&config, "operation", &[record("thread", "event", 42)]).unwrap();
        let (_, ledger, _) = location(&config).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "deleted_sessions::tests::deleted_sessions_hot_journal_child",
                "--nocapture",
            ])
            .env(
                "CODEX_MONIT_TEST_CRASH_LEDGER",
                ledger.join("ledger.sqlite3"),
            )
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            ledger.join("ledger.sqlite3-journal").exists(),
            "the child left an uncommitted rollback journal"
        );
        let recovered = load_deleted_sessions(&config).unwrap();
        assert_eq!(recovered[0].token_usage().total_tokens, 42);
    }

    #[test]
    fn deleted_sessions_hot_journal_child() {
        let Some(path) = std::env::var_os("CODEX_MONIT_TEST_CRASH_LEDGER") else {
            return;
        };
        let connection = Connection::open_with_flags(
            fs::canonicalize(PathBuf::from(path)).unwrap(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .unwrap();
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=1; BEGIN IMMEDIATE; UPDATE sessions SET payload=zeroblob(131072);").unwrap();
        // Deliberately skip connection/transaction Drop. The parent verifies
        // recovery of this temporary synthetic database through a logical read.
        std::process::exit(0);
    }
}
