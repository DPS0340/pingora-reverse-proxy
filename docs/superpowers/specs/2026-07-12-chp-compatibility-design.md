# Pingora Configurable HTTP Proxy Compatibility Design

**Status:** Approved design
**Date:** 2026-07-12
**Repository:** `DPS0340/pingora-reverse-proxy`

## 1. Objective

Rebuild this repository as a production-quality, Pingora-based replacement for JupyterHub's `configurable-http-proxy` (CHP).

The compatibility baseline is:

- `configurable-http-proxy` **5.3.0** for observable CLI, REST API, routing, proxy, WebSocket, TLS, Unix-socket, error, health, and metrics behavior.
- JupyterHub **5.5.0**, the current stable tag at design time, running with `ConfigurableHTTPProxy.should_start = False` as the end-to-end consumer.
- Pingora **0.8.1**, the current stable release at design time.

Success means JupyterHub can use this binary as an external proxy without application changes, and the project's differential compatibility suite finds no unexplained observable differences from CHP 5.3.0 over the supported surface.

This is a clean TDD-based redesign. Existing WIP internals do not constrain the new implementation.

## 2. Explicit Assumptions and Decisions

1. A single Rust binary and a single Cargo package remain the deployment unit.
2. Internal modules have narrow interfaces; `main.rs` only loads configuration and assembles services.
3. In-memory routing is the default, matching CHP's default operational model.
4. Redis is an optional first-class persistent store.
5. External stores use a documented sidecar protocol. Arbitrary Node.js modules are not loaded into the Rust process.
6. `--storage-backend` retains the meaning of choosing a storage implementation, with `memory`, `redis`, and `sidecar` backends.
7. Linux containers are the primary reproducible build and verification environment.
8. macOS remains a secondary CI target after the host SDK/toolchain is healthy. The current host fails native dependency builds because `xcrun` cannot resolve the macOS SDK and `cmake`/`make` discovery fails.
9. Existing response-body rewriting and forced `Accept-Encoding: identity` are removed because they are not CHP behavior.
10. Redis or sidecar network access never occurs on the proxy request hot path.

## 3. Compatibility Contract

### 3.1 Required compatibility

The implementation must reproduce CHP 5.3.0's externally observable behavior for:

- Public listener and API listener over TCP and Unix sockets.
- Public/API TLS and API mutual TLS.
- Upstream HTTP, HTTPS, Unix-socket HTTP, client certificates, CA trust, and verification controls.
- CHP CLI option names, aliases, defaults, environment variables, validation, and startup failure behavior.
- `Authorization: token <value>` management API authentication.
- `GET`, `POST`, and `DELETE /api/routes[/<route>]`.
- `inactiveSince` and legacy `inactive_since` filtering.
- Path routing and host routing.
- Longest path-segment prefix matching.
- `includePrefix`, `prependPath`, target pathname, raw query, and percent-encoding behavior.
- HTTP streaming, keep-alive, and hop-by-hop header handling.
- WebSocket upgrade and bidirectional streaming.
- Route `last_activity` semantics.
- Default, file-based, and custom-target 404/503 error responses.
- `/_chp_healthz` and Prometheus metrics behavior.
- Graceful shutdown and restart-safe store loading.

### 3.2 Storage extension compatibility

CHP loads arbitrary Node.js storage classes with `require()`. A Rust process cannot safely provide binary compatibility with that mechanism. This project provides semantic extension compatibility instead:

- `memory`: built-in default store.
- `redis`: built-in persistent store.
- `sidecar`: an external store implementing the versioned sidecar protocol.
- A stable Rust `Store` trait for in-tree implementations.

The CLI compatibility document must classify every CHP 5.3.0 public option as:

- **identical**: same observable behavior;
- **semantic equivalent**: same purpose with a documented Rust-native mechanism; or
- **intentional difference**: unsupported only with a concrete technical rationale and a startup error rather than silent acceptance.

No feature may be marked compatible without a contract test.

## 4. Architecture

### 4.1 Modules

```text
src/
  lib.rs              Public library surface used by tests and the binary
  main.rs             Configuration loading and service assembly only
  config.rs           CHP-compatible CLI/env parsing and validation
  route.rs            Route model, normalization, and matching
  route_table.rs      Atomic read-optimized route snapshot
  api.rs              Authenticated management REST API
  proxy.rs            Pingora HTTP/WebSocket data plane
  upstream.rs         Target URL parsing and peer construction
  activity.rs         Successful-activity tracking and persistence batching
  errors.rs           CHP-compatible 404/503/custom error handling
  metrics.rs          Health and Prometheus metrics
  shutdown.rs         Coordinated startup and graceful shutdown
  store/
    mod.rs             Async Store trait and shared contracts
    memory.rs          Default in-process store
    redis.rs           Optional Redis persistence
    sidecar.rs         Versioned external store adapter

tests/
  contract/            CHP-derived behavioral contracts
  integration/         Real public/API/upstream service tests
  differential/        CHP 5.3.0 oracle comparisons
  e2e/                 JupyterHub 5.5.0 external-proxy tests
  support/             Test servers, certificates, process harnesses

docs/
  compatibility.md     Flag and behavior compatibility matrix
  sidecar-store.md     Sidecar protocol and failure semantics
  operations.md        Deployment, health, metrics, and troubleshooting
```

