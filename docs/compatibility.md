# CHP 5.3.0 CLI compatibility

This matrix is audited against the pinned CHP 5.3.0 CLI source,
`bin/configurable-http-proxy` lines 24–120. “Identical” means the option name,
value shape, default, and typed configuration intent match CHP. “Semantic
equivalent” means the same purpose is provided through a Rust-native mechanism.
“Intentional difference” means startup rejects an unsafe or binary-incompatible
request with an actionable error; it is never silently ignored.

The configuration contract is exercised by `tests/config_contract.rs`. Runtime
features are only claimed here at the CLI/configuration boundary until their
service-level contract tests land.

## Route mutation lifecycle

Admitted `add`, `put`, activity-update, and delete operations are owned by the
route registry, not by an individual HTTP request future. One synchronized
tracker state contains the terminal admission seal, active count, bounded
diagnostics, and overflow counts. Admission increments under that state lock
and transfers an RAII guard to the registry-owned task; the registry deliberately
retains no Tokio task handles. Cancelling a request drops only its result
receiver; persistence and immutable-snapshot reconciliation continue.
`RouteRegistry::drain_mutations(timeout)` atomically seals admission and provides
the bounded shutdown boundary. The seal is permanent, so every mutation that
begins admission afterward returns the same shutdown `StoreError` without
reaching storage or publication. A mutation already admitted is allowed to
finish. `RouteRegistry::mutation_status()` is the non-consuming, non-sealing
active-count probe.

The drain returns whether it timed out, the remaining active count, and any
detached backend failures or task panics accumulated since the previous drain.
Detached diagnostics use one 256-entry oldest-first eviction ring. Each drain
consumes the entries and per-kind dropped counts present at that instant exactly
once; still-active tasks report later outcomes to the next drain. Panic
diagnostics contain only the operation kind and fixed text, while debug
formatting redacts detached backend error text.

A caught backend panic terminally changes that registry to
`MutationSeal::BackendPanic`. The mutation mutex remains held while the panic
fence is installed, so already-admitted work queued behind the failing
operation and every later mutation are rejected before entering the store.
Consequently an opaque panic payload is quarantined with `mem::forget` at most
once per registry; arbitrary payload `Drop` code is never executed, and the
quarantine cannot grow per management request. This is a fail-stop store
contract violation, distinct from the normal graceful-shutdown seal.

`RouteRegistry::load` never installs or replaces a process panic hook. The
application must explicitly call
`install_route_mutation_panic_hook_at_startup()` once, after its other
crash-reporting setup. This captures the prior hook and delegates every panic
outside a thread-local scope active only while polling a supervised mutation.
For a scoped mutation panic it writes fixed redacted text plus source file,
line, and column through `std::io::Write`, ignoring output errors and never
formatting the payload. The application owns the installed hook for the process
lifetime and must not replace it later. Without this startup call, mutation
panics are still converted to fixed `StoreError`s, but payload redaction from
the existing process hook is not guaranteed.

Task 8 startup must install and permanently own that hook after all other crash
reporting is configured. Shutdown must stop accepting management requests,
call the terminal drain, surface timeout plus diagnostic overflow in its
outcome, and only then allow the Tokio runtime to terminate. A timeout is
reported but does not cancel pending admitted work.
An already-inactive zero-duration drain succeeds; an active zero-duration drain
reports timeout only while its final locked active observation remains nonzero.
Consequently `timed_out == false` always implies `active_mutations == 0`.

## Task 8 listener ownership and shutdown bounds

The production terminal mutation-drain bound is exactly five seconds. Ordered
shutdown spends at most one second stopping management and public accepts,
then five seconds draining terminal mutations, one second flushing the activity
watermark, and two seconds draining admitted HTTP/WebSocket traffic. Pingora's
ten-second grace period strictly contains that nine-second sequential bound;
final Tokio runtime teardown has its own one-second bound.

