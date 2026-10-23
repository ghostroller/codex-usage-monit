//! Machine installation ACLs are independent of current-user private stores.
//!
//! Win32 is needed here for token/account SIDs, known folders, and explicit
//! owner/DACL inspection; standard filesystem permissions cannot express them.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    fs, io,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_ALL, GENERIC_WRITE, LocalFree},
    Security::{
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        *,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL,
        FILE_DELETE_CHILD, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC,
        WRITE_OWNER,
    },
    System::{
        Com::CoTaskMemFree,
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
    UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramFiles, FOLDERID_RoamingAppData,
        SHGetKnownFolderPath,
    },
};

pub(super) fn wide(value: &str) -> Result<Vec<u16>> {
    ensure!(!value.contains('\0'), "machine_invalid: NUL is not allowed");
    Ok(value.encode_utf16().chain(Some(0)).collect())
}

/// Copies a Windows UTF-16 string, rejecting strings beyond the scan limit.
///
/// # Safety
///
/// A non-null `value` must be aligned and readable in one allocation through
/// its first NUL, or for 32,768 code units if no earlier NUL exists. That memory
/// must remain alive and unmodified for this call. The scan limit bounds work;
/// it cannot make a dangling or short unterminated buffer safe to read.
pub(super) unsafe fn read_wide(value: *const u16) -> Result<String> {
    ensure!(!value.is_null(), "machine_invalid: missing Windows string");
    for length in 0..32768 {
        // SAFETY: the caller guarantees readable, aligned storage through this
        // index until the first NUL or the scan limit, without concurrent writes.
        if unsafe { *value.add(length) } == 0 {
            // SAFETY: the scanned prefix is initialized, immutable u16 storage
            // in one live allocation, and the bounded length fits in isize.
            let text = unsafe { std::slice::from_raw_parts(value, length) };
            return Ok(String::from_utf16_lossy(text));
        }
    }
    anyhow::bail!("machine_invalid: oversized Windows string")
}

pub(super) fn require_administrator() -> Result<()> {
    let mut token = ptr::null_mut();
    ensure!(
        // SAFETY: the current-process pseudo-handle is accepted by this API;
        // token is writable output storage for a separately owned query handle.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0,
        "machine_admin_required: {}",
        io::Error::last_os_error()
    );
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    // SAFETY: token is live with TOKEN_QUERY, and elevation is aligned writable
    // storage matching TokenElevation and the supplied size; returned is DWORD
    // output storage. Neither pointer escapes this synchronous call.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of_val(&elevation) as u32,
            &mut returned,
        )
    };
    // SAFETY: OpenProcessToken succeeded and this function owns its handle;
    // GetTokenInformation does not consume it, and no later call uses it.
    unsafe { CloseHandle(token) };
    ensure!(
        ok != 0 && elevation.TokenIsElevated != 0,
        "machine_admin_required: open an elevated terminal for machine service mutations"
    );
    Ok(())
}

