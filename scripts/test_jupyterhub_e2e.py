#!/usr/bin/env python3
"""Focused regression tests for the Task 12 JupyterHub harness."""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace


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


class HubStartupFenceTests(unittest.TestCase):
    def test_full_start_marker_is_required_for_startup_readiness(self) -> None:
        self.assertFalse(
            HARNESS.hub_startup_complete(
                "Hub API listening on http://127.0.0.1:8081/hub/\nNot starting proxy"
            )
        )
        self.assertTrue(
            HARNESS.hub_startup_complete(
                "Initialized 4 spawners in 0.010 seconds\n"
                "JupyterHub is now running, internal Hub API at http://127.0.0.1:8081/hub/"
            )
        )

    def test_hub_config_disallows_background_spawner_initialization(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            proxy = SimpleNamespace(
                public_port=8000,
                api_url="http://127.0.0.1:8001",
            )
            hub = HARNESS.Hub(
                SimpleNamespace(),
                "test-hub",
                proxy,
                8081,
                Path(root) / "hub",
                False,
            )
            config = hub.config.read_text(encoding="utf-8")
        self.assertIn("c.JupyterHub.init_spawners_timeout = -1", config)


class PersistenceTests(unittest.TestCase):
    def test_persisted_route_metadata_must_match_completely(self) -> None:
        expected = {
            "/": {"target": "http://127.0.0.1:8081", "jupyterhub": True},
            "/user/river": {
                "target": "http://127.0.0.1:9000",
                "user": "river",
                "server_name": "",
                "jupyterhub": True,
            },
        }
        reloaded = {
            "/": {
                "target": "http://127.0.0.1:8081",
                "jupyterhub": True,
                "last_activity": "ignored",
            },
            "/user/river": {
                "target": "http://127.0.0.1:9999",
                "user": "river",
                "server_name": "",
                "jupyterhub": True,
                "last_activity": "ignored",
            },
        }
        with self.assertRaisesRegex(HARNESS.GateError, "persisted route metadata"):
            HARNESS.assert_persisted_routes(expected, reloaded)


class RecorderTests(unittest.TestCase):
    def test_missing_required_scenario_fails(self) -> None:
        recorder = HARNESS.Recorder()
        recorder.scenarios = sorted(HARNESS.REQUIRED_SCENARIOS - {"host_routing"})
        with self.assertRaisesRegex(HARNESS.GateError, "missing=.*host_routing"):
            recorder.require_complete()

    def test_extra_scenario_fails(self) -> None:
        recorder = HARNESS.Recorder()
        recorder.scenarios = sorted(HARNESS.REQUIRED_SCENARIOS | {"unexpected"})
        with self.assertRaisesRegex(HARNESS.GateError, "extra=.*unexpected"):
            recorder.require_complete()


class EnvironmentIsolationTests(unittest.TestCase):
    def test_hub_environment_excludes_redis_configuration(self) -> None:
        sentinel = "task12-redis-password-sentinel"
        base = {
            "PATH": os.environ.get("PATH", ""),
            "NO_PROXY": "127.0.0.1",
            "PINGORA_REDIS_URL": f"redis://:{sentinel}@redis:6379/0",
            "PINGORA_REDIS_ROUTE_KEY": "secret-route-key",
            "PINGORA_REDIS_OPERATION_TIMEOUT_MS": "2000",
        }
        hub_env = HARNESS.build_hub_environment(
            base,
            {
                "CONFIGPROXY_AUTH_TOKEN": "proxy-token",
                "JUPYTERHUB_E2E_API_TOKEN": "api-token",
            },
        )
        self.assertEqual(hub_env["PATH"], base["PATH"])
        self.assertFalse(any(name.startswith("PINGORA_REDIS") for name in hub_env))
        self.assertNotIn(sentinel, repr(hub_env))

    def test_redis_credential_is_redacted_from_diagnostics(self) -> None:
        sentinel = "task12-redis-password-sentinel"
        redis_url = f"redis://:{sentinel}@redis:6379/0"
        secrets = HARNESS.environment_secrets(
            {"PINGORA_REDIS_URL": redis_url}, ("proxy-token",)
        )
        rendered = HARNESS.redact_text(
            f"proxy failed for {redis_url}; credential={sentinel}", secrets
        )
        self.assertNotIn(redis_url, rendered)
        self.assertNotIn(sentinel, rendered)
        self.assertIn("<redacted>", rendered)

if __name__ == "__main__":
    unittest.main()
