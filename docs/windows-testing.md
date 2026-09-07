# Windows testing

Use the local Windows VM for routine Windows changes. Reserve hosted CI for
consolidated verification of substantial changes and releases; a local guest
failure is a diagnostic to investigate, not a reason to silently switch runners.

| Surface | Architecture | Where it runs | Purpose |
| --- | --- | --- | --- |
| Daily development | Installed Windows MSVC host, recorded with each result | Existing Windows 11 VM in UTM | Focused regressions and the full Windows pipeline before consolidated checks. |
| Consolidated CI and releases | `x86_64-pc-windows-msvc` | GitHub-hosted Windows runner | Repeatable validation of the release target. |

## Daily host entry point

First verify the existing guest and installed tools. This command does not install
software, create a VM, or run project tests:

```zsh
python3 scripts/macos/test-windows-utm.py --doctor \
  --toolchain-home 'C:\Users\user'
```

`--toolchain-home` identifies an existing guest profile containing `.cargo` and
`.rustup`; replace it with the actual Windows profile or omit it when the guest
execution account already has Rust on PATH. The Guest Agent can run as `SYSTEM`,
so the result records the execution user, process/native architecture, Rust
version, and effective target. The runner reuses that Rust installation without
changing the execution identity or `USERPROFILE`; tests under `SYSTEM` do not
establish standard-user service behavior.

For the normal full pipeline, remove `--doctor`:

```zsh
python3 scripts/macos/test-windows-utm.py \
  --toolchain-home 'C:\Users\user'
```

For a focused regression during development:

```zsh
python3 scripts/macos/test-windows-utm.py \
  --toolchain-home 'C:\Users\user' \
  --focused --test-filter bounded_process::tests
```

`--focused` runs filtered Rust tests and skips format, Clippy, and CLI smoke
checks; it requires `--test-filter`. Without `--focused`, a test filter still
runs the other pipeline stages. Use `--target` for an explicitly installed
Windows MSVC target and `--profile release` for release-mode verification.
`--start` may start the existing stopped VM; no invocation recreates or replaces
it. The guest test deadline defaults to 1,800 seconds and can be set with
`--timeout` (1–7,200 seconds).

The host packages the current tracked and unignored untracked files, including
working-tree changes and deletions, into a ZIP snapshot. It rejects symlinks and
paths unsafe on Windows. Ignored state such as a host's `.cargo/config.toml`
is excluded. The guest checks the archive SHA-256 and expands it into a unique
local temporary directory, so compilation and tests do not depend on WebDAV
locking or source files changing underneath them.

The printed host artifact directory contains `request.json`, `result.json`,
`verify.log` after a test run, and the exact source archive. Use `--output-dir`
to choose a parent directory; each run still gets a unique subdirectory. Results
include the HEAD revision, tracked/untracked dirty flags, archive SHA-256, a
unique run ID, requested scope, toolchain, account, and completion status.
Only a matching result file plus a retrieved verification log establishes a
passing test run. An `utmctl` exit code of zero is insufficient: it may return
before guest execution completes or even accompany a guest-file error.

`ready` means the doctor round trip and Rust tool probe succeeded. `passed` or
`failed` describes the requested native test run; `blocked` describes missing
tools, unavailable guest transport, or an unverifiable result. Guest timeouts
and host cancellation request termination of the verification process tree;
cleanup's exit code is recorded. The guest also enforces its own deadline if
the host transport disappears. Retain failed-run artifacts for diagnosis and
remove only the identified run directories when they are no longer needed.

## Cross-target checks and runtime evidence

Native linking and runtime checks run on Windows. A non-Windows host can still
type-check the Windows branches without linking or running the executable:

```bash
cargo check --locked --tests --target x86_64-pc-windows-msvc
```

The Windows target must already be installed with `rustup`. Windows 11 on Arm
can also run the x64 release executable under system emulation. Without an
explicit `--target`, the runner tests the installed Rust host target. Compare
`effectiveTarget`, `rustHost`, and `nativeArchitecture` in `result.json` before
describing a run as native ARM64 or emulated x64.

During the 2026-09-08 review, a full local run passed on
`aarch64-pc-windows-msvc` with Rust 1.97.0 under `SYSTEM`: **1,658 Rust tests
passed**, with one existing manual history benchmark ignored. Format, Clippy,
the real ConPTY interaction test, and both CLI smoke checks also passed.
This establishes ARM64 guest execution; it does not establish standard-user
service behavior or a separate x64 run.

