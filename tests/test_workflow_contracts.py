"""Offline safety contracts for the repository's deliberately small workflows.

These checks inspect the existing block-mapping layout, not arbitrary YAML.
Unsupported layout changes fail closed and need a contract update; actionlint
remains responsible for validating GitHub Actions syntax and expressions.
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest


WORKFLOWS = Path(__file__).resolve().parents[1] / ".github" / "workflows"


def mapping_block(source, name, indent=0):
    lines = source.splitlines()
    pattern = re.compile(rf"^{' ' * indent}{re.escape(name)}:\s*(?:#.*)?$")
    starts = [index for index, line in enumerate(lines) if pattern.fullmatch(line)]
    if len(starts) != 1:
        raise AssertionError(f"expected one block mapping {name!r} at indent {indent}")
    start = starts[0] + 1
    end = start
    while end < len(lines):
        line = lines[end]
        if line.strip() and not line.lstrip().startswith("#"):
            if len(line) - len(line.lstrip()) <= indent:
                break
        end += 1
    return "\n".join(lines[start:end])


def mapping_keys(source, indent):
    keys = []
    for line in source.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if len(line) - len(line.lstrip()) != indent:
            continue
        match = re.fullmatch(rf"{' ' * indent}([A-Za-z_][A-Za-z_0-9-]*):.*", line)
        if match is None:
            raise AssertionError(f"unsupported mapping layout: {line!r}")
        keys.append(match[1])
    return keys


def scalar(source, name, indent):
    values = re.findall(
        rf"^{' ' * indent}{re.escape(name)}:\s*(.+)$", source, re.MULTILINE
    )
    if len(values) != 1:
        raise AssertionError(f"expected one scalar {name!r} at indent {indent}")
    return values[0]


def job_steps(job):
    source = mapping_block(job, "steps", 4)
    return [part for part in re.split(r"(?=^      - )", source, flags=re.MULTILINE) if part.strip()]


def literal_run(step):
    lines = step.splitlines()
    starts = [index for index, line in enumerate(lines) if line == "        run: |"]
    if len(starts) != 1:
        raise AssertionError("expected one literal run script in this step")
    script = []
    for line in lines[starts[0] + 1:]:
        if line.strip() and len(line) - len(line.lstrip()) <= 8:
            break
        script.append(line)
    return textwrap.dedent("\n".join(script))


class WorkflowContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflows = {
            name: (WORKFLOWS / f"{name}.yml").read_text(encoding="utf-8")
            for name in ("ci", "verify", "release", "dependency-audit")
        }

    def jobs(self, workflow):
        return mapping_block(self.workflows[workflow], "jobs")

    def test_every_workflow_has_an_explicit_trigger_contract(self):
        actual = {path.name for pattern in ("*.yml", "*.yaml") for path in WORKFLOWS.glob(pattern)}
        self.assertEqual(actual, {f"{name}.yml" for name in self.workflows})

    def test_ci_only_runs_at_an_explicit_checkpoint(self):
        events = mapping_block(self.workflows["ci"], "on")
        self.assertEqual(mapping_keys(events, 2), ["workflow_dispatch"])
        dispatch = mapping_block(events, "workflow_dispatch", 2)
        inputs = mapping_block(dispatch, "inputs", 4)
        for name in ("expected_sha", "local_results"):
            required = mapping_block(inputs, name, 6)
            self.assertEqual(scalar(required, "required", 8), "true")
            self.assertEqual(scalar(required, "type", 8), "string")
        job = mapping_block(self.jobs("ci"), "verification", 2)
        self.assertEqual(scalar(job, "uses", 4), "./.github/workflows/verify.yml")
        self.assertEqual(scalar(mapping_block(job, "with", 4), "expected_sha", 6),
                         "${{ inputs.expected_sha }}")

    def test_shared_verification_cannot_be_triggered_by_a_push_or_pr(self):
        self.assertEqual(mapping_keys(mapping_block(self.workflows["verify"], "on"), 2),
                         ["workflow_call"])
        jobs = self.jobs("verify")
        self.assertEqual(set(mapping_keys(jobs, 2)),
                         {"verify", "verify-macos", "verify-windows", "dependency-audit"})
        audit = mapping_block(jobs, "dependency-audit", 2)
        self.assertEqual(scalar(audit, "uses", 4), "./.github/workflows/dependency-audit.yml")
        self.assertEqual(scalar(mapping_block(audit, "with", 4), "expected_sha", 6),
                         "${{ inputs.expected_sha }}")

    def test_each_platform_fails_on_wrong_sha_before_checkout(self):
        for job_name in ("verify", "verify-macos", "verify-windows"):
            with self.subTest(job=job_name):
                job = mapping_block(self.jobs("verify"), job_name, 2)
                self.assertNotIn("if", mapping_keys(job, 4))
                self.assertNotIn("needs", mapping_keys(job, 4))
                steps = job_steps(job)
                guard = steps[0]
                self.assertEqual(guard.splitlines()[0], "      - name: Verify the requested commit")
                self.assertNotIn("if", mapping_keys(guard, 8))
                self.assertEqual(scalar(guard, "shell", 8), "bash")
                self.assertEqual(scalar(mapping_block(guard, "env", 8), "EXPECTED_SHA", 10),
                                 "${{ inputs.expected_sha }}")
                self.assertTrue(literal_run(guard))
                checkouts = [step for step in steps if "uses: actions/checkout@" in step]
                self.assertEqual(len(checkouts), 1)
                self.assertEqual(scalar(mapping_block(checkouts[0], "with", 8), "ref", 10),
                                 "${{ github.sha }}")

    @unittest.skipUnless(shutil.which("bash"), "bash is required to execute the SHA guards")
    def test_sha_guard_scripts_reject_invalid_and_mismatched_inputs(self):
        guards = [literal_run(job_steps(mapping_block(self.jobs("verify"), name, 2))[0])
                  for name in ("verify", "verify-macos", "verify-windows")]
        guards.append(literal_run(job_steps(mapping_block(self.jobs("dependency-audit"), "audit", 2))[0]))
        sha = "a" * 40
        cases = [(sha, sha, True), ("b" * 40, sha, False), ("", "", False),
                 ("a" * 39, "a" * 39, False), ("A" * 40, "A" * 40, False),
                 ("$(touch injected)", "$(touch injected)", False)]
        with tempfile.TemporaryDirectory() as directory:
            for index, guard in enumerate(guards):
                for expected, actual, succeeds in cases:
                    with self.subTest(guard=index, expected=expected, actual=actual):
                        environment = {**os.environ, "EXPECTED_SHA": expected, "GITHUB_SHA": actual}
                        result = subprocess.run([shutil.which("bash"), "-e", "-c", guard],
                                                cwd=directory, env=environment, capture_output=True,
                                                text=True, timeout=5)
                        self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                        self.assertFalse((Path(directory) / "injected").exists())

    def test_audit_is_scheduled_manual_or_reused_without_push_noise(self):
        self.assertEqual(set(mapping_keys(mapping_block(self.workflows["dependency-audit"], "on"), 2)),
                         {"schedule", "workflow_dispatch", "workflow_call"})
        audit = mapping_block(self.jobs("dependency-audit"), "audit", 2)
        self.assertNotIn("if", mapping_keys(audit, 4))
        self.assertIn("cargo audit --deny warnings", audit)

    def test_release_tags_preserve_the_full_verification_dependency_chain(self):
        events = mapping_block(self.workflows["release"], "on")
        self.assertEqual(mapping_keys(events, 2), ["push"])
        push = mapping_block(events, "push", 2)
        self.assertEqual(mapping_keys(push, 4), ["tags"])
        self.assertEqual(mapping_block(push, "tags", 4).strip(), '- "v*.*.*"')
        for name, dependency in (("verify", "validate-tag"), ("build", "verify"), ("publish", "build")):
            with self.subTest(job=name):
                job = mapping_block(self.jobs("release"), name, 2)
                self.assertEqual(scalar(job, "needs", 4), dependency)
                self.assertNotIn("if", mapping_keys(job, 4))
        verify = mapping_block(self.jobs("release"), "verify", 2)
        self.assertEqual(scalar(verify, "uses", 4), "./.github/workflows/verify.yml")
        self.assertEqual(scalar(mapping_block(verify, "with", 4), "expected_sha", 6), "${{ github.sha }}")
        tag_check = mapping_block(self.jobs("release"), "validate-tag", 2)
        self.assertIn('test "$GITHUB_REF_NAME" = "v$package_version"', tag_check)

    def test_user_inputs_are_not_interpolated_into_shell_scripts(self):
        linux = mapping_block(self.jobs("verify"), "verify", 2)
        evidence = next(step for step in job_steps(linux) if "Record verification context" in step)
        self.assertEqual(scalar(mapping_block(evidence, "env", 8), "LOCAL_RESULTS", 10),
                         "${{ inputs.local_results }}")
        for source in self.workflows.values():
            lines = source.splitlines()
            for index, line in enumerate(lines):
                if not line.startswith("        run:"):
                    continue
                script = [line]
                for following in lines[index + 1:]:
                    if following.strip() and len(following) - len(following.lstrip()) <= 8:
                        break
                    script.append(following)
                self.assertNotRegex("\n".join(script), r"\$\{\{\s*inputs\.")


if __name__ == "__main__":
    unittest.main()
