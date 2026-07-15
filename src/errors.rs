//! CHP-compatible proxy error rendering.

use std::fs::File;
use std::future::Future;
use std::io;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioIo;
use pingora::http::ResponseHeader;
use pingora::proxy::Session;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tower::Service as _;
use url::Url;

use crate::config::TlsConfig;
use crate::upstream::Target;

const ERROR_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const ERROR_READ_TIMEOUT: Duration = Duration::from_millis(500);
const ERROR_TOTAL_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ERROR_HEADER_BYTES: usize = 16 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;
const MAX_UNIX_WIRE_BYTES: usize = MAX_ERROR_HEADER_BYTES + MAX_ERROR_BODY_BYTES + 64 * 1024;
const MAX_RESOLVED_ADDRESSES: usize = 16;
const RESOLVER_QUEUE_CAPACITY: usize = 8;
const RESOLVER_CACHE_TTL: Duration = Duration::from_secs(30);
const CHP_404_HTML: &[u8] = b"<!doctype html>\n<html>\n  <head>\n    <meta charset=\"utf-8\" />\n    <title>404: Not Found</title>\n  </head>\n\n  <body>\n    <h1>404: Not Found</h1>\n    <p>No service is registered at this URL</p>\n    <hr />\n    <p>configurable-http-proxy</p>\n  </body>\n</html>\n";
const CHP_503_HTML: &[u8] = b"<!doctype html>\n<html>\n  <head>\n    <meta charset=\"utf-8\" />\n    <title>503: Proxy Target Missing</title>\n  </head>\n\n  <body>\n    <h1>503: Proxy Target Missing</h1>\n    <p>The upstream service is unavailable</p>\n    <hr />\n    <p>configurable-http-proxy</p>\n  </body>\n</html>\n";

/// Classification used to select the public HTTP status without exposing an
/// internal error or target URL to clients and logs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyErrorClass {
    RouteMiss,
    UnavailableUpstream,
    Internal,
}

impl ProxyErrorClass {
    pub fn status(self) -> StatusCode {
        match self {
            Self::RouteMiss => StatusCode::NOT_FOUND,
            Self::UnavailableUpstream => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

struct RenderedError {
    body: Bytes,
    content_type: Option<HeaderValue>,
    content_encoding: Option<HeaderValue>,
}

type LookupFuture = Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>;
type Lookup = Arc<dyn Fn(String) -> LookupFuture + Send + Sync>;

struct ResolveRequest {
    response: oneshot::Sender<io::Result<CachedAddresses>>,
}

fn resolver_batch_count(receiver: &mpsc::Receiver<ResolveRequest>) -> usize {
    receiver
        .len()
        .min(RESOLVER_QUEUE_CAPACITY.saturating_sub(1))
}

fn take_resolver_batch(
    first: ResolveRequest,
    receiver: &mut mpsc::Receiver<ResolveRequest>,
    waiting: usize,
) -> Vec<ResolveRequest> {
    let mut batch = Vec::with_capacity(waiting + 1);
    batch.push(first);
    for _ in 0..waiting {
        batch.push(
            receiver
                .try_recv()
                .expect("captured resolver batch entries must remain queued"),
        );
    }
    batch
}

#[derive(Clone)]
struct CachedAddresses {
    addresses: Vec<SocketAddr>,
    generation: u64,
    expires_at: Instant,
}

#[derive(Default)]
struct ResolverCache {
    current: Option<CachedAddresses>,
    generation: u64,
    leased_generation: Option<u64>,
}

impl ResolverCache {
    fn fresh(&mut self) -> Option<CachedAddresses> {
        if self.current.as_ref().is_some_and(|cached| {
            cached.expires_at <= Instant::now() && self.leased_generation != Some(cached.generation)
        }) {
            self.current = None;
        }
        self.current.clone()
    }

    fn lease(&mut self, generation: u64) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|cached| cached.generation == generation)
            && self.leased_generation.is_none()
        {
            self.leased_generation = Some(generation);
            true
        } else {
            false
        }
    }

    fn finish_lease(&mut self, generation: u64, succeeded: bool) {
        if self.leased_generation == Some(generation) {
            self.leased_generation = None;
        }
        if !succeeded
            && self
                .current
                .as_ref()
                .is_some_and(|cached| cached.generation == generation)
        {
            self.current = None;
        }
    }
}

#[derive(Clone)]
struct BoundedResolver {
    host: Arc<str>,
    cache: Arc<std::sync::Mutex<ResolverCache>>,
    requests: mpsc::Sender<ResolveRequest>,
}

impl std::fmt::Debug for BoundedResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("BoundedResolver").finish()
    }
}

