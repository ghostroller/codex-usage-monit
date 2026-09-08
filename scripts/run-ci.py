#!/usr/bin/env python3
"""Reuse or dispatch a deliberate CI checkpoint for the current, already-pushed commit."""

import argparse
from datetime import datetime
import json
import re
import shlex
import subprocess
import sys
from urllib.parse import quote


REQUIRED_CHECKS = {
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


def output(*args):
    result = subprocess.run(args, text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"{args[0]} exited {result.returncode}")
    return result.stdout.strip()


def paginated_items(endpoint, key):
    pages = json.loads(output("gh", "api", "--paginate", "--slurp", endpoint))
    if not isinstance(pages, list) or not pages:
        raise RuntimeError(f"GitHub returned no usable pages for {key}; no dispatch attempted")
    items = []
    counts = set()
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get(key), list):
            raise RuntimeError(f"GitHub returned malformed {key}; no dispatch attempted")
        count = page.get("total_count")
        if type(count) is not int or count < 0:
            raise RuntimeError(f"GitHub omitted the {key} count; no dispatch attempted")
        counts.add(count)
        items.extend(page[key])
    if counts != {len(items)} or any(not isinstance(item, dict) for item in items):
        raise RuntimeError(f"GitHub returned incomplete or changing {key}; inspect runs before retrying")
    return items


def checkpoint_runs(repository, sha):
    runs = paginated_items(
        f"repos/{repository}/actions/workflows/ci.yml/runs"
        f"?head_sha={sha}&event=workflow_dispatch&per_page=100", "workflow_runs")
    matching = []
    seen = set()
    for run in runs:
        # Keep the scope explicit even though the API query applies filters.
        if run.get("head_sha") != sha or run.get("event") != "workflow_dispatch":
            continue
        path = run.get("path")
        if not isinstance(path, str) or path.partition("@")[0] != ".github/workflows/ci.yml":
            continue
        origin = run.get("repository")
        if not isinstance(origin, dict) or not isinstance(origin.get("full_name"), str):
            raise RuntimeError("GitHub omitted a run's repository; no dispatch attempted")
        if origin["full_name"].casefold() != repository.casefold():
            continue
        head_repository = run.get("head_repository")
        if not isinstance(head_repository, dict) or not isinstance(head_repository.get("full_name"), str):
            raise RuntimeError("GitHub omitted a run's head repository; no dispatch attempted")
        if head_repository["full_name"].casefold() != repository.casefold():
            continue
        if (type(run.get("id")) is not int or run["id"] <= 0
                or type(run.get("run_attempt")) is not int or run["run_attempt"] <= 0
                or not isinstance(run.get("status"), str) or not run["status"]
                or not isinstance(run.get("html_url"), str) or not run["html_url"]):
            raise RuntimeError("GitHub returned malformed run metadata; no dispatch attempted")
        try:
            datetime.strptime(run["updated_at"], "%Y-%m-%dT%H:%M:%SZ")
        except (KeyError, TypeError, ValueError) as error:
            raise RuntimeError("GitHub omitted a run's update time; no dispatch attempted") from error
        if run["id"] in seen:
            raise RuntimeError("GitHub returned duplicate runs; inspect runs before retrying")
        seen.add(run["id"])
        matching.append(run)
    return sorted(matching, key=lambda run: (run["updated_at"], run["id"]), reverse=True)


def complete_verification(repository, sha, run):
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        return False
    jobs = paginated_items(
        f"repos/{repository}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs"
        "?per_page=100", "jobs")
    if len(jobs) != len(REQUIRED_CHECKS):
        return False
    for name, required_steps in REQUIRED_CHECKS.items():
        candidates = [job for job in jobs if job.get("name") == name]
        if len(candidates) != 1:
            return False
        job = candidates[0]
        if (job.get("run_id") != run["id"] or job.get("head_sha") != sha
                or job.get("status") != "completed" or job.get("conclusion") != "success"):
            return False
        steps = job.get("steps")
        if not isinstance(steps, list) or any(not isinstance(step, dict) for step in steps):
            return False
        for step_name in required_steps:
            matches = [step for step in steps if step.get("name") == step_name]
            if (len(matches) != 1 or matches[0].get("status") != "completed"
                    or matches[0].get("conclusion") != "success"):
                return False
    return True


