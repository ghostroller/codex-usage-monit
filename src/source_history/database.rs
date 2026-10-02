//! SQLite transaction and file-safety boundary for source-aware history.
//!
//! Payloads are individual validated business records, never legacy shards.
//! A thread-local connection scope lets the existing typed facade participate
//! in one read snapshot or one multi-family transaction without owning a
//! process-wide connection or extending a write transaction across SSH.

use std::cell::RefCell;
use std::fs;
#[cfg(any(unix, windows))]
use std::fs::File;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, params};
#[cfg(unix)]
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{HistoryProfileId, SourceHistoryReadBudget, invalid_data};

const DATABASE_FILE: &str = "history.sqlite3";
const APPLICATION_ID: i64 = 0x4355_4d48;
// Version 2 establishes rebuild-only history semantics. Preview version 1
// databases may contain imported legacy usage and must never be adopted.
const SCHEMA_VERSION: i64 = 2;
const BUSY_WAIT: Duration = Duration::from_millis(250);
const MAX_VALUE_BYTES: usize = 128 * 1024 * 1024;
static SAVEPOINT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static SCOPES: RefCell<Vec<Scope>> = const { RefCell::new(Vec::new()) };
}

struct Scope {
    path: PathBuf,
    opened: Rc<OpenedDatabase>,
    writable: bool,
}

