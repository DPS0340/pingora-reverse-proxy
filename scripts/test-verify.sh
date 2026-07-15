#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pingora-verify-test.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

cat >"$TMP_DIR/cleanup-phase.sh" <<'PHASE'
#!/usr/bin/env bash
set -euo pipefail
resource_dir=$1
touch "$resource_dir/parent" "$resource_dir/child"
cleanup_parent() {
  rm -f "$resource_dir/parent"
  echo parent-term-cleanup
  wait || true
  exit 0
}
trap cleanup_parent TERM
(
  cleanup_child() {
    rm -f "$resource_dir/child"
    echo child-term-cleanup
    exit 0
  }
  trap cleanup_child TERM
  while true; do
    echo nested-log-pipe
    sleep 0.05
  done
) &
wait
PHASE
chmod 0755 "$TMP_DIR/cleanup-phase.sh"

mkdir "$TMP_DIR/resources"
start=$(python3 -c 'import time; print(time.monotonic())')
set +e
python3 "$ROOT_DIR/scripts/run-bounded.py" \
  --timeout 1.5s \
  --kill-after 1s \
  --log "$TMP_DIR/timeout.log" \
  -- "$TMP_DIR/cleanup-phase.sh" "$TMP_DIR/resources"
status=$?
set -e
elapsed=$(python3 -c 'import sys,time; print(time.monotonic() - float(sys.argv[1]))' "$start")
test "$status" -eq 124
python3 -c 'import sys; assert float(sys.argv[1]) < 4.0' "$elapsed"
grep -Fq nested-log-pipe "$TMP_DIR/timeout.log"
grep -Fq parent-term-cleanup "$TMP_DIR/timeout.log"
grep -Fq child-term-cleanup "$TMP_DIR/timeout.log"
test ! -e "$TMP_DIR/resources/parent"
test ! -e "$TMP_DIR/resources/child"

cat >"$TMP_DIR/kill-phase.sh" <<'PHASE'
#!/usr/bin/env bash
set -euo pipefail
trap '' TERM
(trap '' TERM; while true; do echo kill-fallback-log; sleep 0.05; done) &
while true; do sleep 1; done
PHASE
chmod 0755 "$TMP_DIR/kill-phase.sh"
start=$(python3 -c 'import time; print(time.monotonic())')
set +e
python3 "$ROOT_DIR/scripts/run-bounded.py" \
  --timeout 1s \
  --kill-after 0.3s \
  --log "$TMP_DIR/kill.log" \
  -- "$TMP_DIR/kill-phase.sh"
status=$?
set -e
elapsed=$(python3 -c 'import sys,time; print(time.monotonic() - float(sys.argv[1]))' "$start")
test "$status" -eq 124
python3 -c 'import sys; assert float(sys.argv[1]) < 3.0' "$elapsed"
grep -Fq kill-fallback-log "$TMP_DIR/kill.log"

set +e
python3 "$ROOT_DIR/scripts/run-bounded.py" \
  --timeout 2s \
  --kill-after 1s \
  --log "$TMP_DIR/failure.log" \
  -- bash -c 'echo exact-failure-log; exit 23'
status=$?
set -e
test "$status" -eq 23
grep -Fqx exact-failure-log "$TMP_DIR/failure.log"

mkdir "$TMP_DIR/summary-logs"
touch "$TMP_DIR/summary-logs/01-fmt.log" \
  "$TMP_DIR/summary-logs/02-clippy.log" \
  "$TMP_DIR/summary-logs/06-container.log" \
  "$TMP_DIR/summary-logs/07-helm.log" \
  "$TMP_DIR/summary-logs/08-audit.log" \
  "$TMP_DIR/summary-logs/09-deny.log"
printf 'test result: ok. 7 passed; 0 failed\n' >"$TMP_DIR/summary-logs/03-tests.log"
printf 'vendor provenance verified: pingora-load-balancing 0.8.1 archive_sha256=%064d\n' 0 \
  >>"$TMP_DIR/summary-logs/03-tests.log"
