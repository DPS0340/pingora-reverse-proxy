//! Shutdown-aware Axum listeners and Pingora public-listener policy.

use std::collections::HashSet;
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
#[cfg(unix)]
use pingora::server::ListenFds;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use pingora::services::listening::Service;
use pingora::services::ServiceReadyNotifier;
use pingora::services::ServiceWithDependents;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_openssl::SslStream;

use crate::config::{ListenerConfig, TlsConfig};
use crate::metrics::Metrics;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CONCURRENT_TLS_HANDSHAKES: usize = 64;

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
    stop_requested: AtomicBool,
    accepts_stopped: AtomicBool,
    stop_changed: Notify,
    stopped_changed: Notify,
}

impl ManagementLifecycle {
    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        self.stop_changed.notify_waiters();
    }

    async fn wait_for_stop_requested(&self) {
        loop {
            let changed = self.stop_changed.notified();
            if self.stop_requested.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }

    fn acknowledge_accepts_stopped(&self) {
        self.accepts_stopped.store(true, Ordering::Release);
        self.stopped_changed.notify_waiters();
    }

    pub async fn wait_for_accepts_stopped(&self) {
        loop {
            let changed = self.stopped_changed.notified();
            if self.accepts_stopped.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

/// Admission and completion watermark for public HTTP and WebSocket traffic.
#[derive(Debug)]
pub struct TrafficLifecycle {
    accepting: AtomicBool,
    active: std::sync::atomic::AtomicUsize,
    accepts_stopped: AtomicBool,
    changed: Notify,
}

impl TrafficLifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_admit(&self) -> bool {
        if !self.accepting.load(Ordering::Acquire) {
            return false;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.accepting.load(Ordering::Acquire) {
            true
        } else {
            self.finish();
            false
        }
    }

    pub fn finish(&self) {
        let mut active = self.active.load(Ordering::Acquire);
        while active != 0 {
            match self.active.compare_exchange_weak(
                active,
                active - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(1) => {
                    self.changed.notify_waiters();
                    return;
                }
                Ok(_) => return,
                Err(observed) => active = observed,
            }
        }
        tracing::error!("public traffic lifecycle admission released more than once");
    }

    pub fn acknowledge_accepts_stopped(&self) {
        self.accepting.store(false, Ordering::Release);
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

    pub async fn wait_for_drain(&self) {
        loop {
            let changed = self.changed.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

impl Default for TrafficLifecycle {
    fn default() -> Self {
        Self {
            accepting: AtomicBool::new(true),
            active: std::sync::atomic::AtomicUsize::new(0),
            accepts_stopped: AtomicBool::new(false),
            changed: Notify::new(),
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
                        handshakes: tokio::task::JoinSet::new(),
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
            management.request_stop();
            management.wait_for_accepts_stopped().await;
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
        let result = if let Some(management) = management {
            axum::serve(
                ManagedListener {
                    inner: listener,
                    management: Arc::clone(&management),
                },
                router,
            )
            .with_graceful_shutdown(Self::shutdown_signal(shutdown, Some(management)))
            .await
        } else {
            axum::serve(listener, router)
                .with_graceful_shutdown(Self::shutdown_signal(shutdown, None))
                .await
        };
        if result.is_err() {
            tracing::error!("HTTP listener terminated with an I/O error");
        }
    }
}

struct ManagedListener<L> {
    inner: L,
    management: Arc<ManagementLifecycle>,
}

impl<L> Listener for ManagedListener<L>
where
    L: Listener,
{
    type Io = L::Io;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        tokio::select! {
            biased;
            () = self.management.wait_for_stop_requested() => {
                self.management.acknowledge_accepts_stopped();
                std::future::pending().await
            }
            accepted = self.inner.accept() => accepted,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
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
    handshakes: tokio::task::JoinSet<TlsHandshake>,
}

type TlsHandshake = Option<(SslStream<TcpStream>, SocketAddr)>;
type CompletedTlsHandshake = Option<Result<TlsHandshake, tokio::task::JoinError>>;

impl Listener for TlsTcpListener {
    type Io = SslStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if self.handshakes.len() >= MAX_CONCURRENT_TLS_HANDSHAKES {
                if let Some(accepted) = completed_tls_handshake(self.handshakes.join_next().await) {
                    return accepted;
                }
                continue;
            }

            tokio::select! {
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, address)) => {
                            let acceptor = Arc::clone(&self.acceptor);
                            self.handshakes.spawn(async move {
                                tls_handshake(stream, address, acceptor).await
                            });
                        }
                        Err(_) => {
                            tracing::error!("TLS listener accept failed");
                            tokio::task::yield_now().await;
                        }
                    }
                }
                completed = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                    if let Some(accepted) = completed_tls_handshake(completed) {
                        return accepted;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

async fn tls_handshake(
    stream: TcpStream,
    address: SocketAddr,
    acceptor: Arc<SslAcceptor>,
) -> Option<(SslStream<TcpStream>, SocketAddr)> {
    let ssl = match Ssl::new(acceptor.context()) {
        Ok(ssl) => ssl,
        Err(_) => {
            tracing::warn!("TLS connection initialization failed");
            return None;
        }
    };
    let mut stream = match SslStream::new(ssl, stream) {
        Ok(stream) => stream,
        Err(_) => {
            tracing::warn!("TLS stream initialization failed");
            return None;
        }
    };
    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, Pin::new(&mut stream).accept()).await {
        Ok(Ok(())) => Some((stream, address)),
        Ok(Err(_)) => {
            tracing::warn!("TLS client handshake rejected");
            None
        }
        Err(_) => {
            tracing::warn!("TLS client handshake timed out");
            None
        }
    }
}

fn completed_tls_handshake(
    completed: CompletedTlsHandshake,
) -> Option<(SslStream<TcpStream>, SocketAddr)> {
    match completed {
        Some(Ok(accepted)) => accepted,
        Some(Err(_)) => {
            tracing::error!("TLS handshake task terminated unexpectedly");
            None
        }
        None => None,
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
        let (socket, owner) = bind_unix_socket(path)?;
        let listener =
            tokio::net::UnixListener::from_std(socket.into()).map_err(ListenerError::UnixBind)?;
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

#[cfg(unix)]
impl SocketOwner {
    fn from_path(path: PathBuf) -> io::Result<Self> {
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

/// An exact public-listener owner transferred into Pingora's supported FD adoption table.
#[cfg(unix)]
pub struct PreboundPublicService<A> {
    service: Option<Service<A>>,
    reservation: Option<PublicListenerReservation>,
    traffic: Arc<TrafficLifecycle>,
}

#[cfg(unix)]
enum PublicListenerReservation {
    Tcp {
        socket: socket2::Socket,
        key: String,
    },
    Unix {
        socket: socket2::Socket,
        owner: SocketOwner,
    },
}

#[cfg(unix)]
impl PublicListenerReservation {
    fn bind(listener: &ListenerConfig) -> Result<Self, ListenerError> {
        match listener {
            ListenerConfig::Tcp(address) => {
                use std::net::ToSocketAddrs;

                let key = address.clone();
                let resolved_address = if address.starts_with(':') {
                    format!("0.0.0.0{address}")
                } else {
                    address.clone()
                };
                let resolved = resolved_address
                    .to_socket_addrs()
                    .map_err(ListenerError::TcpBind)?
                    .next()
                    .ok_or_else(|| {
                        ListenerError::TcpBind(io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "listener address resolved to no addresses",
                        ))
                    })?;
                let socket = socket2::Socket::new(
                    socket2::Domain::for_address(resolved),
                    socket2::Type::STREAM,
                    Some(socket2::Protocol::TCP),
                )
                .map_err(ListenerError::TcpBind)?;
                socket
                    .set_reuse_address(true)
                    .map_err(ListenerError::TcpBind)?;
                socket
                    .bind(&socket2::SockAddr::from(resolved))
                    .map_err(ListenerError::TcpBind)?;
                socket.listen(1024).map_err(ListenerError::TcpBind)?;
                socket
                    .set_nonblocking(true)
                    .map_err(ListenerError::TcpBind)?;
                Ok(Self::Tcp { socket, key })
            }
            ListenerConfig::Unix(path) => {
                let (socket, owner) = bind_unix_socket(path.clone())?;
                Ok(Self::Unix { socket, owner })
            }
        }
    }

    fn transfer(self) -> (String, std::os::fd::RawFd, Option<SocketOwner>) {
        use std::os::fd::IntoRawFd;

        match self {
            Self::Tcp { socket, key } => (key, socket.into_raw_fd(), None),
            Self::Unix { socket, owner } => (
                owner.path.display().to_string(),
                socket.into_raw_fd(),
                Some(owner),
            ),
        }
    }
}

#[cfg(unix)]
fn bind_unix_socket(path: PathBuf) -> Result<(socket2::Socket, SocketOwner), ListenerError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for _ in 0..4 {
        let mut random = [0_u8; 16];
        openssl::rand::rand_bytes(&mut random).map_err(|_| {
            ListenerError::UnixBind(io::Error::other(
                "could not generate a private Unix socket name",
            ))
        })?;
        let random: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = parent.join(format!(".chp-{random}.sock"));
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(ListenerError::UnixBind)?;
        let address = socket2::SockAddr::unix(&temporary).map_err(ListenerError::UnixBind)?;
        match socket.bind(&address) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(ListenerError::UnixBind(error)),
        }
        let mut owner = SocketOwner::from_path(temporary).map_err(ListenerError::UnixBind)?;
        socket.listen(1024).map_err(ListenerError::UnixBind)?;
        socket
            .set_nonblocking(true)
            .map_err(ListenerError::UnixBind)?;
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            &owner.path,
            rustix::fs::CWD,
            &path,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            ListenerError::UnixBind(io::Error::from_raw_os_error(error.raw_os_error()))
        })?;
        owner.path = path;
        return Ok((socket, owner));
    }
    Err(ListenerError::UnixBind(io::Error::new(
        io::ErrorKind::AddrInUse,
        "could not reserve a private Unix socket path",
    )))
}

#[cfg(unix)]
impl<A> PreboundPublicService<A> {
    pub fn new(
        service: Service<A>,
        listener: &ListenerConfig,
        traffic: Arc<TrafficLifecycle>,
    ) -> Result<Self, ListenerError> {
        Ok(Self {
            service: Some(service),
            reservation: Some(PublicListenerReservation::bind(listener)?),
            traffic,
        })
    }
}

/// Public-service lifecycle wrapper for platforms without Pingora FD adoption.
#[cfg(not(unix))]
pub struct TrackedPublicService<A> {
    service: Option<Service<A>>,
    traffic: Arc<TrafficLifecycle>,
}

#[cfg(not(unix))]
impl<A> TrackedPublicService<A> {
    pub fn new(service: Service<A>, traffic: Arc<TrafficLifecycle>) -> Self {
        Self {
            service: Some(service),
            traffic,
        }
    }
}

#[cfg(not(unix))]
#[async_trait]
impl<A> ServiceWithDependents for TrackedPublicService<A>
where
    A: pingora::apps::ServerApp + Send + Sync + 'static,
{
    async fn start_service(
        &mut self,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
        ready_notifier: ServiceReadyNotifier,
    ) {
        let Some(mut service) = self.service.take() else {
            tracing::error!("public listener service was started more than once");
            return;
        };
        ready_notifier.notify_ready();
        <Service<A> as pingora::services::Service>::start_service(
            &mut service,
            shutdown,
            listeners_per_fd,
        )
        .await;
        self.traffic.acknowledge_accepts_stopped();
    }

    fn name(&self) -> &str {
        "CHP public proxy"
    }

    fn threads(&self) -> Option<usize> {
        self.service.as_ref().and_then(|service| service.threads)
    }
}

#[cfg(unix)]
#[async_trait]
impl<A> ServiceWithDependents for PreboundPublicService<A>
where
    A: pingora::apps::ServerApp + Send + Sync + 'static,
{
    async fn start_service(
        &mut self,
        fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
        ready_notifier: ServiceReadyNotifier,
    ) {
        let (Some(mut service), Some(reservation), Some(fds)) =
            (self.service.take(), self.reservation.take(), fds)
        else {
            tracing::error!("pre-bound public listener could not be adopted");
            std::process::exit(1);
        };
        let (key, fd, owner) = reservation.transfer();
        tracing::debug!(bind = key, fd, "registering pre-bound public listener");
        {
            let mut table = fds.lock().await;
            table.add(key, fd);
        }
        ready_notifier.notify_ready();
        tracing::debug!(listeners_per_fd, "starting Pingora public listener service");
        <Service<A> as pingora::services::Service>::start_service(
            &mut service,
            Some(fds),
            shutdown,
            listeners_per_fd,
        )
        .await;
        self.traffic.acknowledge_accepts_stopped();
        tracing::debug!("Pingora public listener service stopped");
        drop(owner);
    }

    fn name(&self) -> &str {
        "CHP public proxy"
    }

    fn threads(&self) -> Option<usize> {
        self.service.as_ref().and_then(|service| service.threads)
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

/// Reject public/API/metrics Unix paths that name the same filesystem entry.
pub fn ensure_listener_paths_distinct<'a>(
    listeners: impl IntoIterator<Item = &'a ListenerConfig>,
) -> Result<(), ListenerError> {
    let mut paths = HashSet::new();
    for listener in listeners {
        let ListenerConfig::Unix(path) = listener else {
            continue;
        };
        let file_name = path.file_name().ok_or_else(|| {
            ListenerError::UnixBind(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unix socket path has no file name",
            ))
        })?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let canonical_parent = fs::canonicalize(parent).map_err(ListenerError::UnixBind)?;
        let identity = canonical_parent.join(file_name);
        if !paths.insert(identity) {
            return Err(ListenerError::UnixBind(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Unix listener paths alias each other",
            )));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::TrafficLifecycle;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn traffic_admission_closes_and_drains_64_concurrent_requests() {
        let lifecycle = Arc::new(TrafficLifecycle::new());
        for _ in 0..64 {
            assert!(lifecycle.try_admit());
        }
        lifecycle.acknowledge_accepts_stopped();
        assert!(!lifecycle.try_admit());

        let mut tasks = Vec::new();
        for _ in 0..64 {
            let lifecycle = Arc::clone(&lifecycle);
            tasks.push(tokio::spawn(async move {
                tokio::task::yield_now().await;
                lifecycle.finish();
            }));
        }
        tokio::time::timeout(Duration::from_secs(1), lifecycle.wait_for_drain())
            .await
            .expect("concurrent traffic drain timed out");
        for task in tasks {
            task.await.expect("traffic completion task panicked");
        }
        assert_eq!(lifecycle.active(), 0);
    }
}
