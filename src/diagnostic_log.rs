use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Bounds each live diagnostic log and its single retained backup to 8 MiB.
///
/// Rotation is deliberately implemented as copy-then-truncate while the live
/// file remains exclusively locked. That avoids a rename window in which a
/// second process could create and start writing a replacement live file, and
/// it also works with Windows handles that cannot be renamed while open.
pub(crate) const DIAGNOSTIC_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct JsonlWriter {
    sink: JsonlSink,
}

enum JsonlSink {
    File(RotatingPrivateFile),
    #[cfg(test)]
    Stream(Box<dyn Write + Send>),
}

struct RotatingPrivateFile {
    path: PathBuf,
    description: &'static str,
    writer: BufWriter<LockedPrivateFile>,
    written_bytes: u64,
    max_bytes: u64,
    confidential: bool,
}

/// Release the flock explicitly: closing just this descriptor is insufficient
/// while a concurrent fork still holds a reference to its open-file description.
/// BufWriter owns this guard so its pending bytes are flushed before unlocking.
struct LockedPrivateFile {
    file: File,
}

impl LockedPrivateFile {
    #[cfg(all(test, unix))]
    fn open(path: &Path, description: &str) -> io::Result<Self> {
        Self::open_with_policy(path, description, false)
    }

    fn open_with_policy(path: &Path, description: &str, confidential: bool) -> io::Result<Self> {
        let file = if confidential {
            open_confidential_file(path, description)?
        } else {
            open_private_regular_file(path, description)?
        };
        // On Windows confidential files deny other writers through sharing
        // modes, leaving readers free to tail the live log. Trace/perf retain
        // their existing byte-range locking contract.
        if !cfg!(windows) || !confidential {
            lock_file(&file, description)?;
        }
        Ok(Self { file })
    }
}

impl Write for LockedPrivateFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for LockedPrivateFile {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl JsonlWriter {
    pub(crate) fn open_private(
        path: &Path,
        description: &'static str,
        max_bytes: u64,
    ) -> io::Result<Self> {
        Self::open_with_policy(path, description, max_bytes, false)
    }

    /// Event logs retain previous runs and may contain sanitized error details.
    pub(crate) fn open_events(path: &Path, max_bytes: u64) -> io::Result<Self> {
        Self::open_with_policy(path, "event log", max_bytes, true)
    }

    fn open_with_policy(
        path: &Path,
        description: &'static str,
        max_bytes: u64,
        confidential: bool,
    ) -> io::Result<Self> {
        if max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic log byte limit must be greater than zero",
            ));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if confidential && !parent.exists() {
                #[cfg(windows)]
                crate::windows_private_directory::create_dir_all(parent)?;
                #[cfg(not(windows))]
                fs::create_dir_all(parent)?;
            } else {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = LockedPrivateFile::open_with_policy(path, description, confidential)?;
        let written_bytes = if confidential {
            let length = file.file.seek(SeekFrom::End(0))?;
            if length > 0 {
                file.file.seek(SeekFrom::End(-1))?;
                let mut last = [0];
                file.file.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    file.file.write_all(b"\n")?;
                    length.saturating_add(1)
                } else {
                    length
                }
            } else {
                0
            }
        } else {
            file.file.set_len(0)?;
            file.file.seek(SeekFrom::Start(0))?
        };
        Ok(Self {
            sink: JsonlSink::File(RotatingPrivateFile {
                path: path.to_path_buf(),
                description,
                writer: BufWriter::new(file),
                written_bytes,
                max_bytes,
                confidential,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_stream(writer: Box<dyn Write + Send>) -> Self {
        Self {
            sink: JsonlSink::Stream(writer),
        }
    }

    pub(crate) fn write_json_line(&mut self, value: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
        line.push(b'\n');
        match &mut self.sink {
            JsonlSink::File(file) => file.write_line(&line),
            #[cfg(test)]
            JsonlSink::Stream(writer) => writer.write_all(&line),
        }
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        match &mut self.sink {
            JsonlSink::File(file) => file.writer.flush(),
            #[cfg(test)]
            JsonlSink::Stream(writer) => writer.flush(),
        }
    }
}

impl RotatingPrivateFile {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        let line_bytes = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if line_bytes > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} record exceeds the {}-byte log limit",
                    self.description, self.max_bytes
                ),
            ));
        }
        if self.written_bytes > 0 && self.written_bytes.saturating_add(line_bytes) > self.max_bytes
        {
            self.rotate()?;
        }
        self.writer.write_all(line)?;
        self.written_bytes = self.written_bytes.saturating_add(line_bytes);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.writer.flush()?;

        let backup_path = rotated_path(&self.path);
        let mut backup =
            LockedPrivateFile::open_with_policy(&backup_path, self.description, self.confidential)?;
        backup.file.set_len(0)?;
        backup.file.seek(SeekFrom::Start(0))?;

        let live = &mut self.writer.get_mut().file;
        live.seek(SeekFrom::Start(0))?;
        io::copy(&mut Read::by_ref(live), &mut backup.file)?;
        backup.flush()?;
        // The retained copy must be durable before the live file is truncated.
        // This runs only once per several MiB, so the sync cost is bounded and
        // does not affect the normal per-record path.
        backup.file.sync_data()?;

        live.set_len(0)?;
        live.seek(SeekFrom::Start(0))?;
        self.written_bytes = 0;
        Ok(())
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(".1");
    PathBuf::from(value)
}

