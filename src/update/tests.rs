use super::*;
use std::cell::Cell;

fn fixture() -> (tempfile::TempDir, PathBuf, InstalledVersion) {
    let temp = tempfile::tempdir_in(env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = temp.path().join("application");
    let target = install_bytes(&root, &AgentInfo::local(), b"trusted candidate fixture").unwrap();
    (temp, root, target)
}

fn options(scope: UpdateScope, adopt: bool) -> ApplyOptions {
    ApplyOptions {
        scope,
        install_dir: None,
        adopt,
        allow_dev_build: false,
    }
}

fn absent(target: &InstalledVersion) -> ServiceUpgradeReport {
    ServiceUpgradeReport {
        outcome: "not_installed".into(),
        build_id: target.build_id.clone(),
        enabled: false,
        pid: None,
        last_history_heartbeat: None,
        diagnostic: None,
    }
}

fn write_executable(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    executable_permissions(path).unwrap();
}

#[test]
fn immutable_install_rejects_tampering_and_retains_other_versions() {
    let (_temp, root, first) = fixture();
    let second = install_bytes(&root, &AgentInfo::local(), b"other candidate").unwrap();
    assert_ne!(first.executable, second.executable);
    assert_eq!(
        BINARIES.read_bounded(&first.executable).unwrap(),
        b"trusted candidate fixture"
    );
    fs::write(&first.executable, b"changed bytes").unwrap();
    let error =
        install_bytes(&root, &AgentInfo::local(), b"trusted candidate fixture").unwrap_err();
    assert!(error.to_string().contains("update_install_conflict"));
    assert_eq!(fs::read(first.executable).unwrap(), b"changed bytes");
    validate_version(&root, &second).unwrap();
}

#[test]
fn standalone_adoption_requires_explicit_choice_before_recorder_mutation() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    write_executable(&entry, b"existing user executable");
    let invoked = Cell::new(false);
    let error = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || {
            invoked.set(true);
            Ok(absent(&target))
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_entry_unmanaged"));
    assert!(!invoked.get());
    assert_eq!(fs::read(&entry).unwrap(), b"existing user executable");
    assert!(!root.join(JOURNAL).exists());
}

#[test]
fn node_update_adopts_standalone_cli_retaining_backup_without_installing_service() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    write_executable(&entry, b"existing user executable");
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, true),
        &directory,
        Some(directory.as_os_str()),
        || Ok(absent(&target)),
    )
    .unwrap();
    assert_eq!(report.outcome, "complete");
    assert_eq!(report.recorder.outcome, "not_installed");
    assert_eq!(report.cli.outcome, "updated");
    assert!(!report.cli.shadowed);
    assert_eq!(
        fs::read(
            root.join("adopted-cli")
                .join(digest(b"existing user executable"))
        )
        .unwrap(),
        b"existing user executable"
    );
    assert_eq!(
        proxy_target(&root, &entry).unwrap(),
        Some(target.executable.clone())
    );
    assert_eq!(proxy_target(&root, &target.executable).unwrap(), None);
}

#[test]
fn sync_update_preserves_cli_and_never_registers_it() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    write_executable(&entry, b"unmanaged CLI remains unchanged");
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Sync, false),
        &directory,
        None,
        || Ok(absent(&target)),
    )
    .unwrap();
    assert_eq!(report.outcome, "complete");
    assert_eq!(report.cli.outcome, "not_requested");
    assert_eq!(
        fs::read(&entry).unwrap(),
        b"unmanaged CLI remains unchanged"
    );
    assert!(!root.join(REGISTRATION).exists());
}

#[test]
fn later_update_changes_pointer_without_replacing_open_launcher() {
    let (temp, root, first) = fixture();
    let directory = temp.path().join("bin");
    apply_at(
        &root,
        first.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&first)),
    )
    .unwrap();
    let entry = directory.join(BINARY_NAME);
    let initial = fs::read(&entry).unwrap();
    let mut reader = File::open(&entry).unwrap();
    let second = install_bytes(&root, &AgentInfo::local(), b"later candidate fixture").unwrap();
    let report = apply_at(
        &root,
        second.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&second)),
    )
    .unwrap();
    assert_eq!(report.outcome, "complete");
    assert_eq!(fs::read(&entry).unwrap(), initial);
    let mut held = Vec::new();
    reader.read_to_end(&mut held).unwrap();
    assert_eq!(held, initial);
    assert_eq!(
        proxy_target(&root, &entry).unwrap(),
        Some(second.executable)
    );
    assert!(first.executable.exists());
}

