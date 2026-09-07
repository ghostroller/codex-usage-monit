import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("windows_utm", Path(__file__).parents[1] / "test-windows-utm.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class WindowsRunnerTests(unittest.TestCase):
    def test_zero_transport_exit_with_utm_event_error_is_failure(self):
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"Error from event: failed", b"")):
            with self.assertRaises(runner.GuestError):
                runner.command(["utmctl", "file", "pull"])

    def test_result_requires_matching_nonce_source_and_known_status(self):
        valid = dict(schemaVersion=1, runId="current", sourceArchiveSha256="sha", status="passed")
        self.assertEqual(runner.parse_result(json.dumps(valid).encode(), "current", "sha"), valid)
        for mutation in [dict(runId="stale"), dict(sourceArchiveSha256="old"), dict(status="unknown")]:
            with self.assertRaises(runner.GuestError):
                runner.parse_result(json.dumps(valid | mutation).encode(), "current", "sha")
        for malformed in [b"", b"[]", b"null", b"0"]:
            with self.assertRaises(runner.GuestError):
                runner.parse_result(malformed, "current", "sha")

    def test_passing_verification_requires_a_nonempty_retrieved_log(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory)
            for log_path in [None, "", " ", 1]:
                with self.assertRaises(runner.GuestError):
                    runner.retrieve_log("utmctl", "vm", dict(status="passed", logPath=log_path), run_dir)
            for log in [b"", b" \r\n"]:
                with patch.object(runner, "command", return_value=log):
                    with self.assertRaises(runner.GuestError):
                        runner.retrieve_log("utmctl", "vm", dict(status="passed", logPath="verify.log"), run_dir)
            with patch.object(runner, "command", return_value=b"test result: ok"):
                runner.retrieve_log("utmctl", "vm", dict(status="passed", logPath="verify.log"), run_dir)
            self.assertEqual((run_dir / "verify.log").read_bytes(), b"test result: ok")

    def test_unavailable_vm_leaves_a_machine_readable_blocked_result(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(runner.shutil, "which", return_value="utmctl"), patch.object(runner, "command", return_value=b"stopped\n"), patch("sys.stdout", new_callable=io.StringIO):
                with self.assertRaises(runner.GuestError):
                    runner.main(["--doctor", "--output-dir", directory])
            results = list(Path(directory).glob("*/result.json"))
            self.assertEqual(len(results), 1)
            result = json.loads(results[0].read_text())
            self.assertEqual(result["status"], "blocked")
            self.assertEqual(result["runId"], results[0].parent.name)
            self.assertIn("stopped", result["detail"])

    def test_snapshot_contains_dirty_and_untracked_source_but_excludes_ignored_state(self):
        with tempfile.TemporaryDirectory() as root:
            repository = Path(root) / "repository"
            repository.mkdir()
            subprocess.run(["git", "init", "-q", str(repository)], check=True)
            (repository / ".gitignore").write_text("local-secret\n")
            (repository / "tracked").write_text("old")
            (repository / "deleted").write_text("old")
            subprocess.run(["git", "-C", str(repository), "add", "."], check=True)
            (repository / "tracked").write_text("dirty")
            (repository / "deleted").unlink()
            (repository / "new source").write_text("new")
            (repository / "local-secret").write_text("do not copy")
            archive_path = Path(root) / "source.zip"
            self.assertEqual(len(runner.source_archive(repository, archive_path)), 64)
            with zipfile.ZipFile(archive_path) as archive:
                self.assertEqual(archive.read("tracked"), b"dirty")
                self.assertEqual(archive.read("new source"), b"new")
                self.assertNotIn("deleted", archive.namelist())
                self.assertNotIn("local-secret", archive.namelist())
                self.assertFalse(any(name.startswith(".git/") for name in archive.namelist()))

    def test_snapshot_rejects_windows_path_traversal_and_symlinks(self):
        with tempfile.TemporaryDirectory() as root:
            repository = Path(root)
            with patch.object(runner, "command", return_value=b"..\\escape\0"):
                with self.assertRaises(runner.GuestError):
                    runner.source_archive(repository, repository / "archive.zip")
            (repository / "link").symlink_to(repository / "missing")
            with patch.object(runner, "command", return_value=b"link\0"):
                with self.assertRaises(runner.GuestError):
                    runner.source_archive(repository, repository / "archive.zip")


if __name__ == "__main__":
    unittest.main()