fn open_confidential_file(path: &Path, description: &str) -> io::Result<File> {
    #[cfg(windows)]
    let file = crate::windows_private_directory::open_private_file(path)?;
    #[cfg(not(windows))]
    let file = open_private_regular_file(path, description)?;
    ensure_regular_file(&file, description)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata()?.nlink() != 1 {
            return Err(io::Error::other("event log must not have hard links"));
        }
    }
    Ok(file)
}

/// Only remove our generated session names, and skip files held by live TUIs.
pub(crate) fn prune_event_sessions(directory: &Path, keep: usize) -> io::Result<()> {
    let mut paths = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.strip_prefix("tui-")
                        .and_then(|value| value.strip_suffix(".jsonl"))
                        .is_some_and(|value| {
                            !value.is_empty()
                                && value
                                    .bytes()
                                    .all(|byte| byte.is_ascii_digit() || byte == b'-')
                        })
                })
        })
        .collect::<Vec<_>>();
    paths.sort();
    let remove = paths.len().saturating_sub(keep);
    for path in paths.into_iter().take(remove) {
        if let Ok(mut file) = LockedPrivateFile::open_with_policy(&path, "event log", true) {
            let mut first_line = String::new();
            let mut reader = io::BufReader::new(Read::by_ref(&mut file.file).take(4096));
            if io::BufRead::read_line(&mut reader, &mut first_line).is_err()
                || !serde_json::from_str::<Value>(&first_line)
                    .is_ok_and(|value| value["managedSession"] == true)
            {
                continue;
            }
            drop(file);
            if fs::remove_file(&path).is_ok() {
                let backup = rotated_path(&path);
                if backup.exists()
                    && let Ok(file) =
                        LockedPrivateFile::open_with_policy(&backup, "event log", true)
                {
                    drop(file);
                    let _ = fs::remove_file(backup);
                }
            }
        }
    }
    Ok(())
}

fn open_private_regular_file(path: &Path, description: &str) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path)?;
        ensure_regular_file(&file, description)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        let mut options = OpenOptions::new();
        options
            .create(true)
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        ensure_regular_file(&file, description)?;
        ensure_windows_non_reparse_file(path, &file, description)?;
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        let file = options.open(path)?;
        ensure_regular_file(&file, description)?;
        Ok(file)
    }
}

fn lock_file(file: &File, description: &str) -> io::Result<()> {
    match fs2::FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(()),
        Err(error) if lock_is_contended(&error) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{description} is already in use by another process"),
        )),
        Err(error) => Err(error),
    }
}

fn lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
}

#[cfg(windows)]
fn ensure_windows_non_reparse_file(path: &Path, file: &File, description: &str) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let opened = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || current.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !current.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must not be a symbolic link or reparse point"),
        ));
    }
    Ok(())
}

