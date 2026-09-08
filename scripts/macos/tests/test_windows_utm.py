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
    @staticmethod
    def engine_evidence():
        return [
            dict(engine="windows-powershell", version="5.1.26100.1", executable="C:\\Windows\\powershell.exe", status="passed", casesPassed=60),
            dict(engine="powershell-7", version="7.6.2", executable="C:\\Program Files\\PowerShell\\7\\pwsh.exe", status="passed", casesPassed=60),
        ]

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

    def test_shell_contract_pass_requires_scope_and_two_complete_distinct_engines(self):
        valid = dict(schemaVersion=1, runId="current", sourceArchiveSha256="sha", status="passed",
                     scope="shell-contracts", engines=self.engine_evidence())
        self.assertEqual(runner.parse_result(json.dumps(valid).encode(), "current", "sha", "shell-contracts"), valid)
        invalid = [dict(scope="full"), dict(engines=[]), dict(engines=[valid["engines"][0]]),
                   dict(engines=[valid["engines"][0], valid["engines"][0]]), dict(engines=[None, None])]
        for mutation in [dict(version="5.1.26100.1"), dict(casesPassed=59), dict(status="failed"),
                         dict(executable=""), dict(engine="unexpected")]:
            invalid.append(dict(engines=[valid["engines"][0], valid["engines"][1] | mutation]))
        for mutation in invalid:
            with self.subTest(mutation=mutation), self.assertRaises(runner.GuestError):
                runner.parse_result(json.dumps(valid | mutation).encode(), "current", "sha", "shell-contracts")
        blocked = valid | dict(status="blocked", engines=[], detail="PowerShell 7 is unavailable")
        self.assertEqual(runner.parse_result(json.dumps(blocked).encode(), "current", "sha", "shell-contracts"), blocked)

    def test_shell_contract_cli_rejects_ambiguous_scope_before_guest_access(self):
        path = "C:\\Program Files\\PowerShell\\7\\pwsh.exe"
        invalid = [["--shell-contracts"], ["--pwsh-path", path]]
        for extra in [["--doctor"], ["--focused", "--test-filter", "test"], ["--test-filter", "test"],
                      ["--target", "aarch64-pc-windows-msvc"], ["--profile", "release"]]:
            invalid.append(["--shell-contracts", "--pwsh-path", path] + extra)
        with patch.object(runner, "command") as command, patch("sys.stderr", new_callable=io.StringIO):
            for arguments in invalid:
                with self.subTest(arguments=arguments), self.assertRaises(SystemExit) as error:
                    runner.main(arguments)
                self.assertEqual(error.exception.code, 2)
            command.assert_not_called()

    def test_readiness_cannot_substitute_for_a_test_pass(self):
        for scope in ["full", "filtered", "rust-focused", "shell-contracts"]:
            result = dict(schemaVersion=1, runId="current", sourceArchiveSha256="sha", scope=scope, status="ready")
            with self.subTest(scope=scope), self.assertRaises(runner.GuestError):
                runner.parse_result(json.dumps(result).encode(), "current", "sha", scope)
        result = dict(schemaVersion=1, runId="current", sourceArchiveSha256="sha", scope="doctor", status="passed")
        with self.assertRaises(runner.GuestError):
            runner.parse_result(json.dumps(result).encode(), "current", "sha", "doctor")

    def test_guest_request_preserves_existing_modes_and_scopes_shell_contracts(self):
        path = "C:\\Program Files\\PowerShell\\7\\pwsh.exe"
        cases = [([], "full"), (["--doctor"], "doctor"),
                 (["--test-filter", "test"], "filtered"),
                 (["--focused", "--test-filter", "test"], "rust-focused"),
                 (["--shell-contracts", "--pwsh-path", path], "shell-contracts")]
        for flags, scope in cases:
            with self.subTest(scope=scope), tempfile.TemporaryDirectory() as directory:
                requests = []

                def archive(repository, destination):
                    destination.write_bytes(b"source snapshot")
                    return "archive-hash"

                def command(arguments, **kwargs):
                    if arguments[0] == "git":
                        return b"" if "status" in arguments else b"a" * 40 + b"\n"
                    if arguments[1] == "status":
                        return b"started\n"
                    if arguments[1:3] == ["file", "push"] and arguments[-1].endswith(".config.json"):
                        requests.append(json.loads(kwargs["data"]))
                    if arguments[1:3] == ["file", "pull"]:
                        if arguments[-1].endswith(".result.json"):
                            request = requests[-1]
                            result = dict(schemaVersion=1, runId=request["runId"], scope=request["scope"],
                                          sourceArchiveSha256=request["sourceArchiveSha256"],
                                          status="ready" if scope == "doctor" else "passed", logPath="verify.log")
                            if scope == "shell-contracts":
                                result["engines"] = self.engine_evidence()
                            return json.dumps(result).encode()
                        return b"verification transcript"
                    return b""

                with patch.object(runner.shutil, "which", return_value="utmctl"), patch.object(runner, "command", side_effect=command), \
                        patch.object(runner, "source_archive", side_effect=archive), patch("sys.stdout", new_callable=io.StringIO):
                    self.assertEqual(runner.main(flags + ["--output-dir", directory]), 0)
                self.assertEqual(len(requests), 1)
                self.assertEqual(requests[0]["scope"], scope)
                self.assertEqual(requests[0]["mode"], "doctor" if scope == "doctor" else "verify")
                self.assertEqual(requests[0]["pwshPath"], path if scope == "shell-contracts" else "")
                self.assertEqual(requests[0]["focused"], scope == "rust-focused")

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
