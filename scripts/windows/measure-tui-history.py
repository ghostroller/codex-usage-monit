#!/usr/bin/env python3
"""Opt-in synthetic comparison; no real history, network, CI, or release actions.

Run --phase plan before reserving a quiet native Windows measurement window.
Then prepare (Cargo-managed ignored helpers), pilot, sample, and summarize.
Product binaries are copied and hashed before helper builds, never rebuilt here.
"""
from __future__ import annotations

import argparse
import ctypes
import datetime as dt
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import statistics
import subprocess
import tarfile
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
MARKER = "codex-usage-monit synthetic measurement v1\n"
SCENARIOS = ("fresh_local", "initialized_local", "three_remotes_all", "three_remotes_background")
SEED_TEST = "tui::tests::prepare_synthetic_history_measurement_fixture"


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False), encoding="utf-8")


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def capture(command, env=None):
    return subprocess.check_output(command, cwd=ROOT, env=env, text=True, encoding="utf-8", errors="replace").strip()


def marked(path):
    path.mkdir(parents=True, exist_ok=False)
    (path / "synthetic-measurement.txt").write_bytes(MARKER.encode("utf-8"))


def snapshot(path):
    return {p.relative_to(path).as_posix(): sha(p) for p in sorted(path.rglob("*")) if p.is_file()}


def state_size(path):
    files = [p for p in path.rglob("*") if p.is_file()]
    return {"files": len(files), "bytes": sum(p.stat().st_size for p in files)}


def completed(result):
    return (result.get("exitCode") == 0 and not result.get("driverError") and not result.get("harnessTimedOut")
            and result.get("readerDrained") is True and (result.get("productCleanup") is None or result["productCleanup"].get("confirmedExited") is True))


def trace_records(path):
    events, errors = [], []
    if path.exists():
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError as error:
                errors.append({"line": number, "error": str(error)})
    return events, errors


def system_cpu():
    if os.name != "nt":
        return None
    values = [ctypes.c_uint64() for _ in range(3)]
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    if kernel.GetSystemTimes(*(ctypes.byref(value) for value in values)):
        return [value.value for value in values]
    return None


class WindowsResources:
    """Best-effort process counters; transfer bytes are NOT physical disk I/O."""
    def __init__(self, pid):
        self.handle = None
        self.last = {}
        self.errors = []
        self.poll_count = 0
        if os.name != "nt":
            self.errors.append("Windows process counters unavailable")
            return
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.psapi = ctypes.WinDLL("psapi", use_last_error=True)
        self.kernel.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
        self.kernel.OpenProcess.restype = ctypes.c_void_p
        self.kernel.CloseHandle.argtypes = [ctypes.c_void_p]
        self.handle = self.kernel.OpenProcess(0x1000 | 0x10 | 0x1 | 0x00100000, False, pid)
        if not self.handle:
            self.errors.append(f"OpenProcess: {ctypes.get_last_error()}")

    def poll(self):
        if not self.handle:
            return
        self.poll_count += 1
        class Memory(ctypes.Structure):
            _fields_ = [("cb", ctypes.c_uint32), ("page_faults", ctypes.c_uint32)] + [(name, ctypes.c_size_t) for name in
                ("peak_working_set", "working_set", "peak_paged", "paged", "peak_nonpaged", "nonpaged", "pagefile", "peak_pagefile")]
        class Io(ctypes.Structure):
            _fields_ = [(name, ctypes.c_uint64) for name in ("read_operations", "write_operations", "other_operations", "read_transfer_bytes", "write_transfer_bytes", "other_transfer_bytes")]
        mem, io = Memory(), Io()
        mem.cb = ctypes.sizeof(mem)
        self.psapi.GetProcessMemoryInfo.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32]
        self.kernel.GetProcessIoCounters.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        if self.psapi.GetProcessMemoryInfo(self.handle, ctypes.byref(mem), mem.cb):
            self.last["peakWorkingSetBytes"] = max(self.last.get("peakWorkingSetBytes", 0), mem.peak_working_set)
        if self.kernel.GetProcessIoCounters(self.handle, ctypes.byref(io)):
            self.last["io"] = {name: getattr(io, name) for name, _ in io._fields_}
        times = [ctypes.c_uint64() for _ in range(4)]
        self.kernel.GetProcessTimes.argtypes = [ctypes.c_void_p] * 5
        if self.kernel.GetProcessTimes(self.handle, *(ctypes.byref(value) for value in times)):
            self.last["cpuSeconds"] = (times[2].value + times[3].value) / 10_000_000
        self.last["lastPollMonotonic"] = time.monotonic()

    def finish(self):
        try:
            self.poll()
        except Exception as error:
            self.errors.append(repr(error))
        finally:
            if self.handle:
                self.kernel.CloseHandle(self.handle)
                self.handle = None
        return {"values": self.last, "errors": self.errors, "pollCount": self.poll_count,
                "queueWaitMaximumSeconds": 0.1,
                "note": "Approximate polling: output can shorten the queue wait. Last available process counters; transfer bytes include non-disk I/O; peak working set is not allocator peak."}

    def terminate(self):
        if not self.handle:
            return {"confirmedExited": False, "reason": "no product process handle"}
        self.kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        if self.kernel.WaitForSingleObject(self.handle, 0) == 0:
            return {"confirmedExited": True, "alreadyExited": True}
        self.kernel.TerminateProcess.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        requested = bool(self.kernel.TerminateProcess(self.handle, 1))
        error = None if requested else ctypes.get_last_error()
        wait_result = self.kernel.WaitForSingleObject(self.handle, 5000)
        return {"confirmedExited": wait_result == 0, "terminationRequested": requested,
                "terminationError": error, "waitResult": wait_result}