#[test]
fn recorder_failure_is_durable_and_retry_completes_forward() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let first = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || bail!("fixture recorder failure"),
    )
    .unwrap();
    assert_eq!(first.outcome, "failed");
    assert_eq!(first.recorder.outcome, "failed");
    assert!(!directory.join(BINARY_NAME).exists());
    let journal: UpdateJournal = read_json(&root.join(JOURNAL)).unwrap().unwrap();
    assert_eq!(journal.phase, "failed");
    assert!(
        journal
            .report
            .diagnostic
            .unwrap()
            .contains("fixture recorder failure")
    );
    let second = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&target)),
    )
    .unwrap();
    assert_eq!(second.outcome, "complete");
    assert_eq!(
        proxy_target(&root, &directory.join(BINARY_NAME)).unwrap(),
        Some(target.executable)
    );
}

#[test]
fn pending_update_does_not_silently_change_scope_or_candidate() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || bail!("fixture recorder failure"),
    )
    .unwrap();
    let error = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Sync, false),
        &directory,
        None,
        || panic!("must not mutate recorder"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_pending"));
    let other = install_bytes(&root, &AgentInfo::local(), b"different candidate").unwrap();
    let error = apply_at(
        &root,
        other,
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || panic!("must not mutate recorder"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_pending"));
}

#[test]
fn cli_conflict_after_service_upgrade_reports_partial_and_preserves_changed_file() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || {
            write_executable(&entry, b"concurrent user installation");
            Ok(absent(&target))
        },
    )
    .unwrap();
    assert_eq!(report.outcome, "partial");
    assert_eq!(report.recorder.outcome, "not_installed");
    assert_eq!(report.cli.outcome, "failed");
    assert_eq!(fs::read(&entry).unwrap(), b"concurrent user installation");
    let retry = apply_at(
        &root,
        target,
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || panic!("preflight must reject the conflict"),
    );
    assert!(
        retry
            .unwrap_err()
            .to_string()
            .contains("update_entry_conflict")
    );
}

#[test]
fn crash_after_launcher_publication_can_finish_registration() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let plan = plan_cli(
        &root,
        &options(UpdateScope::Node, false),
        &directory,
        None,
        &target,
    )
    .unwrap();
    write_executable(
        &plan.executable,
        &BINARIES.read_bounded(&target.executable).unwrap(),
    );
    assert!(!root.join(REGISTRATION).exists());
    activate_cli(&root, &plan, &target).unwrap();
    assert_eq!(
        proxy_target(&root, &plan.executable).unwrap(),
        Some(target.executable)
    );
}

#[test]
fn ownership_rejects_package_manager_even_with_adoption() {
    let (temp, root, target) = fixture();
    for path in [
        ".cargo/bin",
        "homebrew/bin",
        "scoop/apps/bin",
        "Cellar/package/bin",
    ] {
        let directory = temp.path().join(path);
        let error = plan_cli(
            &root,
            &options(UpdateScope::Node, true),
            &directory,
            None,
            &target,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("update_external_install"),
            "{error}"
        );
    }
}

#[cfg(unix)]
#[test]
fn adoption_never_replaces_a_symlink_or_its_target() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    fs::create_dir(&directory).unwrap();
    let other = temp.path().join("other-cli");
    write_executable(&other, b"other owner");
    std::os::unix::fs::symlink(&other, directory.join(BINARY_NAME)).unwrap();
    let error = plan_cli(
        &root,
        &options(UpdateScope::Node, true),
        &directory,
        None,
        &target,
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_entry_unmanaged"));
    assert_eq!(fs::read(&other).unwrap(), b"other owner");
}

#[test]
fn path_shadowing_is_reported_without_changing_shell_profiles() {
    let (temp, root, target) = fixture();
    let shadow_directory = temp.path().join("other-bin");
    let shadow = shadow_directory.join(BINARY_NAME);
    write_executable(&shadow, b"existing PATH application");
    let directory = temp.path().join("bin");
    let path = env::join_paths([&shadow_directory, &directory]).unwrap();
    let mut explicit = options(UpdateScope::Node, false);
    explicit.install_dir = Some(directory.clone());
    let report = apply_at(
        &root,
        target.clone(),
        &explicit,
        &directory,
        Some(&path),
        || Ok(absent(&target)),
    )
    .unwrap();
    assert!(report.cli.shadowed);
    assert_eq!(report.cli.resolved_executable, Some(shadow.clone()));
    assert_eq!(fs::read(shadow).unwrap(), b"existing PATH application");
}