// Unix must never open another fd for an active SQLite inode: closing that
// fd releases this process's POSIX locks, including other threads' locks.
// SQLite validates its own fd with HAS_MOVED. Windows handle locks do not
// have that close-any-fd behavior, so an identity guard is safe there.
struct OpenedDatabase {
    // Drop side guards before SQLite closes the connection and removes its
    // WAL/SHM. They deny DELETE sharing only while this connection is alive.
    #[cfg(windows)]
    side_file_guards: RefCell<Vec<(PathBuf, File)>>,
    connection: Connection,
    #[cfg(windows)]
    identity_guard: File,
    #[cfg(unix)]
    identity: (u64, u64),
    path: PathBuf,
    state_root: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct HistoryDatabase {
    state_root: PathBuf,
    profile_root: PathBuf,
    profile: HistoryProfileId,
    path: PathBuf,
}

impl HistoryDatabase {
    pub(crate) fn new(state_root: &Path, profile: &HistoryProfileId) -> Self {
        let profile_root = state_root.join("history-v2").join(profile.as_str());
        Self {
            state_root: state_root.to_owned(),
            path: profile_root.join(DATABASE_FILE),
            profile_root,
            profile: profile.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn is_transaction_active(&self) -> bool {
        SCOPES.with(|scopes| scopes.borrow().iter().any(|scope| scope.path == self.path))
    }

    pub(crate) fn exists(&self) -> io::Result<bool> {
        if !super::private_directory_exists_beneath(&self.state_root, &self.profile_root)? {
            return Ok(false);
        }
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                validate_file(&self.path, &metadata)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Converts a facade's old logical namespace to a database key. It does
    /// not access or create the old family directory or state file.
    pub(crate) fn namespace(&self, path: &Path) -> io::Result<String> {
        let relative = path.strip_prefix(&self.profile_root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "history database namespace is outside its profile",
            )
        })?;
        let mut parts = Vec::new();
        for component in relative.components() {
            let Component::Normal(part) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid history database namespace",
                ));
            };
            let part = part
                .to_str()
                .ok_or_else(|| invalid_data("history database namespace is not UTF-8"))?;
            parts.push(part);
        }
        if parts.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty history database namespace",
            ));
        }
        Ok(parts.join("/"))
    }

    pub(crate) fn read<T>(
        &self,
        operation: impl FnOnce(&Connection) -> io::Result<T>,
    ) -> io::Result<T> {
        self.transact(false, false, operation)
    }

    pub(crate) fn write<T>(
        &self,
        operation: impl FnOnce(&Connection) -> io::Result<T>,
    ) -> io::Result<T> {
        self.transact(true, false, operation)
    }

    /// Config-fenced publication must not wait behind an unrelated writer.
    pub(crate) fn write_nowait<T>(
        &self,
        operation: impl FnOnce(&Connection) -> io::Result<T>,
    ) -> io::Result<T> {
        self.transact(true, true, operation)
    }

    fn transact<T>(
        &self,
        writable: bool,
        nowait: bool,
        operation: impl FnOnce(&Connection) -> io::Result<T>,
    ) -> io::Result<T> {
        let scoped = SCOPES.with(|scopes| {
            scopes
                .borrow()
                .iter()
                .rev()
                .find(|scope| scope.path == self.path)
                .map(|scope| (Rc::clone(&scope.opened), scope.writable))
        });
        if let Some((opened, scope_writable)) = scoped {
            opened.validate()?;
            if writable && !scope_writable {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot write inside a history read snapshot",
                ));
            }
            if !writable {
                return operation(&opened.connection);
            }
            let name = format!(
                "history_{}",
                SAVEPOINT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            opened
                .connection
                .execute_batch(&format!("SAVEPOINT {name}"))
                .map_err(sql_error)?;
            let mut transaction = TransactionGuard {
                connection: &opened.connection,
                savepoint: Some(name),
                completed: false,
            };
            let value = operation(&opened.connection)?;
            opened.validate()?;
            transaction.commit()?;
            return Ok(value);
        }

        let opened = Rc::new(self.open(writable, nowait)?);
        opened
            .connection
            .busy_timeout(if nowait { Duration::ZERO } else { BUSY_WAIT })
            .map_err(sql_error)?;
        opened
            .connection
            .execute_batch(if writable {
                "BEGIN IMMEDIATE"
            } else {
                "BEGIN DEFERRED"
            })
            .map_err(sql_error)?;
        let mut transaction = TransactionGuard {
            connection: &opened.connection,
            savepoint: None,
            completed: false,
        };
        // BEGIN may be the first operation that opens WAL/SHM. Bind those
        // objects before application code is allowed to use this snapshot.
        opened.validate()?;
        SCOPES.with(|scopes| {
            scopes.borrow_mut().push(Scope {
                path: self.path.clone(),
                opened: Rc::clone(&opened),
                writable,
            })
        });
        let _scope = ScopeGuard {
            path: self.path.clone(),
        };
        let value = operation(&opened.connection)?;
        opened.validate()?;
        transaction.commit()?;
        Ok(value)
    }

    fn open(&self, writable: bool, nowait: bool) -> io::Result<OpenedDatabase> {
        if writable {
            super::create_private_directory_beneath(&self.state_root, &self.profile_root)?;
        } else {
            super::validate_private_directory_beneath(&self.state_root, &self.profile_root)?;
        }
        // Only canonicalize the already-validated trusted root. Namespace
        // symlinks beneath it have been rejected by the directory walk.
        let canonical_root = fs::canonicalize(&self.state_root)?;
        let relative = self
            .profile_root
            .strip_prefix(&self.state_root)
            .map_err(|_| invalid_data("history database root changed"))?;
        let path = canonical_root.join(relative).join(DATABASE_FILE);
        validate_side_files(&path)?;

        if writable
            && fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            self.refuse_recreating_active_database()?;
            create_closed_database_file(&path)?;
        }
        #[cfg(unix)]
        if writable {
            self.recover_empty_creation_link(&path)?;
        }
        let metadata = fs::symlink_metadata(&path)?;
        validate_file(&path, &metadata)?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(windows)]
        let identity_guard = {
            let mut options = OpenOptions::new();
            options.read(true).write(writable);
            super::add_nofollow_flags(&mut options);
            options.open(&path)?
        };
        let flags = (if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        }) | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        #[cfg(windows)]
        let side_file_guards = RefCell::new(pin_windows_side_files(&path)?);
        let connection = Connection::open_with_flags(&path, flags).map_err(sql_error)?;
        let opened = OpenedDatabase {
            #[cfg(windows)]
            side_file_guards,
            connection,
            #[cfg(windows)]
            identity_guard,
            #[cfg(unix)]
            identity,
            path,
            state_root: canonical_root,
        };
        opened.validate()?;
        opened
            .connection
            .busy_timeout(if nowait { Duration::ZERO } else { BUSY_WAIT })
            .map_err(sql_error)?;
        opened
            .connection
            .execute_batch(
                "PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;",
            )
            .map_err(sql_error)?;
        opened
            .connection
            .set_limit(
                rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
                MAX_VALUE_BYTES as i32,
            )
            .map_err(sql_error)?;
        if writable {
            initialize_schema(&opened.connection, self)?;
            // FULL is chosen deliberately; performance comparisons may not
            // weaken the old persistence target to manufacture a gain.
            opened.connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=256;").map_err(sql_error)?;
        } else {
            validate_schema(&opened.connection, &self.profile)?;
            opened
                .connection
                .execute_batch("PRAGMA query_only=ON;")
                .map_err(sql_error)?;
        }
        opened.validate()?;
        Ok(opened)
    }

    fn refuse_recreating_active_database(&self) -> io::Result<()> {
        use super::RedactionProfile;
        use crate::history_ownership::{
            HistoryOwnershipState, HistoryOwnershipStore, OwnershipManifestStatus,
        };
        for redaction in [RedactionProfile::Redacted, RedactionProfile::PreviewEnabled] {
            let owner = HistoryOwnershipStore::new(
                self.state_root.clone(),
                self.profile.clone(),
                redaction,
            );
            if let OwnershipManifestStatus::Initialized(manifest) = owner.load_manifest()?
                && manifest.is_sqlite_backend()
                && manifest.state() == HistoryOwnershipState::V2Active
            {
                return Err(invalid_data(
                    "active history database is missing; refusing to recreate it from an old backup",
                ));
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn recover_empty_creation_link(&self, path: &Path) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.nlink() != 2 || metadata.len() != 0 {
            return Ok(());
        }
        self.refuse_recreating_active_database()?;
        super::validate_data_file_metadata(path, &metadata)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid_data("missing database parent"))?;
        let mut candidates = 0;
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(identity) = name
                .strip_prefix(".history.sqlite3.")
                .and_then(|value| value.strip_suffix(".tmp"))
            else {
                continue;
            };
            let parts = identity.split('.').collect::<Vec<_>>();
            if parts.len() != 2
                || parts
                    .iter()
                    .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
            {
                continue;
            }
            candidates += 1;
            if candidates > 32 {
                return Err(invalid_data(
                    "history database creation recovery exceeds its candidate budget",
                ));
            }
            let temporary = entry.path();
            let candidate = fs::symlink_metadata(&temporary)?;
            if candidate.dev() != metadata.dev() || candidate.ino() != metadata.ino() {
                continue;
            }
            super::validate_data_file_metadata(&temporary, &candidate)?;
            // Only an empty, unpublished inode with exactly the temporary
            // and final names is recoverable. Never unlink a data-bearing or
            // active database alias, and never open another fd for this inode.
            fs::remove_file(temporary)?;
            return super::sync_directory(parent);
        }
        Ok(())
    }
}

