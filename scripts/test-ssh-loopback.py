#!/usr/bin/env python3
"""Exercise real OpenSSH and two isolated monitor profiles on this Unix host.

Requires an already built binary and ssh/sshd/ssh-keygen. Only a temporary
loopback listener and temporary keys/configuration are used. No user SSH or
monitor configuration is changed. Artifacts are retained; private keys are not.
"""

import argparse
from datetime import datetime, timedelta, timezone
import getpass
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import subprocess
import tempfile
import time


REPOSITORY = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    programs = {}
    for name in ("ssh", "sshd", "ssh-keygen"):
        programs[name] = shutil.which(name) or shutil.which(name, path="/usr/sbin:/usr/bin")
        if not programs[name]:
            parser.error(f"{name} is required; install it in the test environment first")
    root = Path(tempfile.mkdtemp(prefix="codex-ssh-loopback-", dir="/tmp")).resolve()
    print(f"SSH test artifacts: {root}", flush=True)
    results = {"status": "running", "binary": str(binary),
               "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "binary_version": subprocess.check_output([str(binary), "--version"], text=True).strip(),
               "checks": []}
    server = None
    log = None
    try:
        for name in ("host-key", "client-key"):
            subprocess.run([programs["ssh-keygen"], "-q", "-t", "ed25519", "-N", "",
                            "-f", str(root / name)], check=True, timeout=15)
        (root / "authorized_keys").write_bytes((root / "client-key.pub").read_bytes())
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        (root / "sshd_config").write_text(f"""Port {port}
ListenAddress 127.0.0.1
HostKey {root}/host-key
PidFile {root}/sshd.pid
AuthorizedKeysFile {root}/authorized_keys
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin prohibit-password
AllowUsers {getpass.getuser()}
AllowTcpForwarding no
X11Forwarding no
PermitUserRC no
LogLevel ERROR
""")
        public_key = (root / "host-key.pub").read_text().split()
        (root / "known_hosts").write_text(f"[127.0.0.1]:{port} {' '.join(public_key[:2])}\n")
        (root / "ssh_config").write_text(f"""Host fixture
    HostName 127.0.0.1
    User {getpass.getuser()}
    Port {port}
    IdentityFile {root}/client-key
    IdentitiesOnly yes
    UserKnownHostsFile {root}/known_hosts
    GlobalKnownHostsFile /dev/null
    StrictHostKeyChecking yes
""")
        shim = root / "bin"
        shim.mkdir()
        (shim / "ssh").write_text("#!/bin/sh\nexec " + shlex.join(
            [programs["ssh"], "-F", str(root / "ssh_config")]) + ' "$@"\n')
        (shim / "ssh").chmod(0o700)

        def environment(profile):
            return {f"CODEX_USAGE_MONIT_{name}_DIR": str(root / profile / name.lower())
                    for name in ("STATE", "CONFIG", "CACHE")}

        for profile in ("center", "remote"):
            (root / profile / "codex" / "sessions").mkdir(parents=True)
        workspace = root / "fixture-project"
        workspace.mkdir()
        agent = root / "agent"
        agent.write_text("#!/bin/sh\nexec " + shlex.join([
            "env", *(f"{key}={value}" for key, value in environment("remote").items()),
            str(binary), "--codex-home", str(root / "remote/codex"), "--offline",
            "remote-agent", "export"]) + "\n")
        agent.chmod(0o700)
        records = [json.loads(line) for line in (REPOSITORY /
            "tests/fixtures/codex-home/normal/sessions/rollout-integration.jsonl").read_text().splitlines()]
        anchor = datetime.now(timezone.utc).replace(microsecond=0) - timedelta(hours=2)
        original = datetime.fromisoformat(records[0]["timestamp"].replace("Z", "+00:00"))

        def retime(value):
            if isinstance(value, dict):
                return {key: retime(item) for key, item in value.items()}
            if isinstance(value, list):
                return [retime(item) for item in value]
            if isinstance(value, str) and value.startswith("2026-07-12T"):
                stamp = datetime.fromisoformat(value.replace("Z", "+00:00"))
                return (anchor + (stamp - original)).isoformat()
            return value

        records = retime(records)
        for record in records:
            if record["type"] == "session_meta":
                record["payload"]["cwd"] = str(workspace)
        rollout = root / "remote/codex/sessions/rollout.jsonl"
        rollout.write_text("".join(json.dumps(record) + "\n" for record in records))
        log = (root / "sshd.log").open("w")
        server = subprocess.Popen([programs["sshd"], "-D", "-e", "-f", str(root / "sshd_config")],
                                  stdout=log, stderr=log)
        for _ in range(100):
            if server.poll() is not None:
                raise RuntimeError("temporary sshd failed; inspect sshd.log")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                    break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError("temporary sshd did not start")

        command_environment = {**os.environ, **environment("center"),
                               "PATH": str(shim) + os.pathsep + os.environ["PATH"]}
        counter = 0

        def run(*arguments, accepted=(0, 2)):
            nonlocal counter
            counter += 1
            command = [str(binary), "--codex-home", str(root / "center/codex"),
                       "--offline", "--redact-content", *arguments]
            completed = subprocess.run(command, env=command_environment,
                                       capture_output=True, text=True, timeout=180)
            (root / f"{counter:02d}.json").write_text(json.dumps({
                "command": command, "exit_code": completed.returncode,
                "stdout": completed.stdout, "stderr": completed.stderr,
            }, indent=2) + "\n")
            if completed.returncode not in accepted:
                raise RuntimeError(f"{' '.join(arguments)} failed: {completed.stderr}")
            return completed

        def check(name, condition):
            if not condition:
                raise RuntimeError(f"assertion failed: {name}")
            results["checks"].append(name)
            print(f"PASS {name}", flush=True)

        def sync():
            for _ in range(8):
                completed = run("remote", "sync", "fixture")
                if "status=complete" in completed.stdout:
                    return completed.stdout
            raise RuntimeError("fixture sync did not finish within eight rounds")

        def total(source="all"):
            report = json.loads(run("summary", "--range", "7d", "--source", source,
                                    "--format", "json", "--compact").stdout)
            return int(report["metrics"]["tokenUsage"]["totalTokens"])

        run("remote", "add", "fixture", "--ssh-host", "fixture", "--agent-executable", str(agent))
        run("remote", "pair", "fixture")
        configuration = json.loads(run("remote", "list", "--format", "json").stdout)
        (root / "paired.json").write_text(json.dumps(configuration, indent=2) + "\n")
        source = configuration["hosts"][0]["expectedSource"]["nodeId"]
        check("pairing leaves automatic sync disabled", not configuration["autoSyncEnabled"]
              and not configuration["hosts"][0]["syncEnabled"])
        run("remote", "test", "fixture")
        sync()
        check("bootstrap remote usage", total() == 1500)
        unchanged = sync()
        check("unchanged sync is idempotent", total() == 1500)
        check("unchanged aggregate response fits 8 KiB",
              int(re.search(r"response=(\d+)B", unchanged)[1]) <= 8192)
        shutil.copyfile(rollout, root / "center/codex/sessions/rollout.jsonl")
        total("local")  # Publish the local replica before requesting facts.
        for _ in range(3):
            check("replica fact follow-up has no error", "facts=attention" not in sync())
        check("copied session is counted once", total() == 1500)
        extra = [
            {"timestamp": (anchor + timedelta(minutes=1)).isoformat(), "type": "event_msg",
             "payload": {"type": "task_started", "turn_id": "turn-appended"}},
            {"timestamp": (anchor + timedelta(minutes=1, seconds=1)).isoformat(), "type": "turn_context",
             "payload": {"turn_id": "turn-appended", "model": "gpt-5.3-codex", "effort": "high"}},
            {"timestamp": (anchor + timedelta(minutes=1, seconds=2)).isoformat(), "type": "event_msg",
             "payload": {"type": "token_count", "info": {"total_token_usage": {
                 "input_tokens": 1800, "cached_input_tokens": 300, "output_tokens": 450,
                 "reasoning_output_tokens": 150, "total_tokens": 2250}}}},
        ]
        with rollout.open("a") as output:
            output.write("".join(json.dumps(record) + "\n" for record in extra))
        for _ in range(3):
            check("append fact follow-up has no error", "facts=attention" not in sync())
        check("remote append extends the replica once", total() == 2250)
        check("exact source totals reconcile", total("local") == 1500 and total(source) == 2250)
        run("remote", "source", "exclude", source)
        check("excluded source remains inspectable", total() == 1500 and total(source) == 2250)
        check("excluded source needs no replica facts", "facts=not-needed" in sync())
        check("excluded source can still synchronize", total(source) == 2250)
        run("remote", "source", "include", source)
        check("including source restores union", total() == 2250)
        server.terminate()
        server.wait(timeout=5)
        run("remote", "sync", "fixture", accepted=(1,))
        check("offline remote retains historical totals", total() == 2250)
        run("remote", "remove", "fixture")
        check("removal retains detached source but excludes it from All",
              total() == 1500 and total(source) == 2250)
        results["status"] = "passed"
    except Exception as error:
        results.update(status="failed", error=str(error))
        raise
    finally:
        if server is not None and server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
        if log is not None:
            log.close()
        for name in ("host-key", "client-key"):
            (root / name).unlink(missing_ok=True)
        (root / "result.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
