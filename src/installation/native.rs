//! Registry access is scoped to HKCU. No environment/profile expansion occurs
//! when reading PATH, and no machine registry handle is ever opened.
use std::io;
use std::path::Path;
use std::ptr;

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::RegQueryValueExW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use winreg::enums::{KEY_QUERY_VALUE, KEY_SET_VALUE, REG_CREATED_NEW_KEY, REG_EXPAND_SZ, REG_SZ};
use winreg::{HKCU, RegKey, RegValue};

use super::{InstallReceipt, RegistryValue, UserPath};

const MAX_VALUE: usize = 256 * 1024;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(super) struct NativePath {
    // Production has no configurable target. Tests can exercise the real API
    // against a unique HKCU Software key without touching user Environment.
    #[cfg(test)]
    test_subkey: Option<String>,
}

impl NativePath {
    pub(super) fn open() -> Result<Self> {
        Ok(Self {
            #[cfg(test)]
            test_subkey: None,
        })
    }

    fn subkey(&self) -> &str {
        #[cfg(test)]
        if let Some(subkey) = &self.test_subkey {
            return subkey;
        }
        "Environment"
    }
}

fn open_user_key(subkey: &str, write: bool) -> Result<Option<RegKey>> {
    // A read/doctor never creates the key. Creation is reserved for an actual
    // accepted mutation after its second read of the current value.
    let result = if write {
        HKCU.create_subkey_with_flags(subkey, KEY_QUERY_VALUE | KEY_SET_VALUE)
            .map(|(key, _)| key)
    } else {
        HKCU.open_subkey_with_flags(subkey, KEY_QUERY_VALUE)
    };
    match result {
        Ok(key) => Ok(Some(key)),
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => Ok(None),
        Err(error) => anyhow::bail!("install_registry_failed: {error}"),
    }
}

fn read_value(key: &RegKey) -> Result<Option<RegistryValue>> {
    read_named_value(key, "Path")
}

