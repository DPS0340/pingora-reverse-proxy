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
  --set probes.enabled=false \
  --set tls.public.existingSecret=task13-public-tls \
  --set tls.public.clientCAKey=ca.crt \
  --set tls.public.requestCert=true \
  --set tls.public.rejectUnauthorized=true \
  --set tls.api.existingSecret=task13-api-tls \
  --set tls.api.clientCAKey=ca.crt \
  --set tls.api.requestCert=true \
  --set tls.api.rejectUnauthorized=true \
  --set tls.client.identity.existingSecret=task13-client-tls \
  --set tls.client.ca.existingSecret=task13-upstream-ca \
  --set tls.client.ca.key=ca.crt
render_case upstream-ca \
  --set tls.client.ca.existingSecret=task13-upstream-ca \
  --set tls.client.ca.key=ca.crt
render_case digest \
  --set image.repository=registry.example/pingora-reverse-proxy \
  --set image.tag= \
  --set image.digest="sha256:$(printf 'a%.0s' {1..64})"
render_case upgrade
render_case resources \
  --set resources.requests.cpu=125m \
  --set resources.requests.memory=96Mi \
  --set resources.limits.cpu=500m \
  --set resources.limits.memory=256Mi

expect_render_failure() {
  local case_name=$1
  local expected=$2
  shift 2
  if "$TIMEOUT_BIN" 30 helm template "task13-invalid-${case_name}" "$CHART" "$@" >"$TMP_DIR/invalid-${case_name}.out" 2>&1; then
    printf 'invalid Helm case rendered successfully: %s\n' "$case_name" >&2
    exit 1
  fi
  if ! grep -Fqx "Error: execution error at (pingora-reverse-proxy/templates/deployment.yaml:1:4): ${expected}" "$TMP_DIR/invalid-${case_name}.out"; then
    printf 'invalid Helm case produced the wrong diagnostic: %s\n' "$case_name" >&2
    cat "$TMP_DIR/invalid-${case_name}.out" >&2
    exit 1
  fi
}

expect_render_failure backend "storage.backend must be one of memory, redis, or sidecar" --set storage.backend=sqlite
expect_render_failure redis-secret \
  "redis.auth.existingSecret is required for Redis storage" \
  --set storage.backend=redis \
  --set redis.auth.existingSecret=
expect_render_failure sidecar-endpoint \
  "sidecar.endpoint is required for sidecar storage" \
  --set storage.backend=sidecar \
  --set sidecar.endpoint= \
  --set sidecar.auth.existingSecret=task13-sidecar
expect_render_failure sidecar-secret \
  "sidecar.auth.existingSecret is required for sidecar storage" \
  --set storage.backend=sidecar \
  --set sidecar.endpoint=http://route-store.default.svc:8080/ \
  --set sidecar.auth.existingSecret=
expect_render_failure sidecar-url \
  "sidecar.endpoint must be a root HTTP(S) origin without credentials, query, or fragment" \
  --set storage.backend=sidecar \
  --set sidecar.endpoint=http://user@route-store.default.svc:8080/ \
  --set sidecar.auth.existingSecret=task13-sidecar
expect_render_failure sidecar-timeout \
  "sidecar connect and request timeouts must be positive" \
  --set storage.backend=sidecar \
  --set sidecar.endpoint=http://route-store.default.svc:8080/ \
  --set sidecar.auth.existingSecret=task13-sidecar \
  --set sidecar.requestTimeoutMs=0
expect_render_failure probe-client-cert \
  "probes.enabled must be false when public TLS requires a client certificate" \
  --set tls.public.existingSecret=task13-public-tls \
  --set tls.public.clientCAKey=ca.crt \
  --set tls.public.requestCert=true \
  --set tls.public.rejectUnauthorized=true

for backend in memory redis sidecar; do
  backend_args=(--set replicaCount=2 --set "storage.backend=${backend}")
  if [[ "$backend" == "redis" ]]; then
    backend_args+=(--set redis.auth.existingSecret=task13-redis)
  elif [[ "$backend" == "sidecar" ]]; then
    backend_args+=(--set sidecar.endpoint=http://route-store.default.svc:8080/ --set sidecar.auth.existingSecret=task13-sidecar)
  fi
  expect_render_failure "${backend}-replicas" "replicaCount must be exactly 1 until cross-process route propagation is implemented" "${backend_args[@]}"
done
expect_render_failure public-reject-without-request \
  "tls.public.rejectUnauthorized requires requestCert=true and a non-empty clientCAKey" \
  --set probes.enabled=false \
  --set tls.public.existingSecret=task13-public-tls \
  --set tls.public.rejectUnauthorized=true
expect_render_failure api-reject-without-ca \
  "tls.api.rejectUnauthorized requires requestCert=true and a non-empty clientCAKey" \
  --set tls.api.existingSecret=task13-api-tls \
  --set tls.api.requestCert=true \
  --set tls.api.rejectUnauthorized=true
expect_render_failure public-empty-key \
  "tls.public.certKey and tls.public.keyKey must be non-empty when TLS is enabled" \
  --set tls.public.existingSecret=task13-public-tls \
  --set tls.public.keyKey=
expect_render_failure api-empty-key \
  "tls.api.certKey and tls.api.keyKey must be non-empty when TLS is enabled" \
  --set tls.api.existingSecret=task13-api-tls \
  --set tls.api.certKey=
expect_render_failure upstream-identity-partial \
  "tls.client.identity.certKey and keyKey must be non-empty when client identity is enabled" \
  --set tls.client.identity.existingSecret=task13-client-tls \
  --set tls.client.identity.keyKey=
expect_render_failure image-tag-and-digest \
  "image.tag and image.digest are mutually exclusive" \
  --set image.digest="sha256:$(printf 'a%.0s' {1..64})"
expect_render_failure image-no-reference \
  "exactly one of image.tag or image.digest must be set" \
  --set image.tag=
expect_render_failure image-invalid-digest \
  "image.digest must match sha256 followed by 64 lowercase hexadecimal characters" \
  --set image.tag= \
  --set image.digest=sha256:1234

printf 'helm gate passed: exact single-replica/Recreate storage, TLS, digest, upgrade, and fail-closed matrix\n'
