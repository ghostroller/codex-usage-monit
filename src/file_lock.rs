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
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::io;
    use std::path::Path;

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

    #[cfg(unix)]
    #[test]
    fn drop_releases_inherited_shared_and_exclusive_locks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.lock");
        for shared in [false, true] {
            let file = open(&path);
            if shared {
                fs2::FileExt::try_lock_shared(&file).unwrap();
            } else {
                fs2::FileExt::try_lock_exclusive(&file).unwrap();
            }
            let guard = FileLock::from_locked(file);
            let inherited = guard.try_clone().unwrap();
            let contender = open(&path);
            assert!(fs2::FileExt::try_lock_exclusive(&contender).is_err());
            drop(guard);
            let acquired = fs2::FileExt::try_lock_exclusive(&contender);
            drop(inherited);
            acquired.expect("a completed owner must release its inherited lock");
            drop(FileLock::from_locked(contender));
        }
    }

    #[cfg(unix)]
    #[test]
    fn error_after_acquisition_releases_an_inherited_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.lock");
        let file = open(&path);
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        let inherited = file.try_clone().unwrap();
        let result = (|| -> io::Result<()> {
            let _guard = FileLock::from_locked(file);
            std::fs::metadata(temp.path().join("missing-state"))?;
            Ok(())
        })();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        let contender = open(&path);
        let acquired = fs2::FileExt::try_lock_exclusive(&contender);
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
        fs2::FileExt::try_lock_shared(&first).unwrap();
        let first = FileLock::from_locked(first);
        fs2::FileExt::try_lock_shared(&second).unwrap();
        let second = FileLock::from_locked(second);
        let contender = open(&path);
        drop(first);
        assert!(fs2::FileExt::try_lock_exclusive(&contender).is_err());
        drop(second);
        fs2::FileExt::try_lock_exclusive(&contender).unwrap();
        drop(FileLock::from_locked(contender));
    }
}
