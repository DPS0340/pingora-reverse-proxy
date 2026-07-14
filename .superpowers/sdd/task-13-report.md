# Task 13 report: production image, chart, CI, and operations

Date: 2026-07-14 (Asia/Seoul)

Accepted parent: `e34a13ffb72b21194bb8b7166dd3912c5497ca90`

## Independent review closure (2026-07-15)

Closure starts from the original Task 13 commit `122d0a24ae16f072d007aff85bd6d0ebd62bc962`. The independent specification and security/operations reports were reconciled against the frozen review package and current source. Every High, Medium, and Low finding was reproduced or confirmed by direct source inspection; none was dismissed as false. Two additional suspected sibling cases proved false at the directly tested parser/template boundaries and are documented below.

Closure commits, in order:

- `e330a56425698ecde16e2cd2355124ecf58ac340` — fail-closed runtime, listener TLS, single-replica/Recreate, digest, and chart invariants.
- `a358b4e5fa6a842a124797f75111946d6104f06b` — bounded process groups and fail-closed container context/cleanup.
- `e4557dfef39ae060f43a0aabf60f13a2694c15b0` — exact tested-image artifact and serialized no-rebuild promotion.
- `17ce25fa1c2f27696fd87d75b72c54adac21b429` — single-replica operations, credential, rotation, backup, rollback, and migration documentation.
- `a567140cecc0993a5d07ead11e7ffe7b36eb28eb` — fail-closed sibling parser/template regressions.
- `9df3ba7788a3f04f59fa627ec0e06ef2f1fe5d9d` — deterministic process-tree scheduling bounds.
- `2d07b4b54b702f9fa4ec226f5769668c21aa0282` — process-isolated auth environment cases.
- `51febd769f192e7c0bd432dca160a0a093b65878` — legacy and OCI-layout saved-image identity verification.
- `b7e03f9335839d6fecc4e9ba25d4312b48d31c0c` — warning-free shared sidecar startup barrier.
- `4800d766fabdf387297af9a79c9d777b840faba5` — review-closure evidence update.
- `6e7022c11870e88c3bdd9f23c544e0755e38c81b` — closure-report commit identity binding.
- `f1111e1e602c6b355f2155f69d12c0019a8380e5` — avoid a redundant post-KILL process-group signal.
- `WORKFLOW_SCHEMA_CLOSURE_COMMIT_TO_BE_FILLED` — final workflow-schema validation and focused release regression.

### Workflow-schema closure RED and GREEN

On clean starting HEAD `a7c3b71b9d7954449b958e028494938e9e8627ee`, globally installed actionlint 1.7.12 reproduced six real workflow diagnostics:

```text
actionlint .github/workflows/ci.yml .github/workflows/cd.yml
.github/workflows/ci.yml:22:41: context "runner" is not allowed here ... [expression]
.github/workflows/ci.yml:23:42: context "runner" is not allowed here ... [expression]
.github/workflows/cd.yml:14:3: unexpected key "queue" for "concurrency" section ... [syntax-check]
.github/workflows/cd.yml:90:9: shellcheck reported issue ... SC2174 ... [shellcheck]
.github/workflows/cd.yml:106:9: shellcheck reported issue ... SC2129 ... [shellcheck]
.github/workflows/cd.yml:195:9: shellcheck reported issue ... SC2129 ... [shellcheck]
exit 1
```

The focused release regression was strengthened before the workflow fix. After correcting a Ruby 2.6/Psych 3.1 test-harness incompatibility (`YAML.safe_load_file` is unavailable there), its real RED was the intended path invariant:

```text
just test-release
...
authoritative artifact archive path must be a static Linux-safe absolute path (RuntimeError)
error: recipe `test-release` failed on line 29 with exit code 1
```

