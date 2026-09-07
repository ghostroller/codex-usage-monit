"""Exercise Docker runner argument, isolation, logging and failure contracts."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(os.name == "posix" and shutil.which("bash"), "requires a Unix Bash host")
class LinuxDockerPipelineTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="linux-pipeline-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.calls = self.directory / "docker-calls.jsonl"
        mock_bin = self.directory / "bin"
        mock_bin.mkdir()
        docker = mock_bin / "docker"
        docker.write_text(f"#!{sys.executable}\n" + """
import json
import os
import sys
with open(os.environ["MOCK_DOCKER_CALLS"], "a") as output:
    output.write(json.dumps(sys.argv[1:]) + "\\n")
if sys.argv[1] == "info":
    print("aarch64")
elif sys.argv[1:3] == ["image", "inspect"]:
    print("sha256:fixture" if sys.argv[-1] == "{{.Id}}" else os.environ.get("MOCK_IMAGE_PLATFORM", "linux/arm64"))
elif sys.argv[1] == "run":
    print("mock container output")
    sys.exit(int(os.environ.get("MOCK_DOCKER_EXIT", "0")))
else:
    sys.exit(90)
""")
        docker.chmod(0o755)
        self.environment = os.environ.copy()
        self.environment.update({
            "PATH": f"{mock_bin}{os.pathsep}{self.environment['PATH']}",
            "MOCK_DOCKER_CALLS": str(self.calls),
            "CODEX_USAGE_MONIT_DOCKER_BUILD_ROOT": str(self.directory / "build root"),
            "CODEX_USAGE_MONIT_ALLOW_INTERNAL_BUILD_ROOT": "1",
            "CODEX_USAGE_MONIT_RUST_IMAGE": "local-rust-fixture",
        })

    def run_pipeline(self, *arguments, build=False):
        script = "build-linux-amd64-docker.sh" if build else "test-linux-docker.sh"
        return subprocess.run(
            ["sh", str(ROOT / "scripts" / script), *arguments],
            env=self.environment, text=True, capture_output=True, timeout=15,
        )

    def docker_calls(self):
        if not self.calls.exists():
            return []
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def test_native_platform_focused_argument_and_private_build_paths(self):
        result = self.run_pipeline("--filter", "a test filter")
        self.assertEqual(result.returncode, 0, result.stderr)
        call = next(call for call in self.docker_calls() if call[0] == "run")
        self.assertEqual(call[call.index("--platform") + 1], "linux/arm64")
        self.assertEqual(call[call.index("docker-linux") + 1:], ["verify", "--filter", "a test filter"])
        self.assertIn(f"type=bind,src={ROOT},dst=/source,readonly", call)
        self.assertIn("CARGO_TARGET_DIR=/target", call)
        self.assertIn("CARGO_BUILD_BUILD_DIR=/target/build", call)
        self.assertIn("RUSTUP_HOME=/rustup-home", call)
        self.assertIn("--init", call)
        self.assertEqual(call[call.index("--cap-drop") + 1], "ALL")
        self.assertIn("/container-tmp:rw,exec,mode=1777,size=2g", call)
        self.assertTrue(any(argument.startswith("/container-home:rw,exec,mode=0700,") for argument in call))
        self.assertTrue(any(argument.endswith("dst=/etc/passwd,readonly") for argument in call))
        self.assertEqual(call[call.index("--pull") + 1], "never")
        self.assertEqual(call[call.index("bash") - 1], "sha256:fixture")

    def test_log_write_failure_does_not_report_success(self):
        tee = self.directory / "bin" / "tee"
        tee.write_text("#!/bin/sh\ncat >/dev/null\nexit 74\n")
        tee.chmod(0o755)
        result = self.run_pipeline()
        self.assertEqual(result.returncode, 74, result.stderr)
        self.assertIn("writing the Linux verification log failed", result.stderr)

    def test_failed_container_exit_is_preserved_through_log_streaming(self):
        self.environment["MOCK_DOCKER_EXIT"] = "42"
        result = self.run_pipeline()
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertIn("Linux verify exit code: 42", result.stdout)
        logs = list((self.directory / "build root" / "runs").glob("*/verify.log"))
        self.assertEqual(len(logs), 1)
        self.assertIn("mock container output", logs[0].read_text())
        result = json.loads((logs[0].parent / "result.json").read_text())
        self.assertEqual(result["exit_code"], 42)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["target_platform"], "linux/arm64")
        self.assertEqual(result["image_id"], "sha256:fixture")

    def test_wrong_local_image_architecture_fails_before_container_start(self):
        result = self.run_pipeline("--platform", "linux/amd64")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("is linux/arm64; requested linux/amd64", result.stderr)
        self.assertFalse(any(call[0] == "run" for call in self.docker_calls()))

    def test_amd64_build_uses_explicit_architecture_and_separate_operation(self):
        self.environment["MOCK_IMAGE_PLATFORM"] = "linux/amd64"
        result = self.run_pipeline(build=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        call = next(call for call in self.docker_calls() if call[0] == "run")
        self.assertEqual(call[call.index("--platform") + 1], "linux/amd64")
        self.assertEqual(call[call.index("docker-linux") + 1:], ["build"])
        self.assertIn("emulation", result.stdout)
        override = self.run_pipeline("--platform", "linux/arm64", build=True)
        self.assertEqual(override.returncode, 64, override.stderr)
        self.assertIn("amd64 build entry point requires", override.stderr)

    def test_invalid_arguments_and_help_do_not_contact_docker(self):
        self.assertEqual(self.run_pipeline("--filter").returncode, 64)
        self.assertEqual(self.run_pipeline("--help").returncode, 0)
        self.assertEqual(self.docker_calls(), [])


@unittest.skipUnless(os.name == "posix" and shutil.which("bash"), "requires a Unix shell host")
class UnixVerificationPipelineTests(unittest.TestCase):
    def test_focused_mode_only_runs_matching_rust_tests_and_preserves_failure(self):
        with tempfile.TemporaryDirectory(prefix="unix-pipeline-") as temporary:
            directory = Path(temporary)
            calls = directory / "calls.jsonl"
            for tool in ("cargo", "rustc"):
                path = directory / tool
                path.write_text(f"#!{sys.executable}\n" + """