#[test]
fn update_holds_mutation_lock_during_recorder_cutover() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Sync, false),
        &directory,
        None,
        || {
            let contender = OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.join(STORE.lock_file_name))
                .unwrap();
            assert!(matches!(
                contender.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            Ok(absent(&target))
        },
    )
    .unwrap();
    assert_eq!(report.outcome, "complete");
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(STORE.lock_file_name))
        .unwrap();
    contender.try_lock().unwrap();
    drop(crate::file_lock::FileLock::from_locked(contender));
}

#[test]
fn launcher_rejects_a_tampered_target_instead_of_running_it() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&target)),
    )
    .unwrap();
    fs::write(&target.executable, b"changed target").unwrap();
    let error = proxy_target(&root, &directory.join(BINARY_NAME)).unwrap_err();
    assert!(error.to_string().contains("update_checksum_mismatch"));
}

#[test]
fn semantic_downgrade_is_never_enabled_by_development_override() {
    for (older, newer) in [
        ("0.5.0", "0.6.0"),
        ("1.0.0-rc.2", "1.0.0-rc.10"),
        ("1.0.0-rc.1", "1.0.0"),
        ("1.2.9", "1.2.10"),
    ] {
        let error = ensure_not_downgrade(older, "a", newer, Some("b"), true).unwrap_err();
        assert!(error.to_string().contains("update_downgrade_blocked"));
        ensure_not_downgrade(newer, "b", older, Some("a"), false).unwrap();
    }
    assert!(
        ensure_not_downgrade("0.6.0", "a", "0.6.0", Some("b"), false)
            .unwrap_err()
            .to_string()
            .contains("update_build_conflict")
    );
    ensure_not_downgrade("0.6.0", "a", "0.6.0", Some("b"), true).unwrap();
    ensure_not_downgrade("0.6.0+build", "a", "0.6.0", Some("a"), false).unwrap();
}

#[test]
fn semantic_versions_reject_invalid_complete_input_on_either_side() {
    for invalid in [
        "",
        "1.0",
        "v1.0.0",
        "01.0.0",
        "1.00.0",
        "1.0.00",
        "1.0.0-01",
        "1.0.0-rc.01",
        "1.0.0-",
        "1.0.0+",
        "1.0.0+foo..bar",
        "1.0.0+bad_name",
        "1.0.0+meta+extra",
        "1.0.0+build\n",
        "18446744073709551616.0.0",
    ] {
        for (target, installed) in [(invalid, "1.0.0"), ("1.0.0", invalid)] {
            let error =
                ensure_not_downgrade(target, "same", installed, Some("same"), true).unwrap_err();
            assert!(
                error.to_string().contains("update_version_invalid"),
                "{invalid:?}: {error}"
            );
        }
    }
}

#[test]
fn semantic_versions_preserve_precedence_without_narrow_prerelease_numbers() {
    let ordered = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "1.0.1",
        "1.1.0",
        "2.0.0",
        "18446744073709551615.0.0",
    ];
    for pair in ordered.windows(2) {
        assert_eq!(
            compare_versions(pair[0], pair[1]).unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions(pair[1], pair[0]).unwrap(),
            std::cmp::Ordering::Greater
        );
    }
    let smaller = format!("1.0.0-{}", "9".repeat(100));
    let larger = format!("1.0.0-1{}", "0".repeat(100));
    assert_eq!(
        compare_versions(&smaller, &larger).unwrap(),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        compare_versions(&larger, "1.0.0-alpha").unwrap(),
        std::cmp::Ordering::Less
    );
}

#[test]
fn semantic_metadata_does_not_bypass_source_build_identity() {
    let target = "1.0.0+001.new-build";
    let installed = "1.0.0+old-build";
    assert_eq!(
        compare_versions(target, installed).unwrap(),
        std::cmp::Ordering::Equal
    );
    ensure_not_downgrade(target, "same", installed, Some("same"), false).unwrap();
    let error = ensure_not_downgrade(target, "new", installed, Some("old"), false).unwrap_err();
    assert!(error.to_string().contains("update_build_conflict"));
    ensure_not_downgrade(target, "new", installed, Some("old"), true).unwrap();
}

