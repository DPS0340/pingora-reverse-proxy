# Task 4 implementation report

## Status

Complete. The CHP 5.3.0 CLI/configuration contract is implemented and all
requested verification gates pass.

## Commits

- `f600bfc` — `feat: implement CHP 5.3.0 CLI contract`

## Files

- `src/config.rs` — complete Clap surface, typed `AppConfig`, listener/TLS/store/proxy
  models, environment ingestion, and fallible cross-option validation.
- `src/main.rs` — fallible startup parsing/validation with correct Clap help/error
  printing and no panic path.
- `tests/config_contract.rs` — table-driven option, default, environment,
  validation, help-coverage, and unsupported-option contracts.
- `docs/compatibility.md` — 48-row CHP long-option matrix plus environment and
  help-coverage documentation.

## RED evidence

Command:

```text
cargo test --test config_contract -- --nocapture
```

Actual RED captured in `/tmp/task4-red.txt` before production implementation:

```text
error[E0432]: unresolved imports `pingora_reverse_proxy::config::AppConfig`,
`pingora_reverse_proxy::config::Cli`,
`pingora_reverse_proxy::config::ListenerConfig`,
`pingora_reverse_proxy::config::LogLevel`,
`pingora_reverse_proxy::config::StoreConfig`
error: could not compile `pingora-reverse-proxy` (test "config_contract")
due to 1 previous error; 1 warning emitted
```

## GREEN and regression evidence

Final command and result:

```text
cargo test --test config_contract -- --nocapture
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Final route/store regression command and results:

```text
cargo test --test route_properties --test store_contract -- --nocapture
route_properties: 10 passed; 0 failed
store_contract: 10 passed; 0 failed
```

Final static gates:

```text
cargo fmt --all -- --check
PASS

cargo clippy --all-targets -- -D warnings
Finished `dev` profile; PASS

cargo check --all-targets
Finished `dev` profile; PASS
```

## Authoritative CHP help evidence

The initial pinned CHP help command failed because `/tmp/chp530/node_modules`
was absent and Node reported `ERR_MODULE_NOT_FOUND` for `commander`. No output was
invented. Dependencies were then installed from the pinned source's checked-in
lockfile:

```text
cd /tmp/chp530 && npm ci --omit=dev
added 36 packages, and audited 37 packages
found 0 vulnerabilities
```

Help was generated with the commands required by the brief:

```text
cargo run -- --help > /tmp/rust-help.txt
node /tmp/chp530/bin/configurable-http-proxy --help > /tmp/chp-help.txt
```

A long-option normalizer extracted, sorted, and compared both outputs:

```text
help coverage: CHP=50 Rust=50 missing=0
```

The count includes each program's `--help` and `--version`; the Task 4 matrix and
test-owned authoritative list each contain exactly the 48 CHP options from source
lines 24–120.

Startup-visible difference checks:

```text
--storage-backend ./custom-store.js: exit 1; directs users to the sidecar protocol
--ssl-allow-rc4: exit 1; states OpenSSL security policy will not re-enable RC4
--unsupported: exit 2; reports an unexpected argument
```

## Self-review

- Correctness: reviewed all 48 source options against the pinned CLI and
  `parseListenOptions`; corrected the public default/`*` alias to preserve CHP's
  unspecified all-interface host rather than narrowing it to IPv4.
- Error handling: `Cli::try_parse` and `AppConfig::try_from` return errors;
  production parsing/validation contains no `unwrap`, `expect`, `panic`, `todo`,
  or `unreachable` path.
- Security: authentication tokens and TLS passphrases are sourced only from the
  three CHP environment variables and are not printed by startup errors.
- Architecture: configuration remains isolated in `config.rs`; `main.rs` only
  loads and validates it. No unrelated route/store implementation changed.
- Performance: parsing is startup-only and bounded by the supplied argument list;
  no request-path work was added.
- Scope/diff: staged implementation contained only the four files named by the
  brief. `git diff --check`, formatting, lint, check, and regressions were clean.

## Concerns

- The typed configuration is intentionally consumed only through startup in this
  task. Listener/TLS/store service assembly belongs to the later implementation
  tasks and is not claimed by the CLI-boundary compatibility matrix.
- Node storage modules and RC4 remain deliberate, documented incompatibilities;
  both fail at startup instead of being silently accepted.
