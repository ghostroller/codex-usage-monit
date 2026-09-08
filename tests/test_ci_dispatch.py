"""Offline contract checks: these tests never contact or dispatch GitHub Actions."""

import importlib.util
import copy
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
EXPECTED_CHECKS = {
    "Verify checkpoint / Verify Linux": (
        "Verify the requested commit", "Run shared Unix verification pipeline"),
    "Verify checkpoint / Verify macOS": (
        "Verify the requested commit", "Run shared Unix verification pipeline"),
    "Verify checkpoint / Verify Windows": (
        "Verify the requested commit", "Run Windows verification pipeline"),
    "Verify checkpoint / Check dependency advisories / audit": (
        "Verify the requested commit when called by CI",
        "Check all locked dependencies and advisory warnings"),
}


def run(run_id=123, status="completed", conclusion="success", **changes):
    return {"id": run_id, "head_sha": SHA, "event": "workflow_dispatch",
            "path": ".github/workflows/ci.yml", "repository": {"full_name": "owner/project"},
            "head_repository": {"full_name": "owner/project"},
            "run_attempt": 1, "status": status, "conclusion": conclusion,
            "html_url": f"https://github.com/owner/project/actions/runs/{run_id}",
            "updated_at": "2026-09-08T00:00:00Z", **changes}


def successful_jobs(run_id=123):
    return [{"name": name, "run_id": run_id, "head_sha": SHA,
             "status": "completed", "conclusion": "success",
             "steps": [{"name": step, "status": "completed", "conclusion": "success"}
                       for step in steps]}
            for name, steps in EXPECTED_CHECKS.items()]