impl BoundedResolver {
    fn start_with_lookup(host: &str, lookup: Lookup) -> Self {
        let host: Arc<str> = Arc::from(host);
        let cache = Arc::new(std::sync::Mutex::new(ResolverCache::default()));
        let (requests, mut receiver) = mpsc::channel::<ResolveRequest>(RESOLVER_QUEUE_CAPACITY);
        let worker_host = Arc::clone(&host);
        let worker_cache = Arc::clone(&cache);
        tokio::spawn(async move {
            while let Some(first) = receiver.recv().await {
                let cached = worker_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .fresh();
                let result = if let Some(cached) = cached {
                    Ok(cached)
                } else {
                    lookup(worker_host.to_string()).await.and_then(|addresses| {
                        let mut addresses: Vec<_> =
                            addresses.into_iter().take(MAX_RESOLVED_ADDRESSES).collect();
                        addresses.dedup();
                        if addresses.is_empty() {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                "custom-error hostname resolved to no addresses",
                            ))
                        } else {
                            let mut cache = worker_cache
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            cache.generation = cache.generation.wrapping_add(1);
                            let cached = CachedAddresses {
                                addresses,
                                generation: cache.generation,
                                expires_at: Instant::now() + RESOLVER_CACHE_TTL,
                            };
                            cache.current = Some(cached.clone());
                            Ok(cached)
                        }
                    })
                };
                let waiting = resolver_batch_count(&receiver);
                for request in take_resolver_batch(first, &mut receiver, waiting) {
                    let response = match &result {
                        Ok(cached) => Ok(cached.clone()),
                        Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
                    };
                    let _ = request.response.send(response);
                }
            }
        });
        Self {
            host,
            cache,
            requests,
        }
    }

    async fn ensure_cached(&self) -> io::Result<u64> {
        {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(cached) = cache.fresh() {
                if cache.lease(cached.generation) {
                    return Ok(cached.generation);
                }
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "custom-error resolver generation unavailable",
                ));
            }
        }
        let (response, received) = oneshot::channel();
        self.requests
            .try_send(ResolveRequest { response })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "custom-error resolver queue unavailable",
                ),
                mpsc::error::TrySendError::Closed(_) => io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "custom-error resolver queue unavailable",
                ),
            })?;
        let cached = received.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "custom-error resolver worker unavailable",
            )
        })??;
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.lease(cached.generation) {
            Ok(cached.generation)
        } else {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "custom-error resolver generation changed",
            ))
        }
    }

    fn finish_lease(&self, generation: u64, succeeded: bool) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .finish_lease(generation, succeeded);
    }
}

impl tower::Service<Name> for BoundedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        if name.as_str() != self.host.as_ref() {
            return Box::pin(std::future::ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "custom-error resolver received an unexpected hostname",
            ))));
        }
        if let Some(addresses) = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .fresh()
        {
            return Box::pin(std::future::ready(Ok(addresses.addresses.into_iter())));
        }
        let (response, received) = oneshot::channel();
        if let Err(error) = self.requests.try_send(ResolveRequest { response }) {
            let kind = match error {
                mpsc::error::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                mpsc::error::TrySendError::Closed(_) => io::ErrorKind::BrokenPipe,
            };
            return Box::pin(std::future::ready(Err(io::Error::new(
                kind,
                "custom-error resolver queue unavailable",
            ))));
        }
        Box::pin(async move {
            let cached = received.await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "custom-error resolver worker unavailable",
                )
            })??;
            Ok(cached.addresses.into_iter())
        })
    }
}

type BoundedErrorConnector = HttpsConnector<HttpConnector<BoundedResolver>>;

struct ResolverAttempt {
    resolver: BoundedResolver,
    generation: u64,
    succeeded: bool,
}

impl ResolverAttempt {
    fn new(resolver: BoundedResolver, generation: u64) -> Self {
        Self {
            resolver,
            generation,
            succeeded: false,
        }
    }

    fn succeed(&mut self) {
        self.succeeded = true;
    }
}

impl Drop for ResolverAttempt {
    fn drop(&mut self) {
        self.resolver.finish_lease(self.generation, self.succeeded);
    }
}

/// Renders custom-target, file, and reason-phrase proxy errors in CHP order.
#[derive(Clone)]
pub struct ProxyErrorRenderer {
    connector: BoundedErrorConnector,
    resolver: BoundedResolver,
    request_serialization: Arc<tokio::sync::Mutex<()>>,
    error_target: Option<Url>,
    error_path: Option<PathBuf>,
}

/// A typed, redacted failure while configuring the custom-error HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum ErrorRendererBuildError {
    #[error("failed to read custom-error TLS {kind}")]
    ReadTls {
        kind: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid custom-error TLS CA certificate bundle")]
    InvalidCa,
    #[error("invalid custom-error TLS client identity")]
    InvalidIdentity,
    #[error("failed to build custom-error HTTP client")]
    Client,
}

#[derive(Debug)]
struct NoCertificateVerification {
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

impl ProxyErrorRenderer {
    /// Build a renderer using the proxy's complete upstream TLS policy.
    pub fn with_tls_policy(
        error_target: Option<Url>,
        error_path: Option<PathBuf>,
        verify_tls: bool,
        tls: Option<&TlsConfig>,
    ) -> Result<Self, ErrorRendererBuildError> {
        Self::with_tls_policy_and_lookup(
            error_target,
            error_path,
            verify_tls,
            tls,
            Arc::new(|host| {
                Box::pin(async move {
                    tokio::net::lookup_host((host.as_str(), 0))
                        .await
                        .map(|addresses| addresses.collect())
                })
            }),
        )
    }

