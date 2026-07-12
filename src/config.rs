//! CHP 5.3.0-compatible command-line parsing and typed configuration.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::path::PathBuf;

use clap::{ArgAction, Parser};
use thiserror::Error;
use url::{Position, Url};

const CHP_DEFAULT_KEEP_ALIVE_TIMEOUT_MS: u64 = 5000;
const CHP_5_3_0_DEFAULT_TLS_CIPHERS: &str = "ECDHE-RSA-AES128-GCM-SHA256:\
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

/// Raw command-line options. Validation that spans multiple options is performed by
/// [`AppConfig::try_from`].
#[derive(Debug, Clone, Parser)]
#[command(
    name = "configurable-http-proxy",
    version,
    about = "A Pingora-based configurable HTTP proxy compatible with CHP 5.3.0"
)]
pub struct Cli {
    /// Public-facing IP of the proxy.
    #[arg(long)]
    pub ip: Option<String>,

    /// Public-facing port of the proxy (defaults to 8000).
    #[arg(long)]
    pub port: Option<u16>,

    /// Path to a UNIX domain socket for the public proxy.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["ip", "port"])]
    pub socket: Option<PathBuf>,

    /// SSL key to use for the public listener.
    #[arg(long)]
    pub ssl_key: Option<PathBuf>,
    /// SSL certificate to use for the public listener.
    #[arg(long)]
    pub ssl_cert: Option<PathBuf>,
    /// SSL certificate authority for the public listener.
    #[arg(long)]
    pub ssl_ca: Option<PathBuf>,
    /// Request SSL certificates from public-listener clients.
    #[arg(long)]
    pub ssl_request_cert: bool,
    /// Reject unauthorized public-listener SSL connections.
    #[arg(long)]
    pub ssl_reject_unauthorized: bool,
    /// Set a specific SSL protocol, for example TLSv1_2.
    #[arg(long)]
    pub ssl_protocol: Option<String>,
    /// Colon-separated SSL cipher list.
    #[arg(long)]
    pub ssl_ciphers: Option<String>,
    /// Request deprecated RC4 cipher enablement (unsupported by this implementation).
    #[arg(long)]
    pub ssl_allow_rc4: bool,
    /// SSL Diffie-Hellman parameters PEM file.
    #[arg(long)]
    pub ssl_dhparam: Option<PathBuf>,

    /// Inward-facing IP for API requests (defaults to localhost).
    #[arg(long)]
    pub api_ip: Option<String>,
    /// Inward-facing API port (defaults to the public port plus one).
    #[arg(long)]
    pub api_port: Option<u16>,
    /// Path to a UNIX domain socket for the API server.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["api_ip", "api_port"])]
    pub api_socket: Option<PathBuf>,
    /// SSL key for API requests.
    #[arg(long)]
    pub api_ssl_key: Option<PathBuf>,
    /// SSL certificate for API requests.
    #[arg(long)]
    pub api_ssl_cert: Option<PathBuf>,
    /// SSL certificate authority for API requests.
    #[arg(long)]
    pub api_ssl_ca: Option<PathBuf>,
    /// Request SSL certificates from API clients.
    #[arg(long)]
    pub api_ssl_request_cert: bool,
    /// Reject unauthorized API SSL connections.
    #[arg(long)]
    pub api_ssl_reject_unauthorized: bool,

    /// SSL key used for connections from the proxy to targets.
    #[arg(long)]
    pub client_ssl_key: Option<PathBuf>,
    /// SSL certificate used for connections from the proxy to targets.
    #[arg(long)]
    pub client_ssl_cert: Option<PathBuf>,
    /// SSL certificate authority used for connections from the proxy to targets.
    #[arg(long)]
    pub client_ssl_ca: Option<PathBuf>,
    /// Request SSL certificates for proxy-to-target connections.
    #[arg(long)]
    pub client_ssl_request_cert: bool,
    /// Reject unauthorized proxy-to-target SSL connections.
    #[arg(long)]
    pub client_ssl_reject_unauthorized: bool,

