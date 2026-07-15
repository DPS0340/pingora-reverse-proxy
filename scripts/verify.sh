#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT_DIR
readonly LOG_DIR="${VERIFY_LOG_DIR:-${ROOT_DIR}/.verification-logs}"
readonly MANIFEST_FILE="${LOG_DIR}/manifest.tsv"
mkdir -p "${LOG_DIR}"
cd "${ROOT_DIR}"

VERIFY_STARTED_NS="$(python3 -c 'import time; print(time.monotonic_ns())')"
readonly VERIFY_STARTED_NS
printf 'schema\tpingora-verification-v1\n' >"${MANIFEST_FILE}"
printf 'source_sha\t%s\n' "$(git rev-parse --verify HEAD)" >>"${MANIFEST_FILE}"

finalize_manifest() {
  local status=$?
  trap - EXIT
  set +e
  local ended_ns elapsed_ms
  ended_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
  elapsed_ms=$(((ended_ns - VERIFY_STARTED_NS) / 1000000))
  printf 'result\tstatus\t%s\telapsed_milliseconds\t%s\n' \
    "${status}" "${elapsed_ms}" >>"${MANIFEST_FILE}"
  return "${status}"
}
trap finalize_manifest EXIT

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
record_tool just just --version
record_tool docker docker version --format 'docker client={{.Client.Version}} server={{.Server.Version}}'
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
    --kill-after 15s \
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
run_phase 03-tests 2400s env PROPTEST_CASES=4096 DIFFERENTIAL_ALL_TARGETS=1 ./scripts/test-differential.sh
run_phase 04-differential 1800s just test-differential
run_phase 05-jupyterhub 1800s just test-jupyterhub
run_phase 06-container 2400s just test-container
run_phase 07-helm 300s just test-helm
run_phase 08-audit 600s cargo audit --deny warnings
run_phase 09-deny 600s cargo deny check

printf 'count\tproperty_cases\t36864\n' >>"${MANIFEST_FILE}"
python3 - "${LOG_DIR}" "${MANIFEST_FILE}" <<'PY'
import json
import re
import sys
from pathlib import Path

log_dir = Path(sys.argv[1])
manifest = Path(sys.argv[2])
rust_tests = 0
differential_cases = 0
jupyterhub_scenarios = 0
jupyterhub_runs = 0
for log in sorted(log_dir.glob("[0-9][0-9]-*.log")):
    text = log.read_text(encoding="utf-8", errors="replace")
    passed = sum(
        int(match.group(1))
        for match in re.finditer(r"test result: ok\. (\d+) passed", text)
    )
    rust_tests += passed
    if log.name == "04-differential.log":
        differential_cases += passed
    for line in text.splitlines():
        marker = "JUPYTERHUB_E2E_SUMMARY="
        if marker in line:
            summary = json.loads(line.split(marker, 1)[1].strip())
            jupyterhub_runs += 1
            jupyterhub_scenarios += len(summary["scenarios"])

if rust_tests == 0:
    raise SystemExit("verification manifest found no passing Rust tests")
if differential_cases == 0:
    raise SystemExit("verification manifest found no differential cases")
if jupyterhub_runs == 0 or jupyterhub_scenarios == 0:
    raise SystemExit("verification manifest found no JupyterHub scenario summaries")

with manifest.open("a", encoding="utf-8") as output:
    output.write(f"count\trust_test_passed\t{rust_tests}\n")
    output.write(f"count\tdifferential_cases\t{differential_cases}\n")
    output.write(f"count\tjupyterhub_runs\t{jupyterhub_runs}\n")
    output.write(f"count\tjupyterhub_scenarios\t{jupyterhub_scenarios}\n")
PY

echo "verify: all phases passed"
