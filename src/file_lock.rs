use std::fs::File;
use std::ops::{Deref, DerefMut};

/// Owns an already acquired OS file lock through every subsequent error path.
/// Explicit unlock releases Unix flock even while a concurrent fork retains a
/// descriptor for the same open-file description. Closing only this file does
/// not release that inherited lock.
#[derive(Debug)]
#[must_use = "the guard must remain alive while the protected operation runs"]
pub(crate) struct FileLock {
    file: File,
}

impl FileLock {
    /// Call immediately after a successful lock operation, before validation
    /// or other fallible work. Unsuccessful contenders must not own this guard.
    pub(crate) fn from_locked(file: File) -> Self {
        Self { file }
    }

    pub(crate) fn as_file(&self) -> &File {
        &self.file
    }
}

impl Deref for FileLock {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl DerefMut for FileLock {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::File::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{OpenOptions, TryLockError};
    use std::io::{self, BufRead, Write};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    fn open(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn drop_releases_inherited_shared_and_exclusive_locks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.lock");
        for shared in [false, true] {
            let file = open(&path);
            if shared {
                std::fs::File::try_lock_shared(&file).unwrap();
            } else {
                std::fs::File::try_lock(&file).unwrap();
            }
            let guard = FileLock::from_locked(file);
            let inherited = guard.try_clone().unwrap();
            let contender = open(&path);
            assert!(std::fs::File::try_lock(&contender).is_err());
            drop(guard);
            let acquired = std::fs::File::try_lock(&contender);
            drop(inherited);
            acquired.expect("a completed owner must release its inherited lock");
            drop(FileLock::from_locked(contender));
        }
    }

    #[test]
    fn error_after_acquisition_releases_an_inherited_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.lock");
        let file = open(&path);
        std::fs::File::try_lock(&file).unwrap();
        let inherited = file.try_clone().unwrap();
        let result = (|| -> io::Result<()> {
            let _guard = FileLock::from_locked(file);
            std::fs::metadata(temp.path().join("missing-state"))?;
            Ok(())
        })();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        let contender = open(&path);
        let acquired = std::fs::File::try_lock(&contender);
        drop(inherited);
        acquired.expect("post-acquisition errors must release the inherited lock");
        drop(FileLock::from_locked(contender));
    }

