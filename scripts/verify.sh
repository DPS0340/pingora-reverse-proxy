#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT_DIR
readonly LOG_DIR="${VERIFY_LOG_DIR:-${ROOT_DIR}/.verification-logs}"
readonly MANIFEST_FILE="${LOG_DIR}/manifest.tsv"
mkdir -p "${LOG_DIR}"
rm -f "${LOG_DIR}"/[0-9][0-9]-*.log "${MANIFEST_FILE}"
cd "${ROOT_DIR}"

readonly PROPTEST_CASES=4096
VERIFY_STARTED_NS="$(python3 -c 'import time; print(time.monotonic_ns())')"
readonly VERIFY_STARTED_NS
printf 'schema\tpingora-verification-v1\n' >"${MANIFEST_FILE}"
printf 'source_sha\t%s\n' "$(git rev-parse --verify HEAD)" >>"${MANIFEST_FILE}"

finalize_manifest() {
  local status=$?
  trap - EXIT
  set +e
  python3 "${ROOT_DIR}/scripts/finalize-verification-manifest.py" \
    "${MANIFEST_FILE}" "${VERIFY_STARTED_NS}" "${status}"
  local finalizer_status=$?
  if ((status != 0)); then
    exit "${status}"
  fi
  exit "${finalizer_status}"
}
trap finalize_manifest EXIT

PROPERTY_RUNNER_COUNT="$(python3 scripts/verify-property-inventory.py --count)"
readonly PROPERTY_RUNNER_COUNT
readonly PROPERTY_CASES=$((PROPERTY_RUNNER_COUNT * PROPTEST_CASES))
printf 'count\tproperty_runners\t%s\n' "${PROPERTY_RUNNER_COUNT}" >>"${MANIFEST_FILE}"
printf 'count\tproperty_cases\t%s\n' "${PROPERTY_CASES}" >>"${MANIFEST_FILE}"

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
record_tool() {
  local name="$1"
  shift
  local output
  output="$("${TIMEOUT}" 15s "$@")"
  printf '%s\n' "${output}"
  output="${output//$'\t'/ }"
  output="${output//$'\n'/ }"
  printf 'tool\t%s\t%s\n' "${name}" "${output}" >>"${MANIFEST_FILE}"
}

record_tool rustc rustc --version
record_tool cargo cargo --version
record_tool python3 python3 --version
record_tool just just --version
record_tool docker docker version --format 'docker client={{.Client.Version}} server={{.Server.Version}}'
if "${TIMEOUT}" 15s docker compose version >/dev/null 2>&1; then
  record_tool docker-compose docker compose version
elif command -v docker-compose >/dev/null 2>&1; then
  record_tool docker-compose docker-compose version
else
  echo "verify: Docker Compose is required" >&2
  exit 1
fi
record_tool helm helm version --short
record_tool cargo-audit cargo audit --version
record_tool cargo-deny cargo deny --version

run_phase() {
  local phase="$1"
  local limit="$2"
  shift 2
  local log="${LOG_DIR}/${phase}.log"
  local started_ns ended_ns elapsed_ms status
  echo "verify: phase ${phase}"
  started_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
  set +e
  python3 "${ROOT_DIR}/scripts/run-bounded.py" \
    --timeout "${limit}" \
    --kill-after 30s \
    --log "${log}" \
    -- "$@"
  status=$?
  set -e
  ended_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
  elapsed_ms=$(((ended_ns - started_ns) / 1000000))
  printf 'phase\t%s\tstatus\t%s\telapsed_milliseconds\t%s\tlog\t%s\n' \
    "${phase}" "${status}" "${elapsed_ms}" "$(basename "${log}")" \
    >>"${MANIFEST_FILE}"
  return "${status}"
}

# Release-gate order is contractual. Do not reorder these phases.
run_phase 01-fmt 180s cargo fmt --all -- --check
run_phase 02-clippy 1800s cargo clippy --locked --all-targets --all-features -- -D warnings
run_phase 03-tests 2400s env \
  PROPTEST_CASES="${PROPTEST_CASES}" DIFFERENTIAL_ALL_TARGETS=1 \
  /bin/bash -o pipefail -c \
  'python3 scripts/verify-vendor-provenance.py && ./scripts/test-differential.sh && cargo test --locked --all-targets --all-features -- --list | python3 scripts/verify-property-inventory.py --verify-list'
run_phase 04-differential 1800s just test-differential
run_phase 05-jupyterhub 1800s env -u STORE_BACKEND just test-jupyterhub
run_phase 06-container 2400s just test-container
run_phase 07-helm 300s just test-helm
run_phase 08-audit 600s cargo audit --deny warnings
run_phase 09-deny 600s cargo deny check

python3 scripts/summarize-verification.py "${LOG_DIR}" "${MANIFEST_FILE}"

echo "verify: all phases passed"
