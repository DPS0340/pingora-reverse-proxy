#!/usr/bin/env bash
set -euo pipefail

if (( $# < 1 || $# > 3 )); then
  echo "usage: build-container-context.sh OUTPUT.tar [SOURCE_ROOT [SOURCE_SHA]]" >&2
  exit 2
fi

CONTROL_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ROOT_DIR=$(cd "${2:-$CONTROL_DIR}" && pwd)
SOURCE_SHA=${3:-$(git -C "$ROOT_DIR" rev-parse HEAD)}
if [[ ! $SOURCE_SHA =~ ^[0-9a-f]{40}$ ]]; then
  echo "SOURCE_SHA must be a full lowercase Git SHA" >&2
  exit 2
fi
test "$(git -C "$ROOT_DIR" rev-parse "${SOURCE_SHA}^{commit}")" = "$SOURCE_SHA"
mkdir -p "$(dirname "$1")"
OUTPUT_DIR=$(cd "$(dirname "$1")" && pwd)
OUTPUT="$OUTPUT_DIR/$(basename "$1")"
TEMP=$(mktemp "$OUTPUT_DIR/.pingora-context.XXXXXX")
trap 'rm -f "$TEMP"' EXIT INT TERM
readonly INPUTS=(
  Dockerfile .dockerignore Cargo.toml Cargo.lock rust-toolchain.toml src vendor
)

for required in Dockerfile .dockerignore Cargo.toml Cargo.lock rust-toolchain.toml; do
  if ! git -C "$ROOT_DIR" cat-file -e "$SOURCE_SHA:$required"; then
    echo "required tracked build input is missing: $required" >&2
    exit 1
  fi
done

while IFS= read -r -d '' entry; do
  metadata=${entry%%$'\t'*}
  path=${entry#*$'\t'}
  mode=${metadata%% *}
  if [[ "$mode" != 100644 && "$mode" != 100755 ]]; then
    echo "only regular HEAD files are allowed in the container build context: $path ($mode)" >&2
    exit 1
  fi
done < <(git -C "$ROOT_DIR" ls-tree -rz "$SOURCE_SHA" -- "${INPUTS[@]}")

git -C "$ROOT_DIR" archive \
  --format=tar \
  --output="$TEMP" \
  "$SOURCE_SHA" \
  -- "${INPUTS[@]}"
mv -f "$TEMP" "$OUTPUT"