    #[test]
    fn dropping_one_shared_lock_preserves_an_independent_reader() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.lock");
        let first = open(&path);
        let second = open(&path);
        std::fs::File::try_lock_shared(&first).unwrap();
        let first = FileLock::from_locked(first);
        std::fs::File::try_lock_shared(&second).unwrap();
        let second = FileLock::from_locked(second);
        let contender = open(&path);
        drop(first);
        assert!(std::fs::File::try_lock(&contender).is_err());
        drop(second);
        std::fs::File::try_lock(&contender).unwrap();
        drop(FileLock::from_locked(contender));
    }

    #[cfg(windows)]
    #[test]
    fn locking_without_data_access_reports_io_error_instead_of_contention() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("metadata-only.lock");
        drop(open(&path));
        // LockFileEx requires data read or write access. An attributes-only
        // file is a valid owned handle but cannot acquire either lock mode.
        let metadata_only = OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES)
            .open(&path)
            .unwrap();
        for result in [metadata_only.try_lock(), metadata_only.try_lock_shared()] {
            let Err(TryLockError::Error(error)) = result else {
                panic!("a metadata-only lock must fail with an I/O error: {result:?}");
            };
            assert!(error.raw_os_error().is_some());
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum LockApi {
        Std,
        Fs2,
    }

    impl LockApi {
        fn name(self) -> &'static str {
            match self {
                Self::Std => "std",
                Self::Fs2 => "fs2",
            }
        }

        fn try_acquire(self, file: &File, shared: bool) -> Result<(), TryLockError> {
            match (self, shared) {
                (Self::Std, false) => file.try_lock(),
                (Self::Std, true) => file.try_lock_shared(),
                (Self::Fs2, _) => {
                    let result = if shared {
                        fs2::FileExt::try_lock_shared(file)
                    } else {
                        fs2::FileExt::try_lock_exclusive(file)
                    };
                    result.map_err(|error| {
                        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                            TryLockError::WouldBlock
                        } else {
                            TryLockError::Error(error)
                        }
                    })
                }
            }
        }

        fn release(self, file: &File) {
            match self {
                Self::Std => file.unlock().unwrap(),
                Self::Fs2 => fs2::FileExt::unlock(file).unwrap(),
            }
        }
    }

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    const CHILD_LOCK_PATH: &str = "CODEX_MONIT_LOCK_INTEROP_PATH";
    const CHILD_LOCK_API: &str = "CODEX_MONIT_LOCK_INTEROP_API";
    const CHILD_LOCK_SHARED: &str = "CODEX_MONIT_LOCK_INTEROP_SHARED";

    #[test]
    fn old_and_std_locks_interoperate_across_processes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("interop.lock");
        for (owner_api, contender_api) in
            [(LockApi::Fs2, LockApi::Std), (LockApi::Std, LockApi::Fs2)]
        {
            for owner_shared in [false, true] {
                let child = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "file_lock::tests::lock_interop_child_helper",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(CHILD_LOCK_PATH, &path)
                    .env(CHILD_LOCK_API, owner_api.name())
                    .env(CHILD_LOCK_SHARED, owner_shared.to_string())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap();
                let mut child = ChildGuard(child);
                let stdout = child.0.stdout.take().unwrap();
                let (events, received) = mpsc::channel();
                let reader = std::thread::spawn(move || {
                    for line in io::BufReader::new(stdout).lines() {
                        if events.send(line.unwrap()).is_err() {
                            break;
                        }
                    }
                });
                let await_event = |expected: &str| {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        let line = received
                            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                            .unwrap_or_else(|error| panic!("waiting for {expected}: {error}"));
                        if line == expected {
                            break;
                        }
                    }
                };
                await_event("LOCK_READY");

                // The ready handshake proves the owner holds its lock before
                // testing both contender modes; no timing guess establishes it.
                let contender = open(&path);
                for contender_shared in [false, true] {
                    let started = Instant::now();
                    let result = contender_api.try_acquire(&contender, contender_shared);
                    assert!(started.elapsed() < Duration::from_secs(2));
                    if owner_shared && contender_shared {
                        result.expect("independent shared readers must coexist");
                        contender_api.release(&contender);
                    } else {
                        assert!(
                            matches!(result, Err(TryLockError::WouldBlock)),
                            "{owner_api:?} -> {contender_api:?}: {result:?}"
                        );
                    }
                }
                writeln!(child.0.stdin.as_mut().unwrap(), "CONTINUE").unwrap();
                child.0.stdin.as_mut().unwrap().flush().unwrap();
                await_event("LOCK_RELEASED");
                for shared in [false, true] {
                    contender_api.try_acquire(&contender, shared).unwrap();
                    contender_api.release(&contender);
                }
                // Release is acknowledged before exit; bound shutdown too so a
                // failed helper never leaves the test process waiting forever.
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(status) = child.0.try_wait().unwrap() {
                        assert!(status.success());
                        break;
                    }
                    assert!(Instant::now() < deadline, "lock helper did not exit");
                    std::thread::sleep(Duration::from_millis(5));
                }
                reader.join().unwrap();
            }
        }
    }

    #[test]
    fn lock_interop_child_helper() {
        let Some(path) = std::env::var_os(CHILD_LOCK_PATH) else {
            return;
        };
        let api = match std::env::var(CHILD_LOCK_API).unwrap().as_str() {
            "std" => LockApi::Std,
            "fs2" => LockApi::Fs2,
            other => panic!("unknown lock API: {other}"),
        };
        let shared = std::env::var(CHILD_LOCK_SHARED).unwrap() == "true";
        let file = open(Path::new(&path));
        api.try_acquire(&file, shared).unwrap();
        println!("\nLOCK_READY");
        io::stdout().flush().unwrap();
        let mut request = String::new();
        io::stdin().read_line(&mut request).unwrap();
        assert_eq!(request.trim(), "CONTINUE");
        match api {
            LockApi::Std => drop(FileLock::from_locked(file)),
            LockApi::Fs2 => api.release(&file),
        }
        println!("LOCK_RELEASED");
        io::stdout().flush().unwrap();
    }
}
