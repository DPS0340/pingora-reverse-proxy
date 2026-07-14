#!/usr/bin/env bash
set -euo pipefail

TIMEOUT_BIN=${TIMEOUT_BIN:-timeout}
CHART_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

command -v "$TIMEOUT_BIN" >/dev/null 2>&1
command -v helm >/dev/null 2>&1

"$TIMEOUT_BIN" 300 helm upgrade --install \
  --create-namespace \
  --namespace pingora-reverse-proxy \
  --wait \
  --timeout 4m \
  pingora-reverse-proxy \
  "$CHART_DIR"
