"""Publication must bind every native payload to one complete source identity."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("release_package", Path(__file__).resolve().parents[1] / "scripts/package-release.py")
PACKAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE)


class ReleasePackageTests(unittest.TestCase):
    def package(self, root, target, metadata_only=True, *, body=None, identity=None, binary=None):
        binary = binary or root / "input-binary"
        binary.write_bytes(body or ("native binary " + target).encode())
        info = {"schemaVersion": 1, "product": "codex-usage-monit", "version": "0.5.0",
                "target": target, "buildId": "1" * 64, "protocolVersion": 5,
                "executableSha256": PACKAGE.sha256(binary)}
        info.update(identity or {})
        with patch.object(PACKAGE.subprocess, "check_output", return_value=json.dumps(info).encode()):
            return PACKAGE.package(binary, root / "dist", metadata_only)

    def bundle_files(self, root):
        return {path.name: path.read_bytes() for path in (root / "dist").iterdir()}

    def package_all(self, root):
        for target in sorted(PACKAGE.TARGETS):
            self.package(root, target)

    def test_merges_all_platforms_into_one_identity_without_duplicate_agents(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.package_all(root)
            result = PACKAGE.merge(root / "dist")
            self.assertEqual({entry["target"] for entry in result["artifacts"]}, PACKAGE.TARGETS)
            self.assertEqual(result["buildId"], "1" * 64)
            self.assertFalse(list((root / "dist").glob("*.agent*")))
            self.assertEqual(len(list((root / "dist").glob("*.tar.gz"))), 4)
            self.assertEqual(len(list((root / "dist").glob("*.exe"))), 1)

    def test_rejects_incomplete_target_set(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.package(root, "aarch64-apple-darwin")
            with self.assertRaisesRegex(ValueError, "all five"):
                PACKAGE.merge(root / "dist")
            self.assertFalse((root / "dist/release-manifest.json").exists())

    def test_rejects_mixed_build_version_or_protocol(self):
        for key, value in (("buildId", "2" * 64), ("version", "0.6.0"), ("protocolVersion", 6)):
            with self.subTest(key=key), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.package_all(root)
                path = next((root / "dist").glob("release-metadata-*.json"))
                metadata = json.loads(path.read_text())
                metadata[key] = value
                path.write_text(json.dumps(metadata))
                with self.assertRaisesRegex(ValueError, "identities disagree"):
                    PACKAGE.merge(root / "dist")

    def test_rechecks_downloaded_ci_payloads_before_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.package_all(root)
            payload = next((root / "dist").glob("*.exe"))
            payload.write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                PACKAGE.merge(root / "dist")

    def test_rejects_duplicate_target_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.package_all(root)
            metadata = next((root / "dist").glob("release-metadata-*.json"))
            (root / "dist/release-metadata-duplicate.json").write_bytes(metadata.read_bytes())
            with self.assertRaisesRegex(ValueError, "Duplicate"):
                PACKAGE.merge(root / "dist")

    def test_development_bundle_combines_targets_under_the_shared_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.package(root, "aarch64-apple-darwin", metadata_only=False)
            result = self.package(root, "x86_64-pc-windows-msvc", metadata_only=False)
            self.assertEqual(len(result["artifacts"]), 2)
            self.assertFalse(list((root / "dist").glob("release-metadata-*.json")))

    def test_rejected_build_identity_preserves_the_existing_bundle(self):
        for target in ("aarch64-apple-darwin", "x86_64-pc-windows-msvc"):
            for key, value in (("buildId", "2" * 64), ("version", "0.6.0"), ("protocolVersion", 6)):
                with self.subTest(target=target, field=key), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    self.package(root, target, metadata_only=False)
                    before = self.bundle_files(root)
                    with self.assertRaisesRegex(ValueError, "identities disagree"):
                        self.package(root, target, metadata_only=False, body=b"another native build",
                                     identity={key: value})
                    self.assertEqual(self.bundle_files(root), before)

    def test_rechecks_existing_artifacts_before_replacing_any_payload(self):
        for corrupt_target in ("aarch64-apple-darwin", "x86_64-pc-windows-msvc"):
            with self.subTest(corrupt_target=corrupt_target), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                manifest = self.package(root, "aarch64-apple-darwin", metadata_only=False)
                manifest = self.package(root, "x86_64-pc-windows-msvc", metadata_only=False)
                corrupted = next(entry for entry in manifest["artifacts"] if entry["target"] == corrupt_target)
                (root / "dist" / corrupted["file"]).write_bytes(b"retain these invalid bytes")
                before = self.bundle_files(root)
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    self.package(root, "aarch64-apple-darwin", metadata_only=False, body=b"replacement")
                self.assertEqual(self.bundle_files(root), before)

    def test_failed_archive_staging_preserves_the_existing_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.package(root, "aarch64-apple-darwin", metadata_only=False)
            before = self.bundle_files(root)
            original_open = PACKAGE.tarfile.open

            def interrupted_archive(name, mode, **kwargs):
                if mode == "w:gz":
                    Path(name).write_bytes(b"incomplete archive")
                    raise OSError("archive staging failed")
                return original_open(name, mode, **kwargs)

            with patch.object(PACKAGE.tarfile, "open", side_effect=interrupted_archive):
                with self.assertRaisesRegex(OSError, "archive staging failed"):
                    self.package(root, "aarch64-apple-darwin", metadata_only=False, body=b"replacement")
            self.assertEqual(self.bundle_files(root), before)

    def test_failed_manifest_publication_restores_the_previous_payload(self):
        for existing_target in (True, False):
            with self.subTest(existing_target=existing_target), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.package(root, "aarch64-apple-darwin", metadata_only=False)
                target = "aarch64-apple-darwin" if existing_target else "x86_64-pc-windows-msvc"
                before = self.bundle_files(root)
                original_replace = Path.replace

                def fail_manifest(source, destination):
                    if Path(destination) == root / "dist/release-manifest.json":
                        raise OSError("manifest publication failed")
                    return original_replace(source, destination)

                with patch.object(Path, "replace", fail_manifest):
                    with self.assertRaisesRegex(OSError, "manifest publication failed"):
                        self.package(root, target, metadata_only=False, body=b"replacement")
                self.assertEqual(self.bundle_files(root), before)

    def test_windows_artifact_can_also_be_the_packaging_input(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "dist").mkdir()
            binary = root / "dist/codex-usage-monit-x86_64-pc-windows-msvc.exe"
            manifest = self.package(root, "x86_64-pc-windows-msvc", binary=binary)
            self.assertEqual(binary.read_bytes(), b"native binary x86_64-pc-windows-msvc")
            PACKAGE.validate_artifact(root / "dist", manifest["artifacts"][0])

    def test_switching_manifest_forms_preserves_existing_payload_references(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = "aarch64-apple-darwin"
            metadata = self.package(root, target)
            manifest = self.package(root, target, metadata_only=False)
            self.assertEqual(metadata["artifacts"], manifest["artifacts"])
            before = self.bundle_files(root)
            for metadata_only in (True, False):
                with self.subTest(metadata_only=metadata_only):
                    with self.assertRaisesRegex(ValueError, "referenced by other metadata"):
                        self.package(root, target, metadata_only, body=b"replacement")
                    self.assertEqual(self.bundle_files(root), before)


if __name__ == "__main__":
    unittest.main()
