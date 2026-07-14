use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use pingora_reverse_proxy::config::{AppConfig, Cli, ListenerConfig, LogLevel, StoreConfig};
use serial_test::serial;

fn parse_ok<const N: usize>(args: [&str; N]) -> AppConfig {
    let cli = Cli::try_parse_from(args).unwrap_or_else(|error| panic!("CLI parse failed: {error}"));
    AppConfig::try_from(cli).unwrap_or_else(|error| panic!("config validation failed: {error}"))
}

struct EnvGuard {
    original: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn set(values: &[(&'static str, &'static str)]) -> Self {
        let original = values
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            std::env::set_var(name, value);
        }
        Self { original }
    }

    fn unset(names: &[&'static str]) -> Self {
        let original = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        for name in names {
            std::env::remove_var(name);
        }
        Self { original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.original {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[test]
fn listener_defaults_match_chp() {
    let cfg = parse_ok(["proxy"]);
    assert_eq!(cfg.public_listener, ListenerConfig::Tcp(":8000".into()));
    assert_eq!(
        cfg.api_listener,
        ListenerConfig::Tcp("localhost:8001".into())
    );
    assert_eq!(cfg.metrics_listener, None);
}

#[test]
fn star_ip_alias_matches_chp_all_interfaces_behavior() {
    let cfg = parse_ok(["proxy", "--ip", "*"]);
    assert_eq!(cfg.public_listener, ListenerConfig::Tcp(":8000".into()));
}

#[test]
fn api_port_defaults_to_public_port_plus_one() {
    let cfg = parse_ok(["proxy", "--port", "9100"]);
    assert_eq!(
        cfg.api_listener,
        ListenerConfig::Tcp("localhost:9101".into())
    );
}

#[test]
fn falsy_listener_and_redirect_ports_match_chp() {
    let cases = [
        (
            vec!["proxy", "--port", "0"],
            ListenerConfig::Tcp(":8000".into()),
            ListenerConfig::Tcp("localhost:8001".into()),
        ),
        (
            vec!["proxy", "--port", "9100", "--api-port", "0"],
            ListenerConfig::Tcp(":9100".into()),
            ListenerConfig::Tcp("localhost:9101".into()),
        ),
        (
            vec!["proxy", "--socket", "/tmp/proxy.sock", "--api-port", "0"],
            ListenerConfig::Unix(PathBuf::from("/tmp/proxy.sock")),
            ListenerConfig::Tcp("localhost:8001".into()),
        ),
    ];

    for (args, public, api) in cases {
        let cli = Cli::try_parse_from(args).unwrap();
        let cfg = AppConfig::try_from(cli).unwrap();
        assert_eq!(cfg.public_listener, public);
        assert_eq!(cfg.api_listener, api);
    }

    let metrics = parse_ok(["proxy", "--metrics-port", "0"]);
    assert_eq!(metrics.metrics_listener, None);

    let redirect = parse_ok(["proxy", "--redirect-port", "0"]);
    assert_eq!(redirect.redirect_port, None);

    let redirect_to = parse_ok([
        "proxy",
        "--ssl-key",
        "public.key",
        "--ssl-cert",
        "public.crt",
        "--redirect-port",
        "8080",
        "--redirect-to",
        "0",
    ]);
    assert_eq!(redirect_to.redirect_port, Some(8080));
    assert_eq!(redirect_to.redirect_to, None);
}

#[test]
fn timeout_zero_semantics_match_chp() {
    let cfg = parse_ok([
        "proxy",
        "--timeout",
        "0",
        "--proxy-timeout",
        "0",
        "--keep-alive-timeout",
        "0",
    ]);

    assert_eq!(cfg.proxy.timeout_ms, Some(0));
    assert_eq!(cfg.proxy.proxy_timeout_ms, Some(0));
    assert_eq!(cfg.proxy.keep_alive_timeout_ms, Some(5000));
}

#[test]
fn omitted_keep_alive_timeout_uses_chp_runtime_default() {
    let cfg = parse_ok(["proxy"]);

    assert_eq!(cfg.proxy.keep_alive_timeout_ms, Some(5000));
}

#[test]
fn explicit_api_port_avoids_public_port_overflow() {
    let cfg = parse_ok(["proxy", "--port", "65535", "--api-port", "8001"]);
    assert_eq!(
        cfg.api_listener,
        ListenerConfig::Tcp("localhost:8001".into())
    );
}

#[test]
fn listener_options_are_typed() {
    let cases = [
        (
            vec!["proxy", "--ip", "127.0.0.2", "--port", "9100"],
            ListenerConfig::Tcp("127.0.0.2:9100".into()),
            ListenerConfig::Tcp("localhost:9101".into()),
            None,
        ),
        (
            vec![
                "proxy",
                "--socket",
                "/tmp/proxy.sock",
                "--api-socket",
                "/tmp/api.sock",
            ],
            ListenerConfig::Unix(PathBuf::from("/tmp/proxy.sock")),
            ListenerConfig::Unix(PathBuf::from("/tmp/api.sock")),
            None,
        ),
        (
            vec![
                "proxy",
                "--api-ip",
                "127.0.0.3",
                "--api-port",
                "9102",
                "--metrics-ip",
                "127.0.0.4",
                "--metrics-port",
                "9103",
            ],
            ListenerConfig::Tcp(":8000".into()),
            ListenerConfig::Tcp("127.0.0.3:9102".into()),
            Some(ListenerConfig::Tcp("127.0.0.4:9103".into())),
        ),
        (
            vec!["proxy", "--metrics-socket", "/tmp/metrics.sock"],
            ListenerConfig::Tcp(":8000".into()),
            ListenerConfig::Tcp("localhost:8001".into()),
            Some(ListenerConfig::Unix(PathBuf::from("/tmp/metrics.sock"))),
        ),
    ];

    for (args, public, api, metrics) in cases {
        let cli = Cli::try_parse_from(args).unwrap();
        let cfg = AppConfig::try_from(cli).unwrap();
        assert_eq!(cfg.public_listener, public);
        assert_eq!(cfg.api_listener, api);
        assert_eq!(cfg.metrics_listener, metrics);
    }
}

#[test]
fn socket_options_conflict_with_tcp_options() {
    for args in [
        vec!["proxy", "--socket", "/tmp/proxy.sock", "--port", "8000"],
        vec!["proxy", "--socket", "/tmp/proxy.sock", "--ip", "127.0.0.1"],
        vec![
            "proxy",
            "--api-socket",
            "/tmp/api.sock",
            "--api-port",
            "8001",
        ],
        vec![
            "proxy",
            "--api-socket",
            "/tmp/api.sock",
            "--api-ip",
            "localhost",
        ],
        vec![
            "proxy",
            "--metrics-socket",
            "/tmp/metrics.sock",
            "--metrics-port",
            "9000",
        ],
        vec![
            "proxy",
            "--metrics-socket",
            "/tmp/metrics.sock",
            "--metrics-ip",
            "localhost",
        ],
    ] {
        let error = Cli::try_parse_from(args).expect_err("conflicting listeners accepted");
        assert!(error.to_string().contains("cannot be used with"));
    }
}

#[test]
fn all_supported_tls_options_are_preserved() {
    let cfg = parse_ok([
        "proxy",
        "--ssl-key",
        "public.key",
        "--ssl-cert",
        "public.crt",
        "--ssl-ca",
        "public.ca",
        "--ssl-request-cert",
        "--ssl-reject-unauthorized",
        "--ssl-protocol",
        "TLSv1_2",
        "--ssl-ciphers",
        "HIGH:!aNULL",
        "--ssl-dhparam",
        "dh.pem",
        "--api-ssl-key",
        "api.key",
        "--api-ssl-cert",
        "api.crt",
        "--api-ssl-ca",
        "api.ca",
        "--api-ssl-request-cert",
        "--api-ssl-reject-unauthorized",
        "--client-ssl-key",
        "client.key",
        "--client-ssl-cert",
        "client.crt",
        "--client-ssl-ca",
        "client.ca",
    ]);

    let public = cfg.public_tls.unwrap();
    assert_eq!(public.key, Some(PathBuf::from("public.key")));
    assert_eq!(public.cert, Some(PathBuf::from("public.crt")));
    assert_eq!(public.ca, Some(PathBuf::from("public.ca")));
    assert!(public.request_cert && public.reject_unauthorized);
    assert_eq!(public.protocol.as_deref(), Some("TLSv1_2"));
    assert_eq!(public.ciphers.as_deref(), Some("HIGH:!aNULL"));
    assert_eq!(public.dhparam, Some(PathBuf::from("dh.pem")));

    let api = cfg.api_tls.unwrap();
    assert_eq!(api.key, Some(PathBuf::from("api.key")));
    assert_eq!(api.cert, Some(PathBuf::from("api.crt")));
    assert_eq!(api.ca, Some(PathBuf::from("api.ca")));
    assert!(api.request_cert && api.reject_unauthorized);

    let client = cfg.client_tls.unwrap();
    assert_eq!(client.key, Some(PathBuf::from("client.key")));
    assert_eq!(client.cert, Some(PathBuf::from("client.crt")));
    assert_eq!(client.ca, Some(PathBuf::from("client.ca")));
    assert!(!client.request_cert && !client.reject_unauthorized);
}

#[test]
fn client_certificate_request_flags_are_rejected_instead_of_ignored() {
    for flag in [
        "--client-ssl-request-cert",
        "--client-ssl-reject-unauthorized",
    ] {
        let cli = Cli::try_parse_from(["proxy", flag]).expect("client TLS flag parses");
        let error = AppConfig::try_from(cli).expect_err("ignored client TLS flag was accepted");
        assert!(
            error.to_string().contains(flag),
            "error did not identify {flag}: {error}"
        );
    }
}

#[test]
fn omitted_ssl_ciphers_use_the_exact_chp_5_3_0_policy() {
    const CHP_5_3_0_DEFAULT: &str = "ECDHE-RSA-AES128-GCM-SHA256:\
ECDHE-ECDSA-AES128-GCM-SHA256:\
ECDHE-RSA-AES256-GCM-SHA384:\
ECDHE-ECDSA-AES256-GCM-SHA384:\
DHE-RSA-AES128-GCM-SHA256:\
ECDHE-RSA-AES128-SHA256:\
DHE-RSA-AES128-SHA256:\
ECDHE-RSA-AES256-SHA384:\
DHE-RSA-AES256-SHA384:\
ECDHE-RSA-AES256-SHA256:\
DHE-RSA-AES256-SHA256:\
HIGH:!RC4:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA";

    let cfg = parse_ok([
        "proxy",
        "--ssl-key",
        "public.key",
        "--ssl-cert",
        "public.crt",
        "--api-ssl-key",
        "api.key",
        "--api-ssl-cert",
        "api.crt",
        "--client-ssl-ca",
        "targets.ca",
    ]);

    assert_eq!(
        cfg.public_tls.as_ref().unwrap().ciphers.as_deref(),
        Some(CHP_5_3_0_DEFAULT)
    );
    assert_eq!(
        cfg.api_tls.as_ref().unwrap().ciphers.as_deref(),
        Some(CHP_5_3_0_DEFAULT)
    );
    assert_eq!(
        cfg.client_tls.as_ref().unwrap().ciphers.as_deref(),
        Some(CHP_5_3_0_DEFAULT)
    );
}

#[test]
fn client_ca_can_configure_target_trust_without_a_client_identity() {
    let cfg = parse_ok(["proxy", "--client-ssl-ca", "targets.ca"]);
    let client = cfg.client_tls.unwrap();
    assert_eq!(client.key, None);
    assert_eq!(client.cert, None);
    assert_eq!(client.ca, Some(PathBuf::from("targets.ca")));
}

#[test]
fn proxy_and_process_options_match_chp_surface() {
    let cfg = parse_ok([
        "proxy",
        "--ssl-key",
        "public.key",
        "--ssl-cert",
        "public.crt",
        "--default-target",
        "http://default.example/base",
        "--error-target",
        "https://errors.example",
        "--redirect-port",
        "8080",
        "--redirect-to",
        "8443",
        "--pid-file",
        "/tmp/proxy.pid",
        "--no-x-forward",
        "--no-prepend-path",
        "--no-include-prefix",
        "--auto-rewrite",
        "--change-origin",
        "--protocol-rewrite",
        "https",
        "--custom-header",
        " X-One : first ",
        "--custom-header",
        "X-Two: second:value",
        "--insecure",
        "--host-routing",
        "--log-level",
        "DEBUG",
        "--timeout",
        "1000",
        "--proxy-timeout",
        "2000",
        "--storage-backend",
        "memory",
        "--keep-alive-timeout",
        "3000",
    ]);

    assert_eq!(
        cfg.default_target.unwrap().as_str(),
        "http://default.example/base"
    );
    assert_eq!(
        cfg.error_target.unwrap().as_str(),
        "https://errors.example/"
    );
    assert_eq!(cfg.redirect_port, Some(8080));
    assert_eq!(cfg.redirect_to, Some(8443));
    assert_eq!(cfg.pid_file, Some(PathBuf::from("/tmp/proxy.pid")));
    assert_eq!(cfg.log_level, LogLevel::Debug);
    assert_eq!(cfg.store, StoreConfig::Memory);
    assert!(!cfg.proxy.x_forward);
    assert!(!cfg.proxy.prepend_path);
    assert!(!cfg.proxy.include_prefix);
    assert!(cfg.proxy.auto_rewrite);
    assert!(cfg.proxy.change_origin);
    assert_eq!(cfg.proxy.protocol_rewrite.as_deref(), Some("https"));
    assert!(!cfg.proxy.verify_upstream_tls);
    assert!(cfg.proxy.host_routing);
    assert_eq!(cfg.proxy.timeout_ms, Some(1000));
    assert_eq!(cfg.proxy.proxy_timeout_ms, Some(2000));
    assert_eq!(cfg.proxy.keep_alive_timeout_ms, Some(3000));
    assert_eq!(
        cfg.proxy.custom_headers,
        BTreeMap::from([
            ("X-One".into(), "first".into()),
            ("X-Two".into(), "second:value".into()),
        ])
    );
}

#[test]
fn negative_boolean_flags_default_to_enabled() {
    let cfg = parse_ok(["proxy"]);
    assert!(cfg.proxy.x_forward);
    assert!(cfg.proxy.prepend_path);
    assert!(cfg.proxy.include_prefix);
}

#[test]
fn repeated_custom_header_uses_last_value_like_chp() {
    let cfg = parse_ok([
        "proxy",
        "--custom-header",
        "X-Test: first",
        "--custom-header",
        "X-Test: second",
    ]);
    assert_eq!(cfg.proxy.custom_headers.get("X-Test").unwrap(), "second");
}

#[test]
fn error_path_is_supported() {
    let cfg = parse_ok(["proxy", "--error-path", "/srv/chp-errors"]);
    assert_eq!(cfg.error_path, Some(PathBuf::from("/srv/chp-errors")));
}

#[test]
fn default_and_error_targets_accept_valid_unix_http_urls() {
    for option in ["--default-target", "--error-target"] {
        for target in [
            "http+unix://%2Ftmp%2Fproxy.sock/base",
            "unix+http://%2Fvar%2Frun%2Fproxy.sock/errors",
            "http+unix://%2Ftmp%2F%E2%98%83.sock/unicode",
            "unix+http://%2ftmp%2flowercase.sock/lowercase",
        ] {
            let cli = Cli::try_parse_from(["proxy", option, target]).unwrap();
            let cfg = AppConfig::try_from(cli).unwrap();
            let parsed = if option == "--default-target" {
                cfg.default_target
            } else {
                cfg.error_target
            };
            let expected = if option == "--error-target" {
                format!("{target}/")
            } else {
                target.to_owned()
            };
            assert_eq!(parsed.unwrap().as_str(), expected);
        }
    }
}

#[test]
fn non_root_error_targets_gain_a_trailing_slash_like_chp() {
    for (target, expected) in [
        ("http://errors.example/base", "http://errors.example/base/"),
        (
            "https://errors.example/nested/path",
            "https://errors.example/nested/path/",
        ),
        (
            "http+unix://%2Ftmp%2Fproxy.sock/base",
            "http+unix://%2Ftmp%2Fproxy.sock/base/",
        ),
        (
            "unix+http://%2Ftmp%2Fproxy.sock/errors",
            "unix+http://%2Ftmp%2Fproxy.sock/errors/",
        ),
        (
            "http://errors.example/base?code=500",
            "http://errors.example/base?code=500/",
        ),
    ] {
        let cfg = parse_ok(["proxy", "--error-target", target]);
        assert_eq!(cfg.error_target.unwrap().as_str(), expected);
    }
}

#[test]
fn unix_http_validation_includes_the_whatwg_host_port_suffix() {
    let target = "http+unix://%2Ftmp%2Fproxy.sock:123/base";

    let default = parse_ok(["proxy", "--default-target", target]);
    assert_eq!(default.default_target.unwrap().as_str(), target);

    let error = parse_ok(["proxy", "--error-target", target]);
    assert_eq!(
        error.error_target.unwrap().as_str(),
        "http+unix://%2Ftmp%2Fproxy.sock:123/base/"
    );
}

#[test]
fn unix_http_targets_reject_invalid_or_unusable_socket_hosts() {
    for option in ["--default-target", "--error-target"] {
        for target in [
            "http+unix://",
            "http+unix:///tmp/proxy.sock",
            "unix+http:///tmp/proxy.sock",
            "http+unix://tmp/proxy.sock",
            "unix+http://tmp/proxy.sock",
            "http+unix://tmp%2Fproxy.sock/base",
            "http+unix://%00/base",
            "unix+http://%2Ftmp%2Fproxy%00.sock/base",
            "unix+http://%2Ftmp%2Fproxy.sock%/base",
            "http+unix://%2Ftmp%2Fproxy.sock%2/base",
            "unix+http://%2Ftmp%2Fproxy.sock%GG/base",
            "http+unix://%FF/base",
        ] {
            let cli = Cli::try_parse_from(["proxy", option, target]).unwrap();
            let error = AppConfig::try_from(cli).unwrap_err().to_string();
            assert!(
                error.contains("percent-encoded absolute socket path"),
                "expected malformed {option} {target:?} to fail clearly, got {error:?}"
            );
        }
    }
}

#[test]
fn validation_errors_are_explicit_and_non_panicking() {
    let cases = [
        (
            vec![
                "proxy",
                "--error-target",
                "http://errors",
                "--error-path",
                "/errors",
            ],
            "both --error-target and --error-path",
        ),
        (
            vec!["proxy", "--redirect-port", "8080"],
            "TLS key and certificate",
        ),
        (
            vec!["proxy", "--ssl-key", "key.pem"],
            "--ssl-key and --ssl-cert",
        ),
        (
            vec!["proxy", "--api-ssl-cert", "cert.pem"],
            "--api-ssl-key and --api-ssl-cert",
        ),
        (
            vec!["proxy", "--client-ssl-key", "key.pem"],
            "--client-ssl-key and --client-ssl-cert",
        ),
        (vec!["proxy", "--log-level", "verbose"], "log level"),
        (
            vec!["proxy", "--default-target", "not a URL"],
            "default target",
        ),
        (
            vec!["proxy", "--error-target", "file:///tmp/error"],
            "error target",
        ),
        (
            vec!["proxy", "--storage-backend", "sqlite"],
            "unknown storage backend",
        ),
        (
            vec!["proxy", "--storage-backend", "./custom-store.js"],
            "sidecar protocol",
        ),
        (vec!["proxy", "--ssl-allow-rc4"], "RC4"),
        (vec!["proxy", "--custom-header", "missing-colon"], "colon"),
    ];

    for (args, expected) in cases {
        let error = match Cli::try_parse_from(args) {
            Ok(cli) => AppConfig::try_from(cli).unwrap_err().to_string(),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
    }
}

#[test]
fn supported_storage_backends_are_typed() {
    for (name, expected) in [
        ("memory", StoreConfig::Memory),
        ("sidecar", StoreConfig::Sidecar),
    ] {
        let cfg = parse_ok(["proxy", "--storage-backend", name]);
        assert_eq!(cfg.store, expected);
    }
}

#[test]
#[serial]
fn redis_runtime_requires_an_explicit_url() {
    let _env = EnvGuard::unset(&[
        "PINGORA_REDIS_URL",
        "PINGORA_REDIS_ROUTE_KEY",
        "PINGORA_REDIS_OPERATION_TIMEOUT_MS",
    ]);
    let cli = Cli::try_parse_from(["proxy", "--storage-backend", "redis"]).unwrap();
    let error = AppConfig::try_from(cli).unwrap_err().to_string();
    assert!(
        error.contains("PINGORA_REDIS_URL"),
        "unexpected error: {error}"
    );
}

#[test]
#[serial]
fn redis_runtime_configuration_is_validated_and_redacted() {
    const REDIS_URL: &str = "redis://:REDIS_PASSWORD_SENTINEL@redis.example:6379/0";
    let _env = EnvGuard::set(&[
        ("PINGORA_REDIS_URL", REDIS_URL),
        (
            "PINGORA_REDIS_ROUTE_KEY",
            "pingora-reverse-proxy:routes:v1:test",
        ),
        ("PINGORA_REDIS_OPERATION_TIMEOUT_MS", "2750"),
    ]);

    let cfg = parse_ok(["proxy", "--storage-backend", "redis"]);
    assert_eq!(cfg.store, StoreConfig::Redis);
    let redis = cfg.redis.as_ref().expect("Redis configuration is present");
    assert_eq!(redis.url(), REDIS_URL);
    assert_eq!(redis.route_key(), "pingora-reverse-proxy:routes:v1:test");
    assert_eq!(redis.operation_timeout().as_millis(), 2750);
    let debug = format!("{cfg:?} {redis:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("REDIS_PASSWORD_SENTINEL"));
}

#[test]
#[serial]
fn redis_runtime_rejects_invalid_url_key_and_timeout() {
    for (values, expected) in [
        (
            vec![("PINGORA_REDIS_URL", "https://redis.example")],
            "PINGORA_REDIS_URL",
        ),
        (
            vec![
                ("PINGORA_REDIS_URL", "redis://redis.example:6379"),
                ("PINGORA_REDIS_ROUTE_KEY", ""),
            ],
            "PINGORA_REDIS_ROUTE_KEY",
        ),
        (
            vec![
                ("PINGORA_REDIS_URL", "redis://redis.example:6379"),
                ("PINGORA_REDIS_OPERATION_TIMEOUT_MS", "0"),
            ],
            "PINGORA_REDIS_OPERATION_TIMEOUT_MS",
        ),
    ] {
        let _clear = EnvGuard::unset(&[
            "PINGORA_REDIS_URL",
            "PINGORA_REDIS_ROUTE_KEY",
            "PINGORA_REDIS_OPERATION_TIMEOUT_MS",
        ]);
        let _env = EnvGuard::set(&values);
        let cli = Cli::try_parse_from(["proxy", "--storage-backend", "redis"]).unwrap();
        let error = AppConfig::try_from(cli).unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
    }
}

#[test]
#[serial]
fn chp_environment_variables_are_consumed() {
    let _env = EnvGuard::set(&[
        ("CONFIGPROXY_AUTH_TOKEN", "secret-token"),
        ("CONFIGPROXY_SSL_KEY_PASSPHRASE", "public-passphrase"),
        ("CONFIGPROXY_API_SSL_KEY_PASSPHRASE", "api-passphrase"),
    ]);

    let cfg = parse_ok([
        "proxy",
        "--ssl-key",
        "public.key",
        "--ssl-cert",
        "public.crt",
        "--api-ssl-key",
        "api.key",
        "--api-ssl-cert",
        "api.crt",
    ]);

    assert_eq!(cfg.auth_token.as_deref(), Some("secret-token"));
    assert_eq!(
        cfg.public_tls.unwrap().key_passphrase.as_deref(),
        Some("public-passphrase")
    );
    assert_eq!(
        cfg.api_tls.unwrap().key_passphrase.as_deref(),
        Some("api-passphrase")
    );
}

#[test]
#[serial]
fn debug_output_redacts_auth_and_tls_secrets() {
    const AUTH_SENTINEL: &str = "AUTH_TOKEN_SENTINEL_7f31";
    const PUBLIC_SENTINEL: &str = "PUBLIC_PASSPHRASE_SENTINEL_8a42";
    const API_SENTINEL: &str = "API_PASSPHRASE_SENTINEL_9b53";
    let _env = EnvGuard::set(&[
        ("CONFIGPROXY_AUTH_TOKEN", AUTH_SENTINEL),
        ("CONFIGPROXY_SSL_KEY_PASSPHRASE", PUBLIC_SENTINEL),
        ("CONFIGPROXY_API_SSL_KEY_PASSPHRASE", API_SENTINEL),
    ]);
    let cfg = parse_ok([
        "proxy",
        "--ssl-key",
        "public.key",
        "--ssl-cert",
        "public.crt",
        "--api-ssl-key",
        "api.key",
        "--api-ssl-cert",
        "api.crt",
    ]);

    assert_eq!(cfg.auth_token.as_deref(), Some(AUTH_SENTINEL));
    assert_eq!(
        cfg.public_tls.as_ref().unwrap().key_passphrase.as_deref(),
        Some(PUBLIC_SENTINEL)
    );
    assert_eq!(
        cfg.api_tls.as_ref().unwrap().key_passphrase.as_deref(),
        Some(API_SENTINEL)
    );
    let debug = format!(
        "{cfg:?} {:?} {:?}",
        cfg.public_tls.as_ref().unwrap(),
        cfg.api_tls.as_ref().unwrap()
    );
    assert!(debug.contains("<redacted>"));
    for sentinel in [AUTH_SENTINEL, PUBLIC_SENTINEL, API_SENTINEL] {
        assert!(!debug.contains(sentinel), "Debug leaked {sentinel}");
    }
}

fn normalized_long_options(help: &str) -> BTreeSet<String> {
    help.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            line.starts_with('-').then_some(line)
        })
        .flat_map(str::split_whitespace)
        .filter(|token| token.starts_with("--"))
        .map(|token| token.trim_end_matches(',').to_owned())
        .collect()
}

#[test]
fn help_long_options_exactly_match_the_pinned_chp_5_3_0_fixture() {
    let chp_help = include_str!("fixtures/chp-5.3.0-help.txt");
    let rust_help = Cli::command().render_long_help().to_string();

    assert_eq!(
        normalized_long_options(&rust_help),
        normalized_long_options(chp_help),
        "Rust and pinned CHP help long-option sets differ"
    );
}

#[test]
fn unknown_long_options_are_rejected() {
    let error = Cli::try_parse_from(["proxy", "--made-up-flag"])
        .expect_err("unknown option was silently accepted");
    assert!(error.to_string().contains("unexpected argument"));
}