pub(super) fn account_sid(account: &str) -> Result<String> {
    ensure!(
        !account.trim().is_empty(),
        "machine_account_required: specify a named service account"
    );
    let name = wide(account)?;
    let (mut bytes, mut domain_len, mut kind) = (0, 0, 0);
    // SAFETY: name is terminated and live; zero sizes with null buffers request
    // required lengths, and the three scalar outputs are writable locals.
    unsafe {
        LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            ptr::null_mut(),
            &mut bytes,
            ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    ensure!(
        bytes > 0 && bytes <= 65536 && domain_len <= 32768,
        "machine_account_invalid: cannot resolve account {account}"
    );
    let mut sid = vec![0_u32; (bytes as usize).div_ceil(4)];
    let mut domain = vec![0_u16; domain_len as usize];
    ensure!(
        // SAFETY: name remains terminated. sid is DWORD-aligned and covers bytes;
        // domain covers domain_len UTF-16 units. Both are exclusively writable
        // and their lengths are supplied in the API's respective units.
        unsafe {
            LookupAccountNameW(
                ptr::null(),
                name.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut bytes,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        } != 0,
        "machine_account_invalid: {}",
        io::Error::last_os_error()
    );
    ensure!(
        kind == SidTypeUser,
        "machine_account_invalid: service account must be a user, not a group"
    );
    // SAFETY: successful LookupAccountNameW initialized a complete SID in the
    // aligned sid allocation, which remains alive throughout conversion.
    let sid = unsafe { crate::windows_private_directory::sid_string(sid.as_mut_ptr().cast()) }?;
    ensure!(
        !matches!(sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20"),
        "machine_account_invalid: this mode requires an explicitly provisioned named user account"
    );
    Ok(sid)
}

pub(super) fn current_sid() -> Result<String> {
    let user = crate::source_identity::windows_current_user_sid()?;
    // SAFETY: user owns the token-information storage containing this valid SID
    // and is kept alive until sid_string finishes reading it.
    Ok(unsafe { crate::windows_private_directory::sid_string(user.as_psid()) }?)
}

fn known_folder(id: &windows_sys::core::GUID) -> Result<PathBuf> {
    let mut value = ptr::null_mut();
    // SAFETY: id borrows a live GUID; null token selects the current user, and
    // value is writable output storage for the API's allocated UTF-16 string.
    let result = unsafe { SHGetKnownFolderPath(id, 0, ptr::null_mut(), &mut value) };
    ensure!(
        result >= 0 && !value.is_null(),
        "machine_profile_unavailable: Windows known folder failed with HRESULT {result:#x}"
    );
    // SAFETY: successful SHGetKnownFolderPath returned a non-null terminated
    // UTF-16 allocation, which is neither modified nor freed during this copy.
    let text = unsafe { read_wide(value) };
    // SAFETY: SHGetKnownFolderPath transfers a CoTaskMem allocation to us; the
    // copy above has finished, and we release it once with its matching allocator.
    unsafe { CoTaskMemFree(value.cast()) };
    Ok(PathBuf::from(OsString::from_wide(
        &text?.encode_utf16().collect::<Vec<_>>(),
    )))
}

pub(super) fn machine_base() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles)?.join("codex-usage-monit-machine"))
}

pub(super) fn runtime_environment() -> Result<Vec<(&'static str, PathBuf)>> {
    let profile_context = "machine_profile_unavailable: provision and load the named service account's Windows profile before starting SCM; the installing administrator's profile is never used";
    let profile = known_folder(&FOLDERID_Profile).context(profile_context)?;
    let local = known_folder(&FOLDERID_LocalAppData).context(profile_context)?;
    let roaming = known_folder(&FOLDERID_RoamingAppData).context(profile_context)?;
    Ok(vec![
        ("USERPROFILE", profile.clone()),
        ("HOME", profile),
        ("LOCALAPPDATA", local),
        ("APPDATA", roaming),
    ])
}

struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: Descriptor uniquely owns a buffer allocated by the security
        // APIs with LocalAlloc. No interior owner/ACL/SID pointer is used after
        // this guard is dropped, and LocalFree is the required release API.
        unsafe { LocalFree(self.0) };
    }
}
fn descriptor(reader: &str) -> Result<Descriptor> {
    // A user granted read/execute cannot replace machine code or configuration.
    let sddl = wide(&format!(
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;{reader})"
    ))?;
    let mut descriptor = ptr::null_mut();
    ensure!(
        // SAFETY: sddl is a live terminated string; descriptor is writable output
        // storage and the optional length output is null. Success transfers a
        // LocalAlloc descriptor that is immediately guarded below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } != 0,
        "machine_acl_failed: {}",
        io::Error::last_os_error()
    );
    Ok(Descriptor(descriptor))
}

pub(super) fn create_directory(path: &Path, reader: &str) -> Result<()> {
    crate::source_identity::reject_windows_reparse_components(path, "machine directory")?;
    if path.exists() {
        return validate_machine_path(path);
    }
    let descriptor = descriptor(reader)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path_wide = crate::atomic_file::windows_wide_path(path)?;
    ensure!(
        // SAFETY: path_wide is terminated; attributes has the required size and
        // points to the live descriptor. Windows consumes both during the call.
        unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } != 0,
        "machine_directory_failed: {}",
        io::Error::last_os_error()
    );
    validate_machine_path(path)
}

