#!/usr/bin/env python3
"""Run the real JupyterHub 5.5.0 external-proxy compatibility gate."""

from __future__ import annotations

import argparse
import datetime as dt
import html
import json
import os
import platform
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from urllib.parse import quote, unquote, urlsplit


BUILD_TIMEOUT = 3600
COMMAND_TIMEOUT = 600
CLEANUP_TIMEOUT = 120
READINESS_TIMEOUT = 45
HTTP_TIMEOUT = 4
PROCESS_GRACE = 15
VERSION_PROBE_TIMEOUT = 120
LOG_TAIL_BYTES = 64 * 1024
PINNED_PACKAGES = {
    "ipykernel": "6.30.1",
    "jupyter-server": "2.17.0",
    "jupyterhub": "5.5.0",
    "nbclassic": "1.3.3",
    "requests": "2.32.4",
    "websocket-client": "1.8.0",
}
EXPECTED_JUPYTERHUB_COMMIT = "97b3154610726b5b7d8768f1e89a4d910e002854"
EXPECTED_PYTHON_VERSION = "3.11.2"
JUPYTERHUB_SOURCE_COMMIT_PATH = Path("/opt/jupyterhub-source-commit")
USERS = ("river", "秀樹", "has@", "space user")
REQUIRED_SCENARIOS = frozenset(
    {
        "pinned_python_runtime",
        "proxy_api_add_get_delete",
        "external_proxy_configuration",
        "hub_root_route",
        "hub_user_api_add_get",
        "escaped_route_river",
        "escaped_route_unicode",
        "escaped_route_at_sign",
        "escaped_route_space",
        "login",
        "single_user_page",
        "kernel_websocket_message_flow",
        "hub_restart_existing_route_usable",
        "proxy_restart_backend_state",
        "proxy_route_reconciliation",
        "hub_user_api_delete",
        "host_routing",
        "run_owned_cleanup",
    }
)
HUB_INHERITED_ENVIRONMENT = (
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "NO_PROXY",
    "no_proxy",
    "TZ",
)


class GateError(RuntimeError):
    pass


def redact_text(text: str, secrets_to_redact: tuple[str, ...]) -> str:
    for secret in secrets_to_redact:
        if secret:
            text = text.replace(secret, "<redacted>")
    return text


def environment_secrets(
    environment: dict[str, str], generated: tuple[str, ...]
) -> tuple[str, ...]:
    values = {value for value in generated if value}
    redis_url = environment.get("PINGORA_REDIS_URL")
    if redis_url:
        values.add(redis_url)
        parsed = urlsplit(redis_url)
        for value in (parsed.netloc, parsed.username, parsed.password):
            if value:
                values.add(value)
                values.add(unquote(value))
    return tuple(sorted(values, key=len, reverse=True))


def build_hub_environment(
    inherited: dict[str, str], required: dict[str, str]
) -> dict[str, str]:
    environment = {
        name: inherited[name] for name in HUB_INHERITED_ENVIRONMENT if name in inherited
    }
    environment.update(required)
    return environment


def compose_command() -> list[str]:
    for candidate in (["docker", "compose"], ["docker-compose"]):
        try:
            subprocess.run(
                [*candidate, "version"],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
            )
        except (FileNotFoundError, subprocess.SubprocessError):
            continue
        return candidate
    raise GateError("Docker Compose is required for the JupyterHub E2E gate")


def terminate_group(process: subprocess.Popen[bytes], grace: float = PROCESS_GRACE) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=grace)
        return
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired as error:
        raise GateError(f"process group {process.pid} survived SIGKILL") from error


class CommandRunner:
    def __init__(self) -> None:
        self.active: subprocess.Popen[bytes] | None = None

    def run(
        self,
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        timeout: float,
    ) -> None:
        process = subprocess.Popen(command, cwd=cwd, env=env, start_new_session=True)
        self.active = process
        try:
            status = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            terminate_group(process)
            raise GateError(f"command exceeded {timeout:.0f}s: {command[0]}") from error
        finally:
            self.active = None
        if status != 0:
            raise subprocess.CalledProcessError(status, command)

    def interrupt(self) -> None:
        if self.active is not None:
            terminate_group(self.active)


BUILD_CONTEXT_ROOT_FILES = frozenset(
    {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"}
)
BUILD_CONTEXT_DIRECTORIES = frozenset({"src", "tests", "scripts", "vendor"})


def copy_build_context(workspace: Path, destination: Path) -> None:
    result = subprocess.run(
        [
            "git",
            "-C",
            str(workspace),
            "ls-files",
            "-z",
            "--",
            *sorted(BUILD_CONTEXT_ROOT_FILES),
            *sorted(BUILD_CONTEXT_DIRECTORIES),
        ],
        check=True,
        capture_output=True,
    )
    relative_paths = [
        Path(os.fsdecode(raw)) for raw in result.stdout.split(b"\0") if raw
    ]
    tracked = {path.as_posix() for path in relative_paths}
    missing = sorted(BUILD_CONTEXT_ROOT_FILES - tracked)
    if missing:
        raise GateError(f"required tracked build inputs are missing: {missing!r}")

    for relative in relative_paths:
        if relative.is_absolute() or ".." in relative.parts:
            raise GateError(f"unsafe tracked build path: {relative}")
        if relative.parts[0] not in BUILD_CONTEXT_ROOT_FILES | BUILD_CONTEXT_DIRECTORIES:
            raise GateError(f"tracked path is outside the build allowlist: {relative}")
        source = workspace / relative
        if source.is_symlink():
            raise GateError(
                f"symbolic links are forbidden in the JupyterHub build context: {relative}"
            )
        if not source.is_file():
            raise GateError(f"tracked build input is not a regular file: {relative}")
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)


