set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets --all-features

test-differential:
    ./scripts/test-differential.sh

verify: fmt lint test
