//! Stable executable selection for the Unix standalone Codex installation.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{ServiceOptions, is_executable_file};
use crate::session_launch::{PrepareError, resolve_executable};

/// Resolve the recorder's executable, keeping the standalone installer's
/// stable entry when automatic discovery selected its current release.
pub(crate) fn resolve_for_service(
    codex_home: &Path,
    override_path: Option<&Path>,
    path: &OsStr,
    current_dir: &Path,
) -> Result<PathBuf, PrepareError> {
    let selected = resolve_executable("codex", override_path, path, current_dir)?;
    if override_path.is_none()
        && let Ok(standalone) = codex_home.join("packages/standalone").canonicalize()
        && let Some((entry, resolved)) = current_executable(&standalone)
        && resolved == selected
    {
        return Ok(entry);
    }
    Ok(selected)
}

pub(super) fn rebind_retired_release(options: &mut ServiceOptions) {
    if !options.offline
        && let Some(executable) = &options.codex_bin
        && let Some(current) = current_for_retired_release(&options.codex_home, executable)
    {
        options.codex_bin = Some(current);
    }
}

fn current_for_retired_release(codex_home: &Path, executable: &Path) -> Option<PathBuf> {
    let standalone = codex_home.join("packages/standalone").canonicalize().ok()?;
    let release = release_directory(&standalone.join("releases"), executable)?;
    // An executable missing from a release that still exists can indicate a
    // damaged or deliberately pinned installation. Only migrate removed releases.
    if !release
        .symlink_metadata()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        return None;
    }
    current_executable(&standalone).map(|(entry, _)| entry)
}

fn current_executable(standalone: &Path) -> Option<(PathBuf, PathBuf)> {
    let releases = standalone.join("releases");
    let current = standalone.join("current");
    fs::read_link(&current).ok()?;
    let release = current.canonicalize().ok()?;
    if release.parent()? != releases {
        return None;
    }
    // Both the current package and the legacy layout expose `codex` at the
    // release root. The current package links it to `bin/codex`.
    let entry = current.join("codex");
    let resolved = entry.canonicalize().ok()?;
    if release_directory(&releases, &resolved)? != release || !is_executable_file(&resolved) {
        return None;
    }
    Some((entry, resolved))
}

fn release_directory<'a>(releases: &Path, executable: &'a Path) -> Option<&'a Path> {
    if executable.file_name()? != "codex" {
        return None;
    }
    let parent = executable.parent()?;
    let release = if parent.parent()? == releases {
        parent
    } else if parent.file_name()? == "bin" {
        parent.parent()?
    } else {
        return None;
    };
    if release.parent()? != releases {
        return None;
    }
    let name = release.file_name()?.to_str()?;
    // The official Unix installer names packages <version>-<target>.
    let version = [
        "-aarch64-apple-darwin",
        "-x86_64-apple-darwin",
        "-aarch64-unknown-linux-musl",
        "-x86_64-unknown-linux-musl",
    ]
    .into_iter()
    .find_map(|target_suffix| name.strip_suffix(target_suffix))?;
    semver::Version::parse(version).ok()?;
    Some(release)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn executable(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, ServiceOptions, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let options = ServiceOptions::new(
            root.join("umt"),
            root.join("codex home"),
            root.join("history-v1"),
            root.join("status.json"),
            None,
        );
        let standalone = options.codex_home.join("packages/standalone");
        (temp, options, standalone)
    }

    #[test]
    fn standalone_codex_discovery_keeps_an_entry_across_package_rotation() {
        let (temp, options, standalone) = fixture();
        let first = standalone.join("releases/0.151.0-aarch64-apple-darwin");
        let second = standalone.join("releases/0.171.0-aarch64-apple-darwin");
        executable(&first.join("bin/codex"));
        symlink("bin/codex", first.join("codex")).unwrap();
        executable(&second.join("codex"));
        symlink(&first, standalone.join("current")).unwrap();

        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        symlink(standalone.join("current/bin/codex"), bin.join("codex")).unwrap();
        let entry =
            resolve_for_service(&options.codex_home, None, bin.as_os_str(), temp.path()).unwrap();
        assert_eq!(entry, standalone.join("current/codex"));
        assert_eq!(
            resolve_for_service(&options.codex_home, None, second.as_os_str(), temp.path())
                .unwrap(),
            second.join("codex")
        );
        assert_eq!(
            resolve_for_service(
                &options.codex_home,
                Some(&bin.join("codex")),
                bin.as_os_str(),
                temp.path(),
            )
            .unwrap(),
            first.join("bin/codex")
        );

        fs::remove_file(standalone.join("current")).unwrap();
        symlink(&second, standalone.join("current")).unwrap();
        fs::remove_dir_all(first).unwrap();
        assert_eq!(entry.canonicalize().unwrap(), second.join("codex"));
    }

    #[test]
    fn standalone_codex_upgrade_rebinds_a_removed_release_and_preserves_service_options() {
        let (_temp, mut options, standalone) = fixture();
        let retired = standalone.join("releases/0.151.0-aarch64-unknown-linux-musl");
        let current = standalone.join("releases/0.171.0-aarch64-unknown-linux-musl");
        executable(&current.join("bin/codex"));
        symlink("bin/codex", current.join("codex")).unwrap();
        symlink(&current, standalone.join("current")).unwrap();

        for path in [retired.join("bin/codex"), retired.join("codex")] {
            options.codex_bin = Some(path);
            options.lookback_days = 21;
            let mut expected = options.clone();
            expected.codex_bin = Some(standalone.join("current/codex"));
            rebind_retired_release(&mut options);
            assert_eq!(options, expected);
        }
    }

    #[test]
    fn standalone_codex_upgrade_retains_unrelated_and_unverified_executables() {
        let (temp, mut options, standalone) = fixture();
        let retained = standalone.join("releases/0.151.0-x86_64-apple-darwin");
        let selected = standalone.join("releases/0.171.0-x86_64-apple-darwin");
        fs::create_dir_all(&retained).unwrap();
        executable(&selected.join("codex"));
        symlink(&selected, standalone.join("current")).unwrap();

        let custom = temp.path().join("custom/bin/codex");
        for path in [
            retained.join("bin/codex"),
            standalone.join("releases/custom/bin/codex"),
            custom.clone(),
        ] {
            options.codex_bin = Some(path.clone());
            rebind_retired_release(&mut options);
            assert_eq!(options.codex_bin.as_deref(), Some(path.as_path()));
        }

        let removed = standalone.join("releases/0.166.0-x86_64-apple-darwin/bin/codex");
        options.codex_bin = Some(removed.clone());
        fs::set_permissions(selected.join("codex"), fs::Permissions::from_mode(0o600)).unwrap();
        rebind_retired_release(&mut options);
        assert_eq!(options.codex_bin.as_deref(), Some(removed.as_path()));

        executable(&custom);
        fs::remove_file(selected.join("codex")).unwrap();
        symlink(&custom, selected.join("codex")).unwrap();
        rebind_retired_release(&mut options);
        assert_eq!(options.codex_bin.as_deref(), Some(removed.as_path()));
    }
}
