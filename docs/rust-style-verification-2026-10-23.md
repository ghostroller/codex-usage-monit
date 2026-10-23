# T0–T2 local verification — 2026-10-23

This batch implements the [Rust policy](rust-style.md), fail-closed handling of
poisoned history/cache/ownership state, and the Windows SCM unsafe audit. T3–T5
remain separate work. No GitHub CI, push, release tag, or publication was run.

## Changes and regressions

- T0: document the application's adoption scope, review rules, incremental lint
  policy, and verification requirements; link the policy from AGENTS and README.
- T1: stop TUI refresh after history/cache poison, retain the displayed snapshot,
  expose the restart diagnostic, and skip poisoned shutdown writes. Commit Git
  evidence explicitly after successful export preparation. Reject poisoned
  profile/recorder authorization without panicking during cleanup.
- T2: document SCM unsafe preconditions and enforce
  `clippy::undocumented_unsafe_blocks` for that module family. Check UTF-16
  terminators before safe wrappers enter FFI; preserve process-attribute buffer
  lifetimes and the allocation provenance of extended startup/ACE SID pointers.

There are **14 new test functions**: TUI 4, Git/runtime/export 5, profile ownership
1, recorder ownership 2, and Windows SCM 2. The recorder validation regression
uses three isolated child processes for Acquired, local Busy, and OS Busy. It
checks that validation does not hold the registry, injects a real held-lock panic,
and verifies rejection and OS-lock cleanup without timing sleeps.

The shutdown regression failed against the original production implementation:
the interrupted staged value `900` reached disk instead of preserving committed
value `10`. It passes with the fix. The original implementation plus that one
test is retained in the local evidence directory as `red-source`.

## Source and environment

- Base commit: `22087b1584c9cf0b76295a03658aa52e073830cf`; tested changes are uncommitted.
- Native Windows x64, `x86_64-pc-windows-msvc`, rustc 1.97.0
  (`2d8144b78`, 2026-07-07), PowerShell 7.4.1.
- Windows account: `DESKTOP-U23KNI9\Ghost`.
- Test temporary root:
  `C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023`.
  The native verifier checked inherited permissions; it did not repair ACLs.
- Evidence root: `D:\Workspace\codex-usage-monit\target\rust-style-2026-10-23`.
  Logs and manifests there are local, ignored build artifacts.

The SHA-256 identities below hash the UTF-8 text of a sorted manifest normalized
to LF line endings, covering source, tests, scripts, `.cargo`, Cargo
manifests/lockfile, toolchain file, and build script. This manifest excludes
documentation; the style Markdown edited in this batch is not a build/test input.
Some manifest
files were written with Windows CRLF; their raw-file hash differs. To verify an
identity, hash `Path(path).read_text(encoding="utf-8").encode("utf-8")` with
Python `hashlib.sha256` (universal-newline reading normalizes CRLF).

Tracked inputs outside those manifest paths remain at the base commit, apart
from the AGENTS/README policy links. In particular, two embedded documentation
inputs are unchanged from the baseline: `CHANGELOG.md` has SHA-256
`13cb018b4b1238f2ca7b4dcbd6150fe0b89355a8c09252842eb92241c6f8f956`,
and `docs/model-catalog.example.json` has SHA-256
`117f3cdfe83a6dfae676fe09b9e43d581e540267eb020e2686cb8044394999c3`.

| Source | Manifest SHA-256 |
| --- | --- |
| Original production code plus shutdown regression | `fdbdd7095668c4f39a8f2f2b222c2f67b95c017ed460e4d856127ccbf448abef` |
| First full run, before shortening the recorder critical section | `d240d95af7ab51d7ab7376bed53bbb592e1c5c7db5e0f6ebb29fcbb84e01c572` |
| Final implementation | `c0333ce1e1e42577f3d0811058919f6a66c71b2be1685a4a47a992bb1547a5f2` |

## Commands

All commands use the native Windows environment from
`.agent/environment.local.md` and [testing workflow](testing.md). Git's
`safe.directory` override is process-local because this checkout's owner differs
from the test account. No global Git configuration was changed.

```powershell
$env:GIT_CONFIG_COUNT = '1'
$env:GIT_CONFIG_KEY_0 = 'safe.directory'
$env:GIT_CONFIG_VALUE_0 = 'D:/Workspace/codex-usage-monit'
$repo = 'D:\Workspace\codex-usage-monit'
$evidence = Join-Path $repo 'target\rust-style-2026-10-23'
$testTemp = Join-Path $env:LOCALAPPDATA 'codex-usage-monit\test-temp-style-20261023'
$parameters = @{
    RepositoryPath = $repo
    CargoTargetDir = Join-Path $evidence 'red-target'
    CargoBuildDir = Join-Path $evidence 'red-build'
    TestTempDir = $testTemp
}

# First implementation: fmt + Clippy + poison tests, then the full entry point.
& scripts/windows/verify.ps1 @parameters -TestFilter poison -SkipSmoke
& scripts/windows/verify.ps1 @parameters -SkipFormat -SkipClippy

# Final implementation: saved orchestration, including the commands below.
pwsh -NoProfile -File target/rust-style-2026-10-23/final-local.ps1
```

`final-local.ps1` runs formatting, Clippy and poison tests through the native
entry point, then retains its imported MSVC environment and executes:

```powershell
$env:CARGO_TARGET_DIR = $parameters.CargoTargetDir
$env:CARGO_BUILD_BUILD_DIR = $parameters.CargoBuildDir
$env:TEMP = $testTemp
$env:TMP = $testTemp
cargo test --locked --all-targets --no-fail-fast -- --test-threads=4
& scripts/windows/verify.ps1 @parameters -SkipTests -SkipFormat -SkipClippy
```

The final run uses four test threads to reduce concurrent filesystem pressure.
It does not skip the known failures; `--no-fail-fast` allows integration targets
to run after a failing library target. The final smoke command skips Rust tests
and lint only because they were executed separately against the same source.

The first full run's failures were isolated using its existing compiled binary:

```powershell
$binary = Join-Path $evidence 'red-build\debug\deps\codex_usage_monit-d8ffe5b596c30ec3.exe'
$filters = @(
    'contended_try_is_non_blocking'
    'projection_revision_keeps_sql_stamps_and_policy_in_one_snapshot'
    'prepared_batch_file_identity_cas_rejects_a_concurrent_mapping_change'
    'unchanged_prepared_batch_still_rejects_a_concurrent_mapping_change'
    'unchanged_descriptor_batch_cas_failure_cannot_commit_history'
    'sqlite_committed_stamp_and_data_remain_consistent_in_a_reserved_revision_snapshot'
    'application_refresh_shares_quota_with_background_overview_and_invalidates_quota_policy'
)
& $binary @filters --test-threads=1
```

The baseline runs used the same temporary/build directories:

```powershell
$baseline = $parameters.Clone()
$baseline.RepositoryPath = Join-Path $evidence 'red-source'
# Original red run, before implementation:
& scripts/windows/verify.ps1 @baseline -TestFilter poisoned_history_exit_does_not_persist_staged_observation -SkipFormat -SkipClippy -SkipSmoke
# Baseline CAS comparison after implementation:
(Get-Item -LiteralPath "$evidence\red-source\src\lib.rs").LastWriteTimeUtc = [DateTime]::UtcNow
& scripts/windows/verify.ps1 @baseline -TestFilter prepared_batch -SkipFormat -SkipClippy -SkipSmoke
& $binary --exact remote_sync::tests::unchanged_descriptor_batch_cas_failure_cannot_commit_history
```

Source `src/lib.rs` timestamps were refreshed when switching source copies to
force recompilation in the shared build directory; source bytes were unchanged.

## Results

| Check | Result | Log under evidence root |
| --- | --- | --- |
| Shutdown regression against original implementation | Expected failure: persisted 900 instead of 10 | `red-test-corrected.log` |
| First fmt, Clippy (`-D warnings`), poison filter | Passed; 10 tests | `focused-poison-corrected.log` |
| Installer/release Python contracts and Windows script contracts | Passed through the full native entry point | `windows-full.log` |
| First full library suite, default 16 test threads | 1,912 passed, 7 failed, 1 ignored; later targets/smoke not reached | `windows-full.log` |
| Seven failures re-run serially in one process, one test thread | 4 passed; the 3 CAS failures reproduced | `full-failures-isolated.log` |
| Original-code prepared-batch tests | 1 passed, 2 CAS failures reproduced | `baseline-cas-corrected.log` |
| Original-code remote CAS test | Same CAS failure reproduced | `baseline-remote-cas.log` |
| Final fmt, Clippy, poison filter | Passed; 11 tests | `final-focused.log` |
| Final all Rust targets, four test threads | 2,045 passed, 3 baseline CAS failures, 4 ignored; all targets ran | `final-all-targets.log` |
| Final CLI version/offline JSON smoke | Passed | `final-smoke.log` |

All **14 new regressions passed** in the final all-target run. Its library target
reported 1,917 passed, 3 failed and 1 ignored; integration targets reported 128
passed and 3 ignored, including both native ConPTY tests. The all-target Cargo
command correctly returned 101 for the three failures. This is not a green full
suite. Formatting, Clippy, smoke and the new regressions are green independently.
The final pre/post-run manifests both have the `c033...` identity above.
The four ignored tests are two opt-in synthetic measurements, the real-user
history benchmark, and the portable-launcher comparison requiring a different
trusted build. Their coverage is not claimed. `result.json` records the final
counts, individual new regressions, failed test names, and unchanged-source check.

The first full run and its serial subset rerun use the first-full-run manifest, not
the final implementation. The final implementation additionally shortens the
recorder registry critical section and adds its deterministic regression.

The first full run's four non-CAS failures were a 250 ms lease assertion, two
5/10-second SQLite test handshakes, and a TUI cache-validity assertion whose
production cache expires after 30 seconds. All four passed in the serial subset; this
supports load sensitivity but does not prove a single cause. No test timeout,
production TTL, or assertion was weakened.
All four also passed during the final four-thread all-target run.

The three CAS tests consistently return `Uncategorized` instead of `WouldBlock`
on this machine, including against the original code. They concern prepared
project-mapping publication and remote-sync handling, outside this batch's
modified execution paths. Follow-up should examine validation of the retained
old Windows file handle after atomic replacement, before identity-conflict
classification. This is a diagnostic lead, not a confirmed fix.

Discarded attempts are retained for transparency: `red-test.log` had a test
compilation error, `focused-poison.log` stopped on Clippy diagnostics that were
fixed, and `baseline-cas.log` reused the current binary before baseline
recompilation was forced. None is claimed as a passing or baseline result.

Linux/macOS runtime suites and hosted CI were not run. The local environment is
native Windows and the user explicitly prohibited GitHub CI. No live machine
service was installed; SCM coverage uses the repository's native fixtures.