class DispatchTests(unittest.TestCase):
    def invoke(self, extra=(), dirty=False, remote_sha=SHA, dispatch_error=False,
               runs=(), jobs=None, run_pages=None, job_pages=None, query_error=False):
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
            if args[:4] == ("gh", "api", "--paginate", "--slurp"):
                if query_error:
                    raise RuntimeError("GitHub unavailable")
                if "/actions/workflows/ci.yml/runs?" in args[-1]:
                    self.assertIn("head_sha=" + SHA, args[-1])
                    self.assertIn("event=workflow_dispatch", args[-1])
                    self.assertTrue(args[-1].startswith("repos/owner/project/"))
                    return json.dumps(run_pages if run_pages is not None else
                                      [{"total_count": len(runs), "workflow_runs": runs}])
                if "/jobs?" in args[-1]:
                    selected_jobs = successful_jobs() if jobs is None else jobs
                    return json.dumps(job_pages if job_pages is not None else
                                      [{"total_count": len(selected_jobs), "jobs": selected_jobs}])
                self.fail(f"unexpected paginated API call: {args}")
            if args[:2] == ("gh", "api"):
                return json.dumps({"object": {"sha": remote_sha}})
            if args[:3] == ("gh", "workflow", "run"):
                if dispatch_error:
                    raise subprocess.TimeoutExpired(args, 30)
                return "https://github.com/owner/project/actions/runs/123"
            self.fail(f"unexpected command: {args}")

        self.calls = calls
        with patch.object(sys, "argv", [str(SCRIPT), "--local-results", "Linux PASS; Windows PASS", *extra]), \
             patch.object(MODULE, "output", side_effect=command), patch("builtins.print") as printed:
            self.printed = printed
            MODULE.main()
        return calls

    def dispatches(self):
        return [call for call in self.calls if call[:3] == ("gh", "workflow", "run")]

    def messages(self):
        return "\n".join(" ".join(str(value) for value in call.args)
                         for call in self.printed.call_args_list)

    def test_dispatch_binds_current_branch_and_sha(self):
        calls = self.invoke()
        dispatch = [c for c in calls if c[:3] == ("gh", "workflow", "run")]
        self.assertEqual(len(dispatch), 1)
        self.assertIn("expected_sha=" + SHA, dispatch[0])
        self.assertEqual(dispatch[0][dispatch[0].index("--ref") + 1], "codex/checkpoint")
        self.assertTrue(any("repos/owner/project/git/ref/heads/codex%2Fcheckpoint" in call
                            for call in calls))

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

    def test_any_active_run_is_reused_even_with_an_override(self):
        for status in ("queued", "in_progress", "waiting", "pending", "requested"):
            for reason in ([], ["--rerun-reason", "explicit repeat"]):
                with self.subTest(status=status, reason=reason):
                    self.invoke(reason, runs=[run(456), run(123, status, None)])
                    self.assertEqual(self.dispatches(), [])
                    self.assertIn("runs/123", self.messages())
                    self.assertFalse(any("/jobs?" in call[-1] for call in self.calls))

    def test_complete_success_is_reused(self):
        self.invoke(runs=[run()])
        self.assertEqual(self.dispatches(), [])
        self.assertIn("Reusing complete successful checkpoint", self.messages())
        self.assertIn("runs/123", self.messages())

    def test_explicit_reason_can_recheck_success_and_is_recorded_literally(self):
        reason = "Updated external dependency advisory; $(touch bad) `echo bad`"
        self.invoke(["--rerun-reason", reason], runs=[run()])
        self.assertEqual(len(self.dispatches()), 1)
        self.assertIn("expected_sha=" + SHA, self.dispatches()[0])
        self.assertIn("local_results=Linux PASS; Windows PASS\nExplicit re-run reason: " + reason,
                      self.dispatches()[0])

    def test_empty_rerun_reason_is_rejected_before_commands(self):
        with self.assertRaisesRegex(RuntimeError, "--rerun-reason must be nonempty"):
            self.invoke(["--rerun-reason", " \n "])
        self.assertEqual(self.calls, [])

    def test_failed_or_cancelled_run_requires_a_reason_and_suggests_failed_jobs(self):
        for conclusion in ("failure", "cancelled", "timed_out", "skipped", "neutral", None):
            with self.subTest(conclusion=conclusion):
                with self.assertRaisesRegex(RuntimeError, "gh run rerun 123 --repo owner/project --failed"):
                    self.invoke(runs=[run(conclusion=conclusion)])
                self.assertEqual(self.dispatches(), [])
        self.invoke(["--rerun-reason", "Revalidate runner image after infrastructure diagnosis"],
                    runs=[run(conclusion="failure")])
        self.assertEqual(len(self.dispatches()), 1)
        self.assertIn("--failed", self.messages())

    def test_newer_failure_is_not_hidden_by_an_old_success(self):
        older = run(456, updated_at="2026-09-07T00:00:00Z")
        recent = run(123, conclusion="failure", run_attempt=2)
        with self.assertRaisesRegex(RuntimeError, "runs/123"):
            self.invoke(runs=[older, recent])
        self.assertEqual(self.dispatches(), [])

    def test_other_sha_workflow_event_or_repository_cannot_supply_reusable_evidence(self):
        unrelated = [run(head_sha="b" * 40), run(path=".github/workflows/release.yml"),
                     run(event="push"), run(repository={"full_name": "other/project"}),
                     run(head_repository={"full_name": "fork/project"})]
        self.invoke(runs=unrelated)
        self.assertEqual(len(self.dispatches()), 1)
        self.assertFalse(any("/jobs?" in call[-1] for call in self.calls))

    def test_top_level_success_cannot_hide_missing_skipped_or_failed_jobs(self):
        for index in range(len(EXPECTED_CHECKS)):
            for defect in ("missing", "skipped", "failure", "wrong-sha", "wrong-run", "duplicate"):
                jobs = successful_jobs()
                if defect == "missing":
                    del jobs[index]
                elif defect == "duplicate":
                    jobs.append(copy.deepcopy(jobs[index]))
                elif defect == "wrong-sha":
                    jobs[index]["head_sha"] = "b" * 40
                elif defect == "wrong-run":
                    jobs[index]["run_id"] = 999
                else:
                    jobs[index]["conclusion"] = defect
                with self.subTest(job=index, defect=defect):
                    with self.assertRaisesRegex(RuntimeError, "no complete successful verification"):
                        self.invoke(runs=[run()], jobs=jobs)
                    self.assertEqual(self.dispatches(), [])

    def test_required_sha_checks_and_actual_verification_steps_must_run(self):
        for index in range(len(EXPECTED_CHECKS)):
            for step_index in (0, 1):
                for defect in ("missing", "skipped", "in_progress"):
                    jobs = successful_jobs()
                    if defect == "missing":
                        del jobs[index]["steps"][step_index]
                    elif defect == "skipped":
                        jobs[index]["steps"][step_index]["conclusion"] = "skipped"
                    else:
                        jobs[index]["steps"][step_index]["status"] = "in_progress"
                    with self.subTest(job=index, step=step_index, defect=defect):
                        with self.assertRaisesRegex(RuntimeError, "no complete successful verification"):
                            self.invoke(runs=[run()], jobs=jobs)
                        self.assertEqual(self.dispatches(), [])

    def test_run_and_job_pages_are_all_checked_for_the_selected_attempt(self):
        jobs = successful_jobs()
        self.invoke(run_pages=[{"total_count": 2, "workflow_runs": [
                        run(456, conclusion="failure", updated_at="2026-09-07T00:00:00Z")]},
                               {"total_count": 2, "workflow_runs": [run(run_attempt=3)]}],
                    job_pages=[{"total_count": len(jobs), "jobs": jobs[:2]},
                               {"total_count": len(jobs), "jobs": jobs[2:]}])
        self.assertEqual(self.dispatches(), [])
        self.assertTrue(any("/runs/123/attempts/3/jobs?" in call[-1] for call in self.calls))

    def test_query_failure_and_invalid_or_incomplete_json_never_dispatch(self):
        with self.assertRaisesRegex(RuntimeError, "GitHub unavailable"):
            self.invoke(query_error=True)
        self.assertEqual(self.dispatches(), [])
        for pages in ([], {}, [None], [{"total_count": 1, "workflow_runs": []}],
                      [{"total_count": 1, "workflow_runs": [None]}]):
            with self.subTest(pages=pages):
                with self.assertRaises(RuntimeError):
                    self.invoke(run_pages=pages)
                self.assertEqual(self.dispatches(), [])
        with self.assertRaises(RuntimeError):
            self.invoke(runs=[run()], job_pages=[{"total_count": 4, "jobs": []}])
        self.assertEqual(self.dispatches(), [])

    def test_malformed_run_metadata_never_dispatches(self):
        for changes in ({"updated_at": None}, {"run_attempt": 0}, {"id": True},
                        {"html_url": ""}, {"status": ""}, {"repository": None}, {"head_repository": None}):
            with self.subTest(changes=changes):
                with self.assertRaises(RuntimeError):
                    self.invoke(runs=[run(**changes)])
                self.assertEqual(self.dispatches(), [])

    def test_dry_run_still_reuses_active_or_complete_runs(self):
        for existing in (run(), run(status="in_progress", conclusion=None)):
            with self.subTest(existing=existing):
                self.invoke(["--dry-run"], runs=[existing])
                self.assertEqual(self.dispatches(), [])
                self.assertIn("runs/123", self.messages())


if __name__ == "__main__":
    unittest.main()
