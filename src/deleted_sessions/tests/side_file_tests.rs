//! Unsafe SQLite side files must be rejected before recovery or mutation.

use super::*;

fn rejects_hard_link(suffix: &str, write: bool) {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory, "home");
    prepare_delete(&config, "original", &[record("thread", "event", 42)]).unwrap();
    let (_, ledger, _) = location(&config).unwrap();
    let sentinel = ledger.join("unrelated-private-file");
    // A non-hot journal header permits SQLite to reuse this file for a write.
    // Keeping a distinct tail makes unintended journal writes observable.
    let mut original = vec![0; 4096];
    original[1024..1040].copy_from_slice(b"private sentinel");
    LAYOUT.write_atomically(&sentinel, &original).unwrap();
    let side = ledger.join(format!("ledger.sqlite3{suffix}"));
    fs::hard_link(&sentinel, &side).unwrap();

    let result = if write {
        prepare_delete(&config, "next", &[record("new-thread", "new-event", 13)])
    } else {
        load_deleted_sessions(&config).map(|_| ())
    };
    assert!(
        fs::read(&sentinel).unwrap() == original,
        "opening {suffix} for write={write} changed a hard-linked private file"
    );
    let error = result.expect_err("hard-linked SQLite side files must be rejected");
    assert!(
        format!("{error:#}").contains("hard link"),
        "rejection must come from side-file validation: {error:#}"
    );
}

macro_rules! hard_link_case {
    ($name:ident, $suffix:literal, $write:literal) => {
        #[test]
        fn $name() {
            rejects_hard_link($suffix, $write);
        }
    };
}

hard_link_case!(journal_hard_link_read, "-journal", false);
hard_link_case!(journal_hard_link_write, "-journal", true);
hard_link_case!(wal_hard_link_read, "-wal", false);
hard_link_case!(wal_hard_link_write, "-wal", true);
hard_link_case!(shm_hard_link_read, "-shm", false);
hard_link_case!(shm_hard_link_write, "-shm", true);

#[test]
fn journal_hard_link_created_during_write_rolls_back() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory, "home");
    prepare_delete(&config, "original", &[record("thread", "event", 42)]).unwrap();
    let (_, ledger, _) = location(&config).unwrap();
    let result = with_database(&config, true, |connection| {
        connection.execute(
            "INSERT INTO operations VALUES ('must-roll-back', 'fixture')",
            [],
        )?;
        fs::hard_link(
            ledger.join("ledger.sqlite3-journal"),
            ledger.join("journal-alias"),
        )?;
        Ok(())
    });
    let error = result.expect_err("a newly aliased journal must prevent commit");
    assert!(format!("{error:#}").contains("hard link"), "{error:#}");
    assert_original_operation_only(&config);
}

