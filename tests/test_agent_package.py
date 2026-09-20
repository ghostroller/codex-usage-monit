import importlib.util
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("agent_package", Path(__file__).resolve().parents[1] / "scripts/package-agent.py")
PACKAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE)


class AgentPackageTests(unittest.TestCase):
    def test_manifest_binds_platform_build_and_actual_binary_bytes(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            binary = root / "monit.exe"
            binary.write_bytes(b"native executable")
            info = {"schemaVersion": 1, "product": "codex-usage-monit", "version": "0.4.0",
                    "target": "x86_64-pc-windows-msvc", "buildId": "1" * 64,
                    "protocolVersion": 5, "executableSha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
            with patch.object(PACKAGE.subprocess, "check_output", return_value=json.dumps(info).encode()) as run:
                manifest = PACKAGE.package(binary, root / "agents")
            self.assertEqual(run.call_args.args[0][1:], ["remote-agent", "info", "--sha256"])
            self.assertEqual(manifest["buildId"], info["buildId"])
            self.assertEqual((root / "agents" / manifest["artifacts"][0]["file"]).read_bytes(), binary.read_bytes())
            self.assertNotIn("executableSha256", manifest)
            self.assertFalse(any((root / "agents").glob("*.agent*")))

    def test_rejects_a_binary_changed_while_inspecting_it(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            binary = root / "monit.exe"
            binary.write_bytes(b"changed")
            info = {"schemaVersion": 1, "product": "codex-usage-monit", "target": "x86_64-pc-windows-msvc",
                    "buildId": "1" * 64, "executableSha256": "0" * 64}
            with patch.object(PACKAGE.subprocess, "check_output", return_value=json.dumps(info).encode()):
                with self.assertRaisesRegex(ValueError, "changed"):
                    PACKAGE.package(binary, root / "agents")
            self.assertFalse((root / "agents").exists())


if __name__ == "__main__":
    unittest.main()
