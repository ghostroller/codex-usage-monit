# Unified updates verification — 2026-09-20

Implementation branch: `codex/unified-node-updates`. All local runs started from
`39c703310e96c8cf3471e3db115b9e21aba2b81e` plus the recorded dirty snapshots.
No release tag or release publication is part of this verification.

The final release target is **0.5.1**. The initial implementation and local
installation below used the provisional, unpublished number 0.6.0; the later
renumbering is recorded separately so the original evidence remains accurate.

## Completed local checks

| Environment | Command | Result and source identity |
| --- | --- | --- |
| macOS 15.7.2 ARM64, Rust 1.97.0 | `sh scripts/verify-unix.sh` | Passed; native snapshot `a81577cbceab9628c925383441f07a65088d22fb5c6b3ac85652707012fb0cab`; checkout stayed unchanged throughout the run |
| Docker Linux ARM64 GNU, Rust 1.97.0 | `sh scripts/test-linux-docker.sh` | Passed; snapshot `2a999c342be72aaf6681822441b2d29f7761544351af5f7eb47bc2e70be7df59` |
| macOS portability follow-up | `cargo clippy --locked --all-targets -- -D warnings` | Passed after making the `File` import conditional for Windows; no Unix runtime behavior changed |
| Release workflow | `actionlint .github/workflows/release.yml` | Passed |

The complete Unix suites included formatting, Python pipeline contracts, Clippy,
Rust library/integration tests, real PTY interaction, preview comparison,
installer tests and an offline executable smoke test. The real-user-history
performance benchmark remained ignored by default. The native Windows Python
bootstrap test was skipped on Unix and exercised separately below.

Native evidence: `/private/tmp/codex-unified-update-checks/native-final-result.json`
and `native-final.log`. The result contains per-file hashes and the full command;
`native-portability-result.json` records the subsequent conditional-import check.

Linux evidence:
`/Volumes/File/codex-usage-monit-docker-build/runs/20260920T140113Z-arm64-15646/result.json`
and `verify.log`. This tests the actual Linux runtime; it does not exercise a
live systemd user service or the musl release artifact.

## Windows shell and bootstrap contracts

The existing ARM64 UTM guest ran as SYSTEM. Windows PowerShell
`5.1.26100.9457` ran as an AMD64 process; PowerShell `7.6.5` ran as ARM64.
Both shells passed 87 shell contracts and 12 release-bootstrap cases each.

The shell-contract command was:

```text
python3 scripts/macos/test-windows-utm.py --toolchain-home 'C:\Users\user' --shell-contracts --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' --output-dir /private/tmp/codex-update-windows-contracts
```

The exact PowerShell path, complete command, versions and source are retained in
`/private/tmp/codex-update-windows-contracts/01ae19968921462cabbca6b25224fbd8/request.json`
and `result.json`. Its source ZIP SHA256 is
`ecaf738665ed1309210767211f709b7f0ef059007a009d3933b3d072ae79bebd`.

The bootstrap run reused the original `WindowsReleaseBootstrapTests` assertions
through a host UTM transport adapter because guest Python was unavailable.
It executed both native PowerShell engines without executing fixture binaries.
Full commands and adapter:
`/private/tmp/codex-update-windows-bootstrap/bf0396f899634b1db5dc0f1cf816008e/request-with-adapter.json`.
Results and logs are in the same directory. The tested PowerShell bootstrap and
Python test files are unchanged in the later product snapshots.

An additional native transport experiment verified the encoded PowerShell
invocation with 18 shell/path combinations plus a direct baseline. Its 520-byte
payload included the protocol magic and every byte value in both directions;
output SHA256 and exit code 7 matched exactly. Paths included spaces, Chinese,
an apostrophe and the Windows canonical prefix. Evidence:
`/private/tmp/codex-update-windows-transport/d636c116aca7421f93ccb32d7d9a0929/result.json`.
A separate empty-input text fixture was invalid and is not counted as passed.

## Diagnosed local runner failures

- Linux initially failed before compilation because `rustup toolchain install`
  queried distribution metadata despite a complete pinned cache and encountered
  a TLS EOF. The runner now verifies all six exact-toolchain executables before
  reuse, and installs only if that check fails. Eleven pipeline contracts and a
  real Docker `--network none` check passed before the complete Linux rerun.
  Evidence: `/private/tmp/codex-update-linux-cache-repair/`.
