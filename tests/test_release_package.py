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
    def package(self, root, target, metadata_only=True):
        binary = root / "input-binary"
        binary.write_bytes(("native binary " + target).encode())
        info = {"schemaVersion": 1, "product": "codex-usage-monit", "version": "0.5.0",
                "target": target, "buildId": "1" * 64, "protocolVersion": 5,
                "executableSha256": PACKAGE.sha256(binary)}
        with patch.object(PACKAGE.subprocess, "check_output", return_value=json.dumps(info).encode()):
            return PACKAGE.package(binary, root / "dist", metadata_only)

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


if __name__ == "__main__":
    unittest.main()