#[test]
fn sync_cannot_downgrade_shared_state_below_managed_cli_version() {
    let (temp, root, mut target) = fixture();
    let directory = temp.path().join("bin");
    let mut new_info = AgentInfo::local();
    new_info.version = "99.0.0".into();
    let newer = install_bytes(&root, &new_info, b"newer version").unwrap();
    apply_at(
        &root,
        newer.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&newer)),
    )
    .unwrap();
    target.version = AgentInfo::local().version;
    let error = apply_at(
        &root,
        target,
        &options(UpdateScope::Sync, false),
        &directory,
        None,
        || panic!("must not downgrade recorder"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_downgrade_blocked"));
}

fn version_id(version: &InstalledVersion) -> String {
    version
        .executable
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .into()
}

#[test]
fn prune_requires_explicit_ids_and_external_reference_acknowledgement() {
    for options in [
        PruneOptions {
            versions: vec![],
            apply: true,
            acknowledge_unreferenced: true,
        },
        PruneOptions {
            versions: vec!["some-version".into()],
            apply: true,
            acknowledge_unreferenced: false,
        },
    ] {
        assert!(validate_prune_options(&options).is_err());
    }
    for id in ["../other", "/absolute", "..", "a\\b"] {
        assert!(
            validate_prune_options(&PruneOptions {
                versions: vec![id.into()],
                apply: false,
                acknowledge_unreferenced: false
            })
            .is_err()
        );
    }
}

#[test]
fn prune_keeps_current_service_cli_and_pending_references() {
    let (temp, root, current) = fixture();
    let cli = install_bytes(&root, &AgentInfo::local(), b"selected cli").unwrap();
    let service = install_bytes(&root, &AgentInfo::local(), b"registered recorder").unwrap();
    let pending = install_bytes(&root, &AgentInfo::local(), b"pending update").unwrap();
    let unused = install_bytes(&root, &AgentInfo::local(), b"unused old version").unwrap();
    let directory = temp.path().join("bin");
    apply_at(
        &root,
        cli.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&cli)),
    )
    .unwrap();
    apply_at(
        &root,
        pending.clone(),
        &options(UpdateScope::Sync, false),
        &directory,
        None,
        || bail!("pending fixture"),
    )
    .unwrap();
    let all = [&current, &cli, &service, &pending, &unused];
    let report = prune_at(
        &root,
        &PruneOptions {
            versions: all.iter().map(|v| version_id(v)).collect(),
            apply: true,
            acknowledge_unreferenced: true,
        },
        &current.executable,
        std::slice::from_ref(&service.executable),
    )
    .unwrap();
    assert_eq!(
        report
            .versions
            .iter()
            .filter(|v| v.action == "kept")
            .count(),
        4
    );
    assert_eq!(
        report
            .versions
            .iter()
            .filter(|v| v.action == "removed")
            .count(),
        1
    );
    for item in [&current, &cli, &service, &pending] {
        assert!(item.executable.exists());
    }
    assert!(!unused.executable.exists());
}

#[test]
fn prune_is_dry_by_default_and_keeps_unknown_contents() {
    let (_temp, root, current) = fixture();
    let old = install_bytes(&root, &AgentInfo::local(), b"unused old version").unwrap();
    let unknown = install_bytes(&root, &AgentInfo::local(), b"unknown extra contents").unwrap();
    fs::write(
        unknown.executable.parent().unwrap().join("user-file"),
        b"retain me",
    )
    .unwrap();
    let report = prune_at(
        &root,
        &PruneOptions {
            versions: vec![],
            apply: false,
            acknowledge_unreferenced: false,
        },
        &current.executable,
        &[],
    )
    .unwrap();
    assert!(
        report
            .versions
            .iter()
            .any(|v| v.id == version_id(&old) && v.action == "candidate")
    );
    assert!(
        report
            .versions
            .iter()
            .any(|v| v.id == version_id(&unknown) && v.action == "unknown")
    );
    assert!(old.executable.exists());
    assert!(unknown.executable.exists());
}

