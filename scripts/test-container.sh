#!/usr/bin/env bash
set -euo pipefail

TIMEOUT_BIN=${TIMEOUT_BIN:-timeout}
BUILD_TIMEOUT=${CONTAINER_BUILD_TIMEOUT_SECONDS:-1800}
RUN_ID="task13-container-$(date +%s)-$$"
IMAGE="pingora-reverse-proxy:${RUN_ID}"
CONTAINER="${RUN_ID}"
UID_CONTAINER="${RUN_ID}-uid"
GID_CONTAINER="${RUN_ID}-gid"
TMP_CONTAINER="${RUN_ID}-tmp"
OWNER_LABEL="io.pingora-reverse-proxy.test-owner=${RUN_ID}"
TOKEN="task13-container-token-${RUN_ID}"
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pingora-container-test.XXXXXX")
INJECT_FAILURE=${CONTAINER_GATE_INJECT_FAILURE:-}

if [[ -n "$INJECT_FAILURE" && "$INJECT_FAILURE" != "after-start" ]]; then
  printf 'unsupported CONTAINER_GATE_INJECT_FAILURE: %s\n' "$INJECT_FAILURE" >&2
  exit 2
fi

require_command() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'required command is unavailable: %s\n' "$1" >&2
    exit 1
  }
}

cleanup() {
  local primary_status=$?
  local cleanup_status=0
  trap - EXIT INT TERM

  "$TIMEOUT_BIN" 30 docker rm -f \
    "$CONTAINER" "$UID_CONTAINER" "$GID_CONTAINER" "$TMP_CONTAINER" \
    >/dev/null 2>&1 || true
  if "$TIMEOUT_BIN" 30 docker ps -aq --filter "label=${OWNER_LABEL}" | grep -q .; then
    printf 'owned containers remained after cleanup: %s\n' "$RUN_ID" >&2
    cleanup_status=1
  fi
  "$TIMEOUT_BIN" 60 docker image rm -f "$IMAGE" >/dev/null 2>&1 || true
  if "$TIMEOUT_BIN" 30 docker image inspect "$IMAGE" >/dev/null 2>&1; then
    printf 'owned image remained after cleanup: %s\n' "$IMAGE" >&2
    cleanup_status=1
  fi
  rm -rf "$TMP_DIR"

  if (( primary_status != 0 || cleanup_status != 0 )); then
    exit 1
  fi
}
trap cleanup EXIT INT TERM

require_command docker
require_command curl
require_command tar
require_command "$TIMEOUT_BIN"

tar --exclude .git --exclude target -cf - . | \
  "$TIMEOUT_BIN" "$BUILD_TIMEOUT" docker build \
    --pull \
    --label "$OWNER_LABEL" \
    --tag "$IMAGE" \
    -

test "$("$TIMEOUT_BIN" 30 docker image inspect --format '{{.Config.User}}' "$IMAGE")" = "65532:65532"
test "$("$TIMEOUT_BIN" 30 docker image inspect --format '{{json .Config.Entrypoint}}' "$IMAGE")" = '["/usr/local/bin/pingora-reverse-proxy"]'
test "$("$TIMEOUT_BIN" 30 docker image inspect --format '{{json .Config.Cmd}}' "$IMAGE")" = '["--ip","0.0.0.0","--port","8000","--api-ip","0.0.0.0","--api-port","8001","--metrics-ip","0.0.0.0","--metrics-port","8002"]'

effective_uid=$("$TIMEOUT_BIN" 30 docker run --rm \
  --name "$UID_CONTAINER" \
  --label "$OWNER_LABEL" \
  --entrypoint /usr/bin/id \
  "$IMAGE" -u)
test "$effective_uid" = "65532"

effective_gid=$("$TIMEOUT_BIN" 30 docker run --rm \
  --name "$GID_CONTAINER" \
  --label "$OWNER_LABEL" \
  --entrypoint /usr/bin/id \
  "$IMAGE" -g)
test "$effective_gid" = "65532"

"$TIMEOUT_BIN" 30 docker run --rm \
  --name "$TMP_CONTAINER" \
  --label "$OWNER_LABEL" \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,uid=65532,gid=65532,mode=1770 \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --entrypoint /usr/bin/touch \
  "$IMAGE" /tmp/write-probe