Modules may be split further when a file loses a single clear purpose. The package must not be converted to a multi-crate workspace without a new design decision.

### 4.2 State model

`RouteRecord` contains the normalized route key, target, arbitrary user/JupyterHub metadata, and `last_activity`. Route metadata must round-trip without dropping unknown fields.

The data plane reads an immutable, atomically swappable route snapshot. Management mutations execute in this order:

1. Parse and validate the request.
2. Persist the mutation through the selected store.
3. Build or update the in-memory route index.
4. Atomically publish the new snapshot.
5. Return the CHP-compatible response.

If persistence fails, the published route table does not change. Startup loads and validates the complete store snapshot before listeners become ready.

Activity updates are coalesced and persisted asynchronously. The in-memory observable timestamp advances only for CHP-qualified activity. Persistence failures are logged and measured without failing an already proxied request.

### 4.3 Route matching

Route keys are normalized like CHP:

- ensure a leading slash;
- remove one trailing slash except for `/`;
- decode route API path components according to CHP behavior;
- match complete slash-delimited segments, not arbitrary string prefixes;
- choose the most specific matching route;
- support host routing by prepending the parsed host to the match path.

A simple, obviously correct reference matcher remains in test code and is independent of the production index.

## 5. Request and Data Flow

### 5.1 Management API

1. Match `/api/routes` and optional route suffix.
2. If an auth token is configured, validate `Authorization: token <token>` using constant-time comparison.
3. Reject unsupported methods with 405 and unknown paths with 404.
4. Parse JSON without panicking; invalid JSON returns CHP-compatible 400 text.
5. Require a string `target` for POST and validate supported target schemes.
6. Preserve all additional route fields, including JupyterHub's `jupyterhub` and route metadata.
7. Return the same status, content type, empty body, or JSON shape as CHP.

GET-all supports `inactiveSince` and `inactive_since`. Invalid timestamps return 400. GET-one returns 404 for a missing route. DELETE returns 204 when a route existed and 404 otherwise.

### 5.2 Proxy data plane

1. Handle `/_chp_healthz` before route lookup.
2. Resolve the route from decoded pathname and optional Host routing base.
3. Return 404 through the configured error mechanism if no route matches.
4. Build the upstream URI from target pathname, `includePrefix`, `prependPath`, request path, and raw query using CHP semantics.
5. Preserve request and response streaming. Do not buffer or rewrite arbitrary bodies.
6. Forward end-to-end headers, construct proxy headers according to CHP/http-proxy behavior, and remove connection-specific hop-by-hop headers.
7. Use pooled upstream connections when safe.
8. Map connection, reachability, reset, and timeout failures to CHP-compatible 503 behavior.
9. Apply redirect rewriting only when the corresponding CHP option enables it.

WebSocket upgrades use the same resolver and target construction, then stream frames bidirectionally until either side closes.

### 5.3 Activity semantics

Update `last_activity` when:

- an HTTP response finishes with status below 300; or
- real request/response stream data passes, including WebSocket data.

Do not update it merely because a request started. In particular, route misses, API-auth failures, unavailable upstreams, and redirects do not make a route active.

## 6. Errors, Security, and Reliability

- No route: 404.
- Upstream unavailable or timed out: 503.
- Internal decode/dispatch failure: 500.
- WebSocket failures return an empty HTTP error handshake.
- With `--error-target`, request `<error-target>/<status>?url=<escaped-original-url>`, retain the original status, and forward only CHP-approved content headers.
- If the custom target fails, use `<error-path>/<status>.html`, then `error.html`, then the standard reason phrase.
- Never log auth tokens, TLS private-key contents, or complete secret-bearing URLs.
- Bind API listener conservatively by default and document exposure risks.
- Validate target scheme, authority, port, TLS settings, and Unix socket path before publishing a route.
- Reject malformed values with deterministic errors; never panic on client input.
- Apply explicit request-body, header, and timeout limits while preserving CHP-compatible accepted inputs.
- Support coordinated graceful shutdown so active HTTP/WebSocket connections receive a bounded drain period.
- Docker runs non-root with a read-only root filesystem where practical.
- Helm manifests include readiness/liveness probes, security context, resources, and secret references rather than inline tokens.

## 7. Sidecar Store Protocol