#[test]
fn reexecuted_candidate_reports_require_identity_scope_and_exit_agreement() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&target)),
    )
    .unwrap();
    let info = AgentInfo::local();
    validate_apply_report(
        &report,
        &info,
        &target.executable,
        UpdateScope::Node,
        Some(0),
    )
    .unwrap();
    for mutation in 0..8 {
        let mut changed = report.clone();
        let mut exit = Some(0);
        match mutation {
            0 => changed.schema_version = 2,
            1 => changed.build_id = "a".repeat(64),
            2 => changed.version = "0.0.0".into(),
            3 => changed.executable = directory.join("unexpected"),
            4 => changed.scope = UpdateScope::Sync,
            5 => changed.outcome = "partial".into(),
            6 => exit = Some(2),
            7 => changed.cli.executable = None,
            _ => unreachable!(),
        }
        assert!(
            validate_apply_report(&changed, &info, &target.executable, UpdateScope::Node, exit)
                .is_err()
        );
    }
}

#[cfg(unix)]
#[test]
fn parent_directory_alias_cannot_hide_package_manager_ownership() {
    let (temp, root, target) = fixture();
    let manager = temp.path().join("homebrew");
    fs::create_dir_all(manager.join("bin")).unwrap();
    let alias = temp.path().join("friendly-name");
    std::os::unix::fs::symlink(&manager, &alias).unwrap();
    let error = plan_cli(
        &root,
        &options(UpdateScope::Node, true),
        &alias.join("bin"),
        None,
        &target,
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_external_install"));
}

#[cfg(unix)]
#[test]
fn aliased_install_parent_registers_physical_launcher_path() {
    let (temp, root, target) = fixture();
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let directory = alias.join("bin");
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        Some(directory.as_os_str()),
        || Ok(absent(&target)),
    )
    .unwrap();
    let physical = real.join("bin").join(BINARY_NAME);
    assert_eq!(report.cli.executable, Some(physical.clone()));
    assert!(!report.cli.shadowed);
    assert_eq!(
        proxy_target(&root, &physical).unwrap(),
        Some(target.executable)
    );
}

#[cfg(windows)]
#[test]
fn windows_proxy_leaves_console_close_to_default_handler() {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT};
    assert_eq!(proxy_console_control(CTRL_C_EVENT), 1);
    assert_eq!(proxy_console_control(CTRL_BREAK_EVENT), 1);
    assert_eq!(proxy_console_control(CTRL_CLOSE_EVENT), 0);
}

#[test]
fn discovered_path_cli_requires_adoption_before_recorder_changes() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("custom-bin");
    let entry = directory.join(BINARY_NAME);
    let default = temp.path().join("default-bin");
    write_executable(&entry, b"existing PATH executable");
    let error = apply_at(
        &root,
        target,
        &options(UpdateScope::Node, false),
        &default,
        Some(directory.as_os_str()),
        || panic!("must reject unmanaged PATH CLI before service mutation"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_entry_unmanaged"));
    assert!(!root.join(JOURNAL).exists());
    assert!(!default.exists());
    assert_eq!(fs::read(entry).unwrap(), b"existing PATH executable");
}

#[test]
fn explicit_adoption_updates_the_discovered_path_entry() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("custom-bin");
    let entry = directory.join(BINARY_NAME);
    let default = temp.path().join("default-bin");
    write_executable(&entry, b"existing PATH executable");
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, true),
        &default,
        Some(directory.as_os_str()),
        || Ok(absent(&target)),
    )
    .unwrap();
    assert_eq!(report.outcome, "complete");
    assert_eq!(report.cli.executable, Some(entry.clone()));
    assert!(!report.cli.shadowed);
    assert!(!default.exists());
    assert_eq!(
        proxy_target(&root, &entry).unwrap(),
        Some(target.executable)
    );
}

#[test]
fn registered_cli_takes_precedence_over_a_different_path_entry() {
    let (temp, root, first) = fixture();
    let directory = temp.path().join("managed-bin");
    apply_at(
        &root,
        first.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&first)),
    )
    .unwrap();
    let other_directory = temp.path().join("other-bin");
    let other = other_directory.join(BINARY_NAME);
    write_executable(&other, b"other PATH executable");
    let second = install_bytes(&root, &AgentInfo::local(), b"next candidate").unwrap();
    let report = apply_at(
        &root,
        second.clone(),
        &options(UpdateScope::Node, false),
        &temp.path().join("default-bin"),
        Some(other_directory.as_os_str()),
        || Ok(absent(&second)),
    )
    .unwrap();
    assert_eq!(report.cli.executable, Some(directory.join(BINARY_NAME)));
    assert!(report.cli.shadowed);
    assert_eq!(fs::read(other).unwrap(), b"other PATH executable");
}

