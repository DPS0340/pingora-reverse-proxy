# CHP 5.3.0 behavioral compatibility notes

## Route mutation cancellation and lifetime

Route registry mutations are cancellation-independent once invoked. `add`,
`put`, activity updates, and deletes all use one registry-owned supervisor that
tracks their `JoinHandle`s and active count. Each task serializes with the other
mutations, awaits the backend result, and then publishes the reconciled
immutable snapshot. Dropping an HTTP request future only drops its oneshot
result receiver; it does not cancel the logical mutation. The supervisor still
observes completion, records a detached `StoreError` or redacted panic
diagnostic, decrements the count, and notifies drain waiters. A live caller
receives its exact backend result; a task panic becomes an operation-specific
`StoreError` without retaining its payload.

The task holds the registry alive until the backend operation finishes. Store
implementations must use finite operation timeouts; an ordinary timeout returns
an error, releases the mutation lock, and drops the task's registry reference.
`RouteRegistry::drain_mutations(timeout)` waits within a caller-supplied bound
and returns a structured outcome containing timeout state, remaining active
count, and accumulated detached failures and panics. Diagnostics are consumed
by each drain; timeout never cancels pending work. Task 8 graceful shutdown must
stop accepting management requests, invoke this bounded drain and surface its
outcome, then terminate the Tokio runtime. Runtime termination can still cancel
work left after a reported timeout, so every backend mutation must itself be
one atomic persistence operation. A backend that remains pending forever
violates the store timeout requirement.

## `requests_api` timing divergence

Task 5 does not provide exact CHP parity for the timing of
`requests_api{status}`. CHP increments this counter from the Node response
`finish` event. The current Axum middleware increments after a handler produces
its `Response`, before the response body is necessarily written to the client.
Consequently, a slow body or client disconnect can be counted earlier, and in
some disconnect cases may be counted where CHP would not observe `finish`.

This narrow timing/disconnect divergence is explicitly deferred to Task 11,
where metrics rendering and response-body lifecycle instrumentation are
implemented together. Status selection and completed in-process handler
results remain covered by the Task 5 contract tests, but they are not labeled
as exact response-finish parity.
