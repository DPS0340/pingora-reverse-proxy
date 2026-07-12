# Test Evidence

## Task 1 structural RED

Command:

```bash
cargo check --lib
```

Exit code: `101`

First missing-module diagnostic captured by Task 1:

```text
error[E0583]: file not found for module `activity`
 --> src/lib.rs:1:1
```

## Task 2 route matcher RED

Command:

```bash
cargo test --test route_properties -- --nocapture
```

Exit code: `101`

Route-test diagnostics:

```text
error[E0432]: unresolved imports `pingora_reverse_proxy::route::RouteData`, `pingora_reverse_proxy::route::RouteKey`
 --> tests/route_properties.rs:4:36
  |
4 | use pingora_reverse_proxy::route::{RouteData, RouteKey};
  |                                    ^^^^^^^^^  ^^^^^^^^ no `RouteKey` in `route`
  |                                    |
  |                                    no `RouteData` in `route`

error[E0432]: unresolved imports `pingora_reverse_proxy::route_table::RouteMatch`, `pingora_reverse_proxy::route_table::RouteSnapshot`
 --> tests/route_properties.rs:5:42
  |
5 | use pingora_reverse_proxy::route_table::{RouteMatch, RouteSnapshot};
  |                                          ^^^^^^^^^^  ^^^^^^^^^^^^^ no `RouteSnapshot` in `route_table`
  |                                          |
  |                                          no `RouteMatch` in `route_table`
```

Cargo also attempted to build the pre-existing binary target and reported its known
missing legacy dependencies and `DynamicGateway` implementation. Those unrelated
errors remain outside Task 2's four-file scope.

## Task 2 focused behavioral GREEN

Required Cargo command attempted after implementation:

```bash
PROPTEST_CASES=2048 cargo test --test route_properties -- --nocapture
```

Exit code: `101`

Output:

```text
   Compiling pingora-reverse-proxy v0.2.0 (/Users/lee/programming/pingora-reverse-proxy/.worktrees/chp-compatibility)
error[E0432]: unresolved import `itertools`
 --> src/utils.rs:4:5
error[E0433]: cannot find module or crate `pingora_core` in this scope
 --> src/main.rs:6:5
error[E0432]: unresolved import `log`
 --> src/utils.rs:5:5
error[E0432]: unresolved import `pingora_proxy`
 --> src/main.rs:7:5
error[E0432]: unresolved import `proxy::DynamicGateway`
 --> src/main.rs:8:5
error[E0433]: cannot find module or crate `once_cell` in this scope
 --> src/redis_utils.rs:2:5
error[E0432]: unresolved import `log`
 --> src/redis_utils.rs:1:5
error[E0433]: cannot find module or crate `pingora_runtime` in this scope
  --> src/main.rs:25:19
error[E0599]: no method named `collect_vec` found for struct `std::str::Split<'a, P>` in the current scope
  --> src/utils.rs:17:10
error[E0433]: cannot find module or crate `env_logger` in this scope
  --> src/main.rs:11:5
error: could not compile `pingora-reverse-proxy` (bin "pingora-reverse-proxy") due to 10 previous errors
warning: build failed, waiting for other jobs to finish...
```

The command produced the route test executable before the unrelated binary failure.
The exact produced executable was then run directly with the required case count:

```bash
PROPTEST_CASES=2048 target/debug/deps/route_properties-b466b557fcc1286b --nocapture
```

Exit code: `0`

Output:

```text
running 10 tests
test route_key_normalizes_leading_and_trailing_slashes ... ok
test route_data_round_trips_unknown_fields ... ok
test route_match_carries_shared_route_data ... ok
test segment_boundary_prevents_partial_match ... ok
test deepest_route_with_data_wins ... ok
test unmatched_path_without_root_returns_none ... ok
test interior_empty_segments_are_significant ... ok
test root_route_is_the_fallback ... ok
test normalization_is_idempotent ... ok
test optimized_matcher_equals_reference ... ok

