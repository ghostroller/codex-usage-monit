# Windows lifecycle implementation verification — 2026-09-21

The implementation follows [the reviewed plan](windows-install-update-plan.md).
User-facing operations and account boundaries are documented in the
[Windows installation guide](windows-installation.md).

## Source and execution context

Work started on `codex/windows-lifecycle` after fast-forwarding remote main to
`6f3be33a74258e049a70aee5f717150b9558db2f`. Native checks use isolated dirty source
snapshots, not a moving checkout. Snapshot manifests record SHA-256 for every
tracked/new source input and the aggregate snapshot identity.

Host: Windows `10.0.26200`, x64, Rust `1.97.0`, Python `3.12.4`, PowerShell `7.4.1`
and Windows PowerShell `5.1.26100.9444`. Full native verification runs as
`DESKTOP-U23KNI9\Ghost` (SID ending `1001`); focused sandbox checks used
`CodexSandboxOffline` (SID ending `1008`). Registry tests only create unique
`HKCU\Software\CodexUsageMonit-InstallationTest-*` keys. They do not alter the
real user PATH, application uninstall registration, scheduled task or SCM service.

Local logs and context manifests are retained under
`D:\Workspace\codex-usage-monit\.codex-usage-monit\windows-lifecycle-evidence\`.
The result files and snapshot manifests together record the full commands,
identities, start/end times, source commit, snapshot identity, temporary
directories and outcomes.

The full-suite code snapshot is `snapshot-20260920T190047-final3`, aggregate SHA-256
`f971da991b983019bc6323b9aa94a482e656fa4aa85dab30f17fc03b0cca43dd`.
The follow-up snapshot is `snapshot-20260920T191605-final4`, aggregate SHA-256
`3742c9b7607485227debeec3f22ef1d9a5bcd0d0175d86d63d3404b20eb186a3`.
Its only code changes are test synchronization in `remote_fact_sync.rs` and a
`cfg(test)` lock-probe helper in `remotes_config.rs`, described below. All product
behavior and shell contracts are unchanged between these two snapshots.

macOS ARM64 cross-target Clippy passed for both snapshots, including the final
test-only lock helper:

```text
cargo clippy --offline --target aarch64-apple-darwin --all-targets --target-dir D:\Workspace\codex-usage-monit\.codex-usage-monit\windows-review-20260921\cross-check\target -- -D warnings
```

Its command/context record and log are in
`.codex-usage-monit/windows-review-20260921/cross-check/result-final4.json` and
`clippy-final4.log` (with separate final3 records). This was Windows-hosted
compilation, not macOS execution.

## Diagnosed failures

- The first full Windows run (`final1`, snapshot
  `efc03369c15a58127d8b9cda6541bc205e3355090a241ad45c78ec77a5e23ce4`)
  passed formatting, Clippy and both-shell script contracts; Rust reported
  **1727 passed, 4 failed**. A CLI assertion still rejected the now-supported
  `service install --format json`. Two deployment mocks recognized only plaintext
  commands and missed Windows UTF-16 encoded PowerShell commands. Those assertions
  now inspect the intended command semantics, including unexpected-command failure.
- The fourth failure was runner configuration: a private TEMP inside this Git
  checkout cannot serve as a non-repository fixture. The same unmodified test
  binary passed with TEMP outside the checkout. Subsequent full runs use an
  isolated system-TEMP directory owned by the actual test user, with access only
  for that SID, SYSTEM and Administrators. Product ACL checks were not weakened.
- `final2` stopped in Clippy because the new test helper used `next_back()` on
  `CommandArgs`, which is not a double-ended iterator. The helper now uses
  `last()`; no runtime behavior changed.
- The `final3` full library run completed **1736 passed, 1 failed**. The failure
  was the preexisting remote fact-staging test's one-second readiness wait,
  before its worker had reached the real staging lock; a dropped receiver then
  caused a secondary send error. The exact same binary passed the focused case
  once under the concurrent full-suite load (80.81 seconds). `final4` removes
  the timing assumption: it waits for the production handshake, probes the real
  configuration sidecar with a nonblocking exclusive lock, then performs each
  mutation while staging remains blocked. Every failure path releases staging
  before joining the worker. The four mutations and rejection of late publication
  remain asserted. This is a test fix, not a production lock change or timeout
  increase. See `final3.log` and `fact-staging-final3-default-temp.log`.
- The `final4` integration run reached ConPTY, where the interaction case
  timed out at initial history loading. The same test binary then passed the
  complete keyboard/mouse/search/resize/exit case once in a separate private
  TEMP (7.10 seconds), without changing timeouts. The shared TEMP root was later
  found missing; its deletion was not explained and is not proven to have caused
  the initial timeout. An attempted follow-up with that absent TEMP failed before
  product execution. These are retained separately in `final4.log`,
  `conpty-final4-exact-result.json`, and `final4-remaining.log`.
- Earlier malformed PE fixtures could block Windows process creation in a loader
  dialog before the subprocess timeout began. Launcher probing now rejects
  malformed PE input and suppresses loader error dialogs using a thread-local
  Windows error mode that is restored afterward.

## Verification scope

The Windows pipeline includes formatting, Clippy with warnings denied, all Rust
test targets, native ConPTY, installer/bootstrap/package contracts, PowerShell
outer-wrapper contracts and CLI smoke. Separate evidence exercises a running old
launcher against another independently compiled build. GUI-host tests launch real
processes and verify no console, exit propagation and descendant cleanup. SCM
tests exercise native ACLs, process jobs, readiness identity and recovery logic
without creating an actual machine service.

The final4 focused lock regression passed (1 case; 1736 other library cases were
already covered by final3). All integration targets were exercised, with the
ConPTY result reconciled as described above. The remaining update/usage targets
passed in `final4-remaining-private-temp.log`: 4 update cases and 9 usage-evidence
cases. The opt-in different-build launcher case also passed separately in that
log; `two-builds-result.json` records both executable hashes and the exact test
command. These are reconciled results, not a claim that the original full command
was entirely green.

The subsequent smoke build was interrupted when the prior tool session ended;
its truncated log is not counted as a completed check. The resumed smoke-only
command and outcome are recorded in `final4-smoke-result.json` and
`final4-smoke.log`: build, version output and the offline JSON fixture passed.

The release workflow passed actionlint `1.7.12` and the 10 Python
workflow contracts using Git Bash. Windows script-only verification also passed
under PowerShell 5.1: 60 verification-wrapper, 17 development-launcher and 10
permission-repair cases, plus 13 bootstrap, 4 installer, 2 setup-rejection and 6
packaging tests. Setup rejection covers 20 real-shell scenarios across 5.1/7 and
their `-File`/`-Command` outer wrappers.

## Explicitly unverified boundaries

- **No local Docker/Linux tests were run**, as requested. `utmctl` is unavailable
  on this native Windows host; native Windows execution is recorded directly.
  macOS cross-target Clippy is static compilation only, not macOS runtime coverage.
- No live Task Scheduler install/logon/logoff, RDP disconnect, unattended reboot,
  named service-account registration or live SSH deployment was performed. Those
  require a deliberately provisioned acceptance environment and the intended
  account credentials/policy. No personal credentials or history were copied.
- The signed User Setup build entry point is implemented, including rejection of
  unsigned or mismatched payloads. An actual signed setup was not produced:
  Inno Setup, SignTool and the publisher's code-signing certificate must be
  provisioned for the release. Sign the application before computing its release
  manifest; then compile, sign and verify the setup. No tag or release was created.
- The supported release target remains Windows x64. This work does not add or
  claim native Windows ARM64 artifacts.