    fn with_tls_policy_and_lookup(
        error_target: Option<Url>,
        error_path: Option<PathBuf>,
        verify_tls: bool,
        tls: Option<&TlsConfig>,
        lookup: Lookup,
    ) -> Result<Self, ErrorRendererBuildError> {
        let mut roots = rustls::RootCertStore::empty();
        if let Some(path) = tls.and_then(|tls| tls.ca.as_ref()) {
            let pem = std::fs::read(path).map_err(|source| ErrorRendererBuildError::ReadTls {
                kind: "CA certificate",
                source,
            })?;
            let certificates = CertificateDer::pem_slice_iter(&pem)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| ErrorRendererBuildError::InvalidCa)?;
            if certificates.is_empty() {
                return Err(ErrorRendererBuildError::InvalidCa);
            }
            for certificate in certificates {
                roots
                    .add(certificate)
                    .map_err(|_| ErrorRendererBuildError::InvalidCa)?;
            }
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config_builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|_| ErrorRendererBuildError::Client)?;
        let config_builder = if verify_tls {
            config_builder.with_root_certificates(roots)
        } else {
            config_builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertificateVerification {
                    schemes: provider
                        .signature_verification_algorithms
                        .supported_schemes(),
                }))
        };
        let mut client_identity = None;
        if let Some(tls) = tls {
            if let (Some(certificate), Some(key)) = (&tls.cert, &tls.key) {
                let pem = std::fs::read(certificate).map_err(|source| {
                    ErrorRendererBuildError::ReadTls {
                        kind: "client certificate",
                        source,
                    }
                })?;
                let certificates = CertificateDer::pem_slice_iter(&pem)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| ErrorRendererBuildError::InvalidIdentity)?;
                let key_pem =
                    std::fs::read(key).map_err(|source| ErrorRendererBuildError::ReadTls {
                        kind: "client key",
                        source,
                    })?;
                let private_key = PrivateKeyDer::from_pem_slice(&key_pem)
                    .map_err(|_| ErrorRendererBuildError::InvalidIdentity)?;
                if certificates.is_empty() {
                    return Err(ErrorRendererBuildError::InvalidIdentity);
                }
                client_identity = Some((certificates, private_key));
            }
        }
        let tls_config = if let Some((certificates, private_key)) = client_identity {
            config_builder
                .with_client_auth_cert(certificates, private_key)
                .map_err(|_| ErrorRendererBuildError::InvalidIdentity)?
        } else {
            config_builder.with_no_client_auth()
        };
        let resolver_host = error_target
            .as_ref()
            .and_then(Url::host_str)
            .unwrap_or("localhost");
        let resolver = BoundedResolver::start_with_lookup(resolver_host, lookup);
        let mut connector = HttpConnector::new_with_resolver(resolver.clone());
        connector.set_connect_timeout(Some(ERROR_CONNECT_TIMEOUT));
        connector.enforce_http(false);
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config)
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        Ok(Self {
            connector,
            resolver,
            request_serialization: Arc::new(tokio::sync::Mutex::new(())),
            error_target,
            error_path,
        })
    }

    pub async fn respond(
        &self,
        session: &mut Session,
        status: StatusCode,
        original_uri: &str,
    ) -> pingora::Result<()> {
        let rendered = self.render(status, original_uri).await;
        let mut response = ResponseHeader::build(status.as_u16(), Some(3))?;
        response.set_content_length(rendered.body.len())?;
        if let Some(value) = rendered.content_type {
            response.insert_header(CONTENT_TYPE, value)?;
        }
        if let Some(value) = rendered.content_encoding {
            response.insert_header(CONTENT_ENCODING, value)?;
        }
        session
            .as_downstream_mut()
            .write_error_response(response, rendered.body)
            .await
    }

    async fn render(&self, status: StatusCode, original_uri: &str) -> RenderedError {
        if let Some(target) = &self.error_target {
            if let Some(rendered) = self.custom_error(target, status, original_uri).await {
                return rendered;
            }
            return reason_phrase(status);
        }

        if let Some(path) = &self.error_path {
            for filename in [format!("{}.html", status.as_u16()), "error.html".to_owned()] {
                if let Some(body) = read_bounded_file(path.clone(), filename).await {
                    return RenderedError {
                        body,
                        content_type: Some(HeaderValue::from_static("text/html")),
                        content_encoding: None,
                    };
                }
            }
        } else {
            let body = match status {
                StatusCode::NOT_FOUND => CHP_404_HTML,
                StatusCode::SERVICE_UNAVAILABLE => CHP_503_HTML,
                _ => return reason_phrase(status),
            };
            return RenderedError {
                body: Bytes::from_static(body),
                content_type: Some(HeaderValue::from_static("text/html")),
                content_encoding: None,
            };
        }
        reason_phrase(status)
    }

    async fn custom_error(
        &self,
        target: &Url,
        status: StatusCode,
        original_uri: &str,
    ) -> Option<RenderedError> {
        let url = custom_error_url(target, status, original_uri)?;

        if matches!(url.scheme(), "http+unix" | "unix+http") {
            return unix_custom_error(&url).await;
        }
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let deadline = tokio::time::Instant::now() + ERROR_TOTAL_TIMEOUT;
        tokio::time::timeout_at(deadline, self.bounded_request(url, deadline))
            .await
            .ok()?
    }

    async fn bounded_request(
        &self,
        url: Url,
        deadline: tokio::time::Instant,
    ) -> Option<RenderedError> {
        let _generation_lease =
            tokio::time::timeout_at(deadline, self.request_serialization.lock())
                .await
                .ok()?;
        let generation = tokio::time::timeout_at(deadline, self.resolver.ensure_cached())
            .await
            .ok()?
            .ok()?;
        let mut attempt = ResolverAttempt::new(self.resolver.clone(), generation);
        let rendered = self.bounded_request_inner(url, deadline).await;
        if rendered.is_some() {
            attempt.succeed();
        }
        rendered
    }

    async fn bounded_request_inner(
        &self,
        url: Url,
        deadline: tokio::time::Instant,
    ) -> Option<RenderedError> {
        let uri = url.as_str().parse().ok()?;
        let mut connector = self.connector.clone();
        let stream = connector.call(uri).await.ok()?;
        let mut stream = TokioIo::new(stream);
        let request_target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        let request = format!(
            "GET {request_target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
        );
        write_all_and_flush(&mut stream, request.as_bytes(), deadline)
            .await
            .ok()?;

        let mut response = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let count = tokio::time::timeout(ERROR_READ_TIMEOUT, stream.read(&mut chunk))
                .await
                .ok()?
                .ok()?;
            if count == 0 {
                return parse_http_response(&response, true);
            }
            if response.len().saturating_add(count) > MAX_UNIX_WIRE_BYTES {
                return None;
            }
            response.extend_from_slice(&chunk[..count]);
            if !response.windows(4).any(|bytes| bytes == b"\r\n\r\n")
                && response.len() > MAX_ERROR_HEADER_BYTES
            {
                return None;
            }
            if let Some(rendered) = parse_http_response(&response, false) {
                return Some(rendered);
            }
        }
    }
}

