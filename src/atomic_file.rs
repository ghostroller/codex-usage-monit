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
pub(crate) fn windows_wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows paths cannot contain NUL characters",
        ));
    }
    // Resolve relative paths, separators and dot components before adding a
    // verbatim prefix. This is lexical: neither endpoint must exist, and a
    // final symlink is never resolved to the file it points to.
    let absolute = std::path::absolute(path)?;
    let encoded = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    // Win32's legacy directory limit reserves 12 characters below MAX_PATH.
    // Match std's long-path support without relying on process manifests or
    // machine-wide LongPathsEnabled settings for our direct Win32 calls.
    let mut encoded = if encoded.len() + 1 >= 248 {
        match absolute.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(_) => r"\\?\".encode_utf16().chain(encoded).collect(),
                Prefix::UNC(_, _) => r"\\?\UNC\"
                    .encode_utf16()
                    .chain(encoded.into_iter().skip(2))
                    .collect(),
                _ => encoded,
            },
            _ => encoded,
        }
    } else {
        encoded
    };
    encoded.push(0);
    Ok(encoded)
}

#[cfg(windows)]
fn replace_file_windows(temporary: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        MoveFileExW,
    };

    if !std::fs::symlink_metadata(temporary)?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "replacement source must be a regular file",
        ));
    }

    let temporary_wide = windows_wide_path(temporary)?;
    let target_wide = windows_wide_path(target)?;
    // SAFETY: both buffers are NUL-terminated and remain alive for the call.
    let replaced = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
            return Err(error);
        }
        // A reader may retain the old destination for an identity/CAS check.
        // std retries access-denied replacements with FileRenameInfoEx and
        // POSIX semantics, keeping that reader attached to the old file.
        // Retain and flush the replacement handle across this fallback. It
        // has no WRITE_THROUGH rename flag, so this is a file flush, not a
        // guarantee that the parent directory entry survives a power loss.
        let replacement = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(temporary)?;
        replacement.sync_all()?;
        std::fs::rename(temporary, target)?;
        replacement.sync_all()
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

    #[test]
    fn replacement_preserves_an_open_readers_original_file() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("temporary");
        let target = directory.path().join("target");
        test_fs::write(&temporary, b"replacement").unwrap();
        test_fs::write(&target, b"original").unwrap();
        let mut reader = test_fs::File::open(&target).unwrap();

        replace_file(&temporary, &target).unwrap();

        let mut original = Vec::new();
        reader.read_to_end(&mut original).unwrap();
        assert_eq!(original, b"original");
        assert_eq!(test_fs::read(&target).unwrap(), b"replacement");
        assert!(!temporary.exists());
    }

    #[cfg(windows)]
    #[test]
    fn replacement_creates_and_overwrites_files_beyond_max_path() {
        use std::os::windows::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let nested = directory
            .path()
            .join("long-directory-component".repeat(4))
            .join("another-long-component".repeat(4))
            .join("final-directory-component".repeat(4));
        test_fs::create_dir_all(&nested).unwrap();
        let temporary = nested.join("temporary");
        let target = nested.join("target");
        assert!(target.as_os_str().encode_wide().count() > 260);

        for contents in [b"first".as_slice(), b"replacement".as_slice()] {
            test_fs::write(&temporary, contents).unwrap();
            replace_file(&temporary, &target).unwrap();
            assert_eq!(test_fs::read(&target).unwrap(), contents);
            assert!(!temporary.exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_wide_paths_normalize_long_disk_and_unc_paths_without_io() {
        let component = "directory".repeat(8);
        let nested = format!(r"{component}\{component}\{component}\{component}");
        for (input, expected) in [
            (
                format!(r"C:/missing/ignored/../{nested}/target"),
                format!(r"\\?\C:\missing\{nested}\target"),
            ),
            (
                format!(r"\\server\share/{nested}/target"),
                format!(r"\\?\UNC\server\share\{nested}\target"),
            ),
            (
                r"\\?\C:\missing\target".into(),
                r"\\?\C:\missing\target".into(),
            ),
        ] {
            let mut expected = expected.encode_utf16().collect::<Vec<_>>();
            expected.push(0);
            assert_eq!(windows_wide_path(Path::new(&input)).unwrap(), expected);
        }
        assert_eq!(
            windows_wide_path(Path::new("invalid\0path"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput,
        );
    }
}
