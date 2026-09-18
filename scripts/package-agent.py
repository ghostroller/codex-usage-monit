#!/usr/bin/env python3
"""Package a native agent and its bootstrap manifest from an already built binary."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

TARGETS = {
    "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
}


def package(binary, output):
    binary = binary.resolve(strict=True)
    info = json.loads(subprocess.check_output(
        [str(binary), "remote-agent", "info", "--sha256"], timeout=30,
    ))
    if (info.get("schemaVersion") != 1 or info.get("product") != "codex-usage-monit"
            or info.get("target") not in TARGETS
            or not re.fullmatch(r"[0-9a-f]{64}", info.get("buildId", ""))):
        raise ValueError("Unsupported agent bootstrap identity")
    size = binary.stat().st_size
    if not 0 < size <= 128 * 1024 * 1024:
        raise ValueError("Agent binary must be between 1 byte and 128 MiB")
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    if info.pop("executableSha256", None) != digest:
        raise ValueError("Agent binary changed while being packaged")
    stem = f"codex-usage-monit-{info['target']}.agent"
    filename = stem + (".exe" if "windows" in info["target"] else "")
    output.mkdir(parents=True, exist_ok=True)
    destination = output / filename
    if binary != destination.resolve():
        shutil.copyfile(binary, destination)
    if hashlib.sha256(destination.read_bytes()).hexdigest() != digest:
        raise ValueError("Packaged agent checksum mismatch")
    manifest = {"schemaVersion": 1, "agent": info, "file": filename,
                "size": size, "sha256": digest}
    path = output / (stem + ".json")
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Packaged {info['target']} build {info['buildId']}: {path}")
    return manifest


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path("dist/agents"))
    args = parser.parse_args()
    package(args.binary, args.output_dir)
