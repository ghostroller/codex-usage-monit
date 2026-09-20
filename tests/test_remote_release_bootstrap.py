"""Run the remote preparation logic locally; downloaded fixtures never execute."""
import copy
import gzip
import hashlib
import io
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "src/remote_agent_manager/release_bootstrap.py"
SPEC = importlib.util.spec_from_file_location("release_bootstrap", SCRIPT)
BOOT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BOOT)
STAGE = ".codex-usage-monit-release-" + "a" * 32
BODY = b"This is data, not an executable."


def archive_bytes(body=BODY, name="codex-usage-monit", member_type=tarfile.REGTYPE, extra=False):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz", format=tarfile.USTAR_FORMAT) as archive:
        member = tarfile.TarInfo(name)
        member.size = len(body) if member_type == tarfile.REGTYPE else 0
        member.type = member_type
        if member_type in (tarfile.LNKTYPE, tarfile.SYMTYPE):
            member.linkname = "outside"
        archive.addfile(member, io.BytesIO(body))
        if extra:
            archive.addfile(tarfile.TarInfo("extra"), io.BytesIO())
    return output.getvalue()


def fixtures(target="aarch64-apple-darwin", payload=None):
    expected = {"schemaVersion": 1, "product": "codex-usage-monit", "version": "0.4.0",
                "buildId": "1" * 64, "target": target, "protocolVersion": 5}
    payload = payload if payload is not None else (BODY if "windows" in target else archive_bytes())
    filename = f"codex-usage-monit-{target}" + (".exe" if "windows" in target else ".tar.gz")
    manifest = {key: value for key, value in expected.items() if key != "target"}
    manifest["artifacts"] = [{"target": target, "file": filename,
        "size": len(payload), "sha256": hashlib.sha256(payload).hexdigest(),
        "binarySize": len(BODY), "binarySha256": hashlib.sha256(BODY).hexdigest()}]
    return expected, manifest


