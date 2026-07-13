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

A missing, repeated, or different response header is a protocol failure. PUT
and PATCH requests require the single exact value `Content-Type:
application/json`; parameters such as `charset` are rejected. Successful JSON
responses use that same single exact value. Valid 204 and 404 acknowledgments
have neither a content type nor a body. A nonzero or repeated `Content-Length`,
or any `Transfer-Encoding`, makes an otherwise empty acknowledgment unusable;
the client checks framing because HTTP libraries may suppress illegal 204 body
bytes before exposing the response body.

Protocol and bearer-auth request validation is global middleware. It runs
before endpoint handlers, path extractors, fallbacks, and method rejections.
Consequently unknown paths, wrong methods, and malformed route paths cannot
bypass validation, and their 400/401/404/405 responses still carry exactly one
`X-Store-Protocol: v1` header.

Serialized mutation requests are limited to 1 MiB before dispatch. Responses
are limited to 64 header values, 32 KiB of aggregate header name/value bytes,
and a 4 MiB body. The client uses HTTP/1 only and enforces the documented
header limits immediately after receipt. The pinned reqwest/hyper HTTP/1 parser
also has its own finite parser bounds; reqwest does not expose a supported
client-builder API for configuring a pre-allocation header-byte cap, so this
protocol does not claim one.

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

`target` and `last_activity` are required and may occur exactly once. Every
other member is unique unknown metadata and must round-trip unchanged. Duplicate
route or metadata fields are invalid. Timestamps use the exact canonical UTC
shape produced by `...sssssssssZ`: nine fractional digits and a trailing `Z`.
Offsets and variable fractional precision are rejected by parse plus canonical
re-serialization equality.

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
values and represents one consistent snapshot. The decoder detects duplicate
top-level keys before a JSON map can overwrite them and applies the same strict
duplicate/type/timestamp decoder to every record. Invalid keys, duplicate keys,
malformed values, or an oversized body are corrupt data; no partial snapshot
may be published.

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
X-Store-Last-Activity: 2026-07-13T06:40:00.000000000Z
```

The versioned envelope has four members:

- `version`: exactly `v1`.
- `operation`: exactly `add`, `put`, or `put_preserving_activity`.
- `route`: one complete route value.
- `activityFloor`: one valid canonical timestamp; null and omission are invalid.

For `put`, the sidecar atomically stores `route` exactly. For `add` and
`put_preserving_activity`, it atomically stores the same route except that
`last_activity` is the maximum of the candidate, the incoming floor, and the
existing persisted activity. `put_preserving_activity` is always sent as that
operation, including when the client's current local floor is absent; local
absence never downgrades the request to `put`.

Every successful PUT 204, including plain `put`, carries exactly one
`X-Store-Last-Activity` header. Its value is the canonical UTC nanosecond
timestamp selected by the atomic server commit. The client validates the
single-valued header by parse plus canonical re-serialization. For `add` and
`put_preserving_activity`, the Store result is constructed from the candidate
target and metadata plus this server-selected activity, so the returned record,
registry cache, and backend record are identical. Plain `Store::put` discards
the reconstructed record but still requires and validates the header. Missing,
repeated, malformed, or noncanonical activity headers after the possible commit
are indeterminate. Repeating an envelope is idempotent.

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
returns 404 with no content type and an empty body. The 200 body uses the same
strict duplicate/type/timestamp route decoder as snapshots.

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

This pre-commit guarantee permits bounded 5xx retry. PATCH retries reuse the
byte-for-byte identical payload. A logical PUT/add retry re-samples the activity
floor immediately before every attempt and reserializes the envelope; all
fields remain identical except that `activityFloor` may advance monotonically.
The returned record always uses `X-Store-Last-Activity` from the final
successful attempt rather than a locally predicted floor.
DELETE may retry a returned 5xx because the guarantee proves the delete did not
commit. A transport error or body/reply loss on any mutation is never retried,
because reqwest cannot prove whether dispatch reached the commit point.

The executable fixture decodes PUT envelopes and nested route objects directly
from raw JSON, before any `Value` map can collapse duplicate members. Its
durable temp-file state is reconstructed into a new server state on restart;
the retry-floor tests restart and reload to prove the last attempted floor was
persisted. A raw TCP companion emits literal invalid 204 responses for duplicate
protocol/content-length headers, transfer encoding, truncated framing, and
oversized framing. HTTP semantics suppress 204 response bodies, so the protocol
claims and tests the reachable framing-validation path rather than claiming an
unreachable body-read path.

## Consistency, indeterminate outcomes, and recovery

The sidecar must linearize each mutation and return a snapshot from one
consistent point. Independent writers must implement the same atomic rules.
The protocol has no ownership inference, operation marker, or corrective GET.

After a mutation may have committed, success requires the complete exact
acknowledgment: status, protocol header, content type/body shape, response
headers, the exact committed-activity header for PUT, and bounded readable body.
A missing/wrong protocol header, missing/duplicate/malformed/noncanonical PUT
activity header, unexpected 1xx/2xx/3xx status, invalid content type, nonempty
204/404, malformed DELETE 200, truncated read, oversized headers/body, or
body-read failure is
`StoreError::Indeterminate`. This remains true even if a later GET appears to
show the requested value or absence. `RouteRegistry` terminally seals mutation
admission and all cached serving on that error; readiness and
management/data-plane reads fail closed. Recovery requires an authoritative
`GET /v1/routes` into a new registry/process after the sidecar is reachable.
The poisoned registry is never unsealed in place.

## Error mapping

| Wire or validation result | Store result | Retry |
| --- | --- | --- |
| Exact GET 200 or mutation 200/204/404 acknowledgment, including the PUT committed-activity header | Success described above | No |
| Mutation 4xx other than DELETE 404 | `StoreError::Backend` (v1 proves rejection/no commit) | No |
| Mutation 5xx exhausted | `StoreError::Backend` (v1 proves pre-commit) | Bounded |
| GET transport/body-read loss | `StoreError::Backend` after exhaustion | Bounded |
| Oversized snapshot body | `StoreError::CorruptData` | No |
| Other GET protocol/header/content/status failure | `StoreError::Backend` | No |
| Malformed snapshot record/key/JSON | `StoreError::CorruptData` | No |
| Any unusable reply after mutation may commit | `StoreError::Indeterminate` | Never |
| Oversized outgoing mutation body before dispatch | `StoreError::Backend` | No |

All mappings use fixed operation labels and exclude credentials, URLs, response
bodies, and route targets.
