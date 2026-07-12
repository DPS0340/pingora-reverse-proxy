# CHP-Compatible Pingora Reverse Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:superpowers-subagent-driven-development (recommended) or superpowers:superpowers-executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a Pingora 0.8.1 reverse proxy that JupyterHub 5.5.0 can use as an external drop-in replacement for configurable-http-proxy 5.3.0, with property-based and differential compatibility verification.

**Architecture:** Keep one Cargo package and one binary, but move behavior into focused library modules. Management mutations persist through an async store and then atomically publish an immutable route snapshot; public proxy requests resolve entirely from that snapshot. A pinned CHP process acts as the differential oracle, while a real JupyterHub process provides the final consumer-level gate.

**Tech Stack:** Rust stable; Pingora 0.8.1 with `proxy` and `openssl`; Tokio; Axum 0.8 for management routing; Clap 4; Serde/serde_json; chrono; arc-swap; async-trait; Redis async client; reqwest/hyper for the HTTP/JSON sidecar adapter; Prometheus; proptest; testcontainers or Docker Compose; pinned CHP 5.3.0 and JupyterHub 5.5.0.

## Global Constraints

- Compatibility oracle is configurable-http-proxy 5.3.0 at commit `5651b9d7449aea6c6a390ecd81a9955146a2b05f`.
- Consumer E2E baseline is JupyterHub 5.5.0 at commit `97b3154610726b5b7d8768f1e89a4d910e002854`.
- Network framework baseline is Pingora 0.8.1 at commit `719ef6cd54e40b530127751bab6c1afc5ae815a8`.
- Keep a single Cargo package and a single deployable binary.
- Memory is the default store; Redis and HTTP/JSON sidecar stores are first-class alternatives.
- The sidecar protocol is HTTP/JSON v1. This avoids protobuf code generation, preserves arbitrary route metadata naturally, and reuses the management HTTP stack. The protocol remains versioned so a gRPC transport can be added later without changing store semantics.
- Never perform store network I/O in `ProxyHttp` request routing callbacks.
- Never rewrite arbitrary response bodies or force `Accept-Encoding: identity`.
- Preserve unknown route metadata with `#[serde(flatten)]`.
- Production behavior follows RED → GREEN → REFACTOR. Retain focused RED/GREEN evidence in `docs/test-evidence.md`.
- No unsupported CHP flag may be silently accepted.
- Runtime input paths must not use `unwrap`, `expect`, or panic-prone indexing.
- Linux container verification is authoritative until the local macOS SDK issue is repaired.

## File Map

```text
Cargo.toml                         Unified dependencies/features and binary metadata
Cargo.lock                         Locked dependency graph
rust-toolchain.toml                Reproducible stable toolchain profile
justfile                           Stable developer and CI commands
src/lib.rs                         Library exports
src/main.rs                        CLI parse, assembly, and Pingora server startup
src/config.rs                      CHP-compatible CLI/env model and validation
src/route.rs                       RouteKey, RouteData, target parsing, normalization
src/route_table.rs                 Immutable matcher and atomic snapshot publication
src/store/mod.rs                   Store trait, StoreError, backend construction
src/store/memory.rs                In-memory conformance implementation
src/store/redis.rs                 Redis persistence
src/store/sidecar.rs               HTTP/JSON v1 sidecar client
src/api.rs                         Authenticated route management handlers
src/api_server.rs                  TCP/UDS/TLS listener lifecycle for Axum router
src/proxy.rs                       Pingora ProxyHttp implementation and activity hooks
src/upstream.rs                    TCP/TLS/UDS peer construction and URI/header policy
src/errors.rs                      404/500/503 and custom/file fallback behavior
src/activity.rs                    Coalesced last_activity persistence
src/metrics.rs                     CHP metric names and registry
src/shutdown.rs                    PID file, signals, and drain coordination
tests/route_properties.rs          Route/index proptest suites
tests/store_contract.rs            Backend-neutral store contract/state machine
tests/api_contract.rs              CHP API behavior
tests/proxy_contract.rs             HTTP routing/header/error behavior
tests/websocket_contract.rs        WebSocket behavior
tests/tls_unix_contract.rs         TLS, mTLS, and Unix-socket behavior
tests/differential.rs              CHP-versus-Rust oracle scenarios
tests/jupyterhub_e2e.rs            JupyterHub external-proxy scenarios
tests/support/mod.rs               Process, port, upstream, cert, and normalization helpers
tests/support/sidecar.rs           Sidecar conformance fixture
fixtures/errors/{404,503,error}.html Error fallback fixtures
scripts/chp-oracle.mjs             Start pinned CHP test oracle
scripts/jupyterhub-e2e.py          Start/configure real JupyterHub
scripts/verify.sh                  One-command clean Linux gate
compose.test.yml                   CHP, Redis, sidecar, JupyterHub test dependencies
Dockerfile                         Multi-stage non-root image
helm-chart/                        Secure chart and probes
.github/workflows/ci.yml           Verification matrix
docs/compatibility.md              Every CHP flag/behavior classification
docs/sidecar-store.md              HTTP/JSON v1 protocol
docs/operations.md                 Deployment/health/metrics/runbook
docs/test-evidence.md              RED/GREEN and final gate evidence
```

---