/// Publish an empty inode only after its temporary fd has closed. Opening
/// create_new directly at the final name would let another SQLite connection
/// begin locking that inode before we close the unrelated creation fd.
#[cfg(unix)]
fn create_closed_database_file(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("missing database parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid_data("missing database filename"))?;
    let (temporary, file) = super::create_temporary_file(parent, name)?;
    let synced = file.sync_all();
    drop(file);
    if let Err(error) = synced {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let published = fs::hard_link(&temporary, path);
    let removed = fs::remove_file(&temporary).or_else(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    });
    match published {
        Ok(()) => {
            removed?;
            super::sync_directory(parent)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            removed?;
            Ok(())
        }
        Err(error) => {
            let _ = removed;
            Err(error)
        }
    }
}

#[cfg(windows)]
fn create_closed_database_file(path: &Path) -> io::Result<()> {
    // Windows locks belong to handles, so closing a creation handle cannot
    // cancel another connection's locks. create_new avoids a second hardlink
    // and never replaces a concurrently published database.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    super::add_nofollow_flags(&mut options);
    match options.open(path) {
        Ok(file) => {
            validate_file(path, &file.metadata()?)?;
            file.sync_all()?;
            super::sync_directory(
                path.parent()
                    .ok_or_else(|| invalid_data("missing database parent"))?,
            )
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

struct ScopeGuard {
    path: PathBuf,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPES.with(|scopes| {
            let popped = scopes.borrow_mut().pop();
            debug_assert!(popped.is_some_and(|scope| scope.path == self.path));
        });
    }
}

struct TransactionGuard<'a> {
    connection: &'a Connection,
    savepoint: Option<String>,
    completed: bool,
}

impl TransactionGuard<'_> {
    fn commit(&mut self) -> io::Result<()> {
        let command = self.savepoint.as_ref().map_or_else(
            || "COMMIT".to_owned(),
            |name| format!("RELEASE SAVEPOINT {name}"),
        );
        self.connection.execute_batch(&command).map_err(sql_error)?;
        self.completed = true;
        Ok(())
    }
}

impl Drop for TransactionGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let command = self.savepoint.as_ref().map_or_else(
                || "ROLLBACK".to_owned(),
                |name| format!("ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"),
            );
            let _ = self.connection.execute_batch(&command);
        }
    }
}

impl OpenedDatabase {
    fn validate(&self) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid_data("missing database directory"))?;
        super::validate_private_directory_beneath(&self.state_root, parent)?;
        let metadata = fs::symlink_metadata(&self.path)?;
        validate_file(&self.path, &metadata)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if self.identity != (metadata.dev(), metadata.ino()) {
                return Err(invalid_data("history database path identity changed"));
            }
            let mut moved = 0i32;
            // SQLite's own VFS compares its fd with the current pathname; no
            // extra DB fd is opened/closed alongside its POSIX locks.
            let result = unsafe {
                rusqlite::ffi::sqlite3_file_control(
                    self.connection.handle(),
                    c"main".as_ptr(),
                    rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                    (&mut moved as *mut i32).cast(),
                )
            };
            if result != rusqlite::ffi::SQLITE_OK || moved != 0 {
                return Err(invalid_data("opened history database object changed"));
            }
        }
        #[cfg(windows)]
        {
            let opened_metadata = self.identity_guard.metadata()?;
            validate_file(&self.path, &opened_metadata)?;
            super::ensure_opened_file_matches_path(
                &self.path,
                &self.identity_guard,
                &metadata,
                &opened_metadata,
                "history database",
            )?;
            validate_windows_sqlite_handle(&self.connection, &self.identity_guard)?;
        }
        validate_side_files(&self.path)?;
        #[cfg(unix)]
        validate_unix_sqlite_side_handles(&self.connection, &self.path)?;
        #[cfg(windows)]
        {
            validate_windows_side_guards(&self.path, &self.side_file_guards)?;
            if let Some(journal) = sqlite_journal_file(&self.connection)? {
                let guards = self.side_file_guards.borrow();
                let wal = side_file_path(&self.path, "-wal");
                let guard = guards
                    .iter()
                    .find(|(path, _)| path == &wal)
                    .ok_or_else(|| invalid_data("opened SQLite WAL path is missing"))?;
                validate_windows_file_handle(journal, &guard.1)?;
            }
        }
        Ok(())
    }
}

fn validate_file(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    super::validate_data_file_metadata(path, metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(invalid_data(
                "history database files must have exactly one hard link",
            ));
        }
    }
    #[cfg(windows)]
    {
        let mut options = OpenOptions::new();
        options.read(true);
        super::add_nofollow_flags(&mut options);
        let file = options.open(path)?;
        crate::source_identity::validate_windows_private_file(
            path,
            &file,
            "history database file",
        )?;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if information.nNumberOfLinks != 1 {
            return Err(invalid_data(
                "history database files must have exactly one hard link",
            ));
        }
    }
    Ok(())
}

fn side_file_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn validate_side_files(path: &Path) -> io::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let side = side_file_path(path, suffix);
        match fs::symlink_metadata(&side) {
            Ok(metadata) => validate_file(&side, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Borrow SQLite's real journal object, without interpreting a VFS-private
/// layout or taking ownership of any fd / HANDLE.
fn sqlite_journal_file(
    connection: &Connection,
) -> io::Result<Option<*mut rusqlite::ffi::sqlite3_file>> {
    let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_JOURNAL_POINTER,
            (&mut file as *mut *mut rusqlite::ffi::sqlite3_file).cast(),
        )
    };
    if result != rusqlite::ffi::SQLITE_OK {
        return Err(invalid_data("cannot inspect opened SQLite journal object"));
    }
    if file.is_null() || unsafe { (*file).pMethods.is_null() } {
        Ok(None)
    } else {
        Ok(Some(file))
    }
}