def run(command, env, log, timeout=240):
    started = time.monotonic()
    cpu_before = system_cpu()
    process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, encoding="utf-8", errors="replace")
    output = queue.Queue()
    def drain():
        try:
            for line in process.stdout:
                output.put(line)
        finally:
            output.put(None)
    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    resources = None
    eof = False
    timed_out = False
    failure = None
    cleanup = None
    resource_result = None
    try:
        with log.open("w", encoding="utf-8") as sink:
            while not eof or process.poll() is None:
                if time.monotonic() - started > timeout:
                    timed_out = True
                    sink.write(f"\nMeasurement helper exceeded {timeout}s\n")
                    break
                try:
                    line = output.get(timeout=0.1)
                except queue.Empty:
                    line = ""
                if line is None:
                    eof = True
                elif line:
                    sink.write(line)
                    sink.flush()
                    if "N2_PID=" in line and resources is None:
                        resources = WindowsResources(int(line.split("N2_PID=", 1)[1].strip()))
                if resources:
                    resources.poll()
    except BaseException as error:
        failure = repr(error)
    finally:
        # Hold the exact product handle while terminating it; killing the helper
        # first would bypass its Rust cleanup guard and could orphan the TUI.
        if resources:
            try:
                cleanup = resources.terminate()
                if cleanup.get("terminationRequested") and not timed_out and not failure:
                    failure = "Product still running after helper completion; exact process terminated"
            except Exception as error:
                cleanup = {"confirmedExited": False, "error": repr(error)}
        elif timed_out or failure:
            cleanup = {"confirmedExited": False, "reason": "no product PID observed"}
        try:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
        except Exception as error:
            failure = (failure or "") + f"; helper cleanup: {error!r}"
        reader.join(timeout=5)
        if not reader.is_alive():
            process.stdout.close()
        if resources:
            resource_result = resources.finish()
    cpu_after = system_cpu()
    busy = None
    if cpu_before and cpu_after:
        idle, kernel, user = [after - before for before, after in zip(cpu_before, cpu_after)]
        busy = (kernel + user - idle) / (kernel + user) if kernel + user else None
    return {"command": command, "exitCode": process.poll(), "wallSeconds": time.monotonic() - started,
            "resources": resource_result, "systemCpuBusyFraction": busy, "driverError": failure,
            "harnessTimedOut": timed_out, "productCleanup": cleanup,
            "readerDrained": not reader.is_alive()}


