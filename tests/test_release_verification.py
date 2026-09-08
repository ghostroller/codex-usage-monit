"""Offline release evidence regressions. No requests or workflow dispatches."""

from copy import deepcopy
from datetime import datetime, timedelta, timezone
import importlib.util
from pathlib import Path
import unittest


SPEC = importlib.util.spec_from_file_location("release_verification", Path(__file__).resolve().parents[1] / "scripts/release_verification.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHA = "a" * 40
REPO = "owner/project"
NOW = datetime(2026, 9, 8, 4, tzinfo=timezone.utc)


def successful_run():
    return dict(id=123, run_attempt=1, head_sha=SHA, event="workflow_dispatch",
                path=".github/workflows/ci.yml", repository={"full_name": REPO},
                head_repository={"full_name": REPO}, status="completed", conclusion="success",
                updated_at=NOW.isoformat())


def successful_jobs():
    jobs = []
    for name, verification in MODULE.REQUIRED_JOBS.items():
        guard = "Verify the requested commit when called by CI" if name.endswith(" / audit") else "Verify the requested commit"
        jobs.append(dict(name=name, run_id=123, head_sha=SHA, status="completed", conclusion="success",
                         completed_at=NOW.isoformat(), steps=[
                             dict(name=step, status="completed", conclusion="success")
                             for step in (guard, verification)]))
    return {"total_count": len(jobs), "jobs": jobs}


class ReleaseEvidenceTests(unittest.TestCase):
    def inspect(self, runs=None, jobs=None, current=None, total_count=None):
        runs = [successful_run()] if runs is None else runs
        jobs = successful_jobs() if jobs is None else jobs
        current = successful_run() if current is None else current
        self.calls = []

        def read(endpoint):
            self.calls.append(endpoint)
            if "/workflows/ci.yml/runs?" in endpoint:
                self.assertIn(f"head_sha={SHA}&event=workflow_dispatch", endpoint)
                return {"total_count": len(runs) if total_count is None else total_count, "workflow_runs": runs}
            if "/attempts/" in endpoint:
                self.assertIn("/123/attempts/1/jobs?", endpoint)
                return jobs
            self.assertTrue(endpoint.endswith("/runs/123"))
            return current

        return MODULE.inspect(REPO, SHA, read=read, now=NOW)

    def test_reuses_complete_same_commit_checkpoint(self):
        result = self.inspect()
        self.assertTrue(result["reuse"])
        self.assertEqual(result["run_id"], 123)
        self.assertEqual(len(self.calls), 3)

    def test_missing_evidence_requires_full_verification(self):
        self.assertFalse(self.inspect(runs=[])["reuse"])

    def test_ref_qualified_workflow_path_preserves_reuse_and_active_detection(self):
        qualified = successful_run() | dict(path=".github/workflows/ci.yml@main")
        self.assertTrue(self.inspect(runs=[qualified], current=qualified)["reuse"])
        with self.assertRaisesRegex(RuntimeError, "still running"):
            self.inspect(runs=[qualified | dict(status="queued", conclusion=None)])

    def test_wrong_source_or_event_is_never_reused(self):
        for change in [dict(head_sha="b" * 40), dict(path=".github/workflows/other.yml"),
                       dict(event="pull_request"), dict(repository={"full_name": "fork/project"}),
                       dict(head_repository={"full_name": "fork/project"}), dict(run_attempt=0),
                       dict(conclusion="failure"), dict(conclusion="cancelled")]:
            with self.subTest(change=change):
                self.assertFalse(self.inspect(runs=[successful_run() | change])["reuse"])

    def test_newer_failed_attempt_is_not_hidden_by_old_success(self):
        changed = successful_run() | dict(id=122, conclusion="failure", updated_at=(NOW + timedelta(seconds=1)).isoformat())
        self.assertFalse(self.inspect(runs=[successful_run(), changed])["reuse"])

    def test_running_ci_and_incomplete_listing_do_not_start_duplicate_matrix(self):
        with self.assertRaisesRegex(RuntimeError, "still running"):
            self.inspect(runs=[successful_run() | dict(status="in_progress", conclusion=None)])
        with self.assertRaisesRegex(RuntimeError, "Incomplete run listing"):
            self.inspect(total_count=101)
        for count in [1, -1]:
            with self.assertRaisesRegex(RuntimeError, "Incomplete run listing"):
                self.inspect(runs=[], total_count=count)

    def test_every_required_job_and_step_must_really_succeed(self):
        for index in range(4):
            for change in ("missing", "skipped", "failed", "wrong-sha", "wrong-run", "missing-step", "skipped-step", "skipped-guard"):
                with self.subTest(job=index, change=change):
                    payload = successful_jobs()
                    job = payload["jobs"][index]
                    if change == "missing":
                        payload["jobs"].pop(index)
                    elif change in ("skipped", "failed"):
                        job["conclusion"] = change
                    elif change == "wrong-sha":
                        job["head_sha"] = "b" * 40
                    elif change == "wrong-run":
                        job["run_id"] = 124
                    elif change == "missing-step":
                        job["steps"].pop()
                    else:
                        job["steps"][0 if change == "skipped-guard" else 1]["conclusion"] = "skipped"
                    self.assertFalse(self.inspect(jobs=payload)["reuse"])

    def test_duplicate_or_additional_jobs_do_not_satisfy_evidence(self):
        payload = successful_jobs()
        payload["jobs"][3] = deepcopy(payload["jobs"][0])
        self.assertFalse(self.inspect(jobs=payload)["reuse"])
        payload = successful_jobs()
        payload["jobs"].append(deepcopy(payload["jobs"][0]))
        payload["total_count"] += 1
        self.assertFalse(self.inspect(jobs=payload)["reuse"])

    def test_old_future_and_invalid_evidence_requires_full_verification(self):
        for finished in [(NOW - timedelta(days=8)).isoformat(), (NOW + timedelta(seconds=1)).isoformat(), "invalid", None]:
            payload = successful_jobs()
            payload["jobs"][0]["completed_at"] = finished
            self.assertFalse(self.inspect(jobs=payload)["reuse"])

    def test_rerun_during_lookup_invalidates_evidence(self):
        for change in [dict(run_attempt=2), dict(status="in_progress"), dict(head_sha="b" * 40)]:
            with self.assertRaisesRegex(RuntimeError, "CI changed"):
                self.inspect(current=successful_run() | change)

    def test_api_failure_never_degrades_to_a_new_matrix(self):
        def failing_read(endpoint):
            raise RuntimeError("API unavailable")
        with self.assertRaisesRegex(RuntimeError, "API unavailable"):
            MODULE.inspect(REPO, SHA, read=failing_read, now=NOW)


if __name__ == "__main__":
    unittest.main()