### Task 1: Reproducible Package Skeleton and Verification Entry Points

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `rust-toolchain.toml`
- Create: `justfile`
- Create: `src/lib.rs`

**Interfaces:**
- Produces: crate modules `config`, `route`, `route_table`, `store`, `api`, `api_server`, `proxy`, `upstream`, `errors`, `activity`, `metrics`, and `shutdown`.
- Produces: stable commands `just fmt`, `just lint`, `just test`, and `just verify`.

- [ ] **Step 1: Add the structural RED test**

Create `src/lib.rs` with module declarations only. The first compile must fail because the declared module files do not exist:

```rust
pub mod activity;
pub mod api;
pub mod api_server;
pub mod config;
pub mod errors;
pub mod metrics;
pub mod proxy;
pub mod route;
pub mod route_table;
pub mod shutdown;
pub mod store;
pub mod upstream;
```

- [ ] **Step 2: Run the structural RED command**

Run: `cargo check --lib`

Expected: exit 101 with `file not found for module` for the first undeclared file. Record command, exit code, and first missing-module line in `docs/test-evidence.md` when Task 2 creates the first behavioral module.

- [ ] **Step 3: Replace the dependency graph and add empty compile-safe module files**

Use one Pingora release line and explicit features:

```toml
[package]
name = "pingora-reverse-proxy"
version = "0.2.0"
edition = "2021"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/DPS0340/pingora-reverse-proxy"

[dependencies]
arc-swap = "1.7"
async-trait = "0.1"
axum = "0.8"
bytes = "1"
chrono = { version = "0.4", features = ["serde"] }
clap = { version = "4.5", features = ["derive", "env"] }
constant_time_eq = "0.4"
http = "1"
pingora = { version = "0.8.1", features = ["proxy", "openssl"] }
prometheus = "0.14"
redis = { version = "0.32", features = ["aio", "tokio-comp"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["macros", "net", "rt-multi-thread", "signal", "sync", "time"] }
tower = { version = "0.5", features = ["util"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
url = { version = "2", features = ["serde"] }

[dev-dependencies]
proptest = "1.7"
serial_test = "3"
tempfile = "3"
tokio-tungstenite = "0.27"
wiremock = "0.6"
```

Each not-yet-implemented module contains only a module-level comment, so `cargo check --lib` becomes green without introducing behavior.

- [ ] **Step 4: Add stable command definitions**

Create `justfile`:

```make
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets --all-features

verify: fmt lint test
```

Create `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
profile = "minimal"
components = ["clippy", "rustfmt"]
```

- [ ] **Step 5: Verify GREEN**

Run: `cargo check --lib && just fmt`

