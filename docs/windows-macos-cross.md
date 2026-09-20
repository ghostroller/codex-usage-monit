# Build a macOS agent on Windows

The Windows development machine can build this project's Apple Silicon executable
with Rust, `cargo-zigbuild` and Zig. Compilation and linking run on Windows. The
destination Mac does not need the project source or a Rust toolchain.

This is a development cross-build workflow, not native macOS test evidence.
Native packaging and explicit `remote deploy-dev HOST --bundle-dir DIR` remain
separate steps. Normal `remote deploy` and TUI **[B] Deploy agent** instead have
the remote download a matching official Release; they never select this local
cross-build. See [remote agents](remote-usage.md).

## Installed development toolchain

The first successful build used:

| Component | Version / location |
| --- | --- |
| Host | Windows 11, x86_64 |
| Rust | Repository-pinned 1.97.0, with `aarch64-apple-darwin` installed |
| cargo-zigbuild | 0.23.4 |
| Zig | 0.16.0 |
| Isolated tool directory | `D:\Dev_Kits\codex-macos-cross` |
| Python environment | `venv` below that directory |
| SDK linker files | `sdk\MacOSX26.1-link.sdk` below that directory |

The Python packages were installed from PyPI into that isolated environment,
without changing the global Python installation or the permanent Windows PATH:

```powershell
python -m venv D:\Dev_Kits\codex-macos-cross\venv
D:\Dev_Kits\codex-macos-cross\venv\Scripts\python.exe -m pip install --only-binary=:all: --index-url https://pypi.org/simple cargo-zigbuild==0.23.4 ziglang==0.16.0
rustup target add aarch64-apple-darwin --toolchain 1.97.0-x86_64-pc-windows-msvc
```

The SDK files came from the already installed
`/Library/Developer/CommandLineTools/SDKs/MacOSX26.1.sdk` on `local-mac`, through a
read-only SSH transfer. Only `SDKSettings.json`, `SDKSettings.plist` and `.tbd`
linker descriptions were copied, retaining their paths and resolving file
symlinks into ordinary files. The Mac was not used to compile the project.

The resulting local SDK contains 5,908 files. Its provenance and archive checksum
are recorded in `D:\Dev_Kits\codex-macos-cross\sdk\source.json`; package download
hashes are in `pip-install.json` at the tool directory root. These machine-local
SDK files are not committed to the repository. They are sufficient for the
current project's build, but contain **no C/C++ headers**. Dependencies that
compile C/C++ or use bindgen may require a full SDK later.

## Repeat the build

Run the following in PowerShell from the repository root. These settings apply
to the current terminal only; a fresh terminal retains the normal Windows build
environment. Keep the separate target/build directories to avoid mixing this
build with the Windows TUI executable.

```powershell
$crossTools = 'D:\Dev_Kits\codex-macos-cross'
$env:PATH = "$crossTools\venv\Scripts;$env:PATH"
$env:CARGO_ZIGBUILD_ZIG_PATH = "$crossTools\venv\Lib\site-packages\ziglang\zig.exe"
$env:CARGO_ZIGBUILD_CACHE_DIR = "$crossTools\cache"
$env:ZIG_GLOBAL_CACHE_DIR = "$crossTools\zig-cache"
$env:SDKROOT = "$crossTools\sdk\MacOSX26.1-link.sdk"
$env:CARGO_TARGET_DIR = Join-Path $PWD 'target\macos-cross'
$env:CARGO_BUILD_BUILD_DIR = Join-Path $PWD 'target\macos-cross\build'
cargo zigbuild --locked --release --target aarch64-apple-darwin --bin codex-usage-monit
```

Output: `target\macos-cross\aarch64-apple-darwin\release\codex-usage-monit`.
This is a macOS executable without an `.exe` suffix. It cannot run on Windows.
The installed target and this verification cover Apple Silicon only; Intel Mac
builds require installing and separately verifying `x86_64-apple-darwin`.

Use the output's Mach-O load commands to determine its deployment floor. The
verified binary records **macOS 13.0**, even though the initial build requested
11.0 through `MACOSX_DEPLOYMENT_TARGET`. Do not claim macOS 11/12 compatibility
from that environment variable alone with this toolchain.

## Verified build

On 2026-09-19, source commit `8f5a22b94c7340cc823e08de34bccd8b65bec676`
built successfully from a clean checkout on Windows in approximately 2m 33s.
The source build ID was
`5acc096de47864a7a5b1806c20f026dddc04e178a85a9b9a784602cef428661e`, matching the
Windows development executable.

The output was inspected on Windows with LLVM tools: it is a 64-bit ARM64 Mach-O
executable for macOS, linked against macOS system libraries and CoreFoundation.
Its embedded ad-hoc signature's 3,657 SHA-256 code-page hashes were independently
verified. This establishes file integrity, not Developer ID signing, notarization
or successful execution on a Mac.

The retained artifact and checksum are under
`dist\macos-cross\aarch64-apple-darwin`; the exact command, tool versions, source
identity, SDK provenance, build log and inspection logs are under
`.codex-usage-monit\verification\windows-macos-cross-20260919`.
No native macOS tests or remote deployment were run as part of that cross-build
verification. These identifiers describe the recorded commit; subsequent source
changes require rebuilding both center and development agent.

`scripts/package-agent.py` executes the target binary to obtain its
bootstrap metadata, so it must still run on the binary's native platform. This
cross-build produces the executable; it does not itself produce a validated
deployment bundle or change the configured remote source. The recorded build
used the former `.agent.json` format; current packaging uses the shared platform
asset and `release-manifest.json` described in [remote updates](remote-updates.md).

References: [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild),
[Rust macOS targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html).

## Verified development deployment

Later on 2026-09-19, the current development source was rebuilt on Windows in
approximately 1m 28s, with build ID
`413284b49fce830a5cd9c253cc974fe44f46aef80a6177b812528dad32be4128`.
The Apple Silicon binary executed successfully on `local-mac` (macOS 15.7.2).
Only the verified binary and `scripts/package-agent.py` were sent to a temporary
private directory on the Mac. The existing packager read the binary's native
bootstrap metadata and generated the manifest; no project checkout or Rust
toolchain was deployed. The generated manifest was retrieved into the local
bundle, then the normal explicit development command was exercised:

```powershell
.\target\debug\codex-usage-monit.exe remote deploy-dev local-mac --bundle-dir .\dist\agents\local-mac-413284b49fce
```

Installation, full binary/build verification, source pin validation and the data
readiness probe passed. Subsequent `remote inspect local-mac` and `remote test
local-mac` confirmed matching source build IDs, protocol 5, writable state,
readable rollouts and quota-history capability. The configured exporter switched
to its managed immutable copy; the global executable and source pin were
preserved, and packaging/upload staging files were removed. This is deployment
and readiness evidence, not a full macOS test suite or a bulk-sync test.

The bundle is in `dist/agents/local-mac-413284b49fce`; source snapshot, build,
packaging, deployment and probe logs are in
`.codex-usage-monit/verification/windows-macos-dev-deploy-20260919`.
These artifacts match that recorded source build; later source edits require a
fresh matching center and agent.
