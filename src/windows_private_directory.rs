//! Create application directories with a private ACL from the first instant.
//! Existing directories are validated, never silently re-permissioned.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

use crate::source_identity::{
    reject_windows_reparse_components, validate_windows_private_directory, windows_current_user_sid,
};

/// Caller must supply a valid SID that remains alive for this call.
pub(crate) unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = ptr::null_mut();
    // SAFETY: the caller guarantees a valid SID; text is an output pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let allocation = SecurityDescriptor(text.cast());
    let mut length = 0;
    // SAFETY: the API returns a NUL-terminated, LocalAlloc-owned UTF-16 string.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        drop(allocation);
        Ok(result)
    }
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: these API allocations are uniquely owned and use LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = wide_string(OsStr::new(sddl))?;
        let mut descriptor = ptr::null_mut();
        // SAFETY: wide is NUL-terminated; descriptor is a valid output pointer.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }

    fn private() -> io::Result<Self> {
        let user = windows_current_user_sid()?;
        // SAFETY: user owns the validated token SID.
        let user = unsafe { sid_string(user.as_psid()) }?;
        // P prevents parent ACL inheritance. OI/CI give new files/directories
        // the same access policy; the owner is TokenUser even when elevated.
        Self::from_sddl(&format!(
            "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        ))
    }
}

/// Open a diagnostic file with a protected DACL at creation time. Existing
/// files must already be private; never change another file's permissions.
pub(crate) fn open_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        GetFileInformationByHandle, OPEN_ALWAYS,
    };
    reject_windows_reparse_components(path, "diagnostic log")?;
    let descriptor = SecurityDescriptor::private()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide = crate::atomic_file::windows_wide_path(path)?;
    // SAFETY: all inputs remain alive; the returned handle is uniquely owned.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ,
            &attributes,
            OPEN_ALWAYS,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a valid, owned file handle.
    let file = unsafe { fs::File::from_raw_handle(handle) };
    crate::source_identity::validate_windows_private_file(path, &file, "diagnostic log")?;
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the file owns handle and information is a valid output pointer.
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.nNumberOfLinks != 1 {
        return Err(io::Error::other("diagnostic log must not have hard links"));
    }
    Ok(file)
}

