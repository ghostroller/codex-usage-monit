#[cfg(not(windows))]
use std::fs;
use std::io;
use std::path::Path;

pub(crate) fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temporary, target)
    }
    #[cfg(windows)]
    {
        replace_file_windows(temporary, target)
    }
}

#[cfg(windows)]
fn replace_file_windows(temporary: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows paths cannot contain NUL characters",
            ));
        }
        encoded.push(0);
        Ok(encoded)
    }

    let temporary = wide_path(temporary)?;
    let target = wide_path(target)?;
    // SAFETY: both buffers are NUL-terminated and remain alive for the call.
    let replaced = unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as test_fs;

    #[test]
    fn replacement_overwrites_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("temporary");
        let target = directory.path().join("target");
        test_fs::write(&temporary, b"replacement").unwrap();
        test_fs::write(&target, b"original").unwrap();

        replace_file(&temporary, &target).unwrap();

        assert_eq!(test_fs::read(&target).unwrap(), b"replacement");
        assert!(!temporary.exists());
    }

    #[test]
    fn failed_replacement_preserves_the_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("temporary-directory");
        let target = directory.path().join("target");
        test_fs::create_dir(&temporary).unwrap();
        test_fs::write(&target, b"original").unwrap();

        assert!(replace_file(&temporary, &target).is_err());

        assert_eq!(test_fs::read(&target).unwrap(), b"original");
        assert!(temporary.is_dir());
    }
}
