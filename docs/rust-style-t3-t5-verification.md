# T3–T5 implementation and local verification

T0–T2 were committed as `a588e3f694c90dbed05de8053a98be1862dcc130`
before this batch. This follows the scoped migration in [the Rust
policy](rust-style.md). No GitHub CI, push, tag, or publication was requested or
run.

## Scope

- T3: replace internal positional collection flags with `SnapshotCollectionPolicy`
  variants that carry the required cache and account-refresh policy. Keep all
  external collection signatures. Replace the four-part project-group tuple
  with named fields in the same comparison order, preserving grouping and output.
- T4: document configuration and event-log API contracts and enforce
  `missing_docs` in those two audited modules. Document underlying diagnostic
  file I/O, use explicit production imports in SCM, service upgrade and release
  code, and mark three builder methods returning modified `Self` as `must_use`.
  Simplify cached projection access to one `as_ref()?`.
- T5: extract terminal input, OSC52, termination signals, mouse capture and
  restoration into `tui::terminal`. Preserve TUI entry points, caller-owned
  resource lifetimes and drop ordering. This completes the planned first bounded
  extraction; further view/state decomposition remains separate work.

Two new behavior regressions use real rollout files and history observations:
`collection_policies_preserve_scan_boundaries_and_pending_changes` and
`project_groups_order_missing_and_shared_turn_ids_without_merging_threads`.
They cover scan/coverage boundaries, deferred changes surviving mode switches,
optional turn ordering, duplicate aggregation and cross-thread isolation.
Existing terminal tests retain their original `tui::tests::*` paths and shared
Windows synchronization lock; they were not renamed or duplicated.

## Environment and evidence

Verification follows `.agent/environment.local.md` and [testing.md](testing.md):
native Windows x64, `x86_64-pc-windows-msvc`, rustc 1.97.0 and PowerShell 7.4.1,
under `DESKTOP-U23KNI9\Ghost`. The native entry point checks permissions in
`C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023`.
Four test threads bound concurrent filesystem pressure without changing test
timeouts or assertions.

Local ignored evidence is in
`D:\Workspace\codex-usage-monit\target\rust-style-t3-t5`.
`source_manifest.py` hashes raw bytes of source, tests, scripts, `.cargo`, Cargo
manifests/lockfile, the toolchain and build script, plus embedded `CHANGELOG.md`
and `docs/model-catalog.example.json`. The snapshot identity hashes the sorted
UTF-8 manifest with LF line endings. Verification reports and policy Markdown
are not build inputs and are excluded.

The saved `verify-local.ps1` uses the existing Cargo target/build directories in
`target\rust-style-2026-10-23`, without switching to the older source copy.
Git's safe-directory override applies only to the test process; global Git
configuration is unchanged.

```powershell
cargo fmt --all
pwsh -NoProfile -File target/rust-style-t3-t5/verify-local.ps1 -Stage Focused
pwsh -NoProfile -File target/rust-style-t3-t5/verify-local.ps1 -Stage Full
```

The focused stage invokes the Windows verifier with `-TestFilter
'snapshot::tests' -SkipSmoke`, including fmt and all-target Clippy with
`-D warnings`. It then runs:

```powershell
cargo test --locked --lib -- project_groups_ event_log::tests config::tests remote_sync_scheduler::tests button_mouse_capture_ termination_request_ terminal_fd_poll_ windows_close_handler_ osc52_clipboard_ --test-threads=4
$env:RUSTDOCFLAGS = '-D rustdoc::broken_intra_doc_links'
cargo doc --locked --no-deps
```

The full stage uses the same native verifier with `-SkipTests` to prepare MSVC
and run fmt, Clippy and CLI smoke, then repeats the Rustdoc link check after the
final comment edits and runs
`cargo test --locked --all-targets --no-fail-fast -- --test-threads=4`.
`--no-fail-fast` preserves integration/ConPTY execution after a failing library
target. A nonzero result is retained rather than interpreted as success.

## Results

The focused source identity is
`51c561f39df13632f39503d59808b802c4c4fc7a2ad27e2aab0f6b08f09a355f`;
pre/post-run manifests match. The final source identity, after four contract
comment corrections, is
`e884b7df480bd9fb8be72f166d3b51171a1f527a2fe0208f4a9c7c1df1683665`.
The full run's pre/post manifests match; both new regressions passed.