- The first Windows x64 attempt lacked that target's standard library. The
  existing Rust 1.97.0 owner toolchain was supplemented, then its target list,
  actual `libstd` file and official component checksum were verified. Evidence:
  `/private/tmp/codex-utm-x64-target-repair/99e1454a77f242d3a9db583c1132f2c8/`.
- The next Windows Clippy run found an unconditional `File` import used only by
  Unix production code and tests. It is now guarded by `cfg(any(unix, test))`;
  the macOS follow-up passed. Complete Windows verification is recorded below
  when finished.
- A subsequent Windows run passed 1,682 library tests, but the remaining native
  bootstrap test could not locate the existing portable `pwsh.exe`. The runner
  now accepts its explicit path for full/focused verification and prepends it
  only to that process's PATH, preserving machine/user settings. Ten Python
  runner contracts passed. The partial native run is recorded in
  `/private/tmp/codex-unified-update-windows/dca7e7ff12df48d181faa93e7e365cfb/`;
  its source ZIP SHA256 is
  `6dcec3a57faf49056a5f4658f1b3ee92204ce4694076afabb99c1814d6eddd16`.
- Verification of the revised runner stopped before testing because UTM file
  transfer returned OSStatus -2700 and Windows reported insufficient system
  resources. A single short, read-only PowerShell resource probe could not start
  either. A normal shutdown request (`utmctl stop codex-usage-monit-windows
  --request`) left the VM running; no forced power-off or guessed process cleanup
  was performed. Evidence:
  `/private/tmp/codex-update-windows-pwsh-runner-contracts/c169589f118f406182f0ea5a83a32cdd/runner-evidence.json`
  and `/private/tmp/codex-utm-x64-target-repair/resource-9046e79afcf14fbbbe15945222642335-evidence.json`.
  The previous shell-contract results do not validate this runner change.

## Local optimized candidate

`cargo build --locked --release --bin codex-usage-monit` and
`python3 scripts/check-release-binary.py target/release/codex-usage-monit` passed.
The standard development bundle was produced with
`python3 scripts/package-release.py package target/release/codex-usage-monit
--output-dir /private/tmp/codex-unified-update-checks/local-bundle`.

The initial candidate was version `0.6.0`, protocol 5, build ID
`42a85c60f8a54c05f6054c5a81609411ded81242d01a3d5bd35b3520c7c16ff4`,
binary SHA256 `64c87b735f3997b24a8b2e8df418145e7178b64ed4f511ba39e39f3ce19aaddf`.
Evidence: `/private/tmp/codex-unified-update-checks/local-release.json` and
`local-release-build.log`. This is a locally built development candidate, not a
published release.

## Actual macOS migration

After the native full suite and candidate smoke test passed, the candidate ran:

```text
/Users/user/Workspace/codex-usage-monit/target/release/codex-usage-monit update --bundle-dir /private/tmp/codex-unified-update-checks/local-bundle --scope node --adopt --format json
```

The update returned `complete`: the existing enabled recorder became `ready`
with a fresh persisted heartbeat, and the existing manual CLI entry became
`updated` without PATH shadowing. Both report the candidate build above. The
actual `/Users/user/.local/bin/codex-usage-monit -V` at that stage printed
`codex-usage-monit 0.6.0`; `update status --format json` resolves the executable
inside the managed `versions/` directory and reports journal phase `complete`.
`service status --format json` reports the same build, `running: true`, and
`heartbeatRecent: true`.

The only changed launchd plist key was `ProgramArguments`: the executable and
its derived service-definition ID changed, while all collection, history,
remotes and project-mapping arguments were preserved. Other plist settings
were unchanged. SHA256 comparisons confirmed that `source-identity.json`, its
anchor, `remotes.json`, and `project-mappings.json` were unchanged. The prior
manual CLI backup and legacy agent directory remain retained.

Evidence under `/private/tmp/codex-unified-update-checks/`:
`local-before.json`, `local-preservation-before.json`, `local-update-result.json`,
`local-update.stdout.json`, `local-update.stderr.log`, and
`local-preservation-after.json`.

## Renumbering the unpublished candidate to 0.5.1

At the user's request, only the application version in `Cargo.toml` and its own
`Cargo.lock` package entry changed from `0.6.0` to `0.5.1`. Runtime code, schemas,
protocol 5, and dependencies are unchanged. The preflight compared these inputs
with commit `d854244951b155cad7199f0a51cca6ea361a343d`.

