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
smoke checks. The full Windows entry point runs format, Clippy, all Rust test
targets including ConPTY, and CLI smoke checks. Local and hosted jobs call these
same entry points. A filtered run is intentionally narrower and must be reported
as such. The manual real-user-history benchmark remains opt-in.

For platform-sensitive changes, run the affected platform locally during
iteration and complete the relevant local suites at the checkpoint. A purely
documentation change needs link and instruction checks, not three full Rust
builds. Persistence, process management, terminal behavior and dependency changes
usually need the full platform coverage. A Docker container does not by itself
test a live systemd user service, and a Windows type-check on macOS does not test
ConPTY or the Windows executable.

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
matches local `HEAD`. It does not push, create a tag, or retry a dispatch. For a
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
matches `Cargo.toml`. Its build depends on the shared verification workflow,
including the audit; publication depends on every build. Do not bypass these
dependencies or create test-only version tags.

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
