#!/usr/bin/env python3
"""Exercise the binary that will be published, using isolated offline fixtures."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def check(binary, root):
    version = re.search(r'^version = "([^"]+)"$', (root / "Cargo.toml").read_text(), re.MULTILINE)[1]
    actual = subprocess.check_output([str(binary), "--version"], text=True, timeout=30).strip()
    if actual != f"codex-usage-monit {version}":
        raise RuntimeError(f"Release binary version mismatch: {actual}")
    with tempfile.TemporaryDirectory(prefix="codex-release-smoke-") as temporary:
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith("CODEX_USAGE_MONIT_")}
        for kind in ("STATE", "CONFIG", "CACHE"):
            environment[f"CODEX_USAGE_MONIT_{kind}_DIR"] = str(Path(temporary) / kind.lower())
        result = subprocess.run([
            str(binary), "--codex-home", str(root / "tests/fixtures/codex-home/normal"),
            "--days", "3650", "--offline", "--no-rollout-cache", "snapshot",
            "--format", "json", "--compact",
        ], env=environment, capture_output=True, text=True, timeout=30)
        if result.returncode not in (0, 2):
            raise RuntimeError(f"Release smoke failed ({result.returncode}): {result.stderr}")
        snapshot = json.loads(result.stdout)
        if snapshot.get("partial") is not True or not snapshot.get("tasks"):
            raise RuntimeError("Release smoke must report fixture tasks and explicit partial status")
    print(f"Release binary verified: {actual}; offline fixture passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    check(args.binary.resolve(strict=True), Path(__file__).resolve().parents[1])