On supported Unix targets, PID and UDS owners retain an open descriptor for the
original canonical parent. UDS publication and cleanup are dirfd-relative.
Cleanup atomically moves the public entry into an unpredictable mode-0700
private directory, verifies and unlinks the candidate through that directory's
descriptor, and removes the empty private directory during ordinary cleanup.
A foreign replacement is restored with no-replace semantics; a restoration
collision preserves both entries for diagnosis. This closes the public-parent
verify/unlink race against separate-UID namespace attackers under normal Unix
permission enforcement. Unix does not isolate processes sharing the service
UID: a same-UID actor able to mutate the parent namespace is explicitly outside
the enforceable trust boundary, so this is not claimed as a mathematical
absolute against same-UID interference.

Public-listener exit/readiness and descriptor ownership guards exist before
the first await or FD handoff. Cancellation while Pingora's shared FD table is
locked therefore closes the retained descriptor, removes owned files, withholds
readiness, and acknowledges exit. A real non-listener descriptor contract
exercises Pingora's listener-build failure path. Each `RequestContext` owns its
traffic-admission token and releases it through one take-once path used by the
completion callback and `Drop`, including panic and cancellation unwinding.
The non-owning raw-FD guard remains armed after table insertion until Pingora's
listener construction has completed; successful adoption explicitly disarms
it. UDS prebinding opens the canonical parent before bind and continues through
its descriptor-backed stable path if the configured parent alias is replaced.
Pingora receives that anchored published path, never the configured alias, for
its mandatory UDS permission update. Immediately before real readiness is
forwarded, the configured parent is reopened and the basename is checked
relative to it against both the captured parent and owned socket identities;
an alias change therefore exits without readiness and cleans only the anchored
socket. Immediately after `mkdirat`, private cleanup opens the new directory
with `O_DIRECTORY|O_NOFOLLOW`, captures its descriptor identity, and arms
cleanup before pathname verification. Initial-`statat` faults therefore remove
the identity-matching empty namespace; open failures and foreign or unknown
identities are preserved.