#[test]
fn discovered_package_manager_cli_cannot_be_adopted() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join(".cargo/bin");
    let entry = directory.join(BINARY_NAME);
    write_executable(&entry, b"package manager executable");
    let error = apply_at(
        &root,
        target,
        &options(UpdateScope::Node, true),
        &temp.path().join("default-bin"),
        Some(directory.as_os_str()),
        || panic!("must reject package manager CLI before service mutation"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_external_install"));
    assert_eq!(fs::read(entry).unwrap(), b"package manager executable");
}

#[cfg(unix)]
#[test]
fn discovered_path_symlink_is_not_resolved_into_an_adoptable_target() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("path-bin");
    fs::create_dir(&directory).unwrap();
    let real = temp.path().join("other-bin").join(BINARY_NAME);
    write_executable(&real, b"separately owned target");
    let entry = directory.join(BINARY_NAME);
    std::os::unix::fs::symlink(&real, &entry).unwrap();
    assert_eq!(
        resolve_path_entry(Some(directory.as_os_str())),
        Some(entry.clone())
    );
    let error = apply_at(
        &root,
        target,
        &options(UpdateScope::Node, true),
        &temp.path().join("default-bin"),
        Some(directory.as_os_str()),
        || panic!("must reject symlink CLI before service mutation"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_entry_unmanaged"));
    assert!(
        fs::symlink_metadata(entry)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(real).unwrap(), b"separately owned target");
}

#[test]
fn immutable_agent_directories_cannot_become_cli_entries() {
    let (temp, root, target) = fixture();
    let legacy = temp
        .path()
        .join(".codex-usage-monit-agents/build/target/digest");
    write_executable(&legacy.join(BINARY_NAME), b"legacy immutable agent");
    for directory in [target.executable.parent().unwrap(), legacy.as_path()] {
        let entry = directory.join(BINARY_NAME);
        let original = fs::read(&entry).unwrap();
        for explicit in [false, true] {
            let mut selected = options(UpdateScope::Node, true);
            selected.install_dir = explicit.then(|| directory.to_path_buf());
            let error = apply_at(
                &root,
                target.clone(),
                &selected,
                &temp.path().join("default-bin"),
                Some(directory.as_os_str()),
                || panic!("immutable CLI target must fail before service mutation"),
            )
            .unwrap_err();
            assert!(error.to_string().contains("update_entry_conflict"));
            assert!(error.to_string().contains("immutable"));
            assert_eq!(fs::read(&entry).unwrap(), original);
            assert!(!root.join(JOURNAL).exists());
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_pathext_discovers_wrappers_without_adopting_them() {
    let (temp, root, target) = fixture();
    let wrappers = temp.path().join("wrappers");
    let managed = temp.path().join("managed");
    write_executable(
        &wrappers.join("codex-usage-monit.cmd"),
        b"@echo old-wrapper",
    );
    write_executable(&managed.join(BINARY_NAME), b"existing executable");
    let path = env::join_paths([&wrappers, &managed]).unwrap();
    assert_eq!(
        resolve_path_entry_with_extensions(Some(&path), Some(OsStr::new(".EXE;.CMD"))),
        Some(wrappers.join("codex-usage-monit.CMD")),
    );
    let error = plan_cli(
        &root,
        &options(UpdateScope::Node, true),
        &managed,
        Some(&path),
        &target,
    )
    .unwrap_err();
    assert!(error.to_string().contains("update_entry_wrapper"));
    assert!(!root.join(REGISTRATION).exists());

    // Explicit selection permits a separate owned entry, but must retain the
    // earlier wrapper as a visible command conflict.
    let report = cli_report(Some(managed.join(BINARY_NAME)), Some(&path), "updated");
    assert!(report.shadowed);
    assert_eq!(
        report.resolved_executable,
        Some(
            wrappers
                .join("codex-usage-monit.cmd")
                .canonicalize()
                .unwrap()
        )
    );
    assert!(report.diagnostic.is_some());
}

#[cfg(windows)]
#[test]
fn windows_pathext_extension_order_and_invalid_entries_are_respected() {
    let (temp, _root, _target) = fixture();
    let directory = temp.path().join("bin");
    write_executable(&directory.join(BINARY_NAME), b"exe");
    write_executable(&directory.join("codex-usage-monit.cmd"), b"cmd");
    let result = resolve_path_entry_with_extensions(
        Some(directory.as_os_str()),
        Some(OsStr::new("../other;.CMD;.EXE")),
    )
    .unwrap();
    assert_eq!(result, directory.join("codex-usage-monit.CMD"));
    let result = resolve_path_entry_with_extensions(
        Some(directory.as_os_str()),
        Some(OsStr::new(".EXE;.CMD")),
    )
    .unwrap();
    assert_eq!(result, directory.join("codex-usage-monit.EXE"));
}

#[cfg(windows)]
#[test]
fn windows_adoption_held_process_fixture() {
    if env::var_os("MONIT_TEST_HOLD_ADOPTION_PROCESS").is_none() {
        return;
    }
    // The parent retains stdin until the test is finished. No timing-based
    // sleep is required to keep this actual PE image mapped in Windows.
    let mut byte = [0_u8; 1];
    let _ = io::stdin().read_exact(&mut byte);
}

#[cfg(windows)]
#[test]
fn windows_launcher_probe_rejects_malformed_pe_without_loader_dialogs() {
    use windows_sys::Win32::System::Diagnostics::Debug::GetThreadErrorMode;
    let (temp, root, target) = fixture();
    let entry = temp.path().join("malformed.exe");
    let mut bytes = vec![0; 88];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&64_u32.to_le_bytes());
    bytes[64..68].copy_from_slice(b"PE\0\0");
    write_executable(&entry, &bytes);
    let error_mode = unsafe { GetThreadErrorMode() };
    assert!(!verify_compatible_launcher(&root, &entry, &digest(&bytes), &target).unwrap());
    assert_eq!(unsafe { GetThreadErrorMode() }, error_mode);
    assert!(!root.join(REGISTRATION).exists());
    assert!(!root.join(JOURNAL).exists());
    assert!(!fs::read_dir(&root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".launcher-probe-")
    }));
}

#[cfg(windows)]
#[test]
fn windows_running_incompatible_executable_fails_before_recorder_and_can_retry() {
    let (temp, root, _) = fixture();
    let old_bytes = fs::read(env::current_exe().unwrap()).unwrap();
    let mut new_bytes = old_bytes.clone();
    new_bytes.extend_from_slice(b"different candidate PE overlay");
    let target = install_bytes(&root, &AgentInfo::local(), &new_bytes).unwrap();
    let directory = temp.path().join("portable CLI");
    let entry = directory.join(BINARY_NAME);
    write_executable(&entry, &old_bytes);
    struct HeldProcess(std::process::Child);
    impl Drop for HeldProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = HeldProcess(
        Command::new(&entry)
            .env("MONIT_TEST_HOLD_ADOPTION_PROCESS", "1")
            .args([
                "--exact",
                "update::tests::windows_adoption_held_process_fixture",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let calls = Cell::new(0);
    let error = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, true),
        &directory,
        None,
        || {
            calls.set(calls.get() + 1);
            Ok(absent(&target))
        },
    )
    .unwrap_err();
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("update_entry_busy"), "{diagnostic}");
    assert!(diagnostic.contains(&target.executable.display().to_string()));
    assert!(diagnostic.contains("update apply --scope node --install-dir"));
    assert_eq!(calls.get(), 0);
    assert!(!root.join(JOURNAL).exists());
    assert!(!root.join(REGISTRATION).exists());
    assert_eq!(fs::read(&entry).unwrap(), old_bytes);
    assert!(child.0.try_wait().unwrap().is_none());
    drop(child);
    let report = apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, true),
        &directory,
        None,
        || {
            calls.set(calls.get() + 1);
            Ok(absent(&target))
        },
    )
    .unwrap();
    assert_eq!(report.outcome, "complete");
    assert_eq!(calls.get(), 1);
    assert_eq!(fs::read(entry).unwrap(), new_bytes);
}

