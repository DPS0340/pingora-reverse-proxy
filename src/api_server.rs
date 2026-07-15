//! Shutdown-aware Axum listeners and Pingora public-listener policy.

use std::collections::HashSet;
use std::fs;
#[cfg(unix)]
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::task::Poll;
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
#[cfg(unix)]
use futures_util::FutureExt;
use openssl::dh::Dh;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{Ssl, SslAcceptor, SslAcceptorBuilder, SslMethod};
use openssl::ssl::{SslVerifyMode, SslVersion};
use pingora::listeners::tls::TlsSettings;
use pingora::server::ShutdownWatch;
#[cfg(unix)]
use pingora::server::{Fds, ListenFds};
use pingora::services::background::BackgroundService;
use pingora::services::listening::Service;
use pingora::services::ServiceReadyNotifier;
#[cfg(unix)]
use pingora::services::ServiceWithDependents;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_openssl::SslStream;

use crate::config::{ListenerConfig, TlsConfig};
use crate::metrics::Metrics;
#[cfg(unix)]
use crate::path_ownership::{AnchoredDirectory, OwnedPath, PrivateDirectory};

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
    #[error("public proxy listeners are unsupported on this platform")]
    PublicUnsupported,
}

/// Reject targets where Pingora cannot safely adopt a synchronously owned public socket.
pub fn ensure_public_startup_supported() -> Result<(), ListenerError> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
    ))]
    {
        Ok(())
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
    )))]
    {
        Err(ListenerError::PublicUnsupported)
    }
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
    #[cfg(test)]
    releases: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    over_releases: std::sync::atomic::AtomicUsize,
}

#[cfg(unix)]
struct PublicExitGuard {
    traffic: Arc<TrafficLifecycle>,
}

#[cfg(unix)]
impl Drop for PublicExitGuard {
    fn drop(&mut self) {
        self.traffic.acknowledge_accepts_stopped();
    }
}

#[cfg(unix)]
struct FailClosedReadyNotifier(Option<ServiceReadyNotifier>);

#[cfg(unix)]
impl FailClosedReadyNotifier {
    fn notify_ready(&mut self) {
        if let Some(notifier) = self.0.take() {
            notifier.notify_ready();
        }
    }

    fn take(&mut self) -> ServiceReadyNotifier {
        self.0.take().expect("readiness notifier is taken once")
    }
}

/// Non-owning cleanup for the interval after the descriptor is entered in
/// Pingora's non-owning FD table and before Pingora constructs its listener.
/// Identity validation makes a late drop harmless if the numeric FD was reused.
#[cfg(unix)]
struct RawFdHandoffGuard {
    fd: std::os::fd::RawFd,
    device: libc::dev_t,
    inode: libc::ino_t,
    socket_address: Option<Vec<u8>>,
    armed: bool,
}

#[cfg(unix)]
impl RawFdHandoffGuard {
    fn new(fd: std::os::fd::RawFd) -> io::Result<Self> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            device: stat.st_dev,
            inode: stat.st_ino,
            socket_address: socket_address_identity(fd)?,
            armed: false,
        })
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for RawFdHandoffGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.fd, &mut stat) } == 0
            && stat.st_dev == self.device
            && stat.st_ino == self.inode
            && socket_address_identity(self.fd).ok() == Some(self.socket_address.clone())
        {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

#[cfg(unix)]
fn socket_address_identity(fd: std::os::fd::RawFd) -> io::Result<Option<Vec<u8>>> {
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe { libc::getsockname(fd, std::ptr::addr_of_mut!(address).cast(), &mut length) } != 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOTSOCK) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(std::ptr::addr_of!(address).cast::<u8>(), length as usize)
    };
    Ok(Some(bytes.to_vec()))
}

#[cfg(unix)]
struct PublicServiceFutureState<F> {
    future: Option<Pin<Box<F>>>,
    handoff: Option<RawFdHandoffGuard>,
}

#[cfg(unix)]
impl<F> PublicServiceFutureState<F> {
    fn finish(mut self) -> Option<RawFdHandoffGuard> {
        drop(self.future.take());
        self.handoff.take()
    }
}

#[cfg(unix)]
impl<F> Drop for PublicServiceFutureState<F> {
    fn drop(&mut self) {
        // Pingora may already have constructed an owning listener from the raw
        // descriptor. Always destroy that future/listener before an armed
        // guard conditionally closes the pre-adoption descriptor.
        drop(self.future.take());
        drop(self.handoff.take());
    }
}

#[cfg(unix)]
impl Drop for FailClosedReadyNotifier {
    fn drop(&mut self) {
        if let Some(notifier) = self.0.take() {
            // Pingora 0.8.1 signals ready when a notifier is dropped. A failed
            // or cancelled build must instead leave the readiness bit false.
            std::mem::forget(notifier);
        }
    }
}