    /// Default proxy target (proto://host[:port]).
    #[arg(long)]
    pub default_target: Option<String>,
    /// Alternate server for handling proxy errors.
    #[arg(long)]
    pub error_target: Option<String>,
    /// Filesystem path containing alternate proxy error pages.
    #[arg(long)]
    pub error_path: Option<PathBuf>,
    /// HTTP port whose requests are redirected to HTTPS.
    #[arg(long)]
    pub redirect_port: Option<u16>,
    /// HTTPS port used in redirects from `--redirect-port`.
    #[arg(long)]
    pub redirect_to: Option<u16>,
    /// Write the process ID to this file.
    #[arg(long)]
    pub pid_file: Option<PathBuf>,

    /// Do not add X-Forwarded-* headers to proxied requests.
    #[arg(long = "no-x-forward", action = ArgAction::SetFalse, default_value_t = true)]
    pub x_forward: bool,
    /// Do not prepend target paths to proxied requests.
    #[arg(long = "no-prepend-path", action = ArgAction::SetFalse, default_value_t = true)]
    pub prepend_path: bool,
    /// Do not include the routing prefix in proxied requests.
    #[arg(long = "no-include-prefix", action = ArgAction::SetFalse, default_value_t = true)]
    pub include_prefix: bool,
    /// Rewrite the Location header host and port in redirect responses.
    #[arg(long)]
    pub auto_rewrite: bool,
    /// Change the Host header origin to the target URL.
    #[arg(long)]
    pub change_origin: bool,
    /// Rewrite the Location header protocol in redirect responses.
    #[arg(long)]
    pub protocol_rewrite: Option<String>,
    /// Custom header added to proxied requests; may be repeated.
    #[arg(long, action = ArgAction::Append)]
    pub custom_header: Vec<String>,
    /// Disable target SSL certificate verification.
    #[arg(long)]
    pub insecure: bool,
    /// Use the request host as the first routing path component.
    #[arg(long)]
    pub host_routing: bool,

    /// IP for the metrics server.
    #[arg(long)]
    pub metrics_ip: Option<String>,
    /// Port for the metrics server; metrics are disabled when omitted.
    #[arg(long)]
    pub metrics_port: Option<u16>,
    /// Path to a UNIX domain socket for the metrics server.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["metrics_ip", "metrics_port"])]
    pub metrics_socket: Option<PathBuf>,

    /// Log level: debug, info, warn, or error.
    #[arg(long, default_value = "info")]
    pub log_level: String,
    /// Request connection timeout in milliseconds.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Target response timeout in milliseconds.
    #[arg(long)]
    pub proxy_timeout: Option<u64>,
    /// Storage backend: memory, redis, or sidecar.
    #[arg(long)]
    pub storage_backend: Option<String>,
    /// Keep-alive connection timeout in milliseconds.
    #[arg(long)]
    pub keep_alive_timeout: Option<u64>,
}

/// A TCP address or Unix-domain socket listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerConfig {
    Tcp(String),
    Unix(PathBuf),
}

/// TLS material shared by listener and target TLS configurations.
#[derive(Clone, PartialEq, Eq)]
pub struct TlsConfig {
    pub key: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub ca: Option<PathBuf>,
    pub key_passphrase: Option<String>,
    pub request_cert: bool,
    pub reject_unauthorized: bool,
    pub protocol: Option<String>,
    pub ciphers: Option<String>,
    pub dhparam: Option<PathBuf>,
}