#[cfg(unix)]
#[derive(Deserialize)]
struct SqliteFileStat {
    h: i32,
    #[serde(default)]
    shm: Option<SqliteShmStat>,
}

#[cfg(unix)]
#[derive(Deserialize)]
struct SqliteShmStat {
    h: i32,
}

#[cfg(unix)]
fn sqlite_file_stat(
    control: impl FnOnce(*mut rusqlite::ffi::sqlite3_str) -> i32,
) -> io::Result<SqliteFileStat> {
    struct StringGuard(*mut rusqlite::ffi::sqlite3_str);
    impl Drop for StringGuard {
        fn drop(&mut self) {
            unsafe {
                rusqlite::ffi::sqlite3_free(rusqlite::ffi::sqlite3_str_finish(self.0).cast())
            };
        }
    }
    let string = StringGuard(unsafe { rusqlite::ffi::sqlite3_str_new(std::ptr::null_mut()) });
    if string.0.is_null() {
        return Err(io::Error::other("cannot allocate SQLite file inspection"));
    }
    let result = control(string.0);
    if result != rusqlite::ffi::SQLITE_OK {
        return Err(invalid_data(
            "SQLite FILESTAT is required to verify opened history WAL/SHM; build with SQLITE_ENABLE_FILESTAT",
        ));
    }
    let length = unsafe { rusqlite::ffi::sqlite3_str_length(string.0) };
    let value = unsafe { rusqlite::ffi::sqlite3_str_value(string.0) };
    if length <= 0
        || length > 64 * 1024
        || value.is_null()
        || unsafe { rusqlite::ffi::sqlite3_str_errcode(string.0) } != rusqlite::ffi::SQLITE_OK
    {
        return Err(invalid_data("invalid SQLite opened-file inspection"));
    }
    let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), length as usize) };
    serde_json::from_slice(bytes).map_err(|error| invalid_data(error.to_string()))
}

#[cfg(unix)]
fn validate_borrowed_unix_fd(path: &Path, fd: i32) -> io::Result<()> {
    use std::mem::ManuallyDrop;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;
    if fd < 0 {
        return Err(invalid_data(
            "SQLite opened history side file has no descriptor",
        ));
    }
    // metadata() is fstat on SQLite's borrowed fd. Do not dup/open/close it:
    // closing even a different fd for this inode would release POSIX locks.
    let borrowed = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    let opened = borrowed.metadata()?;
    validate_file(path, &opened)?;
    let current = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid_data("opened history side file disappeared")
        } else {
            error
        }
    })?;
    validate_file(path, &current)?;
    if (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
        return Err(invalid_data("opened history side file identity changed"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_unix_sqlite_side_handles(connection: &Connection, path: &Path) -> io::Result<()> {
    let main = sqlite_file_stat(|output| unsafe {
        rusqlite::ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_FILESTAT,
            output.cast(),
        )
    })?;
    validate_borrowed_unix_fd(path, main.h)?;
    if let Some(shm) = main.shm {
        validate_borrowed_unix_fd(&side_file_path(path, "-shm"), shm.h)?;
    }
    if let Some(journal) = sqlite_journal_file(connection)? {
        let control = unsafe { (*(*journal).pMethods).xFileControl }
            .ok_or_else(|| invalid_data("opened SQLite WAL has no file control"))?;
        let wal = sqlite_file_stat(|output| unsafe {
            control(journal, rusqlite::ffi::SQLITE_FCNTL_FILESTAT, output.cast())
        })?;
        validate_borrowed_unix_fd(&side_file_path(path, "-wal"), wal.h)?;
    }
    Ok(())
}

#[cfg(windows)]
fn pin_windows_side_files(path: &Path) -> io::Result<Vec<(PathBuf, File)>> {
    let guards = RefCell::new(Vec::new());
    validate_windows_side_guards(path, &guards)?;
    Ok(guards.into_inner())
}

#[cfg(windows)]
fn validate_windows_side_guards(
    path: &Path,
    guards: &RefCell<Vec<(PathBuf, File)>>,
) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let mut guards = guards.borrow_mut();
    for suffix in ["-wal", "-shm"] {
        let side = side_file_path(path, suffix);
        let current = match fs::symlink_metadata(&side) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if guards.iter().any(|(path, _)| path == &side) {
                    return Err(invalid_data("opened history side file disappeared"));
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        validate_file(&side, &current)?;
        if !guards.iter().any(|(path, _)| path == &side) {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
            super::add_nofollow_flags(&mut options);
            guards.push((side.clone(), options.open(&side)?));
        }
        let guard = &guards
            .iter()
            .find(|(path, _)| path == &side)
            .expect("guard inserted")
            .1;
        let opened = guard.metadata()?;
        validate_file(&side, &opened)?;
        super::ensure_opened_file_matches_path(
            &side,
            guard,
            &current,
            &opened,
            "history side file",
        )?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_file_handle(
    file: *mut rusqlite::ffi::sqlite3_file,
    guard: &File,
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::HANDLE;
    let control = unsafe { (*(*file).pMethods).xFileControl }
        .ok_or_else(|| invalid_data("opened SQLite file has no file control"))?;
    let mut handle: HANDLE = std::ptr::null_mut();
    let result = unsafe {
        control(
            file,
            rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&mut handle as *mut HANDLE).cast(),
        )
    };
    if result != rusqlite::ffi::SQLITE_OK || handle.is_null() {
        return Err(invalid_data("cannot verify opened SQLite WAL handle"));
    }
    validate_windows_handle_identity(handle, guard)
}

#[cfg(windows)]
fn validate_windows_sqlite_handle(connection: &Connection, guard: &File) -> io::Result<()> {
    use windows_sys::Win32::Foundation::HANDLE;
    let mut handle: HANDLE = std::ptr::null_mut();
    // Borrow the actual SQLite HANDLE; never transfer ownership or close it.
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&mut handle as *mut HANDLE).cast(),
        )
    };
    if result != rusqlite::ffi::SQLITE_OK || handle.is_null() {
        return Err(invalid_data("cannot validate opened SQLite Windows handle"));
    }
    validate_windows_handle_identity(handle, guard)
}

