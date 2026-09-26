"""Failure-path contracts for the opt-in synthetic measurement driver."""
import importlib.util
from pathlib import Path
import os
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("measure_tui_history", Path(__file__).parents[1] / "measure-tui-history.py")
DRIVER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DRIVER)


class MeasurementCleanupTests(unittest.TestCase):
    def test_partial_snapshot_is_valid_but_failed_initial_query_is_excluded(self):
        for query_failed, expected in ((False, 1), (True, 0)):
            with self.subTest(query_failed=query_failed), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                capture = root / "samples" / "one"
                DRIVER.write_json(capture / "invocation.json", {"exitCode": 0, "readerDrained": True, "scenario": "fresh_local", "binary": "baseline"})
                DRIVER.write_json(capture / "sample.json", {"v2Confirmed": True, "initialReadySeconds": 1.0, "initialTraceConfirmed": True,
                    "steadyRefreshesConfirmed": True, "phasesComplete": True, "phases": [], "exceededFormalEightSeconds": False})
                event = {"event": "span_finish", "spanId": 1, "stage": "tui.initial_data_ready", "outcome": "partial", "durationUs": 100,
                    "fields": {"snapshotPartial": True, "historyQueryFailed": query_failed, "remoteLiveFailed": False, "remoteOverviewFailed": False}}
                import json
                (capture / "trace.jsonl").write_text(json.dumps(event) + "\n", encoding="utf-8")
                DRIVER.write_json(root / "samples-manifest.json", {**{key: True for key in
                    ("inputsMatchedBefore", "fixedInputsUnchanged", "helperUnchanged", "binariesUnchanged", "protocolUnchanged")}, "schedule": [{"status": "completed"}]})
                with mock.patch("builtins.print"):
                    DRIVER.summarize(SimpleNamespace(output=root, phase="summarize"), {"unmeasured": []})
                summary = DRIVER.read_json(root / "summary.json")
                self.assertEqual(summary["groups"][0]["validV2Timings"], expected)

    def test_marker_bytes_are_identical_on_windows_and_unix(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fixture"
            DRIVER.marked(root)
            self.assertEqual((root / "synthetic-measurement.txt").read_bytes(), DRIVER.MARKER.encode("utf-8"))

    def test_incomplete_trace_is_retained_as_evidence_error(self):
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / "trace.jsonl"
            trace.write_text('{"event":"complete"}\n{"event":', encoding="utf-8")
            records, errors = DRIVER.trace_records(trace)
        self.assertEqual(records, [{"event": "complete"}])
        self.assertEqual(errors[0]["line"], 2)

    def test_prepare_requires_complete_success(self):
        valid = {"exitCode": 0, "readerDrained": True}
        self.assertTrue(DRIVER.completed(valid))
        for changed in ({"exitCode": None}, {"driverError": "failure"}, {"harnessTimedOut": True}, {"readerDrained": False}):
            self.assertFalse(DRIVER.completed(valid | changed))

    def test_timeout_preserves_failure_and_unknown_product_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            result = DRIVER.run([sys.executable, "-c", "import time; time.sleep(30)"], os.environ.copy(), Path(directory) / "timeout.log", timeout=0.05)
        self.assertTrue(result["harnessTimedOut"])
        self.assertFalse(result["productCleanup"]["confirmedExited"])
        self.assertIsNotNone(result["exitCode"])
        self.assertTrue(result["readerDrained"])

    def test_poll_exception_cleans_exact_product_before_finishing_counters(self):
        calls = []

        class FakeResources:
            def __init__(self, pid):
                self.pid = pid

            def poll(self):
                raise RuntimeError("counter failure")

            def terminate(self):
                calls.append(("terminate", self.pid))
                return {"confirmedExited": True}

            def finish(self):
                calls.append(("finish", self.pid))
                return {"values": {}}

        with tempfile.TemporaryDirectory() as directory, mock.patch.object(DRIVER, "WindowsResources", FakeResources):
            result = DRIVER.run([sys.executable, "-u", "-c", "import time; print('N2_PID=42'); time.sleep(30)"], os.environ.copy(), Path(directory) / "poll.log")
        self.assertIn("counter failure", result["driverError"])
        self.assertEqual(calls, [("terminate", 42), ("finish", 42)])
        self.assertTrue(result["productCleanup"]["confirmedExited"])
        self.assertTrue(result["readerDrained"])


if __name__ == "__main__":
    unittest.main()
