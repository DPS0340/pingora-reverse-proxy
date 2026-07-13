set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets --all-features

test-differential:
    #!/usr/bin/env bash
    set -euo pipefail
    if docker compose version >/dev/null 2>&1; then
        compose=(docker compose)
    else
        compose=(docker-compose)
    fi
    "${compose[@]}" -f compose.test.yml up -d --build chp redis sidecar
    scratch=""
    cleanup() {
        if [[ -n "$scratch" ]]; then
            rm -rf "$scratch"
        fi
        "${compose[@]}" -f compose.test.yml down
    }
    trap cleanup EXIT INT TERM
    source_dir="${CHP_SOURCE_DIR:-}"
    if [[ -z "$source_dir" ]]; then
        scratch="$(mktemp -d "${TMPDIR:-/tmp}/chp-5.3.0.XXXXXX")"
        "${compose[@]}" -f compose.test.yml cp chp:/opt/chp-5.3.0 "$scratch/"
        source_dir="$scratch/chp-5.3.0"
    fi
    CHP_SOURCE_DIR="$source_dir" cargo test --test differential -- --nocapture

verify: fmt lint test
