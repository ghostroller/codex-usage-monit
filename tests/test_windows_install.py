"""Isolated Windows installer contracts: fixture bytes are never executed."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts/install.ps1"
VERIFIER = ROOT / "src/remote_agent_manager/release_bootstrap.ps1"
BODY = b"Offline installer fixture. This is not a Windows executable."


class InstallerSourceContracts(unittest.TestCase):
    def test_embedded_verifier_is_exact_shared_source(self):
        source = INSTALLER.read_text(encoding="utf-8")
        shared = source.split("# BEGIN SHARED RELEASE VERIFIER (src/remote_agent_manager/release_bootstrap.ps1)\n", 1)[1]
        shared = shared.split("\n# END SHARED RELEASE VERIFIER", 1)[0]
        self.assertEqual(shared, VERIFIER.read_text(encoding="utf-8").rstrip())
        self.assertNotIn("SetEnvironmentVariable", source)
        self.assertNotIn("setx ", source.lower())
        self.assertNotIn("Register-ScheduledTask", source)


@unittest.skipUnless(os.name == "nt", "Native Windows shell fixture contracts")
class WindowsInstallerContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.shells = [shutil.which("powershell.exe"), shutil.which("pwsh.exe")]
        if not all(cls.shells):
            raise AssertionError("Both Windows PowerShell 5.1 and PowerShell 7 are required")

    def exercise(self, shell, case):
        with tempfile.TemporaryDirectory(prefix="installer space ") as temporary:
            root = Path(temporary)
            bundle = root / "bundle 中文 'quoted'"
            bundle.mkdir()
            digest = hashlib.sha256(BODY).hexdigest()
            manifest = {"schemaVersion": 1, "product": "codex-usage-monit", "version": "0.5.1",
                        "buildId": "1" * 64, "protocolVersion": 5, "artifacts": [{
                            "target": "x86_64-pc-windows-msvc",
                            "file": "codex-usage-monit-x86_64-pc-windows-msvc.exe",
                            "size": len(BODY), "sha256": digest,
                            "binarySize": len(BODY), "binarySha256": digest}]}
            (bundle / "release-manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
            (bundle / manifest["artifacts"][0]["file"]).write_bytes(BODY if case != "tampered" else b"x" * len(BODY))
            script = root / "fixture.ps1"
            script.write_text(r'''
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
. $env:INSTALL_TEST_SCRIPT
$script:Calls = New-Object Collections.Generic.List[object]
function Invoke-VerifiedInstaller {
    param([string]$Candidate, [string[]]$Arguments)
    if (-not (Test-Path -LiteralPath $Candidate)) { throw 'fixture candidate missing' }
    $script:Calls.Add([pscustomobject]@{candidate=$Candidate; arguments=$Arguments})
    if ($env:INSTALL_TEST_CASE -eq 'candidate_failure' -and $Arguments[0] -eq 'install') {
        throw 'install_candidate_failed: native exit code 7'
    }
}
$version = if ($env:INSTALL_TEST_CASE -eq 'wrong_version') { 'v0.5.0' } else { 'v0.5.1' }
$failed = $false
$message = ''
try {
    Invoke-UserInstallation $version $env:INSTALL_TEST_BUNDLE $true 'on-logon' $true $true
} catch {
    $failed = $true
    $message = $_.Exception.Message
}
@{failed=$failed;message=$message;calls=@($script:Calls.ToArray());remaining=@(Get-ChildItem -LiteralPath ([IO.Path]::GetTempPath()) -Filter '.codex-usage-monit-release-*' | Select-Object -ExpandProperty Name)} | ConvertTo-Json -Depth 10 -Compress
''', encoding="utf-8-sig")
            environment = dict(os.environ, INSTALL_TEST_SCRIPT=str(INSTALLER),
                               INSTALL_TEST_BUNDLE=str(bundle), INSTALL_TEST_CASE=case,
                               TMP=str(root), TEMP=str(root))
            result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script)],
                                    capture_output=True, text=True, encoding="utf-8", errors="replace", env=environment, timeout=45)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            report = json.loads(result.stdout.strip())
            self.assertEqual(report["remaining"], [], "Private staging files leaked")
            self.assertEqual((bundle / manifest["artifacts"][0]["file"]).read_bytes(), BODY if case != "tampered" else b"x" * len(BODY))
            return report

    def test_both_shells_verify_offline_and_forward_options_without_mutation(self):
        for shell in self.shells:
            for case in ("success", "wrong_version", "tampered", "candidate_failure"):
                with self.subTest(shell=shell, case=case):
                    report = self.exercise(shell, case)
                    if case in ("wrong_version", "tampered"):
                        self.assertTrue(report["failed"])
                        self.assertEqual(report["calls"], [])
                    else:
                        self.assertEqual(len(report["calls"]), 2)
                        self.assertEqual(report["calls"][0]["arguments"][:2], ["update", "verify-release"])
                        self.assertEqual(report["calls"][1]["arguments"], ["install", "--current-binary", "--version", "0.5.1", "--recorder", "on-logon", "--no-modify-path", "--adopt", "--allow-dev-build"])
                        self.assertEqual(report["failed"], case == "candidate_failure")
                        if case == "candidate_failure":
                            self.assertIn("exit code 7", report["message"])

    def test_native_nonzero_exit_is_an_error_in_both_shells(self):
        for shell in self.shells:
            with self.subTest(shell=shell), tempfile.TemporaryDirectory() as temporary:
                script = Path(temporary) / "exit.ps1"
                script.write_text(r'''
$ErrorActionPreference = 'Stop'
. $env:INSTALL_TEST_SCRIPT
try {
    Invoke-VerifiedInstaller $env:INSTALL_TEST_SHELL @('-NoProfile','-NonInteractive','-Command','exit 7')
    exit 99
} catch {
    if ($_.Exception.Message -notmatch 'native exit code 7') { throw }
    if ($_.Exception.Data['NativeExitCode'] -ne 7) { throw 'native status was discarded' }
    exit 0
}
''', encoding="utf-8-sig")
                result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script)],
                                        env=dict(os.environ, INSTALL_TEST_SCRIPT=str(INSTALLER), INSTALL_TEST_SHELL=shell),
                                        capture_output=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_installer_entry_preserves_partial_exit_two_in_both_shells(self):
        # Exercise the exact shipped entry point after replacing only the
        # installation operation with a real child process that exits partial.
        entry = INSTALLER.read_text(encoding="utf-8").split("# Dot-sourcing exposes", 1)[1]
        entry = "# Dot-sourcing exposes" + entry
        for shell in self.shells:
            for code in (2, 7):
                with self.subTest(shell=shell, code=code), tempfile.TemporaryDirectory() as temporary:
                    script = Path(temporary) / "entry.ps1"
                    script.write_text(r'''
$ErrorActionPreference = 'Stop'
. $env:INSTALL_TEST_SCRIPT
function Invoke-UserInstallation {
    Invoke-VerifiedInstaller $env:INSTALL_TEST_SHELL @('-NoProfile','-NonInteractive','-Command', ('exit ' + $env:INSTALL_TEST_EXIT))
}
''' + entry, encoding="utf-8-sig")
                    result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script)],
                                            env=dict(os.environ, INSTALL_TEST_SCRIPT=str(INSTALLER), INSTALL_TEST_SHELL=shell, INSTALL_TEST_EXIT=str(code)),
                                            capture_output=True, timeout=30)
                    self.assertEqual(result.returncode, code, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