The sidecar backend is a versioned HTTP or gRPC contract selected through configuration. The first implementation must define:

- protocol version negotiation;
- load-all, get, add/replace, update-activity, and delete operations;
- route record JSON/protobuf schema with unknown metadata preservation;
- idempotency and retry rules;
- deadlines and bounded retries;
- authentication and TLS options;
- startup behavior when the sidecar is unavailable;
- mutation atomicity and consistency guarantees;
- health and metrics.

The default protocol choice will be made in the implementation plan after a source-backed spike compares HTTP/JSON and gRPC operational complexity. Whichever protocol is selected must have a conformance server used in CI.

## 8. Technology Stack

- Rust stable, with MSRV declared after dependency resolution.
- Pingora 0.8.1.
- Tokio on Pingora's supported runtime path.
- Axum or a smaller compatible HTTP layer for the management API only if it can share lifecycle and listener requirements cleanly; otherwise a Pingora HTTP service.
- Serde for route and configuration data.
- `proptest` for property-based tests.
- Redis async client for optional persistence.
- Prometheus-compatible metrics.
- Pinned CHP 5.3.0 and JupyterHub 5.5.0 containers/environments for compatibility tests.

Dependencies must use one coherent Pingora release line. The current 0.3/0.4 mix is forbidden.

## 9. Commands

The implementation plan may refine exact helper names, but the repository must expose these stable developer entry points:

```bash
# Format
cargo fmt --all -- --check

# Static analysis
cargo clippy --all-targets --all-features -- -D warnings

# Unit and property tests
cargo test --all-targets --all-features

# Reproducible complete verification
just verify

# CHP differential suite
just test-differential

# JupyterHub external-proxy end-to-end suite
just test-jupyterhub

# Container and chart verification
just test-container
just test-helm

# Development
cargo run -- --help
```

If `just` is not adopted, identically named scripts under `scripts/` must provide the same stable entry points. CI invokes these repository-owned commands rather than duplicating logic in workflow YAML.

## 10. Code Style

- Rustfmt is authoritative.
- Clippy warnings are denied in CI.
- Library code returns typed errors; `unwrap`, `expect`, and indexing that can panic on runtime input are forbidden.
- Test-only `unwrap` is acceptable when failure itself is the assertion context.
- Public and cross-module types have concise rustdoc documenting invariants.
- Functions should perform one transformation or side effect.
- Compatibility quirks include a source link and a focused regression test.

Example style:

```rust
/// Normalize a CHP route key while preserving the root route.
pub fn normalize_route_key(input: &str) -> Result<RouteKey, RouteKeyError> {
    let prefixed = if input.starts_with('/') {
        input.to_owned()
    } else {
        format!("/{input}")
    };

    let normalized = if prefixed.len() > 1 {
        prefixed.trim_end_matches('/').to_owned()
    } else {
        prefixed
    };

    RouteKey::try_from(normalized)
}
```

## 11. Test Strategy

### 11.1 TDD discipline

Every behavior change follows RED → GREEN → REFACTOR:

1. Add one focused failing test.
2. Run it and record that it fails for the expected missing behavior.
3. Add the minimum implementation.
4. Run the focused test and relevant suite to GREEN.
5. Refactor only while all tests remain green.

Production code without a test observed failing first is not accepted.

### 11.2 Property-based testing

Use `proptest` with persisted regression cases for:

1. Route normalization idempotence.
2. Production route matching equivalence to the independent reference model over random Unicode and percent-encoded segment sets.
3. Random add/update/delete operation sequences preserving equivalence among API output, store contents, and route index.
4. Request URI construction over the Cartesian product of `includePrefix`, `prependPath`, target path, request path, query, and encoding forms.
5. Invalid UTF-8 boundaries, malformed percent escapes, malformed URLs, headers, JSON, and timestamps producing deterministic non-panicking results.
6. Redis/sidecar serialization round trips preserving arbitrary metadata.
7. Concurrent snapshots never exposing a partially applied mutation.

Property test case counts and seed controls are explicit in CI. Any minimized production failure becomes a named regression test when it represents a distinct compatibility rule.

### 11.3 CHP differential testing

Run pinned CHP 5.3.0 and this binary against the same upstream fixtures. Compare:

- API status, selected headers, body, and resulting route table;
- route matching and upstream-observed method, URI, headers, and body;
- HTTP response status, selected headers, and body;
- WebSocket handshake, messages, close behavior, and failure status;
- TLS/mTLS success and failure cases;
- TCP and Unix socket behavior;
- custom error requests and fallback responses;
- activity timestamp eligibility;
- health and metrics semantics.

Time-dependent values are normalized only where the contract permits them. Every ignored header or value requires a documented reason.

### 11.4 JupyterHub end-to-end testing

Run JupyterHub 5.5.0 with an external proxy configuration and verify:

