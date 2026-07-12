# CHP 5.3.0 behavioral compatibility notes

## Route mutation cancellation and lifetime

Route registry mutations are cancellation-independent once admitted. `add`,
`put`, activity updates, and deletes all use one registry-owned supervisor
without retaining `JoinHandle`s. Its single synchronized state contains the
terminal admission seal, active count, diagnostics, and overflow counters.
Admission checks the seal and increments under that lock before an RAII guard
is transferred to the task, so completion, panic unwind, runtime cancellation,
and future drop all decrement under the same lock and notify. Each task
serializes with the other mutations, awaits the backend result, and then
publishes the reconciled immutable snapshot. Dropping an HTTP request future
only drops its oneshot result receiver; it does not cancel the logical mutation.
The supervisor still observes completion, records a detached `StoreError` or
redacted panic diagnostic, decrements the count, and notifies drain waiters. A
live caller receives its exact backend result; a task panic becomes an
operation-specific `StoreError` without retaining its payload.

Detached diagnostics share a fixed-capacity 256-entry `VecDeque`. Overflow
evicts the oldest diagnostic and increments a per-kind dropped counter surfaced
by the next drain. A drain consumes exactly the diagnostics and counters present
while it holds the tracker lock; an active task that reports afterward is
visible to the next drain.

`RouteRegistry::load` does not install or replace a process panic hook. The
application-owned startup path must call
`install_route_mutation_panic_hook_at_startup()` once after other crash-reporting
setup and leave it installed for the process lifetime. It captures the prior
hook and delegates all panics except those marked by a thread-local scope used
only while polling a supervised mutation. Marked panics write fixed redacted
text plus compile-time source file, line, and column through `std::io::Write`,
with output errors ignored and without formatting the payload or `Debug` data.
Without explicit installation, the registry does not claim process-hook payload
redaction.

The task holds the registry alive until the backend operation finishes. Store
implementations must use finite operation timeouts; an ordinary timeout returns
an error, releases the mutation lock, and drops the task's registry reference.
`RouteRegistry::drain_mutations(timeout)` atomically and permanently seals new
mutation admission, then waits within a caller-supplied bound. Work admitted
before the seal drains; later work receives a fixed shutdown `StoreError`
without persistence or publication. The structured outcome contains timeout
state, remaining active count, accumulated detached failures and panics, and
overflow counts. Repeated drains consume newly available diagnostics but remain
sealed. The drain uses armed-notification, locked active recheck, and one final
locked outcome observation. An inactive zero-duration drain succeeds, and
`timed_out == false` implies no active mutations. Timeout never cancels pending
work. Task 8 startup must install and own the hook after crash reporting;
graceful shutdown must stop accepting management requests, invoke this terminal
drain, and surface timeout and overflow before terminating Tokio. Runtime termination can
still cancel work left after a reported timeout, so every backend mutation must
itself be one atomic persistence operation. A backend that remains pending
forever violates the store timeout requirement.

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
