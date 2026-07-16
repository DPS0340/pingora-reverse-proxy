#!/usr/bin/env bash
set -u

if (( $# < 4 )); then
  echo "usage: cleanup-container-resources.sh TIMEOUT LABEL IMAGE CONTAINER..." >&2
  exit 2
fi

TIMEOUT_BIN=$1
OWNER_LABEL=$2
IMAGE=$3
shift 3
CONTAINER_NAMES=("$@")
cleanup_status=0

scan_containers() {
  "$TIMEOUT_BIN" 30 docker ps -aq --filter "label=${OWNER_LABEL}"
}

scan_images() {
  "$TIMEOUT_BIN" 30 docker image ls -q --filter "label=${OWNER_LABEL}"
}

if ! container_ids=$(scan_containers); then
  echo "Docker container cleanup scan failed for ${OWNER_LABEL}" >&2
  cleanup_status=1
elif [[ -n "$container_ids" ]]; then
  mapfile -t container_id_list <<<"$container_ids"
  if ! "$TIMEOUT_BIN" 30 docker rm -f "${container_id_list[@]}" >/dev/null; then
    printf 'failed to remove owned containers (%s): %s\n' "${CONTAINER_NAMES[*]}" "$container_ids" >&2
    cleanup_status=1
  elif ! remaining_containers=$(scan_containers); then
    echo "Docker container cleanup verification scan failed for ${OWNER_LABEL}" >&2
    cleanup_status=1
  elif [[ -n "$remaining_containers" ]]; then
    printf 'owned containers remained after cleanup (%s): %s\n' "${CONTAINER_NAMES[*]}" "$remaining_containers" >&2
    cleanup_status=1
  fi
fi

if ! image_ids=$(scan_images); then
  echo "Docker image cleanup scan failed for ${OWNER_LABEL}" >&2
  cleanup_status=1
elif [[ -n "$image_ids" ]]; then
  if ! "$TIMEOUT_BIN" 60 docker image rm -f "$IMAGE" >/dev/null; then
    printf 'failed to remove owned image %s (%s)\n' "$IMAGE" "$image_ids" >&2
    cleanup_status=1
  elif ! remaining_images=$(scan_images); then
    echo "Docker image cleanup verification scan failed for ${OWNER_LABEL}" >&2
    cleanup_status=1
  elif [[ -n "$remaining_images" ]]; then
    printf 'owned images remained after cleanup: %s\n' "$remaining_images" >&2
    cleanup_status=1
  fi
fi

exit "$cleanup_status"
