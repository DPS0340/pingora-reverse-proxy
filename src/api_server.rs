//! Shutdown-aware Axum listeners and Pingora public-listener policy.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::header::{HOST, LOCATION};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::serve::Listener;
use axum::Router;
use openssl::dh::Dh;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{Ssl, SslAcceptor, SslAcceptorBuilder, SslMethod};
use openssl::ssl::{SslVerifyMode, SslVersion};
use pingora::listeners::tls::TlsSettings;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use pingora::services::listening::Service;
use pingora::services::ServiceReadyNotifier;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_openssl::SslStream;

use crate::config::{ListenerConfig, TlsConfig};
use crate::metrics::Metrics;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

/// Fixed, secret-free listener construction failures.
#[derive(Debug, Error)]
pub enum ListenerError {
    #[error("TLS listener requires a certificate and private key")]
    IncompleteTlsIdentity,
    #[error("TLS listener configuration is invalid: {0}")]
    InvalidTls(&'static str),
    #[error("TCP listener could not be bound")]
    TcpBind(#[source] io::Error),
    #[error("Unix listener could not be bound")]
    UnixBind(#[source] io::Error),
    #[error("Unix listeners are unavailable on this platform")]
    UnixUnsupported,
}

/// Shared ordering signal between management accepts and terminal mutation drain.
#[derive(Debug, Default)]
pub struct ManagementLifecycle {
    accepts_stopped: AtomicBool,
    changed: Notify,
}

impl ManagementLifecycle {
    fn stop_accepting(&self) {
        self.accepts_stopped.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub async fn wait_for_accepts_stopped(&self) {
        loop {
            let changed = self.changed.notified();
            if self.accepts_stopped.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

enum BoundListener {
    Tcp(TcpListener),
    Tls(TlsTcpListener),
    #[cfg(unix)]
    Unix(OwnedUnixListener),
}

/// A pre-bound Axum server that participates in Pingora's service lifecycle.
pub struct ApiServer {
    router: Router,
    listener: Mutex<Option<BoundListener>>,
    management: Option<Arc<ManagementLifecycle>>,
}

impl ApiServer {
    /// Bind before Pingora starts service runtimes so startup failures are returned to `main`.
    pub async fn bind(
        router: Router,
        listener: ListenerConfig,
        tls: Option<TlsConfig>,
        management: Option<Arc<ManagementLifecycle>>,
    ) -> Result<Self, ListenerError> {
        let listener = match listener {
            ListenerConfig::Tcp(address) => {
                let listener = TcpListener::bind(&address)
                    .await
                    .map_err(ListenerError::TcpBind)?;
                if let Some(tls) = tls {
                    let acceptor = build_ssl_acceptor(&tls)?.build();
                    BoundListener::Tls(TlsTcpListener {
                        listener,
                        acceptor: Arc::new(acceptor),
                    })
                } else {
                    BoundListener::Tcp(listener)
                }
            }
            ListenerConfig::Unix(path) => {
                if tls.is_some() {
                    return Err(ListenerError::InvalidTls(
                        "TLS is supported only on TCP listeners",
                    ));
                }
                #[cfg(unix)]
                {
                    BoundListener::Unix(OwnedUnixListener::bind(path)?)
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    return Err(ListenerError::UnixUnsupported);
                }
            }
        };
        Ok(Self {
            router,
            listener: Mutex::new(Some(listener)),
            management,
        })
    }

    async fn shutdown_signal(
        mut shutdown: ShutdownWatch,
        management: Option<Arc<ManagementLifecycle>>,
    ) {
        wait_for_shutdown(&mut shutdown).await;
        if let Some(management) = management {
            management.stop_accepting();
        }
    }

    async fn serve<L>(
        listener: L,
        router: Router,
        shutdown: ShutdownWatch,
        management: Option<Arc<ManagementLifecycle>>,
    ) where
        L: Listener,
        L::Addr: std::fmt::Debug,
    {
        if axum::serve(listener, router)
            .with_graceful_shutdown(Self::shutdown_signal(shutdown, management))
            .await
            .is_err()
        {
            tracing::error!("HTTP listener terminated with an I/O error");
        }
    }
}

#[async_trait]
impl BackgroundService for ApiServer {
    async fn start_with_ready_notifier(
        &self,
        shutdown: ShutdownWatch,
        ready_notifier: ServiceReadyNotifier,
    ) {
        let listener = self
            .listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(listener) = listener else {
            tracing::error!("HTTP listener was started more than once");
            return;
        };
        ready_notifier.notify_ready();
        match listener {
            BoundListener::Tcp(listener) => {
                Self::serve(
                    listener,
                    self.router.clone(),
                    shutdown,
                    self.management.clone(),
                )
                .await;
            }
            BoundListener::Tls(listener) => {
                Self::serve(
                    listener,
                    self.router.clone(),
                    shutdown,
                    self.management.clone(),
                )
                .await;
            }
            #[cfg(unix)]
            BoundListener::Unix(listener) => {
                Self::serve(
                    listener,
                    self.router.clone(),
                    shutdown,
                    self.management.clone(),
                )
                .await;
            }
        }
    }
}

struct TlsTcpListener {
    listener: TcpListener,
    acceptor: Arc<SslAcceptor>,
}

impl Listener for TlsTcpListener {
    type Io = SslStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => {
                    tracing::error!("TLS listener accept failed");
                    tokio::task::yield_now().await;
                    continue;
                }
            };
            let ssl = match Ssl::new(self.acceptor.context()) {
                Ok(ssl) => ssl,
                Err(_) => {
                    tracing::warn!("TLS connection initialization failed");
                    continue;
                }
            };
            let mut stream = match SslStream::new(ssl, stream) {
                Ok(stream) => stream,
                Err(_) => {
                    tracing::warn!("TLS stream initialization failed");
                    continue;
                }
            };
            match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, Pin::new(&mut stream).accept()).await
            {
                Ok(Ok(())) => return (stream, address),
                Ok(Err(_)) => tracing::warn!("TLS client handshake rejected"),
                Err(_) => tracing::warn!("TLS client handshake timed out"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(unix)]
struct OwnedUnixListener {
    listener: tokio::net::UnixListener,
    _owner: SocketOwner,
}

#[cfg(unix)]
impl OwnedUnixListener {
    fn bind(path: PathBuf) -> Result<Self, ListenerError> {
        let listener = tokio::net::UnixListener::bind(&path).map_err(ListenerError::UnixBind)?;
        let owner = SocketOwner::new(path).map_err(ListenerError::UnixBind)?;
        Ok(Self {
            listener,
            _owner: owner,
        })
    }
}

#[cfg(unix)]
impl Listener for OwnedUnixListener {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok(accepted) => return accepted,
                Err(_) => {
                    tracing::error!("Unix listener accept failed");
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(unix)]
struct SocketOwner {
    path: PathBuf,
    device: u64,
    inode: u64,
}

/// Tracks a Pingora-owned public UDS and removes only that exact socket inode.
#[cfg(unix)]
pub struct UnixSocketCleanup {
    path: PathBuf,
}

#[cfg(unix)]
impl UnixSocketCleanup {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[cfg(unix)]
#[async_trait]
impl BackgroundService for UnixSocketCleanup {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let owner = loop {
            match SocketOwner::new(self.path.clone()) {
                Ok(owner) => break Some(owner),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if *shutdown.borrow() {
                        break None;
                    }
                    tokio::task::yield_now().await;
                }
                Err(_) => {
                    tracing::error!("public Unix socket ownership could not be recorded");
                    break None;
                }
            }
        };
        if owner.is_some() {
            wait_for_shutdown(&mut shutdown).await;
        }
        drop(owner);
    }
}

#[cfg(unix)]
impl SocketOwner {
    fn new(path: PathBuf) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[cfg(unix)]
impl Drop for SocketOwner {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;

        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.dev() == self.device && metadata.ino() == self.inode {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

/// Install one TCP/TLS/UDS public Pingora endpoint.
pub fn install_listener<A>(
    service: &mut Service<A>,
    listener: ListenerConfig,
    tls: Option<TlsConfig>,
) -> Result<(), ListenerError> {
    match listener {
        ListenerConfig::Tcp(address) => {
            if let Some(tls) = tls {
                let settings = TlsSettings::from(build_ssl_acceptor(&tls)?);
                service.add_tls_with_settings(&address, None, settings);
            } else {
                service.add_tcp(&address);
            }
            Ok(())
        }
        ListenerConfig::Unix(path) => {
            if tls.is_some() {
                return Err(ListenerError::InvalidTls(
                    "TLS is supported only on TCP listeners",
                ));
            }
            ensure_socket_path_available(&path)?;
            #[cfg(unix)]
            {
                let path = path.to_str().ok_or(ListenerError::UnixBind(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Unix socket path is not UTF-8",
                )))?;
                service.add_uds(path, None);
                Ok(())
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                Err(ListenerError::UnixUnsupported)
            }
        }
    }
}

/// Fail closed when a public Unix path already has an owner.
pub fn ensure_socket_path_available(path: &Path) -> Result<(), ListenerError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ListenerError::UnixBind(error)),
        Ok(_) => Err(ListenerError::UnixBind(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Unix socket path already exists",
        ))),
    }
}

fn build_ssl_acceptor(tls: &TlsConfig) -> Result<SslAcceptorBuilder, ListenerError> {
    let cert = tls
        .cert
        .as_ref()
        .ok_or(ListenerError::IncompleteTlsIdentity)?;
    let key = tls
        .key
        .as_ref()
        .ok_or(ListenerError::IncompleteTlsIdentity)?;
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())
        .map_err(|_| ListenerError::InvalidTls("acceptor policy"))?;
    let private_key = load_private_key(key, tls.key_passphrase.as_deref())?;
    builder
        .set_private_key(&private_key)
        .map_err(|_| ListenerError::InvalidTls("private key"))?;
    builder
        .set_certificate_chain_file(cert)
        .map_err(|_| ListenerError::InvalidTls("certificate chain"))?;
    if let Some(ca) = &tls.ca {
        builder
            .set_ca_file(ca)
            .map_err(|_| ListenerError::InvalidTls("client CA"))?;
    }
    if tls.request_cert {
        let mut verify = SslVerifyMode::PEER;
        if tls.reject_unauthorized {
            verify |= SslVerifyMode::FAIL_IF_NO_PEER_CERT;
        }
        builder.set_verify(verify);
    } else {
        builder.set_verify(SslVerifyMode::NONE);
    }
    if let Some(protocol) = tls.protocol.as_deref() {
        let version = protocol_version(protocol)?;
        builder
            .set_min_proto_version(Some(version))
            .and_then(|()| builder.set_max_proto_version(Some(version)))
            .map_err(|_| ListenerError::InvalidTls("protocol"))?;
    }
    if let Some(ciphers) = &tls.ciphers {
        builder
            .set_cipher_list(ciphers)
            .map_err(|_| ListenerError::InvalidTls("cipher policy"))?;
    }
    if let Some(dhparam) = &tls.dhparam {
        let pem = fs::read(dhparam).map_err(|_| ListenerError::InvalidTls("DH parameters"))?;
        let dh =
            Dh::params_from_pem(&pem).map_err(|_| ListenerError::InvalidTls("DH parameters"))?;
        builder
            .set_tmp_dh(&dh)
            .map_err(|_| ListenerError::InvalidTls("DH parameters"))?;
    }
    builder
        .check_private_key()
        .map_err(|_| ListenerError::InvalidTls("identity mismatch"))?;
    Ok(builder)
}

fn load_private_key(path: &Path, passphrase: Option<&str>) -> Result<PKey<Private>, ListenerError> {
    let pem = fs::read(path).map_err(|_| ListenerError::InvalidTls("private key"))?;
    match passphrase {
        Some(passphrase) => PKey::private_key_from_pem_passphrase(&pem, passphrase.as_bytes()),
        None => PKey::private_key_from_pem(&pem),
    }
    .map_err(|_| ListenerError::InvalidTls("private key"))
}

fn protocol_version(protocol: &str) -> Result<SslVersion, ListenerError> {
    match protocol {
        "TLSv1" | "TLSv1.0" => Ok(SslVersion::TLS1),
        "TLSv1_1" | "TLSv1.1" => Ok(SslVersion::TLS1_1),
        "TLSv1_2" | "TLSv1.2" => Ok(SslVersion::TLS1_2),
        "TLSv1_3" | "TLSv1.3" => Ok(SslVersion::TLS1_3),
        _ => Err(ListenerError::InvalidTls("unsupported protocol")),
    }
}

pub(crate) async fn wait_for_shutdown(shutdown: &mut ShutdownWatch) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Minimal Task 8 metrics surface backed by the process-shared counters.
pub fn metrics_router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(render_metrics))
        .with_state(metrics)
}

async fn render_metrics(State(metrics): State<Arc<Metrics>>) -> Response {
    let snapshot = metrics.snapshot();
    let mut body = String::new();
    for (status, count) in snapshot.requests_api {
        if count != 0 {
            body.push_str(&format!("requests_api{{status=\"{status}\"}} {count}\n"));
        }
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[derive(Clone, Copy)]
struct RedirectState {
    https_port: u16,
}

/// HTTP-to-HTTPS redirect surface used by `--redirect-port`.
pub fn redirect_router(https_port: u16) -> Router {
    Router::new()
        .fallback(redirect_request)
        .with_state(RedirectState { https_port })
}

async fn redirect_request(
    State(state): State<RedirectState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let Some(host) = headers.get(HOST).and_then(|host| host.to_str().ok()) else {
        return empty_response(StatusCode::BAD_REQUEST);
    };
    let Some(host) = redirect_hostname(host) else {
        return empty_response(StatusCode::BAD_REQUEST);
    };
    let path = uri
        .path_and_query()
        .map_or(uri.path(), |path| path.as_str());
    let location = format!("https://{host}:{}{path}", state.https_port);
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(LOCATION, location)
        .body(Body::empty())
        .unwrap_or_else(|_| empty_response(StatusCode::BAD_REQUEST))
}

fn redirect_hostname(host: &str) -> Option<&str> {
    if host.starts_with('[') {
        let closing = host.find(']')?;
        return Some(&host[..=closing]);
    }
    Some(host.split_once(':').map_or(host, |(hostname, _)| hostname))
        .filter(|hostname| !hostname.is_empty())
}

fn empty_response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
