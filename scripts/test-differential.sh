#!/usr/bin/env bash
set -euo pipefail

if docker compose version >/dev/null 2>&1; then
    compose=(docker compose)
elif docker-compose version >/dev/null 2>&1; then
    compose=(docker-compose)
else
    echo "Docker Compose is required for the differential gate" >&2
    exit 127
fi

project="chp-diff-$$-${RANDOM}"
export COMPOSE_PROJECT_NAME="$project"
export CHP_ORACLE_IMAGE="pingora-chp-oracle:${project}"
active_pid=""

cleanup() {
    status=$?
    trap - EXIT INT TERM
    set +e
    if [[ -n "$active_pid" ]]; then
        kill -TERM "$active_pid" 2>/dev/null
        wait "$active_pid" 2>/dev/null
    fi
    "${compose[@]}" -f compose.test.yml down --remove-orphans --volumes
    while IFS= read -r container; do
        if [[ -n "$container" ]]; then
            docker rm -f "$container"
        fi
    done < <(docker ps -aq --filter "ancestor=$CHP_ORACLE_IMAGE")
    docker image rm "$CHP_ORACLE_IMAGE"
    exit "$status"
}

on_signal() {
    exit "$1"
}

trap cleanup EXIT
trap 'on_signal 130' INT
trap 'on_signal 143' TERM

"${compose[@]}" -f compose.test.yml build chp
"${compose[@]}" -f compose.test.yml up -d redis sidecar

if [[ "${DIFFERENTIAL_SKIP_RUNTIME_PROBE:-0}" != 1 ]]; then
    probe="$(docker run --rm "$CHP_ORACLE_IMAGE" node /usr/local/bin/chp-oracle.mjs --runtime-probe)"
    [[ "$probe" == *'"node":"v20.'* ]]
    [[ "$probe" == *'"package":"configurable-http-proxy@5.3.0"'* ]]
    [[ "$probe" == *'"source":"/opt/chp-5.3.0"'* ]]
fi

redis_endpoint="$("${compose[@]}" -f compose.test.yml port redis 6379)"
redis_port="${redis_endpoint##*:}"
TEST_REDIS_URL="redis://127.0.0.1:${redis_port}" \
    CHP_ORACLE_IMAGE="$CHP_ORACLE_IMAGE" \
    cargo test --test differential -- --nocapture &
active_pid=$!
wait "$active_pid"
active_pid=""
