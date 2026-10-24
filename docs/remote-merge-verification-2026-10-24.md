# Remote session-management integration verification

The previously verified Windows CAS and startup fixes were committed as
`cd73cc751eb48cfc00adb5858700e333528b03b8`. The remote `main` was then fetched
at `4a8f3c981b684e84ea947ca272594a798aaf3a04` and merged without rebasing the
local T0–T5 or regression-fix commits. The common ancestor is `22087b1`.
The two incoming commits retain usage for deleted sessions and perform explicit
deletion through the desktop's existing App Server.

No push, tag, release operation or GitHub CI dispatch was performed.

## Conflict resolution and integration coverage

Git reported conflicts in `src/config.rs` and `src/tui.rs`. The configuration
retains the audited API documentation and adds the independent deleted-session
ledger. Its `None` directory setting selects the default user-data directory;
it does not disable the ledger.

The refresh scheduler retains local permanent-stop and poison handling while
respecting deletion dialogs and queued operations. Post-deletion cache
invalidation rejects a poisoned mutex. All three new reconciliation cache-lock
sites also reject poison, and both refresh completion paths discard their
unpublished results when cache or history state is interrupted. Ordinary ledger
errors remain recoverable load errors.

Stopped sessions cannot request, confirm or start deletion. A late preparation
result cannot authorize deletion, while an already executing operation retains
its result receiver. Success, failure and disconnection are reported without
automatic retry or a misleading promise to refresh a stopped session. Disabled
controls keep their layout without advertising an active shortcut.

Ten added regressions cover actual held-lock panics before and during
reconciliation; all reconciliation callback outcomes; ordinary ledger errors;
forced post-deletion refresh with either poisoned owner, with and without a
dialog; stopped keyboard/mouse/compact controls; queued and late preparation;
executing result handling; ledger-only changes through coalescing; and
limits-only collection with an unavailable ledger.

The automatic merge preserves the local Windows CAS identity checks, SQL
snapshot and external revision fences, native attribute fallbacks, and terminal
resource extraction. The incoming database helper visibility change is retained
for the new ledger's SQLite handle validation.

## Local environment and commands

Verification follows `.agent/environment.local.md`: native Windows x64,
`x86_64-pc-windows-msvc`, rustc 1.97.0, PowerShell 7.4.1, running as the native
account with private TEMP
`C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023`.
Git safe-directory configuration is process-local.

Evidence and the complete wrapper are in
`D:\Workspace\codex-usage-monit\target\remote-merge-2026-10-24`.
The wrapper initializes MSVC with `scripts/windows/verify.ps1`, using:

```text
-RepositoryPath D:\Workspace\codex-usage-monit
-CargoTargetDir D:\Workspace\codex-usage-monit\target\rust-style-2026-10-23\red-target
-CargoBuildDir D:\Workspace\codex-usage-monit\target\rust-style-2026-10-23\red-build
-TestTempDir C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023
```

The focused stage uses `-SkipTests` for format, Clippy, build and CLI smoke,
then runs the following affected tests and ConPTY target. The full stage uses
`-ScriptContractsOnly` followed by the complete Rust suite.

