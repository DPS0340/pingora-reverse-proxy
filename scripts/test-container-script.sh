#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pingora-container-script-test.XXXXXX")
SENTINEL="$ROOT_DIR/src/task13-untracked-secret-sentinel-$$.pem"
TRACKED_INPUT="$ROOT_DIR/src/lib.rs"
cp "$TRACKED_INPUT" "$TMP_DIR/tracked-input.original"
cleanup() {
  cp "$TMP_DIR/tracked-input.original" "$TRACKED_INPUT" 2>/dev/null || true
  rm -f "$SENTINEL"
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM
install -m 0600 /dev/null "$SENTINEL"

"$ROOT_DIR/scripts/build-container-context.sh" "$TMP_DIR/context.tar"
if tar -tf "$TMP_DIR/context.tar" | grep -Fq "${SENTINEL#"$ROOT_DIR/"}"; then
  echo "untracked secret sentinel entered the container context" >&2
  exit 1
fi
tar -tf "$TMP_DIR/context.tar" >"$TMP_DIR/context.list"
for required in Dockerfile .dockerignore Cargo.toml Cargo.lock rust-toolchain.toml; do
  grep -Fqx "$required" "$TMP_DIR/context.list"
done
if grep -Ev '^(Dockerfile|\.dockerignore|Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|src/|vendor/)' "$TMP_DIR/context.list" | grep -q .; then
  echo "non-build input entered the container context" >&2
  exit 1
fi
grep -Fqx '**' "$ROOT_DIR/.dockerignore"

printf '\n// dirty workspace content must never enter a production image\n' >>"$TRACKED_INPUT"
"$ROOT_DIR/scripts/build-container-context.sh" "$TMP_DIR/dirty-context.tar"
if tar -xOf "$TMP_DIR/dirty-context.tar" src/lib.rs | grep -Fq \
  'dirty workspace content must never enter a production image'; then
  echo "dirty tracked content entered the immutable HEAD container context" >&2
  exit 1
fi
git -C "$ROOT_DIR" show HEAD:src/lib.rs >"$TMP_DIR/head-lib.rs"
tar -xOf "$TMP_DIR/dirty-context.tar" src/lib.rs >"$TMP_DIR/archive-lib.rs"
cmp "$TMP_DIR/head-lib.rs" "$TMP_DIR/archive-lib.rs"
cp "$TMP_DIR/tracked-input.original" "$TRACKED_INPUT"

mkdir "$TMP_DIR/invalid-injection-tmp"
set +e
TMPDIR="$TMP_DIR/invalid-injection-tmp" \
  CONTAINER_GATE_INJECT_FAILURE=unsupported \
  "$ROOT_DIR/scripts/test-container.sh" >"$TMP_DIR/invalid-injection.log" 2>&1
invalid_status=$?
set -e
test "$invalid_status" -eq 2
test -z "$(find "$TMP_DIR/invalid-injection-tmp" -mindepth 1 -print -quit)"
grep -Fqx 'unsupported CONTAINER_GATE_INJECT_FAILURE: unsupported' "$TMP_DIR/invalid-injection.log"

mkdir "$TMP_DIR/bin"
cat >"$TMP_DIR/bin/docker" <<'DOCKER'
#!/usr/bin/env bash
set -euo pipefail
echo "$*" >>"$FAKE_DOCKER_LOG"
command=${1:-}
shift || true
case "$command ${1:-}" in
  "ps -aq")
    [[ ${FAKE_DOCKER_FAILURE:-} == container-scan ]] && exit 41
    if [[ -e "$FAKE_DOCKER_STATE/container" ]]; then echo owned-container-id; fi
    ;;
  "rm -f")
    [[ ${FAKE_DOCKER_FAILURE:-} == container-rm ]] && exit 42
    rm -f "$FAKE_DOCKER_STATE/container"
    ;;
  "image ls")
    [[ ${FAKE_DOCKER_FAILURE:-} == image-scan ]] && exit 43
    if [[ -e "$FAKE_DOCKER_STATE/image" ]]; then echo owned-image-id; fi
    ;;
  "image rm")
    [[ ${FAKE_DOCKER_FAILURE:-} == image-rm ]] && exit 44
    rm -f "$FAKE_DOCKER_STATE/image"
    ;;
  *) exit 45 ;;
esac
DOCKER
chmod 0755 "$TMP_DIR/bin/docker"

run_cleanup_case() {
  local failure=${1:-}
  local expected=$2
  local state="$TMP_DIR/state-${failure:-success}"
  local log="$TMP_DIR/docker-${failure:-success}.log"
  mkdir "$state"
  touch "$state/container" "$state/image"
  set +e
  PATH="$TMP_DIR/bin:$PATH" \
    FAKE_DOCKER_FAILURE="$failure" \
    FAKE_DOCKER_LOG="$log" \
    FAKE_DOCKER_STATE="$state" \
    "$ROOT_DIR/scripts/cleanup-container-resources.sh" \
      timeout \
      io.pingora-reverse-proxy.test-owner=exact-owner \
      exact-image:tag \
      exact-main exact-uid exact-gid exact-tmp
  local status=$?
  set -e
  test "$status" -eq "$expected"
  grep -Fq 'ps -aq --filter label=io.pingora-reverse-proxy.test-owner=exact-owner' "$log"
  if [[ -z "$failure" ]]; then
    grep -Fq 'rm -f owned-container-id' "$log"
    grep -Fq 'image rm -f exact-image:tag' "$log"
    test ! -e "$state/container"
    test ! -e "$state/image"
  fi
}

run_cleanup_case "" 0
for failure in container-scan image-scan container-rm image-rm; do
  run_cleanup_case "$failure" 1
done

printf 'container script gate passed: allowlisted context sentinel and fail-closed exact cleanup\n'
