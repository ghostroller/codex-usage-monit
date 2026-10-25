# v0.7.0 release preparation

Prepared on 2026-10-25 from `6c2afcdcab239538239ddc1c01bda948d3f9aee1`
with local changes. This record captures the local release-candidate evidence.
After local preparation, the user authorized a GitHub CI checkpoint and v0.7.0
publication if it passes. Hosted results and publication are recorded by the
Actions runs for the exact candidate commit; the local results below do not
claim those later checks have passed.

## Review scope and corrections

The review covers changes since v0.6.0, particularly startup and shutdown,
session-deletion authorization and recovery, private persistence, and the
release verification chain.

1. **SQLite recovery-file safety.** The deleted-session ledger previously
   checked side-file existence without enforcing Windows live ACLs or either
   platform's hard-link limit. A synthetic rollback-journal hard link reproduced
   writes to an unrelated private fixture. Validate journal/WAL/SHM before open
   and before commit; preserve hot-journal recovery and DELETE-journal cleanup.
   The initial 12 regression cases all failed against the original behavior.
2. **Unresolved deletion capacity.** Preparation could publish a 513th pending
   member, after which the complete reconciliation query refused every read.
   Check the union of existing pending members and the confirmed batch inside
   the same locked write transaction, before publishing any member. A rejected
   batch must not send a delete request. Existing malformed or legacy
   over-capacity ledgers remain explicit errors rather than truncated results.
   The same review found that accumulated payload bytes/rows or growth during
   an outcome update could exceed complete-read budgets. Validate final row
   count, total bytes and largest record inside both write transactions;
   regressions exercise rollback and exact boundaries with small private test
   budgets without changing production limits.
3. **Initial display latency.** The prior Windows ConPTY failure waited for
   initial history persistence and projection before displaying local rows.
   Deliver the local snapshot separately while the same worker completes
   history initialization. Keep worker ownership and loading flags active;
   handle completion, poison and exit without joining a still-running worker
   from the display loop. The existing eight-second PTY deadline is retained.

The deletion review also checked endpoint binding, confirmation-bound scope,
durable capture before the RPC, final rollout identity checks, notifications,
missing-file reconciliation, redaction and stopped-refresh behavior. These
checks did not identify another confirmed deletion-authorization defect.

## Version and compatibility

`Cargo.toml`, the root package in `Cargo.lock`, and the bundled changelog select
0.7.0. The version popup continues to use the exact package-version section.
The changelog includes session/turn details, version information, confirmed
session deletion with retained usage, memory improvements, recovery fixes and
the scoped Rust review migration.

History schema 2, remote protocol v5, estimator revision 8 and API catalog
revision 5 remain unchanged. Synchronized nodes still require the same source
build and model-catalog fingerprint. The date in the candidate changelog must
be finalized before the eventual release checkpoint if publication is delayed.

## Local evidence

The native runner is Windows x64 (`x86_64-pc-windows-msvc`), Rust 1.97.0 and
PowerShell 7.4.1. Tests use the native account and private TEMP:
`C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023`.
The commands, manifests, logs and local package are retained under
`target/release-prep-2026-10-25/`. Git safe-directory and Git Bash PATH overrides
are process-local.

The source manifest records raw SHA-256 hashes of `src`, `tests`, `scripts`,
`.cargo`, `.github`, Cargo manifests/lockfile, toolchain, build script, changelog
and the model-catalog example. Verification prose is outside that scope.
The initial regression snapshot was
`8c9ddbc21f2f5c6ea0eb3ad0d6d9f98da17b44f79f1646ce8ea63baba28eeeaf`.

The local `cargo-audit 0.22.2 audit --deny warnings` check passed for the candidate
lockfile (269 dependencies; 1,352 advisories; advisory database commit
`21d909e4c8bd4e14c7aea58bf980c881a0053ab1`). The tool is installed only beneath
the evidence directory. The audited lockfile SHA-256 is
`aca73d6316e7f4fff69c16f775922c523da776b16628d306c7237123ad8a918b`.
See `advisory-results.log`.

Release-evidence reuse contracts passed 11 tests. Workflow contracts passed
10 tests using the installed Git Bash (`D:\Program_Files\Git\bin`). The first
workflow run selected the unavailable WSL Bash shim and failed; its log is
retained separately from the corrected runner result. No workflow logic was
changed to accommodate that environment failure.

The candidate source manifest is
`01b248359a0bb3c6859cf2fffa3913fb6e3dbf0d489bb87b0480cb4bcfee16d3`
(236 files; `candidate-final-source.sha256`). The previous compile-fix and
resumed-run manifests both matched
`9bbf602295a172e18d7d6b9cedafc2a4d827659791c486d27921de525efd4f54`.
The first compile exposed an unsupported `usize` SQL read in a newly added test
helper; that helper now reads `i64` and checks conversion. A later compilation
was interrupted before tests returned a result, so it is not counted as a pass.
Both diagnostic logs are retained. The first completed focused run passed 112
tests but failed a new render assertion that assumed the whole task title fit
in its table column; that panic poisoned a shared test lock and caused four
secondary failures. The regression now checks the complete model title and
the visible row prefix separately, preserving the blocked-history interaction
and exit assertions. A second fixture correction uses the actual Summary key
(`U`); `2` correctly opens Trends. No production change or timeout increase was
made to address these fixture assertions.

