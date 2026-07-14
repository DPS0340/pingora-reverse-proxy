# Task 12 Report: Real JupyterHub 5.5.0 External-Proxy E2E

Date: 2026-07-14

Status: DONE

Baseline: `60291c9 fix: label every CHP oracle launch`

## Delivered scope

- Added a real JupyterHub 5.5.0 external-proxy test target and supervisor.
- Exercised memory and Redis route stores with 18 exact, non-vacuous scenarios
  per backend: pinned runtime, external-proxy configuration, proxy CRUD, Hub
  root/API behavior, four user-route encodings, authenticated login, a rendered
  single-user page, kernel WebSocket request/reply/stream/idle flow, Hub restart,
  proxy restart/persistence, reconciliation, user deletion, host routing, and
  run-owned cleanup.
- Added the `test-jupyterhub` recipe and a pinned Compose image with exact Python
  package versions and image digests.
- Kept `cargo test --locked` and copied the exact Cargo-reported test executable
  from the cached target directory to `/usr/local/bin/jupyterhub-e2e-test`, so
  the runtime image executes the artifact after the BuildKit cache mount is gone.
- Added locked BuildKit caches for Cargo registry, Git, and target directories.
  The cold build is explicitly bounded at 3600 seconds.
- Preserved the Linux compile compatibility correction required by rustix
  `fgetxattr`: it now receives `&mut Vec<u8>`.

## RED evidence

The preserved canonical history established these real REDs before this
continuation:

1. `python3 scripts/jupyterhub-e2e.py` failed because the Linux image lacked
   `cmake`.
2. After adding `cmake`, Linux compilation failed because rustix
   `fgetxattr` required `&mut Vec<u8>` rather than an owned vector.
3. After that type correction, the cold Docker `cargo test --locked --test
   jupyterhub_e2e --no-run` step exceeded `BUILD_TIMEOUT=1800` repeatedly. The
   build was cancelled before its layer committed. The unchanged command was
   not repeated because it could not preserve progress.

This continuation captured the following additional regression REDs:

- `python3 scripts/jupyterhub-e2e.py --help` exited 1 because the top-level
  `BaseException` handler swallowed argparse's successful `SystemExit(0)`.
- The first cached-build run committed the cold Rust layer but memory returned
  status 101 because the login check searched rendered HTML for a raw Unicode
  username. JupyterHub had authenticated the correct user.
- The next run reached `has@` and got a Jupyter Server 404 because the harness
  forced `%40` into a server base path that JupyterHub registered as `has@`.
- The next run passed all 17 path-routing scenarios, then waited for the wrong
  host key (`river.localhost`); JupyterHub 5.5.0 had registered the actual
  `river.hub.localhost/user/river/` route derived from `subdomain_host`.
- A later run reached the real kernel channels endpoint but the harness's
  3-second WebSocket handshake/receive timeout expired under load inside a
  20-second overall flow budget.
- A raw `cargo test --locked --all-targets --all-features` aggregate correctly
  failed 11 differential tests because `CHP_ORACLE_IMAGE` was not provisioned.
  The provisioned aggregate was then run with the existing strict `chp-diff-*`
  ownership label, pinned oracle, and dynamic Redis.

Each runtime correction was limited to the observed harness mismatch: session
identity is checked via `/hub/api/user`, browser-facing paths let `requests`
encode only characters that require encoding, host expectations use the route
JupyterHub emits, and WebSocket idle polling/handshake honor the existing
20-second semantic deadline.

## GREEN evidence

### Compose and cheap checks

Local Compose support is `/opt/homebrew/bin/docker-compose` 2.33.0; the Docker
Compose plugin command is absent. These checks passed:

```text
python3 -c "import ast, pathlib; ast.parse(pathlib.Path('scripts/jupyterhub-e2e.py').read_text())"
python3 scripts/jupyterhub-e2e.py --help
/opt/homebrew/bin/docker-compose -f compose.test.yml config --quiet
/opt/homebrew/bin/docker-compose -f compose.test.yml config --format json | <extract inline Dockerfile> | docker build --check -f - .
```

Docker's BuildKit check reported `Check complete, no warnings found`.

### Canonical JupyterHub gate

The required exact command completed successfully:

```text
python3 scripts/jupyterhub-e2e.py
```

- Cold Rust compile layer: 1706.3 seconds, below the 3600-second bound, then
  successfully committed/exported.
- Cached Rust rebuilds after harness-only changes: 3.83, 2.33, 3.73, and 2.84
  seconds, proving target-cache reuse.
- Memory: 2/2 Rust tests, 18/18 required scenarios, 168.52 seconds.
- Redis: 2/2 Rust tests, 18/18 required scenarios, 193.61 seconds.
- The supervisor removed the unique image, Redis container, and Compose network.

### Required local gates

```text
cargo fmt --all -- --check                                      PASS
cargo test --locked --test jupyterhub_e2e                       PASS 2/2
cargo clippy --locked --all-targets --all-features -- -D warnings PASS
cargo check --locked --all-targets --all-features               PASS
git diff --check                                                 PASS
```

The relevant provisioned aggregate used `PROPTEST_CASES=256`, the pinned Node
20.20.2 / configurable-http-proxy 5.3.0 oracle image, a strict unique oracle
ownership label, and Docker-assigned Redis port:

```text
PROPTEST_CASES=256 TEST_REDIS_URL=<dynamic> \
  cargo test --locked --all-targets --all-features
```

Aggregate result: 391/391 primary tests.

```text
library 67, binary 0, API 34, config 27, differential 35,
differential lifecycle 9, JupyterHub 2, proxy 74, route 11,
store 106, TLS/Unix 22, WebSocket 4
```

## Cleanup and limitations

- Final process scan found no Cargo, rustc, Docker build, BuildKit, JupyterHub,
  single-user, CHP oracle, or helper child owned by these runs.
- Final Docker scans found no owned JupyterHub/differential containers,
  networks, volumes, unique images, or temporary JupyterHub state directories.
- The named BuildKit registry/Git/target caches intentionally remain available
  for bounded rebuild reuse and are subject to normal Docker builder GC; they
  are not runtime resources owned by an individual test run.
- `just` is not installed locally, so its literal `python3
  scripts/jupyterhub-e2e.py` recipe command was executed directly.
- The only emitted warning is the established vendored Pingora OpenSSL
  `Asn1StringRef::as_utf8` deprecation. It is outside Task 12 scope and does not
  fail the warnings-denied workspace Clippy gate.

## Self-review

- Correctness: both backends prove the exact scenario set, and the copied test
  binary is the one Cargo reports for `jupyterhub_e2e`.
- Test strength: identity, route, runtime version, server version, WebSocket
  reply/output/idle state, restart persistence, reconciliation, and cleanup are
  asserted directly; no scenario is recorded before its behavior succeeds.
- Isolation: all ports are dynamically reserved, projects/images/keys are
  unique, and process groups, temporary directories, containers, networks, and
  images are owned and cleaned by the initiating run.
- Scope: no dependency/image pin, lock enforcement, production behavior, Task
  13 artifact, push, or deployment was changed.

Verdict: APPROVE.
