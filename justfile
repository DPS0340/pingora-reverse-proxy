set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --locked --all-targets --all-features -- -D warnings

test:
    cargo test --locked --all-targets --all-features

test-differential:
    ./scripts/test-differential.sh

test-jupyterhub:
    python3 scripts/test_jupyterhub_e2e.py -v
    python3 scripts/jupyterhub-e2e.py

test-container:
    ./scripts/test-container.sh

test-container-script:
    ./scripts/test-container-script.sh

test-verify:
    ./scripts/test-verify.sh

test-release:
    ./scripts/test-release.sh

test-helm:
    ./scripts/test-helm.sh

verify:
    ./scripts/verify.sh
