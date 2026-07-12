//! CHP-compatible target parsing, request policy, and Pingora peer construction.

use std::fs;
use std::net::{IpAddr, SocketAddr as StdSocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use http::header::{
    HeaderName, HeaderValue, CONNECTION, HOST, LOCATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, Request, Response, StatusCode, Uri};
use pingora::tls::{pkey::PKey, x509::X509};
use pingora::upstreams::peer::HttpPeer;
use thiserror::Error;
use url::{Position, Url};

use crate::config::ProxyOptions;
use crate::route::RouteKey;

/// A validated upstream target supported by CHP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Tcp {
        host: String,
        port: u16,
        authority: String,
        path: String,
        tls: bool,
    },
    Unix {
        socket_path: PathBuf,
        path: String,
    },
}

/// A matched route with its already validated upstream target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpstreamRoute {
    pub key: RouteKey,
    pub target: Target,
}

impl UpstreamRoute {
    pub fn new(key: RouteKey, target: Target) -> Self {
        Self { key, target }
    }
}

/// TLS and timeout settings applied to a Pingora upstream peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsClientConfig {
    pub verify_cert: bool,
    pub verify_hostname: bool,
    pub connection_timeout: Option<Duration>,
    pub total_connection_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub write_timeout: Option<Duration>,
    pub idle_timeout: Option<Duration>,
    pub ca_file: Option<PathBuf>,
    pub client_certificate: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

/// Per-request transport facts required by http-proxy's `xfwd` policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForwardedContext<'a> {
    pub client_address: &'a str,
    pub port: u16,
    pub protocol: &'a str,
}

impl Default for TlsClientConfig {
    fn default() -> Self {
        Self {
            verify_cert: true,
            verify_hostname: true,
            connection_timeout: None,
            total_connection_timeout: None,
            read_timeout: None,
            write_timeout: None,
            idle_timeout: None,
            ca_file: None,
            client_certificate: None,
            client_key: None,
        }
    }
}

/// A target validation or peer-construction failure.
#[derive(Debug, Error)]
pub enum TargetError {
    #[error("unsupported upstream target scheme {0:?}")]
    UnsupportedScheme(String),
    #[error("upstream target is missing a host")]
    MissingHost,
    #[error("invalid Unix upstream socket host")]
    InvalidUnixSocket,
    #[error("upstream address {0:?} could not be resolved: {1}")]
    AddressResolution(String, #[source] std::io::Error),
    #[error("upstream address {0:?} resolved to no addresses")]
    NoResolvedAddress(String),
    #[error("invalid Unix upstream socket: {0}")]
    UnixPeer(#[source] Box<pingora::Error>),
    #[error("client certificate and key must be configured together")]
    PartialClientIdentity,
    #[error("failed to read TLS {kind} file {path:?}: {source}")]
    TlsFile {
        kind: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TLS CA file {path:?}: {message}")]
    InvalidCa { path: PathBuf, message: String },
    #[error("invalid TLS client certificate file {path:?}: {message}")]
    InvalidClientCertificate { path: PathBuf, message: String },
    #[error("invalid TLS client key file {path:?}: {message}")]
    InvalidClientKey { path: PathBuf, message: String },
}

/// A malformed request transformation.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProxyRequestError {
    #[error("request path does not begin at the matched route prefix boundary")]
    RoutePrefixMismatch,
    #[error("constructed upstream URI is invalid: {0}")]
    InvalidUri(String),
    #[error("invalid custom header name {0:?}")]
    InvalidCustomHeaderName(String),
    #[error("invalid custom header value for {0:?}")]
    InvalidCustomHeaderValue(String),
    #[error("invalid redirect Location header")]
    InvalidLocation,
    #[error("invalid protocol rewrite {0:?}")]
    InvalidProtocolRewrite(String),
}

impl Target {
    /// Parse an HTTP(S) TCP target or an HTTP Unix-socket target.
    pub fn parse(url: &Url) -> Result<Self, TargetError> {
        let path = target_path(url);
        match url.scheme() {
            "http" | "https" => {
                let raw_host = url.host_str().ok_or(TargetError::MissingHost)?;
                let host = raw_host
                    .strip_prefix('[')
                    .and_then(|host| host.strip_suffix(']'))
                    .unwrap_or(raw_host)
                    .to_owned();
                if host.is_empty() {
                    return Err(TargetError::MissingHost);
                }
                let tls = url.scheme() == "https";
                let port = url
                    .port_or_known_default()
                    .ok_or(TargetError::MissingHost)?;
                let authority = authority(url, &host);
                Ok(Self::Tcp {
                    host,
                    port,
                    authority,
                    path,
                    tls,
                })
            }
            "http+unix" | "unix+http" => {
                let encoded = &url[Position::BeforeHost..Position::AfterPort];
                let decoded = percent_decode(encoded).ok_or(TargetError::InvalidUnixSocket)?;
                if decoded.is_empty() || decoded.contains('\0') || !decoded.starts_with('/') {
                    return Err(TargetError::InvalidUnixSocket);
                }
                Ok(Self::Unix {
                    socket_path: PathBuf::from(decoded),
                    path,
                })
            }
            other => Err(TargetError::UnsupportedScheme(other.to_owned())),
        }
    }

    pub fn authority(&self) -> &str {
        match self {
            Self::Tcp { authority, .. } => authority,
            Self::Unix { .. } => "localhost",
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::Tcp { path, .. } | Self::Unix { path, .. } => path,
        }
    }

    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tcp { tls: true, .. })
    }

