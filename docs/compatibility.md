# CHP 5.3.0 CLI compatibility

This matrix is audited against the pinned CHP 5.3.0 CLI source,
`bin/configurable-http-proxy` lines 24–120. “Identical” means the option name,
value shape, default, and typed configuration intent match CHP. “Semantic
equivalent” means the same purpose is provided through a Rust-native mechanism.
“Intentional difference” means startup rejects an unsafe or binary-incompatible
request with an actionable error; it is never silently ignored.

The differential oracle is built from CHP commit
`5651b9d7449aea6c6a390ecd81a9955146a2b05f` using source-archive SHA-256
`7e749e76b39de0e0d3c204440e1c929e89fbe14a8241390d01053500e41be2ec`;
its runtime probe must report the same commit. The JupyterHub consumer image
installs the universal JupyterHub 5.5.0 wheel at
SHA-256 `2e38d1767742d41911cfc2160cad485eae2698f98ed291308c7d3be090c757a0`,
verifies commit `97b3154610726b5b7d8768f1e89a4d910e002854` from source-archive
SHA-256 `525dfd807f318f19158bb28f31568866d95411a644635d4334e701f6e8f28bdb`,
and byte-compares the installed Python modules and templates to that source tree
before emitting its runtime source marker.

The configuration contract is exercised by `tests/config_contract.rs`. Runtime
behavior is exercised by the proxy, TLS/Unix, WebSocket, store, differential,
and JupyterHub suites named below; this matrix does not infer support from CLI
parsing alone.

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

On supported Unix targets, PID and UDS owners authenticate every component from
the filesystem root through the full canonical parent, and retain a descriptor
for that parent. Every component must be owned by root or the process effective
UID. Group/other-writable components are accepted only when also sticky and
owned by root or the effective UID, permitting an authoritative `/tmp` while
rejecting untrusted rename authority. Extended ACLs are rejected where the
target supports authoritative ACL inspection; Unix targets without that support
fail closed. These invariants are rechecked before pathname-dependent binding
and publication. Linux uses descriptor-backed stable paths; on Apple, pathname
stability instead derives from reauthentication of the complete ancestor chain
around stable-path resolution. PID creation uses the same boundary.

Cleanup atomically moves the public entry into an unpredictable mode-0700
private directory, verifies and unlinks the candidate through that retained
directory descriptor, and removes the empty private directory during ordinary
cleanup. The unlink itself is not claimed to be atomic with verification.
A foreign replacement is restored with no-replace semantics; a restoration
collision preserves both entries for diagnosis. This closes the public-parent
verify/unlink race against separate-UID namespace attackers under normal Unix
permission enforcement. Unix does not isolate processes sharing the service
UID, and root can bypass ordinary permission checks: same-UID and root actors
are explicitly outside the enforceable trust boundary.

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
its authority-checked stable path if the configured parent alias is replaced.
Immediately after creating a cryptographically named stage with `mkdirat`, a
provisional guard is armed. A nofollow `statat` must identify a directory owned
by the effective UID whose permission bits are a subset of 0700. This accepts a
mode-000 result from a fully restrictive umask; descriptor-relative `chmodat`
normalizes it to exact mode 0700 before `openat(O_DIRECTORY|O_NOFOLLOW)`, then
the opened descriptor is rechecked for identity, owner, exact mode, and ACL
authority before child use. This owner check covers replacement by other UIDs
even when the proxy itself is privileged; same-UID and root attackers remain
outside the boundary.

The socket is bound inside that authenticated private stage, verified by exact
device, inode, and socket type, changed to mode 0660 with descriptor-relative
`fchmodat` flags 0, and verified again for identity, type, and exact mode. A
cross-directory no-clobber rename then publishes it; neither the application nor
Pingora performs a public-path chmod. Before real readiness is forwarded, the
configured parent is reopened and the basename is checked relative to it
against both the captured parent and owned socket identities; an alias change
therefore exits without readiness and cleans only the anchored socket.

If opening or descriptor authentication fails, the provisional guard performs a
descriptor-relative nofollow lookup and removes only an empty candidate that is
still a directory owned by the effective UID with permission bits no more
permissive than 0700. Unknown, foreign-owned, non-directory, or permissive-mode
replacements are preserved. After authentication, the identity guard owns
cleanup and likewise avoids pathname adoption. Verification and unlink remain
separate operations protected by the stated permission boundary; no atomic
identity-conditional unlink is claimed. Same-UID and root namespace attackers
remain outside that documented boundary.

