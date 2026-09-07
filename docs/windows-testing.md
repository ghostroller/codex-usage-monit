# Windows testing

Windows runtime verification is configured for the following environments:

| Surface | Architecture | Where it runs | Purpose |
| --- | --- | --- | --- |
| Pull requests and releases | `x86_64-pc-windows-msvc` | GitHub-hosted `windows-2025` | A fresh, repeatable x64 Windows environment and the released executable target. |
| Local macOS development | `aarch64-pc-windows-msvc` | Windows 11 ARM64 in UTM | Native Windows-on-Arm compilation and runtime coverage for changes made on Apple Silicon. |

Native linking and runtime checks run on Windows. A non-Windows host can still
type-check the Windows branches without linking or running the executable:

```bash
cargo check --locked --tests --target x86_64-pc-windows-msvc
```

The Windows target must already be installed with `rustup`. Windows 11 on Arm
can also run the x64 release executable under system emulation, but the local
VM's primary test is the native ARM64 build.

For the 2026-09-08 review changes, local Windows validation was limited to the
MSVC cross-target check above from macOS. The ConPTY tests are now enabled in
the Windows test target; their native runtime result must come from a Windows
CI or VM run. The cross-target check does not establish that result.

## UTM setup on an Apple Silicon Mac

The following configuration is sized for a Mac with 16 GiB RAM. It keeps all
large VM state on an external APFS volume while the source checkout remains in
its usual macOS location.

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
  --iso /Volumes/Drive/codex-usage-monit-windows/iso/Windows11_Arm64.iso \
  --start
```

If the external VM was already provisioned before the ISO was available, attach
the downloaded ISO and start it without recreating its disk:

```zsh
./scripts/macos/attach-windows-iso-utm.sh \
  --iso /Volumes/Drive/codex-usage-monit-windows/iso/Windows11_Arm64.iso \
  --start
```

The script creates the VM in UTM's required staging area, exports it to the
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
disk, and UTM debug logs must stay under
`/Volumes/Drive/codex-usage-monit-windows/`, not in the repository or on the
internal disk.

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

`utmctl list`, `status`, `start`, and `stop` control this VM from macOS. Do not
make the Windows-on-Arm test workflow depend on `utmctl exec`, `utmctl file`,
or `utmctl ip-address`: those commands require the QEMU Guest Agent. The
current UTM Windows Guest Tools package has no native ARM64 Agent. Its
installer attempts to use the x64 Agent under Windows-on-Arm emulation, while
UTM tracks a native ARM64 port as future work. Installing or repairing Guest
Tools is therefore worth one probe with `utmctl exec`, but the guest-command
channel remains best-effort rather than a pipeline dependency. Guest Tools is
still useful for display integration and the WebDAV share.

Run the scripts below from the UTM console, or separately configure an
authenticated remote-management channel such as OpenSSH or WinRM if
unattended guest execution is required. Do not store its passwords or keys in
the repository. Hosted GitHub Actions remains the unattended Windows pipeline.

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
`scripts\windows\verify.ps1`. Its Cargo target directory defaults to
`%LOCALAPPDATA%\codex-usage-monit\cargo-target`, so the build cache and binary
locks remain inside the guest rather than in the mounted macOS checkout.

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

The GitHub Actions Windows jobs invoke the same `verify.ps1` script. This keeps
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
