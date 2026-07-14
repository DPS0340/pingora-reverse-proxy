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

Public TLS uses `--ssl-key` and `--ssl-cert`; API TLS uses `--api-ssl-key` and `--api-ssl-cert`. Strict client-certificate rejection additionally requires the matching CA plus request/reject flags; direct runtime validation and Helm both reject ineffective combinations. Upstream private-CA trust uses `--client-ssl-ca` independently. Optional upstream client identity uses the `--client-ssl-key`/`--client-ssl-cert` pair. The chart models these as separate `tls.client.ca` and `tls.client.identity` Secret references, mounts them read-only, and never creates or renders Secret values. Never enable `--insecure` in production without accepting that upstream identity will not be verified.

## Storage behavior

- `memory` has no external dependency and loses all dynamic routes on restart. Use JupyterHub reconciliation or restore routes through the management API after every restart.
- `redis` requires `PINGORA_REDIS_URL`. Routes are stored in the configured Redis hash. Set a finite positive operation deadline and protect Redis with authentication and network policy.
- `sidecar` requires a root HTTP(S) endpoint, bearer token, and finite positive connect/request deadlines. The sidecar must implement `docs/sidecar-store.md`. Both endpoint and token are redacted from configuration debug output.

Remote writes have fail-stop semantics. When a dispatched Redis or sidecar mutation times out or loses its response, the proxy cannot safely infer whether it committed. The registry becomes indeterminate, subsequent management operations fail closed, and health returns `503`; restart against a healthy authoritative store to reload a known snapshot. A definite pre-dispatch/connect failure returns an error without pretending the mutation succeeded.

## Outage and recovery runbook

1. Confirm public health, authenticated `GET /api/routes`, metrics reachability, and recent application logs. Do not paste tokens or credential-bearing Redis URLs into tickets.
2. Check the selected backing service and network path. For Redis, verify the expected hash independently. For sidecar, verify its health, authentication policy, and versioned route snapshot without modifying data.
3. If the proxy reports an indeterminate mutation, stop automated writers. Establish the authoritative store state before retrying; blind retries can overwrite a mutation that actually committed.
4. Restore the backing service, then restart the single proxy pod. Startup must load the snapshot before health succeeds. Compare authenticated routes with the expected JupyterHub state.
5. Resume writers only after authenticated readiness and representative public traffic succeed. Preserve redacted logs and relevant metrics for incident analysis.

Memory mode has no proxy backup: after every restart or rollback, JupyterHub must reconcile the complete route set before public service resumes. Redis backup/restore is owned by the Redis operator and must preserve the configured hash key and values as one consistent point-in-time dataset. Sidecar backup/restore and schema migration belong to the sidecar owner; this proxy has no independent sidecar backup or migration format. Test Redis/sidecar restores away from production, then start a disposable proxy against the restored snapshot and compare authenticated routes before relying on the backup.

## Secrets and rotation

Supply the management token, Redis URL, sidecar bearer token, TLS private keys, and private-key passphrases through the orchestrator's Secret mechanism. Do not put them in Helm values, command-line arguments, image layers, CI variables printed to logs, or ConfigMaps. Configuration debug formatting redacts remote endpoints/credentials and key passphrases, but operators must still avoid enabling request/header logging around `Authorization`.

The chart enforces one replica and `Recreate`; ordinary rolling rotation is neither supported nor safe. Route writers must not target mixed credential generations, and environment-backed credentials plus certificates are read only at startup. Use this controlled-outage procedure:

1. Announce the outage, stop route writers, and drain public traffic. Scale the Deployment to zero or begin a Helm upgrade that uses `Recreate`; confirm the old pod is gone.
2. For the management token, update JupyterHub's secret source and the proxy Secret from the same protected value. For a sidecar token, first configure overlapping acceptance on the sidecar if it supports overlap; otherwise stop both writer and proxy access while changing both ends. For Redis, enable the new credential before replacing the Secret when the server supports overlap. Replace TLS Secrets only while the listener is stopped.
3. Start the single replacement pod. Require successful startup snapshot loading, public health, authenticated API readiness with the new token, metrics, and representative traffic.
4. Resume JupyterHub route writers and public traffic. Revoke an old sidecar/Redis credential only after the replacement is proven healthy.

Create local Secret input files with `umask 077`/mode 0600, pass only the file path to `kubectl --from-file`, and securely remove the file afterward. Prefer an external secret manager or CSI integration. Never place a live token in `--from-literal`, a Helm `--set` value, shell history, or a support capture.

## Deployment, rollback, and migration

The chart runs UID/GID 65532 with no privilege escalation, all capabilities dropped, `RuntimeDefault` seccomp, a read-only root filesystem, service-account token automount disabled, and only a memory-backed `/tmp` `emptyDir` writable. Default requests are 50m CPU/64Mi and limits are 500m CPU/256Mi; tune from observed request concurrency, TLS cost, route count, and WebSocket duration.

The chart requires exactly one replica for memory, Redis, and sidecar and uses `Recreate`. A mutation changes the receiving process's in-memory route snapshot; persistent storage does not broadcast that change to another process. Even desired replica count one would briefly split the route table under RollingUpdate, so upgrades deliberately stop the old pod before starting the new one. This creates a controlled outage until cross-process add/replace/delete propagation is implemented and proven with two live processes.

Deploy by immutable image digest:

```bash
helm upgrade --install proxy ./helm-chart \
  --set image.repository=ghcr.io/example/pingora-reverse-proxy \
  --set image.tag= \
  --set image.digest=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

The chart rejects a simultaneous tag and digest. Version/SHA tags are registry references and can be mutable in registries that do not enforce immutability; use the CI-reported digest as the deployment identity. A release publishes no `latest` tag and reports its source SHA, registry digest, version/SHA references, and provenance attestation.

Before an upgrade, record the deployed chart values and digest, verify backend backup ownership, and budget for the `Recreate` outage. After replacement, check public health, authenticated API routes, metrics, TLS behavior, and real proxy traffic before resuming writers. To roll back, pause writers and public traffic again, restore the prior chart values and exact image digest, and let `Recreate` replace the pod. Memory mode then requires complete JupyterHub reconciliation. Redis/sidecar rollback is permitted only when the prior binary is compatible with the current stored contract; otherwise restore a tested compatible backend snapshot first.

To migrate from CHP:

1. Record CHP 5.3.0 arguments, auth token delivery, route state, host-routing setting, TLS material, and JupyterHub external-proxy configuration.
2. Choose memory only if JupyterHub can repopulate routes; otherwise provision Redis or the versioned sidecar and verify its snapshot.
3. Start this proxy on separate, non-serving listeners and compare authenticated route output and representative HTTP/WebSocket traffic. Do not load-balance traffic or management mutations across CHP and Rust.
4. Pause JupyterHub route writers. Reconcile the complete route set into the Rust proxy, then switch `c.ConfigurableHTTPProxy.api_url` and public traffic together in a controlled window with `should_start=False` and the Secret-managed token.
5. Verify reconciliation, health, metrics, TLS, HTTP/WebSocket traffic, and graceful termination before resuming writers. For rollback, pause writers again, reconcile CHP's route table, then switch API and public traffic back together; merely leaving a stale CHP process running is not a rollback plan.

Run `just test-container` and `just test-helm` for deployment-asset changes. The full clean-Linux release order is encoded in executable form in `scripts/verify.sh`.
