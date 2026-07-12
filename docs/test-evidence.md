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
