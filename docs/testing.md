# Testing workflow

Run day-to-day tests locally. Use Docker for Linux, the existing UTM guest for
Windows, and the Mac host for macOS. Request one hosted CI run when a substantial
batch is ready for integration or release, or when a specific native platform
cannot be exercised locally. Commit code in reviewable increments; a commit or
push is not itself a request for hosted tests.

## Choose the smallest useful local run

Run these commands from the repository root. Replace `local_observation` with the
Rust test-name filter relevant to the change; remove the focused options for a
complete validation pass.

| Environment | Focused iteration | Complete checkpoint |
| --- | --- | --- |
| macOS host | `sh scripts/verify-unix.sh --filter local_observation` | `sh scripts/verify-unix.sh` |
| Linux in Docker | `sh scripts/test-linux-docker.sh --filter local_observation` | `sh scripts/test-linux-docker.sh` |
| Windows in UTM | `python3 scripts/macos/test-windows-utm.py --toolchain-home 'C:\Users\user' --focused --test-filter local_observation` | `python3 scripts/macos/test-windows-utm.py --toolchain-home 'C:\Users\user'` |

The Windows path is the Windows account that owns the installed Rust toolchain,
not a required username. Probe first with `--doctor` and follow
[Windows and UTM testing](windows-testing.md). The host runner uses a fresh run
directory and checks the guest's matching result file; `utmctl exec` alone does
not prove that a test command succeeded. Setup or login may require the VM
console. Do not provision another VM merely because the existing guest is stopped.

Docker tests use the daemon's native architecture by default, which is ARM64 on
this Apple Silicon machine. Add `--platform linux/amd64` when validating the x64
artifact; emulated x64 is slower. The container runs actual Linux tests, including
Unix PTY interaction, on an isolated writable snapshot. See
[Linux and Docker testing](linux-testing.md) for image preparation, external
storage, logs and recovery. The old `build-linux-amd64-docker.sh` entry point
still builds a release binary; a successful build is not a Linux test pass.

The full Unix entry point runs format, Clippy, all Rust test targets, the PTY
suite, the preview comparison, installer checks, pipeline contract tests and CLI
smoke checks. The full Windows entry point runs format, Clippy, native PowerShell
exit-code/JSON regressions, all Rust test targets including ConPTY, and CLI smoke
checks. Local and hosted jobs call these same entry points. A filtered run is
intentionally narrower and must be reported
as such. The manual real-user-history benchmark remains opt-in.

For platform-sensitive changes, run the affected platform locally during
iteration and complete the relevant local suites at the checkpoint. A purely
documentation change needs link and instruction checks, not three full Rust
builds. Persistence, process management, terminal behavior and dependency changes
usually need the full platform coverage. A Docker container does not by itself
test a live systemd user service, and a Windows type-check on macOS does not test
ConPTY or the Windows executable.

## Spend tests where they provide new evidence

Choose scope from the changed behavior, not the number of commits. In particular:

| Change or failure | Useful next check | Full checkpoint |
| --- | --- | --- |
| Documentation only | Links and command examples | No Rust rebuild |
| CI dispatch/reuse logic | Python contracts, actionlint, read-only lookup of existing runs | No hosted run just to test dispatch |
| PowerShell invocation/exit handling | Local shell contracts in both 5.1 and 7, including the GitHub outer wrapper | Windows suite once after the batch settles |
| Timing/process/lock behavior | Deterministic failing regression, then adjacent owners/callers | Relevant native platform suites once after all related fixes |
| Broad product or dependency changes | Affected tests while editing | Local platform suites, then one hosted checkpoint |

A successful full run stays useful for unchanged source. Do not repeat it after
every documentation edit or intermediate commit. Record which tested files are
unchanged and run checks affected by subsequent edits. Before the final hosted
checkpoint, reconcile the evidence with the committed source. Never turn a
focused pass into a claim of a new full-platform pass.

After a hosted failure, read its exact job/step log and classify it before
requesting more compute:

1. **Product defect:** reproduce locally, add a regression that fails before the
   fix, and inspect other users of the same mechanism. For inherited locks, audit
   every lock owner/error path in the batch rather than fixing one CI symptom at
   a time.
2. **Fragile test:** replace millisecond sleeps with explicit expired deadlines,
   readiness handshakes or controlled descriptors. Use bounded repeated runs of
   the affected test only while a timing concern remains. Raising timeouts or
   repeating the entire suite until green is not a diagnosis.
3. **Runner/script mismatch:** reproduce the outer invocation, exit-code rules,
   shell version and execution identity locally. The same inner script alone
   does not establish equivalent execution. Record ARM64 versus x64 separately;
   request x64 coverage when the change depends on that architecture.
4. **Infrastructure failure with unchanged source:** repair/confirm the external
   cause, then rerun the failed jobs of the exact run instead of dispatching all
   platforms again. Check that it still targets the intended SHA:

   ```sh
   gh run view RUN_ID --json headSha,conclusion,jobs,url
   gh run rerun RUN_ID --failed
   gh run watch RUN_ID --exit-status
   ```

A rerun uses the original commit; it cannot validate a subsequent code fix.
Group related fixes locally and request a new checkpoint for the new SHA only
after the affected checks pass. Record the prior run, root cause, local
reproduction, related-code audit and remaining coverage gap in `--local-results`.
An unexplained second hosted failure in the same batch is a reason to return to
local diagnosis, not to queue a third run automatically.

The v0.4 review exposed a Git test that assumed fixed timing, a Windows wrapper
that leaked a valid partial-result exit code, and inherited Unix locks that
outlived their owners. The first two needed better test/runner coverage; the last
needed a product fix and a family-wide lock audit. Linux/Windows architecture
differences alone did not explain those failures. Tagging the already-green
commit also repeated a whole verification matrix; the release policy below now
avoids that duplicate work.

## Keep evidence tied to the source

Record the commit, whether the source was dirty, the snapshot/archive identity,
OS and CPU architecture, Rust version, full command, skipped stages, status and
log directory. Local snapshots include current edits; remote CI only sees pushed
commits. Re-run affected checks when the source changes after an earlier pass.

If an image, toolchain or VM is unavailable, record **blocked/not run**, the exact
preflight error and the next setup command. Diagnose or repair the local runner
where practical. Do not relabel a cross-check as a runtime pass, and do not
dispatch hosted CI repeatedly to compensate for a broken local setup. A single
explicit checkpoint can cover the remaining gap, with the blocker in its notes.

## Request a hosted integration checkpoint

`workflow_dispatch` is the ordinary remote entry point. It directly expresses a
test request without creating a release identifier. Version tags remain release
intent: `v*.*.*` validates, builds and publishes after every required check passes.
The reusable `verify.yml` runs native Linux, macOS, Windows and an advisory audit
for both callers. Ordinary branch pushes and PR updates do not run this suite.

First commit and push the intended branch through the normal repository workflow.
Then preview or submit the request:

```sh
python3 scripts/run-ci.py --dry-run \
  --local-results 'Linux <tested architecture>: <result>, log: ...; Windows <effectiveTarget>: <result>, log: ...'
python3 scripts/run-ci.py \
  --local-results 'Linux <tested architecture>: <result>, log: ...; Windows <effectiveTarget>: <result>, log: ...'
```

The helper checks that the working tree is clean and that the remote branch head
matches local `HEAD`. It looks up existing manual CI for that exact commit before
dispatching: an active run is monitored instead of duplicated, and a complete
successful run is reused. A new full run after an existing result requires a
specific `--rerun-reason`; this never overrides a run that is still active.
Prefer `gh run rerun RUN_ID --failed` for an unchanged commit whose failure was
external. A newer failed attempt must not be hidden by an older success.

```sh
python3 scripts/run-ci.py --dry-run --rerun-reason 'Specific unresolved concern' \
  --local-results 'Prior run: ...; root cause: ...; local reproduction and fix: ...; remaining gap: ...'
```