def docker_ids(command: list[str]) -> list[str]:
    result = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        raise GateError(f"Docker ownership scan failed: {result.stderr.strip()}")
    return [line for line in result.stdout.splitlines() if line]


def cleanup_docker_run(
    compose: list[str], compose_file: Path, workspace: Path, env: dict[str, str], project: str
) -> list[str]:
    failures: list[str] = []
    base = [*compose, "-f", str(compose_file)]
    try:
        down = subprocess.run(
            [*base, "down", "--remove-orphans", "--volumes"],
            cwd=workspace,
            env=env,
            check=False,
            timeout=CLEANUP_TIMEOUT,
        )
        if down.returncode != 0:
            failures.append("compose down failed")
    except (OSError, subprocess.SubprocessError) as error:
        failures.append(f"compose down failed: {error}")

    label = f"com.docker.compose.project={project}"
    scans = (
        (["docker", "ps", "-aq", "--filter", f"label={label}"], ["docker", "rm", "-f"]),
        (["docker", "network", "ls", "-q", "--filter", f"label={label}"], ["docker", "network", "rm"]),
        (["docker", "volume", "ls", "-q", "--filter", f"label={label}"], ["docker", "volume", "rm", "-f"]),
    )
    for scan, remover in scans:
        try:
            for object_id in docker_ids(scan):
                result = subprocess.run(
                    [*remover, object_id], check=False, capture_output=True, timeout=30
                )
                if result.returncode != 0:
                    failures.append(f"failed to remove Docker object {object_id}")
            if docker_ids(scan):
                failures.append(f"Docker objects remain for {label}")
        except (OSError, subprocess.SubprocessError, GateError) as error:
            failures.append(str(error))

    image = env["JUPYTERHUB_E2E_IMAGE"]
    try:
        inspect = subprocess.run(
            ["docker", "image", "inspect", image],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        failures.append(f"failed to inspect run image {image}: {error}")
        return failures
    if inspect.returncode == 0:
        try:
            removed = subprocess.run(
                ["docker", "image", "rm", "-f", image],
                check=False,
                capture_output=True,
                timeout=60,
            )
        except (OSError, subprocess.SubprocessError) as error:
            failures.append(f"failed to remove run image {image}: {error}")
            return failures
        if removed.returncode != 0:
            failures.append(f"failed to remove run image {image}")
    try:
        final_inspect = subprocess.run(
            ["docker", "image", "inspect", image],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        failures.append(f"failed final run image scan {image}: {error}")
        return failures
    if final_inspect.returncode == 0:
        failures.append(f"run image remains: {image}")
    return failures


def supervisor_main() -> int:
    workspace = Path(__file__).resolve().parents[1]
    compose_file = workspace / "compose.test.yml"
    compose = compose_command()
    selection = os.environ.get("STORE_BACKEND")
    if selection is None:
        backends = ("memory", "redis")
    elif selection in {"memory", "redis"}:
        backends = (selection,)
    else:
        raise GateError("STORE_BACKEND must be memory or redis")
    failure_probe = os.environ.get("JUPYTERHUB_E2E_INJECT_FAILURE")
    if failure_probe not in {None, "after-redis-ready"}:
        raise GateError("unsupported JUPYTERHUB_E2E_INJECT_FAILURE value")

    project = f"jupyterhub-e2e-{os.getpid()}-{secrets.token_hex(4)}"
    image = f"pingora-jupyterhub-e2e:{project}"
    runner = CommandRunner()
    previous_handlers: dict[int, object] = {}

    def handle_signal(signum: int, _frame: object) -> None:
        runner.interrupt()
        raise KeyboardInterrupt(f"received signal {signum}")

    for signum in (signal.SIGINT, signal.SIGTERM):
        previous_handlers[signum] = signal.signal(signum, handle_signal)

    primary_error: BaseException | None = None
    cleanup_failures: list[str] = []
    with tempfile.TemporaryDirectory(prefix="jupyterhub-e2e-build-") as build_dir:
        context = Path(build_dir)
        copy_build_context(workspace, context)
        env = os.environ.copy()
        env.update(
            {
                "COMPOSE_PROJECT_NAME": project,
                "JUPYTERHUB_E2E_BUILD_CONTEXT": str(context),
                "JUPYTERHUB_E2E_IMAGE": image,
            }
        )
        base = [*compose, "-f", str(compose_file)]
        try:
            runner.run(
                [*base, "build", "jupyterhub-e2e"],
                cwd=workspace,
                env=env,
                timeout=BUILD_TIMEOUT,
            )
            if "redis" in backends:
                runner.run(
                    [*base, "up", "-d", "redis"],
                    cwd=workspace,
                    env=env,
                    timeout=60,
                )
                deadline = time.monotonic() + 30
                while True:
                    ready = subprocess.run(
                        [*base, "exec", "-T", "redis", "redis-cli", "ping"],
                        cwd=workspace,
                        env=env,
                        check=False,
                        capture_output=True,
                        text=True,
                        timeout=5,
                    )
                    if ready.returncode == 0 and ready.stdout.strip() == "PONG":
                        break
                    if time.monotonic() >= deadline:
                        raise GateError("Redis did not become ready within 30s")
                    time.sleep(0.2)
                if failure_probe == "after-redis-ready":
                    raise GateError("injected failure after Redis readiness")
            for backend in backends:
                print(f"=== JupyterHub 5.5.0 E2E backend={backend} ===", flush=True)
                runner.run(
                    [
                        *base,
                        "run",
                        "--rm",
                        "--no-deps",
                        "-e",
                        f"STORE_BACKEND={backend}",
                        "jupyterhub-e2e",
                    ],
                    cwd=workspace,
                    env=env,
                    timeout=COMMAND_TIMEOUT,
                )
        except BaseException as error:
            primary_error = error
        finally:
            runner.interrupt()
            cleanup_failures = cleanup_docker_run(
                compose, compose_file, workspace, env, project
            )
    for signum, handler in previous_handlers.items():
        signal.signal(signum, handler)
    if cleanup_failures:
        raise GateError("; ".join(cleanup_failures)) from primary_error
    if primary_error is not None:
        raise primary_error
    return 0


def reserve_ports(count: int) -> list[int]:
    sockets = [socket.socket(socket.AF_INET, socket.SOCK_STREAM) for _ in range(count)]
    try:
        for item in sockets:
            item.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 0)
            item.bind(("127.0.0.1", 0))
        ports = [item.getsockname()[1] for item in sockets]
        if len(set(ports)) != count:
            raise GateError("dynamic port reservation returned duplicates")
        return ports
    finally:
        for item in sockets:
            item.close()


class ManagedProcess:
    def __init__(
        self,
        name: str,
        command: list[str],
        env: dict[str, str],
        cwd: Path,
        log_path: Path,
        secrets_to_redact: tuple[str, ...],
    ) -> None:
        self.name = name
        self.log_path = log_path
        self.secrets = secrets_to_redact
        self._log = log_path.open("wb")
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=self._log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        self.pgid = self.process.pid

    def exited(self) -> bool:
        return self.process.poll() is not None

    def tail(self) -> str:
        self._log.flush()
        with self.log_path.open("rb") as stream:
            stream.seek(0, os.SEEK_END)
            size = stream.tell()
            stream.seek(max(0, size - LOG_TAIL_BYTES))
            text = stream.read().decode("utf-8", "replace")
        return redact_text(text, self.secrets)

    def stop_group(self) -> None:
        terminate_group(self.process)
        self._log.close()

    def stop_parent_only(self) -> None:
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=PROCESS_GRACE)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self._log.close()