"$TIMEOUT_BIN" 30 docker run --detach \
  --name "$CONTAINER" \
  --label "$OWNER_LABEL" \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,uid=65532,gid=65532,mode=1770 \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --env "CONFIGPROXY_AUTH_TOKEN=${TOKEN}" \
  --publish 127.0.0.1::8000 \
  --publish 127.0.0.1::8001 \
  --publish 127.0.0.1::8002 \
  "$IMAGE" >/dev/null

if [[ "$INJECT_FAILURE" == "after-start" ]]; then
  printf 'injected container-gate failure after owned container start\n' >&2
  exit 97
fi

public_port=$("$TIMEOUT_BIN" 30 docker port "$CONTAINER" 8000/tcp | sed -n '1s/.*://p')
api_port=$("$TIMEOUT_BIN" 30 docker port "$CONTAINER" 8001/tcp | sed -n '1s/.*://p')
metrics_port=$("$TIMEOUT_BIN" 30 docker port "$CONTAINER" 8002/tcp | sed -n '1s/.*://p')
test -n "$public_port"
test -n "$api_port"
test -n "$metrics_port"

deadline=$((SECONDS + 60))
while true; do
  if ! "$TIMEOUT_BIN" 15 docker inspect --format '{{.State.Running}}' "$CONTAINER" 2>/dev/null | grep -qx true; then
    "$TIMEOUT_BIN" 15 docker logs "$CONTAINER" >&2 || true
    printf 'production container exited before readiness\n' >&2
    exit 1
  fi
  health=$(curl --silent --show-error --max-time 2 "http://127.0.0.1:${public_port}/_chp_healthz" 2>/dev/null || true)
  routes=$(curl --silent --show-error --max-time 2 \
    --header "Authorization: token ${TOKEN}" \
    "http://127.0.0.1:${api_port}/api/routes" 2>/dev/null || true)
  metrics=$(curl --silent --show-error --max-time 2 "http://127.0.0.1:${metrics_port}/metrics" 2>/dev/null || true)
  if [[ "$health" == '{"status":"OK"}' && "$routes" == '{}' && "$metrics" == *'api_route_get'* ]]; then
    break
  fi
  if (( SECONDS >= deadline )); then
    "$TIMEOUT_BIN" 15 docker logs "$CONTAINER" >&2 || true
    printf 'container listeners did not become ready within 60 seconds\n' >&2
    exit 1
  fi
  sleep 1
done

test "$("$TIMEOUT_BIN" 30 docker inspect --format '{{.HostConfig.ReadonlyRootfs}}' "$CONTAINER")" = "true"
test "$("$TIMEOUT_BIN" 30 docker inspect --format '{{.HostConfig.SecurityOpt}}' "$CONTAINER")" = '[no-new-privileges]'

"$TIMEOUT_BIN" 60 docker export "$CONTAINER" | tar -tf - >"$TMP_DIR/rootfs.txt"
grep -qx 'usr/local/bin/pingora-reverse-proxy' "$TMP_DIR/rootfs.txt"
grep -qx 'etc/ssl/certs/ca-certificates.crt' "$TMP_DIR/rootfs.txt"
if grep -Eq '^(bin|usr/bin)/(sh|dash|bash)$' "$TMP_DIR/rootfs.txt"; then
  printf 'a command shell leaked into the runtime image\n' >&2
  grep -E '^(bin|usr/bin)/(sh|dash|bash)$' "$TMP_DIR/rootfs.txt" >&2
  exit 1
fi
if grep -Eq '(^|/)(Cargo\.(toml|lock)|\.git|target|src)(/|$)|(^|/)(cargo|rustc|gcc|g\+\+|cmake|make|git|bash)$' "$TMP_DIR/rootfs.txt"; then
  printf 'build tools, source, or a debug tree leaked into the runtime image\n' >&2
  grep -E '(^|/)(Cargo\.(toml|lock)|\.git|target|src)(/|$)|(^|/)(cargo|rustc|gcc|g\+\+|cmake|make|git|bash)$' "$TMP_DIR/rootfs.txt" >&2
  exit 1
fi

"$TIMEOUT_BIN" 30 docker stop --time 15 "$CONTAINER" >/dev/null
test "$("$TIMEOUT_BIN" 30 docker inspect --format '{{.State.ExitCode}}' "$CONTAINER")" = "0"

printf 'container gate passed: uid/gid=65532 read-only-root health/api/metrics ready cleanup-owned=%s\n' "$RUN_ID"
