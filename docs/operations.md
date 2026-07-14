# Operations guide

This guide describes the production behavior verified by the repository gates. The proxy exposes three independent listeners: public traffic, the authenticated CHP management API, and Prometheus metrics.

## Health, readiness, and metrics

`GET /_chp_healthz` is served on the public listener without management authentication. It returns success only while the in-process route registry is usable; an indeterminate remote-store mutation makes it return `503`. The Helm startup, readiness, and liveness probes use this real endpoint. If public TLS requires client certificates, Kubernetes cannot safely embed that client credential in an HTTP probe, so the chart rejects enabled probes with `tls.public.rejectUnauthorized=true`.

Management readiness can be checked with `GET /api/routes` on the API listener using `Authorization: token <CONFIGPROXY_AUTH_TOKEN>`. A `200` response confirms authentication, API service, and a readable registry. A missing or wrong token returns `403`. Keep this listener private even though the chart exposes it as ClusterIP by default.

`GET /metrics` is served only on the metrics listener. Scrape it over the cluster network or a protected monitoring proxy; it has no built-in authentication. Metrics cover API status, mutations, proxy requests, HTTP/WebSocket traffic, lookups, and activity updates.

## Startup and shutdown

For Redis and sidecar storage, the process validates configuration, connects, and loads a complete startup snapshot before it binds any listener. Invalid URLs, missing secrets, non-positive deadlines, unavailable storage, or an invalid snapshot stop startup. This prevents an apparently ready proxy from routing with an empty or unknown registry.

Send `SIGTERM` or `SIGINT` for graceful shutdown. Pingora stops accepting work and observes the configured runtime grace period; the API and metrics services depend on the public service so shutdown ordering remains controlled. Kubernetes grants 30 seconds by default. If requests routinely exceed this, raise `terminationGracePeriodSeconds` and coordinate it with upstream/client timeouts before rollout.

## Listener, TLS, and Unix-socket configuration

The public listener defaults to `0.0.0.0:8000`; the source-built CLI defaults the API to loopback and metrics to disabled, while the production image and chart explicitly bind all three listeners on ports 8000, 8001, and 8002. Override them with `--ip/--port`, `--api-ip/--api-port`, and `--metrics-ip/--metrics-port`.

Each listener can instead use a Unix-domain socket (`--socket`, `--api-socket`, or `--metrics-socket`). Socket options conflict with the corresponding TCP options. Place sockets only in a deliberately writable, access-controlled mount; the hardened chart provides only `/tmp`, and its service/probe model is TCP-oriented, so UDS deployments should supply a tailored workload rather than forcing chart TCP services onto absent ports.

Public TLS uses `--ssl-key` and `--ssl-cert`; API TLS uses `--api-ssl-key` and `--api-ssl-cert`. Client-CA verification additionally requires the matching CA and request/reject flags. Upstream client identity uses `--client-ssl-key`, `--client-ssl-cert`, and optionally `--client-ssl-ca`. The chart mounts existing Secrets read-only and passes file paths; it does not create or render Secret values. Never enable `--insecure` in production without accepting that upstream identity will not be verified.

## Storage behavior

- `memory` has no external dependency and loses all dynamic routes on restart. Use JupyterHub reconciliation or restore routes through the management API after every restart.
- `redis` requires `PINGORA_REDIS_URL`. Routes are stored in the configured Redis hash. Set a finite positive operation deadline and protect Redis with authentication and network policy.
- `sidecar` requires a root HTTP(S) endpoint, bearer token, and finite positive connect/request deadlines. The sidecar must implement `docs/sidecar-store.md`. Both endpoint and token are redacted from configuration debug output.

Remote writes have fail-stop semantics. When a dispatched Redis or sidecar mutation times out or loses its response, the proxy cannot safely infer whether it committed. The registry becomes indeterminate, subsequent management operations fail closed, and health returns `503`; restart against a healthy authoritative store to reload a known snapshot. A definite pre-dispatch/connect failure returns an error without pretending the mutation succeeded.

## Outage and recovery runbook

1. Confirm public health, authenticated `GET /api/routes`, metrics reachability, and recent application logs. Do not paste tokens or credential-bearing Redis URLs into tickets.
2. Check the selected backing service and network path. For Redis, verify the expected hash independently. For sidecar, verify its health, authentication policy, and versioned route snapshot without modifying data.
3. If the proxy reports an indeterminate mutation, stop automated writers. Establish the authoritative store state before retrying; blind retries can overwrite a mutation that actually committed.
4. Restore the backing service, then restart one proxy replica. Startup must load the snapshot before health succeeds. Compare authenticated routes with the expected JupyterHub state.
5. Resume writers and roll the remaining replicas only after the canary remains healthy. Preserve redacted logs and relevant metrics for incident analysis.

Memory mode recovers through management-plane reconciliation. Redis recovery uses the Redis backup/restore procedure, preserving the configured route hash. Sidecar backup and restore belong to the sidecar owner; this proxy has no independent sidecar backup format. Test restores away from production before relying on them.

## Secrets and rotation

Supply the management token, Redis URL, sidecar bearer token, TLS private keys, and private-key passphrases through the orchestrator's Secret mechanism. Do not put them in Helm values, command-line arguments, image layers, CI variables printed to logs, or ConfigMaps. Configuration debug formatting redacts remote endpoints/credentials and key passphrases, but operators must still avoid enabling request/header logging around `Authorization`.

To rotate the management or sidecar token, update the upstream consumer/service and Kubernetes Secret in a coordinated window, then perform a rolling restart because environment-backed Secrets are read only at process startup. For TLS Secret rotation, update the Secret and roll the pods; certificates are loaded at listener startup. Rotate Redis credentials according to the server's overlap mechanism, update the Secret, and roll after the new credential is accepted.

## Deployment, rollback, and migration

The chart runs UID/GID 65532 with no privilege escalation, all capabilities dropped, `RuntimeDefault` seccomp, a read-only root filesystem, service-account token automount disabled, and only a memory-backed `/tmp` `emptyDir` writable. Default requests are 50m CPU/64Mi and limits are 500m CPU/256Mi; tune from observed request concurrency, TLS cost, route count, and WebSocket duration. Run more than one replica only with Redis or a sidecar and understand that activity updates and snapshots are coordinated through that store.

Roll out by immutable image digest or version tag, canary one replica, check health/API/metrics and real proxy traffic, then continue. Roll back the Deployment image/chart values if startup or compatibility checks fail. Do not roll back across a store-contract change until the backing data format and sidecar version are confirmed compatible.

To migrate from CHP:

1. Record CHP 5.3.0 arguments, auth token delivery, route state, host-routing setting, TLS material, and JupyterHub external-proxy configuration.
2. Choose memory only if JupyterHub can repopulate routes; otherwise provision Redis or the versioned sidecar and verify its snapshot.
3. Start this proxy on separate listeners and compare authenticated route output and representative HTTP/WebSocket traffic.
4. Point `c.ConfigurableHTTPProxy.api_url` at the private API listener, keep `should_start=False`, and provide the same management token to both processes during cutover.
5. Shift public traffic gradually. Keep CHP available for rollback until route reconciliation, health, metrics, TLS, and graceful termination have all been observed.

Run `just test-container` and `just test-helm` for deployment-asset changes. The full clean-Linux release order is encoded in executable form in `scripts/verify.sh`.