import json
import os
from pathlib import Path
import sys
tool = Path(sys.argv[0]).name
with open(os.environ["MOCK_VERIFY_CALLS"], "a") as output:
    output.write(json.dumps([tool, *sys.argv[1:]]) + "\\n")
if tool == "cargo":
    sys.exit(42)
""")
                path.chmod(0o755)
            environment = {
                **os.environ,
                "PATH": f"{directory}{os.pathsep}{os.environ['PATH']}",
                "MOCK_VERIFY_CALLS": str(calls),
            }
            result = subprocess.run(
                ["sh", str(ROOT / "scripts/verify-unix.sh"), "--filter", "recorder retry"],
                env=environment, text=True, capture_output=True, timeout=15,
            )
            self.assertEqual(result.returncode, 42, result.stderr)
            self.assertEqual([json.loads(line) for line in calls.read_text().splitlines()], [
                ["rustc", "-vV"],
                ["cargo", "test", "--locked", "--all-targets", "recorder retry"],
            ])


@unittest.skipUnless(shutil.which("git"), "requires Git")
class LinuxSnapshotTests(unittest.TestCase):
    def test_snapshot_keeps_dirty_files_and_deletions_excludes_ignored_data_and_hashes_contents(self):
        with tempfile.TemporaryDirectory(prefix="linux-snapshot-") as temporary:
            directory = Path(temporary)
            source = directory / "source"
            source.mkdir()
            output = directory / "run"
            output.mkdir()
            snapshot = directory / "workspace"
            snapshot.mkdir()
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            (source / ".gitignore").write_text(".env\n.cargo/config.toml\n")
            (source / "tracked").write_text("initial")
            (source / "deleted").write_text("remove me")
            subprocess.run(["git", "-C", str(source), "add", "."], check=True)
            subprocess.run(["git", "-C", str(source), "-c", "user.name=Fixture",
                            "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
                            "commit", "-qm", "fixture"], check=True)
            (source / "tracked").write_text("edited")
            (source / "deleted").unlink()
            (source / "new file").write_text("untracked")
            (source / ".env").write_text("private ignored data")
            (source / ".cargo").mkdir()
            (source / ".cargo/config.toml").write_text("host-only paths")
            helper = ROOT / "scripts/docker-linux-snapshot.py"
            subprocess.run([sys.executable, str(helper), "prepare", str(source), str(output),
                            "linux/arm64", "sha256:fixture", "verify", "--filter", "recorder"], check=True)
            manifest = json.loads((output / "source.json").read_text())
            self.assertTrue(manifest["source_dirty"])
            self.assertEqual(len(manifest["source_head"]), 40)
            self.assertEqual(manifest["command"], ["verify", "--filter", "recorder"])
            names = (output / "source-files").read_bytes().split(b"\0")[:-1]
            self.assertEqual(names, [b".gitignore", b"new file", b"tracked"])
            for name in names:
                shutil.copy2(source / os.fsdecode(name), snapshot / os.fsdecode(name))
            stamp = [sys.executable, str(helper), "stamp", str(snapshot),
                     str(output / "source.json"), str(output / "source-files")]
            first = json.loads(subprocess.check_output(stamp, text=True))
            repeated = json.loads(subprocess.check_output(stamp, text=True))
            self.assertEqual(first["snapshot_sha256"], repeated["snapshot_sha256"])
            (snapshot / "tracked").write_text("different snapshot")
            changed = json.loads(subprocess.check_output(stamp, text=True))
            self.assertNotEqual(first["snapshot_sha256"], changed["snapshot_sha256"])


if __name__ == "__main__":
    unittest.main()
