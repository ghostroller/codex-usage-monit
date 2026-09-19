"""Prepare an official release on the SSH host without executing its binary."""
import hashlib
import json
from pathlib import Path
import re
import subprocess

RELEASES = "https://github.com/ghostroller/codex-usage-monit/releases/download/"
MAX_BINARY = 128 * 1024 * 1024
MAX_MANIFEST = 32 * 1024


def receive_release_asset(url, destination, maximum):
    try:
        result = subprocess.run([
            "curl", "--fail", "--silent", "--show-error", "--location",
            "--proto", "=https", "--proto-redir", "=https",
            "--connect-timeout", "10", "--max-time", "120",
            "--max-filesize", str(maximum), "--write-out", "%{http_code}",
            "--output", str(destination), url,
        ], capture_output=True, timeout=125)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("agent_release_download_failed: remote curl unavailable or timed out") from error
    if result.returncode:
        kind = "agent_release_unavailable" if result.stdout.strip() == b"404" else "agent_release_download_failed"
        detail = result.stderr.decode("utf-8", "replace")[:600]
        raise RuntimeError(f"{kind}: {url}: {detail}")
    if destination.stat().st_size > maximum:
        raise RuntimeError("agent_release_invalid: downloaded file exceeds size limit")


def prepare_release(expected, stage):
    if not re.fullmatch(r"\.codex-usage-monit-release-[a-f0-9]{32}", stage):
        raise RuntimeError("agent_release_invalid: invalid staging directory")
    if not re.fullmatch(r"[A-Za-z0-9.+-]{1,80}", expected["version"]):
        raise RuntimeError("agent_release_invalid: invalid release version")
    target = expected["target"]
    if target not in ("x86_64-apple-darwin", "aarch64-apple-darwin",
                      "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"):
        raise RuntimeError("agent_release_invalid: invalid Unix target")
    directory = Path(stage)
    # Fail rather than reuse an existing directory or symlink.
    directory.mkdir(mode=0o700)
    success = False
    try:
        filename = f"codex-usage-monit-{target}.agent"
        base = RELEASES + "v" + expected["version"] + "/"
        receive_release_asset(base + filename + ".json", directory / "manifest.json", MAX_MANIFEST)
        try:
            manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
        except (ValueError, UnicodeError) as error:
            raise RuntimeError("agent_release_invalid: manifest is not valid UTF-8 JSON") from error
        if (not isinstance(manifest, dict)
                or set(manifest) != {"schemaVersion", "agent", "file", "size", "sha256"}
                or type(manifest["schemaVersion"]) is not int or manifest["schemaVersion"] != 1
                or manifest["file"] != filename
                or type(manifest["size"]) is not int or not 0 < manifest["size"] <= MAX_BINARY
                or not isinstance(manifest["sha256"], str)
                or not re.fullmatch(r"[a-f0-9]{64}", manifest["sha256"])):
            raise RuntimeError("agent_release_invalid: invalid official manifest")
        agent = manifest["agent"]
        if (not isinstance(agent, dict)
                or any(type(agent.get(key)) is not type(value) or agent.get(key) != value
                       for key, value in expected.items())):
            raise RuntimeError("agent_release_mismatch: official release does not match this center's source build, version, protocol and target; unpublished development builds require explicit deploy-dev")
        receive_release_asset(base + filename, directory / "agent", manifest["size"])
        binary = directory / "agent"
        if binary.stat().st_size != manifest["size"]:
            raise RuntimeError("agent_checksum_mismatch: official binary size differs")
        digest = hashlib.sha256()
        with binary.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != manifest["sha256"]:
            raise RuntimeError("agent_checksum_mismatch: official binary digest differs")
        # Execution permission is granted only after all download checks pass.
        binary.chmod(0o700)
        success = True
        return manifest
    finally:
        if not success:
            for name in ("agent", "manifest.json"):
                try:
                    (directory / name).unlink(missing_ok=True)
                except OSError:
                    pass
            try:
                directory.rmdir()
            except OSError:
                pass