pub(super) fn create_file(path: &Path, reader: &str) -> Result<fs::File> {
    use std::os::windows::io::FromRawHandle;
    crate::source_identity::reject_windows_reparse_components(path, "machine file")?;
    let descriptor = descriptor(reader)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide = crate::atomic_file::windows_wide_path(path)?;
    // SAFETY: wide is terminated and attributes references a live descriptor
    // with its correct size. CREATE_NEW has no template handle, and none of
    // these borrowed buffers is retained by CreateFileW after it returns.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    ensure!(
        handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
        "machine_file_failed: {}",
        io::Error::last_os_error()
    );
    // SAFETY: CreateFileW succeeded, yielding a handle owned only here. This
    // transfers its sole close responsibility to File; no raw owner remains.
    Ok(unsafe { fs::File::from_raw_handle(handle) })
}

pub(super) fn validate_machine_path(path: &Path) -> Result<()> {
    validate_path_security(path, PathTrust::Machine)
}

pub(super) fn validate_runtime_path(path: &Path, account_sid: &str) -> Result<()> {
    validate_path_security(path, PathTrust::Runtime(account_sid))
}

pub(super) fn validate_executable_path(path: &Path, account_sid: &str) -> Result<()> {
    validate_executable_components(path, account_sid, path.ancestors().skip(1))
}

