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