def reuse_checkpoint(repository, sha, rerun_reason):
    runs = checkpoint_runs(repository, sha)
    active = next((run for run in runs if run["status"] != "completed"), None)
    if active:
        print(f"Existing checkpoint is {active['status']}; no duplicate dispatched: {active['html_url']}")
        return True
    if not runs:
        return False
    latest = runs[0]
    if complete_verification(repository, sha, latest):
        if not rerun_reason:
            print(f"Reusing complete successful checkpoint: {latest['html_url']}")
            return True
        print(f"Explicitly rechecking successful checkpoint: {latest['html_url']}")
    else:
        retry = shlex.join(["gh", "run", "rerun", str(latest["id"]),
                            "--repo", repository, "--failed"])
        message = (f"Latest checkpoint has no complete successful verification "
                   f"({latest.get('conclusion') or latest['status']}): {latest['html_url']}. "
                   f"For an infrastructure failure on unchanged source, prefer: {retry}")
        if not rerun_reason:
            raise RuntimeError(message + ". A new dispatch requires a nonempty --rerun-reason")
        print(message)
    print(f"Re-run reason: {rerun_reason}")
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ref", help="Already-pushed branch (default: current branch)")
    parser.add_argument("--repo", help="GitHub OWNER/REPO (default: gh's repository for this checkout)")
    parser.add_argument("--local-results", required=True,
                        help="Local Linux/Windows results and logs, or concrete blockers")
    parser.add_argument("--rerun-reason",
                        help="Explicit reason for a new checkpoint after a completed run for this SHA")
    parser.add_argument("--dry-run", action="store_true", help="Inspect the checkpoint decision without submitting a dispatch")
    args = parser.parse_args()

    if not args.local_results.strip():
        raise RuntimeError("--local-results must describe local tests or their blockers")
    if args.rerun_reason is not None and not args.rerun_reason.strip():
        raise RuntimeError("--rerun-reason must be nonempty when provided")
    if output("git", "status", "--porcelain"):
        raise RuntimeError("Commit or otherwise finish local changes first: remote CI cannot test a dirty working tree")
    sha = output("git", "rev-parse", "HEAD")
    branch = args.ref or output("git", "symbolic-ref", "--quiet", "--short", "HEAD")
    output("git", "check-ref-format", f"refs/heads/{branch}")
    repository = args.repo or output("gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise RuntimeError("--repo must be a GitHub OWNER/REPO")
    remote = json.loads(output("gh", "api", f"repos/{repository}/git/ref/heads/{quote(branch, safe='')}"))
    if remote.get("object", {}).get("sha") != sha:
        raise RuntimeError(f"Remote branch {branch!r} is not at local HEAD {sha}; push the intended commit before dispatch")

    print(f"Checkpoint: {repository} {branch} {sha}")
    rerun_reason = args.rerun_reason.strip() if args.rerun_reason is not None else None
    if reuse_checkpoint(repository, sha, rerun_reason):
        return
    local_results = args.local_results
    if rerun_reason:
        local_results += f"\nExplicit re-run reason: {rerun_reason}"
    command = ["gh", "workflow", "run", "ci.yml", "--repo", repository,
               "--ref", branch, "--raw-field", f"expected_sha={sha}",
               "--raw-field", f"local_results={local_results}"]
    print(shlex.join(command))
    if args.dry_run:
        print("Dry run: no workflow was dispatched.")
        return
    # Do not retry a dispatch: a transport failure can happen after GitHub has
    # already accepted the event. The workflow checks the event SHA again to
    # catch a branch moving between preflight and dispatch.
    try:
        result = output(*command)
    except (RuntimeError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(f"Dispatch result is uncertain; inspect runs for {sha} before retrying: {error}") from error
    if result:
        print(result)
    print("Use the returned run URL/ID to watch this run. If none was returned, inspect:")
    print(shlex.join(["gh", "run", "list", "--repo", repository, "--workflow", "ci.yml",
                      "--event", "workflow_dispatch", "--commit", sha,
                      "--json", "databaseId,headSha,status,conclusion,url,createdAt"]))
    print("Then: gh run watch RUN_ID --repo " + repository + " --exit-status")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