CI now uses `/tmp/pingora-verified-image` consistently for the verifier outputs and artifact upload; CD downloads that SHA/run-bound artifact into the matching runner-temporary directory and verifies the same two filenames. The authoritative Linux job installs actionlint 1.7.12 from the official release archive, verifies SHA-256 `8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8`, and checks both workflows before `scripts/verify.sh`. CD keeps `group: pingora-container-publication` with `cancel-in-progress: false`, creates the Docker configuration directory at mode 0700 with `install -d`, and groups GitHub output/summary redirects. No action permissions or action pins changed.

GREEN on the closure candidate:

```text
actionlint .github/workflows/ci.yml .github/workflows/cd.yml
exit 0 (no diagnostics)

just test-release
verified_image_id=sha256:bc5857ac9458293d5111ab85c952172cd7f56bceb4e3014ddc4cafac8927b313
release gate passed: strict SemVer, collision-free tags, exact archive identity, and no-rebuild promotion
exit 0
```

The controller-only real-image wrapper is not a product failure. The real production image build, runtime behavior, export, and independent artifact verify/load each emitted PASS before the controller cleanup wrapper attempted to assign zsh's reserved `status` variable and returned 1. The exact owned residue was subsequently removed. This wrapper issue requires no repository source change and is not claimed as a failed image, runtime, export, or verify/load gate.

### Runtime and Helm RED evidence

The smallest runtime and chart regressions were added before implementation. On the original implementation, this command failed as follows:

```text
cargo test --locked --test config_contract --test jupyterhub_e2e
...
running 35 tests
test listener_rejection_requires_certificate_request_and_ca ... FAILED
test internal_required_auth_policy_rejects_missing_empty_and_invalid_values ... FAILED
...
test result: FAILED. 33 passed; 2 failed
```

Both failures were real `unwrap_err()` failures on accepted `AppConfig` values: listener rejection was accepted without request/CA, and `PINGORA_REQUIRE_AUTH_TOKEN=true` with no token was ignored.

The bounded shipped-binary regression independently failed:

```text
cargo test --locked --test jupyterhub_e2e shipped_binary_required_auth_policy_fails_before_listener_binding -- --exact
...
binary did not reject the empty required auth token
test result: FAILED. 0 passed; 1 failed
```

An initial version of that regression used `Command::output` and correctly exposed that the old binary kept running, but the test itself had no bound. It was terminated, its exact three owned processes were removed, and the regression was corrected to use a three-second poll plus `ChildGuard` before the quoted RED run. That interrupted harness attempt is not claimed as RED evidence.

The expanded chart matrix failed immediately on the old Deployment:

```text
just test-helm
...
scripts/assert-helm.rb:10:in `assert': deployment strategy must be Recreate (RuntimeError)
error: recipe `test-helm` failed on line 23 with exit code 1
```

Runtime/Helm GREEN:

```text
cargo test --locked --test config_contract --test jupyterhub_e2e
...
test result: ok. 36 passed; 0 failed
test result: ok. 3 passed; 0 failed

just test-helm
...
helm gate passed: exact single-replica/Recreate storage, TLS, digest, upgrade, and fail-closed matrix
```

The existing real TLS behavioral tests were rerun because they already exercise the review's requested handshake matrix:

```text
cargo test --locked --test tls_unix_contract strict_client_certificates_reject_absent_and_untrusted_but_accept_trusted_everywhere -- --exact
test result: ok. 1 passed; 0 failed

cargo test --locked --test tls_unix_contract optional_client_certificates_accept_absent_untrusted_and_trusted_for_public_and_api -- --exact
test result: ok. 1 passed; 0 failed
```

Helm now sets the non-CHP internal `PINGORA_REQUIRE_AUTH_TOKEN=true` policy, while raw CHP-compatible CLI behavior remains optional-auth by default. Public/API strict client rejection requires request mode and a non-empty CA in both chart validation and direct runtime parsing. The chart requires exactly one replica, uses `Recreate`, supports digest-pinned images, and models upstream private-CA trust independently from optional client identity.

The first final combined Rust invocation also exposed a real test-isolation failure. The new auth-policy regression changed process-global environment while ordinary configuration cases ran concurrently:

```text
cargo test --locked --test config_contract --test jupyterhub_e2e
...
config validation failed: required management authentication token is missing or empty
...
test result: FAILED. 25 passed; 11 failed
```

The auth matrix now runs in exact child test processes with per-command environment. The same required command subsequently passed 36/36 and 3/3 repeatedly; no global policy value leaks into sibling tests.

### Verifier and container-script RED evidence

Focused process-tree and container-boundary regressions were added before their helpers. Their old-code RED results were:

```text
./scripts/test-verify.sh
.../python: can't open file '.../scripts/run-bounded.py': [Errno 2] No such file or directory
exit 1