The local wrapper uses separate target/build directories beneath the evidence
directory, `RUST_TEST_THREADS=4`, and the private TEMP above:

```powershell
cargo fmt --all
pwsh -NoProfile -File target/release-prep-2026-10-25/verify-local.ps1 -Stage Focused
pwsh -NoProfile -File target/release-prep-2026-10-25/verify-local.ps1 -Stage Full
pwsh -NoProfile -File target/release-prep-2026-10-25/verify-local.ps1 -Stage Release
```

Its substantive commands are:

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --lib -- deleted_sessions initial_refresh initial_snapshot poisoned_refresh session_management session_delete version_info --test-threads=4
cargo test --locked --test tui_pty -- --test-threads=1
scripts/windows/verify.ps1 -ScriptContractsOnly -RepositoryPath D:\Workspace\codex-usage-monit -CargoTargetDir D:\Workspace\codex-usage-monit\target\release-prep-2026-10-25\cargo-target -CargoBuildDir D:\Workspace\codex-usage-monit\target\release-prep-2026-10-25\cargo-build -TestTempDir C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023
cargo test --locked --all-targets --no-fail-fast -- --test-threads=4
cargo build --locked --release --bin codex-usage-monit
python scripts/check-release-binary.py target/release-prep-2026-10-25/cargo-target/release/codex-usage-monit.exe
python scripts/package-release.py package target/release-prep-2026-10-25/cargo-target/release/codex-usage-monit.exe --output-dir target/release-prep-2026-10-25/package --metadata-only
```

Formatting and all-target Clippy with warnings denied pass. The affected library
run passes **117 tests**, including all 27 new regressions, in 34.60 seconds.
The native ConPTY target passes **both tests** in 3.95 seconds, including the
previously failing keyboard/mouse/search/resize/exit scenario with its unchanged
eight-second initial-data deadline. See `focused-command.log`.
`post-focused-library-source.sha256` still matches the candidate manifest.

The complete Windows run passes **2,166 tests, 0 failed, 4 existing ignored**
across 13 Cargo targets. The library accounts for 2,038 passes and one ignore
in 832.25 seconds; ConPTY passes both tests again in 3.71 seconds. The ignored
cases remain the synthetic-history fixture and measurement, the opt-in real
history benchmark, and the portable-launcher test needing another trusted
build. No ignore was added. All 31 installer/package Python contracts and the
native PowerShell wrapper/development/permission contracts pass. See
`full-command.log` and `full-summary.json`; `post-full-source.sha256` matches the
candidate manifest exactly.

After that run, only the changelog footer's comparison references were updated
to v0.7.0. `check-final-metadata.py` verifies that all other 235 inputs remain
byte-identical and the selected 0.7.0 release-note body is unchanged after line
ending normalization. This documentation-only correction is checked with the
final release build and smoke rather than repeating the entire native suite.
The resulting release-input manifest is
`523cf7132827ae4edaae37c95efa1229da8dc5cc5e8fa14d58a9a78c4256a020`
(`release-source.sha256`); see `final-metadata.log` for the comparison checks.

The final Windows optimized build passes in 3m 36s. Its `--version` output is
`codex-usage-monit 0.7.0`, and the isolated offline JSON smoke passes with fixture
tasks and explicit partial status. Packaging verifies the executable's own
reported identity and SHA-256 against the actual bytes. See
`release-command.log`; `post-release-source.sha256` still matches the release
input manifest.

- Target: `x86_64-pc-windows-msvc`; protocol: 5.
- Source build ID: `2a514beba7752901b5f60cc35b92c197cebe234821e23ec9c18fcc42f632d766`.
- Binary SHA-256: `d7ecfe092f1fe415b5e03364eaf4483ebd3b979f37e1b5e1c9cb0c5c1c0a2f66`.
- Binary size: 20,461,568 bytes.
- Local package: `target/release-prep-2026-10-25/package/codex-usage-monit-x86_64-pc-windows-msvc.exe`.
- Metadata: `target/release-prep-2026-10-25/package/release-metadata-x86_64-pc-windows-msvc.json`.

The review fixes, regressions, version and documentation form one release
candidate. Local preparation did not change workflows or mutate the remote.

## Publication requirements still apply

Docker's Linux engine is unavailable on this host: `docker info` reports that
the `dockerDesktopLinuxEngine` pipe does not exist. No Linux or macOS native
runtime pass is claimed. Windows tests do not exercise the Unix-gated real
desktop-proxy protocol fixtures. Local fixture success also does not establish
compatibility with a live desktop App Server installation.

The next Linux preflight is to start the existing Docker Desktop Linux engine,
then run `docker info --format '{{.Architecture}}'` and
`docker image inspect rust:1.97.1-bookworm --format '{{.Os}}/{{.Architecture}}'`.
Follow [Linux setup and verification](linux-testing.md) for any missing image,
then run `sh scripts/test-linux-docker.sh --platform linux/amd64` on this x64
host. On a native Mac with the same candidate source, run
`sh scripts/verify-unix.sh`; cross-checks do not substitute for that runtime pass.

Before publication, settle every intended file, commit and push the candidate,
then run the authorized integration checkpoint described in
[testing.md](testing.md). Verify the exact commit's Linux, macOS, Windows and
dependency-audit jobs before tagging that same green SHA as v0.7.0. Preserve
the release workflow's fresh advisory audit, verification gate and smoke checks
of all five platform release binaries.
