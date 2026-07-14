<!-- markdownlint-disable MD013 -->

# pingora-reverse-proxy

[![Artifact Hub](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/pingora-reverse-proxy)](https://artifacthub.io/packages/search?repo=pingora-reverse-proxy)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust)](https://www.rust-lang.org/)
[![CHP compatibility](https://img.shields.io/badge/Configurable%20HTTP%20Proxy-5.3.0-2c7fb8)](https://github.com/jupyterhub/configurable-http-proxy/tree/5.3.0)

A dynamic HTTP and WebSocket reverse proxy built with [Pingora](https://github.com/cloudflare/pingora). It implements the route-management API and routing behavior expected by JupyterHub's [Configurable HTTP Proxy (CHP) 5.3.0](https://github.com/jupyterhub/configurable-http-proxy/tree/5.3.0), without requiring Node.js in the proxy process.

> **Project status:** the CHP-compatible runtime, in-memory route store, management API, TLS listeners, metrics, graceful shutdown, differential oracle, and JupyterHub 5.5 external-proxy flow are implemented and tested. Redis and sidecar store implementations exist behind contracts, but the executable currently starts only with `--storage-backend memory`. The checked-in Dockerfile and Helm chart are still being hardened for release; build from source for the current implementation.

## Why this project?

JupyterHub needs a proxy that can update routes while it is running, preserve route metadata, proxy WebSockets, and choose the most specific path or host route. `pingora-reverse-proxy` provides that control plane in Rust while using Pingora for the public data plane.

Key capabilities:

- CHP 5.3.0-compatible `GET`, `POST`, and `DELETE /api/routes` behavior
- longest-prefix path routing and optional host-based routing
- HTTP and WebSocket proxying without dropping existing connections on route changes
- public, management, and upstream TLS configuration, including client-certificate validation
- TCP and Unix-domain listeners
- optional Prometheus metrics endpoint
- constant-time management-token comparison
- configurable forwarding headers, origin changes, redirect rewriting, and timeouts
- graceful shutdown with management and traffic drain ordering
- differential tests against the pinned CHP oracle
- end-to-end validation with JupyterHub 5.5.0 in external-proxy mode

## Architecture

```text
                                   +----------------------+
JupyterHub / operator ------------>| Axum management API  |--+
                                   | GET/POST/DELETE      |  |
                                   | /api/routes          |  |
                                   +----------------------+  |
                                                              v
Client ---> Pingora public proxy ---> RouteRegistry ---> in-memory store
                  |                       |
                  |                       +---- activity tracking
                  v
           HTTP(S) / WebSocket upstream

Prometheus -----------------------> optional /metrics listener
```

The public proxy, management API, and metrics endpoint use separate listeners. By default, the public listener binds to port `8000`, the API binds to `localhost:8001`, and metrics are disabled.

## Requirements

- Rust 1.85 or newer, as pinned in [`rust-toolchain.toml`](rust-toolchain.toml)
- a C/C++ build toolchain
- CMake and OpenSSL development headers
- Git
- optional: [`just`](https://github.com/casey/just) for repository shortcuts
- optional: Docker plus Compose for differential and JupyterHub end-to-end tests

## Build

```bash
git clone https://github.com/DPS0340/pingora-reverse-proxy.git
cd pingora-reverse-proxy
cargo build --locked --release
```

The executable is written to `target/release/pingora-reverse-proxy`.

## Quick start

The following example runs a local upstream, starts the proxy with an authenticated management API, creates a root route, and sends a request through it.

### 1. Start a local upstream

```bash
mkdir -p /tmp/pingora-demo
printf 'proxied by pingora-reverse-proxy\n' > /tmp/pingora-demo/index.html
python3 -m http.server 9000 --directory /tmp/pingora-demo
```

### 2. Start the proxy in another terminal

```bash
export CONFIGPROXY_AUTH_TOKEN='dev-only-change-me'

cargo run --locked -- \
  --ip 127.0.0.1 \
  --port 8000 \
  --api-ip 127.0.0.1 \
  --api-port 8001 \
  --metrics-ip 127.0.0.1 \
  --metrics-port 8002
```

### 3. Add a route and verify traffic

```bash
export CONFIGPROXY_AUTH_TOKEN='dev-only-change-me'

curl --fail-with-body \
  --request POST \
  --header "Authorization: token ${CONFIGPROXY_AUTH_TOKEN}" \
  --header 'Content-Type: application/json' \
  --data '{"target":"http://127.0.0.1:9000"}' \
  http://127.0.0.1:8001/api/routes

curl --fail-with-body http://127.0.0.1:8000/
curl --fail-with-body http://127.0.0.1:8002/metrics
```

The first request returns `201 Created`; the proxied request prints `proxied by pingora-reverse-proxy`.

## Management API

The API is compatible with CHP's route model. A route contains a required `target`, a generated `last_activity` timestamp, and any additional JSON metadata supplied by the caller.

| Method | Endpoint | Result |
| --- | --- | --- |
| `GET` | `/api/routes` | Return all routes as a JSON object. |
| `GET` | `/api/routes/<routespec>` | Return one route or `404`. |
| `POST` | `/api/routes/<routespec>` | Add or replace a route; returns `201`. |
| `DELETE` | `/api/routes/<routespec>` | Delete a route; returns `204` or `404`. |

The root route uses `/api/routes` or `/api/routes/`. Route paths are URL-decoded and normalized using the same compatibility rules covered by the differential test suite.

Example:

```bash
curl --fail-with-body \
  --request POST \
  --header "Authorization: token ${CONFIGPROXY_AUTH_TOKEN}" \
  --header 'Content-Type: application/json' \
  --data '{
    "target": "http://127.0.0.1:9000",
    "user": "alice",
    "server_name": ""
  }' \
  http://127.0.0.1:8001/api/routes/user/alice
```

List routes that were inactive before an ISO 8601 timestamp:

```bash
curl --fail-with-body \
  --header "Authorization: token ${CONFIGPROXY_AUTH_TOKEN}" \
  'http://127.0.0.1:8001/api/routes?inactiveSince=2026-01-01T00%3A00%3A00Z'
```

### API authentication

Set `CONFIGPROXY_AUTH_TOKEN` before starting the process. Authenticated requests use CHP's header format:

```http
Authorization: token <token>
```

If the variable is missing or empty, the management API does not require authentication. Keep the default loopback API binding and set a strong token in any shared environment.

## JupyterHub external-proxy mode

JupyterHub can use this process as an externally managed proxy. Start `pingora-reverse-proxy` separately, then configure JupyterHub with the same management token:

```python
# jupyterhub_config.py
import os

c.ConfigurableHTTPProxy.should_start = False
c.ConfigurableHTTPProxy.api_url = "http://127.0.0.1:8001"
c.ConfigurableHTTPProxy.auth_token = os.environ["CONFIGPROXY_AUTH_TOKEN"]
```

For host-based routing, start the proxy with `--host-routing` and set JupyterHub's `c.JupyterHub.subdomain_host`. JupyterHub documents the required proxy behavior in [Writing a custom Proxy implementation](https://jupyterhub.readthedocs.io/en/5.5.0/howto/proxy.html).

The repository's Docker-backed test runs JupyterHub 5.5.0 against both memory and Redis lifecycle scenarios:

```bash
just test-jupyterhub
# or
python3 scripts/jupyterhub-e2e.py
```

This is an integration gate, not a lightweight unit test. It builds the test image, launches isolated services, exercises path and host routing, restarts the proxy, and verifies cleanup.

## Configuration

Run the binary with `--help` for the complete, versioned option list:

```bash
cargo run --locked -- --help
```

Common options:

| Option | Purpose | Default |
| --- | --- | --- |
| `--ip`, `--port` | Public HTTP listener | all interfaces, `8000` |
| `--socket` | Public Unix-domain socket; conflicts with `--ip` and `--port` | disabled |
| `--api-ip`, `--api-port` | Management API listener | `localhost`, public port + 1 |
| `--api-socket` | Management API Unix-domain socket | disabled |
| `--metrics-ip`, `--metrics-port` | Prometheus listener | disabled until a port is set |
| `--metrics-socket` | Prometheus Unix-domain socket | disabled |
| `--default-target` | Target used when no dynamic route matches | none |
| `--host-routing` | Add the request host to route selection | off |
| `--change-origin` | Replace the upstream `Host` header with the target origin | off |
| `--custom-header 'Name: value'` | Add an upstream request header; repeatable | none |
| `--timeout` | Upstream connection timeout in milliseconds | Pingora default |
| `--proxy-timeout` | Upstream response timeout in milliseconds | Pingora default |
| `--keep-alive-timeout` | Keep-alive timeout in milliseconds | `5000` |
| `--log-level` | `debug`, `info`, `warn`, or `error` | `info` |
| `--storage-backend` | `memory`, `redis`, or `sidecar` | `memory` |

The parser accepts `redis` and `sidecar` so their contracts can be exercised, but the current executable fails closed at startup for either value. Use `memory` until runtime wiring is released.

### Environment variables

| Variable | Purpose |
| --- | --- |
| `CONFIGPROXY_AUTH_TOKEN` | Management API token. |
| `CONFIGPROXY_SSL_KEY_PASSPHRASE` | Passphrase for the public-listener private key. |
| `CONFIGPROXY_API_SSL_KEY_PASSPHRASE` | Passphrase for the management-listener private key. |

## TLS and Unix sockets

Public TLS requires both `--ssl-key` and `--ssl-cert`. The management listener has an equivalent `--api-ssl-*` option family, and upstream client identity uses `--client-ssl-*`.

```bash
cargo run --locked -- \
  --ip 0.0.0.0 \
  --port 8443 \
  --ssl-key /run/secrets/tls.key \
  --ssl-cert /run/secrets/tls.crt \
  --api-ip 127.0.0.1 \
  --api-port 8001
```

Use `--ssl-ca --ssl-request-cert --ssl-reject-unauthorized` for public-listener client-certificate enforcement. Equivalent `--api-ssl-*` flags protect the management listener. `--insecure` disables upstream certificate verification and should be limited to controlled development environments.

HTTP targets may use `http`, `https`, `http+unix`, or `unix+http`. Unix target URLs require an absolute, UTF-8 socket path encoded in the URL authority.

## Metrics

Set `--metrics-port` or `--metrics-socket` to enable the dedicated Prometheus endpoint at `/metrics`. Metrics include management and proxy request counts, route mutations, WebSocket and HTTP traffic, route lookup latency, and activity-update latency.

Keep metrics on a private listener unless an authenticated monitoring layer protects it.

## Storage status

| Backend | Contract implementation | Executable runtime | Persistence |
| --- | --- | --- | --- |
| Memory | yes | supported | process-local only |
| Redis | yes | not wired yet | backend implementation is tested |
| Sidecar | protocol implementation present | not wired yet | depends on sidecar |

The route registry uses fail-stop semantics when a remote mutation outcome is indeterminate. It does not guess whether a timed-out writer committed a change.

## Testing

Run the local quality gate:

```bash
just verify
```

Equivalent commands:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

Additional compatibility gates:

```bash
just test-differential   # compare behavior with the pinned CHP 5.3.0 oracle
just test-jupyterhub     # JupyterHub 5.5.0 external-proxy end-to-end test
```

Both integration gates require Docker. They create isolated resources and enforce teardown checks.

## Deployment status

A Dockerfile and Helm chart are present in the repository, but they predate the current release-hardening work. For now:

1. prefer `cargo build --locked --release`;
2. run the binary under a process supervisor;
3. bind the management and metrics listeners to private interfaces;
4. set `CONFIGPROXY_AUTH_TOKEN` through a secret manager;
5. terminate or configure TLS explicitly; and
6. persist logs and scrape the dedicated metrics listener.

Do not assume the current chart supplies readiness probes, hardened security contexts, or complete CLI configuration.

## Compatibility boundaries

This project targets observed CHP 5.3.0 behavior rather than every historical Node.js extension point.

- arbitrary CHP storage modules cannot be loaded into the Rust process;
- Redis and sidecar runtime selection is not yet enabled;
- deprecated RC4 enablement is intentionally rejected;
- `--client-ssl-request-cert` and `--client-ssl-reject-unauthorized` are rejected because CHP does not apply them to upstream TLS in the targeted contract;
- deployment assets are not yet the release authority.

See the executable's `--help` output and the test suites for the current contract.

## Contributing

Before opening a pull request:

```bash
just verify
```

Changes to CHP-visible behavior should include a differential fixture or contract test. Changes to JupyterHub integration should also run `just test-jupyterhub`. Keep behavior claims tied to executable tests rather than implementation assumptions.