| CHP long option | Classification | Configuration behavior and contract test |
|---|---|---|
| `--ip` | Identical | Selects the public TCP address; omission and `*` mean all interfaces as in CHP; `listener_options_are_typed`, `star_ip_alias_matches_chp_all_interfaces_behavior`. |
| `--port` | Identical | Public port defaults to 8000; explicit zero is falsy and also normalizes to 8000; `listener_defaults_match_chp`, `api_port_defaults_to_public_port_plus_one`, `falsy_listener_and_redirect_ports_match_chp`. |
| `--socket` | Identical | Selects a public Unix socket and conflicts with explicitly supplied IP/port; `listener_options_are_typed`, `socket_options_conflict_with_tcp_options`. |
| `--ssl-key` | Identical | Public TLS private key; a key/certificate identity must be complete; `all_tls_options_are_preserved`, `validation_errors_are_explicit_and_non_panicking`. |
| `--ssl-cert` | Identical | Public TLS certificate; `all_tls_options_are_preserved`. |
| `--ssl-ca` | Identical | Public listener client CA; `all_tls_options_are_preserved`. |
| `--ssl-request-cert` | Identical | Requests public-listener client certificates; `all_tls_options_are_preserved`. |
| `--ssl-reject-unauthorized` | Identical | Rejects unauthorized public-listener clients; `all_tls_options_are_preserved`. |
| `--ssl-protocol` | Identical | Preserves the requested TLS protocol for public, API, and client TLS setup; `all_tls_options_are_preserved`. |
| `--ssl-ciphers` | Identical | An explicit OpenSSL cipher expression is preserved. When omitted, TLS uses CHP 5.3.0’s exact lines 144–177 policy, including the duplicate `!RC4`; `all_tls_options_are_preserved`, `omitted_ssl_ciphers_use_the_exact_chp_5_3_0_policy`. |
| `--ssl-allow-rc4` | **Intentional difference** | Always fails startup clearly. Modern OpenSSL security policy cannot safely re-enable removed RC4 support; `validation_errors_are_explicit_and_non_panicking`. |
| `--ssl-dhparam` | Identical | Preserves the DH parameters path for TLS setup; `all_tls_options_are_preserved`. |
| `--api-ip` | Identical | API host defaults to `localhost`; `listener_defaults_match_chp`, `listener_options_are_typed`. |
| `--api-port` | Identical | Defaults to public port plus one, or 8001 for a public Unix socket; explicit zero follows the same derived-default path; `api_port_defaults_to_public_port_plus_one`, `listener_options_are_typed`, `falsy_listener_and_redirect_ports_match_chp`. |
| `--api-socket` | Identical | Selects an API Unix socket and conflicts with explicitly supplied API IP/port; `listener_options_are_typed`, `socket_options_conflict_with_tcp_options`. |
| `--api-ssl-key` | Identical | API TLS private key; requires its certificate; `all_tls_options_are_preserved`, `validation_errors_are_explicit_and_non_panicking`. |
| `--api-ssl-cert` | Identical | API TLS certificate; `all_tls_options_are_preserved`. |
| `--api-ssl-ca` | Identical | API client CA; `all_tls_options_are_preserved`. |
| `--api-ssl-request-cert` | Identical | Requests API-client certificates; `all_tls_options_are_preserved`. |
| `--api-ssl-reject-unauthorized` | Identical | Rejects unauthorized API clients; `all_tls_options_are_preserved`. |
| `--client-ssl-key` | Identical | Target-facing client identity key; requires its certificate; `all_tls_options_are_preserved`, `validation_errors_are_explicit_and_non_panicking`. |
| `--client-ssl-cert` | Identical | Target-facing client identity certificate; `all_tls_options_are_preserved`. |
| `--client-ssl-ca` | Identical | Target trust CA, including CA-only configuration; `all_tls_options_are_preserved`, `client_ca_can_configure_target_trust_without_a_client_identity`. |
| `--client-ssl-request-cert` | Identical | Preserved target-facing TLS request-cert setting; `all_tls_options_are_preserved`. |
| `--client-ssl-reject-unauthorized` | Identical | Preserved target-facing TLS rejection setting; `all_tls_options_are_preserved`. |
| `--default-target` | Identical | Accepts validated HTTP(S), `http+unix`, and `unix+http` targets. Unix HTTP strictly validates every escape, decodes the complete WHATWG `host` (hostname plus any authority port suffix) as UTF-8 like `decodeURIComponent(target.host)`, and requires a non-empty, NUL-free absolute socket path; `proxy_and_process_options_match_chp_surface`, `default_and_error_targets_accept_valid_unix_http_urls`, `unix_http_validation_includes_the_whatwg_host_port_suffix`, `unix_http_targets_reject_invalid_or_unusable_socket_hosts`. |
| `--error-target` | Identical | Applies the same strict TCP or Unix HTTP target validation as the default target, conflicts with error path, and appends a trailing slash to every target string that does not already end in one, matching CHP before the target is reparsed; `proxy_and_process_options_match_chp_surface`, `default_and_error_targets_accept_valid_unix_http_urls`, `non_root_error_targets_gain_a_trailing_slash_like_chp`, `unix_http_validation_includes_the_whatwg_host_port_suffix`, `unix_http_targets_reject_invalid_or_unusable_socket_hosts`. |
| `--error-path` | Identical | Selects filesystem error pages and conflicts with error target; `error_path_is_supported`, `validation_errors_are_explicit_and_non_panicking`. |
| `--redirect-port` | Identical | Configures the HTTP redirect listener and requires public TLS material; explicit zero is falsy and disables redirect without requiring TLS; `proxy_and_process_options_match_chp_surface`, `validation_errors_are_explicit_and_non_panicking`, `falsy_listener_and_redirect_ports_match_chp`. |
| `--redirect-to` | Identical | Selects the HTTPS port emitted by redirects; explicit zero is falsy and follows the omitted/default destination path; `proxy_and_process_options_match_chp_surface`, `falsy_listener_and_redirect_ports_match_chp`. |
| `--pid-file` | Identical | Preserves the PID-file path for lifecycle setup; `proxy_and_process_options_match_chp_surface`. |
| `--no-x-forward` | Identical | Changes the positive `x_forward` default from true to false; `negative_boolean_flags_default_to_enabled`, `proxy_and_process_options_match_chp_surface`. |
| `--no-prepend-path` | Identical | Changes the positive `prepend_path` default from true to false; `negative_boolean_flags_default_to_enabled`, `proxy_and_process_options_match_chp_surface`. |
| `--no-include-prefix` | Identical | Changes the positive `include_prefix` default from true to false; `negative_boolean_flags_default_to_enabled`, `proxy_and_process_options_match_chp_surface`. |
| `--auto-rewrite` | Identical | Enables redirect Location host/port rewriting; `proxy_and_process_options_match_chp_surface`. |
| `--change-origin` | Identical | Enables target-origin Host rewriting; `proxy_and_process_options_match_chp_surface`. |
| `--protocol-rewrite` | Identical | Preserves CHP’s free-form redirect protocol value; `proxy_and_process_options_match_chp_surface`. |
| `--custom-header` | Identical | Repeatable `name:value`, whitespace-trimmed, with the last duplicate winning; `proxy_and_process_options_match_chp_surface`, `repeated_custom_header_uses_last_value_like_chp`, `validation_errors_are_explicit_and_non_panicking`. |
| `--insecure` | Identical | Disables upstream certificate verification; `proxy_and_process_options_match_chp_surface`. |
| `--host-routing` | Identical | Enables host-as-first-route-component behavior; `proxy_and_process_options_match_chp_surface`. |
| `--metrics-ip` | Identical | Selects the metrics TCP host when metrics port is enabled; `listener_options_are_typed`. |
| `--metrics-port` | Identical | Enables the metrics TCP listener; omission or explicit zero disables metrics; `listener_defaults_match_chp`, `listener_options_are_typed`, `falsy_listener_and_redirect_ports_match_chp`. |
| `--metrics-socket` | Identical | Enables metrics on a Unix socket and conflicts with metrics IP/port; `listener_options_are_typed`, `socket_options_conflict_with_tcp_options`. |
| `--log-level` | Identical | Case-insensitive debug/info/warn/error, default info; invalid levels return errors; `proxy_and_process_options_match_chp_surface`, `validation_errors_are_explicit_and_non_panicking`. |
| `--timeout` | Identical | Typed request timeout in milliseconds. Zero is deliberately preserved because CHP passes it directly to the proxy library instead of applying a falsy default; `proxy_and_process_options_match_chp_surface`, `timeout_zero_semantics_match_chp`. |
| `--proxy-timeout` | Identical | Typed target response timeout in milliseconds. Zero is deliberately preserved because CHP passes it directly to the proxy library instead of applying a falsy default; `proxy_and_process_options_match_chp_surface`, `timeout_zero_semantics_match_chp`. |
| `--storage-backend` | **Semantic equivalent with intentional Node-module difference** | `memory`, `redis`, and `sidecar` are typed configuration selections. Arbitrary Node module names/paths fail startup because Rust cannot `require()` Node classes. The sidecar selection reserves the planned integration boundary; this task does not claim that protocol or runtime adapter is implemented; `supported_storage_backends_are_typed`, `validation_errors_are_explicit_and_non_panicking`. |
| `--keep-alive-timeout` | Identical | Typed keep-alive timeout in milliseconds; omission and explicit zero both normalize to CHP's 5000 ms runtime default; `proxy_and_process_options_match_chp_surface`, `omitted_keep_alive_timeout_uses_chp_runtime_default`, `timeout_zero_semantics_match_chp`. |

## Environment variables

| CHP environment variable | Classification | Behavior |
|---|---|---|
| `CONFIGPROXY_AUTH_TOKEN` | Identical | Supplies management API authentication without exposing a CLI secret option. |
| `CONFIGPROXY_SSL_KEY_PASSPHRASE` | Identical | Supplies the public-listener key passphrase. |
| `CONFIGPROXY_API_SSL_KEY_PASSPHRASE` | Identical | Supplies the API-listener key passphrase. |

All three are covered by `chp_environment_variables_are_consumed`, restored with
panic-safe serialized test guards, and redacted from `Debug` output by
`debug_output_redacts_auth_and_tls_secrets`. Unknown long options are rejected by
Clap and covered by `unknown_long_options_are_rejected`.

## Help coverage

`tests/fixtures/chp-5.3.0-help.txt` is generated directly from the pinned source
with `node /tmp/chp530/bin/configurable-http-proxy --help`. The
`help_long_options_exactly_match_the_pinned_chp_5_3_0_fixture` test applies the
same normalizer to that fixture and Rust help, then compares exact sets so either
an addition or a removal fails. Intentional runtime differences remain visible
in help and fail during validation; neither is hidden or silently accepted.