#[cfg(windows)]
#[test]
fn unregister_preserves_changed_entries_and_pending_updates() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&target)),
    )
    .unwrap();
    let bytes = fs::read(&entry).unwrap();
    fs::write(&entry, b"external replacement").unwrap();
    assert!(
        unregister_cli_at(&root, &entry)
            .unwrap_err()
            .to_string()
            .contains("update_entry_conflict")
    );
    assert!(root.join(REGISTRATION).exists());
    fs::write(&entry, &bytes).unwrap();
    let mut journal: UpdateJournal = read_json(&root.join(JOURNAL)).unwrap().unwrap();
    journal.phase = "failed".into();
    write_json(&root.join(JOURNAL), &journal).unwrap();
    assert!(
        unregister_cli_at(&root, &entry)
            .unwrap_err()
            .to_string()
            .contains("update_pending")
    );
    assert!(entry.exists());
    journal.phase = "complete".into();
    write_json(&root.join(JOURNAL), &journal).unwrap();
    assert!(unregister_cli_at(&root, &entry).unwrap());
    assert!(!entry.exists());
    assert!(!root.join(REGISTRATION).exists());
    assert!(target.executable.exists());
    assert!(unregister_cli_at(&root, &entry).unwrap());
}

#[cfg(windows)]
#[test]
fn repair_recreates_only_a_missing_registered_launcher() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&target)),
    )
    .unwrap();
    fs::remove_file(&entry).unwrap();
    assert_eq!(repair_cli_at(&root).unwrap(), entry);
    assert_eq!(
        fs::read(&entry).unwrap(),
        fs::read(&target.executable).unwrap()
    );
    assert_eq!(
        proxy_target(&root, &entry).unwrap(),
        Some(target.executable)
    );
    fs::write(&entry, b"user replacement").unwrap();
    assert!(
        repair_cli_at(&root)
            .unwrap_err()
            .to_string()
            .contains("update_entry_conflict")
    );
    assert_eq!(fs::read(entry).unwrap(), b"user replacement");
}

