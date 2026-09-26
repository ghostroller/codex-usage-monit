use super::*;
use std::cell::Cell;

fn value(text: &str, kind: u32) -> RegistryValue {
    RegistryValue::from_units(kind, &text.encode_utf16().collect::<Vec<_>>())
}

fn entry() -> Vec<u16> {
    r"C:\Users\Test User\AppData\Local\codex-usage-monit\bin"
        .encode_utf16()
        .collect()
}

#[test]
fn path_addition_preserves_long_raw_expandable_value_and_type() {
    for kind in [REG_SZ, REG_EXPAND_SZ] {
        let original = format!(
            "%USERPROFILE%\\my tools;{};;C:\\tail\\",
            "C:\\long;".repeat(600)
        );
        let change = add_path(Some(value(&original, kind)), &entry())
            .unwrap()
            .unwrap();
        let after = change.after.unwrap();
        assert_eq!(after.kind, kind);
        assert_eq!(
            String::from_utf16(&after.units().unwrap()).unwrap(),
            format!("{original};{}", String::from_utf16(&entry()).unwrap())
        );
    }
}

#[test]
fn preexisting_normalized_path_is_never_claimed() {
    let raw = r#"C:\other;"c:/users/test user/appdata/local/CODEX-USAGE-MONIT/bin/";C:\last"#;
    assert!(
        add_path(Some(value(raw, REG_EXPAND_SZ)), &entry())
            .unwrap()
            .is_none()
    );
}