./scripts/test-container-script.sh
./scripts/test-container-script.sh: line 10: .../scripts/build-container-context.sh: No such file or directory
exit 127
```

The verifier regression creates a nested child that retains the log pipe, asserts TERM-triggered resource cleanup, exercises a TERM-ignoring KILL fallback with an elapsed bound, verifies ordinary exit-code propagation and log capture, and locks the nine canonical phases in order. The container regression places an untracked 0600 secret sentinel under `src/`, inspects the constructed context, and injects container/image scan and removal failures through an exact fake-Docker command log.

Verifier/container-script GREEN:

```text
./scripts/test-verify.sh
...
verify process-tree gate passed: TERM cleanup, KILL bound, logs, status, and nine-phase order

./scripts/test-container-script.sh
...
container script gate passed: allowlisted context sentinel and fail-closed exact cleanup

bash -n scripts/test-container.sh scripts/test-container-script.sh scripts/test-verify.sh scripts/build-container-context.sh scripts/cleanup-container-resources.sh scripts/verify.sh
shellcheck -x scripts/test-container.sh scripts/test-container-script.sh scripts/test-verify.sh scripts/build-container-context.sh scripts/cleanup-container-resources.sh scripts/verify.sh
python3 -m py_compile scripts/run-bounded.py
```

All exited zero; ShellCheck 0.11.0 emitted no diagnostics. The bounded runner creates a dedicated process session, streams combined output itself, sends TERM to the entire group at the phase deadline, waits 15 seconds for repository cleanup traps, and then sends KILL to the group. The container context is made only from tracked Dockerfile/build inputs, and `.dockerignore` independently defaults to excluding everything. Cleanup treats every Docker scan/removal error as a gate failure and verifies the exact owner label after removal.

A later final-HEAD repeat exposed a macOS process-group cleanup race after the KILL fallback had already succeeded:

```text
PermissionError: [Errno 1] Operation not permitted
  File "scripts/run-bounded.py", line 139, in main
    signal_group(process_group, signal.SIGKILL)
error: recipe `test-verify` failed on line 26 with exit code 1
```

The bounded loop had sent KILL successfully, reaped the direct child, and observed the shared output pipe close; its `finally` block then unnecessarily probed and signaled the drained group a second time. The cleanup path now skips only that redundant second KILL when `kill_sent` is already true. Before-KILL errors still trigger the final group cleanup and still fail closed. The complete nested-child/log/resource/TERM/KILL/status/order regression passed 10 consecutive runs after the change.

### Release-policy RED evidence

The release regression was added before the policy and artifact helpers or workflow changes:

```text
./scripts/test-release.sh
ruby: No such file or directory -- .../scripts/release-policy.rb (LoadError)
exit 1
```

Its table includes valid stable, prerelease, build-metadata, and combined SemVer 2.0 tags plus invalid leading-zero, empty-identifier, numeric-prerelease-leading-zero, punctuation, and duplicate-build-separator cases. It also checks Docker-tag mapping collisions, archive/manifest/config/image identity, corruption and Docker inspection failures, SHA-bound cross-workflow artifact names, serialization, tag conflict inspection, full-SHA actions, least privilege, no `latest`, and absence of any CD build.

Release-policy GREEN:

```text
./scripts/test-release.sh
verified_image_id=sha256:bc5857ac9458293d5111ab85c952172cd7f56bceb4e3014ddc4cafac8927b313
release gate passed: strict SemVer, collision-free tags, exact archive identity, and no-rebuild promotion

