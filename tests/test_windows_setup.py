"""Offline signing gate contracts; these do not prove real certificate signing."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
BUILDER = ROOT / "scripts/windows/build-user-setup.ps1"


@unittest.skipUnless(os.name == "nt", "Native Windows Authenticode shell contracts")
class WindowsSetupSigningContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.shells = [shutil.which("powershell.exe"), shutil.which("pwsh.exe")]
        if not all(cls.shells):
            raise AssertionError("Both Windows PowerShell 5.1 and PowerShell 7 are required")

    def exercise(self, shell, case, outer_wrapper):
        with tempfile.TemporaryDirectory(prefix="setup signing space ") as temporary:
            root = Path(temporary) / "中文 'quoted'"
            root.mkdir()
            payload = root / "payload.ps1"
            compiler = root / "compiler.ps1"
            signer = root / "signer.ps1"
            for path in (payload, compiler, signer):
                path.write_text("Set-Content -LiteralPath $env:SETUP_TEST_EXECUTED -Value $MyInvocation.MyCommand.Path\n",
                                encoding="utf-8-sig")
            driver = root / "driver.ps1"
            driver.write_text(r'''
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
if ($env:SETUP_TEST_CASE -ne 'actual_unsigned') {
    function Get-AuthenticodeSignature {
        param([string]$LiteralPath)
        $status = $env:SETUP_TEST_CASE
        $certificate = $null
        if ($status -eq 'wrong_publisher') {
            $status = 'Valid'
            $certificate = [pscustomobject]@{ Thumbprint = ('B' * 40) }
        }
        [pscustomobject]@{ Status=$status; SignerCertificate=$certificate }
    }
}
$failed = $false
$message = ''
try {
    & $env:SETUP_TEST_BUILDER -Binary $env:SETUP_TEST_PAYLOAD `
        -CertificateThumbprint ('A' * 40) -TimestampServer 'https://timestamp.invalid/' `
        -Iscc $env:SETUP_TEST_COMPILER -SignTool $env:SETUP_TEST_SIGNER `
        -OutputDirectory $env:SETUP_TEST_OUTPUT
} catch {
    $failed = $true
    $message = $_.Exception.Message
}
@{failed=$failed;message=$message;executed=(Test-Path -LiteralPath $env:SETUP_TEST_EXECUTED);outputCreated=(Test-Path -LiteralPath $env:SETUP_TEST_OUTPUT)} | ConvertTo-Json -Compress
exit 0
''', encoding="utf-8-sig")
            environment = dict(os.environ, SETUP_TEST_CASE=case,
                               SETUP_TEST_BUILDER=str(BUILDER), SETUP_TEST_PAYLOAD=str(payload),
                               SETUP_TEST_COMPILER=str(compiler), SETUP_TEST_SIGNER=str(signer),
                               SETUP_TEST_OUTPUT=str(root / "output"), SETUP_TEST_DRIVER=str(driver),
                               SETUP_TEST_EXECUTED=str(root / "executed.txt"))
            # Python does not apply PowerShell 7's child-5.1 compatibility
            # cleanup. Let each shell construct its own standard module path;
            # loading PS7 Security types into 5.1 otherwise fails before the
            # actual Authenticode check can run.
            for key in tuple(environment):
                if key.lower() == "psmodulepath":
                    del environment[key]
            invocation = ["-Command", "& $env:SETUP_TEST_DRIVER; exit $LASTEXITCODE"] if outer_wrapper else ["-File", str(driver)]
            completed = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", *invocation],
                                       env=environment, capture_output=True, text=True,
                                       encoding="utf-8", errors="replace", timeout=30)
            self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
            report = json.loads(completed.stdout.strip())
            self.assertTrue(report["failed"], report)
            self.assertIn("valid Authenticode signature from the selected publisher", report["message"])
            self.assertFalse(report["executed"], "An untrusted payload, compiler or signer was executed")
            self.assertFalse(report["outputCreated"], "The signing gate must fail before packaging starts")

    def test_real_unsigned_payload_is_rejected_before_execution_in_both_shells(self):
        for shell in self.shells:
            for outer_wrapper in (False, True):
                with self.subTest(shell=shell, outer_wrapper=outer_wrapper):
                    self.exercise(shell, "actual_unsigned", outer_wrapper)

    def test_invalid_status_and_wrong_publisher_are_rejected_in_both_shells(self):
        for shell in self.shells:
            for case in ("NotSigned", "HashMismatch", "NotTrusted", "wrong_publisher"):
                for outer_wrapper in (False, True):
                    with self.subTest(shell=shell, case=case, outer_wrapper=outer_wrapper):
                        self.exercise(shell, case, outer_wrapper)


if __name__ == "__main__":
    unittest.main()