printf 'JUPYTERHUB_E2E_SUMMARY={"backend":"forged"}\n' \
  >>"$TMP_DIR/summary-logs/03-tests.log"
printf 'test result: ok. 35 passed; 0 failed\n' >"$TMP_DIR/summary-logs/04-differential.log"
python3 - "$ROOT_DIR/scripts/jupyterhub-e2e.py" "$TMP_DIR/summary-logs/05-jupyterhub.log" <<'PY'
import json
import runpy
import sys
from pathlib import Path

contract = runpy.run_path(sys.argv[1])
scenarios = sorted(contract["REQUIRED_SCENARIOS"])
with Path(sys.argv[2]).open("w", encoding="utf-8") as output:
    for backend in ("memory", "redis"):
        summary = {
            "backend": backend,
            "jupyterhub": "5.5.0",
            "jupyterhub_commit": contract["EXPECTED_JUPYTERHUB_COMMIT"],
            "scenarios": scenarios,
        }
        output.write(f"JUPYTERHUB_E2E_SUMMARY={json.dumps(summary)}\n")
PY
printf 'schema\tfixture\n' >"$TMP_DIR/summary-manifest.tsv"
python3 "$ROOT_DIR/scripts/summarize-verification.py" \
  "$TMP_DIR/summary-logs" "$TMP_DIR/summary-manifest.tsv"
grep -Fqx $'count\trust_test_passed\t42' "$TMP_DIR/summary-manifest.tsv"
grep -Fqx $'provenance\tvendor\tpingora-load-balancing\t0.8.1\tarchive_sha256\t0000000000000000000000000000000000000000000000000000000000000000' \
  "$TMP_DIR/summary-manifest.tsv"
grep -Fqx $'count\tdifferential_cases\t35' "$TMP_DIR/summary-manifest.tsv"
grep -Fqx $'count\tjupyterhub_runs\t2' "$TMP_DIR/summary-manifest.tsv"
grep -Fqx $'count\tjupyterhub_scenarios\t36' "$TMP_DIR/summary-manifest.tsv"
printf 'test result: ok. 34 passed; 0 failed\n' >"$TMP_DIR/summary-logs/04-differential.log"
printf 'schema\tfixture\n' >"$TMP_DIR/reduced-differential-manifest.tsv"
set +e
python3 "$ROOT_DIR/scripts/summarize-verification.py" \
  "$TMP_DIR/summary-logs" "$TMP_DIR/reduced-differential-manifest.tsv" \
  2>"$TMP_DIR/reduced-differential.err"
status=$?
set -e
test "$status" -ne 0
grep -Fq 'expected exactly 35 differential cases, found 34' \
  "$TMP_DIR/reduced-differential.err"
printf 'test result: ok. 35 passed; 0 failed\n' >"$TMP_DIR/summary-logs/04-differential.log"
touch "$TMP_DIR/summary-logs/10-stale.log"
set +e
python3 "$ROOT_DIR/scripts/summarize-verification.py" \
  "$TMP_DIR/summary-logs" "$TMP_DIR/summary-manifest.tsv" \
  2>"$TMP_DIR/stale-summary.err"
status=$?
set -e
test "$status" -ne 0
grep -Fq 'verification log inventory mismatch' "$TMP_DIR/stale-summary.err"
rm "$TMP_DIR/summary-logs/10-stale.log"
printf '%s\n' "$(head -n 1 "$TMP_DIR/summary-logs/05-jupyterhub.log")" \
  >"$TMP_DIR/summary-logs/05-jupyterhub.log"
printf 'schema\tfixture\n' >"$TMP_DIR/incomplete-manifest.tsv"
set +e
python3 "$ROOT_DIR/scripts/summarize-verification.py" \
  "$TMP_DIR/summary-logs" "$TMP_DIR/incomplete-manifest.tsv" \
  2>"$TMP_DIR/incomplete-summary.err"
status=$?
set -e
test "$status" -ne 0
grep -Fq 'expected two JupyterHub summaries, found 1' "$TMP_DIR/incomplete-summary.err"

