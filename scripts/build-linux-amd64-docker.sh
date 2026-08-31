#!/bin/sh
set -eu

# Keep every writable Rust build directory outside Docker Desktop's internal
# disk image. Override this when the external volume is mounted elsewhere.
BUILD_ROOT=${CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT:-/Volumes/File/codex-usage-monit-docker-build}
IMAGE=${CODEX_USAGE_MONIT_RUST_IMAGE:-rust:1.97.1-bookworm}

REPO_ROOT=$(git rev-parse --show-toplevel)
REPO_DEVICE=$(df -P "$REPO_ROOT" | awk 'NR == 2 { print $1 }')

mkdir -p "$BUILD_ROOT"
BUILD_DEVICE=$(df -P "$BUILD_ROOT" | awk 'NR == 2 { print $1 }')
if [ "$BUILD_DEVICE" = "$REPO_DEVICE" ] && [ "${CODEX_USAGE_MONIT_ALLOW_INTERNAL_BUILD_ROOT:-0}" != "1" ]; then
    echo "error: Docker build root is on the same device as the repository: $BUILD_ROOT" >&2
    echo "set CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT to a mounted external volume" >&2
    exit 2
fi

CARGO_HOME_DIR=$BUILD_ROOT/cargo-home
RUSTUP_HOME_DIR=$BUILD_ROOT/rustup-home
TARGET_DIR=$BUILD_ROOT/target-linux-amd64
TMP_DIR=$BUILD_ROOT/tmp
HOME_DIR=$BUILD_ROOT/home
mkdir -p "$CARGO_HOME_DIR" "$RUSTUP_HOME_DIR" "$TARGET_DIR" "$TMP_DIR" "$HOME_DIR"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "error: Docker image is not present locally: $IMAGE" >&2
    echo "pull it explicitly after checking internal-disk free space" >&2
    exit 2
fi

docker run \
    --platform linux/amd64 \
    --pull never \
    --rm \
    --user "$(id -u):$(id -g)" \
    --mount "type=bind,src=$REPO_ROOT,dst=/workspace,readonly" \
    --mount "type=bind,src=$CARGO_HOME_DIR,dst=/cargo-home" \
    --mount "type=bind,src=$RUSTUP_HOME_DIR,dst=/rustup-home" \
    --mount "type=bind,src=$TARGET_DIR,dst=/target" \
    --mount "type=bind,src=$TMP_DIR,dst=/container-tmp" \
    --mount "type=bind,src=$HOME_DIR,dst=/container-home" \
    --workdir /workspace \
    --env HOME=/container-home \
    --env CARGO_HOME=/cargo-home \
    --env CARGO_TARGET_DIR=/target \
    --env CARGO_BUILD_BUILD_DIR=/target/build \
    --env CARGO_PROFILE_RELEASE_STRIP=symbols \
    --env RUSTUP_HOME=/rustup-home \
    --env TMPDIR=/container-tmp \
    "$IMAGE" \
    cargo build --release --locked --bin codex-usage-monit

BINARY=$TARGET_DIR/release/codex-usage-monit
echo "Linux amd64 binary: $BINARY"
file "$BINARY"
shasum -a 256 "$BINARY"
du -sh "$CARGO_HOME_DIR" "$RUSTUP_HOME_DIR" "$TARGET_DIR" "$TMP_DIR" "$HOME_DIR"
