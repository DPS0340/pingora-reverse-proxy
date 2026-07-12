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

The first caught backend panic atomically installs a terminal backend-panic
seal while the registry mutation mutex is still held. Already-admitted queued
work and all later mutations then fail before another store call. The opaque
panic payload is intentionally quarantined instead of dropped because payload
`Drop` is arbitrary code and can panic again; mutex serialization plus the
terminal fence bounds this quarantine to one payload per registry.

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

Task 8 fixes the production terminal mutation-drain timeout at exactly five
seconds. Its ten-second Pingora grace period strictly contains all sequential
phase bounds: one second for management/public accept-stop acknowledgement,
five seconds for terminal mutations, one second for the activity watermark,
and two seconds for admitted HTTP/WebSocket traffic. Final runtime teardown is
bounded separately at one second.

Unix PID/UDS publication and cleanup remain anchored to the originally opened
parent directory. Cleanup places the candidate inside an unpredictable 0700
directory and verifies/unlinks it through that private dirfd, eliminating the
public-parent verify/unlink window for separate-UID attackers under ordinary
Unix permissions. Replacement and restoration collisions are preserved, and
ordinary cleanup removes its private directory. Processes sharing the service
UID are not isolated by Unix file permissions; same-UID namespace attackers
are outside this enforceable trust boundary.

Public UDS prebinding captures that parent before bind and uses a
descriptor-backed stable published path as Pingora's own `ListenAddr`, so
Pingora's UDS permission update cannot follow a replaced configured alias.
Before readiness, a reopened configured parent and descriptor-relative basename
lookup must match the captured parent and socket identities; otherwise startup
fails closed and only the anchored socket is cleaned. The raw listener FD
remains identity-guarded from Pingora table insertion through listener
construction; cancellation or panic closes it without readiness, and successful
adoption disarms the guard. Immediately after `mkdirat`, private-namespace
cleanup opens the directory without following links, captures its descriptor
identity, then arms cleanup before pathname verification. Initial-`statat`
faults remove matching empty debris; open failures and unknown or foreign
identities are preserved.

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