fn assert_original_operation_only(config: &CollectConfig) {
    with_database(config, false, |connection| {
        let count: i64 =
            connection.query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?;
        assert_eq!(
            count, 1,
            "the unsafe journal must roll back the entire operation"
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(
        load_deleted_sessions(config).unwrap()[0]
            .token_usage()
            .total_tokens,
        42
    );
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    };

    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the descriptor APIs below allocate with LocalAlloc.
            unsafe { LocalFree(self.0) };
        }
    }

    struct FixtureDacl {
        file: File,
        _original: Descriptor,
        original_dacl: *mut ACL,
        original_flags: u32,
    }

    impl FixtureDacl {
        fn allow_everyone_read(path: &Path, fixture_root: &Path) -> io::Result<Self> {
            assert!(path.starts_with(fixture_root));
            assert!(fs::symlink_metadata(path)?.is_file());
            let wide = crate::atomic_file::windows_wide_path(path)?;
            // SAFETY: this NUL-terminated path names a disposable fixture. The
            // returned handle is uniquely owned; no file contents are modified.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    READ_CONTROL | WRITE_DAC,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateFileW returned an owned live handle.
            let file = unsafe { File::from_raw_handle(handle) };
            let mut descriptor = ptr::null_mut();
            let mut original_dacl = ptr::null_mut();
            // SAFETY: file owns the handle and all outputs are writable.
            let status = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut original_dacl,
                    ptr::null_mut(),
                    &mut descriptor,
                )
            };
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            let original = Descriptor(descriptor);
            let (mut control, mut revision) = (0, 0);
            // SAFETY: original keeps the descriptor and its DACL alive.
            if unsafe { GetSecurityDescriptorControl(original.0, &mut control, &mut revision) } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let original_flags = DACL_SECURITY_INFORMATION
                | if control & SE_DACL_PROTECTED != 0 {
                    PROTECTED_DACL_SECURITY_INFORMATION
                } else {
                    UNPROTECTED_DACL_SECURITY_INFORMATION
                };
            let user = crate::source_identity::windows_current_user_sid()?;
            // SAFETY: user owns the validated SID throughout the conversion.
            let user = unsafe { crate::windows_private_directory::sid_string(user.as_psid()) }?;
            let sddl = format!("D:P(A;;FA;;;{user})(A;;GR;;;WD)")
                .encode_utf16()
                .chain([0])
                .collect::<Vec<_>>();
            let mut descriptor = ptr::null_mut();
            // SAFETY: sddl is NUL-terminated and descriptor is writable.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let broadened = Descriptor(descriptor);
            let (mut present, mut dacl, mut defaulted) = (0, ptr::null_mut(), 0);
            // SAFETY: broadened owns the descriptor; outputs have correct types.
            if unsafe {
                GetSecurityDescriptorDacl(broadened.0, &mut present, &mut dacl, &mut defaulted)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            assert_ne!(present, 0);
            assert!(!dacl.is_null());
            let guard = Self {
                file,
                _original: original,
                original_dacl,
                original_flags,
            };
            guard.set_dacl(
                dacl,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            )?;
            Ok(guard)
        }

        fn set_dacl(&self, dacl: *mut ACL, flags: u32) -> io::Result<()> {
            // SAFETY: self owns the handle; the caller keeps the DACL alive.
            let status = unsafe {
                SetSecurityInfo(
                    self.file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    flags,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            Ok(())
        }
    }

    impl Drop for FixtureDacl {
        fn drop(&mut self) {
            if let Err(error) = self.set_dacl(self.original_dacl, self.original_flags) {
                eprintln!("failed to restore disposable ledger fixture DACL: {error}");
            }
        }
    }

    fn rejects_broad_acl(suffix: &str, write: bool) {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        prepare_delete(&config, "original", &[record("thread", "event", 42)]).unwrap();
        let (_, ledger, _) = location(&config).unwrap();
        let side = ledger.join(format!("ledger.sqlite3{suffix}"));
        LAYOUT.write_atomically(&side, b"").unwrap();
        let _dacl = FixtureDacl::allow_everyone_read(&side, directory.path()).unwrap();
        let result = if write {
            prepare_delete(&config, "next", &[record("new-thread", "new-event", 13)])
        } else {
            load_deleted_sessions(&config).map(|_| ())
        };
        let error = result.expect_err("SQLite side files with a broad DACL must be rejected");
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied),
            "rejection must come from live side-file ACL validation: {error:#}"
        );
    }

    macro_rules! broad_acl_case {
        ($name:ident, $suffix:literal, $write:literal) => {
            #[test]
            fn $name() {
                rejects_broad_acl($suffix, $write);
            }
        };
    }

    broad_acl_case!(journal_broad_acl_read, "-journal", false);
    broad_acl_case!(journal_broad_acl_write, "-journal", true);
    broad_acl_case!(wal_broad_acl_read, "-wal", false);
    broad_acl_case!(wal_broad_acl_write, "-wal", true);
    broad_acl_case!(shm_broad_acl_read, "-shm", false);
    broad_acl_case!(shm_broad_acl_write, "-shm", true);

    #[test]
    fn journal_acl_broadened_during_write_rolls_back() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        prepare_delete(&config, "original", &[record("thread", "event", 42)]).unwrap();
        let (_, ledger, _) = location(&config).unwrap();
        let mut dacl = None;
        let result = with_database(&config, true, |connection| {
            connection.execute(
                "INSERT INTO operations VALUES ('must-roll-back', 'fixture')",
                [],
            )?;
            dacl = Some(FixtureDacl::allow_everyone_read(
                &ledger.join("ledger.sqlite3-journal"),
                directory.path(),
            )?);
            Ok(())
        });
        drop(dacl);
        let error = result.expect_err("a newly broadened journal DACL must prevent commit");
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied),
            "{error:#}"
        );
        assert_original_operation_only(&config);
    }
}
