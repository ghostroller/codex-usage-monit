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
import tempfile

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
    manifest = {key: info[key] for key in IDENTITY}
    name = f"release-metadata-{target}.json" if metadata_only else "release-manifest.json"
    existing_artifacts = []
    target_references = []
    # Both manifest forms can refer to the payload being replaced. Validate
    # all existing references before writing any published file, including the
    # same target; a rejected build must leave the previous bundle usable.
    for previous_name in ("release-manifest.json", f"release-metadata-{target}.json"):
        previous_path = output / previous_name
        if previous_path.is_symlink():
            raise ValueError("Existing release metadata must be an ordinary file")
        if not previous_path.exists():
            continue
        previous = json.loads(previous_path.read_text(encoding="utf-8"))
        validate_identity(previous)
        if (set(previous) != {*IDENTITY, "artifacts"}
                or not isinstance(previous["artifacts"], list)
                or not 1 <= len(previous["artifacts"]) <= len(TARGETS)):
            raise ValueError("Invalid existing release metadata")
        if any(previous.get(key) != manifest[key] or type(previous.get(key)) is not type(manifest[key])
               for key in IDENTITY):
            raise ValueError("Development bundle identities disagree; use an empty output directory")
        seen = set()
        for existing in previous["artifacts"]:
            validate_artifact(output, existing)
            if existing["target"] in seen:
                raise ValueError("Duplicate release target")
            seen.add(existing["target"])
            if previous_name.startswith("release-metadata-") and (
                    len(previous["artifacts"]) != 1 or existing["target"] != target):
                raise ValueError("Expected one matching artifact in existing target metadata")
            if existing["target"] == target:
                target_references.append((previous_name, existing))
            if previous_name == name and existing["target"] != target:
                existing_artifacts.append(existing)
    if destination.is_symlink() or (destination.exists() and not destination.is_file()):
        raise ValueError("Existing release payload must be an ordinary file")

    reusable_artifact = next((artifact for _, artifact in target_references
                              if artifact["binarySize"] == size and artifact["binarySha256"] == digest), None)
    if reusable_artifact is None and any(reference != name for reference, _ in target_references):
        raise ValueError("Release payload is referenced by other metadata; use an empty output directory")

    staging = Path(tempfile.mkdtemp(prefix=".release-package-", dir=output))
    preserve_staging = False
    try:
        payload = staging / filename
        if reusable_artifact is not None:
            # Retain the archive bytes/hash when the native binary is unchanged,
            # including when another manifest also references this payload.
            shutil.copyfile(destination, payload)
        elif suffix == ".exe":
            shutil.copyfile(binary, payload)
        else:
            with tarfile.open(payload, "w:gz", format=tarfile.USTAR_FORMAT) as archive:
                member = tarfile.TarInfo("codex-usage-monit")
                member.size = size
                member.mode = 0o755
                with binary.open("rb") as stream:
                    archive.addfile(member, stream)
        artifact = {"target": target, "file": filename, "size": payload.stat().st_size,
                    "sha256": sha256(payload), "binarySize": size, "binarySha256": digest}
        validate_artifact(staging, artifact)
        manifest["artifacts"] = sorted([artifact, *existing_artifacts], key=lambda entry: entry["target"])
        metadata = staging / name
        metadata.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

        # Stage on the same filesystem so each publication is an atomic rename.
        # The two files cannot be renamed together: retain the old payload for
        # rollback if publishing its manifest fails. A Windows release pipeline
        # may already have staged the input at its final destination; keep that
        # file in place and publish only its metadata.
        payload_unchanged = reusable_artifact is not None or (
            suffix == ".exe" and binary == destination.resolve())
        backup = staging / "previous-payload"
        if not payload_unchanged and destination.exists():
            shutil.copy2(destination, backup)
        payload_published = False
        try:
            if not payload_unchanged:
                payload.replace(destination)
                payload_published = True
            metadata.replace(output / name)
        except BaseException as error:
            if payload_published:
                try:
                    if backup.exists():
                        backup.replace(destination)
                    else:
                        destination.unlink()
                except OSError as rollback_error:
                    preserve_staging = True
                    raise RuntimeError(
                        f"Package publication failed and payload recovery failed: {rollback_error}; "
                        f"retained staging and previous payload: {staging}"
                    ) from error
            raise
    finally:
        if not preserve_staging:
            shutil.rmtree(staging, ignore_errors=True)
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