async fn write_all_and_flush<W: tokio::io::AsyncWrite + Unpin>(
    stream: &mut W,
    bytes: &[u8],
    deadline: tokio::time::Instant,
) -> io::Result<()> {
    tokio::time::timeout_at(deadline, stream.write_all(bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "custom-error write timed out"))??;
    tokio::time::timeout_at(deadline, stream.flush())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "custom-error flush timed out"))?
}

#[cfg(unix)]
async fn unix_custom_error(url: &Url) -> Option<RenderedError> {
    tokio::time::timeout(ERROR_TOTAL_TIMEOUT, unix_custom_error_inner(url))
        .await
        .ok()?
}

#[cfg(unix)]
async fn unix_custom_error_inner(url: &Url) -> Option<RenderedError> {
    let target = Target::parse(url).ok()?;
    let socket_path = target.unix_path()?;
    let mut stream = tokio::time::timeout(
        ERROR_CONNECT_TIMEOUT,
        tokio::net::UnixStream::connect(socket_path),
    )
    .await
    .ok()?
    .ok()?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        target.path()
    );
    tokio::time::timeout(ERROR_READ_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .ok()?
        .ok()?;
    let mut response = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = tokio::time::timeout(ERROR_READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if count == 0 {
            return parse_http_response(&response, true);
        }
        if response.len().saturating_add(count) > MAX_UNIX_WIRE_BYTES {
            return None;
        }
        response.extend_from_slice(&chunk[..count]);
        if !response.windows(4).any(|bytes| bytes == b"\r\n\r\n")
            && response.len() > MAX_ERROR_HEADER_BYTES
        {
            return None;
        }
        if let Some(rendered) = parse_http_response(&response, false) {
            return Some(rendered);
        }
    }
}

#[cfg(not(unix))]
async fn unix_custom_error(_url: &Url) -> Option<RenderedError> {
    None
}

fn parse_http_response(response: &[u8], eof: bool) -> Option<RenderedError> {
    let header_end = response.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    if header_end.saturating_add(4) > MAX_ERROR_HEADER_BYTES {
        return None;
    }
    let mut raw_headers = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Response::new(&mut raw_headers);
    match parsed.parse(&response[..header_end + 4]).ok()? {
        httparse::Status::Complete(consumed) if consumed == header_end + 4 => {}
        _ => return None,
    }
    parsed.code?;
    let mut content_type = None;
    let mut content_encoding = None;
    let mut chunked = false;
    let mut content_length = None;
    for header in parsed.headers {
        if header.name.eq_ignore_ascii_case("content-type") {
            content_type = HeaderValue::from_bytes(header.value).ok();
        } else if header.name.eq_ignore_ascii_case("content-encoding") {
            content_encoding = HeaderValue::from_bytes(header.value).ok();
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !header.value.eq_ignore_ascii_case(b"chunked") {
                return None;
            }
            chunked = true;
        } else if header.name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return None;
            }
            let length = std::str::from_utf8(header.value)
                .ok()?
                .parse::<usize>()
                .ok()?;
            if length > MAX_ERROR_BODY_BYTES {
                return None;
            }
            content_length = Some(length);
        }
    }
    if chunked && content_length.is_some() {
        return None;
    }
    let raw_body = &response[header_end + 4..];
    let body = if chunked {
        decode_chunked(raw_body)?
    } else if let Some(length) = content_length {
        raw_body.get(..length)?.to_vec()
    } else if eof && raw_body.len() <= MAX_ERROR_BODY_BYTES {
        raw_body.to_vec()
    } else {
        return None;
    };
    Some(RenderedError {
        body: Bytes::from(body),
        content_type,
        content_encoding,
    })
}