    pub fn unix_path(&self) -> Option<&Path> {
        match self {
            Self::Unix { socket_path, .. } => Some(socket_path),
            Self::Tcp { .. } => None,
        }
    }

    /// Construct a Pingora peer without using Pingora's panic-prone DNS constructor path.
    pub fn http_peer(&self, tls: &TlsClientConfig) -> Result<HttpPeer, TargetError> {
        if self.is_tls() {
            validate_identity_pair(tls)?;
        }
        let mut peer = match self {
            Self::Tcp {
                host,
                port,
                tls: use_tls,
                ..
            } => {
                let address = resolve_one(host, *port)?;
                HttpPeer::new(address, *use_tls, host.clone())
            }
            Self::Unix { socket_path, .. } => {
                let path = socket_path.to_str().ok_or(TargetError::InvalidUnixSocket)?;
                HttpPeer::new_uds(path, false, String::new()).map_err(TargetError::UnixPeer)?
            }
        };

        peer.options.verify_cert = tls.verify_cert;
        peer.options.verify_hostname = tls.verify_hostname;
        peer.options.connection_timeout = tls.connection_timeout;
        peer.options.total_connection_timeout = tls.total_connection_timeout;
        peer.options.read_timeout = tls.read_timeout;
        peer.options.write_timeout = tls.write_timeout;
        peer.options.idle_timeout = tls.idle_timeout;
        if self.is_tls() {
            if let Some(path) = &tls.ca_file {
                peer.options.ca = Some(Arc::new(load_ca(path)?));
            }
            if let (Some(cert_path), Some(key_path)) = (&tls.client_certificate, &tls.client_key) {
                peer.client_cert_key = Some(Arc::new(load_identity(cert_path, key_path)?));
            }
        }
        Ok(peer)
    }
}

/// Build the origin-form URI sent upstream while retaining raw path/query bytes.
pub fn build_upstream_uri(
    route: &UpstreamRoute,
    request_uri: &Uri,
    options: &ProxyOptions,
) -> Result<Uri, ProxyRequestError> {
    let raw_request = request_uri
        .path_and_query()
        .map_or(request_uri.path(), |path_and_query| path_and_query.as_str());
    let raw_request_path_and_query = if options.include_prefix {
        raw_request
    } else {
        let prefix_units = route.key.as_str().encode_utf16().count();
        raw_request.get(prefix_units..).unwrap_or("")
    };
    let request_path_and_query = get_path(raw_request_path_and_query)?;

    let path_and_query = if options.prepend_path {
        url_join(route.target.path(), &request_path_and_query)
    } else {
        url_join("/", &request_path_and_query)
    };
    path_and_query
        .parse()
        .map_err(|error: http::uri::InvalidUri| ProxyRequestError::InvalidUri(error.to_string()))
}

fn get_path(raw: &str) -> Result<String, ProxyRequestError> {
    if raw.is_empty() || raw.starts_with('?') {
        return Ok(raw.to_owned());
    }
    let absolute = if raw.starts_with("//") {
        format!("http://base.invalid{raw}")
    } else {
        format!("http://base.invalid/{}", raw.trim_start_matches('/'))
    };
    let parsed =
        Url::parse(&absolute).map_err(|error| ProxyRequestError::InvalidUri(error.to_string()))?;
    Ok(match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_owned(),
    })
}