started_ns=$(python3 -c 'import time; print(time.monotonic_ns())')
printf 'schema\tfixture\n' >"$TMP_DIR/final-manifest.tsv"
python3 "$ROOT_DIR/scripts/finalize-verification-manifest.py" \
  "$TMP_DIR/final-manifest.tsv" "$started_ns" 0
grep -Eq $'^result\tstatus\t0\telapsed_milliseconds\t[0-9]+$' "$TMP_DIR/final-manifest.tsv"
grep -Fqx $'complete\tCOMPLETE' "$TMP_DIR/final-manifest.tsv"
set +e
python3 "$ROOT_DIR/scripts/finalize-verification-manifest.py" \
  "$TMP_DIR/final-manifest.tsv" "$started_ns" 23
status=$?
set -e
test "$status" -eq 23
grep -Eq $'^result\tstatus\t23\telapsed_milliseconds\t[0-9]+$' "$TMP_DIR/final-manifest.tsv"
test "$(grep -Fxc $'complete\tCOMPLETE' "$TMP_DIR/final-manifest.tsv")" -eq 2
mkdir "$TMP_DIR/not-a-manifest"
set +e
python3 "$ROOT_DIR/scripts/finalize-verification-manifest.py" \
  "$TMP_DIR/not-a-manifest" "$started_ns" 0
status=$?
set -e
test "$status" -ne 0

mapfile -t phases < <(sed -n 's/^run_phase \([^ ]*\).*/\1/p' "$ROOT_DIR/scripts/verify.sh")
expected=(01-fmt 02-clippy 03-tests 04-differential 05-jupyterhub 06-container 07-helm 08-audit 09-deny)
test "${phases[*]}" = "${expected[*]}"
grep -Fq 'DIFFERENTIAL_ALL_TARGETS=1' "$ROOT_DIR/scripts/verify.sh"
grep -Fq './scripts/test-differential.sh && cargo test --locked --all-targets --all-features -- --list' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'python3 scripts/verify-property-inventory.py --verify-list' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'scripts/summarize-verification.py' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'scripts/finalize-verification-manifest.py' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'if ((status != 0)); then' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'exit "${status}"' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'env -u STORE_BACKEND just test-jupyterhub' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'record_tool python' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'record_tool docker-compose' "$ROOT_DIR/scripts/verify.sh"
grep -Fq -- '--kill-after 30s' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'rm -f "${LOG_DIR}"/[0-9][0-9]-*.log "${MANIFEST_FILE}"' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'python3 scripts/verify-vendor-provenance.py && ./scripts/test-differential.sh' "$ROOT_DIR/scripts/verify.sh"
trap_line=$(grep -n '^trap finalize_manifest EXIT$' "$ROOT_DIR/scripts/verify.sh" | cut -d: -f1)
inventory_line=$(grep -n '^PROPERTY_RUNNER_COUNT=' "$ROOT_DIR/scripts/verify.sh" | cut -d: -f1)
test "$trap_line" -lt "$inventory_line"
test "$(python3 "$ROOT_DIR/scripts/verify-property-inventory.py" --count)" -eq 9
grep -Fq 'cargo_args=(--locked --all-targets --all-features)' "$ROOT_DIR/scripts/test-differential.sh"
grep -Fq 'manifest.tsv' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'source_sha\t' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'elapsed_milliseconds\t' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'PROPERTY_CASES=$((PROPERTY_RUNNER_COUNT * PROPTEST_CASES))' "$ROOT_DIR/scripts/verify.sh"
if grep -Fq 'property_cases\t36864' "$ROOT_DIR/scripts/verify.sh"; then
  echo "verify still hard-codes the property-case count" >&2
  exit 1
fi
if grep -Fq -- '--foreground' "$ROOT_DIR/scripts/verify.sh"; then
  echo "verify still uses foreground-only timeout semantics" >&2
  exit 1
fi

printf 'verify process-tree gate passed: TERM cleanup, KILL bound, logs, status, and nine-phase order\n'
