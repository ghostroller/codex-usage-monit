#!/usr/bin/env python3
"""Record exactly which Git-visible source files a local Linux run copied."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import stat
import subprocess


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def prepare(root, output, target_platform, image_id, command):
    def git(*arguments):
        return subprocess.check_output(["git", "-C", str(root), *arguments])

    names = sorted(set(git("ls-files", "--cached", "--others", "--exclude-standard", "-z").split(b"\0")) - {b""})
    copied = []
    for name in names:
        if name == b".cargo/config.toml":
            continue
        path = root / os.fsdecode(name)
        if not os.path.lexists(path):
            continue  # Keep tracked deletions deleted in the snapshot.
        if path.is_dir() and not path.is_symlink():
            raise SystemExit(f"source directory entry needs explicit submodule support: {path}")
        copied.append(name)
    (output / "source-files").write_bytes(b"\0".join(copied) + b"\0")
    head = subprocess.run(["git", "-C", str(root), "rev-parse", "--verify", "HEAD"],
                          capture_output=True, text=True, check=False)
    write_json(output / "source.json", {
        "schema_version": 1,
        "source_head": head.stdout.strip() if head.returncode == 0 else None,
        "source_git_head_available": head.returncode == 0,
        "source_dirty": bool(git("status", "--porcelain=v1", "-z")),
        "source_file_count": len(copied),
        "host_os": platform.system(),
        "host_architecture": platform.machine(),
        "target_platform": target_platform,
        "image_id": image_id,
        "command": command,
    })


def stamp(root, source_info, source_files):
    digest = hashlib.sha256()
    for name in source_files.read_bytes().split(b"\0"):
        if not name:
            continue
        path = root / os.fsdecode(name)
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            contents = os.fsencode(os.readlink(path))
            kind = "symlink"
        elif stat.S_ISREG(metadata.st_mode):
            contents = path.read_bytes()
            kind = "file"
        else:
            raise SystemExit(f"unsupported source file type: {path}")
        header = json.dumps([os.fsdecode(name), kind, stat.S_IMODE(metadata.st_mode)],
                            ensure_ascii=True, separators=(",", ":")).encode()
        digest.update(len(header).to_bytes(8, "big"))
        digest.update(header)
        digest.update(hashlib.sha256(contents).digest())
    manifest = json.loads(source_info.read_text(encoding="utf-8"))
    manifest.update({
        "snapshot_sha256": digest.hexdigest(),
        "guest_os": platform.system(),
        "guest_architecture": platform.machine(),
    })
    write_json(root / ".linux-verification.json", manifest)
    print(json.dumps(manifest, sort_keys=True), flush=True)


def result(output, exit_code):
    snapshot = output / "workspace" / ".linux-verification.json"
    manifest = json.loads((snapshot if snapshot.exists() else output / "source.json").read_text(encoding="utf-8"))
    manifest.update({
        "exit_code": exit_code,
        "status": "passed" if exit_code == 0 else "failed",
        "log_path": str(output / "verify.log"),
        "workspace": str(output / "workspace"),
    })
    write_json(output / "result.json", manifest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="operation", required=True)
    preparation = commands.add_parser("prepare")
    preparation.add_argument("root", type=Path)
    preparation.add_argument("output", type=Path)
    preparation.add_argument("target_platform")
    preparation.add_argument("image_id")
    preparation.add_argument("command", nargs=argparse.REMAINDER)
    stamping = commands.add_parser("stamp")
    stamping.add_argument("root", type=Path)
    stamping.add_argument("source_info", type=Path)
    stamping.add_argument("source_files", type=Path)
    completion = commands.add_parser("result")
    completion.add_argument("output", type=Path)
    completion.add_argument("exit_code", type=int)
    arguments = vars(parser.parse_args())
    operation = arguments.pop("operation")
    {"prepare": prepare, "stamp": stamp, "result": result}[operation](**arguments)


if __name__ == "__main__":
    main()
