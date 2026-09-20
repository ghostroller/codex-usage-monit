#!/usr/bin/env python3
"""Package shared application payloads and merge verified release metadata."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile

TARGETS = {
    "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
}
MAX_BINARY = 128 * 1024 * 1024
IDENTITY = ("schemaVersion", "product", "version", "buildId", "protocolVersion")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def validate_identity(info):
    if (type(info.get("schemaVersion")) is not int or info["schemaVersion"] != 1
            or info.get("product") != "codex-usage-monit"
            or not isinstance(info.get("version"), str)
            or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.+-]+)?", info["version"])
            or not isinstance(info.get("buildId"), str)
            or not re.fullmatch(r"[0-9a-f]{64}", info["buildId"])
            or type(info.get("protocolVersion")) is not int or info["protocolVersion"] < 1):
        raise ValueError("Unsupported release identity")


def package(binary, output, metadata_only=False):
    binary = binary.resolve(strict=True)
    info = json.loads(subprocess.check_output(
        [str(binary), "remote-agent", "info", "--sha256"], timeout=30,
    ))
    size = binary.stat().st_size
    if not 0 < size <= MAX_BINARY:
        raise ValueError("Application binary must be between 1 byte and 128 MiB")
    digest = sha256(binary)
    if info.pop("executableSha256", None) != digest:
        raise ValueError("Application binary changed while being packaged")
    validate_identity(info)
    target = info.get("target")
    if target not in TARGETS:
        raise ValueError("Unsupported release target")
    output.mkdir(parents=True, exist_ok=True)
    suffix = ".exe" if "windows" in target else ".tar.gz"
    filename = f"codex-usage-monit-{target}{suffix}"
    destination = output / filename
    if suffix == ".exe":
        if binary != destination.resolve():
            shutil.copyfile(binary, destination)
    else:
        with tarfile.open(destination, "w:gz", format=tarfile.USTAR_FORMAT) as archive:
            member = tarfile.TarInfo("codex-usage-monit")
            member.size = size
            member.mode = 0o755
            with binary.open("rb") as stream:
                archive.addfile(member, stream)
    artifact = {"target": target, "file": filename, "size": destination.stat().st_size,
                "sha256": sha256(destination), "binarySize": size, "binarySha256": digest}
    manifest = {key: info[key] for key in IDENTITY}
    manifest["artifacts"] = [artifact]
    validate_artifact(output, artifact)
    name = f"release-metadata-{target}.json" if metadata_only else "release-manifest.json"
    if not metadata_only and (output / name).exists():
        previous = json.loads((output / name).read_text(encoding="utf-8"))
        if any(previous.get(key) != manifest[key] for key in IDENTITY):
            raise ValueError("Development bundle identities disagree; use an empty output directory")
        for existing in previous["artifacts"]:
            if existing["target"] != target:
                validate_artifact(output, existing)
                manifest["artifacts"].append(existing)
        manifest["artifacts"].sort(key=lambda entry: entry["target"])
    (output / name).write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Packaged {target} build {info['buildId']}: {output / name}")
    return manifest


def validate_artifact(directory, artifact):
    if (not isinstance(artifact, dict)
            or set(artifact) != {"target", "file", "size", "sha256", "binarySize", "binarySha256"}
            or artifact["target"] not in TARGETS):
        raise ValueError("Invalid release artifact")
    suffix = ".exe" if "windows" in artifact["target"] else ".tar.gz"
    if artifact["file"] != f"codex-usage-monit-{artifact['target']}{suffix}":
        raise ValueError("Invalid release artifact filename")
    for key in ("size", "binarySize"):
        if type(artifact[key]) is not int or not 0 < artifact[key] <= MAX_BINARY:
            raise ValueError("Invalid release artifact size")
    for key in ("sha256", "binarySha256"):
        if not isinstance(artifact[key], str) or not re.fullmatch(r"[0-9a-f]{64}", artifact[key]):
            raise ValueError("Invalid release artifact checksum")
    path = directory / artifact["file"]
    if not path.is_file() or path.is_symlink() or path.stat().st_size != artifact["size"] or sha256(path) != artifact["sha256"]:
        raise ValueError("Release artifact checksum mismatch")
    if suffix == ".exe":
        binary_size, binary_digest = artifact["size"], artifact["sha256"]
    else:
        with tarfile.open(path, "r:gz") as archive:
            member = archive.next()
            if (member is None or member.name != "codex-usage-monit" or not member.isfile()
                    or member.size != artifact["binarySize"] or member.pax_headers):
                raise ValueError("Unexpected release archive member")
            with archive.extractfile(member) as stream:
                data = stream.read(MAX_BINARY + 1)
            if archive.next() is not None:
                raise ValueError("Unexpected additional release archive member")
            binary_size, binary_digest = len(data), hashlib.sha256(data).hexdigest()
    if binary_size != artifact["binarySize"] or binary_digest != artifact["binarySha256"]:
        raise ValueError("Release binary checksum mismatch")


def merge(directory, require_all=True):
    manifests = [json.loads(path.read_text(encoding="utf-8"))
                 for path in sorted(directory.glob("release-metadata-*.json"))]
    if not manifests:
        raise ValueError("No release metadata to merge")
    result = {key: manifests[0].get(key) for key in IDENTITY}
    validate_identity(result)
    artifacts = {}
    for manifest in manifests:
        if set(manifest) != {*IDENTITY, "artifacts"} or any(manifest.get(key) != result[key] or type(manifest.get(key)) is not type(result[key]) for key in IDENTITY):
            raise ValueError("Release target identities disagree")
        if not isinstance(manifest["artifacts"], list) or len(manifest["artifacts"]) != 1:
            raise ValueError("Expected one artifact per build")
        artifact = manifest["artifacts"][0]
        validate_artifact(directory, artifact)
        if artifact["target"] in artifacts:
            raise ValueError("Duplicate release target")
        artifacts[artifact["target"]] = artifact
    if require_all and set(artifacts) != TARGETS:
        raise ValueError("Release must include all five supported targets")
    result["artifacts"] = [artifacts[key] for key in sorted(artifacts)]
    (directory / "release-manifest.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("package")
    build.add_argument("binary", type=Path)
    build.add_argument("--output-dir", type=Path, default=Path("dist"))
    build.add_argument("--metadata-only", action="store_true")
    combine = commands.add_parser("merge")
    combine.add_argument("--output-dir", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.command == "package":
        package(args.binary, args.output_dir, args.metadata_only)
    else:
        merge(args.output_dir)