On macOS 15.7.2 ARM64 with Rust 1.97.0, these checks passed on the unchanged dirty
snapshot `8b70357ec05cf489c03d2ab478ea6441cbb5ab8ec5d7c70f111f18590f28e678`:

```text
cargo test --locked --lib update
cargo test --locked --lib service::upgrade
cargo test --locked --test update_cli --test agent_management
python3 -B -m unittest discover -s tests -p test_release_package.py
cargo build --locked --release --bin codex-usage-monit
python3 scripts/check-release-binary.py target/release/codex-usage-monit
python3 scripts/package-release.py package target/release/codex-usage-monit --output-dir /private/tmp/codex-unified-update-checks/local-bundle-0.5.1
```

The Rust runs passed 46, 15, and 4 tests respectively; all 6 packaging contracts
and the optimized binary's offline fixture passed. Evidence:
`/private/tmp/codex-unified-update-checks/version-051-result.json`,
`version-051.log`, and `verify-051.py`. This focused verification does not claim
another full platform run. Linux/Windows suites and hosted CI were not restarted;
the previously documented Windows verification gap remains.

The new build ID is
`a56d5bae9389f09070a94fe6f581e96cd2144249c6aa2068e12d159f9038e855`,
binary SHA256 `eda9b63bfd54d12061c7ff47d14945861325bb432e51e61d7dd9267e99d689ed`.

The actual Mac was explicitly reinstalled to this identical-code candidate.
Ordinary updates still reject numeric downgrades. After verifying the saved
enabled state and every service option, the maintenance operation backed up the
old registration, journals, launcher, and identity/configuration files privately,
staged the new immutable executable, and invoked the existing service install
and upgrade APIs. It then retired the completed CLI registration into that
backup and used the normal node updater's explicit adoption path. No version
field was forged and no general downgrade bypass was added.

The immediate service readiness call initially encountered the new recorder's
first-collection cutover lock. The attempted service restoration also stopped
at that lock before changing registration. Once a fresh heartbeat was observed,
resuming the same candidate's saved upgrade completed with the same new recorder
PID. The CLI pointer remained unchanged until the new recorder was ready.

Final verification passed: the actual CLI prints `codex-usage-monit 0.5.1`, the
recorder runs the same build with a recent heartbeat, both update journals are
complete, and the service floor is `0.5.1`. All original service settings,
source-identity/anchor, remotes, and project mappings were preserved. The old
immutable version and private maintenance backups remain retained.

Evidence under `/private/tmp/codex-unified-update-checks/`:
`renumber-local-051.py`, `renumber-051-result.json` (initial lock conflict),
`renumber-051-service-resume-result.json`, and `renumber-051-completion.json`
(successful final state). No CI, tag, or Release was started for this correction.

## Initial hosted checkpoint pause

Implementation commit `44a44b13131e6a9c855c972e8963e49515e92260` was pushed to
`codex/unified-node-updates` through HTTPS after SSH port 443 closed the
connection. The ordinary checkpoint helper stopped during repository/branch
preflight reads with GitHub API EOF errors, before reaching its dispatch call.
A temporary curl adapter preserved the helper's clean-tree, exact-SHA and
existing-run checks, but its API preflight also failed with a TLS handshake
error. No attempt reported a successful dispatch or returned a run ID.

Automatic approval review then refused another compatibility-transport attempt,
citing potential duplicate CI scheduling after repeated failures. A subsequent
read-only query for this exact commit's workflow runs also failed during TLS
setup, so no hosted result can be claimed. When asked about one checkpoint after
connectivity is restored and existing runs can be checked, the user chose to
keep the current results. Further CI attempts were paused until the subsequent
explicit request to merge, tag, publish 0.5.1 and update `ap-northeast-1`.
The temporary adapter is recorded at
`/private/tmp/codex-unified-update-checks/run-checkpoint-curl.py`; it is not part
of the product or the repository pipeline.

## Release checkpoint: Windows shell fixture correction