test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.81s
```

No `proptest-regressions` file was generated.

## Task 2 format and lint GREEN

Command:

```bash
cargo fmt --all && cargo clippy --test route_properties -- -D warnings
```

Exit code: `0`

Final output:

```text
    Checking pingora-reverse-proxy v0.2.0 (/Users/lee/programming/pingora-reverse-proxy/.worktrees/chp-compatibility)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.33s
```

## Task 5 supervised mutation lifecycle RED

Command:

```bash
cargo test --test store_contract --no-run
```

Exit code: `101`

The contract tests failed before production changes with the expected missing
supervisor API diagnostics:

```text
error[E0432]: unresolved import `pingora_reverse_proxy::route_table::MutationOperation`
error[E0599]: no method named `drain_mutations` found for struct `Arc<RouteRegistry>`
error: could not compile `pingora-reverse-proxy` (test "store_contract") due to 8 previous errors
```

## Task 5 supervised mutation lifecycle focused GREEN

Command:

```bash
cargo test --test store_contract -- --nocapture
```

Exit code: `0`

```text
running 29 tests
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The intentional panic-backend test triggers Rust's process-wide panic hook,
then verifies that the live caller receives a fixed `StoreError`, the detached
caller produces a fixed payload-free tracker diagnostic, and active count
returns to zero.

## Task 5 supervised mutation lifecycle final verification

Command:

```bash
PROPTEST_CASES=2048 cargo test --test api_contract --test config_contract --test route_properties --test store_contract -- --nocapture
```

Exit code: `0`. Results: API `33 passed`, config `26 passed`, route `11
passed`, store `29 passed`; no failures, ignores, or filtered tests.

Quality gates:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
```

All three exited `0`. No dependency was added.

## Task 5 final supervisor findings RED

The lifecycle/overflow contracts were compiled before production changes:

```bash
cargo test --test store_contract --no-run
```

Exit code: `101`. The expected failures were the missing
`DETACHED_MUTATION_DIAGNOSTIC_CAPACITY` export and missing
`MutationDrainOutcome::{dropped_detached_failures,dropped_detached_panics}`
fields.

The process-hook contract was then run before installing the hook:

```bash
cargo test --test store_contract process_panic_hook_redacts_payload_for_live_and_detached_mutations -- --exact --nocapture
```

Exit code: `101`. The subprocess stderr did not contain the required fixed
`process panic redacted at ...` evidence, proving the default hook still exposed
the panic path before the fix.

## Task 5 final supervisor findings GREEN

The focused store contract suite passed after the handle-free RAII tracker,
256-entry diagnostic ring, race-safe drain loop, and once-only process hook were
implemented:

```bash
cargo test --test store_contract -- --nocapture
```

Exit code: `0`; `34 passed`, `0 failed`. The panic tests emitted only fixed
redaction text with `tests/store_contract.rs` line/column evidence. The
subprocess assertion also proved its secret payload sentinel was absent while
live and detached callers received fixed route-put panic errors.

The high-case-count compatibility matrix passed:

```bash
PROPTEST_CASES=2048 cargo test --test api_contract --test config_contract --test route_properties --test store_contract -- --nocapture
```

Exit code: `0`. Results: API `33 passed`, config `26 passed`, route `11 passed`,
store `34 passed`; no failures, ignores, or filtered tests.

The full all-target/all-feature suite also passed with `PROPTEST_CASES=2048`:

```bash
PROPTEST_CASES=2048 cargo test --all-targets --all-features -- --nocapture
```

Exit code: `0`; the same `104` integration contracts passed, with both unit-test
targets reporting no unit tests and no failures.

Final quality gates:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
```

All three exited `0`. `git diff --check` also exited `0`. No dependency was
added.

## Task 7 final concurrency findings RED

Each final finding was reproduced before its production change.