| CHP long option | Classification | Configuration behavior and contract test |
|---|---|---|
| `--ip` | Identical | Selects the public TCP address; omission and `*` mean all interfaces as in CHP; `listener_options_are_typed`, `star_ip_alias_matches_chp_all_interfaces_behavior`. |
| `--port` | Identical | Public port defaults to 8000; explicit zero is falsy and also normalizes to 8000; `listener_defaults_match_chp`, `api_port_defaults_to_public_port_plus_one`, `falsy_listener_and_redirect_ports_match_chp`. |
| `--socket` | Identical | Selects a public Unix socket and conflicts with explicitly supplied IP/port; `listener_options_are_typed`, `socket_options_conflict_with_tcp_options`. |
| `--ssl-key` | Identical | Public TLS private key; a key/certificate identity must be complete; `all_supported_tls_options_are_preserved`, `validation_errors_are_explicit_and_non_panicking`. |
| `--ssl-cert` | Identical | Public TLS certificate; `all_supported_tls_options_are_preserved`. |
| `--ssl-ca` | Identical | Public listener client CA; `all_supported_tls_options_are_preserved`. |
| `--ssl-request-cert` | Identical | Requests public-listener client certificates; `all_supported_tls_options_are_preserved`. |
| `--ssl-reject-unauthorized` | Identical | Rejects unauthorized public-listener clients; `all_supported_tls_options_are_preserved`. |
| `--ssl-protocol` | Identical | Preserves the requested TLS protocol for public, API, and client TLS setup; `all_supported_tls_options_are_preserved`. |
| `--ssl-ciphers` | Identical | An explicit OpenSSL cipher expression is preserved. When omitted, TLS uses CHP 5.3.0’s exact lines 144–177 policy, including the duplicate `!RC4`; `all_supported_tls_options_are_preserved`, `omitted_ssl_ciphers_use_the_exact_chp_5_3_0_policy`. |
| `--ssl-allow-rc4` | **Intentional difference** | Always fails startup clearly. Modern OpenSSL security policy cannot safely re-enable removed RC4 support; `validation_errors_are_explicit_and_non_panicking`. |
| `--ssl-dhparam` | Identical | Preserves the DH parameters path for TLS setup; `all_supported_tls_options_are_preserved`. |
| `--api-ip` | Identical | API host defaults to `localhost`; `listener_defaults_match_chp`, `listener_options_are_typed`. |
| `--api-port` | Identical | Defaults to public port plus one, or 8001 for a public Unix socket; explicit zero follows the same derived-default path; `api_port_defaults_to_public_port_plus_one`, `listener_options_are_typed`, `falsy_listener_and_redirect_ports_match_chp`. |
| `--api-socket` | Identical | Selects an API Unix socket and conflicts with explicitly supplied API IP/port; `listener_options_are_typed`, `socket_options_conflict_with_tcp_options`. |
| `--api-ssl-key` | Identical | API TLS private key; requires its certificate; `all_supported_tls_options_are_preserved`, `validation_errors_are_explicit_and_non_panicking`. |
| `--api-ssl-cert` | Identical | API TLS certificate; `all_supported_tls_options_are_preserved`. |
| `--api-ssl-ca` | Identical | API client CA; `all_supported_tls_options_are_preserved`. |
| `--api-ssl-request-cert` | Identical | Requests API-client certificates; `all_supported_tls_options_are_preserved`. |
| `--api-ssl-reject-unauthorized` | Identical | Rejects unauthorized API clients; `all_supported_tls_options_are_preserved`. |
| `--client-ssl-key` | Identical | Target-facing client identity key; requires its certificate; `all_supported_tls_options_are_preserved`, `validation_errors_are_explicit_and_non_panicking`. |
| `--client-ssl-cert` | Identical | Target-facing client identity certificate; `all_supported_tls_options_are_preserved`. |
| `--client-ssl-ca` | Identical | Target trust CA, including CA-only configuration; `all_supported_tls_options_are_preserved`, `client_ca_can_configure_target_trust_without_a_client_identity`. |
| `--client-ssl-request-cert` | **Intentional difference** | Fails startup clearly instead of accepting a target-facing request-cert mode that has no meaningful client-side equivalent; `client_certificate_request_flags_are_rejected_instead_of_ignored`. |
| `--client-ssl-reject-unauthorized` | **Intentional difference** | Fails startup clearly instead of silently accepting an unsupported target-facing rejection switch; upstream verification remains controlled by `--insecure` and target CA configuration; `client_certificate_request_flags_are_rejected_instead_of_ignored`. |
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
| `--storage-backend` | **Semantic equivalent with intentional Node-module difference** | `memory`, `redis`, and the versioned HTTP/JSON v1 `sidecar` are implemented typed selections. Arbitrary Node module names/paths fail startup because Rust cannot `require()` Node classes. Runtime coverage includes `sidecar_store_satisfies_backend_neutral_contract` and `shipped_binary_loads_authenticated_sidecar_before_listener_readiness`; configuration coverage includes `supported_storage_backends_are_typed` and `validation_errors_are_explicit_and_non_panicking`. |
| `--keep-alive-timeout` | Identical | Typed keep-alive timeout in milliseconds; omission and explicit zero both normalize to CHP's 5000 ms runtime default; `proxy_and_process_options_match_chp_surface`, `omitted_keep_alive_timeout_uses_chp_runtime_default`, `timeout_zero_semantics_match_chp`. |

