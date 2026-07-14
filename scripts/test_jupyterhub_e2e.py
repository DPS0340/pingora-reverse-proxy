#!/usr/bin/env python3
"""Focused regression tests for the Task 12 JupyterHub harness."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("jupyterhub-e2e.py")
SPEC = importlib.util.spec_from_file_location("jupyterhub_e2e", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
HARNESS = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = HARNESS
SPEC.loader.exec_module(HARNESS)


class VersionProbeTests(unittest.TestCase):
    def test_cold_probe_waits_for_process_completion_within_one_deadline(self) -> None:
        HARNESS.assert_command_version(
            [
                sys.executable,
                "-c",
                "import time; time.sleep(0.2); print('5.5.0')",
            ],
            "5.5.0",
            "single-user entry point",
            timeout=2,
        )

    def test_wrong_version_fails_closed(self) -> None:
        with self.assertRaisesRegex(HARNESS.GateError, "expected '5.5.0'"):
            HARNESS.assert_command_version(
                [sys.executable, "-c", "print('5.4.0')"],
                "5.5.0",
                "single-user entry point",
                timeout=2,
            )


class ReconciliationTests(unittest.TestCase):
    class Proxy:
        def __init__(self) -> None:
            self.route_data: dict[str, object] = {}

        def routes(self) -> dict[str, object]:
            return dict(self.route_data)

    class Response:
        status_code = 200
        text = ""

    class Hub:
        def __init__(self, proxy: "ReconciliationTests.Proxy", restore: bool) -> None:
            self.proxy = proxy
            self.restore = restore

        def api(self, method: str, path: str):
            if method != "POST" or path != "/proxy":
                raise AssertionError((method, path))
            if self.restore:
                self.proxy.route_data["/user/river"] = {
                    "target": "http://127.0.0.1:9000",
                    "user": "river",
                    "server_name": "",
                    "jupyterhub": True,
                    "last_activity": "ignored",
                }
            return ReconciliationTests.Response()

    def test_reconciliation_restores_the_exact_missing_route_data(self) -> None:
        proxy = self.Proxy()
        hub = self.Hub(proxy, restore=True)
        expected = {
            "target": "http://127.0.0.1:9000",
            "user": "river",
            "server_name": "",
            "jupyterhub": True,
        }
        HARNESS.reconcile_missing_route(hub, proxy, "/user/river", expected, timeout=1)

    def test_no_op_reconciliation_cannot_pass(self) -> None:
        proxy = self.Proxy()
        hub = self.Hub(proxy, restore=False)
        with self.assertRaisesRegex(HARNESS.GateError, "timed out waiting"):
            HARNESS.reconcile_missing_route(
                hub,
                proxy,
                "/user/river",
                {"target": "http://127.0.0.1:9000", "user": "river"},
                timeout=0.2,
            )

    def test_already_satisfied_reconciliation_precondition_cannot_pass(self) -> None:
        proxy = self.Proxy()
        proxy.route_data["/user/river"] = {
            "target": "http://127.0.0.1:9000",
            "user": "river",
        }
        hub = self.Hub(proxy, restore=False)
        with self.assertRaisesRegex(HARNESS.GateError, "precondition failed"):
            HARNESS.reconcile_missing_route(
                hub,
                proxy,
                "/user/river",
                {"target": "http://127.0.0.1:9000", "user": "river"},
                timeout=0.2,
            )


if __name__ == "__main__":
    unittest.main()