- The resolver refill regression failed to compile with missing
  `resolver_batch_count` and `take_resolver_batch`, establishing the absent
  fixed-batch snapshot API (`cargo test --lib
  errors::tests::resolver_batch_snapshot_leaves_adversarial_refill_for_the_next_turn
  -- --exact --nocapture`, exit `101`).
- The generation interleaving connected a later request to generation N before
  the request owning N completed (`cargo test --lib
  errors::tests::failed_request_invalidates_the_generation_consumed_by_its_connector
  -- --exact --nocapture`, exit `101`). The companion expiry regression then
  observed a second lookup while generation N should have remained leased
  (`errors::tests::resolver_generation_lease_pins_addresses_across_cache_expiry`,
  exit `101`).
- The activity flush regression timed out while post-watermark activity kept
  the hot key resident (`cargo test --test proxy_contract
  activity_flush_uses_an_acceptance_watermark_for_a_continuously_advancing_key
  -- --exact --nocapture`, exit `101`).

## Task 7 final concurrency findings GREEN

The three exact focused tests passed after fixed resolver batching,
deadline-bounded request generation serialization, and per-entry activity
sequence watermarks were implemented. Existing resolver single-flight/bounded
I/O tests and all eight activity-related proxy contracts also passed.

The full all-target/all-feature suite passed with 256 property cases:

```bash
PROPTEST_CASES=256 cargo test --all-targets --all-features -- --nocapture
```

Exit code: `0`. Results: library `8 passed`, API `33 passed`, config `26 passed`,
proxy `68 passed`, route `11 passed`, store `45 passed`, and binary `0 passed`;
`191 passed` total with no failures, ignores, measured, or filtered tests.

The dedicated route property suite passed with 512 cases:

```bash
PROPTEST_CASES=512 cargo test --test route_properties -- --nocapture
```

Exit code: `0`; `11 passed`, with no failures, ignores, measured, or filtered
tests.

Final quality gates:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
git diff --check
```

All four exited `0`. No dependency was added.

## Task 8 listener, TLS, WebSocket, and shutdown evidence

The new real-binary contracts were observed RED before listener assembly. The
initial `cargo test --test websocket_contract --test tls_unix_contract --
--nocapture` run produced four listener/PID failures, and the direct WebSocket
run produced two exact-readiness failures because the API listener did not
exist. No contract was ignored, runtime-skipped, or weakened.

After implementation, the focused Task 8 suite passed:

```bash
cargo test --test websocket_contract --test tls_unix_contract -- --nocapture
```

Exit code: `0`. Results: TLS/Unix/listener/lifecycle `12 passed`, WebSocket `2
passed`; no failures, ignores, measured, or filtered tests. Coverage includes
public WebSocket echo and empty unavailable 503 handshake; public/API/metrics
TCP and UDS; public/API HTTPS with encrypted listener keys; required API client
certificate; positive and negative private upstream CA verification; upstream
client certificate; exact redirect 400/301 behavior; active-response SIGTERM
drain; and atomic PID refusal/cleanup on normal and startup-error paths.
The API TLS listener also rejects a silent handshake after a fixed one-second
bound so a stalled client cannot block later exact API requests indefinitely;
this regression was observed failing before the bound was added.

The requested existing suite counts passed with 256 property cases: library
`8`, proxy `69`, store `45`, API `33`, config `26`, and route `11`. The route
suite separately passed all `11` contracts with `PROPTEST_CASES=512`.

The complete host-macOS verification passed:

```bash
PROPTEST_CASES=256 cargo test --all-targets --all-features -- --nocapture
```

Exit code: `0`; `206 passed` total with no failures or ignores (library `8`,
binary `0`, API `33`, config `26`, proxy `69`, route `11`, store `45`, Task 8
TLS/Unix `12`, and WebSocket `2`). Unix contracts are compile-time gated with
`#[cfg(unix)]`.

