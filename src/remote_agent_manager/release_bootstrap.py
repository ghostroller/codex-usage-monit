"""Prepare an official release on the SSH host without executing its binary."""
import gzip
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile

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


def validate_release_manifest(manifest, expected):
    if (not isinstance(manifest, dict)
            or set(manifest) != {"schemaVersion", "product", "version", "buildId", "protocolVersion", "artifacts"}
            or type(manifest["schemaVersion"]) is not int or manifest["schemaVersion"] != 1
            or manifest["product"] != "codex-usage-monit"
            or not isinstance(manifest["version"], str)
            or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.+-]+)?", manifest["version"])
            or not isinstance(manifest["buildId"], str) or not re.fullmatch(r"[a-f0-9]{64}", manifest["buildId"])
            or type(manifest["protocolVersion"]) is not int or manifest["protocolVersion"] < 1
            or not isinstance(manifest["artifacts"], list) or not 1 <= len(manifest["artifacts"]) <= 5):
        raise RuntimeError("agent_release_invalid: invalid official manifest")
    targets = set()
    selected = None
    for artifact in manifest["artifacts"]:
        if (not isinstance(artifact, dict)
                or set(artifact) != {"target", "file", "size", "sha256", "binarySize", "binarySha256"}
                or artifact["target"] not in ("x86_64-apple-darwin", "aarch64-apple-darwin",
                    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "x86_64-pc-windows-msvc")):
            raise RuntimeError("agent_release_invalid: invalid release artifact")
        suffix = ".exe" if "windows" in artifact["target"] else ".tar.gz"
        if (artifact["target"] in targets
                or artifact["file"] != "codex-usage-monit-" + artifact["target"] + suffix
                or any(type(artifact[key]) is not int or not 0 < artifact[key] <= MAX_BINARY
                       for key in ("size", "binarySize"))
                or any(not isinstance(artifact[key], str) or not re.fullmatch(r"[a-f0-9]{64}", artifact[key])
                       for key in ("sha256", "binarySha256"))):
            raise RuntimeError("agent_release_invalid: invalid release artifact fields")
        targets.add(artifact["target"])
        if artifact["target"] == expected["target"]:
            selected = artifact
    if selected is None:
        raise RuntimeError("agent_release_mismatch: release lacks the requested platform")
    agent = {key: manifest[key] for key in ("schemaVersion", "product", "version", "buildId", "protocolVersion")}
    agent["target"] = selected["target"]
    for key, value in expected.items():
        if key == "version" and value == "latest":
            continue
        if type(agent.get(key)) is not type(value) or agent.get(key) != value:
            raise RuntimeError("agent_release_mismatch: official release does not match this center's source build, version, protocol and target; unpublished development builds require explicit deploy-dev")
    return selected, {"schemaVersion": 1, "agent": agent, "file": selected["file"],
                      "size": selected["binarySize"], "sha256": selected["binarySha256"]}


def verify_download(path, size, digest):
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(block)
    if path.stat().st_size != size or hasher.hexdigest() != digest:
        raise RuntimeError("agent_checksum_mismatch: official payload differs from manifest")


def unpack_binary(payload, binary, size):
    # Decompression itself is bounded, including padding and extension headers.
    raw = payload.with_name("unpacked.tar")
    try:
        remaining = size + 64 * 1024
        with gzip.open(payload, "rb") as source, raw.open("xb") as destination:
            while True:
                block = source.read(min(1024 * 1024, remaining + 1))
                if not block:
                    break
                remaining -= len(block)
                if remaining < 0:
                    raise RuntimeError("agent_release_invalid: archive exceeds expanded size limit")
                destination.write(block)
        with tarfile.open(raw, "r:") as archive:
            member = archive.next()
            if (member is None or member.name != "codex-usage-monit"
                    or member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE)
                    or member.size != size or member.offset != 0 or member.offset_data != 512
                    or member.pax_headers or member.linkname):
                raise RuntimeError("agent_release_invalid: archive must contain one regular application binary")
            with archive.extractfile(member) as source, binary.open("xb") as destination:
                while True:
                    block = source.read(1024 * 1024)
                    if not block:
                        break
                    destination.write(block)
            if archive.next() is not None:
                raise RuntimeError("agent_release_invalid: unexpected archive members")
            end = member.offset_data + ((size + 511) // 512) * 512
            if raw.stat().st_size % 512 or raw.stat().st_size < end + 1024:
                raise RuntimeError("agent_release_invalid: truncated archive padding")
        with raw.open("rb") as stream:
            stream.seek(member.offset_data + size)
            if any(stream.read()):
                raise RuntimeError("agent_release_invalid: unexpected archive trailing data")
    except (OSError, EOFError, tarfile.TarError) as error:
        raise RuntimeError("agent_release_invalid: malformed release archive") from error
    finally:
        raw.unlink(missing_ok=True)


def prepare_release(expected, stage):
    if not re.fullmatch(r"\.codex-usage-monit-release-[a-f0-9]{32}", stage):
        raise RuntimeError("agent_release_invalid: invalid staging directory")
    if not re.fullmatch(r"[A-Za-z0-9.+-]{1,80}", expected["version"]):
        raise RuntimeError("agent_release_invalid: invalid release version")
    if expected["target"] not in ("x86_64-apple-darwin", "aarch64-apple-darwin",
                                  "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"):
        raise RuntimeError("agent_release_invalid: invalid Unix target")
    directory = Path(stage)
    directory.mkdir(mode=0o700)
    success = False
    try:
        base = (RELEASES.replace("download/", "latest/download/") if expected["version"] == "latest"
                else RELEASES + "v" + expected["version"] + "/")
        receive_release_asset(base + "release-manifest.json", directory / "manifest.json", MAX_MANIFEST)
        try:
            manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
        except (ValueError, UnicodeError) as error:
            raise RuntimeError("agent_release_invalid: manifest is not valid UTF-8 JSON") from error
        artifact, normalized = validate_release_manifest(manifest, expected)
        # Pin the payload to the inspected version even if latest moves meanwhile.
        base = RELEASES + "v" + normalized["agent"]["version"] + "/"
        payload = directory / "payload.tar.gz"
        receive_release_asset(base + artifact["file"], payload, artifact["size"])
        verify_download(payload, artifact["size"], artifact["sha256"])
        binary = directory / "agent"
        unpack_binary(payload, binary, normalized["size"])
        verify_download(binary, normalized["size"], normalized["sha256"])
        payload.unlink()
        binary.chmod(0o700)
        success = True
        return normalized
    finally:
        if not success:
            for name in ("agent", "manifest.json", "payload.tar.gz", "unpacked.tar"):
                try:
                    (directory / name).unlink(missing_ok=True)
                except OSError:
                    pass
            try:
                directory.rmdir()
            except OSError:
                pass
