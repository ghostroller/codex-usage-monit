#!/usr/bin/env python3
"""Dispatch one deliberate CI checkpoint for the current, already-pushed commit."""

import argparse
import json
import re
import shlex
import subprocess
import sys
from urllib.parse import quote


def output(*args):
    result = subprocess.run(args, text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"{args[0]} exited {result.returncode}")
    return result.stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ref", help="Already-pushed branch (default: current branch)")
    parser.add_argument("--repo", help="GitHub OWNER/REPO (default: gh's repository for this checkout)")
    parser.add_argument("--local-results", required=True,
                        help="Local Linux/Windows results and logs, or concrete blockers")
    parser.add_argument("--dry-run", action="store_true", help="Run preflight and print the dispatch without submitting it")
    args = parser.parse_args()

    if not args.local_results.strip():
        raise RuntimeError("--local-results must describe local tests or their blockers")
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

    command = ["gh", "workflow", "run", "ci.yml", "--repo", repository,
               "--ref", branch, "--raw-field", f"expected_sha={sha}",
               "--raw-field", f"local_results={args.local_results}"]
    print(f"Checkpoint: {repository} {branch} {sha}")
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
