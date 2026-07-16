#!/usr/bin/env python3
"""Focused regression tests for the Task 12 JupyterHub harness."""

from __future__ import annotations

import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock
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

    def test_pinned_runtime_rejects_the_wrong_python_interpreter(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            source_commit = Path(root) / "jupyterhub-source-commit"
            source_commit.write_text(
                HARNESS.EXPECTED_JUPYTERHUB_COMMIT,
                encoding="ascii",
            )
            recorder = SimpleNamespace(passed=lambda _name: None)
            with (
                mock.patch.object(HARNESS, "PINNED_PACKAGES", {}),
                mock.patch.object(
                    HARNESS,
                    "JUPYTERHUB_SOURCE_COMMIT_PATH",
                    source_commit,
                ),
                mock.patch.object(HARNESS, "assert_command_version"),
                mock.patch("platform.python_version", return_value="3.12.12"),
            ):
                with self.assertRaisesRegex(
                    HARNESS.GateError,
                    "Python is 3.12.12, expected 3.11.2",
                ):
                    HARNESS.assert_pinned_runtime(recorder)


class SecurityPinTests(unittest.TestCase):
    SECURITY_FLOORS = {
        "jupyter[-_]server": "2.20.0",
        "pip": "26.1.2",
        "requests": "2.33.0",
        "wheel": "0.46.2",
    }

    @staticmethod
    def version_tuple(version: str) -> tuple[int, ...]:
        return tuple(int(part) for part in version.split("."))

    def assert_security_floors(self, requirements: Path) -> None:
        contents = requirements.read_text(encoding="utf-8")
        for package_pattern, floor in self.SECURITY_FLOORS.items():
            match = re.search(rf"(?m)^{package_pattern}==([0-9.]+)", contents)
            self.assertIsNotNone(match, package_pattern)
            assert match is not None
            self.assertGreaterEqual(
                self.version_tuple(match.group(1)),
                self.version_tuple(floor),
                package_pattern,
            )

    def test_jupyterhub_dependency_inputs_and_lock_meet_security_floors(self) -> None:
        tests = SCRIPT.parent.parent / "tests"
        self.assert_security_floors(tests / "jupyterhub-requirements.in")
        self.assert_security_floors(tests / "jupyterhub-requirements.txt")
        self.assertGreaterEqual(
            self.version_tuple(HARNESS.PINNED_PACKAGES["jupyter-server"]),
            self.version_tuple("2.20.0"),
        )
        self.assertGreaterEqual(
            self.version_tuple(HARNESS.PINNED_PACKAGES["requests"]),
            self.version_tuple("2.33.0"),
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
            def add_bridge(reservation, _unix_path):
                reservation.release()
                return SimpleNamespace()

            proxy = SimpleNamespace(
                public_port=8000,
                api_url="http://127.0.0.1:8001",
            )
            hub = HARNESS.Hub(
                SimpleNamespace(add_bridge=add_bridge),
                "test-hub",
                proxy,
                HARNESS.PortReservation(),
                Path(root) / "hub",
                False,
            )
            config = hub.config.read_text(encoding="utf-8")
        self.assertIn("c.JupyterHub.init_spawners_timeout = -1", config)


class PortReservationTests(unittest.TestCase):
    def test_dynamic_reservation_uses_non_ephemeral_broker_range(self) -> None:
        reservation = HARNESS.PortReservation()
        try:
            self.assertGreaterEqual(reservation.port, 20_000)
            self.assertLessEqual(reservation.port, 29_999)
        finally:
            reservation.release()

    def test_reservation_holds_the_port_until_explicit_release(self) -> None:
        reservation = HARNESS.PortReservation()
        contender = HARNESS.socket.socket(HARNESS.socket.AF_INET, HARNESS.socket.SOCK_STREAM)
        contender.setsockopt(HARNESS.socket.SOL_SOCKET, HARNESS.socket.SO_REUSEADDR, 1)
        with self.assertRaises(OSError):
            contender.bind(("127.0.0.1", reservation.port))
        contender.close()

        port = reservation.port
        reservation.release()
        replacement = HARNESS.socket.socket(HARNESS.socket.AF_INET, HARNESS.socket.SOCK_STREAM)
        try:
            replacement.bind(("127.0.0.1", port))
        finally:
            replacement.close()

    def test_exact_reservation_can_reclaim_a_time_wait_port(self) -> None:
        server = HARNESS.socket.socket(HARNESS.socket.AF_INET, HARNESS.socket.SOCK_STREAM)
        server.setsockopt(HARNESS.socket.SOL_SOCKET, HARNESS.socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", 0))
        port = server.getsockname()[1]
        server.listen(1)
        client = HARNESS.socket.create_connection(("127.0.0.1", port))
        accepted, _ = server.accept()
        accepted.shutdown(HARNESS.socket.SHUT_WR)
        accepted.close()
        self.assertEqual(client.recv(1), b"")
        client.close()
        server.close()

        reservation = HARNESS.PortReservation(port)
        self.addCleanup(reservation.release)
        self.assertEqual(reservation.port, port)

    @unittest.skipUnless(hasattr(HARNESS.socket, "AF_UNIX"), "Unix sockets unavailable")
    def test_tcp_to_unix_bridge_keeps_listener_and_forwards_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            unix_path = Path(root) / "backend.sock"
            backend = HARNESS.socket.socket(HARNESS.socket.AF_UNIX, HARNESS.socket.SOCK_STREAM)
            backend.bind(str(unix_path))
            backend.listen(1)
            backend.settimeout(2)

            def echo() -> None:
                connection, _ = backend.accept()
                with connection:
                    connection.sendall(connection.recv(4))

            thread = threading.Thread(target=echo, daemon=True)
            thread.start()
            reservation = HARNESS.PortReservation()
            bridge = HARNESS.TcpToUnixBridge(reservation, unix_path)
            try:
                with HARNESS.socket.create_connection(("127.0.0.1", reservation.port)) as client:
                    client.sendall(b"ping")
                    self.assertEqual(client.recv(4), b"ping")
                contender = HARNESS.socket.socket(
                    HARNESS.socket.AF_INET, HARNESS.socket.SOCK_STREAM
                )
                contender.setsockopt(
                    HARNESS.socket.SOL_SOCKET, HARNESS.socket.SO_REUSEADDR, 1
                )
                with self.assertRaises(OSError):
                    contender.bind(("127.0.0.1", reservation.port))
                contender.close()
            finally:
                bridge.close()
                backend.close()
                thread.join(timeout=2)
            self.assertFalse(thread.is_alive())


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


class BuildContextTests(unittest.TestCase):
    def make_workspace(self, root: Path) -> Path:
        workspace = root / "workspace"
        workspace.mkdir()
        subprocess.run(["git", "init", "--quiet"], cwd=workspace, check=True)
        for name in (
            "Cargo.toml",
            "Cargo.lock",
            "compose.test.yml",
            "rust-toolchain.toml",
        ):
            (workspace / name).write_text(f"tracked {name}\n", encoding="utf-8")
        for directory, file_name in (
            ("src", "lib.rs"),
            ("tests", "contract.rs"),
            ("scripts", "gate.py"),
            ("vendor", "NOTICE"),
        ):
            (workspace / directory).mkdir()
            (workspace / directory / file_name).write_text(
                f"tracked {directory}/{file_name}\n", encoding="utf-8"
            )
        subprocess.run(["git", "add", "."], cwd=workspace, check=True)
        subprocess.run(
            [
                "git",
                "-c",
                "user.name=JupyterHub Test",
                "-c",
                "user.email=jupyterhub-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
            cwd=workspace,
            check=True,
        )
        return workspace

    def test_build_context_excludes_untracked_workspace_secret(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            workspace = self.make_workspace(Path(root))
            (workspace / "src" / "untracked-secret.txt").write_text(
                "must-not-enter-context", encoding="utf-8"
            )
            destination = Path(root) / "context"
            destination.mkdir()

            HARNESS.copy_build_context(workspace, destination)

            self.assertTrue((destination / "src" / "lib.rs").is_file())
            self.assertFalse((destination / "src" / "untracked-secret.txt").exists())

    @unittest.skipUnless(hasattr(os, "symlink"), "symlinks are unavailable")
    def test_build_context_rejects_a_tracked_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            workspace = self.make_workspace(Path(root))
            os.symlink("lib.rs", workspace / "src" / "linked.rs")
            subprocess.run(["git", "add", "src/linked.rs"], cwd=workspace, check=True)
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=JupyterHub Test",
                    "-c",
                    "user.email=jupyterhub-test@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "tracked symlink",
                ],
                cwd=workspace,
                check=True,
            )
            destination = Path(root) / "context"
            destination.mkdir()

            with self.assertRaisesRegex(HARNESS.GateError, "symbolic links are forbidden"):
                HARNESS.copy_build_context(workspace, destination)

    def test_build_context_uses_head_bytes_instead_of_dirty_tracked_content(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            workspace = self.make_workspace(Path(root))
            (workspace / "src" / "lib.rs").write_text(
                "dirty tracked injection\n", encoding="utf-8"
            )
            destination = Path(root) / "context"
            destination.mkdir()

            HARNESS.copy_build_context(workspace, destination)

            self.assertEqual(
                (destination / "src" / "lib.rs").read_text(encoding="utf-8"),
                "tracked src/lib.rs\n",
            )

    def test_build_context_uses_head_compose_instead_of_dirty_control(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            workspace = self.make_workspace(Path(root))
            (workspace / "compose.test.yml").write_text(
                "dirty compose injection\n", encoding="utf-8"
            )
            destination = Path(root) / "context"
            destination.mkdir()

            HARNESS.copy_build_context(workspace, destination)

            self.assertEqual(
                (destination / "compose.test.yml").read_text(encoding="utf-8"),
                "tracked compose.test.yml\n",
            )

    @unittest.skipUnless(hasattr(os, "symlink"), "symlinks are unavailable")
    def test_build_context_ignores_an_ancestor_symlink_substitution(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            workspace = self.make_workspace(Path(root))
            shutil.rmtree(workspace / "src")
            outside = Path(root) / "outside"
            outside.mkdir()
            (outside / "lib.rs").write_text("ancestor injection\n", encoding="utf-8")
            os.symlink(outside, workspace / "src")
            destination = Path(root) / "context"
            destination.mkdir()

            HARNESS.copy_build_context(workspace, destination)

            self.assertEqual(
                (destination / "src" / "lib.rs").read_text(encoding="utf-8"),
                "tracked src/lib.rs\n",
            )

if __name__ == "__main__":
    unittest.main()
