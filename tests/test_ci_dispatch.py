"""Offline contract checks: these tests never contact or dispatch GitHub Actions."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "run-ci.py"
SPEC = importlib.util.spec_from_file_location("run_ci", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHA = "a" * 40


class DispatchTests(unittest.TestCase):
    def invoke(self, extra=(), dirty=False, remote_sha=SHA, dispatch_error=False):
        calls = []

        def command(*args):
            calls.append(args)
            if args[:3] == ("git", "status", "--porcelain"):
                return " M src/lib.rs" if dirty else ""
            if args[:2] == ("git", "rev-parse"):
                return SHA
            if args[:2] == ("git", "symbolic-ref"):
                return "codex/checkpoint"
            if args[:2] == ("git", "check-ref-format"):
                return ""
            if args[:3] == ("gh", "repo", "view"):
                return "owner/project"
            if args[:2] == ("gh", "api"):
                return json.dumps({"object": {"sha": remote_sha}})
            if args[:3] == ("gh", "workflow", "run"):
                if dispatch_error:
                    raise subprocess.TimeoutExpired(args, 30)
                return "https://github.com/owner/project/actions/runs/123"
            self.fail(f"unexpected command: {args}")

        self.calls = calls
        with patch.object(sys, "argv", [str(SCRIPT), "--local-results", "Linux PASS; Windows PASS", *extra]), \
             patch.object(MODULE, "output", side_effect=command), patch("builtins.print"):
            MODULE.main()
        return calls

    def test_dispatch_binds_current_branch_and_sha(self):
        calls = self.invoke()
        dispatch = [c for c in calls if c[:3] == ("gh", "workflow", "run")]
        self.assertEqual(len(dispatch), 1)
        self.assertIn("expected_sha=" + SHA, dispatch[0])
        self.assertEqual(dispatch[0][dispatch[0].index("--ref") + 1], "codex/checkpoint")
        self.assertIn("repos/owner/project/git/ref/heads/codex%2Fcheckpoint", calls[-2])

    def test_dirty_input_never_dispatches(self):
        with self.assertRaisesRegex(RuntimeError, "dirty working tree"):
            self.invoke(dirty=True)
        self.assertFalse(any(c[0] == "gh" for c in self.calls))

    def test_unpushed_commit_never_dispatches(self):
        with self.assertRaisesRegex(RuntimeError, "not at local HEAD"):
            self.invoke(remote_sha="b" * 40)
        self.assertFalse(any(c[:3] == ("gh", "workflow", "run") for c in self.calls))

    def test_dry_run_does_not_submit(self):
        calls = self.invoke(["--dry-run"])
        self.assertFalse(any(c[:3] == ("gh", "workflow", "run") for c in calls))

    def test_evidence_is_one_literal_argument(self):
        evidence = "Linux PASS\nWindows blocked; $(touch bad) `echo bad`"
        calls = self.invoke(["--local-results", evidence])
        self.assertIn("local_results=" + evidence, calls[-1])

    def test_empty_evidence_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "must describe"):
            self.invoke(["--local-results", " "])
        self.assertEqual(self.calls, [])

    def test_uncertain_dispatch_is_not_retried(self):
        with self.assertRaisesRegex(RuntimeError, "uncertain"):
            self.invoke(dispatch_error=True)
        self.assertEqual(sum(c[:3] == ("gh", "workflow", "run") for c in self.calls), 1)


if __name__ == "__main__":
    unittest.main()