#[cfg(unix)]
async fn run_public_service_future<F, V>(
    future: F,
    ready_notifier: ServiceReadyNotifier,
    traffic: Arc<TrafficLifecycle>,
    mut initial_adoption_lock: Option<tokio::sync::OwnedMutexGuard<pingora::server::Fds>>,
    handoff: Option<RawFdHandoffGuard>,
    #[cfg(test)] mut after_blocked_adoption_poll: Option<Box<dyn FnOnce() + Send>>,
    verify_ready: V,
) -> (PublicServiceOutcome, Option<RawFdHandoffGuard>)
where
    F: Future<Output = ()>,
    V: FnOnce() -> io::Result<bool>,
{
    let _exit = PublicExitGuard { traffic };
    let mut ready_notifier = FailClosedReadyNotifier(Some(ready_notifier));
    let mut state = PublicServiceFutureState {
        future: Some(Box::pin(
            std::panic::AssertUnwindSafe(future).catch_unwind(),
        )),
        handoff,
    };
    let first_poll = std::future::poll_fn(|context| {
        let poll = {
            let future = state.future.as_mut().expect("public future is present");
            future.as_mut().poll(context)
        };
        match poll {
            Poll::Ready(result) => Poll::Ready(Some(result)),
            Poll::Pending if state.handoff.is_none() => Poll::Ready(None),
            Poll::Pending => {
                #[cfg(test)]
                if let Some(hook) = after_blocked_adoption_poll.take() {
                    hook();
                    return Poll::Pending;
                }
                drop(
                    initial_adoption_lock
                        .take()
                        .expect("the first guarded Pingora poll holds its private FD table"),
                );
                let poll = {
                    let future = state.future.as_mut().expect("public future is present");
                    future.as_mut().poll(context)
                };
                state
                    .handoff
                    .as_mut()
                    .expect("public handoff guard is present")
                    .disarm();
                Poll::Ready(match poll {
                    Poll::Pending => None,
                    Poll::Ready(result) => Some(result),
                })
            }
        }
    })
    .await;

    match first_poll {
        None => match verify_ready() {
            Ok(true) => ready_notifier.notify_ready(),
            Ok(false) => {
                tracing::error!(
                    "configured public Unix path no longer resolves to the anchored socket"
                );
                let handoff = state.finish();
                return (PublicServiceOutcome::FailedBeforeReady, handoff);
            }
            Err(error) => {
                tracing::error!(%error, "configured public Unix path readiness verification failed");
                let handoff = state.finish();
                return (PublicServiceOutcome::FailedBeforeReady, handoff);
            }
        },
        Some(Ok(())) => {
            tracing::error!("public listener service exited before endpoint build completed");
            let handoff = state.finish();
            return (PublicServiceOutcome::FailedBeforeReady, handoff);
        }
        Some(Err(_)) => {
            tracing::error!("public listener endpoint adoption/build panicked");
            let handoff = state.finish();
            return (PublicServiceOutcome::FailedBeforeReady, handoff);
        }
    }

    let panicked = state
        .future
        .as_mut()
        .expect("public future is present")
        .await
        .is_err();
    let handoff = state.finish();
    let outcome = if panicked {
        tracing::error!("public listener service panicked after startup");
        PublicServiceOutcome::PanickedAfterReady
    } else {
        PublicServiceOutcome::Exited
    };
    (outcome, handoff)
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum PublicServiceOutcome {
    FailedBeforeReady,
    PanickedAfterReady,
    Exited,
}

#[cfg(unix)]
fn signal_public_startup_failure(startup_failed: &AtomicBool) {
    startup_failed.store(true, Ordering::Release);
    // Drive Pingora's ordinary graceful path so PID/UDS guards unwind; main
    // converts the recorded service failure into a nonzero exit.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGTERM);
    }
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
                    #[cfg(test)]
                    self.releases.fetch_add(1, Ordering::Relaxed);
                    self.changed.notify_waiters();
                    return;
                }
                Ok(_) => {
                    #[cfg(test)]
                    self.releases.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(observed) => active = observed,
            }
        }
        #[cfg(test)]
        self.over_releases.fetch_add(1, Ordering::Relaxed);
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

    #[cfg(test)]
    pub(crate) fn release_count_for_test(&self) -> usize {
        self.releases.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn over_release_count_for_test(&self) -> usize {
        self.over_releases.load(Ordering::Relaxed)
    }
}

impl Default for TrafficLifecycle {
    fn default() -> Self {
        Self {
            accepting: AtomicBool::new(true),
            active: std::sync::atomic::AtomicUsize::new(0),
            accepts_stopped: AtomicBool::new(false),
            changed: Notify::new(),
            #[cfg(test)]
            releases: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            over_releases: std::sync::atomic::AtomicUsize::new(0),
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
    // Drop ownership and unlink while the listener still pins the socket inode.
    _owner: SocketOwner,
    listener: tokio::net::UnixListener,
}

#[cfg(unix)]
impl OwnedUnixListener {
    fn bind(path: PathBuf) -> Result<Self, ListenerError> {
        let (socket, owner) = bind_unix_socket(path)?;
        let listener =
            tokio::net::UnixListener::from_std(socket.into()).map_err(ListenerError::UnixBind)?;
        Ok(Self {
            _owner: owner,
            listener,
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
    owned: OwnedPath,
    #[cfg(test)]
    before_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

#[cfg(unix)]
impl SocketOwner {
    #[cfg(test)]
    fn from_path(path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            owned: OwnedPath::from_path(path)?,
            before_drop: None,
        })
    }

    fn from_owned(owned: OwnedPath) -> Self {
        Self {
            owned,
            #[cfg(test)]
            before_drop: None,
        }
    }

    #[cfg(test)]
    fn set_before_drop_hook<F>(&mut self, hook: F)
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.before_drop = Some(Box::new(hook));
    }

    fn pingora_path(&self) -> io::Result<PathBuf> {
        self.owned.stable_entry_path()
    }

    fn configured_entry_matches(&self) -> io::Result<bool> {
        self.owned.configured_entry_matches()
    }

    fn prepare_in<F>(&self, staging: &PrivateDirectory, mode: u32, hook: F) -> io::Result<()>
    where
        F: FnOnce() -> io::Result<()>,
    {
        staging.prepare_socket(&self.owned, mode, hook)
    }

    fn publish_into(
        &mut self,
        destination: &AnchoredDirectory,
        path: PathBuf,
        name: std::ffi::OsString,
    ) -> io::Result<()> {
        self.owned.publish_into(destination, path, name)
    }

    #[cfg(test)]
    fn cleanup_for_test<B, V, R>(
        &mut self,
        before_quarantine: B,
        after_verify: V,
        before_restore: R,
    ) -> Option<PathBuf>
    where
        B: FnOnce(),
        V: FnOnce(),
        R: FnOnce(),
    {
        self.owned
            .cleanup_with_hooks(before_quarantine, after_verify, before_restore)
    }
}

#[cfg(all(unix, test))]
impl Drop for SocketOwner {
    fn drop(&mut self) {
        if let Some(hook) = self.before_drop.take() {
            hook();
        }
    }
}

/// An exact public-listener owner transferred into Pingora's supported FD adoption table.
#[cfg(unix)]
pub struct PreboundPublicService<A> {
    service: Option<Service<A>>,
    reservation: Option<PublicListenerReservation>,
    traffic: Arc<TrafficLifecycle>,
    startup_failed: Arc<AtomicBool>,
    #[cfg(test)]
    before_fd_table_lock: Option<Box<dyn FnOnce(std::os::fd::RawFd) + Send + Sync>>,
    #[cfg(test)]
    after_blocked_adoption_poll: Option<Box<dyn FnOnce() + Send + Sync>>,
    #[cfg(test)]
    before_handoff_guard_drop: Option<Box<dyn FnOnce(bool) + Send + Sync>>,
    #[cfg(test)]
    startup_failure_hook: Option<Box<dyn FnOnce() + Send + Sync>>,
    #[cfg(test)]
    descriptor_override: Option<std::os::fd::OwnedFd>,
    #[cfg(test)]
    after_uds_path_resolution: Option<Box<dyn FnOnce() + Send + Sync>>,
}

#[cfg(unix)]
enum PublicListenerReservation {
    Tcp {
        socket: socket2::Socket,
        key: String,
    },
    Unix {
        // Keep the socket descriptor alive until owner cleanup has finished.
        owner: SocketOwner,
        socket: socket2::Socket,
    },
}

#[cfg(unix)]
impl PublicListenerReservation {
    fn bind(listener: &ListenerConfig) -> Result<Self, ListenerError> {
        Self::bind_with_unix_hook(listener, || {})
    }

    fn bind_with_unix_hook<F>(
        listener: &ListenerConfig,
        after_parent_capture_before_bind: F,
    ) -> Result<Self, ListenerError>
    where
        F: FnOnce(),
    {
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
                let (socket, owner) =
                    bind_unix_socket_with_hook(path.clone(), after_parent_capture_before_bind)?;
                Ok(Self::Unix { socket, owner })
            }
        }
    }

    fn into_parts<A, F>(
        self,
        service: &mut Service<A>,
        after_uds_path_resolution: F,
    ) -> Result<(String, socket2::Socket, Option<SocketOwner>), ListenerError>
    where
        F: FnOnce(),
    {
        match self {
            Self::Tcp { socket, key } => Ok((key, socket, None)),
            Self::Unix { socket, owner } => {
                let pingora_path = owner.pingora_path().map_err(ListenerError::UnixBind)?;
                after_uds_path_resolution();
                let path = pingora_path.to_str().ok_or_else(|| {
                    ListenerError::UnixBind(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Unix socket path is not UTF-8",
                    ))
                })?;
                service.add_uds_with_preconfigured_permissions(path);
                Ok((path.to_string(), socket, Some(owner)))
            }
        }
    }
}

