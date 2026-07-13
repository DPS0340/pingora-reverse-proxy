# Sidecar store protocol v1

The sidecar store is a persistence boundary for the complete CHP route map. A
client starts only after `GET /v1/health` completes the v1 handshake. The
in-process server in `tests/support/sidecar.rs` is the executable conformance
reference.

## Transport and versioning

The configured base URL must be an `http` or `https` origin with the root path
`/` and no user information, query, or fragment. Redirects are disabled. Use
HTTPS with a certificate trusted by the platform WebPKI roots whenever the
sidecar is not reached over a protected local transport. Plain HTTP is intended
for loopback or an otherwise protected service network.

Every request and every response, including errors, carries:

```http
X-Store-Protocol: v1
```

A missing or different response header is a protocol failure. JSON requests
and successful JSON responses use `Content-Type: application/json`. Response
bodies are limited to 4 MiB.

An optional configured credential is sent on every request as:

```http
Authorization: Bearer opaque-token
```

The token and base URL are redacted from `Debug`; tokens, response bodies,
route targets, and URLs are never copied into errors or logs. The sidecar
returns `401` when authentication fails.

## Route values and keys

A route value contains the normalized fields plus arbitrary CHP metadata:

```json
{
  "target": "http://127.0.0.1:9000",
  "last_activity": "2026-07-13T06:39:07.991726000Z",
  "owner": "jupyterhub",
  "arbitrary": {"nested": [1, true, null]}
}
```

`target` and `last_activity` are required. Every other member is unknown
metadata and must round-trip unchanged. Timestamps are UTC RFC 3339 strings
with the CHP field name and fixed nanosecond precision; this preserves the full
Store timestamp while remaining valid CHP JSON.

For single-route endpoints, the normalized route key is encoded as exactly one
RFC 3986 path segment. Only `A-Z a-z 0-9 - . _ ~` remain literal; every other
UTF-8 byte is uppercase `%HH`. For example, `//user/alice//` becomes
`%2F%2Fuser%2Falice%2F%2F`. A slash from a route key can never become a path
separator.

## Endpoints

### Health and startup readiness

```http
GET /v1/health

HTTP/1.1 200 OK
X-Store-Protocol: v1
Content-Type: application/json

{"version":"v1","status":"ok"}
```

The object is exact: extra fields, a different version, a different status, or
malformed JSON fail the handshake without retry. `SidecarStore::connect` uses
the same finite connect/request deadlines and bounded GET retry policy described
below. The process must not publish readiness until `connect` succeeds.

### Complete snapshot

```http
GET /v1/routes

HTTP/1.1 200 OK
X-Store-Protocol: v1
Content-Type: application/json

{
  "/user/alice": {
    "target": "http://127.0.0.1:9000",
    "last_activity": "2026-07-13T06:39:07.991726000Z",
    "owner": "jupyterhub"
  }
}
```

The response is one JSON object from normalized route keys to complete route
values and represents one consistent snapshot. Invalid keys, duplicate keys
after normalization, malformed values, or an oversized body are corrupt data;
no partial snapshot may be published.

### Atomic route PUT

```http
PUT /v1/routes/%2Fuser%2Falice
X-Store-Protocol: v1
Content-Type: application/json

{
  "version": "v1",
  "operation": "put_preserving_activity",
  "route": {
    "target": "http://127.0.0.1:9001",
    "last_activity": "2026-07-13T06:39:07.991726000Z",
    "owner": "jupyterhub"
  },
  "activityFloor": "2026-07-13T06:40:00.000000000Z"
}

HTTP/1.1 204 No Content
X-Store-Protocol: v1
```

The versioned envelope has four members:

- `version`: exactly `v1`.
- `operation`: exactly `add`, `put`, or `put_preserving_activity`.
- `route`: one complete route value.
- `activityFloor`: a timestamp or `null`.

For `put`, the sidecar atomically stores `route` exactly and ignores a null
floor. For `add` and `put_preserving_activity`, it atomically stores the same
route except that `last_activity` is the maximum of `route.last_activity` and
`activityFloor`. The client supplies the complete final candidate and returns
that same record after the acknowledged 204. Repeating an identical envelope
is idempotent.

Validation and the activity-floor maximum happen before the single atomic map
replacement. A 204 means the complete record committed. No other success
status is valid.

### Activity PATCH

```http
PATCH /v1/routes/%2Fuser%2Falice/activity
X-Store-Protocol: v1
Content-Type: application/json

{
  "version": "v1",
  "lastActivity": "2026-07-13T06:45:00.000000123Z"
}

HTTP/1.1 204 No Content
X-Store-Protocol: v1
```

The sidecar atomically changes only `last_activity` when the route exists. A
missing route is a successful no-op. Repeating the exact PATCH is idempotent.

### Atomic DELETE with prior value

```http
DELETE /v1/routes/%2Fuser%2Falice

HTTP/1.1 200 OK
X-Store-Protocol: v1
Content-Type: application/json

{
  "target": "http://127.0.0.1:9001",
  "last_activity": "2026-07-13T06:45:00.000000123Z",
  "owner": "jupyterhub"
}
```

The lookup, removal, and capture of the prior value are one atomic operation.
An existing route returns its exact prior record with 200. An absent route
returns 404 with an empty body. A malformed 200 is corrupt data and must not be
published by the registry.

## Deadlines and retries

Defaults are a one-second connect timeout, a two-second total timeout for each
request attempt, three attempts, and exponential delays of 25 ms then 50 ms,
capped at 200 ms. `SidecarConfig` can set both deadlines and a retry policy;
attempts are clamped to the finite range 1 through 10. Every attempt carries
the same protocol and authentication headers.

GET health/snapshot requests retry transport loss and 5xx. They never retry a
4xx, redirect, malformed body, invalid content type, or version/protocol
mismatch.

Mutation retries have a stronger server prerequisite:

> A v1 sidecar MUST return 5xx only when the mutation did not commit.

This pre-commit guarantee permits bounded 5xx retry. PUT and PATCH retries reuse
the byte-for-byte identical serialized payload, so their acknowledged result is
idempotent. DELETE may retry a returned 5xx because the guarantee proves the
delete did not commit. A transport error or body/reply loss on any mutation is
never retried, because reqwest cannot prove whether dispatch reached the commit
point.

## Consistency, indeterminate outcomes, and recovery

The sidecar must linearize each mutation and return a snapshot from one
consistent point. Independent writers must implement the same atomic rules.
The protocol has no ownership inference, operation marker, or corrective GET.

Any mutation transport/reply loss is `StoreError::Indeterminate`, even if a
later GET appears to show the requested value or absence. `RouteRegistry`
terminally seals mutation admission and all cached serving on that error;
readiness and management/data-plane reads fail closed. Recovery requires an
authoritative `GET /v1/routes` into a new registry/process after the sidecar is
reachable. The poisoned registry is never unsealed in place.

## Error mapping

| Wire or validation result | Store result | Retry |
| --- | --- | --- |
| Expected 200/204/404 | Success described above | No |
| 401 or any other 4xx | `StoreError::Backend` | No |
| 5xx exhausted | `StoreError::Backend` (v1 proves no commit) | Bounded |
| GET transport/body loss | `StoreError::Backend` after exhaustion | Bounded |
| Mutation transport/body/reply loss | `StoreError::Indeterminate` | Never |
| Malformed snapshot or DELETE 200 | `StoreError::CorruptData` | No |
| Protocol/header/version/status mismatch | `StoreError::Backend` | No |

All mappings use fixed operation labels and exclude credentials, URLs, response
bodies, and route targets.