After the user authorized publication, the ordinary checkpoint helper completed
its exact-source and duplicate-run checks and dispatched
[CI 35519549658](https://github.com/ghostroller/codex-usage-monit/actions/runs/35519549658)
for `0a3fc4976190bcc42f7fcc03dedc827096a5bff9`. Linux, macOS and the dependency
audit passed. Windows passed all 1,683 library tests, then failed
`self_install_checks_bytes_and_can_execute_its_immutable_copy` in the
`agent_management` integration target with `The specified path is invalid.`
Later integration, ConPTY and smoke stages were not reached. Exact job evidence:
`/private/tmp/codex-release-0.5.1/ci-result.json` and
`ci-windows-job-106101152887.log`.

This was a test fixture defect. Native Windows reproduction showed two distinct
causes: passing a quoted command through ordinary Rust argument escaping breaks
`cmd /c` parsing, and bare `cmd` cannot execute the canonical `\\?\` executable
path returned by installation. The production SSH path already invokes such
paths through PowerShell. The fixture now exercises the same
`cmd -> PowerShell EncodedCommand` route, supplies the command with `raw_arg`,
and includes the stage, command and complete output in failures. Direct process
and direct PowerShell checks remain. No product source or build identity changed.
The related command-owner audit found a separate pre-existing inherited-stdout
test fixture in `src/git_repository.rs`; that follow-up is outside this release
correction and was not changed.

The UTM guest recovered and its doctor check passed without a forced reset.
Native reproduction compared ordinary, spaced and canonical executable paths:
the original invocation failed, while the production-shaped invocation passed.
Evidence: `/private/tmp/codex-release-0.5.1/windows-cmd-repro/analysis.json` and
`windows-cmd-repro/5d04fda016ad495496a6d841f5adfc96/{result.json,repro.log}`.

On macOS 15.7.2 ARM64, Rust 1.97.0, the stable dirty snapshot
`ce3018fc520e0e35aa97ca30726c254682a0e199f1b61f299943dabef35886bc`
at parent `0a3fc4976190bcc42f7fcc03dedc827096a5bff9` passed all four affected
integration tests:

```text
cargo test --locked --test agent_management --test update_cli
```

Evidence: `/private/tmp/codex-unified-update-checks/release-051-windows-fixture-mac-result.json`
and `release-051-windows-fixture-mac.log`. The source was unchanged during testing.
The full macOS/Linux suites were not repeated for this Windows-only test fix;
the first hosted checkpoint had passed both on otherwise identical source.

The native Windows focused regression passed one test on Windows ARM64 UTM,
executing target `x86_64-pc-windows-msvc` with Rust 1.97.0 and PowerShell 7.6.5.
Snapshot ZIP SHA256:
`8c1673d7da550074c0853cbe3b56add3e5202670e7d1c70185b14fedbb8a60ff`.

```text
python3 scripts/macos/test-windows-utm.py --focused --test-filter self_install_checks_bytes_and_can_execute_its_immutable_copy --target x86_64-pc-windows-msvc --toolchain-home 'C:\Users\user' --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' --output-dir /private/tmp/codex-release-0.5.1/windows-focused
```

Evidence: `/private/tmp/codex-release-0.5.1/windows-focused/a4f71e01775c40a4b220dccff4f53197/`
contains the exact request, result and verification log. Focused mode omits
format/Clippy, the full suite, ConPTY and smoke checks; the full follow-up is
recorded separately below. The failed hosted SHA must not be tagged; a new clean,
pushed commit needs its own complete checkpoint before publication.

### Complete Windows follow-up

The same corrected test snapshot (ZIP SHA256 `8c1673d7da550074c0853cbe3b56add3e5202670e7d1c70185b14fedbb8a60ff`)
passed the full native UTM pipeline under SYSTEM, with the same ARM64 guest,
Rust 1.97.0 and x64 target described above:

```text
python3 scripts/macos/test-windows-utm.py --target x86_64-pc-windows-msvc --toolchain-home 'C:\Users\user' --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' --output-dir /private/tmp/codex-release-0.5.1/windows-full
```

Format, Clippy, all 87 PowerShell 5.1 contracts, 1,683 library tests, 124
integration tests (including installation/update and ConPTY), and version/offline
JSON smoke checks passed. One existing opt-in manual benchmark was ignored;
no required pipeline stage was skipped. The run started at 15:52:48 UTC and
finished at 15:59:31 UTC on September 20. Result and full log:
`/private/tmp/codex-release-0.5.1/windows-full/ef4dccdd490f47e3ab2b7453e45ff4c8/`.
This closes the earlier UTM resource and explicit-`pwsh` runner verification gaps.
Subsequent changes before the next checkpoint are documentation only.

The official outer-wrapper contracts then passed in both PowerShell
5.1.26100.9457 (AMD64) and 7.6.5 (ARM64): 87 cases per engine, 174 total.

```text
python3 scripts/macos/test-windows-utm.py --shell-contracts --toolchain-home 'C:\Users\user' --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' --output-dir /private/tmp/codex-release-0.5.1/windows-shell-contracts
```

Run `98fc53b8abab48adad36a04f91bd673b`, ZIP SHA256
`59b8ca651d1b165a101608df980718c90a631b5a8e6c9f022e439b7246499add`,
differs from the full-suite snapshot only in `CHANGELOG.md` and the two update
evidence documents. All Rust, PowerShell, Python, shell and build inputs match.
The run directory under `windows-shell-contracts/` contains the result and log;
`/private/tmp/codex-release-0.5.1/windows-final-verification.json` records the
combined source comparison, complete commands, coverage and exclusions. No
additional project rebuild was performed in shell-contracts mode.

## Release checkpoint: deterministic process-fixture readiness

[CI 35521703978](https://github.com/ghostroller/codex-usage-monit/actions/runs/35521703978)
tested `f643eb6dbaed6309857c7f2bc9efa1c18b62ebef`. Linux, macOS and the dependency
audit passed. Windows passed 1,682 library tests, but
`bounded_process::tests::cancellation_terminates_and_reaps_the_process_tree`
failed with `ParseIntError { kind: Empty }`. Integrations, ConPTY and CLI smoke
were not reached. Evidence: `/private/tmp/codex-release-0.5.1/final-ci-result.json`
and `final-ci-windows-job-106106796411.log`. This was diagnosed locally before
any further hosted request.

The fixture used file existence as readiness, although `fs::write` first creates
an empty file. Cancellation could kill the writer before its PID was recorded.
A native Windows x64 experiment explicitly paused at that boundary: the old
implementation exposed an empty ready marker and failed deterministically; the
new implementation kept the marker unpublished until the complete PID was
written and the file closed. Evidence:
`/private/tmp/codex-release-0.5.1/windows-pid-repro/346b549d514946498476ba12ea78a9bd/repro.log`.

Only test modules changed. The shared process fixture now publishes PID files by
same-directory rename after writing and closing them, with a deterministic
before-write regression and useful failure diagnostics. The related-owner audit
found two Unix SSH cancellation fixtures that also cancelled on file existence;
they now wait for a complete newline-terminated positive PID, with regressions
for empty, partial, invalid and expired-readiness cases. Even readiness failure
triggers process cleanup before the test reports the failure. Other service,
CLI, TUI, lock and completed-output consumers already had suitable ordering.

The first Docker follow-up reproduced the audited timeout-startup assumption:
`timeout_terminates_the_fake_ssh_process_group` could reach its 100 ms deadline
before a descendant existed, then incorrectly require its PID file. That run
passed 1,775 library tests and failed this one, before integration/smoke stages.
Evidence: `/Volumes/File/codex-usage-monit-docker-build/runs/20260920T162545Z-arm64-51544/`;
snapshot `5e2a12952b969c8957f9805a8a37e6f269e3d93b07c776fc6fe1ca0f1c27640d`.

Both related process-tree timeout tests now establish fixture readiness before
exercising an explicitly expired deadline, and measure cleanup separately from
startup. The SSH test calls the production wait and cleanup functions; a
separate high-level probe retains timeout-error classification coverage without
requiring a descendant to start within 100 ms. No production timer, interface,
process-containment behavior or updater policy changed. Production prefixes of
both edited modules were compared byte-for-byte with the parent commit.

### macOS follow-up and final candidate

On macOS 15.7.2 ARM64 / Rust 1.97.0, snapshot
`90816a681eee2af7736d558a393f3fdf58d080d5ff229b1862a2944c1fc0bf51`
passed both affected modules, then the complete `sh scripts/verify-unix.sh`
pipeline: 1,779 library tests, all integrations/PTY, format/Clippy, script
contracts, preview, installer and CLI smoke. The existing manual benchmark and
Windows-only Python case were not run on macOS. Evidence:
`/private/tmp/codex-unified-update-checks/release-051-pid-fixture-mac-result.json`
and its adjacent log.

After removing the two timeout-startup assumptions, final snapshot
`4e07e567f82c6f58e848abc328d6b929d1f0be8aeef2a216d00e03569aa8a20a`
passed these commands on the unchanged checkout:

```text
cargo test --locked --lib bounded_process::tests
cargo test --locked --lib remote_transport::tests
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release --bin codex-usage-monit
python3 scripts/check-release-binary.py target/release/codex-usage-monit
python3 scripts/package-release.py package target/release/codex-usage-monit --output-dir /private/tmp/codex-release-0.5.1/final-timing-bundle
```

The affected modules passed 8 and 35 tests. This was a focused follow-up to the
successful full macOS run, not another full run. Evidence:
`/private/tmp/codex-unified-update-checks/release-051-final-timing-mac-result.json`
and its adjacent log; the complete command sequence is preserved in
`/private/tmp/codex-release-0.5.1/verify-final-timing-mac.py`.

Because the source identity includes test modules under `src/`, the final
candidate's build ID is
`f89ac3620545f89b566124675c581b74d4ee8741a0822bd20f9c0f08dd5fdb47`;
its macOS binary SHA256 is
`69c72709892620dc40b32303c29e4d1df06b8cfd62ded2e655f85031be98d3e1`.
Version remains 0.5.1 and protocol remains 5. The explicit development bundle
path is required to replace an earlier unpublished same-version source build;
ordinary official updates retain their same-version conflict guard.

The final candidate then completed the normal local node updater:

```text
target/release/codex-usage-monit update --bundle-dir /private/tmp/codex-release-0.5.1/final-timing-bundle --scope node --format json
```

The managed CLI and enabled recorder now use the final build above. The updater
reported `complete`, recorder `ready`, CLI `updated`, and a new persisted
heartbeat. The actual shell entry prints `codex-usage-monit 0.5.1` and resolves
the final immutable executable. Source identity, identity anchor, remotes and
project mappings retained their exact hashes; all original service options and
non-argument launchd settings were preserved. Evidence:
`/private/tmp/codex-release-0.5.1/local-final-update-result.json`,
`local-final-update.stdout`, `local-final-before.json` and
`local-final-verification.json`.

### Final Linux follow-up

`sh scripts/test-linux-docker.sh` passed on Linux ARM64 GNU / Rust 1.97.0,
parent `f643eb6dbaed6309857c7f2bc9efa1c18b62ebef`, dirty isolated snapshot
`fc73b5bed35be996b3e28df0597744f74e5de747de6533d7f175d6a370fe7541`.
All 1,777 library tests and the integration/PTY suites passed, along with
format/Clippy, script contracts, preview, installer and CLI smoke checks. The
existing manual benchmark and Windows-only Python case were excluded; this
native-architecture run does not claim Linux x64 coverage. Exact result and log:
`/Volumes/File/codex-usage-monit-docker-build/runs/20260920T163225Z-arm64-60661/`.

### Final Windows follow-up

The final Windows ARM64 UTM guest, running the x64 target under SYSTEM with
Rust 1.97.0, passed the complete pipeline:

```text
python3 scripts/macos/test-windows-utm.py --target x86_64-pc-windows-msvc --toolchain-home 'C:\Users\user' --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' --output-dir /private/tmp/codex-release-0.5.1/windows-pid-full
```

Run `d1eb61d24af84a9e9e6efd8d1900b222`, parent
`f643eb6dbaed6309857c7f2bc9efa1c18b62ebef`, dirty ZIP snapshot
`da869c5d282fb84f5b4ebb5cb8ba40e056485320bcd4215101971d2b4abdcf74`,
passed 1,684 library and 124 integration tests (1,808 total), including both
ConPTY tests, all 87 PowerShell 5.1 contracts, format, Clippy and CLI smoke.
Only the existing manual benchmark was ignored. Start/end:
2026-09-20 16:32:26–16:41:09 UTC. All 150 Rust, Python, PowerShell, shell and
Cargo inputs were compared with the final checkout and match byte-for-byte.
No required stage was skipped. The unchanged PowerShell 7 outer-wrapper evidence
from the prior 174-case dual-engine run remains applicable and was not repeated.

Exact result and full log:
`/private/tmp/codex-release-0.5.1/windows-pid-full/d1eb61d24af84a9e9e6efd8d1900b222/`.
The combined native reproduction, focused pass, final full pass, commands and
source comparison are recorded in
`/private/tmp/codex-release-0.5.1/windows-pid-verification.json`.
Only documentation was updated after this final set of platform checks. The
next hosted checkpoint must test the resulting new commit; neither earlier
failed run can authorize its release tag.

Old release assets and unknown legacy agent references remain retained. Grouping
existing configuration/history into new subdirectories is a separate migration;
this executable update preserves their current locations and source identity.