/// Apply CHP/http-proxy request header policy in-place.
pub fn apply_request_headers(
    headers: &mut HeaderMap,
    target: &Target,
    options: &ProxyOptions,
) -> Result<(), ProxyRequestError> {
    let original_host = headers.get(HOST).cloned();
    let origin_host = if options.change_origin {
        Some(
            HeaderValue::from_str(target.authority())
                .map_err(|_| ProxyRequestError::InvalidCustomHeaderValue("host".to_owned()))?,
        )
    } else {
        None
    };
    let mut custom_headers = Vec::with_capacity(options.custom_headers.len());
    for (name, value) in &options.custom_headers {
        let parsed_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ProxyRequestError::InvalidCustomHeaderName(name.clone()))?;
        let parsed_value = HeaderValue::from_str(value)
            .map_err(|_| ProxyRequestError::InvalidCustomHeaderValue(name.clone()))?;
        custom_headers.push((parsed_name, parsed_value));
    }

    let websocket_upgrade = is_websocket_upgrade(headers).then(|| headers[UPGRADE].clone());
    remove_hop_by_hop(headers, websocket_upgrade.is_some());

    if options.x_forward && !headers.contains_key("x-forwarded-host") {
        if let Some(host) = original_host.as_ref() {
            headers.insert("x-forwarded-host", host.clone());
        }
    }
    if let Some(host) = origin_host {
        headers.insert(HOST, host);
    }
    for (parsed_name, parsed_value) in custom_headers {
        headers.insert(parsed_name, parsed_value);
    }
    remove_hop_by_hop(headers, false);
    if let Some(upgrade) = websocket_upgrade {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(UPGRADE, upgrade);
    }
    Ok(())
}

/// Apply the connection-aware part of http-proxy's `xfwd` pass.
pub fn apply_forwarded_headers(
    headers: &mut HeaderMap,
    context: &ForwardedContext<'_>,
    options: &ProxyOptions,
) -> Result<(), ProxyRequestError> {
    if !options.x_forward {
        return Ok(());
    }

    let values = [
        ("x-forwarded-for", context.client_address.to_owned()),
        ("x-forwarded-port", context.port.to_string()),
        ("x-forwarded-proto", context.protocol.to_owned()),
    ];
    let mut parsed = Vec::with_capacity(values.len());
    for (name, value) in values {
        let mut parts = headers
            .get_all(name)
            .iter()
            .map(|existing| {
                existing
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| ProxyRequestError::InvalidCustomHeaderValue(name.to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        parts.push(value);
        let combined = parts.join(",");
        let value = HeaderValue::from_str(&combined)
            .map_err(|_| ProxyRequestError::InvalidCustomHeaderValue(name.to_owned()))?;
        parsed.push((HeaderName::from_static(name), value));
    }
    let forwarded_host = if headers.contains_key("x-forwarded-host") {
        None
    } else {
        Some(
            headers
                .get(HOST)
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("")),
        )
    };

    for (name, value) in parsed {
        headers.insert(name, value);
    }
    if let Some(host) = forwarded_host {
        headers.insert("x-forwarded-host", host);
    }
    Ok(())
}

/// Rewrite an upstream redirect Location exactly when http-proxy would do so.
pub fn rewrite_location<B, R>(
    response: &mut Response<B>,
    request: &Request<R>,
    target: &Target,
    options: &ProxyOptions,
) -> Result<(), ProxyRequestError> {
    if !is_redirect_rewrite_status(response.status())
        || (!options.auto_rewrite && options.protocol_rewrite.is_none())
    {
        return Ok(());
    }
    let Some(location) = response.headers().get(LOCATION) else {
        return Ok(());
    };
    let location = location
        .to_str()
        .map_err(|_| ProxyRequestError::InvalidLocation)?;
    let Ok(mut parsed) = Url::parse(location) else {
        return Ok(());
    };
    if url_host(&parsed).as_deref() != Some(target.authority()) {
        return Ok(());
    }

    if options.auto_rewrite {
        if let Some(host) = request.headers().get(HOST) {
            let host = host
                .to_str()
                .map_err(|_| ProxyRequestError::InvalidLocation)?;
            parsed
                .set_host(Some(host_name(host)))
                .map_err(|_| ProxyRequestError::InvalidLocation)?;
            parsed
                .set_port(host_port(host))
                .map_err(|_| ProxyRequestError::InvalidLocation)?;
        }
    }
    if let Some(protocol) = &options.protocol_rewrite {
        if let Some(protocol) = whatwg_scheme(protocol) {
            let _ = parsed.set_scheme(protocol);
        }
    }
    let value =
        HeaderValue::from_str(parsed.as_str()).map_err(|_| ProxyRequestError::InvalidLocation)?;
    response.headers_mut().insert(LOCATION, value);
    Ok(())
}

fn target_path(url: &Url) -> String {
    let mut path = if url.path().is_empty() {
        "/".to_owned()
    } else {
        url.path().to_owned()
    };
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    path
}

fn authority(url: &Url, host: &str) -> String {
    let display_host = if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    match url.port() {
        Some(port) => format!("{display_host}:{port}"),
        None => display_host,
    }
}

fn url_host(url: &Url) -> Option<String> {
    url.host_str().map(|host| authority(url, host))
}

fn whatwg_scheme(value: &str) -> Option<&str> {
    let scheme = value.split_once(':').map_or(value, |(scheme, _)| scheme);
    let mut bytes = scheme.bytes();
    bytes.next()?.is_ascii_alphabetic().then_some(())?;
    bytes
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        .then_some(scheme)
}

fn url_join(left: &str, right: &str) -> String {
    let (left_path, left_query) = split_query(left);
    let (right_path, right_query) = split_query(right);
    let mut joined = String::new();
    for segment in [left_path, right_path] {
        if segment.is_empty() {
            continue;
        }
        if joined.ends_with('/') && segment.starts_with('/') {
            joined.push_str(&segment[1..]);
        } else if joined.is_empty() || joined.ends_with('/') || segment.starts_with('/') {
            joined.push_str(segment);
        } else {
            joined.push('/');
            joined.push_str(segment);
        }
    }
    let queries: Vec<&str> = [left_query, right_query]
        .into_iter()
        .flatten()
        .filter(|query| !query.is_empty())
        .collect();
    if !queries.is_empty() {
        joined.push('?');
        joined.push_str(&queries.join("&"));
    }
    joined
}

fn split_query(value: &str) -> (&str, Option<&str>) {
    value
        .split_once('?')
        .map_or((value, None), |(path, query)| (path, Some(query)))
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let websocket = headers
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    websocket && connection_tokens(headers).any(|token| token.eq_ignore_ascii_case("upgrade"))
}

fn connection_tokens(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn remove_hop_by_hop(headers: &mut HeaderMap, websocket: bool) {
    let named: Vec<HeaderName> = connection_tokens(headers)
        .filter(|token| !(websocket && token.eq_ignore_ascii_case("upgrade")))
        .filter_map(|token| HeaderName::from_bytes(token.as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
        HeaderName::from_static("proxy-authorization"),
        HeaderName::from_static("proxy-authenticate"),
        TE,
        TRAILER,
        TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }
    if websocket {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.remove(CONNECTION);
        headers.remove(UPGRADE);
    }
}

fn is_redirect_rewrite_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 201 | 301 | 302 | 307 | 308)
}

fn host_name(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .split_once(']')
            .map_or(authority, |(host, _)| &authority[..=host.len()]);
    }
    authority
        .split_once(':')
        .map_or(authority, |(host, _)| host)
}

fn host_port(authority: &str) -> Option<u16> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed
            .split_once(']')
            .and_then(|(_, rest)| rest.strip_prefix(':'))
            .and_then(|port| port.parse().ok());
    }
    authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
}

