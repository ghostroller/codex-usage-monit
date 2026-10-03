//! Exercise live permission changes on disposable SQLite objects with Win32.

use super::*;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
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
    CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    READ_CONTROL, WRITE_DAC,
};

struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the two descriptor-producing APIs below allocate with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

struct FixtureDacl {
    file: File,
    // The original DACL points into this allocation, so keep it until restoration.
    _original: Descriptor,
    original_dacl: *mut ACL,
    original_flags: u32,
    changed: bool,
}

impl FixtureDacl {
    fn allow_everyone_read(path: &Path, fixture_root: &Path) -> io::Result<Self> {
        assert!(path.starts_with(fixture_root));
        assert!(fs::symlink_metadata(path)?.is_file());
        let wide = crate::atomic_file::windows_wide_path(path)?;
        // SAFETY: the path is a freshly created fixture; the handle is uniquely owned.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned a valid owned file handle.
        let file = unsafe { File::from_raw_handle(handle) };
        let mut descriptor = ptr::null_mut();
        let mut original_dacl = ptr::null_mut();
        // SAFETY: the live handle and output pointers are valid for this call.
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
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: original owns the descriptor; both outputs have the required size.
        if unsafe { GetSecurityDescriptorControl(original.0, &mut control, &mut revision) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let original_flags = DACL_SECURITY_INFORMATION
            | if control & SE_DACL_PROTECTED != 0 {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
        let user = crate::source_identity::windows_current_user_sid()?;
        // SAFETY: the current-user SID is validated and kept alive by user.
        let user = unsafe { crate::windows_private_directory::sid_string(user.as_psid()) }?;
        let sddl = format!("D:P(A;;FA;;;{user})(A;;GR;;;WD)")
            .encode_utf16()
            .chain([0])
            .collect::<Vec<_>>();
        let mut descriptor = ptr::null_mut();
        // SAFETY: sddl is NUL-terminated and descriptor is a valid output pointer.
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
        // SAFETY: broadened owns the descriptor and the output pointers are valid.
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
            // Restore even if an API error occurs while applying the test DACL.
            changed: true,
        };
        guard.set_dacl(
            dacl,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        )?;
        Ok(guard)
    }

    fn set_dacl(&self, dacl: *mut ACL, flags: u32) -> io::Result<()> {
        // SAFETY: self owns the target handle; the caller keeps its DACL alive.
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

    fn restore(&mut self) -> io::Result<()> {
        if self.changed {
            self.set_dacl(self.original_dacl, self.original_flags)?;
            self.changed = false;
        }
        Ok(())
    }
}

impl Drop for FixtureDacl {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            // The normal path asserts successful restoration. Keep the rollback
            // fallback non-panicking if the test is already unwinding.
            eprintln!("failed to restore disposable SQLite fixture DACL: {error}");
        }
    }
}

#[test]
fn live_db_wal_shm_dacl_broadening_refuses_nested_use_and_rolls_back() {
    for suffix in ["", "-wal", "-shm"] {
        let directory = tempfile::tempdir().unwrap();
        let database = HistoryDatabase::new(
            &directory.path().join("state"),
            &"0123456789abcdef".parse().unwrap(),
        );
        database
            .write(|connection| set_state(connection, "committed", &true))
            .unwrap();
        let result = database.write(|connection| {
            set_state(connection, "uncommitted", &true)?;
            let object = side_file_path(database.path(), suffix);
            assert!(object.exists(), "SQLite must have opened object {suffix:?}");
            let mut changed = FixtureDacl::allow_everyone_read(&object, directory.path())?;
            // The already-open database must inspect the live ACL again before
            // invoking application code, even though its identity is unchanged.
            let fenced = database.read::<()>(|_| panic!("a public SQLite object was accepted"));
            changed.restore()?;
            assert_eq!(
                fenced.as_ref().unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "object {suffix:?}"
            );
            fenced
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        database
            .read(|connection| {
                assert_eq!(state::<bool>(connection, "committed")?, Some(true));
                assert_eq!(state::<bool>(connection, "uncommitted")?, None);
                Ok(())
            })
            .unwrap();
    }
}