def wait_for(description: str, function, timeout: float = READINESS_TIMEOUT):
    deadline = time.monotonic() + timeout
    last_error: BaseException | None = None
    while time.monotonic() < deadline:
        try:
            value = function()
            if value is not None and value is not False:
                return value
        except BaseException as error:
            last_error = error
        time.sleep(0.15)
    suffix = f": {last_error}" if last_error else ""
    raise GateError(f"timed out waiting for {description}{suffix}")


class ScenarioRuntime:
    def __init__(self, root: Path, proxy_binary: Path, backend: str) -> None:
        self.root = root
        self.proxy_binary = proxy_binary
        self.backend = backend
        self.run_id = f"task12-{uuid.uuid4().hex}"
        self.proxy_token = secrets.token_urlsafe(32)
        self.api_token = secrets.token_urlsafe(32)
        self.password = secrets.token_urlsafe(24)
        self.secrets = environment_secrets(
            os.environ,
            (self.proxy_token, self.api_token, self.password),
        )
        self.processes: list[ManagedProcess] = []
        self.lingering_groups: set[int] = set()

    def start_process(
        self, name: str, command: list[str], env: dict[str, str], cwd: Path
    ) -> ManagedProcess:
        process = ManagedProcess(
            name,
            command,
            env,
            cwd,
            self.root / f"{name}-{len(self.processes)}.log",
            self.secrets,
        )
        self.processes.append(process)
        return process

    def leaked_pids(self) -> list[int]:
        marker = f"JUPYTERHUB_E2E_RUN_ID={self.run_id}".encode()
        leaked: list[int] = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit() or int(entry.name) == os.getpid():
                continue
            try:
                environ = (entry / "environ").read_bytes().split(b"\0")
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                continue
            if marker in environ:
                leaked.append(int(entry.name))
        return leaked

    def cleanup(self) -> None:
        for process in reversed(self.processes):
            if not process.exited():
                try:
                    process.stop_group()
                except BaseException:
                    pass
        for pgid in self.lingering_groups:
            try:
                os.killpg(pgid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for pid in self.leaked_pids():
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        wait_for("run-owned processes to exit", lambda: not self.leaked_pids(), timeout=10)

    def diagnostics(self) -> str:
        parts = []
        for process in self.processes:
            tail = process.tail() if not process._log.closed else "<log closed>"
            parts.append(f"--- {process.name} (pid {process.process.pid}) ---\n{tail}")
        return "\n".join(parts)


class ExternalProxy:
    def __init__(
        self,
        runtime: ScenarioRuntime,
        name: str,
        public_port: int,
        api_port: int,
        route_key: str,
        host_routing: bool,
    ) -> None:
        self.runtime = runtime
        self.name = name
        self.public_port = public_port
        self.api_port = api_port
        self.route_key = route_key
        self.host_routing = host_routing
        self.process: ManagedProcess | None = None

    @property
    def public_url(self) -> str:
        return f"http://127.0.0.1:{self.public_port}"

    @property
    def api_url(self) -> str:
        return f"http://127.0.0.1:{self.api_port}"

    @property
    def headers(self) -> dict[str, str]:
        return {"Authorization": f"token {self.runtime.proxy_token}"}

    def start(self) -> None:
        env = os.environ.copy()
        env.update(
            {
                "CONFIGPROXY_AUTH_TOKEN": self.runtime.proxy_token,
                "JUPYTERHUB_E2E_RUN_ID": self.runtime.run_id,
                "PINGORA_REDIS_ROUTE_KEY": self.route_key,
            }
        )
        command = [
            str(self.runtime.proxy_binary),
            "--ip",
            "127.0.0.1",
            "--port",
            str(self.public_port),
            "--api-ip",
            "127.0.0.1",
            "--api-port",
            str(self.api_port),
            "--storage-backend",
            self.runtime.backend,
        ]
        if self.host_routing:
            command.append("--host-routing")
        self.process = self.runtime.start_process(
            self.name,
            command,
            env,
            self.runtime.root,
        )

        def ready() -> bool:
            assert self.process is not None
            if self.process.exited():
                raise GateError(f"external proxy exited:\n{self.process.tail()}")
            import requests

            public = requests.get(
                f"{self.public_url}/_chp_healthz", timeout=HTTP_TIMEOUT
            )
            api = requests.get(
                f"{self.api_url}/api/routes",
                headers=self.headers,
                timeout=HTTP_TIMEOUT,
            )
            return (
                public.status_code == 200
                and public.json() == {"status": "OK"}
                and api.status_code == 200
            )

        wait_for(f"{self.name} public and API readiness", ready)

    def stop(self) -> None:
        if self.process is not None:
            self.process.stop_group()

    def restart(self) -> None:
        self.stop()
        self.start()

    def routes(self) -> dict[str, object]:
        import requests

        response = requests.get(
            f"{self.api_url}/api/routes",
            headers=self.headers,
            timeout=HTTP_TIMEOUT,
        )
        if response.status_code != 200:
            raise GateError(f"proxy routes returned {response.status_code}")
        return response.json()

    def api(self, method: str, path: str, **kwargs):
        import requests

        headers = dict(self.headers)
        headers.update(kwargs.pop("headers", {}))
        return requests.request(
            method,
            f"{self.api_url}{path}",
            headers=headers,
            timeout=HTTP_TIMEOUT,
            **kwargs,
        )


class Hub:
    def __init__(
        self,
        runtime: ScenarioRuntime,
        name: str,
        proxy: ExternalProxy,
        hub_port: int,
        directory: Path,
        subdomain: bool,
    ) -> None:
        self.runtime = runtime
        self.name = name
        self.proxy = proxy
        self.hub_port = hub_port
        self.directory = directory
        self.subdomain = subdomain
        self.process: ManagedProcess | None = None
        directory.mkdir(parents=True)
        self.config = directory / "jupyterhub_config.py"
        self._write_config()

    @property
    def public_url(self) -> str:
        host = "hub.localhost" if self.subdomain else "127.0.0.1"
        return f"http://{host}:{self.proxy.public_port}"

    @property
    def direct_url(self) -> str:
        return f"http://127.0.0.1:{self.hub_port}"

    @property
    def api_headers(self) -> dict[str, str]:
        return {"Authorization": f"token {self.runtime.api_token}"}

    def _write_config(self) -> None:
        subdomain_host = self.public_url if self.subdomain else ""
        config = f"""\
import os

c = get_config()
c.JupyterHub.authenticator_class = "dummy"
c.Authenticator.allow_all = True
c.JupyterHub.spawner_class = "localprocess"
c.Spawner.cmd = ["jupyterhub-singleuser"]
c.Spawner.default_url = "/tree"
c.Spawner.http_timeout = 30
c.Spawner.start_timeout = 45
c.JupyterHub.bind_url = {self.public_url + '/'!r}
c.JupyterHub.hub_bind_url = {self.direct_url!r}
c.JupyterHub.hub_connect_url = {self.direct_url!r}
c.JupyterHub.db_url = {('sqlite:///' + str(self.directory / 'jupyterhub.sqlite'))!r}
c.JupyterHub.cookie_secret_file = {str(self.directory / 'jupyterhub_cookie_secret')!r}
c.JupyterHub.pid_file = ""
c.JupyterHub.cleanup_servers = False
c.JupyterHub.cleanup_proxy = False
c.JupyterHub.last_activity_interval = 0
c.JupyterHub.init_spawners_timeout = -1
c.ConfigurableHTTPProxy.should_start = False
c.ConfigurableHTTPProxy.api_url = {self.proxy.api_url!r}
c.ConfigurableHTTPProxy.auth_token = os.environ["CONFIGPROXY_AUTH_TOKEN"]
c.JupyterHub.services = [{{
    "name": "task12-e2e",
    "api_token": os.environ["JUPYTERHUB_E2E_API_TOKEN"],
}}]
c.JupyterHub.load_roles = [{{
    "name": "task12-e2e-role",
    "services": ["task12-e2e"],
    "scopes": ["admin:users", "admin:servers", "proxy"],
}}]
c.JupyterHub.subdomain_host = {subdomain_host!r}
"""
        self.config.write_text(config, encoding="utf-8")

    def start(self) -> None:
        env = build_hub_environment(
            os.environ,
            {
                "CONFIGPROXY_AUTH_TOKEN": self.runtime.proxy_token,
                "HOME": str(self.directory),
                "IPYTHONDIR": str(self.directory / "ipython"),
                "JUPYTERHUB_E2E_API_TOKEN": self.runtime.api_token,
                "JUPYTERHUB_E2E_RUN_ID": self.runtime.run_id,
                "JUPYTER_RUNTIME_DIR": str(self.directory / "runtime"),
            },
        )
        (self.directory / "runtime").mkdir(exist_ok=True)
        self.process = self.runtime.start_process(
            self.name,
            ["jupyterhub", "-f", str(self.config)],
            env,
            self.directory,
        )

        def ready() -> bool:
            assert self.process is not None
            if self.process.exited():
                raise GateError(f"JupyterHub exited:\n{self.process.tail()}")
            import requests

            response = requests.get(
                f"{self.direct_url}/hub/api",
                headers=self.api_headers,
                timeout=HTTP_TIMEOUT,
            )
            return response.status_code == 200

        wait_for(f"{self.name} API readiness", ready)
        wait_for(
            f"{self.name} external-proxy confirmation",
            lambda: "Not starting proxy" in self.process.tail() if self.process else False,
        )
        wait_for(
            f"{self.name} completed startup route reconciliation",
            lambda: hub_startup_complete(self.process.tail()) if self.process else False,
        )

    def stop_for_restart(self) -> None:
        assert self.process is not None
        self.runtime.lingering_groups.add(self.process.pgid)
        self.process.stop_parent_only()

    def stop_final(self) -> None:
        if self.process is not None and not self.process.exited():
            self.process.stop_group()

    def api(self, method: str, path: str, **kwargs):
        import requests

        headers = dict(self.api_headers)
        headers.update(kwargs.pop("headers", {}))
        return requests.request(
            method,
            f"{self.direct_url}/hub/api{path}",
            headers=headers,
            timeout=kwargs.pop("timeout", HTTP_TIMEOUT),
            **kwargs,
        )


class Recorder:
    def __init__(self) -> None:
        self.scenarios: list[str] = []

    def passed(self, scenario: str) -> None:
        if scenario in self.scenarios:
            raise GateError(f"scenario recorded twice: {scenario}")
        self.scenarios.append(scenario)
        print(f"PASS {scenario}", flush=True)

    def require_complete(self) -> list[str]:
        actual = set(self.scenarios)
        missing = sorted(REQUIRED_SCENARIOS - actual)
        extra = sorted(actual - REQUIRED_SCENARIOS)
        if missing or extra:
            raise GateError(f"scenario set mismatch: missing={missing}, extra={extra}")
        return sorted(actual)


def hub_startup_complete(log_tail: str) -> bool:
    return (
        "Initialized " in log_tail
        and " spawners in " in log_tail
        and "JupyterHub is now running," in log_tail
    )


def escaped_user(name: str) -> str:
    return quote(name, safe="")


def expect_status(response, expected: int | tuple[int, ...], description: str) -> None:
    statuses = (expected,) if isinstance(expected, int) else expected
    if response.status_code not in statuses:
        body = response.text[:1000]
        raise GateError(
            f"{description}: expected {statuses}, got {response.status_code}: {body}"
        )


def assert_command_version(
    command: list[str],
    expected: str,
    description: str,
    *,
    timeout: float,
) -> None:
    """Wait for a potentially cold version probe under one overall deadline."""
    process = subprocess.Popen(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    deadline = time.monotonic() + timeout
    while process.poll() is None:
        if time.monotonic() >= deadline:
            terminate_group(process)
            raise GateError(f"timed out waiting for {description} version probe")
        time.sleep(0.05)
    stdout, stderr = process.communicate()
    if process.returncode != 0:
        detail = stderr.strip() or stdout.strip() or f"exit {process.returncode}"
        raise GateError(f"{description} version probe failed: {detail}")
    actual = stdout.strip()
    if actual != expected:
        raise GateError(f"{description} is {actual!r}, expected {expected!r}")


def stable_route_data(route: object) -> dict[str, object] | None:
    if not isinstance(route, dict):
        return None
    return {key: value for key, value in route.items() if key != "last_activity"}


def stable_routes(routes: dict[str, object]) -> dict[str, object]:
    return {key: stable_route_data(value) for key, value in routes.items()}


def assert_persisted_routes(
    expected: dict[str, object], reloaded: dict[str, object]
) -> None:
    actual = stable_routes(reloaded)
    if actual != expected:
        missing = sorted(set(expected) - set(actual))
        extra = sorted(set(actual) - set(expected))
        changed = sorted(
            key
            for key in set(expected) & set(actual)
            if expected[key] != actual[key]
        )
        raise GateError(
            "persisted route metadata mismatch: "
            f"missing={missing}, extra={extra}, changed={changed}"
        )


def reconcile_missing_route(
    hub,
    proxy,
    route_key: str,
    expected_data: dict[str, object],
    *,
    timeout: float = READINESS_TIMEOUT,
) -> None:
    if route_key in proxy.routes():
        raise GateError(f"reconciliation precondition failed: {route_key!r} is present")
    reconcile = hub.api("POST", "/proxy")
    expect_status(reconcile, 200, "Hub proxy reconciliation")

    def exact_route_restored():
        route = stable_route_data(proxy.routes().get(route_key))
        return route if route == expected_data else None

    wait_for(
        f"exact route data for {route_key!r} after reconciliation",
        exact_route_restored,
        timeout=timeout,
    )


def create_and_start_users(hub: Hub, proxy: ExternalProxy) -> None:
    for name in USERS:
        encoded = escaped_user(name)
        expect_status(hub.api("POST", f"/users/{encoded}", json={}), 201, f"create {name}")
        model = hub.api("GET", f"/users/{encoded}")
        expect_status(model, 200, f"get {name}")
        if model.json()["name"] != name:
            raise GateError(f"Hub changed username {name!r}")
        started = hub.api("POST", f"/users/{encoded}/server", json={}, timeout=60)
        expect_status(started, (201, 202), f"start {name}")

        route_key = f"/user/{name}"

        def route_ready():
            route = proxy.routes().get(route_key)
            if isinstance(route, dict) and route.get("user") == name:
                return route
            return None

        wait_for(f"route for {name!r}", route_ready)


def cookie_value(session, name: str) -> str:
    matches = [cookie for cookie in session.cookies if cookie.name == name]
    if not matches:
        raise GateError(f"missing cookie {name}")
    return max(matches, key=lambda cookie: len(cookie.path or "")).value


def login(hub: Hub, username: str):
    import requests

    session = requests.Session()
    login_url = f"{hub.public_url}/hub/login?next=%2Fhub%2Fhome"
    page = session.get(login_url, timeout=HTTP_TIMEOUT)
    expect_status(page, 200, "login page")
    xsrf = cookie_value(session, "_xsrf")
    response = session.post(
        login_url,
        data={
            "_xsrf": html.unescape(xsrf),
            "username": username,
            "password": hub.runtime.password,
        },
        timeout=20,
        allow_redirects=True,
    )
    expect_status(response, 200, "login submission")
    home = session.get(f"{hub.public_url}/hub/home", timeout=HTTP_TIMEOUT)
    expect_status(home, 200, "authenticated Hub home")
    user = session.get(f"{hub.public_url}/hub/api/user", timeout=HTTP_TIMEOUT)
    expect_status(user, 200, "authenticated Hub user API")
    if user.json().get("name") != username:
        raise GateError("authenticated Hub user API returned the wrong user")
    return session


def single_user_page(session, url: str):
    page = session.get(url, timeout=30, allow_redirects=True)
    expect_status(page, 200, "single-user page")
    if not any(marker in page.text for marker in ("Jupyter", "Files", "Notebook")):
        raise GateError("single-user page did not render Jupyter UI")
    return page


def kernel_websocket_flow(session, hub: Hub) -> None:
    import requests
    import websocket

    base = f"{hub.public_url}/user/river"
    xsrf = cookie_value(session, "_xsrf")
    headers = {"X-XSRFToken": xsrf, "Referer": f"{base}/tree"}
    created = session.post(
        f"{base}/api/kernels",
        json={"name": "python3"},
        headers=headers,
        timeout=20,
    )
    expect_status(created, 201, "create kernel")
    kernel_id = created.json()["id"]
    http_channels = f"{base}/api/kernels/{kernel_id}/channels"
    prepared = requests.Request("GET", http_channels).prepare()
    cookie_header = requests.cookies.get_cookie_header(session.cookies, prepared)
    ws_url = "ws" + http_channels[4:]
    ws = websocket.create_connection(
        ws_url,
        cookie=cookie_header,
        origin=hub.public_url,
        timeout=20,
        http_proxy_host=None,
    )
    message_id = uuid.uuid4().hex
    session_id = uuid.uuid4().hex
    request = {
        "channel": "shell",
        "header": {
            "date": dt.datetime.now(dt.timezone.utc).isoformat(),
            "msg_id": message_id,
            "msg_type": "execute_request",
            "session": session_id,
            "username": "river",
            "version": "5.3",
        },
        "parent_header": {},
        "metadata": {},
        "content": {
            "allow_stdin": False,
            "code": "print('task12-kernel-websocket-ok')",
            "silent": False,
            "stop_on_error": True,
            "store_history": False,
            "user_expressions": {},
        },
    }
    ws.send(json.dumps(request))
    reply = stream = idle = False
    deadline = time.monotonic() + 20
    try:
        while time.monotonic() < deadline and not (reply and stream and idle):
            ws.settimeout(min(3, max(0.1, deadline - time.monotonic())))
            try:
                raw = ws.recv()
            except websocket.WebSocketTimeoutException:
                continue
            if isinstance(raw, bytes):
                raw = raw.decode("utf-8")
            message = json.loads(raw)
            if message.get("parent_header", {}).get("msg_id") != message_id:
                continue
            message_type = message.get("header", {}).get("msg_type")
            if message_type == "execute_reply" and message.get("content", {}).get("status") == "ok":
                reply = True
            elif message_type == "stream" and "task12-kernel-websocket-ok" in message.get("content", {}).get("text", ""):
                stream = True
            elif message_type == "status" and message.get("content", {}).get("execution_state") == "idle":
                idle = True
    finally:
        ws.close()
        deleted = session.delete(
            f"{base}/api/kernels/{kernel_id}", headers=headers, timeout=HTTP_TIMEOUT
        )
        expect_status(deleted, 204, "delete kernel")
    if not (reply and stream and idle):
        raise GateError(
            f"kernel WebSocket flow incomplete: reply={reply} stream={stream} idle={idle}"
        )


def stop_and_delete_users(hub: Hub, proxy: ExternalProxy) -> None:
    for name in USERS:
        encoded = escaped_user(name)
        stopped = hub.api("DELETE", f"/users/{encoded}/server", json={}, timeout=45)
        expect_status(stopped, 204, f"stop {name}")
        deleted = hub.api("DELETE", f"/users/{encoded}")
        expect_status(deleted, 204, f"delete {name}")
    wait_for(
        "all user routes to be removed",
        lambda: not any(key.startswith("/user/") for key in proxy.routes()),
    )
    missing = hub.api("GET", "/users/river")
    expect_status(missing, 404, "get deleted river")


def path_routing_scenarios(runtime: ScenarioRuntime, recorder: Recorder) -> None:
    import requests

    public_port, api_port, hub_port = reserve_ports(3)
    proxy = ExternalProxy(
        runtime,
        "path-proxy",
        public_port,
        api_port,
        f"pingora-reverse-proxy:routes:v1:{runtime.run_id}:path",
        False,
    )
    proxy.start()
    hub = Hub(runtime, "path-hub", proxy, hub_port, runtime.root / "path-hub", False)

    crud_path = "/api/routes/task12-crud"
    crud_target = f"http://127.0.0.1:{hub_port}"
    added = proxy.api(
        "POST",
        crud_path,
        json={"target": crud_target, "task12": True},
    )
    expect_status(added, 201, "proxy add route")
    fetched = proxy.api("GET", crud_path)
    expect_status(fetched, 200, "proxy get route")
    if fetched.json().get("target") != crud_target:
        raise GateError("proxy API did not preserve the CRUD target")
    if fetched.json().get("task12") is not True:
        raise GateError("proxy API did not preserve CRUD metadata")
    expect_status(proxy.api("DELETE", crud_path), 204, "proxy delete route")
    expect_status(proxy.api("GET", crud_path), 404, "proxy get deleted route")
    recorder.passed("proxy_api_add_get_delete")

    hub.start()
    recorder.passed("external_proxy_configuration")
    wait_for("Hub root route", lambda: proxy.routes().get("/"))
    root = requests.get(f"{proxy.public_url}/", timeout=HTTP_TIMEOUT, allow_redirects=False)
    expect_status(root, (302, 303), "Hub root through proxy")
    login_page = requests.get(f"{proxy.public_url}/hub/login", timeout=HTTP_TIMEOUT)
    expect_status(login_page, 200, "Hub login through root route")
    recorder.passed("hub_root_route")

    create_and_start_users(hub, proxy)
    recorder.passed("hub_user_api_add_get")
    route_scenarios = {
        "river": "escaped_route_river",
        "秀樹": "escaped_route_unicode",
        "has@": "escaped_route_at_sign",
        "space user": "escaped_route_space",
    }
    sessions = {}
    for name, scenario in route_scenarios.items():
        route = proxy.routes().get(f"/user/{name}")
        if not isinstance(route, dict) or route.get("user") != name:
            raise GateError(f"missing decoded proxy route for {name!r}")
        user_session = login(hub, name)
        routed = user_session.get(
            f"{proxy.public_url}/user/{name}/api",
            timeout=HTTP_TIMEOUT,
            allow_redirects=True,
        )
        expect_status(routed, 200, f"authenticated user route for {name!r}")
        try:
            server_version = routed.json()["version"]
        except (KeyError, TypeError, ValueError) as error:
            raise GateError(f"user route for {name!r} did not reach Jupyter Server") from error
        if server_version != PINNED_PACKAGES["jupyter-server"]:
            raise GateError(
                f"user route for {name!r} reached Jupyter Server {server_version!r}"
            )
        sessions[name] = user_session
        recorder.passed(scenario)

    session = sessions["river"]
    reconciliation_key = "/user/river"
    expected_routes = {
        "/",
        "/user/river",
        "/user/秀樹",
        "/user/has@",
        "/user/space user",
    }
    recorder.passed("login")
    tree_url = f"{hub.public_url}/user/river/tree"
    single_user_page(session, tree_url)
    recorder.passed("single_user_page")
    kernel_websocket_flow(session, hub)
    recorder.passed("kernel_websocket_message_flow")

    hub.stop_for_restart()
    single_user_page(session, tree_url)
    hub.start()
    single_user_page(session, tree_url)
    recorder.passed("hub_restart_existing_route_usable")

    persisted_route_data = stable_routes(proxy.routes())
    if set(persisted_route_data) != expected_routes:
        raise GateError(
            "pre-restart route table mismatch: "
            f"expected={sorted(expected_routes)}, actual={sorted(persisted_route_data)}"
        )
    reconciliation_data = persisted_route_data.get(reconciliation_key)
    if not isinstance(reconciliation_data, dict):
        raise GateError("cannot capture the stable river route before proxy restart")

    proxy.restart()
    restarted_routes = proxy.routes()
    if runtime.backend == "memory" and restarted_routes:
        raise GateError("memory proxy restart unexpectedly retained routes")
    if runtime.backend == "redis":
        assert_persisted_routes(persisted_route_data, restarted_routes)
        single_user_page(session, tree_url)
    recorder.passed("proxy_restart_backend_state")

    if runtime.backend == "redis":
        expect_status(
            proxy.api("DELETE", "/api/routes/user/river"),
            204,
            "remove persisted river route before reconciliation",
        )
    if reconciliation_key in proxy.routes():
        raise GateError("selected reconciliation route is still present")
    reconcile_missing_route(
        hub,
        proxy,
        reconciliation_key,
        reconciliation_data,
    )
    wait_for(
        "all routes after reconciliation",
        lambda: expected_routes.issubset(proxy.routes()),
    )
    single_user_page(session, tree_url)
    recorder.passed("proxy_route_reconciliation")

    stop_and_delete_users(hub, proxy)
    recorder.passed("hub_user_api_delete")
    hub.stop_final()
    proxy.stop()


def host_routing_scenario(runtime: ScenarioRuntime, recorder: Recorder) -> None:
    public_port, api_port, hub_port = reserve_ports(3)
    proxy = ExternalProxy(
        runtime,
        "host-proxy",
        public_port,
        api_port,
        f"pingora-reverse-proxy:routes:v1:{runtime.run_id}:host",
        True,
    )
    proxy.start()
    hub = Hub(runtime, "host-hub", proxy, hub_port, runtime.root / "host-hub", True)
    hub.start()
    encoded = escaped_user("river")
    expect_status(hub.api("POST", f"/users/{encoded}", json={}), 201, "host create river")
    expect_status(
        hub.api("POST", f"/users/{encoded}/server", json={}, timeout=60),
        (201, 202),
        "host start river",
    )
    def host_route_ready():
        route = proxy.routes().get("/river.hub.localhost/user/river")
        if isinstance(route, dict) and route.get("user") == "river":
            return route
        return None

    wait_for("host route for river", host_route_ready)
    session = login(hub, "river")
    page = single_user_page(
        session, f"http://river.hub.localhost:{public_port}/user/river/tree"
    )
    if socket.gethostbyname("river.hub.localhost") != "127.0.0.1":
        raise GateError("river.hub.localhost did not resolve to loopback")
    if "river.hub.localhost" not in page.url:
        raise GateError("single-user host routing did not retain the user host")
    recorder.passed("host_routing")
    expect_status(
        hub.api("DELETE", f"/users/{encoded}/server", json={}, timeout=45),
        204,
        "host stop river",
    )
    wait_for(
        "host route for river to be removed",
        lambda: "/river.hub.localhost/user/river" not in proxy.routes(),
    )
    expect_status(hub.api("DELETE", f"/users/{encoded}"), 204, "host delete river")
    expect_status(hub.api("GET", f"/users/{encoded}"), 404, "host get deleted river")
    hub.stop_final()
    proxy.stop()


def assert_pinned_runtime(recorder: Recorder) -> None:
    from importlib.metadata import version

    actual_python = platform.python_version()
    if actual_python != EXPECTED_PYTHON_VERSION:
        raise GateError(
            f"Python is {actual_python}, expected {EXPECTED_PYTHON_VERSION}"
        )
    for package, expected in PINNED_PACKAGES.items():
        actual = version(package)
        if actual != expected:
            raise GateError(f"{package} is {actual}, expected {expected}")
    try:
        source_commit = JUPYTERHUB_SOURCE_COMMIT_PATH.read_text(encoding="ascii").strip()
    except OSError as error:
        raise GateError(f"cannot read JupyterHub source commit marker: {error}") from error
    if source_commit != EXPECTED_JUPYTERHUB_COMMIT:
        raise GateError(
            f"JupyterHub source commit is {source_commit}, expected {EXPECTED_JUPYTERHUB_COMMIT}"
        )
    assert_command_version(
        ["jupyterhub-singleuser", "--version"],
        "5.5.0",
        "single-user entry point",
        timeout=VERSION_PROBE_TIMEOUT,
    )
    recorder.passed("pinned_python_runtime")


def scenario_main(proxy_binary: Path) -> int:
    backend = os.environ.get("STORE_BACKEND", "")
    if backend not in {"memory", "redis"}:
        raise GateError("scenario STORE_BACKEND must be memory or redis")
    if backend == "redis" and not os.environ.get("PINGORA_REDIS_URL"):
        raise GateError("Redis scenario requires PINGORA_REDIS_URL")
    proxy_binary = proxy_binary.resolve()
    if not proxy_binary.is_file() or not os.access(proxy_binary, os.X_OK):
        raise GateError(f"proxy binary is not executable: {proxy_binary}")
    recorder = Recorder()
    assert_pinned_runtime(recorder)
    root_path: Path | None = None
    runtime: ScenarioRuntime | None = None
    try:
        with tempfile.TemporaryDirectory(prefix=f"jupyterhub-e2e-{backend}-") as root:
            root_path = Path(root)
            runtime = ScenarioRuntime(root_path, proxy_binary, backend)
            try:
                path_routing_scenarios(runtime, recorder)
                host_routing_scenario(runtime, recorder)
            except BaseException:
                print(runtime.diagnostics(), file=sys.stderr)
                raise
            finally:
                runtime.cleanup()
            if runtime.leaked_pids():
                raise GateError(f"run-owned processes remain: {runtime.leaked_pids()}")
        if root_path.exists():
            raise GateError(f"run-owned state directory remains: {root_path}")
        recorder.passed("run_owned_cleanup")
    finally:
        if runtime is not None and runtime.leaked_pids():
            runtime.cleanup()
    verified_scenarios = recorder.require_complete()
    summary = {
        "backend": backend,
        "jupyterhub": "5.5.0",
        "jupyterhub_commit": EXPECTED_JUPYTERHUB_COMMIT,
        "scenarios": verified_scenarios,
    }
    print(f"JUPYTERHUB_E2E_SUMMARY={json.dumps(summary, ensure_ascii=False, sort_keys=True)}")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", action="store_true")
    parser.add_argument("--proxy-binary", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.scenario:
        if args.proxy_binary is None:
            raise GateError("--scenario requires --proxy-binary")
        return scenario_main(args.proxy_binary)
    if args.proxy_binary is not None:
        raise GateError("--proxy-binary is valid only with --scenario")
    return supervisor_main()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except KeyboardInterrupt:
        raise SystemExit(130)
    except BaseException as error:
        print(f"JupyterHub E2E failed: {error}", file=sys.stderr)
        raise SystemExit(1)