class UnixReleaseBootstrapTests(unittest.TestCase):
    def exercise(self, mutation=None, binary=None, expected_override=None, fixture_payload=None):
        expected, manifest = fixtures(payload=fixture_payload)
        if expected_override is not None:
            expected = expected_override
        if binary is None:
            binary = fixture_payload if fixture_payload is not None else archive_bytes()
        if mutation:
            mutation(manifest)
        calls = []

        def receive(url, destination, maximum):
            calls.append(url)
            if url.endswith(".json"):
                destination.write_text(json.dumps(manifest), encoding="utf-8")
            else:
                destination.write_bytes(binary)

        with tempfile.TemporaryDirectory() as root:
            previous = Path.cwd()
            try:
                os.chdir(root)
                with patch.object(BOOT, "receive_release_asset", side_effect=receive), \
                     patch.object(BOOT.subprocess, "run", side_effect=AssertionError("Candidate executed")):
                    try:
                        result = BOOT.prepare_release(expected, STAGE)
                    except Exception as error:
                        self.assertFalse(Path(STAGE).exists(), "Failed preparation left staging files")
                        return error, calls
                    self.assertEqual(result["sha256"], manifest["artifacts"][0]["binarySha256"])
                    self.assertEqual(result["agent"]["version"], manifest["version"])
                    self.assertEqual(set(result), {"schemaVersion", "agent", "file", "size", "sha256"})
                    self.assertEqual((Path(STAGE) / "agent").read_bytes(), BODY)
                    return None, calls
            finally:
                os.chdir(previous)

    def test_prepares_matching_release_without_executing_it(self):
        error, calls = self.exercise()
        self.assertIsNone(error)
        base = "https://github.com/ghostroller/codex-usage-monit/releases/download/v0.4.0/"
        self.assertEqual(calls, [base + "release-manifest.json",
                                 base + "codex-usage-monit-aarch64-apple-darwin.tar.gz"])

    def test_rejects_metadata_mismatch_before_downloading_binary(self):
        for field, value in (("buildId", "2" * 64), ("version", "0.3.0"), ("protocolVersion", 4)):
            with self.subTest(field=field):
                error, calls = self.exercise(lambda m: m.update({field: value}))
                self.assertIn("agent_release_mismatch", str(error))
                self.assertEqual(len(calls), 1)

    def test_rejects_paths_oversize_and_malformed_hash(self):
        for field, value in (("file", "../other"), ("size", 0), ("size", 134217729),
                             ("sha256", "bad"), ("binarySize", True), ("binarySha256", "bad")):
            with self.subTest(field=field):
                error, calls = self.exercise(lambda m: m["artifacts"][0].update({field: value}))
                self.assertIn("agent_release_invalid", str(error))
                self.assertEqual(len(calls), 1)

    def test_rejects_tampered_binary_and_cleans_staging(self):
        error, calls = self.exercise(binary=b"x" * len(BODY))
        self.assertIn("agent_checksum_mismatch", str(error))
        self.assertEqual(len(calls), 2)

    def test_local_latest_pins_payload_to_manifest_version(self):
        error, calls = self.exercise(expected_override={"version": "latest", "target": "aarch64-apple-darwin"})
        self.assertIsNone(error)
        self.assertIn("/releases/latest/download/release-manifest.json", calls[0])
        self.assertIn("/releases/download/v0.4.0/", calls[1])

    def test_rejects_binary_hash_disagreement(self):
        error, calls = self.exercise(lambda m: m["artifacts"][0].update(binarySha256="0" * 64))
        self.assertIn("agent_checksum_mismatch", str(error))
        self.assertEqual(len(calls), 2)

    def test_rejects_duplicate_targets(self):
        error, calls = self.exercise(lambda m: m["artifacts"].append(copy.deepcopy(m["artifacts"][0])))
        self.assertIn("agent_release_invalid", str(error))
        self.assertEqual(len(calls), 1)

    def test_rejects_unsafe_tar_members_even_with_correct_payload_hash(self):
        for options in ({"name": "../codex-usage-monit"}, {"member_type": tarfile.SYMTYPE},
                        {"member_type": tarfile.LNKTYPE}, {"extra": True}):
            with self.subTest(options=options):
                error, calls = self.exercise(fixture_payload=archive_bytes(**options))
                self.assertIn("agent_release_invalid", str(error))
                self.assertEqual(len(calls), 2)

    def test_rejects_nonzero_padding_and_missing_end_markers(self):
        raw = bytearray(gzip.decompress(archive_bytes()))
        nonzero_padding = bytearray(raw)
        nonzero_padding[512 + len(BODY)] = 1
        for tar in (raw[:1024], raw[:-1], nonzero_padding):
            with self.subTest(size=len(tar)):
                error, calls = self.exercise(fixture_payload=gzip.compress(bytes(tar)))
                self.assertIn("agent_release_invalid", str(error))
                self.assertEqual(len(calls), 2)

    def test_bounds_archive_decompression(self):
        bomb = gzip.compress(b"\0" * (2 * 1024 * 1024))
        error, _ = self.exercise(fixture_payload=bomb)
        self.assertIn("expanded size limit", str(error))

    def test_missing_release_fails_closed_and_download_uses_https_limits(self):
        response = subprocess.CompletedProcess([], 22, b"404", b"not found")
        with patch.object(BOOT.subprocess, "run", return_value=response) as run:
            with self.assertRaisesRegex(RuntimeError, "agent_release_unavailable"):
                BOOT.receive_release_asset(BOOT.RELEASES + "v0.4.0/manifest", Path("unused"), 32768)
        argv = run.call_args.args[0]
        self.assertEqual(argv[argv.index("--proto") + 1], "=https")
        self.assertEqual(argv[argv.index("--proto-redir") + 1], "=https")
        self.assertEqual(argv[argv.index("--max-filesize") + 1], "32768")
        self.assertEqual(run.call_args.kwargs["timeout"], 125)

    def test_does_not_reuse_existing_staging_or_accept_path_injection(self):
        expected, _ = fixtures()
        for name in ("../outside", ".codex-usage-monit-release-" + "A" * 32):
            with self.assertRaisesRegex(RuntimeError, "invalid staging"):
                BOOT.prepare_release(expected, name)
        with tempfile.TemporaryDirectory() as root:
            previous = Path.cwd()
            try:
                os.chdir(root)
                Path(STAGE).mkdir()
                marker = Path(STAGE) / "keep"
                marker.write_text("keep")
                with self.assertRaises(FileExistsError):
                    BOOT.prepare_release(expected, STAGE)
                self.assertEqual(marker.read_text(), "keep")
            finally:
                os.chdir(previous)


