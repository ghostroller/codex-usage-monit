#!/bin/sh
# Shared native Linux/macOS verification entry point for local runs and CI.
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"
test_filter=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --filter)
            [ "$#" -ge 2 ] && [ -n "$2" ] || { echo 'error: --filter needs a test name' >&2; exit 64; }
            test_filter=$2
            shift 2
            ;;
        --help|-h)
            echo 'usage: sh scripts/verify-unix.sh [--filter TEST_NAME]'
            echo 'Default: format, pipeline contracts, Clippy, every test target including PTY, preview, installer, CLI smoke.'
            echo '--filter runs only matching Rust tests; it is not full verification.'
            exit 0
            ;;
        *) echo "error: unknown argument: $1" >&2; exit 64 ;;
    esac
done

for prerequisite in cargo rustc git bash python3; do
    command -v "$prerequisite" >/dev/null 2>&1 || {
        echo "error: missing verification prerequisite: $prerequisite" >&2
        exit 2
    }
done
rustc -vV
if [ -n "$test_filter" ]; then
    echo "Running focused Rust tests: $test_filter"
    exec cargo test --locked --all-targets "$test_filter"
fi

cargo fmt --all -- --check
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py'
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/macos/tests -p 'test_*.py'
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
sh scripts/update-tui-preview.sh --check
sh tests/install.sh
cargo build --locked --bin codex-usage-monit
python3 - <<'PY'
import json
import os
from pathlib import Path
import subprocess
import tempfile

root = Path.cwd()
metadata = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], text=True
))
binary = Path(metadata["target_directory"]) / "debug" / "codex-usage-monit"
subprocess.run([str(binary), "--version"], check=True, timeout=30)
with tempfile.TemporaryDirectory(prefix="codex-usage-monit-unix-smoke-") as temporary:
    environment = os.environ.copy()
    for name in ("STATE", "CONFIG", "CACHE"):
        environment[f"CODEX_USAGE_MONIT_{name}_DIR"] = str(Path(temporary) / name.lower())
    result = subprocess.run([
        str(binary), "--codex-home", str(root / "tests/fixtures/codex-home/normal"),
        "--days", "3650", "--offline", "--no-rollout-cache", "snapshot",
        "--format", "json", "--compact",
    ], env=environment, capture_output=True, text=True, timeout=30)
    if result.returncode not in (0, 2):
        raise SystemExit(f"offline CLI smoke failed ({result.returncode}): {result.stderr}")
    snapshot = json.loads(result.stdout)
    if snapshot.get("partial") is not True or not snapshot.get("tasks"):
        raise SystemExit("offline CLI smoke must contain fixture tasks and an explicit partial flag")
print("Unix verification passed, including offline CLI smoke")
PY