Evidence is in
`/private/tmp/codex-utm-review/6faba9595f604aa590542d4ff1e6370b/`
(`result.json`, `verify.log`, and `source.zip`). The source was commit
`cd6b49ee372bad4903ddba8c03dba153352f3d84` plus tracked/untracked working changes;
the archive SHA-256 was
`d4f31358e7a46b078f63c00fe04be00d50b5b5dcf2ef8e3f70db844ee961ce54`.
Later host result-validation refinements were covered by the Python contracts.
An additional one-second deadline probe returned `timed_out`, a nonzero host
exit status, and process-tree cleanup code 0; its artifacts are in the sibling
run directory `2b3e6d82f642443d884102602e4479ba`.
Neither a doctor result nor cross-target compilation alone establishes ConPTY
behavior; cite the matching native result and transcript for subsequent runs.

## UTM setup on an Apple Silicon Mac

The following configuration is sized for a Mac with 16 GiB RAM. It keeps all
large VM state on an external APFS volume while the source checkout remains in
its usual macOS location.

Provision only when no existing test VM is available. First run `utmctl list`
and `utmctl status codex-usage-monit-windows`. Use the existing VM's name or UUID
with the runner's `--vm` option; add `--start` if it is stopped. If the VM is not
registered, mount its external volume, locate the existing `.utm` bundle, and
open that bundle in UTM. A missing external volume is a mount/recovery issue,
not a reason to change the VM name or create another disk. The configuration
below describes first-time setup; substitute the mounted volume's actual path.

