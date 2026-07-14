#!/usr/bin/env bash
set -euo pipefail

if (( $# != 1 )); then
  echo "usage: build-container-context.sh OUTPUT.tar" >&2
  exit 2
fi

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUTPUT=$1
LIST=$(mktemp "${TMPDIR:-/tmp}/pingora-context-list.XXXXXX")
trap 'rm -f "$LIST"' EXIT INT TERM
cd "$ROOT_DIR"

git ls-files -z -- \
  Dockerfile .dockerignore Cargo.toml Cargo.lock rust-toolchain.toml src vendor \
  >"$LIST"

for required in Dockerfile .dockerignore Cargo.toml Cargo.lock rust-toolchain.toml; do
  if ! tr '\0' '\n' <"$LIST" | grep -Fqx "$required"; then
    echo "required tracked build input is missing: $required" >&2
    exit 1
  fi
done

while IFS= read -r -d '' path; do
  if [[ -L "$path" ]]; then
    echo "symbolic links are forbidden in the container build context: $path" >&2
    exit 1
  fi
done <"$LIST"

mkdir -p "$(dirname "$OUTPUT")"
tar -cf "$OUTPUT" --null -T "$LIST"