Final quality gates:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
git diff --check
```

All four exited `0`. Docker was unavailable and was not used. Runtime
`openssl`/`tokio-openssl` support Axum TLS listeners; `futures-util` is test-only
for WebSocket frame assertions.

### Task 8 active-shutdown follow-up (2026-07-13)

The original one-run shutdown evidence was superseded after a clean-HEAD
`74efd70` parent run failed the active-response contract at the HTTP response
delimiter (`11/12` passed). The repaired contract now waits for exact management
and public readiness, retains the public client stream, waits until the upstream
has received the proxied request and written response headers plus a body
prefix, and only then sends SIGTERM. The upstream withholds the remaining body
behind an explicit channel while a bounded 1.5-second hold proves the process
does not terminate admitted traffic; explicit release must produce the exact
`drained` response and a successful process exit.

The deterministic test was RED against the old production shutdown phases:
Pingora had a zero-second grace period and began runtime teardown after one
second, disconnecting the active response. A longer final runtime timeout alone
still truncated the response to `dra`, proving that timeout was the wrong
phase. Production now uses a six-second Pingora grace phase, derived from the
five-second terminal mutation drain plus one second, followed by a separate
one-second final runtime timeout.

The focused active-response test passed 20 consecutive runs (`20/20`). Both
Task 8 executables then passed (`12` TLS/Unix/lifecycle and `2` WebSocket). The
full `PROPTEST_CASES=256 cargo test --all-targets --all-features -- --nocapture`
run passed `206` tests: library `8`, binary `0`, API `33`, config `26`, proxy
`69`, route `11`, store `45`, TLS/Unix `12`, and WebSocket `2`, with zero
failures or ignores. The dedicated `PROPTEST_CASES=512` route suite passed all
`11` tests. Clippy with `-D warnings` and the all-target/all-feature check both
exited `0`; final formatting and diff checks were rerun after documentation.

### Task 8 final acceptance closure (2026-07-13)

Bounded probes confirmed that the interrupted foreground command was a normal
long-running server, not a startup hang. The actual partial prebind defect was a
public socket transferred without listener/nonblocking setup: TCP connected but
health responses timed out. RED regressions also reproduced serialized API TLS
handshakes behind 24 silent clients and incorrect public UDS cleanup identity on
macOS. Each passed after the production fix.

The completed contracts cover synchronous fail-closed public TCP/TLS/UDS
ownership, every public/API/metrics collision, canonical UDS aliases and
replacement-safe cleanup, bounded 64-way TLS handshake dispatch, explicit CHP
5.3.0 client-TLS flag rejection, Windows and Unix PID identity, exact ordered
shutdown acknowledgements and watermarks, a real selected-route Unix upstream,
and WebSocket binary/close/header/query/UDS/activity/active-drain behavior. Test
subprocesses continuously drain stdout and stderr; WebSocket readiness retries
only early process exits caused by the dropped-port reservation window.

Verification passed:

```bash
cargo test --test websocket_contract --test tls_unix_contract -- \
  --nocapture --test-threads=1
```

Results: TLS/Unix/lifecycle `20 passed`; WebSocket `4 passed`.

The active HTTP shutdown regression passed 20 consecutive bounded runs
(`20/20`). A separate unit stress drained 64 concurrent admitted lifecycle
tokens after admission closed. The complete matrix passed:

```bash
PROPTEST_CASES=256 cargo test --all-targets --all-features -- \
  --nocapture --test-threads=1
PROPTEST_CASES=512 cargo test --test route_properties -- \
  --nocapture --test-threads=1
