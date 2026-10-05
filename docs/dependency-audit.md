# Dependency advisory disposition

## 2026-10-05 dependency update

Implemented in a separate worktree on `codex/dependency-updates`, based on
`78da31f23738ae61bd7ae1fa33bd1d81da4930bd`. The original checkout's uncommitted
changes were left in that checkout.

| Dependency | Locked before | Locked after | Reason |
| --- | --- | --- | --- |
| unicode-width | 0.2.0 | 0.2.2 | Unicode 17 width tables and corrected quotation variation widths; see the [upstream source](https://github.com/unicode-rs/unicode-width/blob/v0.2.2/src/tables.rs) and [width rules](https://github.com/unicode-rs/unicode-width/blob/v0.2.2/src/lib.rs). |
| clap / clap_derive | 4.6.1 | 4.6.7 | Same-major maintenance release, including help fixes; see the [upstream changelog](https://github.com/clap-rs/clap/blob/v4.6.7/CHANGELOG.md). |
| clap_builder | 4.6.0 | 4.6.7 | Required Clap companion update. |

The manifest now requires these versions as its lower bounds. The lockfile changes
only these four package versions; clap_derive selects the already locked syn 3.0.5.
Clap's new opt-in deferred subcommand initialization is not enabled.

The new width rules exposed a session-title truncation bug: adding individual
character widths undercounted a fullwidth quotation variation sequence.
Session pane titles and process error messages now consume complete grapheme
clusters with their string widths, preserving their 48- and 512-cell limits and
avoiding split ZWJ emoji. Regressions cover both paths and narrow TUI search,
cursor and text truncation. The new tests failed against the old dependency
snapshot; with the updated dependencies but the original truncation helper,
the title-budget regression also failed. Red-run evidence is in
`target/dependency-updates-2026-10-05/red-baseline.json`,
`red-compatibility.json`; logs are `macos-red-compatibility.log` and
`red-baseline/target/dependency-updates-2026-10-05/macos-red-baseline.log`
under the same evidence directory.

### Advisory audit

cargo-audit 0.22.2 checked all 256 locked packages against 1,290 advisories at
RustSec commit `ef6173cbc5c50ec8166f9a5b28f07834144373ee`
(updated `2026-10-03T10:14:03+02:00`). The result was **zero vulnerabilities
and zero advisory warnings**, with no ignored advisories or platform filters.
The final lockfile SHA-256 is
`3217c0f6a13c01eef36b467f3a3bcbb9e05a6e3726e21bc32c4032685fc11bd1`.

The checker and database were reused from the original checkout's ignored
`target/dependency-review-2026-10-05/` directory. The command, run from the new
worktree, was:

```sh
/Users/user/Workspace/codex-usage-monit/target/dependency-review-2026-10-05/tools/bin/cargo-audit audit --deny warnings --db /Users/user/Workspace/codex-usage-monit/target/dependency-review-2026-10-05/advisory-db --json
```

The report and execution record are
`target/dependency-updates-2026-10-05/audit.json` and `audit-evidence.json`.

### Local verification

The full platform verification runs used Rust 1.97.0 and the dirty source based
on the commit above. The six-file implementation diff SHA-256 before adding this
documentation was
`eadc4e67f26b7091a393b5405af706b2c4d7beb2f7c97d253deac8186bf7cc27`.
The build inputs remained unchanged after these runs;
`target/dependency-updates-2026-10-05/tested-source-files.json` records their
individual SHA-256 values. Documentation added afterward was checked separately.

| Platform | Full command | Result and evidence |
| --- | --- | --- |
| macOS 15.7.2 / ARM64 | `sh scripts/verify-unix.sh` | Passed: 2,036 Rust tests, 0 failures, 3 ignored; format, Clippy, Python contracts (80 + 10), PTY (2/2), preview, installer and offline CLI smoke. Logs and source-before/after records: `target/dependency-updates-2026-10-05/macos-full-clean.{log,json}`. |
| Linux / ARM64 Docker | `sh scripts/test-linux-docker.sh` | Passed: 2,033 Rust tests, 0 failures, 3 ignored; full Unix pipeline including PTY (2/2). Snapshot SHA-256 `68a002fd85df1ad18d8181a8d07c4164eff647d38827a9d52e43699ec6e3c773`; `verify.log`, `source.json` and `result.json` in `/Volumes/File/codex-usage-monit-docker-build/runs/20261004T235228Z-arm64-98098/`. |
| Windows 11 ARM64 UTM / x64 MSVC target | Full recovery invocation below | Passed: 1,990 Rust tests across 13 targets, 0 failures, 4 ignored; format, Clippy, 25 Python contracts, 105 PowerShell 5.1 contracts, ConPTY (2/2) and real CLI/offline JSON smoke. Archive SHA-256 `09ab0878aeff017e1e82ab98e4ea97c0c2fcda8d7fc7712804924ec48f57259c`. |

The macOS commands were captured with
`python3 /private/tmp/codex-dependency-update-run-check.py macos-full-clean -- sh scripts/verify-unix.sh`;
the recording helper was preserved as `target/dependency-updates-2026-10-05/run-check.py`.
The first native full attempt reused the old compatibility snapshot's test
artifact because the snapshots shared a Cargo target directory. After
`cargo clean -p codex-usage-monit`, both focused regressions and the full native
pipeline passed with stable source. The failed run remains in `macos-full.{log,json}`.
Windows's first full attempt passed format/Clippy but stopped before tests
because Python was absent from the SYSTEM process PATH; evidence remains in
`target/dependency-updates-2026-10-05/windows/2eace57922244bad85c8838bb4801c7e/`.
The first Python supplement found the existing
`C:\Tools\codex-usage-monit\python-3.13.16-arm64\python.exe`.
That SYSTEM run then stopped at the existing `temp-missing` shell contract:
[GetTempPath2](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-gettemppath2w)
resolves SYSTEM's temporary directory independently of `TEMP`/`TMP`.
Its result and log remain in
`target/dependency-updates-2026-10-05/windows-python-path/31c0b55c1bb741da88f6f90ca51b73de/`.

The successful Windows run reused the documented
[interactive-user recovery](config-lock-test-stability-2026-10-04.zh-CN.md),
with the same source ZIP and unchanged standard host runner, guest endpoint and
`verify.ps1`. The copied recovery helpers and their SHA-256 provenance are in
the ignored evidence directory. The full command was:

```sh
python3 target/dependency-updates-2026-10-05/verify_windows_interactive.py \
  --interactive-user 'WIN-MM0JRLGM2Q3\user' \
  --python-dir 'C:\Tools\codex-usage-monit\python-3.13.16-arm64' \
  --private-temp 'C:\Users\user\AppData\Local\Temp' \
  --toolchain-home 'C:\Users\user' \
  --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' \
  --target x86_64-pc-windows-msvc \
  --output-dir target/dependency-updates-2026-10-05/windows-interactive-x64
```

The result is in
`target/dependency-updates-2026-10-05/windows-interactive-x64/f564178df10d4161a3c5563305b4507e/`:
`result.json`, `verify.log`, `interactive-context.json`,
`source-binding.json`, `verification-summary.json` and `task-cleanup.json`.
The account was `WIN-MM0JRLGM2Q3\user`, administrator=true, session 1.
The Rust host and VM were ARM64; the tested executables used the supported
`x86_64-pc-windows-msvc` target and ran through Windows x64 emulation.
There were no filters, focused mode or skipped stages. Both new Unicode
regressions and both real ConPTY tests passed. Cleanup confirms that the
temporary task was removed, its process tree stopped and no process IDs remained.

An earlier interactive ARM64 run passed the two new regressions but failed
seven remote-agent tests before integration/ConPTY. Static inspection tied
all seven failures to the unchanged remote-agent/release allowlists accepting
Windows x64 while those fixtures used the current ARM64 build target; an old
dependency Windows baseline was not run. The failed run and classification remain in
`target/dependency-updates-2026-10-05/windows-interactive/a6b7463d94ad40bfb7a4a7c3b8044869/`.

Rust pass counts include only the main all-target results; nested child tests
and the preview gallery rerun are excluded.

The three ignored Unix Rust tests are the existing manual history benchmark and
fixture/synthetic measurements; Python's six Unix skips require Windows.
Windows also ignores the existing manual proxy case requiring `PREVIOUS_TEST_BINARY`.
Documentation checks covered whitespace, local links, upstream version sources,
source hashes and verification counts; see `target/dependency-updates-2026-10-05/documentation-check.json`.
No hosted CI was dispatched, and no release tag or publication was created.
macOS and Linux were tested on ARM64. Windows coverage uses the supported
x64 target running in the ARM64 Windows VM, with the logged-in user's administrator
test process. This does not establish Windows ARM64 agent support or unelevated
Windows service behavior.

## 2026-09-26 local checkpoint

Audited the lockfile at source commit `62f6a3ab2a787759867379071ffd5fa1d00b4ba6`
using cargo-audit 0.22.2 on native Windows x86_64. RustSec database commit
`e2111519ba6d14a5da59a7b2e5c8083ae8a37c01` was last updated at
`2026-09-25T19:51:57+02:00` and contained 1,271 advisories. The complete lockfile
contained 247 dependencies; there were **zero vulnerabilities and zero advisory
warnings**, with no ignored advisories or target filters. Yanked-package checking
was enabled. No project dependency or lockfile changes were needed.

The lockfile SHA-256 was
`3d6e81337754deabd7eb5c25a440501dd7e414ba3a1b4d9b07362d26711bc298`.
The missing checker was installed into the ignored workspace directory with
`cargo install cargo-audit --version 0.22.2 --locked --root target/tools/refactoring-audit`;
it was not installed globally. The audit command was:

```powershell
target/tools/refactoring-audit/bin/cargo-audit.exe audit --deny warnings --db target/verification/refactoring-next-20260926/n3/advisory-db --json
```

The JSON report and execution record are in
`target/verification/refactoring-next-20260926/n3/audit.log` and `audit.json`.
The earlier offline installation attempt failed because the tool was not cached;
the subsequent fixed-version installation and network-backed audit succeeded.
This local audit did not trigger hosted CI and is not a new runtime test pass or
a guarantee against future advisories. The historical dependency changes below
remain unchanged.

## 2026-09-08 dependency changes

Checked on 2026-09-08 against RustSec database commit
`faedffd5118c1835e13cca3babb6059afb1eb8d0` using cargo-audit 0.22.2.
The review started at repository commit `8104cfc`; its supplied report used
`6efcede3ae80975ac1e4d055ae9f36539da2d1eb`.

| Locked dependency before | Advisory | Disposition |
| --- | --- | --- |
| lru 0.12.5 via ratatui 0.29.0 | [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002.html), unsound mutable iterator | Upgrade to lru 0.18.4 via ratatui 0.30.2 / ratatui-core 0.1.2; patched since 0.16.3. |
| lru 0.12.5 | [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253.html), panic safety in pop | Same upgrade; patched since 0.18.2. |
| paste 1.0.15 via ratatui | [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html), unmaintained | Removed by the Ratatui upgrade. This was a maintenance warning, not a demonstrated application vulnerability. |
| quick-xml 0.39.4 | [RUSTSEC-2026-0194](https://rustsec.org/advisories/RUSTSEC-2026-0194.html), quadratic duplicate-attribute checks | Upgrade to patched 0.41.0. Found by the complete audit, absent from the supplied review. |
| quick-xml 0.39.4 | [RUSTSEC-2026-0195](https://rustsec.org/advisories/RUSTSEC-2026-0195.html), namespace allocation | Same upgrade. The application uses Reader rather than NsReader; this does not change the decision to remove the affected release. |

The old Ratatui layout cache stores `(Rect, Layout)` keys and uses cache lookup
and insertion. This inspection does not establish an application path to the
affected mutable iterator or a key destructor that panics. No exploitability
claim is made; the affected dependency is removed regardless.

Crossterm is upgraded to 0.29 so the application and Ratatui backend share one
version. Ratatui enables only the existing Crossterm, layout cache and underline
color capabilities. The PTY fixture now answers cursor-position queries issued
by terminal initialization. The XML API replacement uses the same implicit XML
1.0 mode as the deprecated method.

Validation: locked all-target compilation and the complete macOS test suite,
including semantic TUI rendering and real PTY interaction, passed. The final
lockfile passed `cargo audit --deny warnings` with zero vulnerabilities and
zero advisory warnings. The dependency audit workflow checks all locked target
dependencies at manually requested CI checkpoints, before version-tag releases,
and on a weekly schedule. It can also be dispatched independently; ordinary
pushes and pull requests do not start it. See [testing policy](testing.md).
The workflow does not ignore advisories. These are point-in-time results, not a
claim about future advisories.
