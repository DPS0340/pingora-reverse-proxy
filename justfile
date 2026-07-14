set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets --all-features

test-differential:
    ./scripts/test-differential.sh

test-jupyterhub:
    python3 scripts/test_jupyterhub_e2e.py -v
    python3 scripts/jupyterhub-e2e.py

verify: fmt lint test