#[test]
fn canonical_verbatim_paths_match_shell_drive_and_unc_spelling() {
    assert_eq!(
        normalized_entry(&r"\\?\C:\Users\Test\bin".encode_utf16().collect::<Vec<_>>()),
        normalized_entry(&r"c:\users\test\bin\".encode_utf16().collect::<Vec<_>>())
    );
    assert_eq!(
        path_for_shell(r"\\?\UNC\server\share\bin"),
        r"\\server\share\bin"
    );
}

#[test]
fn uninstall_keeps_later_edits_and_only_removes_exact_owned_segment() {
    let text = format!(
        "C:\\before;{};;%AFTER%\\bin;C:\\bin-extra",
        String::from_utf16(&entry()).unwrap()
    );
    let change = remove_owned_path(Some(value(&text, REG_SZ)), &entry())
        .unwrap()
        .unwrap();
    assert_eq!(
        change.after,
        Some(value(r"C:\before;;%AFTER%\bin;C:\bin-extra", REG_SZ))
    );
    let changed_spelling = String::from_utf16(&entry()).unwrap().to_lowercase();
    assert!(
        remove_owned_path(Some(value(&changed_spelling, REG_SZ)), &entry())
            .unwrap()
            .is_none()
    );
}

#[test]
fn uninstall_refuses_ambiguous_duplicate_and_does_not_delete_existing_entries() {
    let text = String::from_utf16(&entry()).unwrap();
    assert!(
        remove_owned_path(Some(value(&format!("{text};{text}"), REG_SZ)), &entry())
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    assert!(
        remove_owned_path(Some(value("C:\\user-owned", REG_SZ)), &entry())
            .unwrap()
            .is_none()
    );
}

#[test]
fn raw_non_unicode_segments_survive_and_bad_types_fail_closed() {
    let original = [0xd800, b';' as u16, b'X' as u16];
    let change = add_path(
        Some(RegistryValue::from_units(REG_EXPAND_SZ, &original)),
        &entry(),
    )
    .unwrap()
    .unwrap();
    assert!(
        change
            .after
            .unwrap()
            .units()
            .unwrap()
            .starts_with(&original)
    );
    assert!(
        add_path(
            Some(RegistryValue {
                kind: 4,
                bytes: vec![1, 0, 0, 0]
            }),
            &entry()
        )
        .is_err()
    );
    assert!(
        add_path(
            Some(RegistryValue {
                kind: REG_SZ,
                bytes: vec![1]
            }),
            &entry()
        )
        .is_err()
    );
}

struct MemoryPath {
    value: Option<RegistryValue>,
    conflict: bool,
    broadcasts: Cell<usize>,
}
impl UserPath for MemoryPath {
    fn read(&self) -> Result<Option<RegistryValue>> {
        Ok(self.value.clone())
    }
    fn compare_write(
        &mut self,
        before: &Option<RegistryValue>,
        after: &Option<RegistryValue>,
    ) -> Result<bool> {
        if self.conflict {
            self.conflict = false;
            self.value = Some(value("%EXTERNAL%\\tools", REG_EXPAND_SZ));
            return Ok(false);
        }
        if self.value != *before {
            return Ok(false);
        }
        self.value = after.clone();
        Ok(true)
    }
    fn broadcast(&self) -> Result<()> {
        self.broadcasts.set(self.broadcasts.get() + 1);
        Ok(())
    }
}

fn fixture() -> (tempfile::TempDir, InstallReceipt) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("installation");
    STORE.create_directory_beneath(&root, &root).unwrap();
    let receipt = InstallReceipt {
        schema_version: 1,
        installer_protocol: 1,
        owner: OWNER.into(),
        channel: "test".into(),
        user_sid: "S-1-5-21-test".into(),
        executable: root.join("bin/codex-usage-monit.exe"),
        root,
        state: "installed".into(),
        path_requested: true,
        path_entry: None,
        path_empty_value_before: None,
        task_identity: None,
        pending_recorder_executable: None,
        uninstall_executable: None,
        display_version: String::new(),
        pending_path: None,
    };
    (temp, receipt)
}

#[test]
fn interrupted_path_transaction_recovers_without_overwriting_new_edits() {
    let (_temp, mut receipt) = fixture();
    let entry = directory_entry(&receipt.root).unwrap();
    let change = add_path(Some(value("C:\\original", REG_SZ)), &entry)
        .unwrap()
        .unwrap();
    receipt.pending_path = Some(change.clone());
    write_receipt(&receipt).unwrap();
    let mut backend = MemoryPath {
        value: change.after.clone(),
        conflict: false,
        broadcasts: Cell::new(0),
    };
    recover_path(&mut receipt, &mut backend).unwrap();
    assert_eq!(receipt.path_entry, Some(entry));
    assert!(receipt.pending_path.is_none());
    receipt.pending_path = Some(change);
    backend.value = Some(value("C:\\edited-in-the-meantime", REG_SZ));
    let original = backend.value.clone();
    assert!(recover_path(&mut receipt, &mut backend).is_err());
    assert_eq!(backend.value, original);
}

#[test]
fn concurrent_edit_is_reread_and_repeated_install_keeps_ownership() {
    let (_temp, mut receipt) = fixture();
    let mut backend = MemoryPath {
        value: Some(value("C:\\old", REG_SZ)),
        conflict: true,
        broadcasts: Cell::new(0),
    };
    change_path(&mut receipt, &mut backend, true).unwrap();
    assert!(
        String::from_utf16(&backend.value.as_ref().unwrap().units().unwrap())
            .unwrap()
            .starts_with("%EXTERNAL%\\tools;")
    );
    let owned = receipt.path_entry.clone();
    change_path(&mut receipt, &mut backend, true).unwrap();
    assert_eq!(receipt.path_entry, owned);
    assert_eq!(backend.broadcasts.get(), 1);
    change_path(&mut receipt, &mut backend, false).unwrap();
    assert_eq!(
        backend.value,
        Some(value("%EXTERNAL%\\tools", REG_EXPAND_SZ))
    );
}

#[test]
fn receipt_is_separate_and_foreign_identity_or_owner_is_rejected() {
    let (_temp, receipt) = fixture();
    write_receipt(&receipt).unwrap();
    assert!(!receipt.root.join("installation.json").exists());
    assert!(read_receipt(&receipt.root, "another SID").is_err());
    let mut other = receipt.clone();
    other.owner = "scoop".into();
    assert!(
        ensure_owner(&other)
            .unwrap_err()
            .to_string()
            .contains("scoop")
    );
}

#[test]
fn uninstall_passes_exact_task_identity_and_keeps_it_when_unregister_fails() {
    let (_temp, mut receipt) = fixture();
    receipt.task_identity = Some("\\codex-usage-monit-fixture-user".into());
    write_receipt(&receipt).unwrap();
    let error = unregister_owned_task(&mut receipt, |expected| {
        assert_eq!(expected, "\\codex-usage-monit-fixture-user");
        bail!("task changed outside the installer")
    })
    .unwrap_err();
    assert!(error.to_string().contains("task changed"));
    assert!(receipt.task_identity.is_some());
    unregister_owned_task(&mut receipt, |_| Ok(())).unwrap();
    assert!(receipt.task_identity.is_none());
    unregister_owned_task(&mut receipt, |_| {
        panic!("no owned task must not unregister anything")
    })
    .unwrap();
}

#[test]
fn recorder_recovery_claims_only_proven_identity_and_blocks_changed_definition() {
    let (_temp, mut receipt) = fixture();
    let expected = receipt.root.join("versions/build/codex-usage-monit.exe");
    receipt.pending_recorder_executable = Some(expected.clone());
    write_receipt(&receipt).unwrap();
    assert!(recover_recorder_using(&mut receipt, |_| bail!("untrusted task fingerprint")).is_err());
    assert_eq!(
        receipt.pending_recorder_executable.as_ref(),
        Some(&expected)
    );
    assert!(receipt.task_identity.is_none());
    recover_recorder_using(&mut receipt, |actual| {
        assert_eq!(actual, expected);
        Ok(Some("trusted-task-id".into()))
    })
    .unwrap();
    assert_eq!(receipt.task_identity.as_deref(), Some("trusted-task-id"));
    assert!(receipt.pending_recorder_executable.is_none());
    receipt.pending_recorder_executable = Some(expected);
    recover_recorder_using(&mut receipt, |_| Ok(None)).unwrap();
    assert!(receipt.task_identity.is_none());
    assert!(receipt.pending_recorder_executable.is_none());
}

#[test]
fn recorder_recovery_write_failure_keeps_pending_in_returned_receipt() {
    let (_temp, mut receipt) = fixture();
    let expected = receipt.root.join("versions/build/codex-usage-monit.exe");
    receipt.pending_recorder_executable = Some(expected.clone());
    // A directory in the receipt's slot makes the atomic write fail without
    // touching registry state or relying on a platform-specific ACL failure.
    std::fs::create_dir(receipt.root.join(RECEIPT)).unwrap();
    assert!(recover_recorder_using(&mut receipt, |_| Ok(Some("trusted-task-id".into()))).is_err());
    assert_eq!(receipt.pending_recorder_executable, Some(expected));
    assert!(receipt.task_identity.is_none());
}

#[test]
fn uninstall_restores_only_an_original_empty_value_and_its_type() {
    let (_temp, mut receipt) = fixture();
    for initial in [
        None,
        Some(value("", REG_SZ)),
        Some(value("", REG_EXPAND_SZ)),
    ] {
        let mut backend = MemoryPath {
            value: initial.clone(),
            conflict: false,
            broadcasts: Cell::new(0),
        };
        change_path(&mut receipt, &mut backend, true).unwrap();
        change_path(&mut receipt, &mut backend, false).unwrap();
        assert_eq!(backend.value, initial);
    }
}