| Check | Result | Log under evidence root |
| --- | --- | --- |
| Focused fmt, all-target Clippy and snapshot tests | Passed; 30 tests | `focused-entrypoint.log` |
| Related grouping, log, config, scheduler and Windows terminal tests | Passed; 81 tests | `focused-modules.log` |
| Initial Rustdoc link check | Passed | `rustdoc.log` |
| Final fmt, all-target Clippy and CLI version/offline JSON smoke | Passed | `final-checks-smoke.log` |
| Final Rustdoc link check | Passed | `final-rustdoc.log` |
| Full Rust all-target run | 2,046 passed, 4 failed, 4 ignored | `all-targets.log` |
| Isolated, serial current ConPTY binary | 1 passed, 1 initial-snapshot timeout | `conpty-isolated.log` |
| Recompiled `a588e3f` baseline, serial ConPTY | Same 1 pass and 1 timeout | `conpty-Baseline.log` |
| Recompiled current source after baseline comparison, serial ConPTY | Same 1 pass and 1 timeout | `conpty-Current.log` |

The initial Clippy attempt found one `std::env` import previously supplied by
the removed glob in `service/upgrade.rs`. It was added explicitly before tests;
the failed attempt is retained as `initial-missing-import.log` and its manifest.

The full run is **not green**; Cargo returned 101. The library target has 1,919
passes, three failures and one ignored test. Integration targets have 127 passes,
one failure and three ignored tests. The ignored tests are the two opt-in
synthetic measurements, real-user-history benchmark and previous-build launcher
comparison; their coverage is not claimed. `result.json` records per-target
counts and source identities.

The three library failures are the same Windows CAS classification failures
reproduced against original production code in the T0–T2 report. All still
observe `Uncategorized` instead of `WouldBlock`:

- `project_mapping::tests::prepared_batch_file_identity_cas_rejects_a_concurrent_mapping_change`
- `project_mapping::tests::unchanged_prepared_batch_still_rejects_a_concurrent_mapping_change`
- `remote_sync::tests::unchanged_descriptor_batch_cas_failure_cannot_commit_history`

The additional integration failure is
`real_tui_pty_handles_keyboard_mouse_search_resize_and_exit`: its existing
eight-second wait expires before the initial fixture row appears. The screen
has rendered and displays `Finalizing usage snapshot...`. Serial execution of
the same frozen binary reproduced it, so concurrency alone does not explain it.
The other native ConPTY test, remote setup diagnostics, passed in both runs.

To distinguish a T3–T5 regression from an existing startup-budget problem, an
independent source snapshot of committed `a588e3f` was compiled and tested under
the same account, temporary directory and serial test settings. It reproduced
the identical timeout. Its archive is `baseline-a588e3f.zip`, SHA-256
`69f8f88d643974f731d4ff7c5bd3a7589ed39f6bfc21a57f9e27266ec00a7986`.
This rules out the timeout being newly introduced by this batch, but does not
resolve the underlying startup latency or establish a passing ConPTY suite.
The [earlier startup investigation](conpty-startup-timeout-analysis-2026-10-03.zh-CN.md)
also explains why the displayed stage can persist during later history I/O;
the screen text alone cannot identify the slow operation in this run.

```powershell
# Isolated current binary, with the same private TEMP/TMP and process-local Git override:
& target/rust-style-2026-10-23/red-build/debug/deps/tui_pty-a29b6fa8597066b0.exe --test-threads=1
git -c safe.directory=D:/Workspace/codex-usage-monit archive --format=zip --output=target/rust-style-t3-t5/baseline-a588e3f.zip a588e3f
Expand-Archive -LiteralPath target/rust-style-t3-t5/baseline-a588e3f.zip -DestinationPath target/rust-style-t3-t5/baseline-source
pwsh -NoProfile -File target/rust-style-t3-t5/verify-conpty.ps1 -Source Baseline
pwsh -NoProfile -File target/rust-style-t3-t5/verify-conpty.ps1 -Source Current
```

`verify-conpty.ps1` imports MSVC through the native verifier with all check stages
skipped, sets the same Cargo/TEMP overrides, and runs
`cargo test --locked --test tui_pty -- --test-threads=1` from the selected source
root. It refreshes only `src/lib.rs`'s timestamp to force project recompilation
when switching roots in the shared cache. Source bytes do not change, and each
log names the actual compiled source path. The current-source run restores the
current CLI and integration-test artifacts after the baseline comparison.
No timeout, assertion, production TTL or test expectation was weakened.

Linux/macOS runtime suites are not claimed. This machine's configured native
environment is Windows. Platform-conditioned imports were reviewed, but that
does not replace native Unix execution. Script-only suites are unchanged by this
Rust batch and are not repeated; their previous evidence remains in the
[T0–T2 report](rust-style-verification-2026-10-23.md). No live service was installed.