fn ensure_regular_file(file: &File, description: &str) -> io::Result<()> {
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must be a regular file"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn event_log_rotation_keeps_private_complete_records() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let mut writer = JsonlWriter::open_events(&path, 64).unwrap();
        for index in 0..10 {
            writer
                .write_json_line(&serde_json::json!({"event": "failure", "index": index}))
                .unwrap();
        }
        writer.flush().unwrap();
        drop(writer);
        for path in [&path, &rotated_path(&path)] {
            assert!(std::fs::metadata(path).unwrap().len() <= 64);
            assert!(
                std::fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .all(|line| serde_json::from_str::<Value>(line).is_ok())
            );
            #[cfg(windows)]
            crate::source_identity::validate_windows_private_file(
                path,
                &File::open(path).unwrap(),
                "test event log",
            )
            .unwrap();
        }
    }

    #[test]
    fn event_log_append_separates_an_existing_record_without_final_newline() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        drop(JsonlWriter::open_events(&path, 1024).unwrap());
        std::fs::write(&path, b"{\"event\":\"previous\"}").unwrap();
        let mut writer = JsonlWriter::open_events(&path, 1024).unwrap();
        writer
            .write_json_line(&serde_json::json!({"event":"next"}))
            .unwrap();
        writer.flush().unwrap();
        drop(writer);
        let contents = std::fs::read_to_string(path).unwrap();
        let records: Vec<Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "previous");
        assert_eq!(records[1]["event"], "next");
    }

    #[cfg(windows)]
    #[test]
    fn event_log_rejects_hard_links_without_altering_the_target() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let writer = JsonlWriter::open_events(&path, 1024).unwrap();
        drop(writer);
        std::fs::write(&path, "sentinel").unwrap();
        let alias = temp.path().join("alias.jsonl");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(JsonlWriter::open_events(&alias, 1024).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn writer_drop_flushes_and_unlocks_with_an_inherited_descriptor() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("diagnostic.jsonl");
        let mut writer = JsonlWriter::open_private(&path, "test log", 96).unwrap();
        writer
            .write_json_line(&serde_json::json!({ "event": "buffered" }))
            .unwrap();
        let JsonlSink::File(file) = &writer.sink else {
            unreachable!();
        };
        // dup and fork share the same open-file description and flock. Keep
        // that inherited reference alive beyond the actual writer's lifetime.
        let inherited = file.writer.get_ref().file.try_clone().unwrap();
        assert_eq!(
            JsonlWriter::open_private(&path, "test log", 96)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(writer);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"event\":\"buffered\"}\n"
        );
        let reopened = JsonlWriter::open_private(&path, "test log", 96);
        drop(inherited);
        assert!(
            reopened.is_ok(),
            "writer left its flock held: {:?}",
            reopened.err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotation_releases_backup_lock_but_preserves_live_writer_exclusivity() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("diagnostic.jsonl");
        let mut writer = JsonlWriter::open_private(&path, "test log", 32).unwrap();
        let first = serde_json::json!({ "event": "first" });
        writer.write_json_line(&first).unwrap();
        writer.flush().unwrap();

        let backup_path = rotated_path(&path);
        let backup = LockedPrivateFile::open(&backup_path, "test backup").unwrap();
        let inherited = backup.file.try_clone().unwrap();
        let second = serde_json::json!({ "event": "second" });
        assert_eq!(
            writer.write_json_line(&second).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        // A failed rotation must preserve the live record and its writer lock.
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{first}\n"));
        drop(backup);

        // Keep the inherited backup descriptor open across several rotations.
        // A close-only release would leave its old flock held and fail here.
        for index in 0..8 {
            assert_eq!(
                JsonlWriter::open_private(&path, "test log", 32)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            writer
                .write_json_line(&serde_json::json!({ "index": index }))
                .unwrap();
        }
        drop(writer);
        let reopened = LockedPrivateFile::open(&backup_path, "test backup");
        drop(inherited);
        assert!(
            reopened.is_ok(),
            "backup left its flock held: {:?}",
            reopened.err()
        );
    }

    #[test]
    fn bounded_writer_rotates_complete_json_lines_and_keeps_writing() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("diagnostic.jsonl");
        let mut writer = JsonlWriter::open_private(&path, "test log", 96).unwrap();
        for index in 0..8 {
            writer
                .write_json_line(&serde_json::json!({ "event": "item", "index": index }))
                .unwrap();
        }
        writer.flush().unwrap();

        let backup = rotated_path(&path);
        assert!(backup.is_file());
        assert!(fs::metadata(&path).unwrap().len() <= 96);
        assert!(fs::metadata(&backup).unwrap().len() <= 96);
        // Release the Windows byte-range lock before reading via new handles.
        drop(writer);
        for contents in [
            fs::read_to_string(path).unwrap(),
            fs::read_to_string(backup).unwrap(),
        ] {
            assert!(
                contents
                    .lines()
                    .all(|line| serde_json::from_str::<Value>(line).is_ok())
            );
        }
    }
}