#[cfg(windows)]
fn validate_windows_handle_identity(
    handle: windows_sys::Win32::Foundation::HANDLE,
    guard: &File,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut actual: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let mut expected: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut actual) } == 0
        || unsafe { GetFileInformationByHandle(guard.as_raw_handle(), &mut expected) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if actual.nNumberOfLinks != 1
        || actual.dwVolumeSerialNumber != expected.dwVolumeSerialNumber
        || actual.nFileIndexHigh != expected.nFileIndexHigh
        || actual.nFileIndexLow != expected.nFileIndexLow
    {
        return Err(invalid_data("opened SQLite Windows file identity changed"));
    }
    Ok(())
}

fn initialize_schema(connection: &Connection, database: &HistoryDatabase) -> io::Result<()> {
    let id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(sql_error)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_error)?;
    if id == APPLICATION_ID {
        return validate_schema(connection, &database.profile);
    }
    let table_count: i64 = connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
        .map_err(sql_error)?;
    if id != 0 || version != 0 || table_count != 0 {
        return Err(invalid_data("unrecognized history database schema"));
    }
    database.refuse_recreating_active_database()?;
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(sql_error)?;
    let mut guard = TransactionGuard {
        connection,
        savepoint: None,
        completed: false,
    };
    let objects = schema_objects();
    for index in [1, 3, 0, 2] {
        let (_, _, sql) = &objects[index];
        connection.execute_batch(sql).map_err(sql_error)?;
    }
    connection
        .execute_batch(&format!(
            "PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version={SCHEMA_VERSION};"
        ))
        .map_err(sql_error)?;
    set_state(connection, "database/profile", &database.profile)?;
    guard.commit()?;
    validate_schema(connection, &database.profile)
}

fn validate_schema(connection: &Connection, profile: &HistoryProfileId) -> io::Result<()> {
    let id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(sql_error)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_error)?;
    if id != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err(invalid_data(format!(
            "unsupported history database schema: application={id}, version={version}"
        )));
    }
    // application_id alone is not a schema contract: reject replacement
    // views, triggers or changed constraints before reading business data.
    let mut statement = connection
        .prepare("SELECT type,name,sql FROM sqlite_schema ORDER BY name")
        .map_err(sql_error)?;
    let objects = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    let expected = schema_objects();
    if objects.len() != expected.len()
        || objects.iter().zip(expected).any(
            |((kind, name, sql), (expected_kind, expected_name, expected_sql))| {
                kind != expected_kind
                    || name != expected_name
                    || normalize_schema_sql(sql) != normalize_schema_sql(&expected_sql)
            },
        )
    {
        return Err(invalid_data(
            "history database schema objects do not match the supported schema",
        ));
    }
    if state::<HistoryProfileId>(connection, "database/profile")?.as_ref() != Some(profile) {
        return Err(invalid_data(
            "history database belongs to a different profile",
        ));
    }
    if rusqlite::version_number() < 3_051_003 {
        return Err(invalid_data(
            "SQLite runtime does not contain the required WAL-reset fix",
        ));
    }
    Ok(())
}

fn schema_objects() -> [(&'static str, &'static str, String); 4] {
    // Keep this in sqlite_schema's name order for exact validation.
    [
        (
            "index",
            "history_record_time",
            "CREATE INDEX history_record_time ON history_records(namespace, sort_time, record_key)"
                .to_owned(),
        ),
        (
            "table",
            "history_records",
            format!(
                "CREATE TABLE history_records (namespace TEXT NOT NULL, record_key TEXT NOT NULL, sort_time INTEGER NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<={MAX_VALUE_BYTES}), PRIMARY KEY(namespace, record_key)) STRICT, WITHOUT ROWID"
            ),
        ),
        (
            "index",
            "history_source_metadata",
            "CREATE INDEX history_source_metadata ON history_state(state_key) WHERE substr(state_key,-12)='/source.json'".to_owned(),
        ),
        (
            "table",
            "history_state",
            format!(
                "CREATE TABLE history_state (state_key TEXT PRIMARY KEY NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<={MAX_VALUE_BYTES})) STRICT, WITHOUT ROWID"
            ),
        ),
    ]
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<String>()
}

pub(crate) fn sql_error(error: rusqlite::Error) -> io::Error {
    let kind = match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => io::ErrorKind::WouldBlock,
        Some(
            ErrorCode::ReadOnly
            | ErrorCode::PermissionDenied
            | ErrorCode::AuthorizationForStatementDenied,
        ) => io::ErrorKind::PermissionDenied,
        Some(ErrorCode::DiskFull) => io::ErrorKind::StorageFull,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, error)
}

