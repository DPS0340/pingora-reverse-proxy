#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT_DIR
readonly LOG_DIR="${VERIFY_LOG_DIR:-${ROOT_DIR}/.verification-logs}"
mkdir -p "${LOG_DIR}"
cd "${ROOT_DIR}"

if command -v gtimeout >/dev/null 2>&1; then
  TIMEOUT=gtimeout
elif command -v timeout >/dev/null 2>&1; then
  TIMEOUT=timeout
else
  echo "verify: GNU timeout (timeout or gtimeout) is required" >&2
  exit 1
fi

require() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "verify: required tool is missing: $1" >&2
    exit 1
  }
}

for tool in cargo rustc just docker helm; do
  require "${tool}"
done
require python3
"${TIMEOUT}" 15s cargo audit --version >/dev/null 2>&1 || {
  echo "verify: cargo-audit is required" >&2
  exit 1
}
"${TIMEOUT}" 15s cargo deny --version >/dev/null 2>&1 || {
  echo "verify: cargo-deny is required" >&2
  exit 1
}

echo "verify: tool versions"
"${TIMEOUT}" 15s rustc --version
"${TIMEOUT}" 15s cargo --version
"${TIMEOUT}" 15s just --version
"${TIMEOUT}" 15s docker version --format 'docker client={{.Client.Version}} server={{.Server.Version}}'
"${TIMEOUT}" 15s helm version --short
"${TIMEOUT}" 15s cargo audit --version
"${TIMEOUT}" 15s cargo deny --version

run_phase() {
  local phase="$1"
  local limit="$2"
  shift 2
  local log="${LOG_DIR}/${phase}.log"
  echo "verify: phase ${phase}"
  python3 "${ROOT_DIR}/scripts/run-bounded.py" \
    --timeout "${limit}" \
    --kill-after 15s \
    --log "${log}" \
    -- "$@"
}

# Release-gate order is contractual. Do not reorder these phases.
run_phase 01-fmt 180s cargo fmt --all -- --check
run_phase 02-clippy 1800s cargo clippy --locked --all-targets --all-features -- -D warnings
run_phase 03-tests 2400s env PROPTEST_CASES=4096 DIFFERENTIAL_ALL_TARGETS=1 ./scripts/test-differential.sh
run_phase 04-differential 1800s just test-differential
run_phase 05-jupyterhub 1800s just test-jupyterhub
run_phase 06-container 2400s just test-container
run_phase 07-helm 300s just test-helm
run_phase 08-audit 600s cargo audit --deny warnings
run_phase 09-deny 600s cargo deny check

echo "verify: all phases passed"