fn wide_string(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide = value.encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn create_with_descriptor(path: &Path, descriptor: &SecurityDescriptor) -> io::Result<()> {
    let wide = crate::atomic_file::windows_wide_path(path)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: the path and descriptor remain alive throughout CreateDirectoryW.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Same AlreadyExists behavior as fs::create_dir, with a private creation ACL.
pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
    reject_windows_reparse_components(path, "private directory")?;
    create_with_descriptor(path, &SecurityDescriptor::private()?)?;
    validate_windows_private_directory(path, "private directory")
}

/// Like create_dir_all, except each newly created directory is private. An
/// existing ancestor is not modified or required to have a private ACL.
pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    reject_windows_reparse_components(path, "private directory")?;
    let path = std::path::absolute(path)?;
    let mut missing = Vec::new();
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "private directory ancestor is not a directory: {}",
                        ancestor.display()
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(ancestor),
            Err(error) => return Err(error),
        }
    }
    let descriptor = SecurityDescriptor::private()?;
    for directory in missing.into_iter().rev() {
        reject_windows_reparse_components(directory, "private directory")?;
        match create_with_descriptor(directory, &descriptor) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // A concurrent creator's directory must pass the same validation.
        validate_windows_private_directory(directory, "private directory")?;
    }
    validate_windows_private_directory(&path, "private directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_identity::{SourceIdentityStore, validate_windows_private_file};

    fn public_parent(path: &Path) {
        let user = windows_current_user_sid().unwrap();
        let user = unsafe { sid_string(user.as_psid()) }.unwrap();
        let descriptor = SecurityDescriptor::from_sddl(&format!(
            "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GR;;;WD)"
        ))
        .unwrap();
        create_with_descriptor(path, &descriptor).unwrap();
    }

    #[test]
    fn windows_private_directory_identity_initializes_beneath_public_parent() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("public");
        public_parent(&parent);
        let state = parent.join("nested/state");
        let store = SourceIdentityStore::at_path(state.join("source-identity.json"));
        let identity = store.load_or_create().unwrap();
        assert_eq!(store.load().unwrap(), identity);
        for name in [
            "source-identity.json",
            "source-identity.anchor",
            "source-identity.lock",
        ] {
            let path = state.join(name);
            let file = fs::File::open(&path).unwrap();
            validate_windows_private_file(&path, &file, "test file").unwrap();
        }
        // The broad parent's ACL is untouched.
        assert_eq!(
            validate_windows_private_directory(&parent, "parent")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn windows_private_directory_existing_public_state_is_not_repaired() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        public_parent(&state);
        fs::write(state.join("sentinel"), b"preserve me").unwrap();
        assert_eq!(
            create_dir_all(&state).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(state.join("sentinel")).unwrap(), b"preserve me");
        assert!(validate_windows_private_directory(&state, "state").is_err());
    }

    #[test]
    fn windows_private_directory_other_first_writers_keep_shared_root_private() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("public");
        public_parent(&parent);
        for first_writer in ["ui", "open", "cache", "remotes"] {
            let state = parent.join(first_writer);
            match first_writer {
                "ui" => {
                    crate::ui_state::UiStateStore::new(state.join("tui-state.json"))
                        .save(&crate::ui_state::UiState::default())
                        .unwrap();
                }
                "open" => {
                    crate::open_config::OpenConfigStore::new(state.join("open.json"))
                        .load_or_create()
                        .unwrap();
                }
                "cache" => {
                    crate::cache::write_private_atomically(&state.join("cache/fixture.json"), b"{}")
                        .unwrap()
                }
                "remotes" => {
                    crate::remotes_config::RemotesConfigStore::new(state.join("remotes.json"))
                        .load_or_create()
                        .unwrap();
                }
                _ => unreachable!(),
            }
            SourceIdentityStore::at_path(state.join("source-identity.json"))
                .load_or_create()
                .unwrap();
        }
    }

    #[test]
    fn windows_private_directory_diagnostic_identifies_path_sid_and_inheritance() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("public");
        public_parent(&parent);
        let state = parent.join("state");
        fs::create_dir(&state).unwrap();
        let message = validate_windows_private_directory(&state, "state")
            .unwrap_err()
            .to_string();
        assert!(message.contains(&state.display().to_string()), "{message}");
        assert!(message.contains("SID=S-1-1-0"), "{message}");
        assert!(message.contains("inherited=true"), "{message}");
    }

    #[test]
    fn windows_private_directory_concurrent_creation_converges() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("public");
        public_parent(&parent);
        let path = parent.join("nested/state");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| create_dir_all(&path).unwrap());
            }
        });
        validate_windows_private_directory(&path, "state").unwrap();
    }

    #[test]
    fn windows_private_directory_supports_paths_beyond_max_path() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("public");
        public_parent(&parent);
        let mut state = parent;
        for _ in 0..6 {
            state.push("long-private-state-component-0123456789");
        }
        assert!(state.as_os_str().encode_wide().count() > 260);
        create_dir_all(&state).unwrap();
        let child = state.join("child");
        create_dir(&child).unwrap();
        validate_windows_private_directory(&child, "long directory").unwrap();
        let store = SourceIdentityStore::at_path(child.join("source-identity.json"));
        let identity = store.load_or_create().unwrap();
        assert_eq!(store.load().unwrap(), identity);
    }

    #[test]
    fn windows_private_directory_rejects_file_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, b"unchanged").unwrap();
        assert!(create_dir_all(&file.join("state")).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"unchanged");
    }

    #[test]
    fn windows_private_directory_rejects_junction_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let junction = temp.path().join("junction");
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let result = create_dir_all(&junction.join("state"));
        // Remove only the junction, never recurse into its target.
        fs::remove_dir(&junction).unwrap();
        assert!(result.is_err());
        assert!(!outside.join("state").exists());
    }
}
