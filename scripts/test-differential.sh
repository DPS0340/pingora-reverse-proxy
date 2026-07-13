#!/usr/bin/env bash
set -euo pipefail

project="chp-diff-$$-${RANDOM}"
export COMPOSE_PROJECT_NAME="$project"
export CHP_ORACLE_IMAGE="pingora-chp-oracle:${project}"
export CHP_ORACLE_RUN_LABEL="io.openrusty.chp-differential.run=$project"
readonly project COMPOSE_PROJECT_NAME CHP_ORACLE_IMAGE CHP_ORACLE_RUN_LABEL

if docker compose version >/dev/null 2>&1; then
    compose=(docker compose)
elif docker-compose version >/dev/null 2>&1; then
    compose=(docker-compose)
else
    echo "Docker Compose is required for the differential gate" >&2
    exit 127
fi

active_pid=""
active_pgid=""
cleanup_failed=0

cleanup_failure() {
    echo "differential cleanup failed: $1" >&2
    cleanup_failed=1
}

stop_active_group() {
    if [[ -z "$active_pid" ]]; then
        return
    fi

    if [[ -n "$active_pgid" ]]; then
        if kill -0 -- "-$active_pgid" 2>/dev/null; then
            kill -TERM -- "-$active_pgid" 2>/dev/null || cleanup_failure "failed to TERM cargo process group $active_pgid"
            attempts="${DIFFERENTIAL_TERM_GRACE_ATTEMPTS:-50}"
            interval="${DIFFERENTIAL_TERM_GRACE_INTERVAL:-0.1}"
            for ((attempt = 0; attempt < attempts; attempt++)); do
                if ! kill -0 -- "-$active_pgid" 2>/dev/null; then
                    break
                fi
                sleep "$interval"
            done
            if kill -0 -- "-$active_pgid" 2>/dev/null; then
                kill -KILL -- "-$active_pgid" 2>/dev/null || cleanup_failure "failed to KILL cargo process group $active_pgid"
            fi
        fi
    else
        kill -TERM "$active_pid" 2>/dev/null || true
        cleanup_failure "cargo process group was not recorded"
    fi

    wait "$active_pid" 2>/dev/null
    if [[ -n "$active_pgid" ]]; then
        attempts="${DIFFERENTIAL_TERM_GRACE_ATTEMPTS:-50}"
        interval="${DIFFERENTIAL_TERM_GRACE_INTERVAL:-0.1}"
        for ((attempt = 0; attempt < attempts; attempt++)); do
            if ! kill -0 -- "-$active_pgid" 2>/dev/null; then
                break
            fi
            sleep "$interval"
        done
        if kill -0 -- "-$active_pgid" 2>/dev/null; then
            cleanup_failure "cargo process group $active_pgid survived bounded teardown"
        fi
    fi
    active_pid=""
    active_pgid=""
}

cleanup() {
    original_status=$?
    trap - EXIT INT TERM
    set +e
    stop_active_group
    if ! "${compose[@]}" -f compose.test.yml down --remove-orphans --volumes; then
        cleanup_failure "compose down failed"
    fi
    containers="$(docker ps -aq --filter "label=$CHP_ORACLE_RUN_LABEL")"
    scan_status=$?
    if ((scan_status != 0)); then
        cleanup_failure "failed to scan oracle containers for $CHP_ORACLE_RUN_LABEL"
    else
        while IFS= read -r container; do
            if [[ -n "$container" ]] && ! docker rm -f "$container"; then
                cleanup_failure "failed to remove oracle container $container"
            fi
        done <<< "$containers"
    fi
    if ! docker image rm "$CHP_ORACLE_IMAGE"; then
        cleanup_failure "failed to remove oracle image $CHP_ORACLE_IMAGE"
    fi
    final_status=$original_status
    if ((final_status == 0 && cleanup_failed != 0)); then
        final_status=1
    fi
    exit "$final_status"
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
    probe="$(docker run --rm --label "$CHP_ORACLE_RUN_LABEL" "$CHP_ORACLE_IMAGE" node /usr/local/bin/chp-oracle.mjs --runtime-probe)"
    [[ "$probe" == *'"node":"v20.'* ]]
    [[ "$probe" == *'"package":"configurable-http-proxy@5.3.0"'* ]]
    [[ "$probe" == *'"source":"/opt/chp-5.3.0"'* ]]
fi

redis_endpoint="$("${compose[@]}" -f compose.test.yml port redis 6379)"
redis_port="${redis_endpoint##*:}"
set -m
TEST_REDIS_URL="redis://127.0.0.1:${redis_port}" \
    cargo test --test differential -- --nocapture &
active_pid=$!
active_pgid="$active_pid"
observed_pgid="$(ps -o pgid= -p "$active_pid")"
observed_pgid="${observed_pgid//[[:space:]]/}"
if [[ ! "$observed_pgid" =~ ^[0-9]+$ || "$observed_pgid" != "$active_pid" ]]; then
    recorded_pgid="$observed_pgid"
    echo "failed to isolate cargo process group: pid=$active_pid pgid=$recorded_pgid" >&2
    exit 1
fi
set +e
wait "$active_pid"
cargo_status=$?
set -e
if kill -0 -- "-$active_pgid" 2>/dev/null; then
    stop_active_group
else
    active_pid=""
    active_pgid=""
fi
if ((cargo_status != 0)); then
    exit "$cargo_status"
fi
if ((cleanup_failed != 0)); then
    exit 1
fi
