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

## Remaining verification

- The complete Windows x64 Rust/ConPTY suite and revised runner's native checks
  remain blocked on the ARM64 UTM guest by the resource failure above. A single
  hosted native Windows checkpoint will cover the product suite; it does not
  substitute for the UTM-specific runner check.
- One hosted integration checkpoint for the final committed source, recording
  the exact run ID and head SHA.

## Hosted checkpoint status

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
keep the current results. No further CI dispatch attempts are authorized in this
batch; a later checkpoint must first confirm the intended source and existing runs.
The temporary adapter is recorded at
`/private/tmp/codex-unified-update-checks/run-checkpoint-curl.py`; it is not part
of the product or the repository pipeline.

Old release assets and unknown legacy agent references remain retained. Grouping
existing configuration/history into new subdirectories is a separate migration;
this executable update preserves their current locations and source identity.