fn percent_decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn resolve_one(host: &str, port: u16) -> Result<StdSocketAddr, TargetError> {
    let display = format!("{host}:{port}");
    (host, port)
        .to_socket_addrs()
        .map_err(|error| TargetError::AddressResolution(display.clone(), error))?
        .next()
        .ok_or(TargetError::NoResolvedAddress(display))
}

fn validate_identity_pair(tls: &TlsClientConfig) -> Result<(), TargetError> {
    if tls.client_certificate.is_some() != tls.client_key.is_some() {
        Err(TargetError::PartialClientIdentity)
    } else {
        Ok(())
    }
}

fn read_tls(path: &Path, kind: &'static str) -> Result<Vec<u8>, TargetError> {
    fs::read(path).map_err(|source| TargetError::TlsFile {
        kind,
        path: path.to_owned(),
        source,
    })
}

fn load_ca(path: &Path) -> Result<Box<[X509]>, TargetError> {
    let bytes = read_tls(path, "CA")?;
    let certificates = X509::stack_from_pem(&bytes).map_err(|error| TargetError::InvalidCa {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if certificates.is_empty() {
        return Err(TargetError::InvalidCa {
            path: path.to_owned(),
            message: "no certificates found".to_owned(),
        });
    }
    Ok(certificates.into_boxed_slice())
}

fn load_identity(
    certificate_path: &Path,
    key_path: &Path,
) -> Result<pingora::utils::tls::CertKey, TargetError> {
    let certificate_bytes = read_tls(certificate_path, "client certificate")?;
    let certificates = X509::stack_from_pem(&certificate_bytes).map_err(|error| {
        TargetError::InvalidClientCertificate {
            path: certificate_path.to_owned(),
            message: error.to_string(),
        }
    })?;
    if certificates.is_empty() {
        return Err(TargetError::InvalidClientCertificate {
            path: certificate_path.to_owned(),
            message: "no certificates found".to_owned(),
        });
    }
    let key_bytes = read_tls(key_path, "client key")?;
    let key =
        PKey::private_key_from_pem(&key_bytes).map_err(|error| TargetError::InvalidClientKey {
            path: key_path.to_owned(),
            message: error.to_string(),
        })?;
    Ok(pingora::utils::tls::CertKey::new(certificates, key))
}
