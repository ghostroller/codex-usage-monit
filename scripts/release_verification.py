#!/usr/bin/env python3
"""Read GitHub evidence before deciding whether a release needs full verification.

Only the newest same-commit manual CI run can be reused. Missing, old or
incomplete evidence requests full verification; API failures stop the cheap
preflight rather than silently launching another expensive test matrix.
"""

import argparse
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import sys


MAX_AGE = timedelta(days=7)
REQUIRED_JOBS = {
    "Verify checkpoint / Verify Linux": "Run shared Unix verification pipeline",
    "Verify checkpoint / Verify macOS": "Run shared Unix verification pipeline",
    "Verify checkpoint / Verify Windows": "Run Windows verification pipeline",
    "Verify checkpoint / Check dependency advisories / audit":
        "Check all locked dependencies and advisory warnings",
}


def api(endpoint):
    result = subprocess.run(
        ["gh", "api", endpoint], capture_output=True, text=True, timeout=30,
    )
    if result.returncode:
        raise RuntimeError(f"Evidence lookup failed; no verification was requested: {result.stderr.strip()}")
    return json.loads(result.stdout)


def matching_source(run, repository, sha):
    path = run.get("path")
    return (
        run.get("head_sha") == sha
        and run.get("event") == "workflow_dispatch"
        and isinstance(path, str) and path.partition("@")[0] == ".github/workflows/ci.yml"
        and run.get("repository", {}).get("full_name", "").lower() == repository.lower()
        and run.get("head_repository", {}).get("full_name", "").lower() == repository.lower()
        and type(run.get("id")) is int and run["id"] > 0
        and type(run.get("run_attempt")) is int and run["run_attempt"] > 0
    )


def reusable_run(run, repository, sha):
    return (matching_source(run, repository, sha)
            and run.get("status") == "completed" and run.get("conclusion") == "success")


def complete_jobs(payload, run, sha, now):
    jobs = payload.get("jobs", [])
    if payload.get("total_count") != len(REQUIRED_JOBS) or len(jobs) != len(REQUIRED_JOBS):
        return False
    if {job.get("name") for job in jobs} != set(REQUIRED_JOBS):
        return False
    for job in jobs:
        if (job.get("run_id") != run["id"] or job.get("head_sha") != sha
                or job.get("status") != "completed" or job.get("conclusion") != "success"):
            return False
        try:
            finished = datetime.fromisoformat(job["completed_at"].replace("Z", "+00:00"))
            if not timedelta(0) <= now - finished <= MAX_AGE:
                return False
        except (AttributeError, KeyError, TypeError, ValueError):
            return False
        # A skipped platform/verification step can leave the whole run green.
        guard = ("Verify the requested commit when called by CI" if job["name"].endswith(" / audit")
                 else "Verify the requested commit")
        for required in (guard, REQUIRED_JOBS[job["name"]]):
            steps = [step for step in job.get("steps", []) if step.get("name") == required]
            if len(steps) != 1 or steps[0].get("status") != "completed" or steps[0].get("conclusion") != "success":
                return False
    return True


def inspect(repository, sha, read=api, now=None):
    now = now or datetime.now(timezone.utc)
    prefix = f"repos/{repository}/actions"
    # Do not filter by success: a newer failure or pending rerun must not be
    # hidden by an earlier green result for the same commit.
    listing = read(f"{prefix}/workflows/ci.yml/runs?head_sha={sha}&event=workflow_dispatch&per_page=100")
    runs = listing.get("workflow_runs")
    if not isinstance(runs, list) or type(listing.get("total_count")) is not int:
        raise RuntimeError("Malformed workflow evidence; no verification was requested")
    decision = {"reuse": False, "sha": sha, "reason": "No matching completed CI evidence"}
    if listing["total_count"] < 0 or listing["total_count"] != len(runs):
        raise RuntimeError("Incomplete run listing; inspect existing CI before requesting more verification")
    if not runs:
        return decision
    if any(matching_source(run, repository, sha) and run.get("status") != "completed" for run in runs):
        raise RuntimeError("Same-commit CI is still running; finish that run before retrying release preflight")
    run = max(runs, key=lambda item: (item.get("updated_at", ""), item.get("id", 0)))
    if not reusable_run(run, repository, sha):
        return decision | {"reason": "Newest same-commit CI is not a trusted successful checkpoint"}
    run_id, attempt = run["id"], run["run_attempt"]
    jobs = read(f"{prefix}/runs/{run_id}/attempts/{attempt}/jobs?per_page=100")
    if not complete_jobs(jobs, run, sha, now):
        return decision | {"reason": "CI lacks four complete successful checks from the last seven days"}
    # Recheck the attempt after inspecting jobs, so an in-progress rerun cannot
    # be mistaken for the completed attempt we just read.
    current = read(f"{prefix}/runs/{run_id}")
    if (not reusable_run(current, repository, sha)
            or current["id"] != run_id or current["run_attempt"] != attempt):
        raise RuntimeError("CI changed while its evidence was being checked; retry only this preflight")
    return decision | {
        "reuse": True, "run_id": run_id, "run_attempt": attempt,
        "url": f"https://github.com/{repository}/actions/runs/{run_id}",
        "reason": "Same-commit Linux, macOS, Windows and audit checks all passed within seven days",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--sha", required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        raise ValueError("--repo must be OWNER/REPO")
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        raise ValueError("--sha must be a full lowercase commit SHA")
    result = inspect(args.repo, args.sha)
    print(json.dumps(result, indent=2))
    if output := os.environ.get("GITHUB_OUTPUT"):
        with Path(output).open("a") as target:
            target.write(f"reuse={str(result['reuse']).lower()}\n")
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary).open("a") as target:
            target.write(f"Release verification for `{args.sha}`: {result['reason']}.\n\n")
            if result["reuse"]:
                target.write(f"Evidence: {result['url']} (attempt {result['run_attempt']}).\n")
            target.write("A fresh dependency audit and all release binary checks remain required.\n")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