fn validate_executable_components<'a>(
    path: &Path,
    account_sid: &str,
    parents: impl Iterator<Item = &'a Path>,
) -> Result<()> {
    ensure!(
        path.is_absolute() && fs::symlink_metadata(path)?.is_file(),
        "machine_codex_invalid: --codex-bin must name an existing absolute executable file"
    );
    validate_runtime_path(path, account_sid)
        .with_context(|| format!("machine_codex_untrusted: {}", path.display()))?;
    for (index, parent) in parents.enumerate() {
        ensure!(
            fs::symlink_metadata(parent)?.is_dir(),
            "machine_codex_invalid: executable ancestor is not a directory"
        );
        validate_path_security(
            parent,
            PathTrust::ExecutableDirectory {
                account_sid,
                direct: index == 0,
            },
        )
        .with_context(|| format!("machine_codex_ancestor_untrusted: {}", parent.display()))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PathTrust<'a> {
    Machine,
    Runtime(&'a str),
    PrivateOutput(&'a str),
    ExecutableDirectory { account_sid: &'a str, direct: bool },
}

pub(super) fn validate_private_output(
    path: &Path,
    account_sid: &str,
    directory: bool,
) -> Result<()> {
    crate::source_identity::reject_windows_reparse_components(
        path,
        "private machine recorder output",
    )?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                },
                "machine_output_invalid: unexpected output type: {}",
                path.display()
            );
            validate_path_security(path, PathTrust::PrivateOutput(account_sid))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let ancestor = path
                .ancestors()
                .skip(1)
                .find(|path| path.exists())
                .context("machine_output_invalid: missing output has no existing ancestor")?;
            ensure!(
                ancestor.is_dir(),
                "machine_output_invalid: ancestor is not a directory"
            );
            // Newly created descendants get the real service token's private
            // descriptor. An existing output itself must already be private;
            // neither installer nor runtime silently changes ownership/ACLs.
            validate_runtime_path(ancestor, account_sid)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_path_security(path: &Path, policy: PathTrust<'_>) -> Result<()> {
    let runtime_writer = match policy {
        PathTrust::Machine => None,
        PathTrust::Runtime(sid) | PathTrust::PrivateOutput(sid) => Some(sid),
        PathTrust::ExecutableDirectory { account_sid, .. } => Some(account_sid),
    };
    let private = matches!(policy, PathTrust::PrivateOutput(_));
    crate::source_identity::reject_windows_reparse_components(path, "service path")?;
    let wide = crate::atomic_file::windows_wide_path(path)?;
    let (mut owner, mut acl, mut descriptor) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
    // SAFETY: wide is terminated and the three output pointers are writable.
    // OWNER/DACL flags match the requested outputs; success returns a separately
    // allocated descriptor containing the owner/ACL pointers, guarded below.
    let error = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    ensure!(error == 0, "machine_acl_unverifiable: Win32 error {error}");
    let _descriptor = Descriptor(descriptor);
    ensure!(
        !owner.is_null() && !acl.is_null(),
        "machine_acl_untrusted: owner/DACL is absent"
    );
    // SAFETY: owner is the non-null SID returned inside the live descriptor;
    // conversion only reads it, and _descriptor outlives the returned String.
    let owner = unsafe { crate::windows_private_directory::sid_string(owner) }?;
    // Windows owns Program Files and volume roots as this fixed service SID.
    // Only executable ancestry accepts it; machine receipts and private output
    // retain their existing Administrators/SYSTEM/target-account policies.
    const TRUSTED_INSTALLER: &str =
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    let trusted = |sid: &str| {
        trusted_writer(sid, runtime_writer)
            || (matches!(policy, PathTrust::ExecutableDirectory { .. }) && sid == TRUSTED_INSTALLER)
    };
    ensure!(
        if private {
            runtime_writer == Some(owner.as_str())
        } else {
            trusted(&owner)
        },
        "machine_acl_untrusted: existing private output must be owned by the service account; machine code must be owned by Administrators/SYSTEM"
    );
    let mut control = 0;
    let mut revision = 0;
    ensure!(
        // SAFETY: descriptor is the live result of GetNamedSecurityInfoW; both
        // scalar output pointers are properly aligned writable locals.
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } != 0,
        "machine_acl_unverifiable: {}",
        io::Error::last_os_error()
    );
    // Child files may inherit the protected directory policy. Validate actual
    // grants rather than assuming inheritance alone implies a safe policy.
    let mut write_mask = GENERIC_ALL
        | GENERIC_WRITE
        | DELETE
        | WRITE_DAC
        | WRITE_OWNER
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | FILE_DELETE_CHILD;
    if matches!(policy, PathTrust::ExecutableDirectory { direct: false, .. }) {
        // Creating an unrelated sibling cannot replace an existing protected
        // component. Default drive ACLs allow Users to create directories.
        write_mask &= !(FILE_WRITE_DATA | FILE_APPEND_DATA);
        if path.parent().is_none() {
            // A volume root itself cannot be renamed/deleted or converted to
            // a junction. Its child deletion and ACL/owner rights still matter.
            write_mask = GENERIC_ALL | WRITE_DAC | WRITE_OWNER | FILE_DELETE_CHILD;
        }
    }
    // SAFETY: acl is the non-null ACL returned in the live descriptor allocation,
    // which is a private snapshot and is never mutated during this traversal.
    let ace_count = unsafe { (*acl).AceCount };
    for index in 0..u32::from(ace_count) {
        let mut ace = ptr::null_mut();
        ensure!(
            // SAFETY: acl remains alive and unmodified; index is below its
            // reported AceCount, and ace is writable pointer output storage.
            unsafe { GetAce(acl, index, &mut ace) } != 0,
            "machine_acl_unverifiable: cannot inspect ACE"
        );
        // SAFETY: successful GetAce returns a DWORD-aligned ACE in the live
        // descriptor; every ACE begins with an initialized ACE_HEADER.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if matches!(policy, PathTrust::ExecutableDirectory { .. })
            && u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
        {
            // This entry does not grant access to this directory. Every
            // existing child on the executable path is checked separately.
            continue;
        }
        // ACCESS_ALLOWED_ACE_TYPE=0, ACCESS_DENIED_ACE_TYPE=1. Object/callback
        // ACEs require additional evaluation and are deliberately rejected.
        ensure!(
            header.AceType == 0 || header.AceType == 1,
            "machine_acl_untrusted: unsupported ACE type"
        );
        if header.AceType == 0 {
            // SAFETY: the checked ACE type identifies ACCESS_ALLOWED_ACE, whose
            // initialized fixed prefix is DWORD-aligned in the live descriptor.
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            // SAFETY: this OS-supplied access-allowed ACE contains a complete SID
            // at SidStart, followed by its variable-length tail. Derive the SID
            // pointer from the original ACE pointer, not a reference to the
            // fixed-size prefix, so it retains access to that tail allocation.
            let sid = unsafe {
                crate::windows_private_directory::sid_string(
                    ace.cast::<u8>()
                        .add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
                        .cast(),
                )
            }?;
            ensure!(
                (if private {
                    allowed.Mask == 0
                } else {
                    allowed.Mask & write_mask == 0
                }) || trusted(&sid),
                "machine_acl_untrusted: unauthorized access to private output or write access to machine code/configuration"
            );
        }
    }
    Ok(())
}

pub(super) fn read_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    use io::Read;
    validate_machine_path(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= maximum,
        "machine_file_invalid: oversized/nonregular file"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= maximum,
        "machine_file_invalid: oversized file"
    );
    Ok(bytes)
}

fn trusted_writer(sid: &str, runtime_writer: Option<&str>) -> bool {
    matches!(sid, "S-1-5-18" | "S-1-5-32-544") || runtime_writer == Some(sid)
}

pub(super) fn write_file(path: &Path, reader: &str, bytes: &[u8]) -> Result<()> {
    use io::Write;
    let parent = path.parent().context("machine file has no parent")?;
    validate_machine_path(parent)?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("machine_staging_failed: {error}"))?;
    let temporary = parent.join(format!(".machine-{}.tmp", super::digest(&random)));
    let result = (|| {
        let mut file = create_file(&temporary, reader)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if path.exists() {
            validate_machine_path(path)?;
        }
        crate::atomic_file::replace_file(&temporary, path)?;
        validate_machine_path(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scm_read_wide_obeys_termination_and_scan_limit() {
        // SAFETY: null is explicitly accepted and rejected before dereference.
        assert!(unsafe { read_wide(ptr::null()) }.is_err());
        let empty = [0_u16];
        // SAFETY: empty is aligned, live, immutable, and terminated in bounds.
        assert_eq!(unsafe { read_wide(empty.as_ptr()) }.unwrap(), "");
        let text = [b'A' as u16, 0xd800, 0, b'B' as u16];
        // SAFETY: text remains live and immutable through its in-bounds NUL.
        assert_eq!(unsafe { read_wide(text.as_ptr()) }.unwrap(), "A\u{fffd}");

        let mut bounded = vec![b'x' as u16; 32768];
        // SAFETY: the allocation provides every code unit in the documented
        // scan range even though it intentionally contains no terminator.
        assert!(unsafe { read_wide(bounded.as_ptr()) }.is_err());
        bounded[32767] = 0;
        // SAFETY: bounded is terminated at the final readable scan position.
        assert_eq!(unsafe { read_wide(bounded.as_ptr()) }.unwrap().len(), 32767);
    }

    fn fixture_acl(path: &Path, extra_aces: &str) {
        use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
        let sid = current_sid().unwrap();
        let sddl = wide(&format!(
            "O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA){extra_aces}"
        ))
        .unwrap();
        let mut descriptor = ptr::null_mut();
        assert_ne!(
            // SAFETY: sddl is terminated and descriptor is writable output;
            // successful conversion is guarded by Descriptor immediately below.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            },
            0
        );
        let descriptor = Descriptor(descriptor);
        let (mut present, mut acl, mut defaulted) = (0, ptr::null_mut(), 0);
        assert_ne!(
            // SAFETY: descriptor owns the successfully converted security
            // descriptor; all outputs are live, aligned writable locals.
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            },
            0
        );
        let path = crate::atomic_file::windows_wide_path(path).unwrap();
        assert_eq!(
            // SAFETY: path is terminated and acl belongs to the still-live
            // descriptor. Only DACL information is requested; unused SID/SACL
            // pointers are null and the API does not retain the input ACL.
            unsafe {
                SetNamedSecurityInfoW(
                    path.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    acl,
                    ptr::null_mut(),
                )
            },
            0
        );
    }

    #[test]
    fn scm_codex_rejects_foreign_file_writes_and_replaceable_ancestors() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("target-account");
        let ancestor = root.join("AppData");
        let directory = ancestor.join("npm");
        crate::windows_private_directory::create_dir_all(&directory).unwrap();
        let executable = directory.join("codex.cmd");
        crate::windows_private_directory::open_private_file(&executable).unwrap();
        let sid = current_sid().unwrap();
        let validate = || {
            validate_executable_components(
                &executable,
                &sid,
                executable
                    .ancestors()
                    .skip(1)
                    .take_while(|path| path.starts_with(&root)),
            )
        };
        validate().unwrap();
        // Other principals may read a target-owned npm installation.
        fixture_acl(&executable, "(A;;GR;;;WD)");
        validate().unwrap();
        fixture_acl(&executable, "(A;;GW;;;WD)");
        assert!(
            validate()
                .unwrap_err()
                .to_string()
                .contains("machine_codex_untrusted")
        );
        fixture_acl(&executable, "");
        fixture_acl(&directory, "(A;;FA;;;WD)");
        assert!(
            validate()
                .unwrap_err()
                .to_string()
                .contains("machine_codex_ancestor_untrusted")
        );
        fixture_acl(&directory, "");
        // A protected immediate parent does not save a replaceable ancestor.
        fixture_acl(&ancestor, "(A;;0x40;;;WD)"); // FILE_DELETE_CHILD
        assert!(
            validate()
                .unwrap_err()
                .to_string()
                .contains("machine_codex_ancestor_untrusted")
        );
        fixture_acl(&ancestor, "(A;;0x4;;;WD)(A;OICIIO;FA;;;CO)"); // create siblings + inherit-only owner
        validate().unwrap();
        let validate_other_target = || {
            validate_executable_components(
                &executable,
                "S-1-5-21-1-2-3-9999",
                executable
                    .ancestors()
                    .skip(1)
                    .take_while(|path| path.starts_with(&root)),
            )
        };
        if sid == "S-1-5-18" {
            // SYSTEM owns this fixture when the UTM Guest Agent runs it.
            // Its protected code/ancestor grants remain trusted for another
            // service target; ordinary user-owned code is target-specific.
            validate_other_target().unwrap();
        } else {
            assert!(validate_other_target().is_err());
        }
        // A real foreign principal's write grant must still be rejected even
        // when SYSTEM owns the code and the target SID is changed.
        fixture_acl(&executable, "(A;;GW;;;BU)"); // BUILTIN\Users, S-1-5-32-545
        assert!(
            validate()
                .unwrap_err()
                .to_string()
                .contains("machine_codex_untrusted")
        );
        assert!(validate_other_target().is_err());
    }

    #[test]
    fn machine_output_requires_target_owner_and_private_existing_acl() {
        let temporary = tempfile::tempdir().unwrap();
        let private = temporary.path().join("private");
        crate::windows_private_directory::create_dir(&private).unwrap();
        let sid = current_sid().unwrap();
        validate_private_output(&private, &sid, true).unwrap();
        assert!(validate_private_output(&private, "S-1-5-21-1-2-3-9999", true).is_err());
        validate_private_output(&private.join("new-history"), &sid, true).unwrap();

        let public = temporary.path().join("public");
        let sddl = wide(&format!(
            "O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GR;;;WD)"
        ))
        .unwrap();
        let mut descriptor = ptr::null_mut();
        assert_ne!(
            // SAFETY: the fixture SDDL is terminated, descriptor is writable
            // output, and the successful allocation is guarded below.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            },
            0
        );
        let descriptor = Descriptor(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let path = crate::atomic_file::windows_wide_path(&public).unwrap();
        // SAFETY: path is terminated; attributes has its correct size and points
        // to a live descriptor for the duration of this directory creation call.
        assert_ne!(unsafe { CreateDirectoryW(path.as_ptr(), &attributes) }, 0);
        validate_runtime_path(&public, &sid).unwrap();
        assert!(validate_private_output(&public, &sid, true).is_err());
        // A public/read-only ancestor is permitted only when the output is
        // missing and will be created privately by the service's real token.
        validate_private_output(&public.join("new-history"), &sid, true).unwrap();
    }

    #[test]
    fn machine_code_never_trusts_the_installer_or_service_user_as_a_writer() {
        for sid in ["S-1-5-18", "S-1-5-32-544"] {
            assert!(trusted_writer(sid, None));
        }
        let service = "S-1-5-21-111-222-333-1001";
        let installer = "S-1-5-21-111-222-333-1002";
        assert!(!trusted_writer(service, None));
        assert!(!trusted_writer(installer, None));
        assert!(trusted_writer(service, Some(service)));
        assert!(!trusted_writer(installer, Some(service)));
        assert!(!trusted_writer("S-1-5-32-545", Some(service)));
    }
}