def plan(args):
    if args.samples < 10:
        raise ValueError("formal sampling requires at least 10 repetitions per condition")
    args.output.mkdir(parents=True, exist_ok=False)
    work = args.temp_root / args.output.name
    marked(work)
    binaries = {}
    for label, binary in (("baseline", args.baseline_binary), ("candidate", args.candidate_binary)):
        if not binary or not binary.is_file():
            raise ValueError(f"missing {label} binary; provide a previously verified product build")
        destination = args.output / "binaries" / (label + ".exe")
        destination.parent.mkdir(exist_ok=True)
        shutil.copyfile(binary, destination)
        binaries[label] = {"originalPath": str(binary), "path": str(destination), "sha256": sha(destination), "bytes": destination.stat().st_size}
    now = dt.datetime.now(dt.timezone.utc).replace(second=0, microsecond=0) - dt.timedelta(minutes=5)
    protocol = {"version": 1, "createdAt": dt.datetime.now(dt.timezone.utc).isoformat(),
        "head": capture(["git", "-c", f"safe.directory={ROOT.as_posix()}", "rev-parse", "HEAD"]),
        "baselineRevision": args.baseline_revision, "candidateRevision": args.candidate_revision,
        "binaries": binaries, "work": str(work), "observedAt": now.isoformat(), "samplesPerCondition": args.samples,
        "scenarios": list(SCENARIOS), "order": "scenario, repetition; alternate baseline/candidate order each repetition",
        "profile": "Unknown until associated original build evidence is recorded; source buildId does not prove compiler/profile equivalence.",
        "firstLaunch": "Each row launches a new process; repetition 0 is retained separately as the first formal launch of that condition, after identity probes/pilots. Fresh means empty application state, never cold OS cache.",
        "initialDiagnosticDeadlineSeconds": 30, "steadyDiagnosticDeadlineSeconds": 55, "formalConptyDeadlineSeconds": 8,
        "instrumentation": "Identical trace/debug logs; screen and approximate process-counter polling while running, locked logs read only after exit. Formal uninstrumented ConPTY remains separate.",
        "steadyObservationSeconds": 45,
        "helperHardDeadlineSeconds": 240,
        "exitCleanupDeadlineSeconds": 5,
        "unmeasured": ["physical disk I/O", "allocator peak", "controlled product build cost", "revision-probe count/time (no existing trace span)", "quota DTO clone time", "system file-cache temperature"],
        "environment": {"TEMP": str(args.temp_root), "TMP": str(args.temp_root), "platform": platform.platform(),
                        "machine": platform.machine(), "python": platform.python_version()},
        "cargoTargetDir": str(args.target_dir), "cargoBuildDir": str(args.build_dir)}
    write_json(args.output / "protocol.json", protocol)
    print(json.dumps({"protocol": str(args.output / "protocol.json"), "work": str(work)}))


def environment(protocol):
    env = os.environ.copy()
    env.update({key: protocol["environment"][key] for key in ("TEMP", "TMP")})
    env.update(CARGO_TARGET_DIR=protocol["cargoTargetDir"], CARGO_BUILD_BUILD_DIR=protocol["cargoBuildDir"])
    return env


def source_build_id(revision):
    archive = subprocess.check_output(["git", "-c", f"safe.directory={ROOT.as_posix()}", "archive", "--format=tar", revision,
                                       "Cargo.toml", "Cargo.lock", "build.rs", "src"], cwd=ROOT)
    with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
        files = {member.name: tree.extractfile(member).read().decode("utf-8").replace("\r\n", "\n").encode("utf-8")
                 for member in tree.getmembers() if member.isfile() and (member.name.endswith(".rs") or member.name in
                    ("Cargo.toml", "Cargo.lock", "src/remote_agent_manager/release_bootstrap.py", "src/remote_agent_manager/release_bootstrap.ps1"))}
    digest = hashlib.sha256()
    for name, data in sorted(files.items()):
        name = name.encode("utf-8")
        digest.update(len(name).to_bytes(8, "little"))
        digest.update(name)
        digest.update(len(data).to_bytes(8, "little"))
        digest.update(data)
    return digest.hexdigest()


def identity(args, protocol):
    env = environment(protocol)
    work = Path(protocol["work"])
    for label, binary in protocol["binaries"].items():
        assert sha(Path(binary["path"])) == binary["sha256"]
        isolated = work / f"info-{label}"
        isolated.mkdir(exist_ok=True)
        info_env = env | {"CODEX_USAGE_MONIT_STATE_DIR": str(isolated / "state"), "CODEX_USAGE_MONIT_CONFIG_DIR": str(isolated / "config"), "CODEX_USAGE_MONIT_CACHE_DIR": str(isolated / "cache")}
        binary["info"] = json.loads(capture([binary["path"], "--codex-home", str(ROOT / "tests/fixtures/codex-home/normal"), "remote-agent", "info", "--sha256"], info_env))
        binary["expectedBuildId"] = source_build_id(protocol[label + "Revision"])
        assert binary["info"]["buildId"] == binary["expectedBuildId"], f"{label} binary does not match the designated source revision"
    write_json(args.output / "protocol.json", protocol)
    print(json.dumps({label: value for label, value in protocol["binaries"].items()}, indent=2))


