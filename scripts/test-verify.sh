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

mapfile -t phases < <(sed -n 's/^run_phase \([^ ]*\).*/\1/p' "$ROOT_DIR/scripts/verify.sh")
expected=(01-fmt 02-clippy 03-tests 04-differential 05-jupyterhub 06-container 07-helm 08-audit 09-deny)
test "${phases[*]}" = "${expected[*]}"
grep -Fq 'DIFFERENTIAL_ALL_TARGETS=1 ./scripts/test-differential.sh' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'cargo_args=(--locked --all-targets --all-features)' "$ROOT_DIR/scripts/test-differential.sh"
grep -Fq 'manifest.tsv' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'source_sha\t' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'elapsed_milliseconds\t' "$ROOT_DIR/scripts/verify.sh"
grep -Fq 'property_cases\t36864' "$ROOT_DIR/scripts/verify.sh"
if grep -Fq -- '--foreground' "$ROOT_DIR/scripts/verify.sh"; then
  echo "verify still uses foreground-only timeout semantics" >&2
  exit 1
fi

printf 'verify process-tree gate passed: TERM cleanup, KILL bound, logs, status, and nine-phase order\n'