Expected: both commands exit 0.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock rust-toolchain.toml justfile src/lib.rs src/activity.rs src/api.rs src/api_server.rs src/config.rs src/errors.rs src/metrics.rs src/proxy.rs src/route.rs src/route_table.rs src/shutdown.rs src/store src/upstream.rs
git commit -m "build: establish Pingora 0.8.1 project skeleton"
```

### Task 2: Route Model, Normalization, and Property-Based Matcher

**Files:**
- Modify: `src/route.rs`
- Modify: `src/route_table.rs`
- Create: `tests/route_properties.rs`
- Create: `docs/test-evidence.md`

**Interfaces:**
- Produces: `RouteKey::parse(&str) -> Result<RouteKey, RouteError>`.
- Produces: `RouteData { target: Url, last_activity: DateTime<Utc>, extra: Map<String, Value> }`.
- Produces: `RouteSnapshot::from_routes(BTreeMap<RouteKey, RouteData>)` and `RouteSnapshot::resolve(&str) -> Option<RouteMatch>`.
- Produces: `RouteMatch { key: RouteKey, data: Arc<RouteData> }`.

- [ ] **Step 1: Write deterministic and property RED tests**

Add tests covering CHP's segment trie semantics and normalization:

```rust
proptest! {
    #[test]
    fn normalization_is_idempotent(raw in route_key_strategy()) {
        let once = RouteKey::parse(&raw).unwrap();
        let twice = RouteKey::parse(once.as_str()).unwrap();
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn optimized_matcher_equals_reference(
        routes in route_map_strategy(),
        request in request_path_strategy(),
    ) {
        let snapshot = RouteSnapshot::from_routes(routes.clone());
        prop_assert_eq!(
            snapshot.resolve(&request).map(|m| m.key),
            reference_resolve(&routes, &request),
        );
    }
}

#[test]
fn segment_boundary_prevents_partial_match() {
    let snapshot = snapshot_with(["/b/c", "/b/c/d"]);
    assert_eq!(snapshot.resolve("/b/c/dword").unwrap().key.as_str(), "/b/c");
}
```

The test-side `reference_resolve` splits paths into slash-delimited segments and scans every route; it must not call production matching helpers.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test route_properties -- --nocapture`

Expected: compile failure for missing `RouteKey`, `RouteSnapshot`, and `RouteMatch`. Record this and Task 1's structural RED in `docs/test-evidence.md`.

- [ ] **Step 3: Implement the minimal route types and immutable matcher**

Use a nested `BTreeMap<String, Node>` matcher or an equivalent immutable segment index. Preserve root fallback and choose the deepest node containing data. `RouteData` uses flattening:

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RouteData {
    pub target: Url,
    pub last_activity: DateTime<Utc>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
```

`RouteKey::parse` ensures one leading slash and removes all trailing slashes except root, matching CHP's effective clean-path behavior.

- [ ] **Step 4: Verify GREEN and persistence of proptest regressions**

Run: `PROPTEST_CASES=2048 cargo test --test route_properties -- --nocapture`

Expected: all deterministic and generated cases pass. Confirm any generated `proptest-regressions` file is committed if created.

- [ ] **Step 5: Refactor and lint**

Run: `cargo fmt --all && cargo clippy --test route_properties -- -D warnings`

Expected: exit 0 with no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/route.rs src/route_table.rs tests/route_properties.rs docs/test-evidence.md proptest-regressions 2>/dev/null || true
git commit -m "feat: add property-tested CHP route matching"
```

### Task 3: Store Contract, Memory Store, and Atomic Registry

**Files:**
- Modify: `src/store/mod.rs`
- Create: `src/store/memory.rs`
- Modify: `src/route_table.rs`
- Create: `tests/store_contract.rs`

**Interfaces:**
- Produces:

```rust
#[async_trait]
pub trait Store: Send + Sync {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError>;
    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError>;
    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError>;
    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError>;
}
```

- Produces: `RouteRegistry::load`, `get`, `all`, `resolve`, `put`, `update_activity`, and `delete`.
- Guarantees: store mutation succeeds before `ArcSwap<RouteSnapshot>` publication.

- [ ] **Step 1: Write backend-neutral RED tests and a state-machine property**

Define a `memory_store()` constructor and run this contract against it:

```rust
#[tokio::test]
async fn failed_persistence_never_publishes_route() {
    let store = Arc::new(FailingStore::on_put());
    let registry = RouteRegistry::load(store).await.unwrap();
    let result = registry.put(key("/user/a"), route("http://127.0.0.1:9000")).await;
    assert!(result.is_err());
    assert!(registry.resolve("/user/a/tree").is_none());
}
```

The generated state machine applies random `Put`, `Activity`, and `Delete` operations to both `MemoryStore` and a test `BTreeMap`, comparing full snapshots after every operation.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test store_contract -- --nocapture`

Expected: compile failure for missing `Store`, `MemoryStore`, and `RouteRegistry` methods.

- [ ] **Step 3: Implement the memory store and registry**

Use `tokio::sync::RwLock<BTreeMap<...>>` only inside `MemoryStore`. Use `ArcSwap<RouteSnapshot>` inside `RouteRegistry`. For every mutation, clone the current logical map, apply the successful mutation, construct a new snapshot, then publish once.

- [ ] **Step 4: Verify GREEN including concurrency observation**

Run: `PROPTEST_CASES=1024 cargo test --test store_contract -- --nocapture`

Expected: state-machine, failure atomicity, overwrite, merge-activity, missing-delete, and concurrent-reader tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/store/mod.rs src/store/memory.rs src/route_table.rs tests/store_contract.rs
git commit -m "feat: add atomic route registry and memory store"
```

### Task 4: CHP-Compatible CLI and Configuration Validation

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Create: `tests/config_contract.rs`
- Create: `docs/compatibility.md`

**Interfaces:**
- Produces: `Cli::parse_from`, `AppConfig::try_from(Cli)`, `ListenerConfig`, `TlsConfig`, `StoreConfig`, and `ProxyOptions`.
- Consumes: `CONFIGPROXY_AUTH_TOKEN`, `CONFIGPROXY_SSL_KEY_PASSPHRASE`, and `CONFIGPROXY_API_SSL_KEY_PASSPHRASE`.

- [ ] **Step 1: Write table-driven RED tests for every CHP 5.3.0 flag**

The table includes `--ip`, `--port`, `--socket`, all public/API/client SSL flags, `--default-target`, error options, redirect options, PID file, negative boolean flags, rewrite flags, repeated custom headers, insecure, host routing, metrics listener, log level, timeouts, storage backend, and keep-alive timeout.

```rust
#[test]
fn api_port_defaults_to_public_port_plus_one() {
    let cfg = parse_ok(["proxy", "--port", "9100"]);
    assert_eq!(cfg.api_listener, ListenerConfig::Tcp("localhost:9101".parse().unwrap()));
}

#[test]
fn socket_conflicts_with_ip_and_port() {
    parse_err(["proxy", "--socket", "/tmp/proxy.sock", "--port", "8000"])
        .contains("cannot be used with");
}
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --test config_contract -- --nocapture`

Expected: missing parser/config types.

- [ ] **Step 3: Implement Clap parsing and explicit validation**

Map negative CHP flags to positive defaults:

```rust
#[arg(long = "no-x-forward", action = clap::ArgAction::SetFalse, default_value_t = true)]
pub x_forward: bool,
```

Reject error-target plus error-path, redirect without TLS material, incomplete key/cert pairs, invalid log levels, invalid target URLs, and unknown storage backends. Accept `memory`, `redis`, and `sidecar`; an arbitrary Node module path fails startup with a message directing users to the sidecar protocol.

- [ ] **Step 4: Build the compatibility matrix**

`docs/compatibility.md` lists every flag from CHP 5.3.0 lines 24–120 as identical, semantic equivalent, or intentional difference. The only planned semantic difference is Node storage module loading; deprecated RC4 enablement must fail clearly because OpenSSL security policy will not re-enable RC4.

- [ ] **Step 5: Verify GREEN and help snapshot**

Run:

```bash
cargo test --test config_contract -- --nocapture
cargo run -- --help > /tmp/rust-help.txt
node /tmp/chp530/bin/configurable-http-proxy --help > /tmp/chp-help.txt
```

Expected: contract tests pass; a test-owned normalizer confirms all CHP long option names occur in Rust help or in the documented intentional-difference allowlist.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs src/main.rs tests/config_contract.rs docs/compatibility.md
git commit -m "feat: implement CHP 5.3.0 CLI contract"
```

### Task 5: Authenticated Route Management API

**Files:**
- Modify: `src/api.rs`
- Modify: `src/metrics.rs`
- Create: `tests/api_contract.rs`
- Create: `tests/support/mod.rs`

**Interfaces:**
- Produces: `api::router(ApiState) -> axum::Router`.
- `ApiState` contains `Arc<RouteRegistry>`, optional auth token bytes, and `Arc<Metrics>`.
- Produces handlers for `/api/routes` and `/api/routes/{*route}`.

- [ ] **Step 1: Port CHP API behavior into RED tests**

Cover token acceptance/rejection, GET-all, GET-one, 404, POST root/path/escaped route, missing target, malformed JSON, DELETE 204/404, unknown method 405, unknown API path 404, both inactivity query spellings, and invalid timestamps.

```rust
#[tokio::test]
async fn post_preserves_unknown_jupyterhub_metadata() {
    let app = test_api(Some("secret")).await;
    let response = request(&app, "POST", "/api/routes/user/%E7%A7%80%E6%A8%B9", Some(json!({
        "target": "http://127.0.0.1:9000",
        "jupyterhub": true,
        "user": "秀樹"
    })), Some("token secret")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let route = get_json(&app, "/api/routes/user/%E7%A7%80%E6%A8%B9").await;
    assert_eq!(route["user"], "秀樹");
}
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --test api_contract -- --nocapture`

Expected: missing router and handlers.

- [ ] **Step 3: Implement the API with bounded JSON parsing and constant-time token comparison**

Match CHP's case-sensitive `/token\s+(\S+)/` semantics without logging the supplied value. Serialize dates as ISO 8601 strings. Return empty bodies for POST 201 and successful DELETE 204.

- [ ] **Step 4: Add property tests for malformed inputs**

Generate arbitrary byte bodies, percent sequences, auth headers, and query timestamps. Assert the service always returns a response and never panics; valid JSON objects with string targets are the only generated POST class allowed to publish routes.

- [ ] **Step 5: Verify GREEN**

Run: `PROPTEST_CASES=2048 cargo test --test api_contract -- --nocapture`

Expected: all API and malformed-input properties pass.

- [ ] **Step 6: Commit**

```bash
git add src/api.rs src/metrics.rs tests/api_contract.rs tests/support/mod.rs
git commit -m "feat: add CHP-compatible route management API"
```

#### Task 5 final acceptance finding: supervised mutation lifecycle

The registry owns and tracks every `add`, `put`, activity-update, and delete
task. Caller cancellation drops only the response receiver. The shared
supervisor observes every terminal backend result or panic, records detached
diagnostics without mutation data or panic payloads, decrements active count,
and wakes bounded `drain_mutations(timeout)` waiters. Live callers retain exact
backend `Result` behavior, with a fixed operation-specific `StoreError` for a
task panic. Contract tests cover cancelled success and failure, panic, timeout
and later release, concurrent drain, unavailable-runtime spawn failure, runtime
shutdown cancellation, and registry lifetime release.

### Task 6: URI Construction, Header Policy, and Upstream Peer Selection

**Files:**
- Modify: `src/upstream.rs`
- Modify: `src/route.rs`
- Create: `tests/proxy_contract.rs`

**Interfaces:**
- Produces: `Target::parse(&Url) -> Result<Target, TargetError>` with TCP HTTP/HTTPS and Unix HTTP variants.
- Produces: `build_upstream_uri(route, request_uri, options) -> Result<Uri, ProxyRequestError>`.
- Produces: `apply_request_headers(headers, target, options)` and `rewrite_location(response, request, target, options)`.
- Produces: `Target::http_peer(&TlsClientConfig) -> Result<HttpPeer, TargetError>`, using `HttpPeer::new_uds` for Unix targets.

- [ ] **Step 1: Write the URI/header RED matrix and properties**

Cover every `includePrefix × prependPath`, target pathname, root route, escaped path, query, host routing, custom header, x-forward, change-origin, auto-rewrite, and protocol-rewrite combination.

```rust
proptest! {
    #[test]
    fn query_bytes_are_preserved(case in uri_case_strategy()) {
        let output = build_upstream_uri(&case.route, &case.request, &case.options).unwrap();
        prop_assert_eq!(output.query(), case.request.query());
    }
}
```

Assert hop-by-hop headers named by `Connection` are removed and WebSocket upgrade headers remain when the request is an upgrade.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test proxy_contract uri_ -- --nocapture`

Expected: missing URI and header policy functions.

- [ ] **Step 3: Implement pure transformations and peer creation**

Do not mutate strings with broad `replace`. Split route and request paths at known boundaries and join exactly one slash where CHP/http-proxy does. Build `HttpPeer` with `verify_cert`, `verify_hostname`, timeouts, CA, and client certificate settings. Decode `http+unix://<percent-encoded-socket>` into `HttpPeer::new_uds(path, false, String::new())`.

- [ ] **Step 4: Verify GREEN**

Run: `PROPTEST_CASES=4096 cargo test --test proxy_contract uri_ -- --nocapture`

Expected: the matrix and properties pass without ignored generated cases.

- [ ] **Step 5: Commit**

```bash
git add src/upstream.rs src/route.rs tests/proxy_contract.rs
git commit -m "feat: implement property-tested upstream request policy"
```

### Task 7: Pingora HTTP Data Plane, Health, Errors, and Activity

**Files:**
- Modify: `src/proxy.rs`
- Modify: `src/errors.rs`
- Modify: `src/activity.rs`
- Modify: `src/main.rs`
- Extend: `tests/proxy_contract.rs`

**Interfaces:**
- Produces: `ChpProxy: ProxyHttp<CTX = RequestContext>`.
- `RequestContext` stores the resolved route key, original URI/host, activity eligibility, and error classification.
- Produces: `ActivityWriter` background service and `ProxyErrorRenderer`.

- [ ] **Step 1: Write real-network RED tests**

Start an upstream echo server and the Rust binary on reserved ports. Test basic proxying, default route, 404, 503, keep-alive, streaming body, target path behavior, custom headers, redirects untouched by default, enabled rewrites, health precedence, and `last_activity` eligibility.

```rust
#[tokio::test]
async fn unavailable_upstream_returns_503_without_activity() {
    let harness = ProxyHarness::start().await;
    harness.add_route("/missing", unused_target()).await;
    let before = harness.route("/missing").await.last_activity;
    let response = harness.get("/missing/path").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(harness.route("/missing").await.last_activity, before);
}
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --test proxy_contract network_ -- --nocapture`

Expected: proxy harness cannot reach a functional listener or receives incorrect status.

- [ ] **Step 3: Implement `ProxyHttp` callbacks**

Use official Pingora phases:

- `request_filter` for health and route-miss early responses;
- `upstream_peer` for `HttpPeer` selection;
- `upstream_request_filter` for URI and headers;
- `upstream_response_filter` for redirect policy;
- `logging` plus stream callbacks to enqueue qualified activity;
- `fail_to_proxy` or typed error mapping for 500/503.

Activity writes update the in-memory registry first for observable semantics and coalesce persistence by route in a bounded channel. A store error increments a metric and leaves the proxy response unchanged.

- [ ] **Step 4: Implement custom and file error fallback**

Custom error requests use `GET <base>/<status>?url=<percent-encoded-original-uri>` and copy only `content-type` and `content-encoding`. File fallback order is `<status>.html`, `error.html`, then standard reason phrase.

- [ ] **Step 5: Verify GREEN**

Run: `cargo test --test proxy_contract -- --nocapture`

Expected: all pure and network contracts pass.

- [ ] **Step 6: Commit**

```bash
git add src/proxy.rs src/errors.rs src/activity.rs src/main.rs tests/proxy_contract.rs
git commit -m "feat: add Pingora HTTP data plane and CHP errors"
```

### Task 8: API/Public/Metrics Listeners, TLS, mTLS, Unix Sockets, WebSockets, and Shutdown

**Files:**
- Modify: `src/api_server.rs`
- Modify: `src/shutdown.rs`
- Modify: `src/main.rs`
- Create: `tests/websocket_contract.rs`
- Create: `tests/tls_unix_contract.rs`

**Interfaces:**
- Produces: `ApiServer: BackgroundService`, which serves the Axum router on TCP or UDS and honors Pingora shutdown.
- Produces: `install_listener(service, ListenerConfig, Option<TlsConfig>)`.
- Produces: PID file guard removed on normal shutdown.

- [ ] **Step 1: Write RED tests for WebSocket and listener modes**

Test public WS echo, upstream unavailable WS error, public UDS, API UDS, metrics UDS, HTTPS public listener, HTTPS API listener, required API client certificate, upstream CA verification, upstream client certificate, redirect port, SIGTERM drain, and PID cleanup.

```rust
#[tokio::test]
async fn websocket_messages_cross_the_selected_user_route() {
    let harness = ProxyHarness::start_with_ws_upstream().await;
    harness.add_route("/user/alice", harness.ws_target()).await;
    let mut ws = connect_async(harness.ws_url("/user/alice/api/kernels/1/channels")).await.unwrap().0;
    ws.send(Message::Text("ping".into())).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::Text("ping".into()));
}
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --test websocket_contract --test tls_unix_contract -- --nocapture`

Expected: listener/upgrade tests fail because these modes are not assembled.

- [ ] **Step 3: Implement listeners and WebSocket path**

Use Pingora's public proxy service for TCP/TLS/UDS. Use `HttpPeer::new_uds` for Unix upstreams. Implement API TCP/UDS accept loops as a Pingora `BackgroundService`; wrap TLS listeners with the selected OpenSSL configuration. Ensure Pingora's upgrade path preserves `Connection`, `Upgrade`, and WebSocket headers.

- [ ] **Step 4: Implement shutdown/PID/redirect behavior**

Create the PID file atomically with `create_new`, remove it through an RAII guard, and wire Pingora shutdown watch into API and activity services. The redirect service returns 400 without Host and 301 to the configured HTTPS port otherwise.

Shutdown must first stop accepting management requests, then invoke
`RouteRegistry::drain_mutations` with a configured finite bound and report its
structured timeout/failure/panic outcome before Tokio runtime termination. A
timed-out drain must not cancel the remaining registry-owned tasks; runtime
termination is the final fallback after the timeout has been surfaced.

- [ ] **Step 5: Verify GREEN**

Run: `cargo test --test websocket_contract --test tls_unix_contract -- --nocapture`

Expected: every TCP/UDS/TLS/mTLS/WS/shutdown case passes on Linux. Platform-gate Unix cases with `#[cfg(unix)]`, not runtime ignores.

- [ ] **Step 6: Commit**

```bash
git add src/api_server.rs src/shutdown.rs src/main.rs tests/websocket_contract.rs tests/tls_unix_contract.rs
git commit -m "feat: support CHP listener TLS Unix and WebSocket modes"
```

### Task 9: Redis Store with the Shared Conformance Suite

**Files:**
- Create: `src/store/redis.rs`
- Modify: `src/store/mod.rs`
- Extend: `tests/store_contract.rs`
- Create: `compose.test.yml`

**Interfaces:**
- Produces: `RedisStore::connect(RedisStoreConfig) -> Result<RedisStore, StoreError>`.
- Key layout: one versioned hash key, default `pingora-reverse-proxy:routes:v1`; fields are normalized route keys and values are complete JSON `RouteData` records.

- [ ] **Step 1: Add Redis RED conformance and fault tests**

Run the same store contract used by memory against a disposable Redis. Add corrupt JSON, unavailable startup, disconnect during mutation, overwrite, delete-missing, and metadata round-trip cases.

- [ ] **Step 2: Verify RED**

Run: `docker compose -f compose.test.yml up -d redis && TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test --test store_contract redis_ -- --nocapture`

Expected: missing Redis implementation.

- [ ] **Step 3: Implement atomic Redis operations**

Use async Redis commands. `put` serializes before `HSET`; delete uses `HGET` plus `HDEL` in a Lua script so the returned prior value and deletion are atomic. Activity update reads/parses/updates/writes in a watched transaction or Lua script. Reject corrupt snapshots before registry publication.

- [ ] **Step 4: Verify GREEN and restart persistence**

Run the focused command twice without clearing Redis between processes; the second registry load must recover the first process's routes.

- [ ] **Step 5: Commit**

```bash
git add src/store/redis.rs src/store/mod.rs tests/store_contract.rs compose.test.yml
git commit -m "feat: add conformant Redis route persistence"
```

### Task 10: Versioned HTTP/JSON Sidecar Store

**Files:**
- Create: `src/store/sidecar.rs`
- Extend: `tests/store_contract.rs`
- Create: `tests/support/sidecar.rs`
- Create: `docs/sidecar-store.md`

**Interfaces:**
- Produces sidecar endpoints:
  - `GET /v1/routes` → complete route object;
  - `PUT /v1/routes/{encoded-key}` → 204;
  - `PATCH /v1/routes/{encoded-key}/activity` → 204;
  - `DELETE /v1/routes/{encoded-key}` → 200 with prior record or 404;
  - `GET /v1/health` → `{"version":"v1","status":"ok"}`.
- Produces: `SidecarStore::connect(SidecarConfig) -> Result<SidecarStore, StoreError>`.

- [ ] **Step 1: Write sidecar RED conformance tests**

Run the shared store suite against the in-test sidecar fixture. Add version mismatch, 401, timeout, 500 retry, malformed body, unknown metadata, and idempotent PUT cases.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test store_contract sidecar_ -- --nocapture`

Expected: missing adapter/protocol implementation.

- [ ] **Step 3: Implement the bounded client**

Use a `reqwest::Client` with connect and total request deadlines. Retry only transport errors and 5xx responses with bounded exponential delays; never retry 4xx. Send `X-Store-Protocol: v1` and optional bearer auth. Encode route keys as one percent-encoded path component.

- [ ] **Step 4: Document exact protocol and consistency semantics**

`docs/sidecar-store.md` includes request/response examples, JSON schema, auth/TLS options, deadlines, retries, idempotency, error mapping, and startup readiness behavior. The test fixture is the executable conformance reference.

- [ ] **Step 5: Verify GREEN**

Run: `cargo test --test store_contract sidecar_ -- --nocapture`

Expected: the shared contract and protocol failure tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/store/sidecar.rs tests/store_contract.rs tests/support/sidecar.rs docs/sidecar-store.md
git commit -m "feat: add versioned sidecar storage protocol"
```

### Task 11: CHP Metrics and Differential Oracle Harness

**Files:**
- Modify: `src/metrics.rs`
- Create: `scripts/chp-oracle.mjs`
- Create: `tests/differential.rs`
- Extend: `tests/support/mod.rs`
- Extend: `compose.test.yml`

**Interfaces:**
- Produces CHP metric families: `api_route_get`, `api_route_add`, `api_route_delete`, `find_target_for_req`, `last_activity_updating`, `requests_ws`, `requests_web`, `requests_proxy{status}`, and `requests_api{status}`.
- Produces `OraclePair`, which sends one normalized scenario to CHP 5.3.0 and the Rust binary and compares observable outcomes.

- [ ] **Step 1: Write metric and differential RED tests**

The initial scenarios are API CRUD, route escapes, HTTP route selection, all path-option combinations, host routing, redirect policy, 404/503, custom errors, health, metrics names, WS, TLS, mTLS, and UDS.

```rust
#[tokio::test]
async fn chp_and_rust_agree_on_encoded_unicode_route() {
    let pair = OraclePair::start().await;
    pair.post_route("/user/%E7%A7%80%E6%A8%B9", route_json(pair.echo_target())).await;
    pair.assert_same_http("/user/%E7%A7%80%E6%A8%B9/tree?x=%2F").await;
    pair.assert_same_route_tables().await;
}
```

Normalize only Date values, server-generated connection headers, and ephemeral addresses. Keep a documented normalization allowlist in test code.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test differential -- --nocapture`

Expected: oracle helper missing or at least one intentional parity failure.

- [ ] **Step 3: Implement the pinned oracle launcher and metrics**

`scripts/chp-oracle.mjs` imports `/opt/chp-5.3.0/lib/configproxy.js` or a repository path supplied by `CHP_SOURCE_DIR`; it never resolves an unpinned global package. `compose.test.yml` builds a Node 20 CHP 5.3.0 service.

- [ ] **Step 4: Iterate each mismatch with a regression-first fix**

For every mismatch:

1. reduce it to one focused failing Rust contract test;
2. verify RED;
3. fix production code;
4. verify the focused test;
5. rerun the differential scenario.

Do not broaden the normalization allowlist to hide semantic mismatches.

- [ ] **Step 5: Verify GREEN**

Run: `just test-differential` after adding this recipe:

```make
test-differential:
    docker compose -f compose.test.yml up -d --build chp redis sidecar
    CHP_SOURCE_DIR=/opt/chp-5.3.0 cargo test --test differential -- --nocapture
```

Expected: zero unexplained differences.

- [ ] **Step 6: Commit**

```bash
git add src/metrics.rs scripts/chp-oracle.mjs tests/differential.rs tests/support/mod.rs compose.test.yml justfile
git commit -m "test: add pinned CHP differential compatibility gate"
```

### Task 12: Real JupyterHub 5.5.0 External-Proxy E2E

**Files:**
- Create: `scripts/jupyterhub-e2e.py`
- Create: `tests/jupyterhub_e2e.rs`
- Extend: `compose.test.yml`
- Extend: `justfile`

**Interfaces:**
- Produces a JupyterHub configuration with `ConfigurableHTTPProxy.should_start = False`, matching API URL/token, temporary authenticator, and local-process spawner.
- Produces `just test-jupyterhub`.

- [ ] **Step 1: Write E2E RED scenarios**

Cover Hub root route, users `river`, `秀樹`, `has@`, and a name with a space-equivalent escaped route; add/get/delete; proxy restart/reconciliation; Hub restart with existing route usability; host routing; login; single-user page; and real kernel WebSocket message flow.

- [ ] **Step 2: Verify RED**

Run: `just test-jupyterhub`

Expected: missing script/recipe or JupyterHub cannot complete one route/WS scenario.

- [ ] **Step 3: Implement the pinned JupyterHub harness**

The container installs exactly JupyterHub 5.5.0 and a minimal single-user server. The script waits for both proxy endpoints, starts the Hub, creates users through the Hub API, starts servers, polls route state, and opens `/api/kernels/<id>/channels` through the proxy.

- [ ] **Step 4: Verify GREEN with both memory and Redis**

Run:

```bash
STORE_BACKEND=memory just test-jupyterhub
STORE_BACKEND=redis just test-jupyterhub
```

Expected: all scenarios pass for both backends.

- [ ] **Step 5: Commit**

```bash
git add scripts/jupyterhub-e2e.py tests/jupyterhub_e2e.rs compose.test.yml justfile
git commit -m "test: verify JupyterHub 5.5.0 external proxy compatibility"
```

### Task 13: Container, Helm, CI, Security, and Operations Documentation

**Files:**
- Modify: `Dockerfile`
- Modify: `helm-chart/Chart.yaml`
- Modify: `helm-chart/values.yaml`
- Modify: `helm-chart/templates/deployment.yaml`
- Modify: `.github/workflows/ci.yml`
- Create: `scripts/verify.sh`
- Create: `docs/operations.md`
- Modify: `README.md`

**Interfaces:**
- Produces: non-root multi-stage image exposing configurable public/API/metrics listeners.
- Produces: Helm configuration for memory/Redis/sidecar, secret refs, TLS refs, probes, resources, and security context.
- Produces: `scripts/verify.sh` as the clean Linux release gate.

- [ ] **Step 1: Add artifact RED checks**

Add `just test-container` and `just test-helm` recipes that fail against the current image/chart by checking non-root UID, read-only root compatibility, `/_chp_healthz`, API readiness, Helm lint, and rendered secret references.

- [ ] **Step 2: Verify RED**

Run: `just test-container && just test-helm`

Expected: at least the old container startup or chart security/probe assertions fail.

- [ ] **Step 3: Implement production artifacts**

The final container uses a Rust builder and minimal Debian runtime with CA certificates, runs as UID 65532, owns only writable `/tmp`, and uses an exec-form entrypoint. Helm defaults API exposure to ClusterIP, injects `CONFIGPROXY_AUTH_TOKEN` from a Secret, and sets `allowPrivilegeEscalation: false`, dropped capabilities, `runAsNonRoot`, and `readOnlyRootFilesystem: true`.

- [ ] **Step 4: Implement CI and the one-command verifier**

`scripts/verify.sh` executes, in order:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
PROPTEST_CASES=4096 cargo test --all-targets --all-features
just test-differential
just test-jupyterhub
just test-container
just test-helm
cargo audit --deny warnings
cargo deny check
```

CI runs this on Linux, uploads logs and minimized proptest regressions on failure, and runs a secondary macOS `fmt/clippy/unit` job only when the hosted toolchain supports Pingora's native dependencies.

- [ ] **Step 5: Update user and operator documentation**

README contains CHP-to-Rust launch examples, JupyterHub external proxy configuration, store selection, TLS, UDS, and migration notes. `docs/operations.md` documents health, metrics, graceful shutdown, Redis/sidecar outages, recovery, and log redaction.

- [ ] **Step 6: Verify GREEN**

Run: `just test-container && just test-helm`

Expected: image and chart checks pass.

- [ ] **Step 7: Commit**

```bash
git add Dockerfile helm-chart .github/workflows/ci.yml scripts/verify.sh docs/operations.md README.md justfile
git commit -m "build: ship hardened image chart and CI gates"
```

### Task 14: Final Compatibility Closure and Release Evidence

**Files:**
- Modify: `docs/compatibility.md`
- Modify: `docs/test-evidence.md`
- Modify: `README.md`
- Modify only when a failing regression requires it: relevant `src/` and `tests/` files

**Interfaces:**
- Produces a zero-unexplained-difference compatibility report and exact clean-checkout verification evidence.

- [ ] **Step 1: Run the complete clean Linux gate**

Run in a fresh container/worktree: `bash scripts/verify.sh`

Expected: every command exits 0. Save command versions, elapsed times, test counts, proptest case counts, differential scenario count, and JupyterHub E2E scenario count in `docs/test-evidence.md`.

- [ ] **Step 2: Treat every failure as a regression-first loop**

For each failure, add or isolate a focused failing test, verify RED, implement the minimum fix, verify GREEN, and rerun the failed layer before rerunning `scripts/verify.sh`.

- [ ] **Step 3: Audit the compatibility matrix against the CHP CLI source**

Compare `docs/compatibility.md` with `/tmp/chp530/bin/configurable-http-proxy` lines 24–120 and CHP API/proxy tests. Every flag and public behavior must have a test name and status. The Node storage plugin and RC4 differences remain explicit and startup-visible.

- [ ] **Step 4: Run fresh static and dependency checks**

Run:

```bash
cargo tree -d
cargo audit --deny warnings
cargo deny check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: no denied vulnerability/license/source findings and no Clippy warnings. Duplicate crates must be explained or removed when versions can be unified safely.

- [ ] **Step 5: Verify repository state and commit closure evidence**

```bash
git diff --check
git status --short
git log --oneline --decorate -15
```

Expected: only intended evidence/doc updates remain before commit.

```bash
git add docs/compatibility.md docs/test-evidence.md README.md src tests
git commit -m "docs: close CHP 5.3.0 compatibility verification"
```

- [ ] **Step 6: Independent review gate**

Dispatch one fresh correctness reviewer against the design and plan and one fresh security/operability reviewer against the final diff. Resolve every confirmed finding with a regression-first fix, rerun `scripts/verify.sh`, and record the final commit with `git rev-parse HEAD`.

## Source References

- CHP 5.3.0 CLI: https://github.com/jupyterhub/configurable-http-proxy/blob/5.3.0/bin/configurable-http-proxy
- CHP 5.3.0 API: https://github.com/jupyterhub/configurable-http-proxy/blob/5.3.0/lib/configproxy.js
- CHP route trie: https://github.com/jupyterhub/configurable-http-proxy/blob/5.3.0/lib/trie.js
- CHP tests: https://github.com/jupyterhub/configurable-http-proxy/tree/5.3.0/test
- JupyterHub 5.5.0 proxy client: https://github.com/jupyterhub/jupyterhub/blob/5.5.0/jupyterhub/proxy.py
- JupyterHub external proxy guide: https://jupyterhub.readthedocs.io/en/5.5.0/howto/separate-proxy.html
- Pingora 0.8.1 `ProxyHttp`: https://docs.rs/pingora-proxy/0.8.1/pingora_proxy/trait.ProxyHttp.html
- Pingora peer guide: https://github.com/cloudflare/pingora/blob/0.8.1/docs/user_guide/peer.md
- Pingora `HttpPeer::new_uds`: https://docs.rs/pingora-core/0.8.1/pingora_core/upstreams/peer/struct.HttpPeer.html#method.new_uds
- Pingora graceful shutdown: https://github.com/cloudflare/pingora/blob/0.8.1/docs/user_guide/graceful.md
