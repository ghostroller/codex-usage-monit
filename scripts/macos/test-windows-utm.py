#!/usr/bin/env python3
"""Run Windows checks in the existing UTM VM, with verified result files.

UTM's exec/file calls can return status zero before a guest result exists. Only
this run's result JSON proves readiness or completion; transport status never
does. Source is copied to a unique guest-local snapshot, avoiding WebDAV locks.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
import zipfile

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_VM = "codex-usage-monit-windows"


class GuestError(RuntimeError):
    pass


def command(arguments, *, data=None, timeout=30):
    try:
        result = subprocess.run(arguments, input=data, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=timeout, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuestError(f"Command could not complete: {arguments[0]}: {error}") from error
    if result.returncode or b"Error from event:" in result.stdout + result.stderr:
        detail = (result.stderr or result.stdout).decode("utf-8", errors="replace").strip()
        raise GuestError(f"{arguments[0]} failed ({result.returncode}): {detail}")
    return result.stdout


def source_archive(repository, destination):
    names = command(["git", "-C", str(repository), "ls-files", "-z", "--cached",
                     "--others", "--exclude-standard"]).split(b"\0")
    with zipfile.ZipFile(destination, "w", zipfile.ZIP_DEFLATED) as archive:
        for raw_name in sorted(set(names)):
            if not raw_name:
                continue
            name = os.fsdecode(raw_name)
            parts = PurePosixPath(name).parts
            if ".." in parts or not parts or name.startswith("/") or "\\" in name or ":" in name:
                raise GuestError(f"Unsafe repository path: {name!r}")
            path = repository / name
            if path.is_symlink():
                raise GuestError(f"Source snapshot requires an ordinary file: {name!r}")
            if not path.exists():  # A tracked deletion is part of the current source state.
                continue
            if not path.is_file():
                raise GuestError(f"Source snapshot requires an ordinary file: {name!r}")
            archive.write(path, name)
    return hashlib.sha256(destination.read_bytes()).hexdigest()


def parse_result(data, run_id, source_hash, expected_scope=None):
    try:
        result = json.loads(data.decode("utf-8-sig"))
    except (UnicodeError, ValueError) as error:
        raise GuestError("Guest result is not valid UTF-8 JSON") from error
    if not isinstance(result, dict):
        raise GuestError("Guest result must be a JSON object")
    if (result.get("schemaVersion") != 1 or result.get("runId") != run_id
            or result.get("sourceArchiveSha256") != source_hash):
        raise GuestError("Guest result does not match this run and source snapshot")
    if result.get("status") not in {"ready", "passed", "failed", "blocked", "timed_out", "cancelled"}:
        raise GuestError("Guest returned an unknown verification status")
    if expected_scope is not None and result.get("scope") != expected_scope:
        raise GuestError("Guest result does not match the requested verification scope")
    if expected_scope is not None and ((result.get("status") == "ready" and expected_scope != "doctor")
                                       or (result.get("status") == "passed" and expected_scope == "doctor")):
        raise GuestError("Guest readiness and verification-pass statuses cannot substitute for each other")
    if expected_scope == "shell-contracts" and result.get("status") == "passed":
        engines = result.get("engines")
        if not isinstance(engines, list) or len(engines) != 2:
            raise GuestError("Passing shell contracts require both PowerShell engines")
        expected = {"windows-powershell": "5.1.", "powershell-7": "7."}
        for engine in engines:
            if not isinstance(engine, dict):
                raise GuestError("Guest returned invalid PowerShell engine evidence")
            prefix = expected.pop(engine.get("engine"), None)
            version = engine.get("version")
            if (prefix is None or not isinstance(version, str) or not version.startswith(prefix)
                    or engine.get("status") != "passed" or engine.get("casesPassed") != 60
                    or not isinstance(engine.get("executable"), str) or not engine["executable"].strip()):
                raise GuestError("Passing shell contracts require 60 cases on PowerShell 5.1 and 7")
    return result


def retrieve_log(executable, vm, result, run_dir):
    required = result["status"] == "passed"
    log_path = result.get("logPath")
    if not isinstance(log_path, str) or not log_path.strip():
        if required:
            raise GuestError("Guest reported passing without a verification log path")
        return
    try:
        log = command([executable, "file", "pull", vm, log_path], timeout=60)
        if not log.strip():
            raise GuestError("Guest verification log is empty")
        (run_dir / "verify.log").write_bytes(log)
    except GuestError as error:
        if required:
            raise GuestError("Guest reported passing but its nonempty validation log could not be retrieved") from error
        print(f"Log retrieval failed: {error}", file=sys.stderr)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vm", default=DEFAULT_VM)
    parser.add_argument("--doctor", action="store_true", help="check existing guest execution and tools")
    parser.add_argument("--start", action="store_true", help="start an existing stopped VM")
    parser.add_argument("--toolchain-home", default="", help="existing guest Rust user's profile, e.g. C:\\Users\\user")
    parser.add_argument("--target", default="")
    parser.add_argument("--profile", choices=("debug", "release"), default="debug")
    parser.add_argument("--test-filter", default="")
    parser.add_argument("--focused", action="store_true", help="run filtered Rust tests only; requires --test-filter")
    parser.add_argument("--shell-contracts", action="store_true", help="run only the 60 PowerShell wrapper contracts on each of Windows PowerShell 5.1 and PowerShell 7")
    parser.add_argument("--pwsh-path", default="", help="existing guest PowerShell 7 executable; required with --shell-contracts")
    parser.add_argument("--timeout", type=int, default=1800, help="guest verification limit in seconds")
    parser.add_argument("--output-dir", type=Path, help="host results directory; defaults to a unique temporary directory")
    args = parser.parse_args(argv)
    if args.timeout < 1 or args.timeout > 7200:
        parser.error("--timeout must be between 1 and 7200 seconds")
    if args.focused and not args.test_filter:
        parser.error("--focused requires --test-filter")
    if args.shell_contracts and (args.doctor or args.focused or args.test_filter or args.target or args.profile != "debug"):
        parser.error("--shell-contracts cannot be combined with --doctor, --focused, --test-filter, --target, or --profile release")
    if args.shell_contracts and not args.pwsh_path.strip():
        parser.error("--shell-contracts requires --pwsh-path pointing to an existing guest PowerShell 7 executable")
    if args.pwsh_path and not args.shell_contracts:
        parser.error("--pwsh-path is only used with --shell-contracts")
    executable = shutil.which("utmctl")
    if not executable:
        raise GuestError("utmctl is unavailable. Install UTM and its command-line tool first.")
    run_id = uuid.uuid4().hex
    output_dir = args.output_dir or Path(tempfile.mkdtemp(prefix="codex-windows-"))
    output_dir.mkdir(parents=True, exist_ok=True)
    # Avoid overwriting earlier evidence when a caller reuses an output directory.
    run_dir = output_dir / run_id
    run_dir.mkdir()
    print(f"Windows validation artifacts: {run_dir}", flush=True)
    try:
        return run_guest(args, executable, run_id, run_dir)
    except (GuestError, KeyboardInterrupt) as error:
        request_path = run_dir / "request.json"
        request = json.loads(request_path.read_text()) if request_path.exists() else {}
        # This one-shot cancellation request is checked by the guest supervisor.
        # If the channel itself fails, its independent deadline still applies.
        if request_path.exists():
            try:
                command([executable, "file", "push", args.vm,
                         "C:\\Windows\\Temp\\codex-usage-monit-" + run_id + ".cancel"],
                        data=b"cancel", timeout=10)
            except GuestError:
                pass
        failure = dict(request, schemaVersion=1, runId=run_id, vm=args.vm, status="blocked", detail=str(error) or "Host interrupted; guest cancellation requested.")
        (run_dir / "result.json").write_text(json.dumps(failure, indent=2) + "\n")
        raise GuestError(f"{failure['detail']} Artifacts: {run_dir}") from error


def run_guest(args, executable, run_id, run_dir):
    vm_status = command([executable, "status", args.vm]).decode().strip()
    if vm_status == "stopped" and args.start:
        command([executable, "start", args.vm])
        vm_status = command([executable, "status", args.vm]).decode().strip()
    if vm_status != "started":
        raise GuestError(f"Existing VM {args.vm!r} is {vm_status!r}; use --start if stopped. No VM was recreated.")
    source_hash = ""
    if not args.doctor:
        source_hash = source_archive(REPOSITORY, run_dir / "source.zip")
    revision = command(["git", "-C", str(REPOSITORY), "rev-parse", "HEAD"]).decode().strip()
    dirty_entries = [entry for entry in command(["git", "-C", str(REPOSITORY), "status", "--porcelain", "-z"]).split(b"\0") if entry]
    source_dirty = {"tracked": any(not entry.startswith(b"?? ") for entry in dirty_entries),
                    "untracked": any(entry.startswith(b"?? ") for entry in dirty_entries)}
    toolchain_match = re.search(r'^channel\s*=\s*"([^"\n]+)"', (REPOSITORY / "rust-toolchain.toml").read_text(), re.MULTILINE)
    if not toolchain_match:
        raise GuestError("Could not read the repository's pinned Rust toolchain")
    scope = ("doctor" if args.doctor else "shell-contracts" if args.shell_contracts else
             "rust-focused" if args.focused else "filtered" if args.test_filter else "full")
    config = dict(vm=args.vm, rustToolchain=toolchain_match.group(1), runId=run_id, mode="doctor" if args.doctor else "verify",
                  toolchainHome=args.toolchain_home, sourceRevision=revision, sourceDirty=source_dirty,
                  sourceArchiveSha256=source_hash, profile=args.profile, target=args.target,
                  testFilter=args.test_filter, focused=args.focused, scope=scope, pwshPath=args.pwsh_path,
                  timeoutSeconds=args.timeout)
    config_bytes = json.dumps(config).encode()
    (run_dir / "request.json").write_bytes(config_bytes)
    guest_prefix = "C:\\Windows\\Temp\\codex-usage-monit-" + run_id
    for suffix, data in [(".ps1", (REPOSITORY / "scripts/windows/utm-guest-run.ps1").read_bytes()),
                         (".config.json", config_bytes)]:
        command([executable, "file", "push", args.vm, guest_prefix + suffix], data=data)
    if not args.doctor:
        command([executable, "file", "push", args.vm, guest_prefix + ".zip"],
                data=(run_dir / "source.zip").read_bytes(), timeout=120)
    deadline = time.monotonic() + (60 if args.doctor else args.timeout + 120)
    command([executable, "exec", args.vm, "--cmd",
             "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
             "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
             "-File", guest_prefix + ".ps1", "-ConfigPath", guest_prefix + ".config.json"],
            timeout=60 if args.doctor else args.timeout + 120)
    last_error = "Guest has not published a result file."
    while time.monotonic() < deadline:
        try:
            data = command([executable, "file", "pull", args.vm, guest_prefix + ".result.json"])
        except GuestError as error:
            last_error = str(error)
            time.sleep(2)
            continue
        # Guest publication is atomic. A published malformed or mismatched
        # result is a hard verification failure, not a reason to wait again.
        result = parse_result(data, run_id, source_hash, scope)
        break
    else:
        raise GuestError(f"No verified guest result before deadline. {last_error} Artifacts: {run_dir}")
    (run_dir / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    if not args.doctor:
        retrieve_log(executable, args.vm, result, run_dir)
    print(json.dumps(result, indent=2), flush=True)
    expected_status = "ready" if args.doctor else "passed"
    return 0 if result["status"] == expected_status else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except GuestError as error:
        print(f"Windows VM validation blocked: {error}", file=sys.stderr)
        sys.exit(2)