- Hub root route creation and discovery.
- User route creation for ASCII, Unicode, spaces, `@`, and escaped route specs.
- Add/get/delete and route restoration after proxy restart.
- Host/subdomain routing.
- Login and single-user server HTTP flow.
- A real notebook/kernel WebSocket connection.
- Hub restart while existing proxy routes remain usable.
- Proxy restart followed by JupyterHub route reconciliation.

### 11.5 Fault and operational tests

- Redis and sidecar unavailable at startup.
- Persistence failure during mutation.
- Activity persistence interruption and recovery.
- Upstream timeout/reset/refusal.
- Graceful shutdown with active HTTP and WebSocket traffic.
- Corrupt persisted records.
- API request concurrency.
- Docker image startup as non-root.
- Helm template/schema and smoke tests.

## 12. Verification Gates

A release candidate is complete only when all of these pass from a clean checkout:

1. Formatting and `clippy -D warnings`.
2. Unit and property tests.
3. Integration tests for all listeners, stores, TLS modes, errors, and metrics.
4. CHP 5.3.0 differential suite with zero unexplained differences.
5. JupyterHub 5.5.0 external-proxy E2E suite.
6. Redis and sidecar restart/fault tests.
7. Docker image build and runtime smoke test.
8. Helm render, lint, and cluster smoke test where CI supports it.
9. Dependency license, vulnerability, and supply-chain audit.
10. Documentation examples executed as tests.

Performance is measured against CHP 5.3.0 under representative HTTP and WebSocket workloads. Performance results do not waive correctness gates.

## 13. Boundaries

### Always

- Use TDD for production behavior.
- Preserve unknown route metadata.
- Verify behavior against CHP 5.3.0 source/tests and JupyterHub 5.5.0.
- Keep framework decisions tied to official Pingora documentation.
- Run focused tests after each slice and full gates before completion claims.
- Use typed errors and redact secrets.

### Ask first

- Change the CHP or JupyterHub compatibility baseline.
- Introduce a multi-crate workspace.
- Change the sidecar protocol after it is published.
- Add an intentional compatibility difference.
- Change persistence consistency semantics.
- Push a release tag or publish an image/chart.

### Never

- Silently accept an unsupported CHP flag.
- Claim compatibility without a contract test.
- Perform store network I/O in the routing hot path.
- Rewrite arbitrary response bodies.
- Commit credentials, tokens, private keys, or generated local certificates.
- Remove or weaken a failing compatibility test to make CI green.
- Treat child-agent summaries as verification evidence without fresh commands.

## 14. Success Criteria

1. JupyterHub 5.5.0 runs in external-proxy mode against the binary without JupyterHub code changes.
2. All JupyterHub routes, including Unicode and host-routed cases, round-trip correctly.
3. HTTP and WebSocket notebook traffic works through Pingora.
4. CHP 5.3.0 differential tests report zero unexplained differences on the declared surface.
5. Property suites complete with no failures and persist reproducible regressions.
6. Memory, Redis, and sidecar stores pass the same conformance suite.
7. Redis/sidecar failures never expose partially applied route state.
8. The proxy hot path performs no storage network calls.
9. No client-controlled input can trigger a panic in tests or fuzz/property runs.
10. Docker and Helm artifacts pass security and smoke-test gates.
11. README and compatibility documentation state exactly what is identical, semantically equivalent, and intentionally different.
12. A clean Linux environment can run the complete verification through one repository-owned command.

## 15. Authoritative Sources

- CHP 5.3.0 repository and tests: https://github.com/jupyterhub/configurable-http-proxy/tree/5.3.0
- CHP REST API description: https://github.com/jupyterhub/configurable-http-proxy/blob/5.3.0/doc/rest-api.yml
- JupyterHub external proxy guidance: https://jupyterhub.readthedocs.io/en/5.5.0/howto/separate-proxy.html
- JupyterHub proxy implementation: https://github.com/jupyterhub/jupyterhub/blob/5.5.0/jupyterhub/proxy.py
- Pingora 0.8.1 release: https://github.com/cloudflare/pingora/releases/tag/0.8.1
- Pingora user guide: https://github.com/cloudflare/pingora/tree/0.8.1/docs/user_guide
- Pingora `ProxyHttp` API: https://docs.rs/pingora-proxy/0.8.1/pingora_proxy/trait.ProxyHttp.html

## 16. Open Questions for the Implementation Plan

There are no unresolved product-scope questions. The implementation plan must resolve these engineering choices through short, source-backed spikes before their dependent slices:

1. HTTP/JSON versus gRPC for the sidecar store protocol.
2. Pingora-native management API service versus Axum lifecycle integration.
3. The exact immutable route index representation after correctness and benchmark comparison.
4. Activity update batching interval and durability trade-off, constrained by CHP-visible semantics.
