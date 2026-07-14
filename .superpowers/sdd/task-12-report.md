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

---

## Review-fix closure (2026-07-14)

This section supersedes the original report wherever the independent review
found that its evidence or runtime claims were too broad.

Status: DONE

Implementation commit:
`fd4d9591c81775620acad943e0169691b852a9ee fix: close Task 12 runtime review findings`

### Root cause and resolution by finding

1. **CRITICAL — the E2E launched a test helper instead of the product binary.**
   `src/main.rs` rejected every non-memory backend, while
   `tests/jupyterhub_e2e.rs` manually assembled a second proxy runtime with a
   `RedisStore`. Compose built and installed that test executable, so the Redis
   result did not establish product-binary support. The normal executable now
   constructs either `MemoryStore` or `RedisStore`; Redis requires an explicit,
   parseable `PINGORA_REDIS_URL`, accepts a non-empty route key and positive
   operation timeout, and redacts the URL from debug/errors. Compose runs
   `cargo build --locked`, installs the resulting
   `pingora-reverse-proxy`, and the Python scenario launches that artifact
   directly for both backends. Sidecar selection remains explicitly
   fail-closed for Task 13 and has a binary-level regression test.

2. **HIGH — the pinned single-user probe was fragile on a cold image.** The
   arbitrary per-call 15-second `subprocess.run` timeout included cold Python
   import and entry-point startup. The probe now owns one 120-second monotonic
   overall deadline, polls process completion, terminates the process group on
   expiry, and still requires successful exit plus exact `5.5.0` stdout. A
   delayed probe succeeds within its deadline and a wrong version fails closed.

3. **HIGH — Redis reconciliation was already satisfied.** The previous Redis
   branch proved that every persisted route existed and then waited for the
   same subset after `POST /proxy`; a no-op passed. The final scenario first
   proves Redis persistence after the product-binary restart as a separate
   assertion, deliberately deletes `/user/river`, proves that exact key is
   absent, invokes JupyterHub's real `POST /proxy`, and requires the selected
   route's complete stable JSON metadata to equal the pre-restart value.
   `last_activity` is deliberately excluded from equality because every new
   route mutation receives a new activity timestamp. Memory separately proves
   its complete route table was lost on product-binary restart before using the
   same absent-route restoration check. Focused tests prove an already-present
   precondition and a reconciliation no-op both fail.

4. **MEDIUM — the Linux ACL compile correction lacked explicit Task 12 scope.**
   The Task 12 controller now carries a local scope addendum, and this tracked
   report is the durable addendum: `src/path_ownership.rs` is in Task 12 scope
   only for the rustix 1.1.2 signature correction that passes `&mut Vec<u8>` to
   `fgetxattr`. It does not alter ACL policy. A Linux-only focused test opens a
   fresh private directory and executes the production descriptor-backed POSIX
   ACL probe; the authoritative JupyterHub image compiles and runs it.

During final verification, host Python 3.10 also exposed that
`python3 -m unittest -v scripts/test_jupyterhub_e2e.py` treats the slash-form
argument as a module name. The `just` and image commands now use the portable
direct form `python3 scripts/test_jupyterhub_e2e.py -v`.

### Preserved RED evidence

These focused tests were added and run before the corresponding production or
harness changes. The recorded failures are actual command results.

```text
$ cargo test --locked --test config_contract redis_runtime_ -- --nocapture
error[E0609]: no field `redis` on type `AppConfig`
  --> tests/config_contract.rs:672
exit 101

$ cargo test --locked --test jupyterhub_e2e \
    canonical_harness_builds_and_launches_the_shipped_binary \
    -- --exact --nocapture
thread 'canonical_harness_builds_and_launches_the_shipped_binary' panicked:
the E2E image must release-build the deployable binary
test result: FAILED. 0 passed; 1 failed
exit 101

$ python3 -m unittest -v scripts/test_jupyterhub_e2e.py
ERROR: four tests could not find `assert_command_version` or
`reconcile_missing_route` in the harness module
Ran 4 tests; FAILED (errors=4)
exit 1
```

The final-gate portability failure was also preserved rather than hidden:

```text
$ python3 -m unittest -v scripts/test_jupyterhub_e2e.py
ModuleNotFoundError: No module named 'scripts.test_jupyterhub_e2e'
Ran 1 test; FAILED (errors=1)
exit 1
```

### Changed files

- `src/config.rs`, `src/main.rs`: validated/redacted Redis runtime selection and
  explicit sidecar fail-closed boundary.
- `src/path_ownership.rs`: Linux compile correction coverage only; the
  production correction itself remains the minimal mutable-buffer change.
- `tests/config_contract.rs`, `tests/jupyterhub_e2e.rs`: Redis configuration,
  product-artifact, and sidecar regressions; removed the manually assembled
  helper.
- `scripts/jupyterhub-e2e.py`, `scripts/test_jupyterhub_e2e.py`: direct product
  launch, bounded version probe, non-vacuous reconciliation, and cleanup tests.