fn encode<T: Serialize + ?Sized>(value: &T) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|error| invalid_data(error.to_string()))?;
    if bytes.len() > MAX_VALUE_BYTES {
        return Err(invalid_data(
            "history database record exceeds its size budget",
        ));
    }
    Ok(bytes)
}

pub(crate) fn state<T: DeserializeOwned>(
    connection: &Connection,
    key: &str,
) -> io::Result<Option<T>> {
    let mut statement = connection
        .prepare("SELECT payload FROM history_state WHERE state_key=?1")
        .map_err(sql_error)?;
    let mut rows = statement.query([key]).map_err(sql_error)?;
    let Some(row) = rows.next().map_err(sql_error)? else {
        return Ok(None);
    };
    let bytes = row
        .get_ref(0)
        .map_err(sql_error)?
        .as_blob()
        .map_err(|error| invalid_data(error.to_string()))?;
    serde_json::from_slice(bytes)
        .map(Some)
        .map_err(|error| invalid_data(error.to_string()))
}

pub(crate) fn set_state<T: Serialize + ?Sized>(
    connection: &Connection,
    key: &str,
    value: &T,
) -> io::Result<()> {
    connection.execute("INSERT INTO history_state(state_key,payload) VALUES (?1,?2) ON CONFLICT(state_key) DO UPDATE SET payload=excluded.payload", params![key, encode(value)?]).map_err(sql_error)?;
    Ok(())
}

pub(crate) fn delete_state(connection: &Connection, key: &str) -> io::Result<()> {
    connection
        .execute("DELETE FROM history_state WHERE state_key=?1", [key])
        .map_err(sql_error)?;
    Ok(())
}

pub(crate) fn states<T: DeserializeOwned>(
    connection: &Connection,
    prefix: &str,
) -> io::Result<Vec<(String, T)>> {
    let mut statement = connection.prepare("SELECT state_key,payload FROM history_state WHERE substr(state_key,1,length(?1))=?1 ORDER BY state_key").map_err(sql_error)?;
    let mut rows = statement.query([prefix]).map_err(sql_error)?;
    let mut values = Vec::new();
    let mut budget = SourceHistoryReadBudget::for_query();
    while let Some(row) = rows.next().map_err(sql_error)? {
        let key = row.get::<_, String>(0).map_err(sql_error)?;
        let bytes = row
            .get_ref(1)
            .map_err(sql_error)?
            .as_blob()
            .map_err(|error| invalid_data(error.to_string()))?;
        budget.charge_decoded_bytes(bytes.len() as u64)?;
        budget.charge_records(1)?;
        let value =
            serde_json::from_slice(bytes).map_err(|error| invalid_data(error.to_string()))?;
        values.push((key, value));
    }
    Ok(values)
}

/// List identities before selecting their typed decoder. Prefixes can contain
/// state from several business families and must not be decoded as one DTO.
pub(crate) fn state_keys(connection: &Connection, prefix: &str) -> io::Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT state_key FROM history_state WHERE substr(state_key,1,length(?1))=?1 ORDER BY state_key").map_err(sql_error)?;
    let mut rows = statement.query([prefix]).map_err(sql_error)?;
    let mut values = Vec::new();
    let mut budget = SourceHistoryReadBudget::for_query();
    while let Some(row) = rows.next().map_err(sql_error)? {
        let key = row.get::<_, String>(0).map_err(sql_error)?;
        budget.charge_decoded_bytes(key.len() as u64)?;
        budget.charge_records(1)?;
        values.push(key);
    }
    Ok(values)
}

pub(crate) fn records<T: DeserializeOwned>(
    connection: &Connection,
    namespace: &str,
    since_millis: i64,
    budget: &mut SourceHistoryReadBudget,
) -> io::Result<Vec<T>> {
    let mut statement = connection.prepare("SELECT payload FROM history_records WHERE namespace=?1 AND sort_time>=?2 ORDER BY sort_time,record_key").map_err(sql_error)?;
    let mut rows = statement
        .query(params![namespace, since_millis])
        .map_err(sql_error)?;
    let mut values = Vec::new();
    while let Some(row) = rows.next().map_err(sql_error)? {
        let bytes = row
            .get_ref(0)
            .map_err(sql_error)?
            .as_blob()
            .map_err(|error| invalid_data(error.to_string()))?;
        budget.charge_decoded_bytes(bytes.len() as u64)?;
        budget.charge_records(1)?;
        values
            .push(serde_json::from_slice(bytes).map_err(|error| invalid_data(error.to_string()))?);
    }
    Ok(values)
}

pub(crate) fn put_record<T: Serialize + ?Sized>(
    connection: &Connection,
    namespace: &str,
    key: &str,
    sort_time_millis: i64,
    value: &T,
) -> io::Result<()> {
    connection.execute("INSERT INTO history_records(namespace,record_key,sort_time,payload) VALUES (?1,?2,?3,?4) ON CONFLICT(namespace,record_key) DO UPDATE SET sort_time=excluded.sort_time,payload=excluded.payload", params![namespace, key, sort_time_millis, encode(value)?]).map_err(sql_error)?;
    Ok(())
}

pub(crate) fn delete_record(connection: &Connection, namespace: &str, key: &str) -> io::Result<()> {
    connection
        .execute(
            "DELETE FROM history_records WHERE namespace=?1 AND record_key=?2",
            params![namespace, key],
        )
        .map_err(sql_error)?;
    Ok(())
}

