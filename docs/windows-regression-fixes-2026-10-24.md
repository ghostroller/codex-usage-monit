# Windows CAS and ConPTY regression verification

T3–T5 were committed first as `ea9745c` (`refactor: name collection policies
and isolate TUI terminal lifecycle`). This follow-up addresses the four failures
recorded in [the T3–T5 verification report](rust-style-t3-t5-verification.md).
No GitHub CI, push, tag or publication was run.

## Diagnosis

The two prepared mapping CAS tests and
`remote_sync::tests::unchanged_descriptor_batch_cas_failure_cannot_commit_history`
expected `WouldBlock` after another writer replaced the prepared mapping file.
On this Windows host, querying the retained, unlinked file's ACL failed with
Windows error 1168 before the identity comparison ran. A direct diagnostic
confirmed both the ACL error and the distinct file identities. Arbitrary I/O
errors must not be reclassified as concurrency conflicts.

The ConPTY keyboard/mouse/search/resize/exit test missed its existing eight-second
initial-data deadline. An isolated observer retained the real PTY harness and
fixture predicate, then continued measuring after the deadline. Initial data
appeared at 11.437 seconds: rollout scanning took 2.7 ms, runtime activation about
3.15 seconds, history recording 4.722 seconds, and history loading 2.403 seconds.
The observation failed the original deadline; it was not a passing test.

Two sources of redundant work were identified:

- A projection revision read the same SQL inputs twice within one stable read
  snapshot. A deterministic regression measured 13 read calls and one connection,
  exceeding the eight-call budget for one SQL sweep and both receipt fences.
- Windows ancestor checks repeatedly requested full filesystem metadata while
  using only the reparse attribute. A native attribute query can answer that
  predicate without changing the number or placement of ancestor checks.

## Changes and regression coverage

- Prepared mapping publication checks identity before querying the retained ACL
  and again afterwards. Only a proven identity mismatch becomes `WouldBlock`.
  Regressions cover changed and unchanged batches, unsafe directory replacement,
  weakened permissions on the same file, concurrent replacement, preservation
  of the concurrent mapping, temporary-file cleanup, and lock release.
- Projection revision reads SQL inputs once within its existing short snapshot.
  Ownership and mapping inputs are still read before and after; each ownership
  manifest still validates its receipt. Regressions cover the read budget,
  concurrent SQL writes, mapping changes, epoch/receipt mismatch, and absence of
  database creation. Test-only measurement is isolated by database and thread
  and restores its prior scope after unwinding.
- Ancestor checks use `GetFileAttributesW` with the existing Windows path encoder;
  failed attribute queries retain the former metadata fallback. This matters for
  bare UNC share roots. The API reports the link/mounted-folder object's own
  attributes ([Microsoft contract](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfileattributesw)).
  Handle ACL checks, file identity checks, link counts, and before/after ancestor
  walks remain in place. Regressions cover deep Unicode/long/verbatim paths,
  missing and invalid paths, junctions, and missing descendants beneath junctions.
  The two private-file/directory validators likewise obtain only live handle
  attributes through `GetFileInformationByHandleEx(FileAttributeTagInfo)`, falling
  back to the same handle's full metadata on failure
  ([Microsoft contract](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getfileinformationbyhandleex)).
  Regressions check changed attributes on held files/directories, a real rejected
  native query followed by fallback, and a nofollow junction handle.
- Activation checks and initializes receipts in one write transaction. Both
  privacy ownership leases, retained-state rollback, activation fences and final
  receipt validation remain. GC reads metadata, progress and the retention clock
  in one preparation snapshot, released before scanning and the final publication
  transaction. Deterministic budgets cover one activation write connection and
  two first-GC read connections, alongside retention and restart/resume behavior.
- Optional SQL reads can reuse an existing snapshot's existence guarantee only
  when the original and canonical root/path bindings match exactly. The read
  still validates the opened database, permissions and side files. Missing or
  noncanonical paths keep their prior existence checks; revision reservation,
  source locking and query retry transaction boundaries remain unchanged.
- History queries read their SQL inputs once per attempt, while checking external
  ownership and project mappings before and after. Concurrent publication remains
  invisible to the current SQL snapshot; a changed mapping triggers a new snapshot
  and observes that publication. The local-query regression budgets 12 reads
  rather than the previous 16.