#[cfg(unix)]
fn bind_unix_socket(path: PathBuf) -> Result<(socket2::Socket, SocketOwner), ListenerError> {
    bind_unix_socket_with_hook(path, || {})
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum SocketPublicationStage {
    BeforeChmod,
}

#[cfg(unix)]
fn bind_unix_socket_with_hook<F>(
    path: PathBuf,
    after_parent_capture_before_bind: F,
) -> Result<(socket2::Socket, SocketOwner), ListenerError>
where
    F: FnOnce(),
{
    bind_unix_socket_with_stage_hook(path, after_parent_capture_before_bind, |_| Ok(()))
}

#[cfg(unix)]
fn bind_unix_socket_with_stage_hook<F, S>(
    path: PathBuf,
    after_parent_capture_before_bind: F,
    before_chmod: S,
) -> Result<(socket2::Socket, SocketOwner), ListenerError>
where
    F: FnOnce(),
    S: FnOnce(SocketPublicationStage) -> io::Result<()>,
{
    let public_name = path
        .file_name()
        .ok_or_else(|| {
            ListenerError::UnixBind(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unix socket path has no file name",
            ))
        })?
        .to_owned();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let anchor = AnchoredDirectory::capture(parent).map_err(ListenerError::UnixBind)?;
    after_parent_capture_before_bind();
    let staging = anchor
        .create_private_directory()
        .map_err(ListenerError::UnixBind)?;
    let staged_name = std::ffi::OsString::from("s");
    let staged_path = staging
        .stable_path()
        .map_err(ListenerError::UnixBind)?
        .join(&staged_name);
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
        .map_err(ListenerError::UnixBind)?;
    let address = socket2::SockAddr::unix(&staged_path).map_err(ListenerError::UnixBind)?;
    socket.bind(&address).map_err(ListenerError::UnixBind)?;
    let mut owner = match staging.own_entry(staged_name.clone()) {
        Ok(owned) => SocketOwner::from_owned(owned),
        Err(error) => {
            staging.remove_entry(&staged_name);
            return Err(ListenerError::UnixBind(error));
        }
    };
    socket.listen(1024).map_err(ListenerError::UnixBind)?;
    socket
        .set_nonblocking(true)
        .map_err(ListenerError::UnixBind)?;
    owner
        .prepare_in(&staging, 0o660, || {
            before_chmod(SocketPublicationStage::BeforeChmod)
        })
        .map_err(ListenerError::UnixBind)?;
    owner
        .publish_into(&anchor, path, public_name)
        .map_err(ListenerError::UnixBind)?;
    Ok((socket, owner))
}

#[cfg(unix)]
impl<A> PreboundPublicService<A> {
    pub fn new(
        service: Service<A>,
        listener: &ListenerConfig,
        tls: Option<TlsConfig>,
        traffic: Arc<TrafficLifecycle>,
        startup_failed: Arc<AtomicBool>,
    ) -> Result<Self, ListenerError> {
        let reservation = PublicListenerReservation::bind(listener)?;
        Self::from_reservation(service, tls, traffic, startup_failed, reservation)
    }

    #[cfg(test)]
    fn new_with_unix_bind_hook<F>(
        service: Service<A>,
        listener: &ListenerConfig,
        tls: Option<TlsConfig>,
        traffic: Arc<TrafficLifecycle>,
        startup_failed: Arc<AtomicBool>,
        hook: F,
    ) -> Result<Self, ListenerError>
    where
        F: FnOnce(),
    {
        let reservation = PublicListenerReservation::bind_with_unix_hook(listener, hook)?;
        Self::from_reservation(service, tls, traffic, startup_failed, reservation)
    }

    fn from_reservation(
        mut service: Service<A>,
        tls: Option<TlsConfig>,
        traffic: Arc<TrafficLifecycle>,
        startup_failed: Arc<AtomicBool>,
        reservation: PublicListenerReservation,
    ) -> Result<Self, ListenerError> {
        match &reservation {
            PublicListenerReservation::Tcp { key, .. } => {
                install_listener(&mut service, ListenerConfig::Tcp(key.clone()), tls)?;
            }
            PublicListenerReservation::Unix { .. } => {
                if tls.is_some() {
                    return Err(ListenerError::InvalidTls(
                        "TLS is supported only on TCP listeners",
                    ));
                }
            }
        }
        Ok(Self {
            service: Some(service),
            reservation: Some(reservation),
            traffic,
            startup_failed,
            #[cfg(test)]
            before_fd_table_lock: None,
            #[cfg(test)]
            after_blocked_adoption_poll: None,
            #[cfg(test)]
            before_handoff_guard_drop: None,
            #[cfg(test)]
            startup_failure_hook: None,
            #[cfg(test)]
            descriptor_override: None,
            #[cfg(test)]
            after_uds_path_resolution: None,
        })
    }