bash -n scripts/test-container.sh scripts/test-release.sh scripts/image-artifact.sh
shellcheck -x scripts/test-container.sh scripts/test-release.sh scripts/image-artifact.sh
ruby -c scripts/release-policy.rb
ruby -e 'require "yaml"; ARGV.each { |path| YAML.parse_file(path) }' .github/workflows/ci.yml .github/workflows/cd.yml
```

All exited zero and ShellCheck emitted no diagnostics. CI now saves the image only after that same image passes the real container behavior gate, records independently checked archive/manifest/config/image-ID/revision/platform metadata, and uploads both under an artifact name derived only from the verified SHA. CD downloads by the triggering workflow run ID and head SHA, revalidates every identity and checksum, loads the archive, and never rebuilds. Publication is globally serialized; strict version/SHA tags are refused if already present; the release Git tag is fetched and rechecked immediately before candidate push and final manifest promotion. Final tags point at the candidate registry digest, and GitHub publishes a registry-linked build-provenance attestation. No `latest` tag is produced.

SemVer build metadata maps `+` to `_`. This mapping is collision-free because `_` is invalid in every SemVer identifier but valid in a Docker tag; prerelease dots/hyphens and build identifiers otherwise remain unchanged.

The real-image artifact pass discovered two current-engine save-layout assumptions after the production container behavior checks had succeeded. Both failures were retained as release regressions:

```text
unsafe image config path
error: recipe `test-container` failed on line 20 with exit code 1