- Local observation publication skips an empty prepared digest family. An empty
  incoming digest list can still produce reconciliation tombstones, which remain
  published. Revision reservation and data publication remain separate commits.

The original ConPTY timeout and interaction assertions are unchanged.

## Local environment and commands

Following `.agent/environment.local.md`, verification uses native Windows x64,
`x86_64-pc-windows-msvc`, rustc 1.97.0, PowerShell 7.4.1, and the private TEMP
directory `C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023`.
Tests run as the native account because sandbox identities alter Windows ACL
results. Four test threads bound concurrent filesystem pressure. Focused ConPTY
runs serially; the full all-targets run uses four threads for every target,
including ConPTY. Linux and macOS suites are outside this native Windows verification
batch and are not claimed as verified. A real UNC share was not exercised.

Ignored local evidence is in
`D:\Workspace\codex-usage-monit\target\test-failures-2026-10-24`.
The saved `verify-local.ps1` imports the MSVC environment using the repository's
Windows verifier and reuses `target\rust-style-2026-10-23\red-target` and
`red-build`. Git safe-directory configuration is process-local.

```powershell
pwsh -NoProfile -File target/test-failures-2026-10-24/verify-local.ps1 -Stage RedRevision
pwsh -NoProfile -File target/test-failures-2026-10-24/verify-local.ps1 -Stage RedAttributes
pwsh -NoProfile -File target/test-failures-2026-10-24/verify-local.ps1 -Stage RedBatch
pwsh -NoProfile -File target/test-failures-2026-10-24/verify-local.ps1 -Stage RedQuery
cargo fmt --all
pwsh -NoProfile -File target/test-failures-2026-10-24/verify-local.ps1 -Stage Focused
python target/test-failures-2026-10-24/observe.py --source-manifest target/test-failures-2026-10-24/final-source.sha256
python target/test-failures-2026-10-24/observe.py --logging none
pwsh -NoProfile -File target/test-failures-2026-10-24/verify-local.ps1 -Stage Full
```

The focused stage runs format, all-target Clippy with `-D warnings`, CLI smoke,
affected library tests, and the unmodified `tui_pty` integration target. The full
stage runs Windows script/installer contracts and
`cargo test --locked --all-targets --no-fail-fast -- --test-threads=4`.
The driver contains the complete filters and environment settings.