The helper does not push, create a tag, or retry a dispatch. For a
detached checkout, explicitly supply `--ref BRANCH`. `--repo OWNER/REPO` can
override GitHub CLI's repository selection. The equivalent direct command is:

```sh
gh workflow run ci.yml --ref YOUR_PUSHED_BRANCH \
  -f expected_sha=FULL_40_CHARACTER_COMMIT_SHA \
  -f local_results='Linux: ...; Windows: ...; logs or blockers: ...'
```

`--ref` is a branch name, not an arbitrary commit SHA. Each platform job checks
the event SHA against `expected_sha` before testing and checks out that event
commit. If the branch moved, the jobs **fail**; they do not silently skip or test
a different revision. `local_results` is an operator-supplied note, not a proof
that local tests passed.

Use the run URL/ID returned by the CLI. If the CLI does not return one, list
`workflow_dispatch` runs for the exact commit as printed by the helper, inspect
the timestamps and select the request you just made. Do not choose the latest
run across the repository. Finish with:

```sh
gh run watch RUN_ID --exit-status
gh run view RUN_ID --json headSha,event,conclusion,jobs,url
```

Check `headSha`, all three platform jobs and the dependency audit. A green run
for another commit is not evidence for your changes. Same-branch manual requests
cancel older in-progress manual CI; release jobs are not cancelled by that policy.

## Activation, release and dependency policy

The manual workflow must exist on the repository's default branch before GitHub
will accept dispatches. These changes take effect after they reach `main`;
having a workflow only in a local or feature checkout is insufficient. Authentication
needs permission to run Actions. This document does not authorize publishing a
release merely to validate workflow changes.

The release workflow remains triggered by `v*.*.*` tags and verifies that the tag
matches `Cargo.toml`. The release preflight can reuse the most recently updated
manual `ci.yml` run for the **identical full commit SHA in this repository**.
All four named jobs (Linux, macOS, Windows and audit), their SHA guards and their
actual verification steps must have succeeded in the inspected run attempt
within the last seven days. A green top-level run with skipped/missing checks,
a fork/PR run, another SHA, or an old result is not sufficient.

When that evidence is complete, the release runs a **fresh dependency audit**
and reuses the platform tests. Without eligible evidence, it runs the complete
shared verification workflow, including its audit. API errors and an active
same-commit CI stop the cheap preflight; finish/inspect the existing run and retry
the failed release job when ready, rather than launch a duplicate matrix.
Preflight records the reused run URL and attempt in the Actions summary.

Both paths meet at an explicit verification gate. Failure, cancellation or an
unexpected skip blocks all builds. Every platform then builds and smoke-tests
its actual release binary before uploading; publication depends on every build.
Preserve this complete evidence/audit/build chain when editing workflows. Do not
create test-only version tags or treat successful compilation as runtime proof.

Settle version, code, scripts and documentation before requesting the final CI,
then tag that exact green commit after release approval. A commit made after CI,
even for a version bump, has a different SHA and cannot reuse the earlier result.

The dependency audit remains weekly and manually callable, and is included in
every hosted checkpoint/release. The weekly default-branch audit is the deliberate
low-frequency remote exception because advisories can change without a source
change. It does not replace auditing a release candidate's exact lockfile.

At the 2026-09-08 inspection, `main` had neither legacy branch protection nor
active rulesets. No remote rules were changed. If required checks are added,
select the actual caller/callee check names from a successful manual run; do not
require an obsolete push-only check. Merge queues and organization-required
workflows need a separate `merge_group`/ruleset design before enabling them.

## References

- [GitHub: manually running a workflow](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)
- [GitHub CLI: workflow run and branch selection](https://cli.github.com/manual/gh_workflow_run)
- [GitHub: troubleshooting required checks, including skipped jobs](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks)
- [GitHub: inspecting workflow runs](https://docs.github.com/en/rest/actions/workflow-runs#list-workflow-runs-for-a-workflow)
- [GitHub: inspecting jobs of a specific run attempt](https://docs.github.com/en/rest/actions/workflow-jobs#list-jobs-for-a-workflow-run-attempt)