    #[cfg(test)]
    fn set_before_fd_table_lock_hook<F>(&mut self, hook: F)
    where
        F: FnOnce(std::os::fd::RawFd) + Send + Sync + 'static,
    {
        self.before_fd_table_lock = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn set_after_blocked_adoption_poll_hook<F>(&mut self, hook: F)
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.after_blocked_adoption_poll = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn set_before_handoff_guard_drop_hook<F>(&mut self, hook: F)
    where
        F: FnOnce(bool) + Send + Sync + 'static,
    {
        self.before_handoff_guard_drop = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn set_startup_failure_hook<F>(&mut self, hook: F)
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.startup_failure_hook = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn set_descriptor_override(&mut self, descriptor: std::os::fd::OwnedFd) {
        self.descriptor_override = Some(descriptor);
    }

    #[cfg(test)]
    fn set_after_uds_path_resolution_hook<F>(&mut self, hook: F)
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.after_uds_path_resolution = Some(Box::new(hook));
    }

    fn signal_startup_failure(&mut self) {
        self.startup_failed.store(true, Ordering::Release);
        #[cfg(test)]
        if let Some(hook) = self.startup_failure_hook.take() {
            hook();
            return;
        }
        signal_public_startup_failure(&self.startup_failed);
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
        use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};

        // These guards precede every ownership transfer and every await in
        // this method, including contention on Pingora's shared FD table.
        let _exit = PublicExitGuard {
            traffic: Arc::clone(&self.traffic),
        };
        let mut ready_notifier = FailClosedReadyNotifier(Some(ready_notifier));
        let service = self.service.take();
        let reservation = self.reservation.take();
        let Some(fds) = fds else {
            tracing::error!("pre-bound public listener could not be adopted");
            self.signal_startup_failure();
            return;
        };
        let (Some(mut service), Some(reservation)) = (service, reservation) else {
            tracing::error!("pre-bound public listener could not be adopted");
            self.signal_startup_failure();
            return;
        };
        #[cfg(test)]
        let after_uds_path_resolution = self.after_uds_path_resolution.take();
        let (key, socket, owner) = match reservation.into_parts(&mut service, || {
            #[cfg(test)]
            if let Some(hook) = after_uds_path_resolution {
                hook();
            }
        }) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::error!(%error, "pre-bound public listener address resolution failed");
                self.signal_startup_failure();
                return;
            }
        };
        #[cfg(test)]
        let descriptor: OwnedFd = if let Some(descriptor) = self.descriptor_override.take() {
            drop(socket);
            descriptor
        } else {
            socket.into()
        };
        #[cfg(not(test))]
        let descriptor: OwnedFd = socket.into();
        let fd = descriptor.as_raw_fd();
        tracing::debug!(bind = key, fd, "registering pre-bound public listener");
        #[cfg(test)]
        if let Some(hook) = self.before_fd_table_lock.take() {
            hook(fd);
        }
        let mut handoff = match RawFdHandoffGuard::new(fd) {
            Ok(handoff) => handoff,
            Err(error) => {
                tracing::error!(%error, "pre-bound public listener descriptor identity failed");
                self.signal_startup_failure();
                return;
            }
        };
        let adoption_fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let mut adoption_lock = Arc::clone(&adoption_fds).lock_owned().await;
        {
            let mut table = fds.lock().await;
            table.add(key.clone(), fd);
        }
        adoption_lock.add(key, fd);
        handoff.arm();
        let transferred_fd = descriptor.into_raw_fd();
        debug_assert_eq!(fd, transferred_fd);
        tracing::debug!(listeners_per_fd, "starting Pingora public listener service");
        let shutdown_observer = shutdown.clone();
        let (outcome, returned_handoff) = run_public_service_future(
            <Service<A> as pingora::services::Service>::start_service(
                &mut service,
                Some(adoption_fds),
                shutdown,
                listeners_per_fd,
            ),
            ready_notifier.take(),
            Arc::clone(&self.traffic),
            Some(adoption_lock),
            Some(handoff),
            #[cfg(test)]
            self.after_blocked_adoption_poll
                .take()
                .map(|hook| hook as Box<dyn FnOnce() + Send>),
            || match owner.as_ref() {
                Some(owner) => owner.configured_entry_matches(),
                None => Ok(true),
            },
        )
        .await;
        let handoff = returned_handoff.expect("public handoff guard is returned");
        if outcome != PublicServiceOutcome::Exited || !*shutdown_observer.borrow() {
            self.signal_startup_failure();
        }
        #[cfg(test)]
        if let Some(hook) = self.before_handoff_guard_drop.take() {
            hook(handoff.armed);
        }
        drop_public_listener_resources(owner, handoff);
        tracing::debug!("Pingora public listener service stopped");
    }

    fn name(&self) -> &str {
        "CHP public proxy"
    }

    fn threads(&self) -> Option<usize> {
        self.service.as_ref().and_then(|service| service.threads)
    }
}

#[cfg(unix)]
fn drop_public_listener_resources(owner: Option<SocketOwner>, handoff: RawFdHandoffGuard) {
    // Cleanup must run before the handoff guard closes the last listener FD.
    drop(owner);
    drop(handoff);
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
            builder.set_verify(verify);
        } else {
            builder.set_verify_callback(verify, |_preverified, _certificate| true);
        }
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

/// CHP-compatible metrics surface backed by the process-shared counters.
pub fn metrics_router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(render_metrics))
        .with_state(metrics)
}