@unittest.skipUnless(os.name == "nt", "Native Windows PowerShell contracts")
class WindowsReleaseBootstrapTests(unittest.TestCase):
    def test_both_shells_prepare_and_reject_invalid_release_without_execution(self):
        shells = [shutil.which("powershell.exe"), shutil.which("pwsh.exe")]
        if not all(shells):
            self.fail("Both Windows PowerShell 5.1 and PowerShell 7 are required")
        source = str(ROOT / "src/remote_agent_manager/release_bootstrap.ps1").replace("'", "''")
        for shell in shells:
            for case in ("success", "latest", "missing", "wrong_build", "wrong_protocol", "wrong_version", "bad_path", "tampered", "oversize", "duplicate", "binary_identity", "schema_bool"):
                with self.subTest(shell=shell, case=case), tempfile.TemporaryDirectory(prefix="release preparation ") as root:
                    root = Path(root)
                    expected, manifest = fixtures("x86_64-pc-windows-msvc")
                    if case == "latest":
                        expected = {"target": expected["target"], "version": "latest"}
                    elif case == "wrong_build":
                        manifest["buildId"] = "2" * 64
                    elif case == "wrong_protocol":
                        manifest["protocolVersion"] = 4
                    elif case == "wrong_version":
                        manifest["version"] = "0.3.0"
                    elif case == "duplicate":
                        manifest["artifacts"].append(copy.deepcopy(manifest["artifacts"][0]))
                    elif case == "binary_identity":
                        manifest["artifacts"][0]["binarySha256"] = "2" * 64
                    elif case == "schema_bool":
                        manifest["schemaVersion"] = True
                    elif case == "bad_path":
                        manifest["artifacts"][0]["file"] = "../other.exe"
                    elif case == "oversize":
                        manifest["artifacts"][0]["size"] = 134217729
                    (root / "expected.json").write_text(json.dumps(expected))
                    (root / "fixture.json").write_text(json.dumps(manifest))
                    (root / "fixture.bin").write_bytes(BODY if case != "tampered" else b"x" * len(BODY))
                    harness = f"""
$ErrorActionPreference='Stop'
. '{source}'
$e=Get-Content -Raw expected.json | ConvertFrom-Json
$global:Downloads=0
function Receive-ReleaseAsset {{
    param([string]$Url,[string]$Destination,[long]$Maximum)
    if (-not $Url.StartsWith('https://github.com/ghostroller/codex-usage-monit/releases/download/v0.4.0/') -and
        -not ('{case}' -eq 'latest' -and $Url -ceq 'https://github.com/ghostroller/codex-usage-monit/releases/latest/download/release-manifest.json')) {{ throw 'wrong origin' }}
    $global:Downloads++
    if ('{case}' -eq 'missing') {{ throw 'agent_release_unavailable: fixture 404' }}
    $fixture=if ($Url.EndsWith('.json')) {{ 'fixture.json' }} else {{ 'fixture.bin' }}
    Copy-Item -LiteralPath $fixture -Destination $Destination
}}
try {{
    $m=Invoke-ReleasePreparation $e '{STAGE}'
    $di=[IO.DirectoryInfo]::new((Join-Path (Get-Location).Path '{STAGE}'))
    $acl=if ($PSVersionTable.PSEdition -eq 'Desktop') {{ $di.GetAccessControl() }} else {{ [IO.FileSystemAclExtensions]::GetAccessControl($di) }}
    if (-not $acl.AreAccessRulesProtected) {{ throw 'staging DACL inherits rules' }}
    $rules=$acl.Access
    $current=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    foreach ($rule in $rules) {{ if ($rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -ne $current) {{ throw 'unexpected directory principal' }} }}
    $r=@{{success=$true;hash=$m.sha256}}
}} catch {{ $r=@{{success=$false;error=$_.Exception.Message}} }}
$r.downloads=$global:Downloads
$r.stageExists=(Test-Path -LiteralPath '{STAGE}')
$r | ConvertTo-Json -Compress
"""
                    script = root / "test.ps1"
                    script.write_text(harness, encoding="utf-8-sig")
                    completed = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-File", str(script)],
                                               cwd=root, capture_output=True, timeout=30)
                    self.assertEqual(completed.returncode, 0, completed.stderr.decode(errors="replace"))
                    result = json.loads(completed.stdout.decode("utf-8-sig"))
                    if case in ("success", "latest"):
                        self.assertTrue(result["success"], result)
                        self.assertEqual(result["hash"], manifest["artifacts"][0]["binarySha256"])
                        self.assertEqual(result["downloads"], 2)
                    else:
                        self.assertFalse(result["success"], result)
                        self.assertFalse(result["stageExists"], result)
                        self.assertIn("agent_", result["error"])
                        self.assertEqual(result["downloads"], 2 if case == "tampered" else 1)


if __name__ == "__main__":
    unittest.main()