- `compose.test.yml`, `justfile`: locked application build and portable focused
  harness gate.
- `README.md`: only the now-stale Redis/sidecar runtime and storage claims.
- `.superpowers/sdd/task-12-report.md`: this durable handoff.

### Final GREEN evidence

Focused and static gates:

```text
python3 scripts/test_jupyterhub_e2e.py -v                       PASS 5/5, exit 0
cargo test --locked --test config_contract                     PASS 30/30, exit 0
cargo test --locked --test jupyterhub_e2e                      PASS 2/2, exit 0
cargo fmt --all -- --check                                     PASS, exit 0
cargo clippy --locked --all-targets --all-features -- -D warnings PASS, exit 0
cargo check --locked --all-targets --all-features              PASS, exit 0
git diff --check                                                PASS, exit 0
```

The only Clippy/check warning is the pre-existing vendored Pingora OpenSSL
`Asn1StringRef::as_utf8` deprecation; the warnings-denied workspace gate exits
zero.

The final exact canonical command, after the portable test-command correction,
was:

```text
python3 scripts/jupyterhub-e2e.py
```

Result: exit 0. The image used `cargo build --locked`, installed and executed
`/usr/local/bin/pingora-reverse-proxy`, ran the Linux ACL probe 1/1, ran the
Python harness regressions 5/5, and passed `pip check`. Memory passed all 18
required scenarios; Redis passed all 18 required scenarios. The final run was
`jupyterhub-e2e-88759-a7d8d2c9`.

A release-profile image build was attempted first because release was
preferred. On the cold builder it reached the final application compile/link
but exceeded the existing 3600-second supervisor bound (`#14 CANCELED` and
`JupyterHub E2E failed: command exceeded 3600s: docker-compose`). The final
image therefore uses the explicitly permitted debug-profile
`cargo build --locked`. BuildKit registry, Git, and target caches remain
enabled; the final changed layer rebuilt the product in 2.00 seconds and
completed its test/pip work in 39.6 seconds before export.

The correctly provisioned aggregate used a strict unique
`chp-diff-task12-*` label, Node 20.20.2, configurable-http-proxy 5.3.0, a
Docker-assigned Redis port, and `PROPTEST_CASES=256`:

```text
PROPTEST_CASES=256 TEST_REDIS_URL=<dynamic> \
  cargo test --locked --all-targets --all-features
```

Result: exit 0, 394/394 primary tests:

```text
library 67, binary 0, API 34, config 30, differential 35,
differential lifecycle 9, JupyterHub 2, proxy 74, route 11,
store 106, TLS/Unix 22, WebSocket 4
```

The authoritative Linux image additionally passed the cfg-gated POSIX ACL
probe 1/1.

### Cleanup proof

- Success: after final canonical run `jupyterhub-e2e-88759-a7d8d2c9`, exact
  label/tag scans found no owned container, network, volume, or image; scan exit
  0.
- Failure: with `STORE_BACKEND=redis` and
  `JUPYTERHUB_E2E_INJECT_FAILURE=after-redis-ready`, the canonical supervisor
  failed at the injected point with exit 1, stopped/removed its unique Redis
  container and network, and removed its volume/image. Exact follow-up scans
  for `jupyterhub-e2e-43795-a41baebc` passed with exit 0.
- Aggregate: the successful provisioned run stopped/removed its unique Redis
  container and network and removed the oracle image. Two earlier diagnostic
  invocations interrupted by output capture or rejected by the strict label
  validator left uniquely identifiable resources; those exact projects were
  removed and follow-up container/network/volume/image scans passed before the
  successful aggregate was run.
- BuildKit cache mounts are intentionally reusable build caches, not per-run
  runtime resources, and remain subject to normal builder garbage collection.

### Final self-review

- **Independent findings:** all four are addressed above with direct focused
  tests and canonical runtime evidence.
- **Correctness:** the shipped executable owns both memory and Redis selection;
  Redis startup failure is bounded and redacted; sidecar cannot be silently
  selected; restart persistence and reconciliation are distinct assertions.
- **Test strength:** the product artifact path is statically guarded, both
  reconciliation vacuity modes fail focused tests, wrong versions fail closed,
  and real JupyterHub/Redis behavior is exercised in containers.
- **Security/isolation:** Redis credentials never appear in Debug or typed
  configuration errors; tokens are generated per run; ports, Redis keys,
  project names, images, process groups, and cleanup scans are run-owned.
- **Scope:** no Task 13 image/Helm/CI work, dependency changes, branch rewrite,
  amendment, rebase, push, or unrelated README rewrite was performed. The
  English README commit remains in history.

Concerns: none. The vendored deprecation warning and retained BuildKit caches
are established non-blocking conditions, and the debug-profile image is the
documented bounded-build choice after the measured release attempt exceeded the
one-hour limit.