fn decode_chunked(mut input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let line_end = input.windows(2).position(|bytes| bytes == b"\r\n")?;
        let size = std::str::from_utf8(&input[..line_end])
            .ok()?
            .split(';')
            .next()
            .and_then(|value| usize::from_str_radix(value.trim(), 16).ok())?;
        input = &input[line_end + 2..];
        if size == 0 {
            return input.starts_with(b"\r\n").then_some(output);
        }
        let chunk = input.get(..size)?;
        output.extend_from_slice(chunk);
        if output.len() > MAX_ERROR_BODY_BYTES {
            return None;
        }
        input = input.get(size..)?;
        input = input.strip_prefix(b"\r\n")?;
    }
}

fn custom_error_url(target: &Url, status: StatusCode, original_uri: &str) -> Option<Url> {
    let literal = format!(
        "{}{status}?url={}",
        target.as_str(),
        encode_uri_component(original_uri),
        status = status.as_u16()
    );
    Url::parse(&literal).ok()
}

fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

async fn read_bounded_file(root: PathBuf, filename: String) -> Option<Bytes> {
    tokio::task::spawn_blocking(move || read_bounded_file_sync(&root, &filename))
        .await
        .ok()?
}

fn read_bounded_file_sync(root: &Path, filename: &str) -> Option<Bytes> {
    read_bounded_file_sync_with_hook(root, filename, || {})
}

fn read_bounded_file_sync_with_hook(
    root: &Path,
    filename: &str,
    after_root_open: impl FnOnce(),
) -> Option<Bytes> {
    if Path::new(filename).components().count() != 1 {
        return None;
    }
    let mut file = open_bounded_regular_file(root, filename, after_root_open)?;
    if file.metadata().ok()?.len() > MAX_ERROR_BODY_BYTES as u64 {
        return None;
    }
    let mut body = Vec::new();
    file.by_ref()
        .take(MAX_ERROR_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() <= MAX_ERROR_BODY_BYTES).then(|| Bytes::from(body))
}

#[cfg(unix)]
fn open_bounded_regular_file(
    root: &Path,
    filename: &str,
    after_root_open: impl FnOnce(),
) -> Option<File> {
    use std::path::Component;

    use rustix::fs::{fstat, open, openat, FileType, Mode, OFlags};

    let mut directory = open(
        if root.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        },
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    after_root_open();
    for component in root.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                directory = openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .ok()?;
            }
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let stat_flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let stat_flags = OFlags::from_bits_retain(libc::O_EVTONLY as u32)
        | OFlags::CLOEXEC
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    return None;

    let stat_only = openat(&directory, filename, stat_flags, Mode::empty()).ok()?;
    let before = fstat(&stat_only).ok()?;
    if !FileType::from_raw_mode(before.st_mode).is_file()
        || before.st_size < 0
        || before.st_size as u64 > MAX_ERROR_BODY_BYTES as u64
    {
        return None;
    }
    let readable = openat(
        &directory,
        filename,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let after = fstat(&readable).ok()?;
    if before.st_dev != after.st_dev
        || before.st_ino != after.st_ino
        || !FileType::from_raw_mode(after.st_mode).is_file()
    {
        return None;
    }
    Some(File::from(readable))
}

#[cfg(not(unix))]
fn open_bounded_regular_file(
    _root: &Path,
    _filename: &str,
    after_root_open: impl FnOnce(),
) -> Option<File> {
    after_root_open();
    // Secure descriptor-relative, no-follow traversal is implemented for the
    // supported macOS/Linux/Unix deployment targets. Other platforms fail
    // closed instead of falling back to a pathname check/open sequence.
    None
}

