#!/usr/bin/env bash
# Host orchestration: read-only source, per-run writable copy/log, cached builds.
set -euo pipefail

operation=${1:-verify}
shift || true
case "$operation" in verify|build) ;; *) echo "error: unsupported operation: $operation" >&2; exit 64 ;; esac
repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
platform=
verification_arguments=("$operation")
while (($#)); do
    case "$1" in
        --platform)
            (($# >= 2)) || { echo 'error: --platform needs linux/arm64 or linux/amd64' >&2; exit 64; }
            platform=$2
            shift 2
            ;;
        --filter)
            [[ "$operation" == verify && $# -ge 2 && -n "$2" ]] || { echo 'error: --filter needs a test name and is only valid for verification' >&2; exit 64; }
            verification_arguments+=(--filter "$2")
            shift 2
            ;;
        --help|-h)
            echo 'usage: sh scripts/test-linux-docker.sh [--platform linux/arm64|linux/amd64] [--filter TEST_NAME]'
            echo '       sh scripts/build-linux-amd64-docker.sh'
            echo 'Environment: CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT, CODEX_USAGE_MONIT_RUST_IMAGE'
            exit 0
            ;;
        *) echo "error: unknown argument: $1" >&2; exit 64 ;;
    esac
done

for prerequisite in docker git python3; do
    command -v "$prerequisite" >/dev/null || { echo "error: host prerequisite is unavailable: $prerequisite" >&2; exit 2; }
done
daemon_arch=$(docker info --format '{{.Architecture}}')
case "$daemon_arch" in aarch64|arm64) native_platform=linux/arm64 ;; x86_64|amd64) native_platform=linux/amd64 ;; *) echo "error: unsupported Docker architecture: $daemon_arch" >&2; exit 2 ;; esac
platform=${platform:-$native_platform}
case "$platform" in linux/arm64) architecture=arm64; triple=aarch64-unknown-linux-gnu ;; linux/amd64) architecture=amd64; triple=x86_64-unknown-linux-gnu ;; *) echo "error: unsupported platform: $platform" >&2; exit 64 ;; esac
if [[ "$operation" == build && "$platform" != linux/amd64 ]]; then
    echo 'error: the amd64 build entry point requires --platform linux/amd64' >&2
    exit 64
fi
image=${CODEX_USAGE_MONIT_RUST_IMAGE:-rust:1.97.1-bookworm}
image_platform=$(docker image inspect "$image" --format '{{.Os}}/{{.Architecture}}') || {
    echo "error: image is not present locally: $image" >&2
    echo "pull an image explicitly for $platform, then retry; this script never pulls or deletes images" >&2
    exit 2
}
if [[ "$image_platform" != "$platform" ]]; then
    echo "error: local image $image is $image_platform; requested $platform" >&2
    echo 'set CODEX_USAGE_MONIT_RUST_IMAGE to a matching local tag or image ID' >&2
    exit 2
fi
image_id=$(docker image inspect "$image" --format '{{.Id}}')
if [[ "$platform" != "$native_platform" ]]; then
    echo "Using $platform emulation on $native_platform; use native verification first."
fi

if [[ -n "${CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT:-}" ]]; then
    build_root=$CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT
elif [[ "$(uname -s)" == Darwin ]]; then
    [[ -d /Volumes/File ]] || { echo 'error: set CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT to an existing external volume' >&2; exit 2; }
    build_root=/Volumes/File/codex-usage-monit-docker-build
else
    build_root=${XDG_CACHE_HOME:-$HOME/.cache}/codex-usage-monit/docker
fi
mkdir -p "$build_root"
build_root=$(CDPATH= cd -- "$build_root" && pwd)
if [[ "$(uname -s)" == Darwin && "${CODEX_USAGE_MONIT_ALLOW_INTERNAL_BUILD_ROOT:-0}" != 1 ]]; then
    repository_device=$(df -P "$repository_root" | awk 'NR == 2 { print $1 }')
    build_device=$(df -P "$build_root" | awk 'NR == 2 { print $1 }')
    [[ "$repository_device" != "$build_device" ]] || {
        echo 'error: Docker build root must be on an external volume; explicitly set CODEX_USAGE_MONIT_ALLOW_INTERNAL_BUILD_ROOT=1 to opt into internal storage' >&2
        exit 2
    }
fi

