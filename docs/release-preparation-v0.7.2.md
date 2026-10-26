# v0.7.2 Windows recorder registration fix

Prepared on 2026-10-27 from published v0.7.1,
`5ec87ccbfdcb59e0813e3fbf064c0391d625d332`.

## Scope and remote integration

The user authorized publishing the recorder fix and installing the official
release after v0.7.0 installation exposed a native Windows registration failure.
Before publication, the remote advanced to v0.7.1. The checkout was fast-forwarded
and the patch version advanced to v0.7.2; the existing tag is unchanged.

The remote fixes weekly-reset reconciliation, budgets actual projection work,
and preserves quota, token buckets and deduplicated totals when weekly projection
is unavailable. Its history preservation and compatibility guarantees remain
intact. Its deterministic version-popup fixtures supersede the equivalent local
test changes. No additional TUI or history changes are part of this patch.

The local changes were preserved in stash
`61a5f863d95d04f22ec9b2d260a2d9506c4c4f7f` and under
`target/remote-sync-2026-10-27/` before synchronization.

## Reproduction and repair

The hash-verified official v0.7.0 executable installed its commands, but recorder
registration failed in `schtasks /Create /XML` with
`(1,40): unable to switch encoding`. Neither v0.7.1 commit changes that path.

The registration file now uses UTF-16LE with a BOM and matching declaration.
Native task export also normalizes default IdleSettings, omits LeastPrivilege
RunLevel, and can replace a trigger SID with a qualified account name. The
verifier accepts only equivalent defaults and resolves account names through
Windows to the expected user SID. Canonical fingerprints use that verified SID.
Unknown or duplicate nodes, non-default settings, elevation, and other identities
remain errors. Executable, arguments and working-directory checks remain intact.

Regressions cover Unicode and XML escaping, fingerprint equivalence, accepted
defaults and rejected mutations, and malformed identities. The opt-in native
test registers a unique disabled task with a nonexistent payload, verifies its
export and identity, deletes it and verifies cleanup. It never runs the payload.
See [Windows testing](windows-testing.md).

## Verification environment and prior evidence

Native Windows x64 (`x86_64-pc-windows-msvc`), Rust 1.97.0, PowerShell 7.4.1,
interactive `Ghost` account. A SID-private TEMP is used outside the repository.
Final logs belong under `target/remote-sync-2026-10-27/`.

Before synchronization, service tests passed 109 tests with one opt-in test
ignored; the native task test passed separately. Format and all-target Clippy
passed. Evidence: `target/local-install-2026-10-26/registration-fix-verification.log`,
input manifest `955d7d2f4dd24910e7a755eb9da9f741e8826796da897ffec66a282e8ccd715d`.

The earlier full attempt passed 2,036 library tests and failed five popup tests
that depended on release-note length. Its follow-up was interrupted during
compilation and is not a completed test result. Remote v0.7.1 already fixes those
fixtures. Neither earlier run establishes full validation of this merged source.

The final Windows checkpoint runs:

```powershell
$env:RUST_TEST_THREADS = '4'
$env:PATH = 'D:\Program_Files\Git\bin;' + $env:PATH
$env:SHELL = 'D:/Program_Files/Git/usr/bin/sh.exe'
pwsh -NoProfile -File scripts/windows/verify.ps1 -RepositoryPath D:\Workspace\codex-usage-monit -CargoTargetDir D:\Workspace\codex-usage-monit\target\release-prep-2026-10-25\cargo-target -CargoBuildDir D:\Workspace\codex-usage-monit\target\release-prep-2026-10-25\cargo-build -TestTempDir C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023
$env:CARGO_TARGET_DIR = 'D:\Workspace\codex-usage-monit\target\release-prep-2026-10-25\cargo-target'
$env:CARGO_BUILD_BUILD_DIR = 'D:\Workspace\codex-usage-monit\target\release-prep-2026-10-25\cargo-build'
$env:TEMP = 'C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023'
$env:TMP = $env:TEMP
cargo test --locked --lib service::tests::windows_task_registration_accepts_unicode_xml_without_starting_payload -- --exact --ignored --nocapture
```

Linux and macOS runtime checks require the authorized exact-commit GitHub
checkpoint: this host has no macOS runtime and its Docker Linux engine is
unavailable (`docker version --format '{{json .Server}}'` failed because
`//./pipe/dockerDesktopLinuxEngine` was not found). The existing v0.7.1 green
runs cannot validate this patch. Release
requires a green checkpoint, fresh advisory audit, explicit verification gate,
and smoke checks of all five release binaries. Installation must additionally
verify the real current-user recorder task and a matching fresh heartbeat.

## Final local result

The dirty candidate on base `5ec87ccbfdcb59e0813e3fbf064c0391d625d332`
has 237 verification inputs with SHA-256
`8bed926fd162e9c73801513a7e1fede0477090908eafb23b26599eaa2182c575`.
The manifests before and after verification match:
`target/release-prep-2026-10-25/v0.7.2-final-source.sha256` and
`target/release-prep-2026-10-25/v0.7.2-after-verification.sha256`.

The complete Windows entry point above exited 0. Formatting, all-target Clippy,
31 Python installer/package contracts, 78 verification-wrapper cases,
17 development-launcher cases and 10 permission-repair contracts passed.
The library passed 2,060 tests with two opt-in tests ignored; integration targets
passed 128 tests with three opt-in tests ignored. The real Windows TUI interaction
tests and both version/offline-JSON CLI smoke checks passed.
Full log: `target/remote-sync-2026-10-27/v0.7.2-full-windows.log`.

Ordinary-suite omissions are the explicit native task test, two synthetic
measurement fixtures, the real-user-history benchmark, and a launcher regression
requiring a separately supplied previous trusted build. These omissions are not
reported as passes.

The explicit native task regression then exited 0 (one passed, zero ignored),
including real registration, export, identity verification and cleanup.
Log: `target/remote-sync-2026-10-27/v0.7.2-native-registration.log`.
