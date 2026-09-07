# Local Linux verification

Use Docker for routine Linux validation. On Apple Silicon the default test
platform is native Linux arm64; reserve GitHub Actions for a coordinated final
check. Windows runtime validation belongs in the UTM guest's Windows pipeline.

```sh
sh scripts/test-linux-docker.sh
sh scripts/test-linux-docker.sh --filter recorder_
```

The default run uses the same `sh scripts/verify-unix.sh` entry point as CI:
formatting, Python pipeline contracts, Clippy, all Rust test targets (including real PTY interactions),
published-preview comparison, installer tests, and the real offline CLI smoke.
`--filter` runs matching Rust tests only and must not be described as a full pass.
The shared entry point also runs directly on native Linux or macOS.

The host needs Docker, Git, Bash, and Python 3. Docker must be running and the Rust image must already exist locally. The
default image is `rust:1.97.1-bookworm`; the runner installs the exact repository
toolchain from `rust-toolchain.toml`, including rustfmt and Clippy, into a cache.
The first run needs network access for Rust and Cargo dependencies. Subsequent
runs reuse downloads and compiled artifacts. Custom images need Git, Bash,
Python 3, a C compiler, pkg-config, tar, Perl, file, sha256sum, and normal Unix process utilities.
The Debian Rust image supplies these prerequisites. Installer tests exercise
shell profiles without requiring a Zsh installation.

Inspect image architecture before explicitly fetching an image:

```sh
docker info --format '{{.Architecture}}'
docker image inspect rust:1.97.1-bookworm --format '{{.Os}}/{{.Architecture}}'
# If needed, after checking Docker Desktop disk capacity:
docker pull --platform linux/arm64 rust:1.97.1-bookworm
```

The runner never pulls images automatically or removes images/volumes. A local
image with the wrong architecture is rejected before execution. To use another
already present image, set `CODEX_USAGE_MONIT_RUST_IMAGE` to its tag or image ID.

## Storage and repeatability

On macOS the default build root is
`/Volumes/File/codex-usage-monit-docker-build`. Set
`CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT` when using a different external volume.
Internal storage requires explicit
`CODEX_USAGE_MONIT_ALLOW_INTERNAL_BUILD_ROOT=1`. On Linux the default is
`${XDG_CACHE_HOME:-$HOME/.cache}/codex-usage-monit/docker`.

Source is mounted read-only, then Git's tracked and non-ignored untracked file
inventory is copied into a new writable run directory, preserving current edits
and deletions. Ignored files and `.cargo/config.toml` overrides are excluded.
Tests use an initialized independent
Git repository. Host Cargo/Rustup configuration is not mounted; both Cargo target
and Cargo build directories are explicitly container paths. Rustup caches and
targets are separated by Linux architecture, while crate downloads are shared.

Test temporary files and the test user's home use bounded Linux tmpfs mounts
(2 GiB and 128 MiB). This preserves Linux file locking and permission semantics;
macOS file-sharing bind mounts can silently break lock-contention tests. A private
passwd entry binds the host UID to the test home, and container capabilities are
dropped. These temporary files disappear when the container exits. Source,
compiled artifacts, gallery output, and logs remain on the selected build volume.

Every invocation prints its workspace and log paths. Its exit status is the
Docker/test status, even though output is streamed through `tee`; log-write failure
also fails a successful test run. The generated
gallery remains at `workspace/target/tui-gallery` inside that run. Run directories
and logs are retained for diagnosis; inspect and remove old run directories
manually when no longer needed. Existing caches are not deleted.

`source.json` records the source HEAD, dirty state, host OS/architecture, Docker
image ID, requested platform, and operation/filter. The guest records a SHA-256
fingerprint of the actual copied paths, modes, and file contents in
`workspace/.linux-verification.json` and prints it in the log. `result.json`
combines that identity with the final status and log path. A Git repository with
no commit explicitly records `source_head: null` and
`source_git_head_available: false`; it must not be cited as a verified commit.
Rust's full version and host triple are also printed by the shared verifier.

## amd64 and release limits

```sh
CODEX_USAGE_MONIT_RUST_IMAGE=<local-amd64-image> \
  sh scripts/test-linux-docker.sh --platform linux/amd64
CODEX_USAGE_MONIT_RUST_IMAGE=<local-amd64-image> \
  sh scripts/build-linux-amd64-docker.sh
```

On an arm64 Docker engine these commands require amd64 emulation and are slower;
timing-sensitive tests may behave differently. The build script builds only a
GNU/glibc amd64 executable and prints its checksum. It neither runs tests nor
produces the musl release artifacts. Native arm64 tests establish Linux runtime
behavior; cross compilation alone cannot establish amd64 runtime behavior.
Docker also does not exercise a real logged-in systemd user manager, a desktop
terminal emulator, Windows ConPTY, or macOS launchd.