cargo_cache=$build_root/cargo-home
rustup_cache=$build_root/rustup-linux-$architecture
target_directory=$build_root/target-linux-$architecture
run_directory=$build_root/runs/$(date -u +%Y%m%dT%H%M%SZ)-$architecture-$$
mkdir -p "$cargo_cache" "$rustup_cache" "$target_directory" "$run_directory/workspace"
# A numeric host UID usually has no passwd entry in the Linux image. Supply a
# private test identity so OS-derived home lookup works without changing the
# image or running the tests with elevated filesystem capabilities.
runner_uid=$(id -u)
runner_gid=$(id -g)
printf 'codex-test:x:%s:%s:Linux verification:/container-home:/bin/bash\n' "$runner_uid" "$runner_gid" > "$run_directory/passwd"
if [[ "$runner_uid" != 0 ]]; then
    printf '%s\n' 'root:x:0:0:root:/root:/bin/bash' >> "$run_directory/passwd"
fi
log_path=$run_directory/verify.log
python3 "$repository_root/scripts/docker-linux-snapshot.py" prepare \
    "$repository_root" "$run_directory" "$platform" "$image_id" "${verification_arguments[@]}"
echo "Linux workspace: $run_directory/workspace"
echo "Linux log: $log_path"

# Preserve Docker's exit code through tee. --init also reaps orphaned tests.
set +e
docker run --init --rm --pull never --platform "$platform" --cap-drop ALL \
    --user "$runner_uid:$runner_gid" \
    --mount "type=bind,src=$repository_root,dst=/source,readonly" \
    --mount "type=bind,src=$run_directory/workspace,dst=/workspace" \
    --mount "type=bind,src=$cargo_cache,dst=/cargo-home" \
    --mount "type=bind,src=$rustup_cache,dst=/rustup-home" \
    --mount "type=bind,src=$target_directory,dst=/target" \
    --mount "type=bind,src=$run_directory/passwd,dst=/etc/passwd,readonly" \
    --mount "type=bind,src=$run_directory/source-files,dst=/source-files,readonly" \
    --mount "type=bind,src=$run_directory/source.json,dst=/source.json,readonly" \
    --tmpfs '/container-tmp:rw,exec,mode=1777,size=2g' \
    --tmpfs "/container-home:rw,exec,mode=0700,uid=$runner_uid,gid=$runner_gid,size=128m" \
    --workdir /workspace \
    --env HOME=/container-home --env CARGO_HOME=/cargo-home \
    --env RUSTUP_HOME=/rustup-home --env TMPDIR=/container-tmp \
    --env CARGO_TARGET_DIR=/target --env CARGO_BUILD_BUILD_DIR=/target/build \
    --env CODEX_USAGE_MONIT_LINUX_TRIPLE="$triple" \
    "$image_id" bash -c '
        set -euo pipefail
        for prerequisite in git bash python3 cc pkg-config tar file sha256sum perl; do
            command -v "$prerequisite" >/dev/null || { echo "missing image prerequisite: $prerequisite" >&2; exit 2; }
        done
        # Copy the host-produced Git-visible inventory, retaining edits/new
        # files/deletions while excluding ignored files and host Cargo config.
        tar -C /source --no-recursion --null -T /source-files -cf - \
            | tar -C /workspace -xf -
        python3 scripts/docker-linux-snapshot.py stamp /workspace /source.json /source-files
        git init -q /workspace
        channel=$(sed -n "s/^channel = \"\([^\"]*\)\"/\1/p" rust-toolchain.toml)
        [[ -n "$channel" ]] || { echo "missing repository Rust toolchain" >&2; exit 2; }
        export RUSTUP_TOOLCHAIN=$channel-$CODEX_USAGE_MONIT_LINUX_TRIPLE
        rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal --component rustfmt,clippy --no-self-update
        operation=$1
        shift
        if [[ "$operation" == build ]]; then
            cargo build --release --locked --bin codex-usage-monit
            file /target/release/codex-usage-monit
            sha256sum /target/release/codex-usage-monit
        else
            sh scripts/verify-unix.sh "$@"
        fi
    ' docker-linux "${verification_arguments[@]}" 2>&1 | tee "$log_path"
pipeline_status=("${PIPESTATUS[@]}")
command_status=${pipeline_status[0]}
if [[ "$command_status" == 0 && "${pipeline_status[1]}" != 0 ]]; then
    command_status=${pipeline_status[1]}
    echo 'error: writing the Linux verification log failed' >&2
fi
set -e
echo "Linux $operation exit code: $command_status"
echo "Linux log: $log_path"
python3 "$repository_root/scripts/docker-linux-snapshot.py" result "$run_directory" "$command_status"
echo "Linux result: $run_directory/result.json"
if [[ "$command_status" == 0 && "$operation" == build ]]; then
    echo "Linux $architecture GNU binary: $target_directory/release/codex-usage-monit"
fi
exit "$command_status"