saved image config does not match the inspected image ID
error: recipe `test-container` failed on line 20 with exit code 1
```

Docker's containerd image store emitted `blobs/sha256/<digest>` config paths and reported the OCI manifest-list digest as `.Id`, while the legacy archive shape uses `<config-digest>.json` and the config digest as `.Id`. The verifier now accepts only those two safe config-path shapes. It independently hashes the config bytes; when image ID differs from config digest, it also requires the exact image-identity blob, verifies that blob's digest, and requires one exact reference from `index.json`. The deterministic release fixture exercises this OCI-layout case. A real create/verify/load pass on implementation HEAD reported:

```text
source_sha=b7e03f9335839d6fecc4e9ba25d4312b48d31c0c
image_id=sha256:48ea3dcc6c60a602a0e9b007faa0391095df06a25cf9f08047727eb22f819b03
archive_sha256=0ed53d721e10a859495238b6bcdba517d773b5aa4904ae4581e282c000b38de2
manifest_sha256=2fadb22009db060a45411b3b98acb3366b0222b6827f848858a1c34183eb6cd1
config_sha256=40fbdac2c0182eca7488b7efff385de7b69c419fec1df040e8fec0cdde8d083f
revision_label=b7e03f9335839d6fecc4e9ba25d4312b48d31c0c
```

One earlier real-image attempt is deliberately not claimed green: after about 15 minutes the local Colima daemon disconnected during the release build with `failed to solve: Unavailable: error reading from server: EOF`. Cleanup then failed closed because both Docker residue scans returned `Cannot connect to the Docker daemon`. The VM manager confirmed the VM stopped; its stale state was cleared, the same profile was restarted, and the full gate was rerun from a fresh artifact directory. No artifact was produced by the failed attempt.

### Closure implementation-HEAD acceptance

On committed implementation HEAD `b7e03f9335839d6fecc4e9ba25d4312b48d31c0c`, these required gates exited zero:

```text
just test-release
just test-verify
just test-container-script
just test-helm
cargo test --locked --test config_contract --test jupyterhub_e2e
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo check --locked --all-targets --all-features
cargo test --locked --test tls_unix_contract strict_client_certificates_reject_absent_and_untrusted_but_accept_trusted_everywhere -- --exact
cargo test --locked --test tls_unix_contract optional_client_certificates_accept_absent_untrusted_and_trusted_for_public_and_api -- --exact
bash -n <all ten shell scripts changed from e34a13f>
shellcheck -x <all ten shell scripts changed from e34a13f>
ruby -e 'require "yaml"; ARGV.each { |path| YAML.parse_file(path) }' .github/workflows/ci.yml .github/workflows/cd.yml
ruby -c scripts/release-policy.rb
python3 -m py_compile scripts/run-bounded.py
git diff --check e34a13ffb72b21194bb8b7166dd3912c5497ca90..HEAD
```

Results were 36/36 configuration contracts, 3/3 shipped-binary/JupyterHub contracts, both real TLS matrices 1/1, and exact success diagnostics from release, process-tree, container-script, and Helm gates. ShellCheck 0.11.0 emitted no diagnostics. `cargo clippy -D warnings` first found the startup barrier wrapper dead in the separately compiled `store_contract` support module; the barrier was refactored to use the fixture's already shared gated-fault path, after which all-target clippy passed without an allow attribute.

The implementation-HEAD real-image command and independent artifact verify/load both exited zero. The real injected `CONTAINER_GATE_INJECT_FAILURE=after-start` run returned the expected 97, and exact container-label, image-label, and `task13-container-*` name scans were empty. The injected Helm run returned 97 and left its dedicated `TMPDIR` empty. The fake-Docker matrix separately proved that container/image scan failures and removal failures fail the gate and use exact labels/names.

### Review-discovered sibling probes that were false

Two additional fail-closed edge cases were probed before adding production code. No production change was necessary:

```text
cargo test --locked --test config_contract listener_tls_file_paths_must_be_non_empty -- --exact
```

The first test draft expected raw Clap parsing to accept an empty `--ssl-key`, `--api-ssl-cert`, or `--ssl-ca`; it instead failed inside `Cli::try_parse_from` with `InvalidValue` and an empty value before `AppConfig` could be built. The retained regression asserts this parser boundary directly.

```text
helm template task13-invalid-non-integral ./helm-chart --set replicaCount=1.5
```

This exited 1 with the exact `replicaCount must be exactly 1 until cross-process route propagation is implemented` diagnostic. The suspected float-to-one acceptance was false; the negative case remains in the Helm matrix.

## Scope and ownership

Task 13 owns the production image/chart/release gates and the deployable closure of Task 12's intentionally fail-closed `sidecar` executable selection. Production Rust changes are limited to `src/config.rs` and `src/main.rs`: validated sidecar endpoint/token/deadlines are converted to `SidecarConfig`, `SidecarStore::connect` runs, and `RouteRegistry::load` completes before listener binding. Memory and Redis construction remain unchanged. Focused config and shipped-binary runtime tests cover this boundary.

## Tool versions

Local focused acceptance used:

- `rustc 1.96.0 (ac68faa20 2026-05-25)`
- `cargo 1.96.0 (30a34c682 2026-05-25)`
- `just 1.56.0` (isolated at `/tmp/pingora-task13-tools/bin/just`)
- Docker client `28.0.0`, server `29.5.2`, Linux/amd64
- Helm `v3.18.3+g6838ebc`
- Ruby `2.6.10p210`, Psych `3.1.0`
- ShellCheck `0.11.0`
- cargo-audit `0.22.2`
- cargo-deny `0.20.2`

The production builder resolves to Rust `1.85.1` from the pinned `rust:1.85-bookworm` manifest digest. The runtime is pinned `debian:bookworm-slim`.

## Strict RED evidence

Artifact tests and recipes were written before the production Dockerfile/chart implementation.

Command:

```text
PATH=/tmp/pingora-task13-tools/bin:$PATH just test-container && just test-helm
```

Real old-artifact RED: after the bounded, repository-owned tar build context was established, the old single-stage image failed during its build because floating `debian:latest` resolved to trixie and the OpenResty package operation failed. The recipe exited nonzero and its exact owned image/container scan was empty after the cleanup trap. An earlier first attempt exposed the old repository's unbounded build context; that owned process was interrupted and its exact resources removed before the deterministic recipe was corrected, so it is not claimed as test RED.

The chart RED was then captured independently:

```text
PATH=/tmp/pingora-task13-tools/bin:$PATH just test-helm
```

`helm lint --strict` passed the old chart, then the structural YAML assertion failed with `KeyError: key not found: "args"` on the old Deployment. This is the required real failure for absent listener/storage/security wiring; it was not fabricated.

Sidecar regression tests were also added before production runtime wiring:

```text
cargo test --locked --test config_contract --test jupyterhub_e2e
```

Real RED: compilation failed with `E0609` because the accepted Task 12 `AppConfig` had no `sidecar` runtime field. This directly demonstrated that the shipped executable could not construct the sidecar path.

## Production image and container evidence

The Dockerfile is a multi-stage locked release build. Builder and runtime base manifests are pinned to:

- `rust:1.85-bookworm@sha256:e51d0265072d2d9d5d320f6a44dde6b9ef13653b035098febd68cce8fa7c0bc4`
- `debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818`

The final stage contains the stripped release binary, Debian CA bundle, required shared libraries, and ordinary utilities supplied by the minimal Debian packages; it is not claimed to contain “runtime libraries only.” Build context source, Cargo, compilers, CMake, Git, Make, Bash, and debug/target trees are rejected by the rootfs assertion. It has exec-form `ENTRYPOINT`, explicit public/API/metrics command arguments, UID/GID `65532:65532`, no declared writable volume, and supports a read-only root with a bounded `/tmp` tmpfs.

During GREEN iteration, the first complete image build exposed real Debian-slim paths (`/usr/bin/bash`, Bash metadata, and empty `/usr/src` directories) through the leakage test. The runtime layer now removes all Bash/dash/sh executables and source directories after package configuration. The subsequent final gate passed; this intermediate failure is why the final image is shell-free rather than merely lacking build tools.

## Helm structural matrix

Canonical recipe:

```text
PATH=/tmp/pingora-task13-tools/bin:$PATH just test-helm
```

GREEN: Helm 3.18.3 linted the chart with `--strict`; independently rendered storage, TLS, upstream-CA, digest, upgrade-strategy, and resource YAML streams were parsed with Ruby/Psych and asserted structurally. Assertions cover:

- all three listener arguments and named public/API/metrics ports/services;
- API ClusterIP by default;
- management, Redis, and sidecar credentials only through `secretKeyRef`;
- no rendered `Secret` resources or `latest` image;
- storage-specific args/env and sidecar endpoint/deadlines;
- public/API/client TLS args and read-only Secret mounts;
- startup/readiness/liveness `/_chp_healthz` probes with finite timeouts;
- CPU/memory requests and limits;
- pod/container UID/GID 65532, non-root, `RuntimeDefault` seccomp, no privilege escalation, all capabilities dropped, read-only root, service-account token automount false;
- writable memory-backed `/tmp` only.

Fail-closed renders cover unknown storage, missing Redis/sidecar Secret refs, missing/credential-bearing sidecar endpoints, non-positive sidecar deadlines, every backend at more than one replica, ineffective listener mTLS, empty TLS keys, partial upstream identity, invalid tag/digest combinations, and an enabled HTTP probe combined with mandatory public client certificates. Each case matches its exact Helm diagnostic.

Failure-path evidence:

```text
TMPDIR=<owned-empty-directory> HELM_GATE_INJECT_FAILURE=after-memory ./scripts/test-helm.sh
```

The gate exited `97` at the requested injection point and its trap removed the exact owned temporary render directory. No cluster resources are created by this gate.

## Sidecar runtime GREEN

Command:

```text
cargo test --locked --test config_contract --test jupyterhub_e2e
```

GREEN: `config_contract` 33/33 and `jupyterhub_e2e` 2/2. The binary regression starts an authenticated protocol fixture, launches the shipped executable with `--storage-backend sidecar`, waits for public health and an authenticated route snapshot, proves one startup health request and one snapshot request, and terminates cleanly. Configuration tests require an HTTP(S) root origin without credentials/query/fragment, a non-empty bearer token, and positive connect/request deadlines; debug output excludes endpoint and token sentinel values.

## Verification, CI, and supply chain decisions

`scripts/verify.sh` is executable, fails fast, requires and prints tool versions, writes per-phase non-secret logs, bounds each gate, and preserves exactly this order:

1. format;
2. clippy with warnings denied;
3. all-target/all-feature tests with `PROPTEST_CASES=4096`;
4. CHP differential;
5. JupyterHub;
6. production container;
7. Helm;
8. cargo-audit with warnings denied;
9. cargo-deny.

Locked flags are present on Cargo build/test inputs. Linux CI is authoritative, has `contents: read`, a 90-minute job bound, explicit native/Rust/Helm/just/audit/deny prerequisites, checksum-pinned Helm, exact cargo-tool versions, full-SHA GitHub actions, a Cargo cache keyed by lockfile/toolchain/config, and failure-only upload of gate logs/proptest regressions. The secondary macOS job installs native dependencies and runs genuine fmt/clippy/unit commands without `continue-on-error`.

The old CD workflow published `latest` on every push with excessive Pages/content/OIDC/package permissions, stale actions, and a broken registry username. It was replaced. Publication now consumes only a successful same-repository `CI` push run, checks out and rechecks its exact verified SHA, requires exactly one strict SemVer 2.0 release tag, downloads only the SHA/run-bound tested image artifact, verifies and loads it, and promotes its registry digest without rebuilding. Publication is serialized and refuses existing version/full-SHA tags. It does not publish `latest`; the digest, source revision, artifact checksums, and provenance attestation are emitted.

Implementation choices were checked against current primary documentation: Dockerfile reference and multi-stage builds (`docs.docker.com/reference/dockerfile`, `docs.docker.com/build/building/multi-stage/`), Docker read-only/tmpfs run behavior (`docs.docker.com/reference/cli/docker/container/run/`, `docs.docker.com/engine/storage/tmpfs/`), Docker save/load and immutable digest behavior (`docs.docker.com/reference/cli/docker/image/save/`, `docs.docker.com/reference/cli/docker/image/load/`, `docs.docker.com/dhi/core-concepts/digests/`), Kubernetes security contexts/probes/service accounts/seccomp (`kubernetes.io/docs/tasks/configure-pod-container/security-context/`, `kubernetes.io/docs/concepts/configuration/liveness-readiness-startup-probes/`, `kubernetes.io/docs/concepts/security/service-accounts/`, `kubernetes.io/docs/tutorials/security/seccomp/`), Helm template validation (`helm.sh/docs/v3/howto/charts_tips_and_tricks/`), SemVer 2.0 (`semver.org/`), GitHub workflow artifacts, `workflow_run`, concurrency, artifact attestations, and secure action pinning (`docs.github.com/en/actions/concepts/workflows-and-actions/workflow-artifacts`, `docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows`, `docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency`, `docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations/using-artifact-attestations-to-establish-provenance-for-builds`, `docs.github.com/en/actions/reference/security/secure-use`), RustSec cargo-audit, and Embark cargo-deny.

Workflow YAML parses successfully with Ruby/Psych. All new shell scripts pass `bash -n`; ShellCheck 0.11.0 reports no diagnostics. The Helm gate's injected failure proves its cleanup/failure semantics. The container gate has a corresponding `after-start` injection used after the canonical image build to prove exact container/image cleanup.

## Security gate concerns assigned to Task 14

Policy was not weakened and no advisory was ignored. On 2026-07-14:

```text
timeout 600 cargo audit --deny warnings
```

failed against 355 locked dependencies with four vulnerabilities: `idna 0.5.0` (`RUSTSEC-2024-0421`), `protobuf 2.28.0` (`RUSTSEC-2024-0437`), `ring 0.17.8` (`RUSTSEC-2025-0009`), and `tracing-subscriber 0.3.18` (`RUSTSEC-2025-0055`). It also denied warnings for unmaintained `adler`, `daemonize`, `derivative`, `paste`, and `rustls-pemfile`, unsound `rand 0.8.5` (`RUSTSEC-2026-0097`), and yanked `spin 0.9.8`.

```text
timeout 600 cargo deny check
```

also failed. In addition to the advisory blockers, license gathering could not establish the `ring 0.17.8` license at the configured 0.8 confidence threshold. Duplicate versions were warnings. These are pre-existing locked graph/Task 14 release blockers, not suppressed exceptions; therefore the complete `scripts/verify.sh` intentionally remains red until they are resolved.

## Operations and migration

`docs/operations.md` covers public health, authenticated API readiness, metrics, startup snapshot ordering, graceful signals, TCP/TLS/UDS configuration, memory/Redis/sidecar behavior, indeterminate fail-stop semantics, outage recovery, backup/restore ownership, Secret/TLS rotation, redaction, probes, resource/security assumptions, staged rollback, and CHP migration. README launch/JupyterHub/storage/TLS/UDS/container/Helm/CI instructions were updated only for behavior covered by source and focused gates.

## Final focused acceptance

Final production image command:

```text
PATH=/tmp/pingora-task13-tools/bin:$PATH just test-container
```

GREEN. The uncached pinned build completed its locked release profile in 17m02s on the local amd64 Docker VM. The canonical rerun reported:

```text
container gate passed: uid/gid=65532 read-only-root health/api/metrics ready cleanup-owned=<unique-run-id>
```

Image inspection asserted `User=65532:65532`, exact exec-form entrypoint and listener command, effective UID/GID 65532, read-only root, no-new-privileges, a successful write to bounded UID-owned `/tmp`, CA bundle, stripped release executable, no shell/build/source/debug paths, authenticated API `{}`, public health `{"status":"OK"}`, a metrics sample, graceful `SIGTERM` exit code 0, and removal of the exact named/labeled image and containers.

Failure cleanup command:

```text
CONTAINER_GATE_INJECT_FAILURE=after-start PATH=/tmp/pingora-task13-tools/bin:$PATH just test-container
```

Expected nonzero result. The subsequent label scans returned no `io.pingora-reverse-proxy.test-owner` containers or images.

Final chart command:

```text
PATH=/tmp/pingora-task13-tools/bin:$PATH just test-helm
```

GREEN: `helm lint --strict` plus the storage, TLS, upstream-CA, digest, upgrade-strategy, resource, and exact fail-closed matrices. Final output: `helm gate passed: exact single-replica/Recreate storage, TLS, digest, upgrade, and fail-closed matrix`.

Final focused runtime/static commands:

```text
cargo test --locked --test config_contract --test jupyterhub_e2e
cargo fmt --all -- --check
timeout 1800 cargo clippy --locked --all-targets --all-features -- -D warnings
timeout 1200 cargo check --locked --all-targets --all-features
bash -n scripts/test-container.sh scripts/test-helm.sh scripts/verify.sh helm-chart/01-install.sh
shellcheck -x scripts/test-container.sh scripts/test-helm.sh scripts/verify.sh helm-chart/01-install.sh
ruby -e 'require "yaml"; ARGV.each { |path| YAML.parse_file(path) }' .github/workflows/ci.yml .github/workflows/cd.yml
git diff --check
```

All original Task 13 focused checks passed. Review-closure counts and exact final-HEAD commands supersede the original 33/33 and 2/2 counts and are recorded in the closure section. The only emitted Rust warning is the accepted vendored Pingora OpenSSL deprecation, which does not evade the project's `-D warnings` checks. ShellCheck returned no diagnostics; both workflows parsed; diff check was empty. Final read-only Docker scans showed zero Task 13 test-owned containers and images.

The complete `scripts/verify.sh` was not claimed green: as assigned to Task 14, it reaches policy gates that intentionally fail on the inherited advisory/license blockers listed above. Task 13 acceptance is the focused image/chart, sidecar runtime, static, syntax, cleanup, and diff set documented here.