| Setting | Value |
| --- | --- |
| Windows installer | [Official Windows 11 ARM64 ISO](https://www.microsoft.com/software-download/windows11arm64) |
| UTM provisioner | `scripts/macos/provision-windows-utm.sh` (UTM AppleScript bridge) |
| VM name | `codex-usage-monit-windows` |
| VM bundle location | `/Volumes/Drive/codex-usage-monit-windows/utm/codex-usage-monit-windows.utm` |
| ISO location | `/Volumes/Drive/codex-usage-monit-windows/iso/` |
| Guest tools ISO | `utm-guest-tools-latest.iso` in that directory (downloaded from UTM's official URL if absent) |
| RAM | 6144 MiB |
| CPU cores | 4 |
| Virtual disk | 96 GiB, dynamically allocated |
| Guest tools | Mounted as the second CD; install automatically during Setup or manually afterwards |
| Shared directory | Optional; configure the repository path in UTM and use **SPICE WebDAV** |

Download a Windows 11 ARM64 ISO from the official Microsoft page and save it
under the external `iso/` directory. Then provision and start the VM with the
official UTM AppleScript interface:

```zsh
./scripts/macos/provision-windows-utm.sh \
  --storage-root /Volumes/Drive/codex-usage-monit-windows \
  --iso /Volumes/Drive/codex-usage-monit-windows/iso/Windows11_Arm64.iso \
  --start
```

If the external VM was already provisioned before the ISO was available, attach
the downloaded ISO and start it without recreating its disk:

```zsh
./scripts/macos/attach-windows-iso-utm.sh \
  --iso /Volumes/Drive/codex-usage-monit-windows/iso/Windows11_Arm64.iso \
  --guest-tools-iso /Volumes/Drive/codex-usage-monit-windows/iso/utm-guest-tools-latest.iso \
  --start
```

When substituting another volume, update every path in the command. Provisioning
uses `--storage-root` for the VM and default guest-tools location; changing only
`--iso` does not change that root. Attaching media uses the explicit
`--guest-tools-iso` path. Both setup scripts accept `--vm-name` for a nondefault
VM name; the daily Python runner accepts `--vm` for a name or UUID.

The provisioner creates the VM in UTM's required staging area, exports it to the
external bundle path, verifies the bundle, removes only that just-created
staging VM, and opens the external package in place. This avoids retaining a
second virtual disk on the internal SSD. It uses UTM's documented `make`,
`export`, `delete`, `open`, and `start` AppleScript commands; it never
overwrites an existing `.utm` bundle. It also mounts the Windows installer and
the UTM Windows Guest Tools ISO as the first and second CD drives.

The scripting dictionary exposes UEFI but not TPM, Secure Boot, or the host
directory URL. If Windows Setup reports a hardware-requirement problem, use
UTM's Windows wizard/settings to enable TPM and Secure Boot, or follow UTM's
documented installer workaround. After installing SPICE guest tools, select the
repository directory in UTM's sharing UI; the script has already selected the
WebDAV sharing mode. The Windows installer, guest tools ISO, VM bundle, virtual
disk, and UTM debug logs must stay under the selected external storage root
(`/Volumes/Drive/codex-usage-monit-windows/` in these examples), outside the
repository and the internal disk.

UTM's Windows guide explains the wizard and its current Windows 11 guest-tools
workarounds. After Windows Setup, install the guest tools if they did not run
automatically. They supply the SPICE network/display drivers and WebDAV share;
if the shared drive is absent, run:

```powershell
& "C:\Program Files\SPICE webdavd\map-drive.bat"
```

Use a local Windows account or an account appropriate for this development VM.
Windows activation and any Microsoft account credentials are deliberately not
stored in this repository or in automation scripts.

### UTM CLI automation boundary

`utmctl list`, `status`, `start`, and `stop` manage the existing VM. The host
runner uses `utmctl exec` and `utmctl file push/pull`, which require a functioning
Guest Agent. Probe the installed guest with `--doctor`; do not infer agent
availability from the guest CPU architecture or an installer version. The
2026-09-08 local probe demonstrated working execution and file transfer while
`utmctl exec` returned empty output and status zero even for a failing command.
The runner therefore reads a unique result file and verifies its run ID and
source hash instead of trusting that transport return value.

If the Guest Agent is unavailable, use the UTM console to run the PowerShell
pipeline below, or repair the existing guest tools. Do not recreate the VM or
store passwords or SSH keys in the repository. A mapped WebDAV drive belongs
to a Windows logon session and may be absent from the Guest Agent account; the
host runner's copied source snapshot avoids that dependency. For manual runs
from a shared checkout, `verify.ps1` still forces both Cargo artifact directories
onto a guest-local fixed drive.

## First guest run

Open an elevated PowerShell in the Windows VM at the mapped repository path and
run the bootstrap once:

```powershell
Set-ExecutionPolicy -Scope Process Bypass -Force
& .\scripts\windows\bootstrap.ps1 -RepositoryPath (Get-Location).Path
```

The bootstrap script installs Git, the Microsoft C++ Build Tools workload with
both ARM64 and x64 MSVC targets, `rustup`, and the pinned Rust toolchain with
`rustfmt` and Clippy. Git is a test dependency because repository-evidence
tests create temporary repositories. The script then calls
`scripts\windows\verify.ps1`. Its Cargo target and intermediate build directories default to
`%LOCALAPPDATA%\codex-usage-monit\cargo-target` and
`%LOCALAPPDATA%\codex-usage-monit\cargo-build`. Both are enforced by
`verify.ps1`, including when it is called directly, so a host's ignored Cargo
configuration cannot redirect Windows build locks onto the shared checkout.
Explicit `-CargoTargetDir` and `-CargoBuildDir` overrides must also name local
fixed drives. Bootstrap discovers already installed Git and Rust before
requesting an installer; `winget` is required only when installation is needed.

For later runs, invoke the shared verification pipeline directly:

```powershell
.\scripts\windows\verify.ps1 -RepositoryPath (Get-Location).Path `
  -CargoTargetDir "$env:LOCALAPPDATA\codex-usage-monit\cargo-target"
```

Use `-Profile release` for an additional release-mode test and smoke run. Use
`-Target x86_64-pc-windows-msvc` on the ARM64 guest only after installing that
Rust target and the matching MSVC components; Windows 11 on Arm can execute
the resulting x64 binary under emulation.

For a focused diagnosis, pass `-TestFilter` with a Cargo test-name filter;
the script still loads the same MSVC environment and guest-local target
directory. Do not use a filter as a substitute for the normal full pipeline.

The pipeline performs the following checks in order:

1. `cargo fmt --all -- --check`
2. `cargo clippy --locked --all-targets -- -D warnings`
3. `cargo test --locked --all-targets`
4. A real `codex-usage-monit.exe` smoke test: `--version` plus a compact JSON
   snapshot using the checked-in offline fixture and isolated temporary state.
   The offline snapshot is intentionally marked `partial` and may return the
   CLI's usable-but-partial exit code (`2`); the script validates that JSON
   contract instead of treating it as a failure.

The consolidated Windows pipeline invokes the same `verify.ps1` script. This keeps
local UTM validation and hosted x64 CI aligned. `tests/tui_pty.rs` uses ConPTY on
Windows to exercise keyboard input, search focus, mouse clicks, compact resize,
rendered styles, and normal exit. The same interaction test uses Unix PTYs in
the Linux and macOS jobs. Signal-based terminal-restoration checks remain
Unix-only; Windows console-close handling still needs separate native
verification.

## References

- [UTM: Windows 11 guest guide](https://docs.getutm.app/guides/windows/)
- [UTM: Windows guest tools and SPICE WebDAV sharing](https://docs.getutm.app/guest-support/windows/)
- [UTM: AppleScript scripting reference](https://docs.getutm.app/scripting/reference/)
- [UTM: QEMU Guest Agent ARM64 tracking issue](https://github.com/utmapp/UTM/issues/5134)
- [UTM Guest Tools installer source](https://github.com/utmapp/spice-nsis/blob/main/win-guest-tools.nsis)
- [Microsoft: Windows 11 Arm64 ISO overview](https://learn.microsoft.com/windows/arm/iso)
- [Microsoft: Visual Studio Build Tools component IDs](https://learn.microsoft.com/visualstudio/install/workload-component-id-vs-build-tools)
- [Rust: Windows MSVC platform support](https://doc.rust-lang.org/rustc/platform-support/windows-msvc.html)
- [GitHub Actions: matrix workflow syntax](https://docs.github.com/actions/reference/workflows-and-actions/workflow-syntax)