impl fmt::Debug for TlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsConfig")
            .field("key", &self.key)
            .field("cert", &self.cert)
            .field("ca", &self.ca)
            .field(
                "key_passphrase",
                &self.key_passphrase.as_ref().map(|_| "<redacted>"),
            )
            .field("request_cert", &self.request_cert)
            .field("reject_unauthorized", &self.reject_unauthorized)
            .field("protocol", &self.protocol)
            .field("ciphers", &self.ciphers)
            .field("dhparam", &self.dhparam)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreConfig {
    Memory,
    Redis,
    Sidecar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyOptions {
    pub x_forward: bool,
    pub prepend_path: bool,
    pub include_prefix: bool,
    pub auto_rewrite: bool,
    pub change_origin: bool,
    pub protocol_rewrite: Option<String>,
    pub custom_headers: BTreeMap<String, String>,
    pub verify_upstream_tls: bool,
    pub host_routing: bool,
    pub timeout_ms: Option<u64>,
    pub proxy_timeout_ms: Option<u64>,
    pub keep_alive_timeout_ms: Option<u64>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub public_listener: ListenerConfig,
    pub api_listener: ListenerConfig,
    pub metrics_listener: Option<ListenerConfig>,
    pub public_tls: Option<TlsConfig>,
    pub api_tls: Option<TlsConfig>,
    pub client_tls: Option<TlsConfig>,
    pub default_target: Option<Url>,
    pub error_target: Option<Url>,
    pub error_path: Option<PathBuf>,
    pub redirect_port: Option<u16>,
    pub redirect_to: Option<u16>,
    pub pid_file: Option<PathBuf>,
    pub auth_token: Option<String>,
    pub log_level: LogLevel,
    pub store: StoreConfig,
    pub proxy: ProxyOptions,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppConfig")
            .field("public_listener", &self.public_listener)
            .field("api_listener", &self.api_listener)
            .field("metrics_listener", &self.metrics_listener)
            .field("public_tls", &self.public_tls)
            .field("api_tls", &self.api_tls)
            .field("client_tls", &self.client_tls)
            .field("default_target", &self.default_target)
            .field("error_target", &self.error_target)
            .field("error_path", &self.error_path)
            .field("redirect_port", &self.redirect_port)
            .field("redirect_to", &self.redirect_to)
            .field("pid_file", &self.pid_file)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("log_level", &self.log_level)
            .field("store", &self.store)
            .field("proxy", &self.proxy)
            .finish()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("cannot specify both --error-target and --error-path")]
    ConflictingErrorHandlers,
    #[error("HTTPS redirection requires a complete TLS key and certificate pair")]
    RedirectWithoutTls,
    #[error("{0} must be provided together")]
    IncompleteKeyPair(&'static str),
    #[error("invalid log level {0:?}; expected debug, info, warn, or error")]
    InvalidLogLevel(String),
    #[error(
        "invalid {kind} {value:?}; expected an HTTP(S) URL with a host or a Unix HTTP URL with a valid UTF-8 percent-encoded absolute socket path"
    )]
    InvalidTarget { kind: &'static str, value: String },
    #[error(
        "unknown storage backend {0:?}; use memory, redis, or sidecar. Arbitrary Node storage modules cannot be loaded; use the planned sidecar protocol once implemented"
    )]
    UnknownStorageBackend(String),
    #[error("--ssl-allow-rc4 is unsupported: OpenSSL security policy will not re-enable RC4")]
    Rc4Unsupported,
    #[error("a colon was expected in custom header {0:?}")]
    InvalidCustomHeader(String),
    #[error("public port 65535 cannot supply the default API port; specify --api-port")]
    ApiPortOverflow,
}

impl TryFrom<Cli> for AppConfig {
    type Error = ConfigError;