## Public behavior compatibility matrix

This table audits externally observable CHP and JupyterHub behavior separately
from the CLI surface. `Identical` rows are checked against the pinned CHP oracle;
backend and lifecycle rows marked semantic equivalent or extension have explicit
Rust and consumer-level contracts instead of an unexplained difference.

| Public behavior | Status | Contract tests / scenarios |
|---|---|---|
| Management API authentication, CRUD, encoded route keys, JSON errors, and unknown metadata | Identical | `chp_and_rust_agree_on_api_crud_and_encoded_unicode_route`, `crud_scenarios_compare_populated_route_tables_at_each_mutation`, `arbitrary_authorization_headers_always_return_a_response`, `post_preserves_unknown_jupyterhub_metadata`. |
| Inactivity query parsing and strictly-older filtering | Identical | `both_inactivity_query_spellings_filter_strictly_older_routes`, `inactive_since_accepts_the_pinned_timezone_stable_date_parse_subset`, `invalid_inactivity_timestamp_matches_chp_400_body`. |
| Longest-prefix/root route selection and all prepend/include-prefix combinations | Identical | `chp_and_rust_agree_on_longest_http_route_selection`, `chp_and_rust_agree_on_every_path_option_combination`, `deepest_route_with_data_wins`, `root_route_is_the_fallback`. |
| Host routing and decoded host/path boundary handling | Identical | `chp_and_rust_agree_on_host_routing_and_redirect_policy`, `network_host_routing_selects_the_host_prefixed_route`, `uri_host_routing_prefix_is_removed_at_a_decoded_path_boundary`. |
| HTTP forwarding, request path/query, hop-header scrubbing, custom headers, origin and X-Forwarded policy | Identical | `network_registered_route_streams_request_and_applies_forwarding_policy`, `network_forwarding_runs_before_custom_headers_and_final_hop_scrub_preserves_websocket`, `uri_request_headers_apply_custom_origin_and_forwarded_host_policy`, `comparator_preserves_duplicate_and_non_utf8_semantic_headers`. |
| WebSocket upgrade, headers/query, binary messages, close, and unavailable upstream | Identical | `chp_and_rust_agree_on_websocket_upgrade_and_messages`, `websocket_messages_cross_the_selected_user_route`, `public_unix_socket_carries_websocket_binary_headers_query_and_close`, `unavailable_websocket_upstream_returns_empty_503_handshake`. |
| Request/response activity timestamps and HTTP/WebSocket phase deduplication | Identical | `network_http_response_activity_waits_for_successful_completion_like_chp`, `network_redirect_request_and_response_body_traffic_records_activity_like_chp`, `chp_activity_phases_dedupe_each_direction_and_ignore_http_response_chunks`, `activity_updates_do_not_change_alias_matcher_order`. |
| Health plus default/custom 404, typed 500, empty 503, file and target fallback behavior | Identical | `chp_and_rust_agree_on_custom_404_and_503_errors`, `chp_and_rust_agree_on_health_errors_and_metric_schema`, `network_typed_internal_errors_are_500_and_custom_failure_uses_reason_phrase`, `network_custom_and_file_errors_follow_chp_fallback_policy`. |
| Redirect status/Location rewriting and protocol/host/port rules | Identical | `chp_and_rust_agree_on_host_routing_and_redirect_policy`, `network_redirects_are_untouched_by_default_and_rewritten_when_enabled`, `uri_redirect_rewrite_matrix_matches_http_proxy`, `redirect_without_host_is_400_and_host_uses_the_exact_https_port`. |
| Request/proxy/keep-alive timeouts and downstream/upstream connection reuse | Identical | `timeout_zero_semantics_match_chp`, `omitted_keep_alive_timeout_uses_chp_runtime_default`, `network_reuses_the_same_downstream_socket_and_upstream_connection`, `network_custom_error_slow_and_oversized_responses_fall_back_within_bounds`. |
| Public and API TLS, encrypted keys, strict/optional mTLS, and upstream certificate/hostname verification | Identical except the two startup-rejected client request flags documented above | `chp_and_rust_agree_on_public_tls`, `chp_and_rust_agree_on_public_mutual_tls`, `public_and_api_https_accept_encrypted_listener_keys`, `optional_client_certificates_accept_absent_untrusted_and_trusted_for_public_and_api`, `uri_http_peer_defaults_to_certificate_and_hostname_verification`, `client_certificate_request_flags_are_rejected_instead_of_ignored`. |
| Public/API/metrics Unix sockets and Unix upstream targets | Identical | `chp_and_rust_agree_on_unix_public_api_and_metrics_sockets`, `public_api_and_metrics_unix_sockets_serve_and_are_cleaned_up`, `selected_route_reaches_a_real_unix_upstream_with_path_and_query`, `uri_unix_http_peer_uses_the_decoded_socket_path`. |
| CHP metric families, metadata, labels, values, deltas, and response/route counters | Identical | `metrics_expose_exact_chp_families_and_status_labels`, `metric_comparator_rejects_unexpected_families_help_types_labels_and_values`, `network_metrics_count_web_requests_proxy_statuses_and_route_lookups`, `metrics_match_completed_responses_and_successful_operation_promises`. |
| In-memory route storage and atomic published snapshots | Semantic equivalent | `memory_store_matches_reference_state_machine`, `memory_store_satisfies_backend_neutral_contract`, `registry_loads_and_exposes_complete_store_snapshot`, `failed_persistence_never_publishes_route`. |
| Redis persistence, restart recovery, mutation ambiguity, and credential-redacted failure | Semantic equivalent first-class backend | `redis_store_satisfies_backend_neutral_contract`, `redis_restart_persistence_recovers_complete_routes_without_clearing`, `redis_every_dispatched_mutator_reply_loss_is_indeterminate`, `redis_unavailable_startup_is_bounded_typed_and_redacts_credentials`. |
| HTTP/JSON v1 sidecar persistence and authenticated startup | Rust extension replacing arbitrary in-process Node storage modules | `sidecar_store_satisfies_backend_neutral_contract`, `sidecar_put_is_idempotent_and_restart_reconnect_preserves_state`, `sidecar_optional_bearer_auth_authenticates_every_endpoint`, `shipped_binary_loads_authenticated_sidecar_before_listener_readiness`. |
| JupyterHub 5.5.0 external-proxy consumption: API reconciliation, escaped users, login, user server, kernel WebSocket, restart, and host routing | Compatible consumer gate | `canonical_harness_builds_and_launches_the_shipped_binary`; required scenarios `proxy_api_add_get_delete`, `proxy_route_reconciliation`, `escaped_route_unicode`, `login`, `single_user_page`, `kernel_websocket_message_flow`, `hub_restart_existing_route_usable`, `proxy_restart_backend_state`, and `host_routing` pass on memory and Redis. |
| PID ownership, non-clobber startup, graceful HTTP/WebSocket drain, and run-owned cleanup | Operationally hardened semantic equivalent | `existing_pid_or_socket_paths_are_refused_without_deleting_the_owner`, `sigterm_drains_an_active_websocket_before_ordered_exit`, `traffic_admission_closes_and_drains_64_concurrent_requests`, `socket_cleanup_never_deletes_a_boundary_replacement`. |

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