async fn render_metrics(State(metrics): State<Arc<Metrics>>) -> Response {
    let body = metrics.render_prometheus();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
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
    #[cfg(unix)]
    use super::SocketOwner;
    use super::TrafficLifecycle;
    #[cfg(unix)]
    use crate::shutdown::PidFileGuard;
    #[cfg(unix)]
    use pingora::services::ServiceReadyNotifier;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::io;
    #[cfg(unix)]
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[cfg(unix)]
    struct TestServerApp;

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl pingora::apps::ServerApp for TestServerApp {
        async fn process_new(
            self: &Arc<Self>,
            _session: pingora::protocols::Stream,
            _shutdown: &pingora::server::ShutdownWatch,
        ) -> Option<pingora::protocols::Stream> {
            None
        }
    }

    #[test]
    fn public_startup_support_is_decided_synchronously_by_target() {
        let result = super::ensure_public_startup_supported();
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
        ))]
        assert!(result.is_ok());
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
        )))]
        assert!(matches!(
            result,
            Err(super::ListenerError::PublicUnsupported)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn injected_public_build_panic_never_signals_ready_and_unwinds_every_owner() {
        let directory = tempfile::tempdir().expect("temporary public failure directory");
        let socket_path = directory.path().join("public.sock");
        fs::write(&socket_path, b"owned socket").expect("write owned socket entry");
        let socket_owner = SocketOwner::from_path(socket_path.clone()).expect("own socket entry");
        let pid_path = directory.path().join("proxy.pid");
        let pid_guard = PidFileGuard::acquire(&pid_path).expect("own PID entry");
        let traffic = Arc::new(TrafficLifecycle::new());
        let (ready_sender, ready_watch) = tokio::sync::watch::channel(false);

        super::run_public_service_future(
            async { panic!("injected public endpoint build failure") },
            ServiceReadyNotifier::new(ready_sender),
            Arc::clone(&traffic),
            None,
            None,
            None,
            || Ok(true),
        )
        .await;
        drop((socket_owner, pid_guard));

        assert!(
            !*ready_watch.borrow(),
            "failed public service announced ready"
        );
        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("public exit acknowledgement did not fire");
        assert!(!socket_path.exists());
        assert!(!pid_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_public_service_future_acknowledges_exit() {
        let traffic = Arc::new(TrafficLifecycle::new());
        let (ready_sender, mut ready_watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(super::run_public_service_future(
            std::future::pending::<()>(),
            ServiceReadyNotifier::new(ready_sender),
            Arc::clone(&traffic),
            None,
            None,
            None,
            || Ok(true),
        ));
        ready_watch
            .wait_for(|ready| *ready)
            .await
            .expect("service readiness channel closed");

        task.abort();
        let _ = task.await;

        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("cancelled public service did not acknowledge exit");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_while_pingora_waits_for_its_second_fd_table_lock_closes_every_owner() {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;
        use std::os::fd::RawFd;
        use std::sync::atomic::{AtomicI32, Ordering};

        let directory = tempfile::tempdir_in("/tmp").expect("temporary handoff directory");
        let socket_path = directory.path().join("public.sock");
        let pid_path = directory.path().join("proxy.pid");
        let pid_guard = PidFileGuard::acquire(&pid_path).expect("own PID entry");
        let traffic = Arc::new(TrafficLifecycle::new());
        let startup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new(
            service,
            &crate::config::ListenerConfig::Unix(socket_path.clone()),
            None,
            Arc::clone(&traffic),
            startup_failed,
        )
        .expect("prebind public UDS");
        let reached_lock = Arc::new(tokio::sync::Notify::new());
        let observed_fd = Arc::new(AtomicI32::new(-1));
        let observed_identity = Arc::new(std::sync::Mutex::new(None));
        public.set_before_fd_table_lock_hook({
            let reached_lock = Arc::clone(&reached_lock);
            let observed_fd = Arc::clone(&observed_fd);
            let observed_identity = Arc::clone(&observed_identity);
            move |fd: RawFd| {
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
                *observed_identity
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((stat.st_dev, stat.st_ino));
                observed_fd.store(fd, Ordering::Release);
                reached_lock.notify_one();
            }
        });
        let blocked_adoption = Arc::new(tokio::sync::Notify::new());
        public.set_after_blocked_adoption_poll_hook({
            let blocked_adoption = Arc::clone(&blocked_adoption);
            move || blocked_adoption.notify_one()
        });

        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, ready_watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn({
            let fds = Arc::clone(&fds);
            async move {
                let _pid_guard = pid_guard;
                public
                    .start_service(
                        Some(fds),
                        shutdown,
                        1,
                        ServiceReadyNotifier::new(ready_sender),
                    )
                    .await;
            }
        });
        tokio::time::timeout(Duration::from_secs(1), blocked_adoption.notified())
            .await
            .expect("Pingora did not block on its private adoption table");
        task.abort();
        let _ = task.await;

        assert!(!*ready_watch.borrow(), "cancelled handoff announced ready");
        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("cancelled pre-wrapper handoff did not acknowledge exit");
        assert!(!socket_path.exists(), "cancelled public UDS owner leaked");
        assert!(!pid_path.exists(), "cancelled PID owner leaked");
        let fd = observed_fd.load(Ordering::Acquire);
        assert!(fd >= 0, "handoff hook did not observe the descriptor");
        let original_identity = observed_identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .expect("handoff hook did not capture descriptor identity");
        let mut current: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut current) } == 0 {
            assert_ne!(
                (current.st_dev, current.st_ino),
                original_identity,
                "cancelled handoff left the original file description open"
            );
        } else {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panic_while_pingora_waits_for_its_second_fd_table_lock_closes_every_owner() {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;
        use std::os::fd::RawFd;
        use std::sync::atomic::{AtomicI32, Ordering};

        let directory = tempfile::tempdir_in("/tmp").expect("temporary panic handoff directory");
        let socket_path = directory.path().join("public.sock");
        let pid_path = directory.path().join("proxy.pid");
        let pid_guard = PidFileGuard::acquire(&pid_path).expect("own PID entry");
        let traffic = Arc::new(TrafficLifecycle::new());
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new(
            service,
            &crate::config::ListenerConfig::Unix(socket_path.clone()),
            None,
            Arc::clone(&traffic),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .expect("prebind public UDS");
        let reached_handoff = Arc::new(tokio::sync::Notify::new());
        let observed_fd = Arc::new(AtomicI32::new(-1));
        let observed_identity = Arc::new(std::sync::Mutex::new(None));
        public.set_before_fd_table_lock_hook({
            let reached_handoff = Arc::clone(&reached_handoff);
            let observed_fd = Arc::clone(&observed_fd);
            let observed_identity = Arc::clone(&observed_identity);
            move |fd: RawFd| {
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
                *observed_identity
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((stat.st_dev, stat.st_ino));
                observed_fd.store(fd, Ordering::Release);
                reached_handoff.notify_one();
            }
        });
        public.set_after_blocked_adoption_poll_hook(|| {
            panic!("injected panic while Pingora awaits FD adoption")
        });

        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, ready_watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn({
            let fds = Arc::clone(&fds);
            async move {
                let _pid_guard = pid_guard;
                public
                    .start_service(
                        Some(fds),
                        shutdown,
                        1,
                        ServiceReadyNotifier::new(ready_sender),
                    )
                    .await;
            }
        });
        assert!(task.await.is_err(), "injected adoption panic was swallowed");

        assert!(!*ready_watch.borrow(), "panicked handoff announced ready");
        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("panicked handoff did not acknowledge exit");
        assert!(!socket_path.exists(), "panicked public UDS owner leaked");
        assert!(!pid_path.exists(), "panicked PID owner leaked");
        let fd = observed_fd.load(Ordering::Acquire);
        let identity = observed_identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .expect("capture descriptor identity");
        let mut current: libc::stat = unsafe { std::mem::zeroed() };
        assert!(
            unsafe { libc::fstat(fd, &mut current) } != 0
                || (current.st_dev, current.st_ino) != identity,
            "panicked handoff leaked the transferred descriptor"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_pingora_adoption_disarms_the_handoff_guard() {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;

        let traffic = Arc::new(TrafficLifecycle::new());
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new(
            service,
            &crate::config::ListenerConfig::Tcp("127.0.0.1:0".to_string()),
            None,
            traffic,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .expect("prebind public TCP");
        public.set_before_handoff_guard_drop_hook(|armed| {
            assert!(!armed, "successful Pingora adoption left its guard armed");
        });
        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, mut ready_watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            public
                .start_service(
                    Some(fds),
                    shutdown,
                    1,
                    ServiceReadyNotifier::new(ready_sender),
                )
                .await;
        });
        ready_watch
            .wait_for(|ready| *ready)
            .await
            .expect("Pingora adoption readiness");
        shutdown_sender
            .send(true)
            .expect("signal listener shutdown");
        task.await.expect("successful Pingora service task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parent_alias_replaced_after_capture_before_bind_uses_anchor_and_fails_closed() {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;

        let root = tempfile::tempdir_in("/tmp").expect("temporary pre-bind root");
        let configured_parent = root.path().join("live");
        let anchored_parent = root.path().join("anchored");
        fs::create_dir(&configured_parent).expect("create configured parent");
        let configured_path = configured_parent.join("public.sock");
        let traffic = Arc::new(TrafficLifecycle::new());
        let startup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new_with_unix_bind_hook(
            service,
            &crate::config::ListenerConfig::Unix(configured_path.clone()),
            None,
            Arc::clone(&traffic),
            Arc::clone(&startup_failed),
            {
                let configured_parent = configured_parent.clone();
                let anchored_parent = anchored_parent.clone();
                let configured_path = configured_path.clone();
                move || {
                    fs::rename(&configured_parent, &anchored_parent).expect("move captured parent");
                    fs::create_dir(&configured_parent).expect("replace configured parent alias");
                    fs::write(&configured_path, b"foreign replacement")
                        .expect("install foreign entry");
                    fs::set_permissions(&configured_path, fs::Permissions::from_mode(0o600))
                        .expect("set foreign permissions");
                }
            },
        )
        .expect("prebind through captured directory");
        public.set_startup_failure_hook(|| {});

        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, ready_watch) = tokio::sync::watch::channel(false);
        tokio::time::timeout(Duration::from_secs(1), async move {
            public
                .start_service(
                    Some(fds),
                    shutdown,
                    1,
                    ServiceReadyNotifier::new(ready_sender),
                )
                .await;
        })
        .await
        .expect("fail-closed public service did not exit");

        assert!(!*ready_watch.borrow(), "foreign alias announced ready");
        assert!(startup_failed.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("public exit acknowledgement did not fire");
        assert_eq!(
            fs::read(&configured_path).expect("foreign entry retained"),
            b"foreign replacement"
        );
        assert_eq!(
            fs::metadata(&configured_path)
                .expect("foreign metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "Pingora chmod'd the foreign configured alias"
        );
        assert_eq!(
            fs::read_dir(&anchored_parent)
                .expect("list anchored parent")
                .count(),
            0,
            "owned socket or private cleanup debris remained"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parent_alias_replaced_after_bind_fails_closed_before_pingora_readiness() {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;

        let root = tempfile::tempdir_in("/tmp").expect("temporary readiness root");
        let configured_parent = root.path().join("live");
        let anchored_parent = root.path().join("anchored");
        fs::create_dir(&configured_parent).expect("create configured parent");
        let configured_path = configured_parent.join("public.sock");
        let traffic = Arc::new(TrafficLifecycle::new());
        let startup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new(
            service,
            &crate::config::ListenerConfig::Unix(configured_path.clone()),
            None,
            Arc::clone(&traffic),
            Arc::clone(&startup_failed),
        )
        .expect("prebind public UDS");
        public.set_startup_failure_hook(|| {});

        fs::rename(&configured_parent, &anchored_parent).expect("move anchored parent");
        fs::create_dir(&configured_parent).expect("replace configured parent alias");
        fs::write(&configured_path, b"foreign replacement").expect("install foreign entry");
        fs::set_permissions(&configured_path, fs::Permissions::from_mode(0o600))
            .expect("set foreign permissions");

        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, ready_watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            public
                .start_service(
                    Some(fds),
                    shutdown,
                    1,
                    ServiceReadyNotifier::new(ready_sender),
                )
                .await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = shutdown_sender.send(true);
        task.await.expect("public service task");

        assert!(!*ready_watch.borrow(), "foreign alias announced ready");
        assert!(startup_failed.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("public exit acknowledgement did not fire");
        assert_eq!(
            fs::read(&configured_path).expect("foreign entry retained"),
            b"foreign replacement"
        );
        assert_eq!(
            fs::metadata(&configured_path)
                .expect("foreign metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "Pingora chmod'd the foreign configured alias"
        );
        assert_eq!(
            fs::read_dir(&anchored_parent)
                .expect("list anchored parent")
                .count(),
            0,
            "owned socket or private cleanup debris remained"
        );
    }

    #[cfg(unix)]
    async fn assert_replacement_after_stable_resolution_is_untouched(use_symlink: bool) {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::sync::atomic::Ordering;

        let root = tempfile::tempdir_in("/tmp").expect("temporary stable-resolution root");
        let configured_parent = root.path().join("live");
        let anchored_parent = root.path().join("anchored");
        let sentinel = root.path().join("sentinel");
        fs::create_dir(&configured_parent).expect("create configured parent");
        fs::write(&sentinel, b"sentinel content").expect("create sentinel");
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600))
            .expect("set sentinel permissions");
        let configured_path = configured_parent.join("public.sock");
        let traffic = Arc::new(TrafficLifecycle::new());
        let startup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new(
            service,
            &crate::config::ListenerConfig::Unix(configured_path.clone()),
            None,
            Arc::clone(&traffic),
            Arc::clone(&startup_failed),
        )
        .expect("prebind public UDS");
        public.set_startup_failure_hook(|| {});
        public.set_after_uds_path_resolution_hook({
            let configured_parent = configured_parent.clone();
            let anchored_parent = anchored_parent.clone();
            let configured_path = configured_path.clone();
            let sentinel = sentinel.clone();
            move || {
                fs::rename(&configured_parent, &anchored_parent).expect("move anchored parent");
                fs::create_dir(&configured_parent).expect("replace configured parent alias");
                if use_symlink {
                    symlink(&sentinel, &configured_path).expect("install foreign symlink");
                } else {
                    fs::write(&configured_path, b"foreign replacement")
                        .expect("install foreign regular file");
                    fs::set_permissions(&configured_path, fs::Permissions::from_mode(0o600))
                        .expect("set foreign permissions");
                }
            }
        });

        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, ready_watch) = tokio::sync::watch::channel(false);
        tokio::time::timeout(Duration::from_secs(1), async move {
            public
                .start_service(
                    Some(fds),
                    shutdown,
                    1,
                    ServiceReadyNotifier::new(ready_sender),
                )
                .await;
        })
        .await
        .expect("fail-closed public service did not exit");

        assert!(!*ready_watch.borrow(), "foreign alias announced ready");
        assert!(startup_failed.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("public exit acknowledgement did not fire");
        if use_symlink {
            assert_eq!(
                fs::read_link(&configured_path).expect("foreign symlink retained"),
                sentinel
            );
        } else {
            assert_eq!(
                fs::read(&configured_path).expect("foreign regular file retained"),
                b"foreign replacement"
            );
            assert_eq!(
                fs::metadata(&configured_path)
                    .expect("foreign metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "Pingora chmod'd the foreign regular file"
            );
        }
        assert_eq!(
            fs::read(&sentinel).expect("sentinel retained"),
            b"sentinel content"
        );
        assert_eq!(
            fs::metadata(&sentinel)
                .expect("sentinel metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "Pingora followed the foreign symlink"
        );
        assert_eq!(
            fs::read_dir(&anchored_parent)
                .expect("list anchored parent")
                .count(),
            0,
            "owned socket or private cleanup debris remained"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn regular_file_replacement_after_stable_resolution_is_not_mutated() {
        assert_replacement_after_stable_resolution_is_untouched(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_replacement_after_stable_resolution_does_not_mutate_target() {
        assert_replacement_after_stable_resolution_is_untouched(true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normal_public_unix_socket_mode_is_0660() {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir_in("/tmp").expect("temporary socket-mode root");
        let socket_path = root.path().join("public.sock");
        let traffic = Arc::new(TrafficLifecycle::new());
        let startup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new(
            service,
            &crate::config::ListenerConfig::Unix(socket_path.clone()),
            None,
            Arc::clone(&traffic),
            startup_failed,
        )
        .expect("prebind public UDS");
        public.set_startup_failure_hook(|| {});

        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, mut ready_watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            public
                .start_service(
                    Some(fds),
                    shutdown,
                    1,
                    ServiceReadyNotifier::new(ready_sender),
                )
                .await;
        });
        tokio::time::timeout(Duration::from_secs(1), ready_watch.changed())
            .await
            .expect("public readiness timed out")
            .expect("public readiness channel closed");
        assert!(*ready_watch.borrow(), "public UDS never announced ready");
        assert_eq!(
            fs::metadata(&socket_path)
                .expect("public socket metadata")
                .permissions()
                .mode()
                & 0o777,
            0o660
        );

        let _ = shutdown_sender.send(true);
        task.await.expect("public service task");
        assert!(!socket_path.exists(), "public socket cleanup leaked");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unique_test_descriptor_exercises_real_pingora_listener_build_failure() {
        use pingora::server::{Fds, ListenFds};
        use pingora::services::ServiceWithDependents;
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::sync::atomic::Ordering;

        let traffic = Arc::new(TrafficLifecycle::new());
        let startup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service =
            pingora::services::listening::Service::new("test public".to_string(), TestServerApp);
        let mut public = super::PreboundPublicService::new(
            service,
            &crate::config::ListenerConfig::Tcp("127.0.0.1:0".to_string()),
            None,
            Arc::clone(&traffic),
            Arc::clone(&startup_failed),
        )
        .expect("prebind public TCP");
        let unique_file = tempfile::tempfile().expect("unique non-listener descriptor");
        let unique_fd = unique_file.as_raw_fd();
        let mut unique_identity: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(unique_fd, &mut unique_identity) }, 0);
        let descriptor: OwnedFd = unique_file.into();
        public.set_descriptor_override(descriptor);
        public.set_startup_failure_hook(|| {});

        let fds: ListenFds = Arc::new(tokio::sync::Mutex::new(Fds::new()));
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let (ready_sender, ready_watch) = tokio::sync::watch::channel(false);
        tokio::time::timeout(Duration::from_secs(1), async move {
            public
                .start_service(
                    Some(fds),
                    shutdown,
                    1,
                    ServiceReadyNotifier::new(ready_sender),
                )
                .await;
        })
        .await
        .expect("listener-build failure did not exit");

        assert!(
            !*ready_watch.borrow(),
            "failed listener build announced ready"
        );
        assert!(startup_failed.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(1), traffic.wait_for_accepts_stopped())
            .await
            .expect("public exit acknowledgement did not fire");
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(unique_fd, &mut stat) } == 0 {
            assert_ne!(
                (stat.st_dev, stat.st_ino),
                (unique_identity.st_dev, unique_identity.st_ino),
                "failed Pingora build leaked the unique descriptor"
            );
        } else {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        }
    }

    #[cfg(unix)]
    #[test]
    fn armed_handoff_guard_does_not_close_a_replaced_descriptor() {
        use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

        let original =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind original listener");
        let fd = original.into_raw_fd();
        let mut handoff = super::RawFdHandoffGuard::new(fd).expect("capture descriptor identity");
        handoff.arm();
        let source =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind replacement listener");
        assert_eq!(unsafe { libc::dup2(source.as_raw_fd(), fd) }, fd);
        let replacement = unsafe { std::net::TcpListener::from_raw_fd(fd) };

        drop(handoff);

        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::fstat(replacement.as_raw_fd(), &mut stat) },
            0,
            "armed handoff guard closed a replacement descriptor"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_unix_listener_keeps_descriptor_alive_through_owner_cleanup() {
        use std::os::fd::AsRawFd;

        let directory = tempfile::tempdir().expect("temporary owned listener directory");
        let path = directory.path().join("owned.sock");
        let mut listener = super::OwnedUnixListener::bind(path).expect("bind owned listener");
        let fd = listener.listener.as_raw_fd();
        let descriptor_was_open = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&descriptor_was_open);
        listener._owner.set_before_drop_hook(move || {
            observed.store(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
                Ordering::Release,
            );
        });

        drop(listener);

        assert!(descriptor_was_open.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn unix_reservation_keeps_descriptor_alive_through_owner_cleanup() {
        use std::os::fd::AsRawFd;

        let directory = tempfile::tempdir().expect("temporary reservation directory");
        let path = directory.path().join("reservation.sock");
        let (socket, mut owner) = super::bind_unix_socket(path).expect("bind reservation");
        let fd = socket.as_raw_fd();
        let descriptor_was_open = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&descriptor_was_open);
        owner.set_before_drop_hook(move || {
            observed.store(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
                Ordering::Release,
            );
        });

        drop(super::PublicListenerReservation::Unix { socket, owner });

        assert!(descriptor_was_open.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn adopted_public_listener_keeps_descriptor_alive_through_owner_cleanup() {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let directory = tempfile::tempdir().expect("temporary adopted listener directory");
        let path = directory.path().join("adopted.sock");
        let (socket, mut owner) = super::bind_unix_socket(path).expect("bind adopted listener");
        let fd = socket.as_raw_fd();
        let descriptor_was_open = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&descriptor_was_open);
        owner.set_before_drop_hook(move || {
            observed.store(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
                Ordering::Release,
            );
        });
        let fd = socket.into_raw_fd();
        let mut handoff = super::RawFdHandoffGuard::new(fd).expect("capture adopted descriptor");
        handoff.arm();

        super::drop_public_listener_resources(Some(owner), handoff);

        assert!(descriptor_was_open.load(Ordering::Acquire));
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    }

    #[cfg(unix)]
    #[test]
    fn socket_cleanup_never_deletes_a_boundary_replacement() {
        let directory = tempfile::tempdir().expect("temporary socket ownership directory");
        let path = directory.path().join("public.sock");
        fs::write(&path, b"owned").expect("write owned entry");
        // A live Unix listener pins its socket inode while SocketOwner runs.
        // Keep the regular-file fixture equally faithful so Linux cannot
        // recycle the unlinked inode for the foreign replacement.
        let _original = fs::File::open(&path).expect("pin owned entry inode");
        let mut owner = SocketOwner::from_path(path.clone()).expect("capture socket identity");

        owner.cleanup_for_test(
            || {
                fs::remove_file(&path).expect("replace owned entry");
                fs::write(&path, b"replacement").expect("write replacement");
            },
            || {},
            || {},
        );

        assert_eq!(
            fs::read(&path).expect("replacement was preserved"),
            b"replacement"
        );
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("list ownership directory")
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_cleanup_preserves_quarantine_when_restore_collides() {
        let directory = tempfile::tempdir().expect("temporary socket ownership directory");
        let path = directory.path().join("public.sock");
        fs::write(&path, b"owned").expect("write owned entry");
        // Match production's live Unix-listener descriptor and prevent the
        // regular-file test inode from being recycled after the unlink hook.
        let _original = fs::File::open(&path).expect("pin owned entry inode");
        let mut owner = SocketOwner::from_path(path.clone()).expect("capture socket identity");

        let quarantine = owner.cleanup_for_test(
            || {
                fs::remove_file(&path).expect("replace owned entry");
                fs::write(&path, b"replacement").expect("write replacement");
            },
            || {},
            || fs::write(&path, b"restore-collision").expect("install restore collision"),
        );

        assert_eq!(
            fs::read(&path).expect("collision was preserved"),
            b"restore-collision"
        );
        let quarantine = quarantine.expect("foreign replacement must remain quarantined");
        assert_eq!(
            fs::read(quarantine).expect("quarantined replacement was preserved"),
            b"replacement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_owned_socket_entry_is_removed_without_private_debris() {
        let directory = tempfile::tempdir().expect("temporary socket ownership directory");
        let path = directory.path().join("public.sock");
        fs::write(&path, b"owned").expect("write owned entry");
        let owner = SocketOwner::from_path(path.clone()).expect("capture socket identity");

        drop(owner);

        assert!(!path.exists());
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("list ownership directory")
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn uds_publication_uses_the_captured_parent_after_alias_replacement() {
        use std::os::unix::net::UnixStream;

        let root = tempfile::tempdir_in("/tmp").expect("temporary publication root");
        let public_parent = root.path().join("live");
        let captured_parent = root.path().join("captured");
        fs::create_dir(&public_parent).expect("create publication parent");
        let configured = public_parent.join("public.sock");

        let (socket, owner) = super::bind_unix_socket_with_hook(configured.clone(), || {
            assert_eq!(
                fs::read_dir(&public_parent)
                    .expect("list parent before bind")
                    .count(),
                0,
                "temporary socket was bound before the parent-capture hook"
            );
            fs::rename(&public_parent, &captured_parent).expect("rename captured parent");
            fs::create_dir(&public_parent).expect("replace configured parent alias");
            fs::write(&configured, b"foreign destination").expect("install foreign destination");
            fs::write(public_parent.join("foreign-temp.sock"), b"foreign temp")
                .expect("install foreign temporary path");
        })
        .expect("publish through captured parent descriptor");

        assert!(
            UnixStream::connect(captured_parent.join("public.sock")).is_ok(),
            "real socket was not published in the captured directory"
        );
        assert_eq!(
            fs::read(&configured).expect("foreign configured path remains"),
            b"foreign destination"
        );
        assert_eq!(
            fs::read(public_parent.join("foreign-temp.sock")).expect("foreign temp remains"),
            b"foreign temp"
        );
        drop((socket, owner));
        assert_eq!(
            fs::read_dir(&captured_parent)
                .expect("list captured publication parent")
                .count(),
            0,
            "captured publication directory leaked entries"
        );
    }

    #[cfg(unix)]
    fn publication_stage_path(parent: &Path) -> PathBuf {
        fs::read_dir(parent)
            .expect("list publication parent")
            .find_map(|entry| {
                let entry = entry.expect("read publication entry");
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".chp-stage-")
                    .then_some(entry.path())
            })
            .expect("private publication stage exists")
    }

    #[cfg(unix)]
    #[test]
    fn permission_boundary_mutates_only_the_private_staged_socket() {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};

        let root = tempfile::tempdir_in("/tmp").expect("temporary staging boundary root");
        let public = root.path().join("public.sock");
        let result = super::bind_unix_socket_with_stage_hook(
            public.clone(),
            || {},
            |stage| {
                if stage != super::SocketPublicationStage::BeforeChmod {
                    return Ok(());
                }
                let staging = publication_stage_path(root.path());
                assert_eq!(
                    fs::metadata(&staging)
                        .expect("staging metadata")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
                let staged = staging.join("s");
                assert!(fs::symlink_metadata(&staged)
                    .expect("staged socket metadata")
                    .file_type()
                    .is_socket());
                assert!(!public.exists(), "socket was published before chmod");
                fs::write(&public, b"replacement").expect("install public replacement");
                fs::set_permissions(&public, fs::Permissions::from_mode(0o600))
                    .expect("set replacement mode");
                Ok(())
            },
        );

        assert!(
            result.is_err(),
            "no-clobber publication replaced a public entry"
        );
        assert_eq!(
            fs::read(&public).expect("replacement retained"),
            b"replacement"
        );
        assert_eq!(
            fs::metadata(&public)
                .expect("replacement metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "staging chmod mutated the public replacement"
        );
        assert_eq!(
            fs::read_dir(root.path())
                .expect("list cleaned parent")
                .count(),
            1,
            "owned staging socket or directory leaked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_chmod_failure_removes_owned_socket_and_stage() {
        let root = tempfile::tempdir_in("/tmp").expect("temporary chmod-fault root");
        let public = root.path().join("public.sock");
        let result = super::bind_unix_socket_with_stage_hook(
            public.clone(),
            || {},
            |stage| {
                if stage == super::SocketPublicationStage::BeforeChmod {
                    return Err(io::Error::other("injected staging chmod failure"));
                }
                Ok(())
            },
        );

        assert!(result.is_err(), "injected chmod failure succeeded");
        assert!(!public.exists(), "failed staging socket was published");
        assert_eq!(
            fs::read_dir(root.path())
                .expect("list chmod-fault root")
                .count(),
            0,
            "chmod failure leaked an owned socket or staging directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_replacement_during_chmod_failure_is_preserved() {
        let root = tempfile::tempdir_in("/tmp").expect("temporary stage-replacement root");
        let public = root.path().join("public.sock");
        let displaced = root.path().join("displaced-owned-stage");
        let result = super::bind_unix_socket_with_stage_hook(
            public.clone(),
            || {},
            |stage| {
                if stage != super::SocketPublicationStage::BeforeChmod {
                    return Ok(());
                }
                let staging = publication_stage_path(root.path());
                fs::rename(&staging, &displaced).expect("displace owned staging directory");
                fs::create_dir(&staging).expect("install staging replacement");
                fs::write(staging.join("foreign"), b"replacement")
                    .expect("populate staging replacement");
                Err(io::Error::other("injected staging chmod failure"))
            },
        );

        assert!(result.is_err(), "injected replacement failure succeeded");
        assert!(!public.exists(), "failed staging socket was published");
        let replacement = publication_stage_path(root.path());
        assert_eq!(
            fs::read(replacement.join("foreign")).expect("replacement retained"),
            b"replacement"
        );
        assert_eq!(
            fs::read_dir(&displaced)
                .expect("list displaced owned stage")
                .count(),
            0,
            "owned staged socket was not cleaned through its retained dirfd"
        );
    }

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