    fn try_from(cli: Cli) -> Result<Self, Self::Error> {
        if cli.ssl_allow_rc4 {
            return Err(ConfigError::Rc4Unsupported);
        }
        if cli.error_target.is_some() && cli.error_path.is_some() {
            return Err(ConfigError::ConflictingErrorHandlers);
        }

        validate_pair(&cli.ssl_key, &cli.ssl_cert, "--ssl-key and --ssl-cert")?;
        validate_pair(
            &cli.api_ssl_key,
            &cli.api_ssl_cert,
            "--api-ssl-key and --api-ssl-cert",
        )?;
        validate_pair(
            &cli.client_ssl_key,
            &cli.client_ssl_cert,
            "--client-ssl-key and --client-ssl-cert",
        )?;

        let redirect_port = nonzero_port(cli.redirect_port);
        let redirect_to = nonzero_port(cli.redirect_to);
        if redirect_port.is_some() && (cli.ssl_key.is_none() || cli.ssl_cert.is_none()) {
            return Err(ConfigError::RedirectWithoutTls);
        }

        let public_port = nonzero_port(cli.port).unwrap_or(8000);
        let public_uses_socket = cli.socket.is_some();
        let public_listener = match cli.socket {
            Some(path) => ListenerConfig::Unix(path),
            None => ListenerConfig::Tcp(tcp_address(cli.ip.as_deref().unwrap_or(""), public_port)),
        };

        let api_listener = match cli.api_socket {
            Some(path) => ListenerConfig::Unix(path),
            None => {
                let api_port = if let Some(api_port) = nonzero_port(cli.api_port) {
                    api_port
                } else if public_uses_socket {
                    8001
                } else {
                    public_port
                        .checked_add(1)
                        .ok_or(ConfigError::ApiPortOverflow)?
                };
                ListenerConfig::Tcp(tcp_address(
                    cli.api_ip.as_deref().unwrap_or("localhost"),
                    api_port,
                ))
            }
        };

        let metrics_listener = match (cli.metrics_socket, nonzero_port(cli.metrics_port)) {
            (Some(path), _) => Some(ListenerConfig::Unix(path)),
            (None, Some(port)) => Some(ListenerConfig::Tcp(tcp_address(
                cli.metrics_ip.as_deref().unwrap_or(""),
                port,
            ))),
            (None, None) => None,
        };

        let inherited_tls = InheritedTls {
            protocol: cli.ssl_protocol,
            ciphers: Some(
                cli.ssl_ciphers
                    .unwrap_or_else(|| CHP_5_3_0_DEFAULT_TLS_CIPHERS.to_owned()),
            ),
            dhparam: cli.ssl_dhparam,
        };
        let public_tls = tls_config(
            cli.ssl_key,
            cli.ssl_cert,
            cli.ssl_ca,
            env_nonempty("CONFIGPROXY_SSL_KEY_PASSPHRASE"),
            cli.ssl_request_cert,
            cli.ssl_reject_unauthorized,
            &inherited_tls,
            false,
        );
        let api_tls = tls_config(
            cli.api_ssl_key,
            cli.api_ssl_cert,
            cli.api_ssl_ca,
            env_nonempty("CONFIGPROXY_API_SSL_KEY_PASSPHRASE"),
            cli.api_ssl_request_cert,
            cli.api_ssl_reject_unauthorized,
            &inherited_tls,
            false,
        );
        let client_tls = tls_config(
            cli.client_ssl_key,
            cli.client_ssl_cert,
            cli.client_ssl_ca,
            None,
            cli.client_ssl_request_cert,
            cli.client_ssl_reject_unauthorized,
            &inherited_tls,
            true,
        );

        let default_target = parse_target(cli.default_target, "default target")?;
        let error_target = parse_error_target(cli.error_target)?;
        let log_level = parse_log_level(cli.log_level)?;
        let store = parse_store(cli.storage_backend)?;
        let custom_headers = parse_custom_headers(cli.custom_header)?;

        Ok(Self {
            public_listener,
            api_listener,
            metrics_listener,
            public_tls,
            api_tls,
            client_tls,
            default_target,
            error_target,
            error_path: cli.error_path,
            redirect_port,
            redirect_to,
            pid_file: cli.pid_file,
            auth_token: env_nonempty("CONFIGPROXY_AUTH_TOKEN"),
            log_level,
            store,
            proxy: ProxyOptions {
                x_forward: cli.x_forward,
                prepend_path: cli.prepend_path,
                include_prefix: cli.include_prefix,
                auto_rewrite: cli.auto_rewrite,
                change_origin: cli.change_origin,
                protocol_rewrite: cli.protocol_rewrite,
                custom_headers,
                verify_upstream_tls: !cli.insecure,
                host_routing: cli.host_routing,
                timeout_ms: cli.timeout,
                proxy_timeout_ms: cli.proxy_timeout,
                keep_alive_timeout_ms: Some(
                    cli.keep_alive_timeout
                        .filter(|timeout| *timeout != 0)
                        .unwrap_or(CHP_DEFAULT_KEEP_ALIVE_TIMEOUT_MS),
                ),
            },
        })
    }
}