```

The first command passed `218` tests with zero failures or ignores: library 9,
binary 0, API 33, config 27, proxy 69, routes 11, store 45, TLS/Unix 20, and
WebSocket 4. The second passed all 11 route contracts. `cargo fmt --check`,
Clippy with `-D warnings`, all-target/all-feature `cargo check`, and
`git diff --check` all exited `0`. Cargo resolved Pingora 0.8.1 exactly. No Task
9 files or implementation were added.

### Task 8 cross-platform lifecycle ownership re-review (2026-07-13)

The four final re-review findings were reproduced before production changes.
Filesystem-owner tests forced a foreign replacement immediately before cleanup
and a second collision immediately before restoration. Optional mTLS rejected a
foreign-CA client, the public build-failure injection was ignored and remained
running, and the non-Unix readiness policy still had an unsafe wrapper.

UDS and PID cleanup now atomically rename the public entry to an unpredictable
same-directory quarantine with no-replace semantics, verify the private entry's
identity, and unlink only that verified private identity. A foreign entry is
restored with no-replace; if restoration collides, both entries are preserved
and the quarantine is logged. Unix cleanup is descriptor-relative to the
original canonical parent, while the configured public spelling remains the
Pingora FD-adoption key. PID creation is descriptor-relative and exclusive on
Unix. Deterministic tests cover the replacement boundary, restoration
collision, parent-directory replacement, ordinary cleanup, and absence of
private debris.

Non-Unix public startup now fails synchronously before PID, API, metrics, or
redirect binding. On Unix, management listeners depend on actual public
readiness. The public wrapper polls Pingora's service through its synchronous FD
adoption/listener-build prefix before forwarding `ServiceReadyNotifier`, catches
panics, and uses an RAII exit guard for panic and cancellation. A debug-only
injected build failure proves no readiness, nonzero orderly exit, public/API/PID
cleanup, no operational API, and no publication/quarantine debris. The
`process::exit` bypass was removed. Traffic admission remains single-release:
the request context takes its token in logging and its `Drop` fallback owns
cancellation/panic cleanup.

For `requestCert=true` with `rejectUnauthorized=false`, OpenSSL retains `PEER`
but uses a permissive verification callback, matching the pinned CHP 5.3.0
Node TLS options: absent, foreign-CA, and trusted client certificates all pass.
Strict public and API modes reject absent and foreign-CA certificates and accept
the trusted identity. Upstream client request/reject flags remain rejected.

Verification after the final implementation:

- focused Task 8: TLS/Unix/lifecycle `23 passed`; WebSocket `4 passed`;
- stress: cleanup boundary, injected service failure, TLS silent-client
  saturation/termination, active HTTP lifecycle, and active WebSocket lifecycle
  each passed `20/20`;
- default-parallel `PROPTEST_CASES=256` all targets/features: `231 passed`, zero
  failures or ignores (library 19, binary 0, API 33, config 27, proxy 69,
  routes 11, store 45, TLS/Unix 23, WebSocket 4);
- `PROPTEST_CASES=512` routes: `11 passed`;
- format, Clippy with warnings denied, all-target/all-feature check, and diff
  check: all exit `0`.

Only `aarch64-apple-darwin` is installed, so a Windows cross-check could not be
run. The non-Unix startup rejection has mutually exclusive cfg implementations
and a target-cfg unit contract; Windows PID identity/quarantine code remains
source-covered. Pingora stayed pinned at 0.8.1 and CHP at 5.3.0. No Task 9 work
was added.

### Task 8 final atomic-ownership and shutdown-bound wave (2026-07-13)

The interrupted worktree based on `c34ddd4` was preserved and audited. Focused
unit verification passed `25/25`. The Task 8 integration command passed `27/27`
(`23` TLS/Unix/listener/lifecycle and `4` WebSocket), including a real Pingora
listener-build failure produced by adopting `/dev/null` as a non-listener FD.

The final-wave stress axes each passed 20 consecutive runs:

- exact five-second terminal bound and strict grace containment: `20/20`;
- 0700 private quarantine after verification/namespace replacement: `20/20`
  for each adversarial branch (`40` test invocations);
- dirfd-relative UDS publication across parent-alias replacement: `20/20`;
- cancellation while the Pingora FD table is locked: `20/20`;
- `RequestContext` callback panic/cancellation/drop single release: `20/20`.

The real Pingora non-listener FD build-failure integration contract was also
rerun separately and passed `20/20`. Earlier evidence above already records the
five operational cleanup/TLS/HTTP/WebSocket stress axes; the current focused
integration run reconfirmed each once after this wave.

Default-parallel regression verification passed:

```bash
PROPTEST_CASES=256 cargo test --all-targets --all-features -- --nocapture
```

Exit code `0`: `237 passed`, zero failed or ignored (library `25`, binary `0`,
API `33`, config `27`, proxy `69`, routes `11`, store `45`, TLS/Unix `23`,
WebSocket `4`). `PROPTEST_CASES=512 cargo test --test route_properties --
--nocapture` passed `11/11`.

The Unix cleanup guarantee is intentionally bounded: the unpredictable 0700
dirfd-owned namespace removes the public-parent verify/unlink race for
separate-UID attackers under normal Unix permissions. Same-UID namespace
attackers are outside the enforceable boundary and no mathematical absolute is
claimed against them. Ordinary cleanup leaves no private debris; foreign
replacement and restoration collisions remain preserved.

Final quality gates all exited `0`:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
git diff --check
```