fn read_named_value(key: &RegKey, name: &str) -> Result<Option<RegistryValue>> {
    // winreg::get_raw_value grows/retries without a bound and rejects unknown
    // value types. Keep this narrow raw query to cap allocation and contention,
    // and let the PATH validation retain its existing unsupported-type error.
    let name = wide(name);
    for _ in 0..8 {
        let mut size = 0;
        let mut kind = 0;
        // SAFETY: null data with a writable size is the documented size query.
        let status = unsafe {
            RegQueryValueExW(
                key.raw_handle(),
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut size,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        ensure!(
            status == ERROR_SUCCESS,
            "install_registry_read_failed: {}",
            io::Error::from_raw_os_error(status as i32)
        );
        ensure!(
            size as usize <= MAX_VALUE,
            "install_path_invalid: PATH exceeds registry read limit"
        );
        let mut bytes = vec![0; size as usize];
        // SAFETY: bytes has the capacity specified in size; Windows initializes
        // the returned length and type, which are checked before using them.
        let status = unsafe {
            RegQueryValueExW(
                key.raw_handle(),
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                bytes.as_mut_ptr(),
                &mut size,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status == ERROR_MORE_DATA {
            continue;
        }
        ensure!(
            status == ERROR_SUCCESS && size as usize <= bytes.len(),
            "install_registry_read_failed: {}",
            io::Error::from_raw_os_error(status as i32)
        );
        bytes.truncate(size as usize);
        return Ok(Some(RegistryValue { kind, bytes }));
    }
    anyhow::bail!("install_path_conflict: PATH changed repeatedly while reading")
}

const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\codex-usage-monit";

fn uninstall_key_at(subkey: &str, create: bool) -> Result<Option<(RegKey, bool)>> {
    let result = if create {
        HKCU.create_subkey_with_flags(subkey, KEY_QUERY_VALUE | KEY_SET_VALUE)
            .map(|(key, disposition)| (key, disposition == REG_CREATED_NEW_KEY))
    } else {
        HKCU.open_subkey_with_flags(subkey, KEY_QUERY_VALUE)
            .map(|key| (key, false))
    };
    match result {
        Ok(key) => Ok(Some(key)),
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => Ok(None),
        Err(error) => anyhow::bail!("install_uninstall_registry_failed: {error}"),
    }
}

fn check_uninstall_owner(key: &RegKey, root: &Path) -> Result<()> {
    let location = read_named_value(key, "InstallLocation")?
        .context("install_uninstall_key_conflict: existing uninstall key has no ownership")?;
    let actual = String::from_utf16(&location.units()?)?;
    ensure!(
        actual == root.to_string_lossy(),
        "install_uninstall_key_conflict: another installation owns the uninstall entry"
    );
    Ok(())
}

fn owned_uninstall_key_at(subkey: &str, root: &Path) -> Result<RegKey> {
    let (key, created) =
        uninstall_key_at(subkey, true)?.context("install_uninstall_registry_failed")?;
    // Check the actual handle returned by create/open. A separate existence
    // probe could race another install creating this shared product key.
    if !created {
        check_uninstall_owner(&key, root)?;
    }
    Ok(key)
}

pub(super) fn register_uninstall(receipt: &InstallReceipt) -> Result<()> {
    let executable = receipt
        .uninstall_executable
        .as_ref()
        .context("install_uninstall_executable_missing")?;
    ensure!(
        executable.is_file(),
        "install_uninstall_executable_missing: immutable uninstaller was removed"
    );
    let key = owned_uninstall_key_at(UNINSTALL_KEY, &receipt.root)?;
    for (name, text) in [
        (
            "InstallLocation",
            receipt.root.to_string_lossy().into_owned(),
        ),
        ("DisplayName", "Codex Usage Monitor (current user)".into()),
        ("DisplayVersion", receipt.display_version.clone()),
        ("Publisher", "codex-usage-monit contributors".into()),
        (
            "UninstallString",
            format!(
                "\"{}\" uninstall",
                super::path_for_shell(&executable.to_string_lossy())
            ),
        ),
        (
            "QuietUninstallString",
            format!(
                "\"{}\" uninstall --format json",
                super::path_for_shell(&executable.to_string_lossy())
            ),
        ),
    ] {
        key.set_value(name, &text)
            .map_err(|error| anyhow::anyhow!("install_uninstall_registry_failed: {error}"))?;
    }
    for name in ["NoModify", "NoRepair"] {
        key.set_value(name, &1_u32)
            .map_err(|error| anyhow::anyhow!("install_uninstall_registry_failed: {error}"))?;
    }
    Ok(())
}

pub(super) fn unregister_uninstall(receipt: &InstallReceipt) -> Result<()> {
    let Some((existing, _)) = uninstall_key_at(UNINSTALL_KEY, false)? else {
        return Ok(());
    };
    check_uninstall_owner(&existing, &receipt.root)?;
    drop(existing);
    // Deletes only the fixed product key whose InstallLocation was
    // checked above; never removes its parent or any machine-wide registration.
    match HKCU.delete_subkey_all(UNINSTALL_KEY) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => Ok(()),
        Err(error) => anyhow::bail!("install_uninstall_registry_failed: {error}"),
    }
}

impl UserPath for NativePath {
    fn read(&self) -> Result<Option<RegistryValue>> {
        match open_user_key(self.subkey(), false)? {
            Some(key) => read_value(&key),
            None => Ok(None),
        }
    }

    fn compare_write(
        &mut self,
        before: &Option<RegistryValue>,
        after: &Option<RegistryValue>,
    ) -> Result<bool> {
        if self.read()? != *before {
            return Ok(false);
        }
        let key = open_user_key(self.subkey(), true)?
            .context("install_registry_failed: Environment key unavailable")?;
        if read_value(&key)? != *before {
            return Ok(false);
        }
        let result = if let Some(after) = after {
            after.units()?;
            let _: u32 = after.bytes.len().try_into()?;
            key.set_raw_value(
                "Path",
                &RegValue {
                    vtype: if after.kind == super::REG_SZ {
                        REG_SZ
                    } else {
                        REG_EXPAND_SZ
                    },
                    bytes: after.bytes.as_slice().into(),
                },
            )
        } else {
            key.delete_value("Path")
        };
        if let Err(error) = result
            && !(after.is_none() && error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32))
        {
            anyhow::bail!("install_registry_write_failed: {error}");
        }
        Ok(true)
    }

    fn broadcast(&self) -> Result<()> {
        #[cfg(test)]
        if self.test_subkey.is_some() {
            return Ok(());
        }
        let environment = wide("Environment");
        // Notification is advisory. A hung desktop or headless SSH session must
        // not turn a durable PATH write into a misleading installation failure.
        // SAFETY: buffer remains live during the synchronous, bounded call.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                environment.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                1000,
                ptr::null_mut(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installation::{REG_EXPAND_SZ, REG_SZ, add_path, remove_owned_path};

    struct IsolatedKey(String);
    impl IsolatedKey {
        fn new() -> Self {
            let mut nonce = [0_u8; 16];
            getrandom::fill(&mut nonce).unwrap();
            let name: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
            Self(format!(r"Software\CodexUsageMonit-InstallationTest-{name}"))
        }
        fn backend(&self) -> NativePath {
            NativePath {
                test_subkey: Some(self.0.clone()),
            }
        }
    }
    impl Drop for IsolatedKey {
        fn drop(&mut self) {
            // The test only constructs this fixed prefix plus random hex. It
            // never deletes Environment, a real ARP entry, or a shared parent.
            assert!(
                self.0
                    .starts_with(r"Software\CodexUsageMonit-InstallationTest-")
            );
            let _ = HKCU.delete_subkey_all(&self.0);
        }
    }

    #[test]
    fn native_isolated_path_preserves_type_long_raw_expansion_and_other_edits() {
        let isolated = IsolatedKey::new();
        let mut backend = isolated.backend();
        let entry: Vec<u16> = r"C:\Users\Test User\AppData\Local\codex-usage-monit\bin"
            .encode_utf16()
            .collect();
        for kind in [REG_SZ, REG_EXPAND_SZ] {
            let raw = format!("%USERPROFILE%\\tools;{};;C:\\tail", "C:\\long;".repeat(600));
            let initial = Some(RegistryValue::from_units(
                kind,
                &raw.encode_utf16().collect::<Vec<_>>(),
            ));
            assert!(
                backend
                    .compare_write(&backend.read().unwrap(), &initial)
                    .unwrap()
            );
            assert_eq!(backend.read().unwrap(), initial);
            let change = add_path(initial.clone(), &entry).unwrap().unwrap();
            assert!(
                backend
                    .compare_write(&change.before, &change.after)
                    .unwrap()
            );
            assert_eq!(backend.read().unwrap(), change.after);
            let with_later_edit = {
                let mut units = backend.read().unwrap().unwrap().units().unwrap();
                units.extend(";%LATER_USER_CHANGE%\\bin".encode_utf16());
                Some(RegistryValue::from_units(kind, &units))
            };
            assert!(
                backend
                    .compare_write(&change.after, &with_later_edit)
                    .unwrap()
            );
            assert!(
                !backend.compare_write(&change.after, &initial).unwrap(),
                "stale compare/write must not overwrite user's edit"
            );
            let remove = remove_owned_path(backend.read().unwrap(), &entry)
                .unwrap()
                .unwrap();
            assert!(
                backend
                    .compare_write(&remove.before, &remove.after)
                    .unwrap()
            );
            let expected = format!("{raw};%LATER_USER_CHANGE%\\bin");
            assert_eq!(
                backend.read().unwrap(),
                Some(RegistryValue::from_units(
                    kind,
                    &expected.encode_utf16().collect::<Vec<_>>()
                ))
            );
        }
    }

    #[test]
    fn native_isolated_missing_value_and_unsupported_type_are_safe() {
        let isolated = IsolatedKey::new();
        let mut backend = isolated.backend();
        assert!(backend.read().unwrap().is_none());
        assert!(open_user_key(&isolated.0, false).unwrap().is_none());
        let key = open_user_key(&isolated.0, true).unwrap().unwrap();
        let unsupported = RegistryValue {
            kind: winreg::enums::REG_DWORD as u32,
            bytes: 7_u32.to_le_bytes().to_vec(),
        };
        key.set_value("Path", &7_u32).unwrap();
        assert!(
            backend
                .compare_write(&Some(unsupported.clone()), &Some(unsupported.clone()))
                .is_err()
        );
        assert_eq!(backend.read().unwrap(), Some(unsupported.clone()));
        assert!(backend.compare_write(&Some(unsupported), &None).unwrap());
        assert!(backend.read().unwrap().is_none());
    }

    #[test]
    fn native_isolated_path_keeps_raw_utf16_and_enforces_read_limit() {
        let isolated = IsolatedKey::new();
        let mut backend = isolated.backend();
        for kind in [REG_SZ, REG_EXPAND_SZ] {
            // RegSetValueExW requires a terminating NUL for string values. An
            // unpaired UTF-16 surrogate must survive without String conversion.
            let raw = Some(RegistryValue {
                kind,
                bytes: [b'C' as u16, b':' as u16, 0xd800, 0]
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            });
            assert!(
                backend
                    .compare_write(&backend.read().unwrap(), &raw)
                    .unwrap()
            );
            assert_eq!(backend.read().unwrap(), raw);
        }
        let key = open_user_key(&isolated.0, true).unwrap().unwrap();
        key.set_raw_value(
            "Path",
            &RegValue {
                vtype: winreg::enums::REG_SZ,
                bytes: vec![0; MAX_VALUE + 2].into(),
            },
        )
        .unwrap();
        assert!(
            backend
                .read()
                .unwrap_err()
                .to_string()
                .contains("registry read limit")
        );
    }

    #[test]
    fn native_isolated_uninstall_key_checks_ownership_after_create_or_open() {
        let isolated = IsolatedKey::new();
        let our_root = Path::new(r"C:\Users\fixture\codex-usage-monit");
        assert!(uninstall_key_at(&isolated.0, false).unwrap().is_none());
        // Simulate another installer winning the create after an absent probe.
        let (foreign, created) = uninstall_key_at(&isolated.0, true).unwrap().unwrap();
        assert!(created);
        let foreign_root = RegistryValue::from_units(
            REG_SZ,
            &r"C:\other-installer".encode_utf16().collect::<Vec<_>>(),
        );
        foreign
            .set_value("InstallLocation", &r"C:\other-installer")
            .unwrap();
        assert!(owned_uninstall_key_at(&isolated.0, our_root).is_err());
        assert_eq!(
            read_named_value(&foreign, "InstallLocation").unwrap(),
            Some(foreign_root)
        );
        // An existing owned entry remains repairable, even after partial setup.
        let our_location = RegistryValue::from_units(
            REG_SZ,
            &our_root
                .to_string_lossy()
                .encode_utf16()
                .collect::<Vec<_>>(),
        );
        foreign
            .set_value("InstallLocation", &our_root.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(
            read_named_value(&foreign, "InstallLocation").unwrap(),
            Some(our_location)
        );
        assert!(owned_uninstall_key_at(&isolated.0, our_root).is_ok());
    }
}