```powershell
cargo fmt --all
pwsh -NoProfile -File target/remote-merge-2026-10-24/verify-local.ps1 -Stage Focused
pwsh -NoProfile -File target/remote-merge-2026-10-24/verify-local.ps1 -Stage Full
```

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --lib -- poisoned_refresh session_management deleted_sessions limits_only_collection shared_session session_delete prepared_batch unchanged_descriptor_batch projection_revision history_query::tests::request_context read_if_exists_facade windows_bound_ windows_handle_attribute_probe_ windows_job_resumes_child_and_terminates_inherited_grandchild --test-threads=4
cargo test --locked --test tui_pty -- --test-threads=1
cargo test --locked --all-targets --no-fail-fast -- --test-threads=4
```

The manifest hashes raw build-input bytes using the scope documented in the
[previous verification report](windows-regression-fixes-2026-10-24.md).
Verification Markdown is outside that scope. Each run tests the dirty merge
snapshot over `cd73cc7`; the final merge commit is made after verification.

## Results

The corrected merge snapshot is
`3616e5556b7a9a96fa7d2c567bd8162c1d458e2d57cbc58788b2a26580d6ed16`.
Formatting, all-target Clippy with `-D warnings`, the Windows build, version
output and offline JSON smoke checks pass (`focused-checks-smoke.log`).
The affected library run passes all 100 tests, with no failures or ignored
tests, in 38.32 seconds (`focused-library.log`). This includes all ten new
cross-feature regressions. Focused ConPTY reports one passed and one failed:
`real_tui_pty_handles_keyboard_mouse_search_resize_and_exit` exceeded its
unchanged eight-second initial-data deadline (`focused-conpty.log`, 10.60 seconds
for the complete target).

The complete native run finished all 13 Cargo targets: **2138 passed, 1 failed,
4 ignored**, Cargo exit code 1 (`all-targets.log`). The library target passed
2011 tests with one existing ignored test in 1017.13 seconds. The sole failure
was again the ConPTY initial-data deadline; its other test passed (8.12 seconds
for that target under four-thread execution). All other targets passed. The
four ignored cases are the existing synthetic-history fixture and measurement,
the opt-in real-user-history benchmark, and the portable-launcher test requiring
a trusted prior-build binary. No test was newly ignored.

`full-source.sha256` and `full-source-after.sha256` are byte-identical and match
the corrected snapshot hash above. `git diff --check` and the report's relative
link check pass. The merge is verified with this explicitly recorded timing
failure; it is **not** a fully green Windows checkpoint. The pre-existing
startup timing instability remains unresolved by this conflict-resolution batch.

Windows script contracts pass (`full-script-contracts.log`): native verification,
development and permission-repair wrappers, plus 31 Python contract tests across
remote bootstrap, Windows installation/setup and release packaging. Packaging
contracts operate on synthetic local fixtures; no release was published.

Both `focused-source.sha256` and `focused-source-after.sha256` have the corrected
snapshot hash above. No source was changed in response to the timing failure.

A controlled ConPTY observer retained the real harness, fixture and original
deadline, then measured readiness beyond the deadline for diagnosis. The merged
binary became ready at 10.354 seconds and exited normally after `q` in 0.554
seconds. Its trace measured 3.242 seconds before the initial scan, 6.6 ms for
rollout scanning, 4.020 seconds for history recording and 1.910 seconds for
history loading. The scan includes two of the three new missing-ledger probes;
it is not evidence of a seconds-long ledger regression.

The previously verified, frozen pre-merge binary was then observed with the same
current harness and explicit logging. Its SHA-256 is
`85c7ee82e8fc47324303972aceaaa96a473f368d234780b8a1d5fe1658d5a1bd`,
matching the previous report's final observer executable. It also exceeded the
deadline: ready at 8.240 seconds, normal exit in 0.591 seconds. Both processes
performed 363 read operations; other I/O operations were 318,866 for the merged
binary and 318,662 for the pre-merge binary. This establishes that the timing
failure also occurs without the incoming changes under the current conditions;
it does not prove the timing variation's complete cause or turn either failed
observation into a pass. No timeout, assertion, permission fence or durable
transaction was weakened.

The observer logs are `observer-20261024T095907163224Z` and
`observer-pre-merge-baseline`, including frozen executable/harness hashes,
commands, retained synthetic fixtures and process samples. The local observer
explicitly selects this locked build's `tempfile` artifact because the reusable
dependency directory contains the previous build as well. It does not select
artifacts by modification time.

```powershell
python target/remote-merge-2026-10-24/observe.py --expected-source-commit cd73cc7 --source-manifest target/remote-merge-2026-10-24/focused-source.sha256
python target/remote-merge-2026-10-24/observe.py --expected-source-commit cd73cc7 --binary target/test-failures-2026-10-24/observer-20261023T175446945816Z/current-cli.exe --out target/remote-merge-2026-10-24/observer-pre-merge-baseline --logging explicit
```

The first compile attempt (`ce7e13ab6be4dc6cc61867f2dc0d2d897d3288f1f7541ed9063d436b0182721f`,
unchanged before/after) found a namespace collision in the added scheduler
regression: a sibling test module shadowed the production session-management
module. Explicit imports fixed the test references. `compile-error.log` records
this failed attempt; it is not test-pass evidence.

## Coverage limits and separate review item

Linux and macOS runtime suites were not run on this native Windows host. In
particular, `session_delete::protocol_tests` is gated to Unix; Windows state
machine and mocked client tests do not establish that complete socket-protocol
coverage or a live desktop deletion. No real user session was deleted.

Read-only integration review also identified a separate property of the incoming
ledger: its Windows SQLite side-file checks use the shared layout path/type
checks, rather than the retained-handle ACL and hard-link checks used by the
history database. The main ledger database retains its live ACL, identity and
SQLite-handle checks. Aligning side-file validation and adding weak-ACL/hard-link
regressions remains a separate review item; this integration does not claim that
additional coverage or change those incoming semantics.