### Task 8 listener-adoption window closure (2026-07-13)

The interrupted worktree based on `98ea4b8` was preserved. The three previously
reported TLS/Unix integration failures were first rerun individually with
`--nocapture`; each passed `1/1` with `22` tests filtered out. The complete
TLS/Unix executable then passed `23/23` under default parallelism.

A consolidated default-parallel unit run exposed the remaining test-only FD
race: the successful-adoption assertion used `dup2` after Pingora had closed
its listener descriptor, so another parallel test could already own the reused
number. That run aborted with Rust's exact `IO Safety violation: owned file
descriptor already closed` diagnostic. The assertion now directly observes
that successful Pingora adoption disarms the handoff guard. A separate
self-contained identity test replaces an open descriptor atomically and proves
that an armed late guard does not close the replacement.

Production retains a raw-FD identity guard from insertion into Pingora's
non-owning FD table until listener construction is observed. Cancellation and
panic while Pingora waits for its second FD-table lock close the original
descriptor, withhold readiness, acknowledge public exit, and unwind PID/UDS
owners. UDS prebinding captures the parent before bind and uses its stable
descriptor-backed path after alias replacement. Faults during private cleanup
namespace setup remove only the matching empty namespace and preserve foreign
replacements.

The new stress axes passed `20/20`: cancellation at the second FD-table lock,
panic at that lock, successful adoption disarm, late replacement identity,
pre-bind parent capture, and all four namespace-fault stages per iteration. The
three previously reported integration axes also each passed `20/20`: injected
public build failure, public/API/metrics UDS service and cleanup, and listener
alias/replacement cleanup.

Final focused verification passed `32/32` unit tests and `27/27` Task 8
integration tests (`23` TLS/Unix and `4` WebSocket). Default-parallel
`PROPTEST_CASES=256 cargo test --all-targets --all-features -- --nocapture`
passed `244/244`: library `32`, binary `0`, API `33`, config `27`, proxy `69`,
routes `11`, store `45`, TLS/Unix `23`, and WebSocket `4`. The dedicated
`PROPTEST_CASES=512` route suite passed `11/11`.

Mechanical closure reran `cargo test --test tls_unix_contract` under default
parallelism (`23/23`) and `PROPTEST_CASES=256 cargo test --all-targets
--all-features` (`244/244`, with the same per-executable counts above). The
requested target name `routes_contract` does not exist in this repository;
Cargo lists the suite as `route_properties`, and `PROPTEST_CASES=512 cargo test
--test route_properties` passed `11/11`. Formatting, warnings-denied Clippy,
all-target/all-feature checking, and whitespace validation also passed.
