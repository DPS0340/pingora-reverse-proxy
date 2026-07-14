#!/usr/bin/env bash
set -euo pipefail

TIMEOUT_BIN=${TIMEOUT_BIN:-timeout}
CHART=${CHART:-./helm-chart}
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pingora-helm-test.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM
INJECT_FAILURE=${HELM_GATE_INJECT_FAILURE:-}

if [[ -n "$INJECT_FAILURE" && "$INJECT_FAILURE" != "after-memory" ]]; then
  printf 'unsupported HELM_GATE_INJECT_FAILURE: %s\n' "$INJECT_FAILURE" >&2
  exit 2
fi

for command in helm ruby "$TIMEOUT_BIN"; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'required command is unavailable: %s\n' "$command" >&2
    exit 1
  }
done

"$TIMEOUT_BIN" 60 helm lint "$CHART" --strict

render_case() {
  local case_name=$1
  shift
  "$TIMEOUT_BIN" 60 helm template "task13-${case_name}" "$CHART" "$@" >"$TMP_DIR/${case_name}.yaml"
  "$TIMEOUT_BIN" 30 ruby scripts/assert-helm.rb "$case_name" <"$TMP_DIR/${case_name}.yaml"
}

render_case memory
if [[ "$INJECT_FAILURE" == "after-memory" ]]; then
  printf 'injected Helm-gate failure after memory render\n' >&2
  exit 97
fi
render_case redis \
  --set storage.backend=redis \
  --set redis.auth.existingSecret=task13-redis \
  --set redis.auth.urlKey=url
render_case sidecar \
  --set storage.backend=sidecar \
  --set sidecar.endpoint=http://route-store.default.svc:8080/ \
  --set sidecar.auth.existingSecret=task13-sidecar \
  --set sidecar.auth.tokenKey=token
render_case tls \
  --set tls.public.existingSecret=task13-public-tls \
  --set tls.public.clientCAKey=ca.crt \
  --set tls.public.requestCert=true \
  --set tls.api.existingSecret=task13-api-tls \
  --set tls.client.existingSecret=task13-client-tls \
  --set tls.client.caKey=ca.crt
render_case resources \
  --set resources.requests.cpu=125m \
  --set resources.requests.memory=96Mi \
  --set resources.limits.cpu=500m \
  --set resources.limits.memory=256Mi

expect_render_failure() {
  local case_name=$1
  shift
  if "$TIMEOUT_BIN" 30 helm template "task13-invalid-${case_name}" "$CHART" "$@" >"$TMP_DIR/invalid-${case_name}.out" 2>&1; then
    printf 'invalid Helm case rendered successfully: %s\n' "$case_name" >&2
    exit 1
  fi
}

expect_render_failure backend --set storage.backend=sqlite
expect_render_failure redis-secret \
  --set storage.backend=redis \
  --set redis.auth.existingSecret=
expect_render_failure sidecar-endpoint \
  --set storage.backend=sidecar \
  --set sidecar.endpoint= \
  --set sidecar.auth.existingSecret=task13-sidecar
expect_render_failure sidecar-secret \
  --set storage.backend=sidecar \
  --set sidecar.endpoint=http://route-store.default.svc:8080/ \
  --set sidecar.auth.existingSecret=
expect_render_failure sidecar-url \
  --set storage.backend=sidecar \
  --set sidecar.endpoint=http://user@route-store.default.svc:8080/ \
  --set sidecar.auth.existingSecret=task13-sidecar
expect_render_failure sidecar-timeout \
  --set storage.backend=sidecar \
  --set sidecar.endpoint=http://route-store.default.svc:8080/ \
  --set sidecar.auth.existingSecret=task13-sidecar \
  --set sidecar.requestTimeoutMs=0
expect_render_failure probe-client-cert \
  --set tls.public.existingSecret=task13-public-tls \
  --set tls.public.clientCAKey=ca.crt \
  --set tls.public.requestCert=true \
  --set tls.public.rejectUnauthorized=true

printf 'helm gate passed: lint plus memory/redis/sidecar/tls/resources and fail-closed cases\n'