#[cfg(windows)]
#[test]
fn repair_uses_newer_registered_selection_independently_of_the_calling_build() {
    let (temp, root, first) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    apply_at(
        &root,
        first.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&first)),
    )
    .unwrap();
    let mut newer_info = AgentInfo::local();
    newer_info.version = "9999.0.0".into();
    newer_info.build_id = "f".repeat(64);
    let newer = install_bytes(&root, &newer_info, b"newer selected immutable build").unwrap();
    apply_at(
        &root,
        newer.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&newer)),
    )
    .unwrap();
    // The test/calling executable still identifies as the original build.
    assert_ne!(AgentInfo::local().build_id, newer.build_id);
    assert_eq!(selected_cli_executable_at(&root).unwrap(), newer.executable);
    let before = fs::read(root.join(REGISTRATION)).unwrap();
    assert_eq!(repair_cli_at(&root).unwrap(), entry);
    assert_eq!(selected_cli_executable_at(&root).unwrap(), newer.executable);
    assert_eq!(fs::read(root.join(REGISTRATION)).unwrap(), before);
    fs::remove_file(&entry).unwrap();
    repair_cli_at(&root).unwrap();
    assert_eq!(selected_cli_executable_at(&root).unwrap(), newer.executable);
    assert_eq!(fs::read(entry).unwrap(), b"newer selected immutable build");
}

#[cfg(windows)]
#[test]
fn selected_cli_rejects_pending_updates_and_unverified_launcher_or_version() {
    let (temp, root, target) = fixture();
    let directory = temp.path().join("bin");
    let entry = directory.join(BINARY_NAME);
    apply_at(
        &root,
        target.clone(),
        &options(UpdateScope::Node, false),
        &directory,
        None,
        || Ok(absent(&target)),
    )
    .unwrap();
    let mut journal: UpdateJournal = read_json(&root.join(JOURNAL)).unwrap().unwrap();
    journal.phase = "prepared".into();
    write_json(&root.join(JOURNAL), &journal).unwrap();
    assert!(
        selected_cli_executable_at(&root)
            .unwrap_err()
            .to_string()
            .contains("update_pending")
    );
    journal.phase = "complete".into();
    write_json(&root.join(JOURNAL), &journal).unwrap();
    let bytes = fs::read(&entry).unwrap();
    fs::write(&entry, b"external change").unwrap();
    assert!(
        selected_cli_executable_at(&root)
            .unwrap_err()
            .to_string()
            .contains("update_entry_conflict")
    );
    fs::remove_file(&entry).unwrap();
    assert!(selected_cli_executable_at(&root).is_err());
    fs::write(&entry, bytes).unwrap();
    fs::write(&target.executable, b"tampered selection").unwrap();
    assert!(
        selected_cli_executable_at(&root)
            .unwrap_err()
            .to_string()
            .contains("update_checksum_mismatch")
    );
}