fn reason_phrase(status: StatusCode) -> RenderedError {
    RenderedError {
        body: Bytes::copy_from_slice(
            status
                .canonical_reason()
                .unwrap_or("Unknown Error")
                .as_bytes(),
        ),
        content_type: None,
        content_encoding: None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use hyper_util::client::legacy::connect::dns::Name;
    use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tower::Service as _;
    use url::Url;

    use super::{
        encode_uri_component, parse_http_response, read_bounded_file_sync_with_hook,
        resolver_batch_count, take_resolver_batch, write_all_and_flush, BoundedResolver,
        ErrorRendererBuildError, ProxyErrorRenderer, MAX_RESOLVED_ADDRESSES,
        RESOLVER_QUEUE_CAPACITY,
    };
    use crate::config::TlsConfig;

    struct FlushRequiredWriter {
        flushed: bool,
    }

    fn resolve_request() -> super::ResolveRequest {
        let (response, _received) = tokio::sync::oneshot::channel();
        super::ResolveRequest { response }
    }

    #[test]
    fn resolver_batch_snapshot_leaves_adversarial_refill_for_the_next_turn() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(RESOLVER_QUEUE_CAPACITY);
        sender.try_send(resolve_request()).unwrap();
        for _ in 0..2 {
            sender.try_send(resolve_request()).unwrap();
        }

        let first = receiver.try_recv().unwrap();
        let captured = resolver_batch_count(&receiver);
        for _ in 0..(RESOLVER_QUEUE_CAPACITY - 2) {
            sender.try_send(resolve_request()).unwrap();
        }
        assert_eq!(receiver.len(), RESOLVER_QUEUE_CAPACITY);

        let batch = take_resolver_batch(first, &mut receiver, captured);
        assert_eq!(batch.len(), 3);
        assert!(batch.len() <= RESOLVER_QUEUE_CAPACITY);
        assert_eq!(receiver.len(), RESOLVER_QUEUE_CAPACITY - 2);

        let next = receiver.try_recv().unwrap();
        let next_captured = resolver_batch_count(&receiver);
        let next_batch = take_resolver_batch(next, &mut receiver, next_captured);
        assert_eq!(next_batch.len(), RESOLVER_QUEUE_CAPACITY - 2);
        assert!(receiver.is_empty());
    }

    #[tokio::test]
    async fn resolver_generation_lease_pins_addresses_across_cache_expiry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let lookup: super::Lookup = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_host: String| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)]) })
            })
        };
        let resolver = BoundedResolver::start_with_lookup("errors.invalid", lookup);
        let generation = resolver.ensure_cached().await.unwrap();
        let mut attempt = super::ResolverAttempt::new(resolver.clone(), generation);
        resolver
            .cache
            .lock()
            .unwrap()
            .current
            .as_mut()
            .unwrap()
            .expires_at = std::time::Instant::now() - Duration::from_secs(1);

        let mut connector_resolver = resolver;
        let name: Name = "errors.invalid".parse().unwrap();
        assert_eq!(connector_resolver.call(name).await.unwrap().count(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        attempt.succeed();
    }

    impl AsyncWrite for FlushRequiredWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.flushed = true;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn uri_encoding_tls_validation_and_error_io_are_bounded() {
        assert_eq!(
            encode_uri_component("/!~*()'% already%20한글"),
            "%2F!~*()'%25%20already%2520%ED%95%9C%EA%B8%80"
        );

        for invalid in [b"".as_slice(), b"not a PEM certificate".as_slice()] {
            let directory = tempfile::tempdir().unwrap();
            let ca = directory.path().join("ca.pem");
            fs::write(&ca, invalid).unwrap();
            let tls = TlsConfig {
                key: None,
                cert: None,
                ca: Some(ca),
                key_passphrase: None,
                request_cert: false,
                reject_unauthorized: true,
                protocol: None,
                ciphers: None,
                dhparam: None,
            };
            let error = match ProxyErrorRenderer::with_tls_policy(None, None, true, Some(&tls)) {
                Ok(_) => panic!("empty and malformed CA bundles must fail eagerly"),
                Err(error) => error,
            };
            assert!(matches!(error, ErrorRendererBuildError::InvalidCa));
            assert_eq!(
                error.to_string(),
                "invalid custom-error TLS CA certificate bundle"
            );
        }

        for invalid in [b"".as_slice(), b"not PEM data".as_slice()] {
            let directory = tempfile::tempdir().unwrap();
            let cert = directory.path().join("client.pem");
            let key = directory.path().join("client.key");
            fs::write(&cert, invalid).unwrap();
            fs::write(&key, invalid).unwrap();
            let tls = TlsConfig {
                key: Some(key),
                cert: Some(cert),
                ca: None,
                key_passphrase: None,
                request_cert: false,
                reject_unauthorized: true,
                protocol: None,
                ciphers: None,
                dhparam: None,
            };
            let error = match ProxyErrorRenderer::with_tls_policy(None, None, true, Some(&tls)) {
                Ok(_) => panic!("empty and malformed client identities must fail eagerly"),
                Err(error) => error,
            };
            assert!(matches!(error, ErrorRendererBuildError::InvalidIdentity));
            assert_eq!(
                error.to_string(),
                "invalid custom-error TLS client identity"
            );
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let lookup: super::Lookup = {
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            Arc::new(move |_host: String| {
                let calls = Arc::clone(&calls);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    release.acquire().await.unwrap().forget();
                    if call == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "injected DNS outage",
                        ));
                    }
                    Ok((1..=MAX_RESOLVED_ADDRESSES + 8)
                        .map(|octet| {
                            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, octet as u8)), 0)
                        })
                        .collect())
                })
            })
        };
        let resolver = BoundedResolver::start_with_lookup("errors.invalid", lookup);
        let name: Name = "errors.invalid".parse().unwrap();
        let mut first_resolver = resolver.clone();
        let first = tokio::spawn(async move { first_resolver.call(name).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first DNS lookup did not start");

        for _ in 0..64 {
            let mut resolver = resolver.clone();
            let name: Name = "errors.invalid".parse().unwrap();
            let _ = tokio::time::timeout(Duration::from_millis(1), resolver.call(name)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("404.html"),
            "fallback remains available",
        )
        .unwrap();
        let directory_path = directory.path().canonicalize().unwrap();
        assert_eq!(
            read_bounded_file_sync_with_hook(&directory_path, "404.html", || {}).unwrap(),
            bytes::Bytes::from_static(b"fallback remains available")
        );

        release.add_permits(1);
        assert!(first.await.unwrap().is_err());
        let mut recovered_resolver = resolver.clone();
        let recovered_name: Name = "errors.invalid".parse().unwrap();
        let recovered = tokio::spawn(async move {
            recovered_resolver
                .call(recovered_name)
                .await
                .unwrap()
                .count()
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("resolver did not recover after the DNS outage");
        release.add_permits(1);
        assert_eq!(recovered.await.unwrap(), MAX_RESOLVED_ADDRESSES);

        let mut cached_resolver = resolver;
        let cached_name: Name = "errors.invalid".parse().unwrap();
        assert_eq!(
            cached_resolver.call(cached_name).await.unwrap().count(),
            MAX_RESOLVED_ADDRESSES
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let mut writer = FlushRequiredWriter { flushed: false };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        write_all_and_flush(&mut writer, b"request", deadline)
            .await
            .unwrap();
        assert!(
            writer.flushed,
            "TLS request bytes must be flushed explicitly"
        );
    }

    #[tokio::test]
    async fn failed_cached_address_is_invalidated_and_concurrent_retry_is_single_flight() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let stale_guard = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port))
            .await
            .unwrap();
        drop(stale_guard);
        let calls = Arc::new(AtomicUsize::new(0));
        let release_healthy_lookup = Arc::new(tokio::sync::Semaphore::new(0));
        let lookup: super::Lookup = {
            let calls = Arc::clone(&calls);
            let release_healthy_lookup = Arc::clone(&release_healthy_lookup);
            Arc::new(move |_host: String| {
                let calls = Arc::clone(&calls);
                let release_healthy_lookup = Arc::clone(&release_healthy_lookup);
                Box::pin(async move {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    if call > 0 {
                        release_healthy_lookup.acquire().await.unwrap().forget();
                    }
                    let ip = if call == 0 {
                        IpAddr::V6(Ipv6Addr::LOCALHOST)
                    } else {
                        IpAddr::V4(Ipv4Addr::LOCALHOST)
                    };
                    Ok(vec![SocketAddr::new(ip, 0)])
                })
            })
        };
        let target = Url::parse(&format!("http://errors.invalid:{port}/errors/")).unwrap();
        let renderer =
            ProxyErrorRenderer::with_tls_policy_and_lookup(Some(target), None, true, None, lookup)
                .unwrap();

        let stale = renderer.render(http::StatusCode::NOT_FOUND, "/stale").await;
        assert_eq!(stale.body, bytes::Bytes::from_static(b"Not Found"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..super::RESOLVER_QUEUE_CAPACITY {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 512];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "custom-error request ended before its headers");
                    request.extend_from_slice(&chunk[..count]);
                    assert!(
                        request.len() <= 4096,
                        "custom-error test request is unbounded"
                    );
                }
                requests.push(request);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nhealthy",
                    )
                    .await
                    .unwrap();
            }
            requests
        });
        let retries: Vec<_> = (0..super::RESOLVER_QUEUE_CAPACITY)
            .map(|_| {
                let renderer = renderer.clone();
                tokio::spawn(
                    async move { renderer.render(http::StatusCode::NOT_FOUND, "/retry").await },
                )
            })
            .collect();
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("healthy retry lookup did not start");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        release_healthy_lookup.add_permits(1);

        for retry in retries {
            assert_eq!(
                retry.await.unwrap().body,
                bytes::Bytes::from_static(b"healthy")
            );
        }
        let requests = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("healthy custom-error server did not finish")
            .unwrap();
        assert!(requests.iter().all(|request| request.starts_with(
            format!("GET /errors/404?url=%2Fretry HTTP/1.1\r\nHost: errors.invalid:{port}\r\n")
                .as_bytes()
        )));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_request_invalidates_the_generation_consumed_by_its_connector() {
        let healthy_listener = Arc::new(
            tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap(),
        );
        let port = healthy_listener.local_addr().unwrap().port();
        let stale_listener = Arc::new(
            tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port))
                .await
                .unwrap(),
        );
        let stale_entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release_stale = Arc::new(tokio::sync::Semaphore::new(0));
        let stale_server = {
            let stale_listener = Arc::clone(&stale_listener);
            let stale_entered = Arc::clone(&stale_entered);
            let release_stale = Arc::clone(&release_stale);
            tokio::spawn(async move {
                let (mut stream, _) = stale_listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 512];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                stale_entered.add_permits(1);
                release_stale.acquire().await.unwrap().forget();
            })
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let lookup: super::Lookup = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_host: String| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    let ip = match call {
                        0 => IpAddr::V6(Ipv6Addr::LOCALHOST),
                        1 => IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
                        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    };
                    Ok(vec![SocketAddr::new(ip, 0)])
                })
            })
        };
        let target = Url::parse(&format!("http://errors.invalid:{port}/errors/")).unwrap();
        let renderer =
            ProxyErrorRenderer::with_tls_policy_and_lookup(Some(target), None, true, None, lookup)
                .unwrap();

        let first = {
            let renderer = renderer.clone();
            tokio::spawn(async move { renderer.render(http::StatusCode::NOT_FOUND, "/n").await })
        };
        tokio::time::timeout(Duration::from_secs(1), stale_entered.acquire())
            .await
            .expect("generation N connector did not enter")
            .unwrap()
            .forget();

        let canceled = {
            let renderer = renderer.clone();
            tokio::spawn(async move {
                tokio::time::timeout(
                    Duration::from_millis(20),
                    renderer.render(http::StatusCode::NOT_FOUND, "/canceled"),
                )
                .await
            })
        };
        assert!(canceled.await.unwrap().is_err());
        let second = {
            let renderer = renderer.clone();
            tokio::spawn(async move {
                renderer
                    .render(http::StatusCode::NOT_FOUND, "/n-plus-one")
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stale_listener.accept())
                .await
                .is_err(),
            "a later request consumed generation N before its owner completed"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        release_stale.add_permits(1);
        assert_eq!(
            first.await.unwrap().body,
            bytes::Bytes::from_static(b"Not Found")
        );
        stale_server.await.unwrap();
        assert_eq!(
            second.await.unwrap().body,
            bytes::Bytes::from_static(b"Not Found")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let healthy_server = {
            let healthy_listener = Arc::clone(&healthy_listener);
            tokio::spawn(async move {
                let (mut stream, _) = healthy_listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 512];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nhealthy",
                    )
                    .await
                    .unwrap();
                request
            })
        };
        let recovered = renderer
            .render(http::StatusCode::NOT_FOUND, "/recovered")
            .await;
        assert_eq!(recovered.body, bytes::Bytes::from_static(b"healthy"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let request = healthy_server.await.unwrap();
        assert!(request.starts_with(
            format!("GET /errors/404?url=%2Frecovered HTTP/1.1\r\nHost: errors.invalid:{port}\r\n")
                .as_bytes()
        ));
    }

    #[test]
    fn wire_header_limit_includes_status_line_and_final_delimiter() {
        let prefix = b"HTTP/1.1 200 OK\r\nX-Fill: ";
        let suffix = b"\r\n\r\nbody";
        for (wire_header_bytes, accepted) in [
            (16 * 1024 - 1, true),
            (16 * 1024, true),
            (16 * 1024 + 1, false),
        ] {
            let mut response = prefix.to_vec();
            response.extend(vec![b'x'; wire_header_bytes - prefix.len() - 4]);
            response.extend_from_slice(suffix);
            assert_eq!(
                parse_http_response(&response, true).is_some(),
                accepted,
                "unexpected result for {wire_header_bytes} wire header bytes"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_open_defeats_a_deterministic_symlink_swap() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "outside secret").unwrap();
        fs::write(directory.path().join("404.html"), "safe").unwrap();

        let result = read_bounded_file_sync_with_hook(directory.path(), "404.html", || {
            fs::remove_file(directory.path().join("404.html")).unwrap();
            std::os::unix::fs::symlink(outside.path(), directory.path().join("404.html")).unwrap();
        });

        assert!(
            result.is_none(),
            "a swapped symlink must never disclose its target"
        );

        let traversed = directory.path().join("traversed");
        let safe_parent = traversed.join("safe-parent");
        let replacement_parent = traversed.join("replacement-parent");
        fs::create_dir_all(safe_parent.join("errors")).unwrap();
        fs::create_dir_all(&replacement_parent).unwrap();
        fs::write(safe_parent.join("errors/404.html"), "safe ancestor").unwrap();
        fs::write(replacement_parent.join("404.html"), "outside ancestor").unwrap();
        let selected = traversed.join("selected");
        std::os::unix::fs::symlink(&safe_parent, &selected).unwrap();
        assert!(
            read_bounded_file_sync_with_hook(&selected.join("errors"), "404.html", || {}).is_none(),
            "every ancestor must reject symlinks, not only the final directory"
        );

        fs::remove_file(&selected).unwrap();
        fs::rename(&safe_parent, &selected).unwrap();
        let swapped =
            read_bounded_file_sync_with_hook(&selected.join("errors"), "404.html", || {
                fs::rename(&selected, traversed.join("detached-safe")).unwrap();
                std::os::unix::fs::symlink(&replacement_parent, &selected).unwrap();
            });
        assert!(
            swapped.is_none(),
            "an intermediate parent swapped before traversal must be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_error_files_are_rejected_without_blocking() {
        use std::os::unix::fs::FileTypeExt as _;
        use std::process::Command;
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("404.html");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        assert!(fs::metadata(&fifo).unwrap().file_type().is_fifo());

        let started = Instant::now();
        assert!(read_bounded_file_sync_with_hook(directory.path(), "404.html", || {}).is_none());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "FIFO rejection blocked the error-rendering worker"
        );

        fs::remove_file(&fifo).unwrap();
        fs::create_dir(&fifo).unwrap();
        assert!(read_bounded_file_sync_with_hook(directory.path(), "404.html", || {}).is_none());
        fs::remove_dir(&fifo).unwrap();

        let _socket = std::os::unix::net::UnixListener::bind(&fifo).unwrap();
        assert!(read_bounded_file_sync_with_hook(directory.path(), "404.html", || {}).is_none());

        assert!(
            read_bounded_file_sync_with_hook(Path::new("/dev"), "null", || {}).is_none(),
            "character devices must be rejected before a readable open"
        );
    }
}