fn validate_pair<T>(
    key: &Option<T>,
    cert: &Option<T>,
    names: &'static str,
) -> Result<(), ConfigError> {
    if key.is_some() != cert.is_some() {
        Err(ConfigError::IncompleteKeyPair(names))
    } else {
        Ok(())
    }
}

fn nonzero_port(value: Option<u16>) -> Option<u16> {
    value.filter(|port| *port != 0)
}

fn tcp_address(host: &str, port: u16) -> String {
    let host = if host == "*" { "" } else { host };
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

struct InheritedTls {
    protocol: Option<String>,
    ciphers: Option<String>,
    dhparam: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
fn tls_config(
    key: Option<PathBuf>,
    cert: Option<PathBuf>,
    ca: Option<PathBuf>,
    key_passphrase: Option<String>,
    request_cert: bool,
    reject_unauthorized: bool,
    inherited: &InheritedTls,
    ca_activates: bool,
) -> Option<TlsConfig> {
    if key.is_none() && cert.is_none() && (!ca_activates || ca.is_none()) {
        return None;
    }
    Some(TlsConfig {
        key,
        cert,
        ca,
        key_passphrase,
        request_cert,
        reject_unauthorized,
        protocol: inherited.protocol.clone(),
        ciphers: inherited.ciphers.clone(),
        dhparam: inherited.dhparam.clone(),
    })
}

fn parse_target(value: Option<String>, kind: &'static str) -> Result<Option<Url>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let url = Url::parse(&value).map_err(|_| ConfigError::InvalidTarget {
        kind,
        value: value.clone(),
    })?;
    let valid = match url.scheme() {
        "http" | "https" => url.host_str().is_some_and(|host| !host.is_empty()),
        "http+unix" | "unix+http" => {
            is_percent_encoded_socket_host(&url[Position::BeforeHost..Position::AfterPort])
        }
        _ => false,
    };
    if !valid {
        return Err(ConfigError::InvalidTarget { kind, value });
    }
    Ok(Some(url))
}

fn parse_error_target(value: Option<String>) -> Result<Option<Url>, ConfigError> {
    let value = value.map(|mut target| {
        if !target.ends_with('/') {
            target.push('/');
        }
        target
    });
    parse_target(value, "error target")
}

fn is_percent_encoded_socket_host(host: &str) -> bool {
    let bytes = host.as_bytes();
    let mut index = 0;
    let mut has_percent_encoding = false;
    let mut decoded = Vec::with_capacity(bytes.len());
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            decoded.push((high << 4) | low);
            has_percent_encoding = true;
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    has_percent_encoding
        && !decoded.is_empty()
        && !decoded.contains('\0')
        && decoded.starts_with('/')
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn parse_log_level(value: String) -> Result<LogLevel, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "debug" => Ok(LogLevel::Debug),
        "info" => Ok(LogLevel::Info),
        "warn" => Ok(LogLevel::Warn),
        "error" => Ok(LogLevel::Error),
        _ => Err(ConfigError::InvalidLogLevel(value)),
    }
}

fn parse_store(value: Option<String>) -> Result<StoreConfig, ConfigError> {
    match value.as_deref().unwrap_or("memory") {
        "memory" => Ok(StoreConfig::Memory),
        "redis" => Ok(StoreConfig::Redis),
        "sidecar" => Ok(StoreConfig::Sidecar),
        _ => Err(ConfigError::UnknownStorageBackend(
            value.unwrap_or_default(),
        )),
    }
}

fn parse_custom_headers(values: Vec<String>) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut headers = BTreeMap::new();
    for value in values {
        let trimmed = value.trim();
        let Some((name, header_value)) = trimmed.split_once(':') else {
            return Err(ConfigError::InvalidCustomHeader(value));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(ConfigError::InvalidCustomHeader(value));
        }
        headers.insert(name.to_owned(), header_value.trim().to_owned());
    }
    Ok(headers)
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}
