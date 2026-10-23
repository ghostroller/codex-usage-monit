#!/usr/bin/env python3
"""Local stdio fixture; its manifest and every mutation live in a test home."""
import json
import os
from pathlib import Path
import sqlite3
import sys


def main():
    assert sys.argv[1:] == ["app-server", "--listen", "stdio://"]
    home = Path(os.environ["CODEX_HOME"]).resolve()
    manifest = json.loads((home / "management-fixture.json").read_text())
    threads = manifest["threads"]
    for raw in sys.stdin:
        request = json.loads(raw)
        method = request["method"]
        with (home / "management-requests.jsonl").open("a") as log:
            log.write(json.dumps(request) + "\n")
        if "id" not in request:
            assert method == "initialized"
            continue
        params = request.get("params", {})
        try:
            if method == "initialize":
                assert params["capabilities"]["experimentalApi"] is True
                result = {"userAgent": "session-delete-fixture"}
            elif method == "thread/read":
                target = threads[params["threadId"]]
                if not Path(target["path"]).exists():
                    print(json.dumps({"id": request["id"], "error": {
                        "code": -32600,
                        "message": "no rollout found for thread id " + target["id"],
                    }}), flush=True)
                    continue
                if manifest.get("mismatched_identity"):
                    target = dict(target, id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
                result = {"thread": target}
            elif method == "thread/list":
                assert params["ancestorThreadId"] == manifest["root"]
                assert set(params["sourceKinds"]) == {
                    "cli", "vscode", "exec", "appServer", "subAgent",
                    "subAgentReview", "subAgentCompact", "subAgentThreadSpawn",
                    "subAgentOther", "unknown",
                }
                assert params["useStateDbOnly"] is True
                matches = [thread for thread in threads.values()
                           if thread.get("parentThreadId") == manifest["root"]
                           and thread["archived"] == params["archived"]
                           and Path(thread["path"]).exists()]
                matches.sort(key=lambda thread: thread["id"])
                offset = int(params.get("cursor") or "0")
                result = {"data": matches[offset:offset + 1],
                          "nextCursor": str(offset + 1) if offset + 1 < len(matches) else None}
            elif method == "thread/delete":
                assert params == {"threadId": manifest["root"]}
                # The separate server process must see the already committed
                # evidence, not a caller's uncommitted SQLite transaction.
                ledger = Path(manifest["ledger"])
                with sqlite3.connect(ledger.as_uri() + "?mode=ro", uri=True) as db:
                    records = [json.loads(row[0]) for row in db.execute("SELECT payload FROM sessions")]
                assert {record["threadId"] for record in records} == set(threads)
                assert all(record["status"] == "pending" for record in records)
                assert sum(call["tokens"]["totalTokens"] for record in records
                           for call in record["calls"]) == manifest["tokens"]
                for thread in threads.values():
                    source = Path(thread["path"]).resolve()
                    assert source.is_relative_to(home)
                    source.rename(home / ("retired-" + thread["id"]))
                result = {"unrecognized": True} if manifest.get("invalid_delete_reply") else {}
            else:
                raise AssertionError("unapproved method " + method)
            print(json.dumps({"id": request["id"], "result": result}), flush=True)
        except Exception as error:
            print(json.dumps({"id": request["id"], "error": {
                "code": -32602, "message": "fixture rejected request: " + str(error),
            }}), flush=True)


if __name__ == "__main__":
    main()