def prepare(args, protocol):
    env = environment(protocol)
    work = Path(protocol["work"])
    home = work / "codex-home"
    if home.exists():
        assert snapshot(home) == snapshot(ROOT / "tests/fixtures/codex-home/normal"), "existing fixed home differs"
    else:
        shutil.copytree(ROOT / "tests/fixtures/codex-home/normal", home)
    preparation = str(time.time_ns())
    # Keep exact fixture bytes and absolute spelling shared across every row.
    protocol["fixtureHome"] = str(home)
    protocol["firstLaunch"] = "Each row launches a new process; repetition 0 is the first formal launch of its condition, after identity probes/pilots. OS cache is uncontrolled; fresh means empty application state."
    protocol["fixtureHashes"] = snapshot(home)
    protocol["plannedObservedAt"] = protocol["observedAt"]
    protocol["observedAt"] = (dt.datetime.now(dt.timezone.utc).replace(second=0, microsecond=0) - dt.timedelta(minutes=5)).isoformat()
    protocol["fixturePreparedAt"] = dt.datetime.now(dt.timezone.utc).isoformat()
    protocol["helperToolchain"] = capture(["rustc", "-Vv"], env)
    protocol["profile"] = "Product source identity verified; use originalBuildEvidence for observed build settings. Unrecorded flags remain unknown."
    protocol["instrumentation"] = "Both products use identical trace/debug logging, screen observations and approximate process-counter polling; output can shorten 100ms queue waits. Windows logs are read only after exit releases their byte-range locks. Formal uninstrumented ConPTY remains separate."
    protocol["steadyObservationSeconds"] = 45
    protocol["helperHardDeadlineSeconds"] = 240
    protocol["initialDiagnosticDeadlineSeconds"] = 30
    protocol["steadyDiagnosticDeadlineSeconds"] = 55
    protocol["exitCleanupDeadlineSeconds"] = 5
    protocol["identity"] = capture(["whoami", "/user"], env) if os.name == "nt" else capture(["id"], env)
    command = ["cargo", "test", "--locked", "--offline", "--lib", "--test", "tui_measurement", "--no-run", "--message-format=json"]
    if args.artifact_log:
        shutil.copyfile(args.artifact_log, args.output / "build.log")
        result = {"reusedCargoArtifactLog": str(args.artifact_log), "sha256": sha(args.artifact_log)}
    else:
        result = run(command, env, args.output / "build.log", timeout=900)
        if not completed(result):
            raise RuntimeError("helper build failed; see build.log")
    artifacts = {}
    for line in (args.output / "build.log").read_text(encoding="utf-8").splitlines():
        try:
            artifact = json.loads(line)
        except json.JSONDecodeError:
            continue
        if artifact.get("reason") == "compiler-artifact" and artifact.get("executable") and artifact.get("profile", {}).get("test"):
            artifacts[artifact["target"]["name"]] = artifact["executable"]
    helpers = args.output / "helpers"
    helpers.mkdir(exist_ok=True)
    protocol["helpers"] = {}
    for name in ("codex_usage_monit", "tui_measurement"):
        supplied = Path(artifacts[name])
        frozen = helpers / (name + supplied.suffix)
        shutil.copyfile(supplied, frozen)
        protocol["helpers"][name] = {"path": str(frozen), "cargoArtifactPath": str(supplied), "sha256": sha(frozen)}
    protocol["helperBuild"] = result
    identity(args, protocol)
    for scenario in SCENARIOS:
        fixture = work / ("templates-" + preparation) / scenario
        marked(fixture)
        if scenario != "fresh_local":
            seed_env = env | {"N2_FIXTURE_ROOT": str(fixture), "N2_CODEX_HOME": str(home), "N2_OBSERVED_AT": protocol["observedAt"],
                "N2_REMOTE_COUNT": "3" if scenario.startswith("three_") else "0", "N2_LOCAL_SELECTED": "1" if scenario.endswith("background") else "0"}
            command = [artifacts["codex_usage_monit"], SEED_TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
            result = run(command, seed_env, args.output / f"prepare-{preparation}-{scenario}.log")
            write_json(args.output / f"prepare-{preparation}-{scenario}.json", result)
            if not completed(result):
                raise RuntimeError(f"fixture preparation failed: {scenario}")
        protocol.setdefault("templates", {})[scenario] = {"path": str(fixture), "hashes": snapshot(fixture), "size": state_size(fixture)}
    write_json(args.output / "protocol.json", protocol)


def sample(args, protocol, pilot):
    env = environment(protocol)
    for value in protocol["binaries"].values():
        assert sha(Path(value["path"])) == value["sha256"], "product binary changed"
    helper = protocol["helpers"]["tui_measurement"]
    assert sha(Path(helper["path"])) == helper["sha256"], "helper changed; prepare a new measurement output"
    prefix = args.pilot_name if pilot else "samples"
    inputs_before = {"home": snapshot(Path(protocol["fixtureHome"])),
                     "templates": {name: snapshot(Path(value["path"])) for name, value in protocol["templates"].items()}}
    expected_inputs = {"home": protocol["fixtureHashes"], "templates": {name: value["hashes"] for name, value in protocol["templates"].items()}}
    if inputs_before != expected_inputs:
        write_json(args.output / f"{prefix}-input-rejection.json", {"expected": expected_inputs, "actual": inputs_before})
        raise ValueError("Fixed fixture inputs changed before sampling; rejected before launching samples")
    repetitions = 1 if pilot else protocol["samplesPerCondition"]
    schedule = [{"scenario": scenario, "repetition": repetition, "binary": label, "status": "not_run"}
                for scenario in SCENARIOS for repetition in range(repetitions)
                for label in (("baseline", "candidate") if repetition % 2 == 0 else ("candidate", "baseline"))]
    manifest = {"protocolSha256": sha(args.output / "protocol.json"), "helperSha256": helper["sha256"],
                "binaries": {label: binary["sha256"] for label, binary in protocol["binaries"].items()},
                "startedAt": dt.datetime.now(dt.timezone.utc).isoformat(), "pilot": pilot,
                "inputsMatchedBefore": True, "schedule": schedule}
    manifest_path = args.output / f"{prefix}-manifest.json"
    if manifest_path.exists():
        raise ValueError("This fixed batch was already started; preserve it and create an explicitly separate batch instead of running until green")
    write_json(manifest_path, manifest)
    row_index = 0
    for scenario in SCENARIOS:
        for repetition in range(repetitions):
            for label in (("baseline", "candidate") if repetition % 2 == 0 else ("candidate", "baseline")):
                if (args.output / "stop-measurement").exists():
                    manifest["abortedReason"] = "explicit stop marker observed between samples; remaining schedule retained"
                    write_json(manifest_path, manifest)
                    return
                identifier = f"{scenario}-{repetition:02}-{label}"
                capture_dir = args.output / prefix / identifier
                capture_dir.mkdir(parents=True, exist_ok=False)
                fixture = Path(protocol["work"]) / prefix / identifier
                shutil.copytree(protocol["templates"][scenario]["path"], fixture)
                before = state_size(fixture)
                sample_env = env | {"N2_FIXTURE_ROOT": str(fixture), "N2_CODEX_HOME": protocol["fixtureHome"],
                    "N2_OBSERVED_AT": protocol["observedAt"], "N2_BINARY": protocol["binaries"][label]["path"],
                    "N2_CAPTURE_DIR": str(capture_dir), "N2_MODE": "steady" if scenario.endswith("background") else "startup"}
                command = [helper["path"], "synthetic_history_sample", "--exact", "--ignored", "--nocapture", "--test-threads=1"]
                schedule[row_index]["status"] = "started"
                write_json(manifest_path, manifest)
                try:
                    result = run(command, sample_env, capture_dir / "harness.log")
                except Exception as error:
                    result = {"command": command, "exitCode": None, "driverError": repr(error)}
                result.update(scenario=scenario, repetition=repetition, binary=label, pilot=pilot,
                    startedState=before, finishedState=state_size(fixture), environment={key: sample_env[key] for key in sample_env if key.startswith("N2_") or key in ("TEMP", "TMP")})
                write_json(capture_dir / "invocation.json", result)
                schedule[row_index]["status"] = "completed" if completed(result) else "failed"
                schedule[row_index]["invocation"] = str(capture_dir / "invocation.json")
                row_index += 1
                write_json(manifest_path, manifest)
                print(f"{prefix} {identifier}: exit {result['exitCode']}", flush=True)
                if result.get("driverError") or not result.get("readerDrained", False) or (result.get("productCleanup") is not None and not result["productCleanup"].get("confirmedExited")):
                    manifest["abortedReason"] = "helper cleanup or launch could not be established; remaining planned samples stay not_run"
                    write_json(manifest_path, manifest)
                    return
    manifest["finishedAt"] = dt.datetime.now(dt.timezone.utc).isoformat()
    manifest["protocolUnchanged"] = sha(args.output / "protocol.json") == manifest["protocolSha256"]
    manifest["helperUnchanged"] = sha(Path(helper["path"])) == helper["sha256"]
    manifest["binariesUnchanged"] = all(sha(Path(value["path"])) == value["sha256"] for value in protocol["binaries"].values())
    inputs_after = {"home": snapshot(Path(protocol["fixtureHome"])),
                    "templates": {name: snapshot(Path(value["path"])) for name, value in protocol["templates"].items()}}
    manifest["fixedInputsUnchanged"] = inputs_after == inputs_before
    if not manifest["fixedInputsUnchanged"]:
        manifest["invalidReason"] = "fixed fixture inputs changed; timings must not support a comparison"
        write_json(args.output / f"{prefix}-input-change.json", {"before": inputs_before, "after": inputs_after})
    write_json(manifest_path, manifest)


def summarize(args, protocol):
    rows = []
    prefix = args.pilot_name if args.phase == "summarize-pilot" else "samples"
    for invocation in sorted((args.output / prefix).glob("*/invocation.json")):
        row = read_json(invocation)
        sample_file = invocation.with_name("sample.json")
        row["sample"] = read_json(sample_file) if sample_file.exists() else None
        trace = invocation.with_name("trace.jsonl")
        events, row["traceParseErrors"] = trace_records(trace)
        telemetry, row["eventParseErrors"] = trace_records(invocation.with_name("events.jsonl"))
        starts = {event["spanId"]: event.get("fields", {}) for event in events if event.get("event") == "span_start"}
        for event in events:
            if event.get("event") == "span_finish":
                event["fields"] = starts.get(event.get("spanId"), {}) | event.get("fields", {})
        initial_event = next((event for event in events if event.get("event") == "span_finish" and event.get("stage") == "tui.initial_data_ready"), None)
        row["initialQueryEvidence"] = ({"outcome": initial_event.get("outcome"),
            **{name: initial_event.get("fields", {}).get(name) for name in
               ("snapshotPartial", "historyQueryFailed", "remoteLiveFailed", "remoteOverviewFailed")}} if initial_event else None)
        row["initialQueriesConfirmed"] = bool(initial_event and all(row["initialQueryEvidence"].get(name) is False
            for name in ("historyQueryFailed", "remoteLiveFailed", "remoteOverviewFailed")))
        row["stageTotals"] = {}
        for event in events:
            if event.get("event") != "span_finish":
                continue
            total = row["stageTotals"].setdefault(event["stage"], {"count": 0, "durationUs": 0})
            total["count"] += 1
            total["durationUs"] += event["durationUs"]
        row["phaseStages"] = []
        preceding_account_counts = []
        preceding_quota_sources = []
        for phase in (row["sample"] or {}).get("phases", []):
            stages = {}
            accounts, quota_sources, cache_hits, quota_points, unavailable = [], [], [], [], []
            start, end = [dt.datetime.fromisoformat(phase[name]) for name in ("startedAt", "finishedAt")]
            for event in events:
                if not start <= dt.datetime.fromisoformat(event["at"]) <= end:
                    continue
                if event.get("event") == "span_finish":
                    key = event["stage"] + (":" + event["fields"]["sourceScope"] if "sourceScope" in event.get("fields", {}) else "")
                    item = stages.setdefault(key, {"count": 0, "durationUs": 0})
                    item["count"] += 1
                    item["durationUs"] += event["durationUs"]
                    if event.get("outcome") == "ok":
                        item["successfulCount"] = item.get("successfulCount", 0) + 1
                    if event["stage"] == "history.v2.account_load":
                        value = event["fields"].get("recordCount")
                        if isinstance(value, (int, float)):
                            accounts.append(value)
                        else:
                            unavailable.append("account_load recordCount missing")
                    if event["stage"] == "history.v2.quota_merge":
                        value = event["fields"].get("confirmedSourceCount")
                        if isinstance(value, (int, float)):
                            quota_sources.append(value)
                        else:
                            unavailable.append("quota_merge confirmedSourceCount missing")
                    if event["stage"] == "history.stage_load":
                        cache_hits.append(event["fields"].get("projectionCacheHit"))
                        quota_points.append(event["fields"].get("quotaPointCount"))
            proof = {"accountRecordCounts": accounts, "confirmedQuotaSources": quota_sources,
                     "evidenceUnavailable": unavailable,
                     "projectionCacheHits": cache_hits, "projectionQuotaPointCounts": quota_points,
                     "localQueries": stages.get("history.v2.query:local", {}).get("successfulCount", 0),
                     "backgroundAllQueries": stages.get("history.v2.query:all", {}).get("successfulCount", 0)}
            if phase["phase"] == "idle_cache":
                boundary = start
                quiet_refreshes = 0
                for refresh in telemetry:
                    at = dt.datetime.fromisoformat(refresh["at"])
                    if refresh.get("event") != "tui.refresh" or not start <= at <= end:
                        continue
                    queries = [event for event in events if event.get("event") == "span_finish"
                               and event.get("stage") in ("history.v2.query", "history.v2.account_load")
                               and boundary <= dt.datetime.fromisoformat(event["at"]) <= at]
                    quiet_refreshes += not queries
                    boundary = at
                proof["completedRefreshesWithoutDiskQuery"] = quiet_refreshes
                inferred = quiet_refreshes > 0
                proof["cacheReuseObserved"] = True in cache_hits or inferred
                proof["cacheReuseEvidence"] = ("history.stage_load reported projectionCacheHit" if True in cache_hits else
                    "inferred: completed refresh with no new usage/account disk query" if inferred else "not established; see actual query counts")
            elif phase["phase"] == "quota_changed":
                proof["precedingAccountRecordMaximum"] = max(preceding_account_counts) if preceding_account_counts else None
                proof["quotaMutationObserved"] = bool(preceding_account_counts and any(count > max(preceding_account_counts) for count in accounts))
            elif phase["phase"] == "quota_policy_changed":
                proof["quotaPolicyMutationObserved"] = 3 in preceding_quota_sources and 2 in quota_sources
            row["phaseStages"].append({"phase": phase["phase"], "selectedTab": phase["selectedTab"], "stages": stages, "scenarioEvidence": proof,
                "note": "Span finishes within this phase; concurrent spans can cross phase boundaries."})
            preceding_account_counts.extend(accounts)
            preceding_quota_sources.extend(quota_sources)
        proofs = {phase["phase"]: phase["scenarioEvidence"] for phase in row["phaseStages"]}
        initial = proofs.get("initial", {})
        if row["scenario"].endswith("background"):
            row["scenarioEstablished"] = bool((row["sample"] or {}).get("phasesComplete") and (row["sample"] or {}).get("v2Confirmed")
                and initial.get("localQueries", 0) > 0 and initial.get("backgroundAllQueries", 0) > 0
                and sum(value.get("localQueries", 0) for key, value in proofs.items() if key != "initial") > 0
                and sum(value.get("backgroundAllQueries", 0) for key, value in proofs.items() if key != "initial") > 0
                and any(phase["phase"] == "idle_cache" and phase.get("refreshesInPhase", 0) >= 1 for phase in (row["sample"] or {}).get("phases", []))
                and proofs.get("quota_changed", {}).get("quotaMutationObserved")
                and proofs.get("quota_policy_changed", {}).get("quotaPolicyMutationObserved"))
        else:
            row["scenarioEstablished"] = bool((row["sample"] or {}).get("phasesComplete") and (row["sample"] or {}).get("v2Confirmed"))
            if row["scenario"] == "three_remotes_all":
                row["scenarioEstablished"] &= initial.get("backgroundAllQueries", 0) > 0 and 3 in initial.get("confirmedQuotaSources", [])
        if row["traceParseErrors"] or row["eventParseErrors"]:
            row["scenarioEstablished"] = False
            row["phaseEvidenceIncomplete"] = True
        row["scenarioEstablished"] &= bool((row["sample"] or {}).get("initialTraceConfirmed") and (row["sample"] or {}).get("steadyRefreshesConfirmed"))
        row["scenarioEstablished"] &= row["initialQueriesConfirmed"]
        rows.append(row)
    groups = []
    for scenario in SCENARIOS:
        for binary in ("baseline", "candidate"):
            selected = [row for row in rows if row["scenario"] == scenario and row["binary"] == binary]
            values = sorted(row["sample"]["initialReadySeconds"] for row in selected if row["sample"] and row["sample"]["v2Confirmed"] and row["sample"].get("initialTraceConfirmed") and row["initialQueriesConfirmed"] and row["sample"]["initialReadySeconds"] is not None)
            groups.append({"scenario": scenario, "binary": binary, "samples": len(selected), "validV2Timings": len(values),
                "failures": sum(not completed(row) for row in selected),
                "initialQueryFailureOrUnknown": sum(not row["initialQueriesConfirmed"] for row in selected),
                "establishedScenarios": sum(row["scenarioEstablished"] for row in selected),
                "scenarioNotEstablished": sum(not row["scenarioEstablished"] for row in selected),
                "overEightSeconds": sum((row["sample"] or {}).get("exceededFormalEightSeconds") is True for row in selected),
                "initialReadyUnknown": sum((row["sample"] or {}).get("initialReadySeconds") is None for row in selected),
                "medianSeconds": statistics.median(values) if values else None, "secondSlowestSeconds": values[-2] if len(values) > 1 else None,
                "maximumSeconds": max(values) if values else None, "allSeconds": values})
    manifest = read_json(args.output / f"{prefix}-manifest.json")
    invalid = [name for name in ("inputsMatchedBefore", "fixedInputsUnchanged", "helperUnchanged", "binariesUnchanged", "protocolUnchanged") if manifest.get(name) is not True]
    if any(row["status"] == "not_run" or row["status"] == "started" for row in manifest["schedule"]):
        invalid.append("planned samples incomplete")
    write_json(args.output / (f"{prefix}-summary.json" if args.phase == "summarize-pilot" else "summary.json"), {"groups": groups, "rows": rows,
        "inputBatchValid": not invalid, "inputBatchExclusionReasons": invalid,
        "allScenariosEstablished": bool(rows) and all(completed(row) and row["scenarioEstablished"] for row in rows),
        "batchManifest": manifest, "limitations": protocol["unmeasured"],
        "note": "Input batch validity is separate from scenario validity and harness success. Startup groups include only V2 timings corroborated by the initial trace, even when a later steady phase failed; do not claim steady performance from unestablished phases. Nested stage durations must not be added across stages. Small fixed batches; second-slowest is descriptive, not a stable p95 guarantee. No performance claim from successful exit alone."})
    print(json.dumps(groups, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=("plan", "identity", "prepare", "pilot", "sample", "summarize", "summarize-pilot"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--temp-root", type=Path, default=Path(os.environ.get("TEMP", "/tmp")))
    parser.add_argument("--baseline-binary", type=Path)
    parser.add_argument("--candidate-binary", type=Path)
    parser.add_argument("--artifact-log", type=Path, help="Reuse a completed Cargo JSON build log for these helpers instead of rebuilding")
    parser.add_argument("--pilot-name", default="pilot", help="Explicit separate pilot batch name, preserving earlier instrumentation failures")
    parser.add_argument("--baseline-revision", default="abef359eac136a2b004c7e4b98890cee27bd37fe")
    parser.add_argument("--candidate-revision", default="0656a201edbb5517a9f3ecac78549cbc6d8a15c0")
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target/tui-measurement")
    parser.add_argument("--build-dir", type=Path, default=ROOT / "target/tui-measurement-build")
    args = parser.parse_args()
    if not args.pilot_name.replace("-", "").isalnum() or not args.pilot_name.startswith("pilot"):
        parser.error("pilot-name must begin with pilot and contain only letters, digits or hyphens")
    for key in ("output", "temp_root", "target_dir", "build_dir", "baseline_binary", "candidate_binary", "artifact_log"):
        value = getattr(args, key)
        if value is not None:
            setattr(args, key, value.resolve())
    if args.phase == "plan":
        plan(args)
    else:
        protocol = read_json(args.output / "protocol.json")
        if args.phase == "identity": identity(args, protocol)
        elif args.phase == "prepare": prepare(args, protocol)
        elif args.phase in ("pilot", "sample"): sample(args, protocol, args.phase == "pilot")
        else: summarize(args, protocol)


if __name__ == "__main__":
    main()