The final Cargo commands are:

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --lib -- prepared_batch unchanged_descriptor_batch projection_revision v2_projection_cache windows_reparse windows_path_attributes windows_private source_history::database:: sqlite_history_initialization::tests source_history::sqlite_gc::tests sqlite_gc_ failed_gc_scan history_query::tests::request_context source_history::local_observation::tests::sqlite_ windows_handle_attribute --test-threads=4
cargo test --locked --test tui_pty -- --test-threads=1
cargo test --locked --all-targets --no-fail-fast -- --test-threads=4
```

The Windows entry point was also invoked with `-SkipTests` for lint/build/CLI
smoke and with `-ScriptContractsOnly` for the full checkpoint's script contracts.
Both used `-RepositoryPath D:\Workspace\codex-usage-monit`,
`-CargoTargetDir D:\Workspace\codex-usage-monit\target\rust-style-2026-10-23\red-target`,
`-CargoBuildDir D:\Workspace\codex-usage-monit\target\rust-style-2026-10-23\red-build`,
and `-TestTempDir C:\Users\Ghost\AppData\Local\codex-usage-monit\test-temp-style-20261023`.

## Evidence

The source manifest uses the same raw-byte hashing and build-input scope as the
T3–T5 report. Policy and verification Markdown are excluded from build inputs.

| Stage | Source identity | Result and log |
| --- | --- | --- |
| Initial CAS diagnosis | `ea9745c` plus diagnostic assertions | Two original CAS failures report Windows error 1168; `red-cas.log` |
| Revision and CAS red tests | `bbe3750df430ee59103bbae87f64fb4b52bbb5902b188b599a7e63eedc9e4b4a` before and after | 6 passed, 3 failed: the read budget and two original CAS cases; `red-revision-cas.log` |
| Attribute red tests and CAS/revision fixes | `f0c469d3e938a9577d1fcab4a586552c1bdd1ca966124b9c2c500349c4f17278` before and after | 20 passed, 1 failed: 14 full metadata probes instead of zero. All three original CAS failures and the revision budget passed; `red-attributes.log` |
| Baseline ConPTY observer | Frozen T3–T5 CLI and harness hashes in `execution.json` | Original deadline failed; `observer-20261023T165017843643Z` |
| First fix batch | `cbf25fd5740a21dabb043fa3738bc19e240c3191f5220f71b4081ac5dff52977` before and after | fmt, Clippy, CLI smoke and 46 focused library tests passed; ConPTY remained 1 passed / 1 failed; `round1-*.log` |
| First fix observer | Same `cbf25fd5` snapshot, frozen source/CLI/harness hashes | Initial data at 8.660 seconds, still outside the original deadline; activation 2.542 s, record 3.420 s, load 1.743 s; `observer-20261023T171118238103Z` |
| Connection/existence budget red tests | `038340db6d0085fbc302ee6784ba614dfeccee5f3560f399bc06ad06c311528d` before and after | 5 passed, 3 failed as expected: activation 2 write opens versus 1, first GC 4 read opens versus 2, revision 5 redundant existence checks versus 0; `red-batch.log` |
| Second fix batch | `8b6b38be4c23585094db73ae39c4a1170d6673e6c5f0d75f47f0a52a7416a795` before and after | fmt, Clippy, CLI smoke and 67 focused library tests passed; ConPTY remained 1 passed / 1 failed, including one isolated follow-up; `round2-*.log` |
| Second fix observers | Same frozen CLI, individual CLI/harness hashes in each `execution.json` | Explicit log path: 6.728 s (`observer-20261023T173329630489Z`); default event log with timing instrumentation: 7.953 s (`observer-20261023T173630943460Z`); original CLI arguments without timing logs: 10.744 s, deadline failed (`observer-20261023T173705984440Z`). Diagnostic variability is not a passing integration result. |
| Query/digest/handle-attribute red tests | `afb94614d95b3bdc40c430582155008536666121c7887c36c0dbb8d973d33d27` before and after | 2 behavior tests passed, 3 budget tests failed as expected: 16 query reads versus 12, 7 empty-digest observation reads versus 6, and 7 full handle metadata probes versus zero; `red-query.log` |
| Third fix batch | `12550bc315e0f53b8c07f7df950bc5be3e587eb3f07fbaffa53f542d2f91eacb` before and after | fmt, Clippy, CLI smoke and 85 focused library tests passed; both original ConPTY tests passed (8.49 s for the whole target); `round3-*.log` |
| Third fix observer, original CLI arguments | Same `12550bc3` snapshot; frozen source/CLI/harness hashes | Data ready at 6.556 s, within the unchanged 8 s deadline; normal exit after `q` in 0.469 s. No timing/event-log overrides; `observer-20261023T175446945816Z`. |
| Full native Windows checkpoint | `12550bc315e0f53b8c07f7df950bc5be3e587eb3f07fbaffa53f542d2f91eacb` before and after | All 13 Cargo targets completed: 2070 passed, 0 failed, 4 ignored. Library: 1942 passed / 1 ignored; ConPTY: 2 passed, 7.27 s for the whole target under four-thread execution. `all-targets.log`; Cargo exit code 0. |
| Windows script contracts | Same frozen source | 31 Python contract tests passed, exercising the documented native shell wrappers; `full-script-contracts.log`. |

The four original failures pass in both the focused and full native runs.
Formatting, all-target Clippy (`-D warnings`), CLI smoke, script contracts and
`git diff --check` pass. `full-source.sha256` and `full-source-after.sha256`
are byte-identical. These results describe the fix snapshot verified before its
follow-up commit after the requested T3–T5 commit.

The four existing ignored tests are the synthetic-history fixture and measurement,
the opt-in real-user-history benchmark, and the portable-launcher test requiring a
separate trusted previous-build binary. None was newly ignored to obtain a pass.
No Linux/macOS runtime suite, real UNC share, push, tag or GitHub CI dispatch was
performed. The earlier failed rows remain part of the diagnostic record; only the
third batch and final checkpoint establish the completed fix result.