pub(crate) fn delete_namespace(connection: &Connection, namespace: &str) -> io::Result<()> {
    connection
        .execute(
            "DELETE FROM history_records WHERE namespace=?1",
            [namespace],
        )
        .map_err(sql_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(directory: &Path) -> HistoryDatabase {
        HistoryDatabase::new(
            &directory.join("state"),
            &"0123456789abcdef".parse().unwrap(),
        )
    }

    #[test]
    fn read_missing_database_does_not_create_state() {
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        assert_eq!(
            database.read(|_| Ok(())).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!database.path().exists());
        assert!(!database.profile_root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn bundled_sqlite_includes_required_file_stat_support() {
        let directory = tempfile::tempdir().unwrap();
        database(directory.path())
            .write(|connection| {
                // SQLite does not list FILESTAT in sqlite_compileoption_used().
                // Probe the required public file-control API itself.
                let main = sqlite_file_stat(|output| unsafe {
                    rusqlite::ffi::sqlite3_file_control(
                        connection.handle(),
                        c"main".as_ptr(),
                        rusqlite::ffi::SQLITE_FCNTL_FILESTAT,
                        output.cast(),
                    )
                })?;
                assert!(main.h >= 0);
                Ok(())
            })
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_file_stat_refuses_instead_of_using_a_path_only_fallback() {
        let error = match sqlite_file_stat(|_| rusqlite::ffi::SQLITE_NOTFOUND) {
            Ok(_) => panic!("unsupported SQLite file inspection was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("SQLITE_ENABLE_FILESTAT"));
    }

    #[test]
    fn failed_multi_family_transaction_rolls_back_and_nested_failure_is_isolated() {
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        database
            .write(|connection| set_state(connection, "quota", &1u64))
            .unwrap();
        let error = database
            .write(|connection| {
                set_state(connection, "quota", &2u64)?;
                database.write(|nested| set_state(nested, "bucket", &2u64))?;
                Err::<(), _>(io::Error::other("interrupted"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "interrupted");
        database
            .read(|connection| {
                assert_eq!(state::<u64>(connection, "quota")?, Some(1));
                assert_eq!(state::<u64>(connection, "bucket")?, None);
                assert_eq!(
                    database.write(|_| Ok(())).unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied
                );
                Ok(())
            })
            .unwrap();
        database
            .write(|connection| {
                let nested = database.write(|nested| {
                    set_state(nested, "bucket", &3u64)?;
                    Err::<(), _>(io::Error::other("nested interruption"))
                });
                assert!(nested.is_err());
                assert_eq!(state::<u64>(connection, "bucket")?, None);
                set_state(connection, "quota", &3u64)
            })
            .unwrap();
    }

    #[test]
    fn record_payload_preserves_unsigned_boundaries_and_query_budget() {
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        #[derive(Debug, Serialize, serde::Deserialize, PartialEq)]
        struct Exact {
            generation: u64,
            amount: u128,
        }
        let value = Exact {
            generation: u64::MAX,
            amount: u128::MAX,
        };
        database
            .write(|connection| {
                put_record(connection, "facts/physical-replica", "event", 1, &value)
            })
            .unwrap();
        database
            .read(|connection| {
                let rows: Vec<Exact> = records(
                    connection,
                    "facts/physical-replica",
                    0,
                    &mut SourceHistoryReadBudget::for_query(),
                )?;
                assert_eq!(rows, vec![value]);
                let error = records::<Exact>(
                    connection,
                    "facts/physical-replica",
                    0,
                    &mut SourceHistoryReadBudget::with_limits(1, 1, 1),
                )
                .unwrap_err();
                assert!(SourceHistoryReadBudget::is_exhaustion(&error));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn sqlite_process_lock_probe() {
        let Some(root) = std::env::var_os("MONIT_SQLITE_LOCK_PROBE_ROOT") else {
            return;
        };
        let database = database(Path::new(&root));
        database
            .read(|connection| {
                assert_eq!(state::<u64>(connection, "quota")?, Some(1));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            database
                .write_nowait(|connection| set_state(connection, "quota", &99u64))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn sqlite_writer_keeps_cross_process_lock_when_a_sibling_reader_closes() {
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        database
            .write(|connection| set_state(connection, "quota", &1u64))
            .unwrap();
        database
            .write(|connection| {
                set_state(connection, "quota", &2u64)?;
                let sibling = database.clone();
                std::thread::spawn(move || {
                    sibling.read(|connection| {
                        assert_eq!(state::<u64>(connection, "quota")?, Some(1));
                        Ok(())
                    })
                })
                .join()
                .unwrap()?;
                // Repeated fd/HANDLE inspections must not close SQLite's
                // descriptors or release the writer's POSIX inode locks.
                for _ in 0..8 {
                    database.read(|connection| {
                        assert_eq!(state::<u64>(connection, "quota")?, Some(2));
                        Ok(())
                    })?;
                }
                let child = std::process::Command::new(std::env::current_exe()?)
                    .args([
                        "--exact",
                        "source_history::database::tests::sqlite_process_lock_probe",
                        "--nocapture",
                    ])
                    .env("MONIT_SQLITE_LOCK_PROBE_ROOT", directory.path())
                    .output()?;
                assert!(
                    child.status.success(),
                    "{} {}",
                    String::from_utf8_lossy(&child.stdout),
                    String::from_utf8_lossy(&child.stderr)
                );
                assert_eq!(state::<u64>(connection, "quota")?, Some(2));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn sqlite_unrecognized_schema_cannot_execute_through_typed_facade() {
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        database.write(|connection| {
            connection.execute_batch("CREATE TRIGGER sqliteXhidden BEFORE UPDATE ON history_state BEGIN SELECT RAISE(ABORT,'unexpected trigger'); END").map_err(sql_error)
        }).unwrap();
        let error = database
            .read(|connection| state::<bool>(connection, "ready"))
            .unwrap_err();
        assert!(error.to_string().contains("schema objects"));
        assert!(
            database
                .write(|connection| set_state(connection, "ready", &true))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_interrupted_empty_database_publication_recovers_its_own_link() {
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        super::super::create_private_directory_beneath(
            &database.state_root,
            &database.profile_root,
        )
        .unwrap();
        let (temporary, file) = super::super::create_temporary_file(
            &database.profile_root,
            std::ffi::OsStr::new(DATABASE_FILE),
        )
        .unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::hard_link(&temporary, database.path()).unwrap();
        assert!(database.read(|_| Ok(())).is_err());
        database
            .write(|connection| set_state(connection, "recovered", &true))
            .unwrap();
        assert!(!temporary.exists());
        assert_eq!(
            database
                .read(|connection| state::<bool>(connection, "recovered"))
                .unwrap(),
            Some(true)
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_wal_and_shm_replacement_or_loss_refuses_nested_use_and_rolls_back() {
        use std::os::unix::fs::PermissionsExt;

        for suffix in ["-wal", "-shm"] {
            for replace in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let database = database(directory.path());
                database
                    .write(|connection| set_state(connection, "committed", &true))
                    .unwrap();
                let result = database.write(|connection| {
                    set_state(connection, "uncommitted", &true)?;
                    let side = side_file_path(database.path(), suffix);
                    let displaced = side_file_path(database.path(), &format!("{suffix}.displaced"));
                    assert!(side.exists(), "SQLite must have opened {suffix}");
                    fs::rename(&side, &displaced)?;
                    if replace {
                        fs::write(&side, [])?;
                        fs::set_permissions(&side, fs::Permissions::from_mode(0o600))?;
                    }
                    // Restore before SQLite closes its borrowed objects. The
                    // application fence must reject the changed path while
                    // the original WAL fd / SHM mmap is still open.
                    let fenced = database.read(|_| Ok(()));
                    if replace {
                        fs::remove_file(&side)?;
                    }
                    fs::rename(&displaced, &side)?;
                    fenced
                });
                let error = result.expect_err("an opened SQLite side object changed");
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                database
                    .read(|connection| {
                        assert_eq!(state::<bool>(connection, "committed")?, Some(true));
                        assert_eq!(state::<bool>(connection, "uncommitted")?, None);
                        Ok(())
                    })
                    .unwrap();
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_open_side_guards_deny_replacement_then_allow_normal_close_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        database
            .write(|connection| {
                set_state(connection, "committed", &true)?;
                database.read(|_| Ok(()))?;
                for suffix in ["-wal", "-shm"] {
                    let side = side_file_path(database.path(), suffix);
                    assert!(side.exists());
                    assert!(
                        fs::rename(
                            &side,
                            side_file_path(database.path(), &format!("{suffix}.replaced"))
                        )
                        .is_err()
                    );
                    assert!(fs::remove_file(&side).is_err());
                }
                Ok(())
            })
            .unwrap();
        // The final writer closes and removes its side files. Read-only
        // connections may recreate WAL/SHM without cleaning them up on close.
        for suffix in ["-wal", "-shm"] {
            assert!(!side_file_path(database.path(), suffix).exists());
        }
        database
            .read(|connection| {
                assert_eq!(state::<bool>(connection, "committed")?, Some(true));
                Ok(())
            })
            .unwrap();
        // Guards are dropped before SQLite closes its final handle. An
        // ordinary close must be able to remove its two auxiliary files.
        database.write(|_| Ok(())).unwrap();
        for suffix in ["-wal", "-shm"] {
            assert!(!side_file_path(database.path(), suffix).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_rejects_hardlinks_symlinks_and_replaced_objects() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let database = database(directory.path());
        database
            .write(|connection| set_state(connection, "ready", &true))
            .unwrap();
        let linked = database.profile_root.join("linked.sqlite3");
        fs::hard_link(database.path(), &linked).unwrap();
        assert!(database.read(|_| Ok(())).is_err());
        fs::remove_file(linked).unwrap();
        let moved = database.profile_root.join("old.sqlite3");
        database
            .read(|_| {
                fs::rename(database.path(), &moved)?;
                symlink(&moved, database.path())?;
                Ok(())
            })
            .unwrap_err();
        assert!(database.read(|_| Ok(())).is_err());
    }
    #[test]
    fn sqlite_preview_schema_is_rejected_without_upgrading_or_adopting_rows() {
        let root = tempfile::tempdir().unwrap();
        let db = database(root.path());
        db.write(|connection| {
            set_state(connection, "preview-derived", &42_u64)?;
            connection
                .pragma_update(None, "user_version", 1)
                .map_err(sql_error)
        })
        .unwrap();
        for readonly in [true, false] {
            let result = if readonly {
                db.read(|_| Ok(()))
            } else {
                db.write(|_| Ok(()))
            };
            let error = result.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("version=1"));
        }
        let connection = Connection::open(db.path()).unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert_eq!(
            state::<u64>(&connection, "preview-derived").unwrap(),
            Some(42)
        );
    }
}
