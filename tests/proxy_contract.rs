use std::collections::BTreeMap;
use std::fs;
use std::net::{SocketAddr as StdSocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use clap::Parser;
use http::header::{CONNECTION, HOST, LOCATION, UPGRADE};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Uri};
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::rsa::Rsa;
use openssl::ssl::{SslAcceptor, SslMethod};
use openssl::ssl::{SslVerifyMode, SslVersion};
use openssl::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
    SubjectKeyIdentifier,
};
use pingora::protocols::l4::socket::SocketAddr;
use pingora::tls::{
    hash::MessageDigest,
    pkey::PKey,
    x509::{X509NameBuilder, X509},
};
use proptest::prelude::*;
use url::Url;

use axum::body::Bytes;
use axum::extract::Request as AxumRequest;
use axum::response::IntoResponse;
use axum::routing::any as any_route;
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

use pingora_reverse_proxy::activity::ActivityWriter;
use pingora_reverse_proxy::config::ProxyOptions;
use pingora_reverse_proxy::config::{AppConfig, Cli};
use pingora_reverse_proxy::metrics::Metrics;
use pingora_reverse_proxy::proxy::ChpProxy;
use pingora_reverse_proxy::route::RouteData;
use pingora_reverse_proxy::route::RouteKey;
use pingora_reverse_proxy::route_table::{MutationSeal, RouteRegistry};
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::{ActivityFloor, Store, StoreError};
use pingora_reverse_proxy::upstream::{
    apply_forwarded_headers, apply_request_headers, build_upstream_uri, rewrite_location,
    ForwardedContext, Target, TargetError, TlsClientConfig, UpstreamRoute,
};

const RAW_NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

async fn timed_accept(
    listener: &tokio::net::TcpListener,
    diagnostic: &'static str,
) -> (tokio::net::TcpStream, StdSocketAddr) {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, listener.accept())
        .await
        .expect(diagnostic)
        .expect(diagnostic)
}

#[cfg(unix)]
async fn timed_unix_accept(
    listener: &tokio::net::UnixListener,
    diagnostic: &'static str,
) -> (tokio::net::UnixStream, tokio::net::unix::SocketAddr) {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, listener.accept())
        .await
        .expect(diagnostic)
        .expect(diagnostic)
}

async fn timed_read<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
    diagnostic: &'static str,
) -> usize {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, reader.read(buffer))
        .await
        .expect(diagnostic)
        .expect(diagnostic)
}

async fn timed_read_until<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    ceiling: usize,
    deadline: tokio::time::Instant,
    diagnostic: &'static str,
) -> usize {
    assert!(buffer.len() < ceiling, "{diagnostic}: byte ceiling reached");
    let old_len = buffer.len();
    let read_len = (ceiling - old_len).min(8192);
    buffer.resize(old_len + read_len, 0);
    let count = tokio::time::timeout_at(deadline, reader.read(&mut buffer[old_len..]))
        .await
        .expect(diagnostic)
        .expect(diagnostic);
    buffer.truncate(old_len + count);
    count
}

async fn timed_read_to_end<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    diagnostic: &'static str,
) {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, reader.read_to_end(buffer))
        .await
        .expect(diagnostic)
        .expect(diagnostic);
}

async fn timed_write_all<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    buffer: &[u8],
    diagnostic: &'static str,
) {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, writer.write_all(buffer))
        .await
        .expect(diagnostic)
        .expect(diagnostic);
}

async fn timed_write_all_allow_disconnect<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    buffer: &[u8],
    diagnostic: &'static str,
) {
    let result = tokio::time::timeout(RAW_NETWORK_TIMEOUT, writer.write_all(buffer))
        .await
        .expect(diagnostic);
    if let Err(error) = result {
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ),
            "{diagnostic}: {error}"
        );
    }
}

async fn timed_flush<W: AsyncWriteExt + Unpin>(writer: &mut W, diagnostic: &'static str) {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, writer.flush())
        .await
        .expect(diagnostic)
        .expect(diagnostic);
}

async fn timed_join<T>(task: tokio::task::JoinHandle<T>, diagnostic: &'static str) -> T {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, task)
        .await
        .expect(diagnostic)
        .expect(diagnostic)
}

async fn timed_activity_flush(writer: &ActivityWriter) {
    tokio::time::timeout(RAW_NETWORK_TIMEOUT, writer.flush())
        .await
        .expect("activity writer flush timed out");
}

struct ReservedPort {
    listener: Option<StdTcpListener>,
    address: StdSocketAddr,
}

impl ReservedPort {
    fn new() -> Self {
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        Self {
            listener: Some(listener),
            address,
        }
    }

    fn release(&mut self) -> StdSocketAddr {
        drop(self.listener.take());
        self.address
    }
}

struct EchoServer {
    address: StdSocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl EchoServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, stopped) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any_route(move |request: AxumRequest| async move {
                    let (parts, body) = request.into_parts();
                    let body = axum::body::to_bytes(body, 2 * 1024 * 1024).await.unwrap();
                    if parts.uri.path().ends_with("/redirect-body") {
                        return (
                            StatusCode::FOUND,
                            [(LOCATION, format!("http://{address}/next"))],
                            "redirect traffic",
                        )
                            .into_response();
                    }
                    if parts.uri.path().ends_with("/redirect") {
                        return (
                            StatusCode::MOVED_PERMANENTLY,
                            [(LOCATION, format!("http://{address}/next"))],
                            "",
                        )
                            .into_response();
                    }
                    let response = serde_json::json!({
                        "method": parts.method.as_str(),
                        "uri": parts.uri.to_string(),
                        "host": parts.headers.get(HOST).and_then(|value| value.to_str().ok()),
                        "x_custom": parts.headers.get("x-custom").and_then(|value| value.to_str().ok()),
                        "x_forwarded_for": parts.headers.get("x-forwarded-for").and_then(|value| value.to_str().ok()),
                        "x_forwarded_port": parts.headers.get("x-forwarded-port").and_then(|value| value.to_str().ok()),
                        "x_forwarded_proto": parts.headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()),
                        "x_forwarded_host": parts.headers.get("x-forwarded-host").and_then(|value| value.to_str().ok()),
                        "connection": parts.headers.get(CONNECTION).and_then(|value| value.to_str().ok()),
                        "upgrade": parts.headers.get(UPGRADE).and_then(|value| value.to_str().ok()),
                        "x_custom_hop": parts.headers.get("x-custom-hop").and_then(|value| value.to_str().ok()),
                        "body": String::from_utf8_lossy(&body),
                    });
                    (
                        [
                            ("content-type", "application/json"),
                            ("content-encoding", "identity"),
                            ("x-disallowed", "secret"),
                        ],
                        response.to_string(),
                    )
                        .into_response()
                })),
            )
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        });
        Self {
            address,
            shutdown: Some(shutdown),
        }
    }

    fn target(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn exact_health_ready(client: &reqwest::Client, base_url: &str) -> bool {
    let Ok(Ok(response)) = tokio::time::timeout(
        Duration::from_millis(250),
        client.get(format!("{base_url}/_chp_healthz")).send(),
    )
    .await
    else {
        return false;
    };
    if response.status() != StatusCode::OK
        || response.headers().get(http::header::CONTENT_TYPE)
            != Some(&HeaderValue::from_static("application/json"))
    {
        return false;
    }
    matches!(
        tokio::time::timeout(Duration::from_millis(250), response.bytes()).await,
        Ok(Ok(body)) if body == Bytes::from_static(br#"{"status":"OK"}"#)
    )
}

struct ProxyProcess {
    child: Child,
    base_url: String,
}

impl ProxyProcess {
    async fn start(extra_args: &[String]) -> Self {
        let client = reqwest::Client::new();
        for _attempt in 0..5 {
            let mut reservation = ReservedPort::new();
            let address = reservation.release();
            let mut child = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"))
                .args([
                    "--ip",
                    "127.0.0.1",
                    "--port",
                    &address.port().to_string(),
                    "--api-port",
                    &address.port().saturating_add(1).to_string(),
                ])
                .args(extra_args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let base_url = format!("http://{address}");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while tokio::time::Instant::now() < deadline {
                if exact_health_ready(&client, &base_url).await {
                    return Self { child, base_url };
                }
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("proxy binary did not bind and pass its exact health contract after retries");
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .unwrap()
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestShutdown(Arc<AtomicBool>);

#[async_trait]
impl pingora::server::ShutdownSignalWatch for TestShutdown {
    async fn recv(&self) -> pingora::server::ShutdownSignal {
        while !self.0.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pingora::server::ShutdownSignal::FastShutdown
    }
}

struct ProxyHarness {
    store: Arc<MemoryStore>,
    registry: Arc<RouteRegistry>,
    activity: ActivityWriter,
    base_url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProxyHarness {
    async fn start(arguments: &[String], routes: &[(&str, String)]) -> Self {
        let client = reqwest::Client::new();
        for _attempt in 0..5 {
            let mut reservation = ReservedPort::new();
            let address = reservation.address;
            let mut argv = vec![
                "proxy-test".to_owned(),
                "--ip".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                address.port().to_string(),
                "--api-port".to_owned(),
                address.port().saturating_add(1).to_string(),
            ];
            argv.extend_from_slice(arguments);
            let config = AppConfig::try_from(Cli::try_parse_from(argv).unwrap()).unwrap();
            let store = Arc::new(MemoryStore::new());
            let registry = RouteRegistry::load(store.clone()).await.unwrap();
            for (key, target) in routes {
                registry
                    .add(
                        RouteKey::parse(key).unwrap(),
                        target.clone(),
                        Default::default(),
                    )
                    .await
                    .unwrap();
            }
            let activity = ActivityWriter::start(Arc::clone(&registry), 1);
            let proxy = ChpProxy::from_config(Arc::clone(&registry), &config, activity.clone())
                .await
                .unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            reservation.release();
            let thread = std::thread::spawn(move || {
                let mut server = pingora::server::Server::new(None).unwrap();
                server.bootstrap();
                let mut service = pingora::proxy::http_proxy_service(&server.configuration, proxy);
                service.add_tcp(&address.to_string());
                server.add_service(service);
                server.run(pingora::server::RunArgs {
                    shutdown_signal: Box::new(TestShutdown(thread_stop)),
                });
            });
            let base_url = format!("http://{address}");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while tokio::time::Instant::now() < deadline {
                if exact_health_ready(&client, &base_url).await {
                    return Self {
                        store,
                        registry,
                        activity,
                        base_url,
                        stop,
                        thread: Some(thread),
                    };
                }
                if thread.is_finished() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            stop.store(true, Ordering::Release);
            join_thread_with_timeout(thread, "failed Pingora bind attempt did not stop");
        }
        panic!("in-process Pingora server did not bind and pass exact health after retries");
    }

    async fn request(&self, request: reqwest::RequestBuilder) -> reqwest::Response {
        request.send().await.unwrap()
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.request(reqwest::Client::new().get(self.url(path)))
            .await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn route(&self, key: &str) -> RouteData {
        self.registry
            .get(&RouteKey::parse(key).unwrap())
            .expect("route exists")
    }

    async fn wait_for_activity_after(&self, key: &str, before: chrono::DateTime<chrono::Utc>) {
        for _ in 0..100 {
            if self.route(key).last_activity > before {
                timed_activity_flush(&self.activity).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("activity for {key} was not observed");
    }
}

impl Drop for ProxyHarness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            join_thread_with_timeout(thread, "Pingora test server did not stop");
        }
    }
}

struct ChunkedUpstream {
    address: StdSocketAddr,
    task: tokio::task::JoinHandle<()>,
}

struct CustomErrorProbe {
    address: StdSocketAddr,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl CustomErrorProbe {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) =
                    timed_accept(&listener, "custom-error probe accept timed out").await;
                task_requests.fetch_add(1, Ordering::Relaxed);
                let mut request = [0; 1024];
                timed_read(
                    &mut stream,
                    &mut request,
                    "custom-error probe read timed out",
                )
                .await;
                timed_write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\ncustom error",
                    "custom-error probe write timed out",
                )
                .await;
            }
        });
        Self {
            address,
            requests,
            task,
        }
    }

    fn target(&self) -> String {
        format!("http://{}/errors/", self.address)
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

impl Drop for CustomErrorProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct RawErrorServer {
    address: StdSocketAddr,
    request: oneshot::Receiver<String>,
    task: tokio::task::JoinHandle<()>,
}

impl RawErrorServer {
    async fn start(response: Vec<u8>, delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) =
                timed_accept(&listener, "raw custom-error server accept timed out").await;
            let mut bytes = vec![0; 32 * 1024];
            let count = timed_read(
                &mut stream,
                &mut bytes,
                "raw custom-error request read timed out",
            )
            .await;
            let _ = request_tx.send(String::from_utf8_lossy(&bytes[..count]).into_owned());
            tokio::time::sleep(delay).await;
            timed_write_all_allow_disconnect(
                &mut stream,
                &response,
                "raw custom-error response write timed out",
            )
            .await;
        });
        Self {
            address,
            request,
            task,
        }
    }

    fn target(&self, suffix: &str) -> String {
        format!("http://{}{suffix}", self.address)
    }
}

async fn partial_response_upstream() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = timed_accept(&listener, "partial upstream accept timed out").await;
        let mut request = [0; 1024];
        timed_read(
            &mut stream,
            &mut request,
            "partial upstream request read timed out",
        )
        .await;
        timed_write_all(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial-200",
            "partial upstream response write timed out",
        )
        .await;
    });
    (format!("http://{address}"), task)
}

async fn stalled_request_upstream() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = timed_accept(&listener, "stalled upstream accept timed out").await;
        let mut request = [0; 1024];
        timed_read(
            &mut stream,
            &mut request,
            "stalled upstream request read timed out",
        )
        .await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    });
    (format!("http://{address}"), task)
}

fn https_error_server(response: Vec<u8>) -> (String, JoinHandle<()>) {
    https_error_server_with_read_delay(response, Duration::ZERO)
}

fn https_error_server_with_read_delay(
    response: Vec<u8>,
    read_delay: Duration,
) -> (String, JoinHandle<()>) {
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "localhost").unwrap();
    let name = name.build();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    let mut serial = BigNum::new().unwrap();
    serial.rand(64, MsbOption::MAYBE_ZERO, false).unwrap();
    let serial = serial.to_asn1_integer().unwrap();
    certificate.set_serial_number(&serial).unwrap();
    certificate.set_subject_name(&name).unwrap();
    certificate.set_issuer_name(&name).unwrap();
    certificate.set_pubkey(&key).unwrap();
    certificate
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    certificate
        .set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    certificate.sign(&key, MessageDigest::sha256()).unwrap();
    let certificate = certificate.build();
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&certificate).unwrap();
    acceptor.check_private_key().unwrap();
    let acceptor = acceptor.build();
    let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let thread = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "HTTPS server accept timed out"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("HTTPS server accept failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let Ok(mut stream) = acceptor.accept(stream) else {
            return;
        };
        std::thread::sleep(read_delay);
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 2048];
            let count =
                std::io::Read::read(&mut stream, &mut chunk).expect("HTTPS request read timed out");
            assert!(count > 0, "HTTPS request ended before its headers");
            request.extend_from_slice(&chunk[..count]);
            assert!(
                request.len() <= 16 * 1024,
                "HTTPS request headers too large"
            );
        }
        if let Err(error) = std::io::Write::write_all(&mut stream, &response) {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ),
                "HTTPS response write failed: {error}"
            );
        }
    });
    (format!("https://{address}/errors/"), thread)
}

fn signed_certificate(
    common_name: &str,
    key: &PKey<openssl::pkey::Private>,
    issuer: Option<(&X509, &PKey<openssl::pkey::Private>)>,
    server: bool,
) -> X509 {
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", common_name).unwrap();
    let name = name.build();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    let mut serial = BigNum::new().unwrap();
    serial.rand(64, MsbOption::MAYBE_ZERO, false).unwrap();
    certificate
        .set_serial_number(&serial.to_asn1_integer().unwrap())
        .unwrap();
    certificate.set_subject_name(&name).unwrap();
    certificate
        .set_issuer_name(issuer.map_or(&name, |(issuer, _)| issuer.subject_name()))
        .unwrap();
    certificate.set_pubkey(key).unwrap();
    certificate
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    certificate
        .set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    if issuer.is_none() {
        certificate
            .append_extension(BasicConstraints::new().critical().ca().build().unwrap())
            .unwrap();
        certificate
            .append_extension(
                KeyUsage::new()
                    .critical()
                    .key_cert_sign()
                    .crl_sign()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        let extension = {
            let context = certificate.x509v3_context(None, None);
            SubjectKeyIdentifier::new().build(&context).unwrap()
        };
        certificate.append_extension(extension).unwrap();
    } else {
        certificate
            .append_extension(BasicConstraints::new().critical().build().unwrap())
            .unwrap();
        let extension = {
            let context = certificate
                .x509v3_context(issuer.map(|(certificate, _)| certificate.as_ref()), None);
            AuthorityKeyIdentifier::new()
                .keyid(true)
                .issuer(true)
                .build(&context)
                .unwrap()
        };
        certificate.append_extension(extension).unwrap();
        certificate
            .append_extension(
                KeyUsage::new()
                    .critical()
                    .digital_signature()
                    .key_encipherment()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        let extended_key_usage = if server {
            ExtendedKeyUsage::new().server_auth().build().unwrap()
        } else {
            ExtendedKeyUsage::new().client_auth().build().unwrap()
        };
        certificate.append_extension(extended_key_usage).unwrap();
        if server {
            let extension = {
                let context = certificate
                    .x509v3_context(issuer.map(|(certificate, _)| certificate.as_ref()), None);
                SubjectAlternativeName::new()
                    .ip("127.0.0.1")
                    .dns("localhost")
                    .build(&context)
                    .unwrap()
            };
            certificate.append_extension(extension).unwrap();
        }
    }
    let signer = issuer.map_or(key, |(_, key)| key);
    certificate.sign(signer, MessageDigest::sha256()).unwrap();
    certificate.build()
}

fn private_ca_mtls_error_server(
    directory: &tempfile::TempDir,
    response: Vec<u8>,
) -> (String, PathBuf, PathBuf, PathBuf, JoinHandle<()>) {
    let ca_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let ca = signed_certificate("task-7-private-ca", &ca_key, None, false);
    let server_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let server = signed_certificate("localhost", &server_key, Some((&ca, &ca_key)), true);
    let client_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let client = signed_certificate("task-7-client", &client_key, Some((&ca, &ca_key)), false);

    let ca_path = directory.path().join("ca.pem");
    let client_path = directory.path().join("client.pem");
    let key_path = directory.path().join("client-key.pem");
    fs::write(&ca_path, ca.to_pem().unwrap()).unwrap();
    fs::write(&client_path, client.to_pem().unwrap()).unwrap();
    fs::write(&key_path, client_key.private_key_to_pem_pkcs8().unwrap()).unwrap();

    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .unwrap();
    acceptor
        .set_max_proto_version(Some(SslVersion::TLS1_2))
        .unwrap();
    acceptor.set_private_key(&server_key).unwrap();
    acceptor.set_certificate(&server).unwrap();
    acceptor.cert_store_mut().add_cert(ca).unwrap();
    acceptor.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    acceptor.check_private_key().unwrap();
    let acceptor = acceptor.build();
    let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let thread = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "mTLS server accept timed out"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("mTLS server accept failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let Ok(mut stream) = acceptor.accept(stream) else {
            return;
        };
        let mut request = [0; 2048];
        std::io::Read::read(&mut stream, &mut request).expect("mTLS request read timed out");
        std::io::Write::write_all(&mut stream, &response).expect("mTLS response write timed out");
    });
    (
        format!("https://localhost:{}/errors/", address.port()),
        ca_path,
        client_path,
        key_path,
        thread,
    )
}

fn join_thread_with_timeout(thread: JoinHandle<()>, diagnostic: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !thread.is_finished() {
        assert!(std::time::Instant::now() < deadline, "{diagnostic}");
        std::thread::sleep(Duration::from_millis(5));
    }
    thread.join().unwrap();
}

impl ChunkedUpstream {
    async fn start(chunks: Vec<&'static [u8]>) -> Self {
        Self::start_with_delay(chunks, Duration::from_millis(15)).await
    }

    async fn start_with_delay(chunks: Vec<&'static [u8]>, chunk_delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) =
                timed_accept(&listener, "chunked upstream accept timed out").await;
            let mut request = vec![0; 4096];
            timed_read(
                &mut stream,
                &mut request,
                "chunked upstream request read timed out",
            )
            .await;
            timed_write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                "chunked upstream headers write timed out",
            )
            .await;
            for chunk in chunks {
                timed_write_all(
                    &mut stream,
                    format!("{:x}\r\n", chunk.len()).as_bytes(),
                    "chunked upstream size write timed out",
                )
                .await;
                timed_write_all(&mut stream, chunk, "chunked upstream body write timed out").await;
                timed_write_all(
                    &mut stream,
                    b"\r\n",
                    "chunked upstream delimiter write timed out",
                )
                .await;
                timed_flush(&mut stream, "chunked upstream flush timed out").await;
                tokio::time::sleep(chunk_delay).await;
            }
            timed_write_all(
                &mut stream,
                b"0\r\n\r\n",
                "chunked upstream end write timed out",
            )
            .await;
        });
        Self { address, task }
    }

    fn target(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for ChunkedUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
#[serial_test::serial]
async fn network_binary_proxies_default_route_and_health_takes_precedence() {
    let upstream = EchoServer::start().await;
    let proxy =
        ProxyProcess::start(&["--default-target".to_owned(), upstream.target("/base/")]).await;

    let response = proxy.get("/tree?q=%2F").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["uri"], "/base/tree?q=%2F");

    let response = proxy.get("/_chp_healthz").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[http::header::CONTENT_TYPE],
        "application/json"
    );
    assert_eq!(
        response.bytes().await.unwrap(),
        Bytes::from_static(br#"{"status":"OK"}"#)
    );
}

#[tokio::test]
#[serial_test::serial]
async fn network_default_target_is_a_persisted_root_route_with_activity_policy() {
    let upstream = EchoServer::start().await;
    let harness = ProxyHarness::start(
        &["--default-target".to_owned(), upstream.target("/default/")],
        &[],
    )
    .await;
    let root = RouteKey::parse("/").unwrap();
    let installed = harness
        .registry
        .get(&root)
        .expect("default target must be visible as the registry root route");
    assert_eq!(installed.target, upstream.target("/default/"));
    let persisted = harness.store.snapshot().await.unwrap();
    assert_eq!(persisted.get(&root), Some(&installed));
    let reloaded = RouteRegistry::load(harness.store.clone()).await.unwrap();
    assert_eq!(reloaded.all(), harness.registry.all());
    assert_eq!(reloaded.resolve("/restart").unwrap().key, root);
    let response = harness.get("/eligible").await;
    assert_eq!(response.status(), StatusCode::OK);
    timed_activity_flush(&harness.activity).await;
    assert!(harness.registry.get(&root).unwrap().last_activity > installed.last_activity);

    let mut unavailable = ReservedPort::new();
    let harness = ProxyHarness::start(
        &[
            "--default-target".to_owned(),
            format!("http://{}", unavailable.release()),
        ],
        &[],
    )
    .await;
    let installed = harness.registry.get(&root).unwrap();
    assert_eq!(
        harness.get("/ineligible").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    timed_activity_flush(&harness.activity).await;
    assert_eq!(
        harness.registry.get(&root).unwrap().last_activity,
        installed.last_activity
    );
}

#[tokio::test]
#[serial_test::serial]
async fn network_binary_without_route_returns_404() {
    let proxy = ProxyProcess::start(&[]).await;
    let response = proxy.get("/missing").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "Not Found");
}

#[tokio::test]
#[serial_test::serial]
async fn network_registered_route_streams_request_and_applies_forwarding_policy() {
    let upstream = EchoServer::start().await;
    let harness = ProxyHarness::start(
        &[
            "--no-include-prefix".to_owned(),
            "--change-origin".to_owned(),
            "--custom-header".to_owned(),
            "x-custom: configured".to_owned(),
        ],
        &[("/external", upstream.target("/base/"))],
    )
    .await;
    let before = harness.route("/external").last_activity;
    let url = Url::parse(&harness.url("/external/tree?q=%2F")).unwrap();
    let address = (url.host_str().unwrap(), url.port().unwrap());
    let mut stream =
        tokio::time::timeout(RAW_NETWORK_TIMEOUT, tokio::net::TcpStream::connect(address))
            .await
            .expect("streaming client connect timed out")
            .expect("streaming client connect failed");
    let host = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    timed_write_all(
        &mut stream,
        format!(
            "POST /external/tree?q=%2F HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
        "streaming request headers write timed out",
    )
    .await;
    for chunk in [b"streamed-".as_slice(), b"request".as_slice()] {
        timed_write_all(
            &mut stream,
            format!("{:x}\r\n", chunk.len()).as_bytes(),
            "streaming request chunk size write timed out",
        )
        .await;
        timed_write_all(&mut stream, chunk, "streaming request body write timed out").await;
        timed_write_all(
            &mut stream,
            b"\r\n",
            "streaming request delimiter write timed out",
        )
        .await;
        timed_flush(&mut stream, "streaming request flush timed out").await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    timed_write_all(
        &mut stream,
        b"0\r\n\r\n",
        "streaming request end write timed out",
    )
    .await;
    let mut response = Vec::new();
    timed_read_to_end(
        &mut stream,
        &mut response,
        "streaming response read timed out",
    )
    .await;
    let body_offset = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    let body: serde_json::Value = serde_json::from_slice(&response[body_offset..]).unwrap();
    assert_eq!(body["uri"], "/base/tree?q=%2F");
    assert_eq!(body["body"], "streamed-request");
    assert_eq!(body["host"], upstream.address.to_string());
    assert_eq!(body["x_custom"], "configured");
    assert_eq!(body["x_forwarded_proto"], "http");
    assert_eq!(body["x_forwarded_port"], url.port().unwrap().to_string());
    assert_eq!(body["x_forwarded_host"], host);
    assert!(body["x_forwarded_for"]
        .as_str()
        .unwrap()
        .starts_with("127.0.0.1"));
    harness.wait_for_activity_after("/external", before).await;
}

#[tokio::test]
#[serial_test::serial]
async fn network_request_body_is_forwarded_incrementally_before_downstream_completion() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let (first_tx, first_rx) = oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut stream, _) =
            timed_accept(&listener, "incremental upstream accept timed out").await;
        let mut received = Vec::new();
        let first_deadline = tokio::time::Instant::now() + RAW_NETWORK_TIMEOUT;
        loop {
            let count = timed_read_until(
                &mut stream,
                &mut received,
                64 * 1024,
                first_deadline,
                "incremental upstream first-body phase timed out",
            )
            .await;
            assert!(count > 0);
            if received
                .windows(b"first".len())
                .any(|part| part == b"first")
            {
                break;
            }
        }
        let _ = first_tx.send(());
        let completion_deadline = tokio::time::Instant::now() + RAW_NETWORK_TIMEOUT;
        while !received.windows(5).any(|part| part == b"0\r\n\r\n") {
            let count = timed_read_until(
                &mut stream,
                &mut received,
                64 * 1024,
                completion_deadline,
                "incremental upstream completion phase timed out",
            )
            .await;
            assert!(count > 0);
        }
        timed_write_all(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            "incremental upstream response write timed out",
        )
        .await;
    });
    let harness =
        ProxyHarness::start(&[], &[("/stream-request", format!("http://{address}"))]).await;
    let url = Url::parse(&harness.base_url).unwrap();
    let mut client = tokio::time::timeout(
        RAW_NETWORK_TIMEOUT,
        tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())),
    )
    .await
    .expect("incremental client connect timed out")
    .expect("incremental client connect failed");
    timed_write_all(
        &mut client,
        format!(
            "POST /stream-request HTTP/1.1\r\nHost: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nfirst\r\n",
            url.authority()
        )
        .as_bytes(),
        "incremental client first write timed out",
    )
    .await;
    timed_flush(&mut client, "incremental client flush timed out").await;
    tokio::time::timeout(Duration::from_millis(500), first_rx)
        .await
        .expect("upstream sees the first chunk before the request is complete")
        .unwrap();
    timed_write_all(
        &mut client,
        b"6\r\nsecond\r\n0\r\n\r\n",
        "incremental client final write timed out",
    )
    .await;
    let mut response = Vec::new();
    timed_read_to_end(
        &mut client,
        &mut response,
        "incremental client response read timed out",
    )
    .await;
    assert!(response.starts_with(b"HTTP/1.1 200"));
    timed_join(upstream, "incremental upstream task did not join").await;
}

async fn read_raw_response(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    let deadline = tokio::time::Instant::now() + RAW_NETWORK_TIMEOUT;
    const MAX_RAW_RESPONSE_HEADER: usize = 64 * 1024;
    const MAX_RAW_RESPONSE_BODY: usize = 2 * 1024 * 1024;
    loop {
        let count = timed_read_until(
            stream,
            &mut response,
            MAX_RAW_RESPONSE_HEADER + MAX_RAW_RESPONSE_BODY,
            deadline,
            "raw response total deadline elapsed",
        )
        .await;
        assert!(count > 0, "raw response ended before its declared body");
        let Some(header_end) = response.windows(4).position(|part| part == b"\r\n\r\n") else {
            assert!(response.len() <= MAX_RAW_RESPONSE_HEADER);
            continue;
        };
        let headers = std::str::from_utf8(&response[..header_end]).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
            })
            .unwrap();
        assert!(length <= MAX_RAW_RESPONSE_BODY);
        if response.len() >= header_end + 4 + length {
            response.truncate(header_end + 4 + length);
            return response;
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn network_reuses_the_same_downstream_socket_and_upstream_connection() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let task_accepts = Arc::clone(&accepts);
    let upstream = tokio::spawn(async move {
        let (mut stream, _) = timed_accept(&listener, "reuse upstream accept timed out").await;
        task_accepts.fetch_add(1, Ordering::Relaxed);
        let mut buffered = Vec::new();
        let requests_deadline = tokio::time::Instant::now() + RAW_NETWORK_TIMEOUT;
        for _ in 0..2 {
            while !buffered.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = timed_read_until(
                    &mut stream,
                    &mut buffered,
                    128 * 1024,
                    requests_deadline,
                    "reuse upstream requests phase timed out",
                )
                .await;
                assert!(count > 0);
            }
            let end = buffered
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            buffered.drain(..end);
            timed_write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                "reuse upstream response write timed out",
            )
            .await;
        }
    });
    let harness = ProxyHarness::start(&[], &[("/reuse", format!("http://{address}"))]).await;
    let url = Url::parse(&harness.base_url).unwrap();
    let mut client = tokio::time::timeout(
        RAW_NETWORK_TIMEOUT,
        tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())),
    )
    .await
    .expect("reuse client connect timed out")
    .expect("reuse client connect failed");
    for index in 0..2 {
        timed_write_all(
            &mut client,
            format!(
                "GET /reuse/{index} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
                url.authority()
            )
            .as_bytes(),
            "reuse client request write timed out",
        )
        .await;
        let response = read_raw_response(&mut client).await;
        assert!(response.starts_with(b"HTTP/1.1 200"));
        assert!(response.ends_with(b"ok"));
    }
    timed_join(upstream, "reuse upstream task did not join").await;
    assert_eq!(accepts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
#[serial_test::serial]
async fn network_host_routing_selects_the_host_prefixed_route() {
    let upstream = EchoServer::start().await;
    let harness = ProxyHarness::start(
        &["--host-routing".to_owned()],
        &[("/example.test/service", upstream.target("/"))],
    )
    .await;
    let response = harness
        .request(
            reqwest::Client::new()
                .get(harness.url("/service/tree"))
                .header(HOST, "example.test"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["uri"], "/service/tree");

    let response = harness
        .request(
            reqwest::Client::new()
                .get(harness.url("/service/tree"))
                .header(HOST, "other.test"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[serial_test::serial]
async fn network_forwarding_runs_before_custom_headers_and_final_hop_scrub_preserves_websocket() {
    let upstream = EchoServer::start().await;
    let harness = ProxyHarness::start(
        &[
            "--custom-header".to_owned(),
            "x-forwarded-for: configured-for".to_owned(),
            "--custom-header".to_owned(),
            "x-forwarded-port: 7443".to_owned(),
            "--custom-header".to_owned(),
            "x-forwarded-proto: configured-proto".to_owned(),
            "--custom-header".to_owned(),
            "x-forwarded-host: configured.example".to_owned(),
            "--custom-header".to_owned(),
            "connection: x-custom-hop".to_owned(),
            "--custom-header".to_owned(),
            "x-custom-hop: secret".to_owned(),
            "--custom-header".to_owned(),
            "upgrade: h2c".to_owned(),
        ],
        &[("/headers", upstream.target("/"))],
    )
    .await;
    let response = harness
        .request(
            reqwest::Client::new()
                .get(harness.url("/headers"))
                .header(CONNECTION, "keep-alive, Upgrade")
                .header(UPGRADE, "websocket"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["x_forwarded_for"], "configured-for");
    assert_eq!(body["x_forwarded_port"], "7443");
    assert_eq!(body["x_forwarded_proto"], "configured-proto");
    assert_eq!(body["x_forwarded_host"], "configured.example");
    assert_eq!(body["connection"], "upgrade");
    assert_eq!(body["upgrade"], "websocket");
    assert!(body["x_custom_hop"].is_null());
}

#[tokio::test]
#[serial_test::serial]
async fn network_streams_chunked_response_and_reuses_downstream_keepalive() {
    let upstream = EchoServer::start().await;
    let harness = ProxyHarness::start(&[], &[("/", upstream.target("/"))]).await;
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let response = harness.request(client.get(harness.url("/keepalive"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONNECTION], "keep-alive");
    }

    let chunked = ChunkedUpstream::start(vec![b"streamed ", b"response"]).await;
    let chunked_harness = ProxyHarness::start(&[], &[("/", chunked.target())]).await;
    let response = chunked_harness
        .request(reqwest::Client::new().get(chunked_harness.url("/chunks")))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap(),
        Bytes::from_static(b"streamed response")
    );
}

#[tokio::test]
#[serial_test::serial]
async fn network_stream_activity_is_observable_before_response_finishes() {
    let upstream =
        ChunkedUpstream::start_with_delay(vec![b"first", b"last"], Duration::from_millis(250))
            .await;
    let harness = ProxyHarness::start(&[], &[("/stream", upstream.target())]).await;
    let before = harness.route("/stream").last_activity;
    let mut response = harness
        .request(reqwest::Client::new().get(harness.url("/stream")))
        .await;

    assert_eq!(response.chunk().await.unwrap().unwrap(), "first");
    assert!(
        harness.route("/stream").last_activity > before,
        "stream activity must publish before the final chunk and logging callback"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn network_redirect_request_and_response_body_traffic_never_records_activity() {
    let upstream = EchoServer::start().await;
    let harness = ProxyHarness::start(
        &["--no-include-prefix".to_owned()],
        &[("/redirect", upstream.target("/"))],
    )
    .await;
    let before = harness.route("/redirect").last_activity;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let response = harness
        .request(
            client
                .post(harness.url("/redirect/redirect-body"))
                .body("request body traffic"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.text().await.unwrap(), "redirect traffic");

    let response = harness
        .request(client.get(harness.url("/redirect/redirect-body")))
        .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.text().await.unwrap(), "redirect traffic");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.route("/redirect").last_activity, before);
}

#[tokio::test]
#[serial_test::serial]
async fn network_unavailable_upstream_is_503_without_activity_and_health_wins() {
    let mut unavailable = ReservedPort::new();
    let target = format!("http://{}", unavailable.release());
    let harness = ProxyHarness::start(
        &[],
        &[("/missing", target.clone()), ("/_chp_healthz", target)],
    )
    .await;
    let before_missing = harness.route("/missing").last_activity;
    let before_health = harness.route("/_chp_healthz").last_activity;

    let response = harness
        .request(reqwest::Client::new().get(harness.url("/missing/path")))
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.route("/missing").last_activity, before_missing);

    let response = harness
        .request(reqwest::Client::new().get(harness.url("/_chp_healthz")))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({"status":"OK"})
    );
    assert_eq!(harness.route("/_chp_healthz").last_activity, before_health);
}

#[tokio::test]
#[serial_test::serial]
async fn network_proxy_failures_never_render_after_response_start_or_downstream_disconnect() {
    let errors = CustomErrorProbe::start().await;
    let (upstream, partial_task) = partial_response_upstream().await;
    let partial = ProxyHarness::start(
        &["--error-target".to_owned(), errors.target()],
        &[("/partial", upstream)],
    )
    .await;
    let url = Url::parse(&partial.url("/partial")).unwrap();
    let mut stream = tokio::time::timeout(
        RAW_NETWORK_TIMEOUT,
        tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())),
    )
    .await
    .expect("partial-response client connect timed out")
    .expect("partial-response client connect failed");
    timed_write_all(
        &mut stream,
        format!(
            "GET /partial HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            url.authority()
        )
        .as_bytes(),
        "partial-response request write timed out",
    )
    .await;
    let mut response = Vec::new();
    timed_read_to_end(
        &mut stream,
        &mut response,
        "partial-response read timed out",
    )
    .await;
    assert!(response.starts_with(b"HTTP/1.1 200"));
    assert_eq!(
        response
            .windows(b"HTTP/1.1 ".len())
            .filter(|window| *window == b"HTTP/1.1 ")
            .count(),
        1,
        "a partial success response must not be followed by a second status"
    );
    timed_join(partial_task, "partial-response upstream task did not join").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(errors.request_count(), 0);

    let (upstream, stalled_task) = stalled_request_upstream().await;
    let disconnected = ProxyHarness::start(
        &["--error-target".to_owned(), errors.target()],
        &[("/disconnect", upstream)],
    )
    .await;
    let url = Url::parse(&disconnected.url("/disconnect")).unwrap();
    let mut stream = tokio::time::timeout(
        RAW_NETWORK_TIMEOUT,
        tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())),
    )
    .await
    .expect("disconnect client connect timed out")
    .expect("disconnect client connect failed");
    timed_write_all(
        &mut stream,
        format!(
            "POST /disconnect HTTP/1.1\r\nHost: {}\r\nContent-Length: 100\r\n\r\nx",
            url.authority()
        )
        .as_bytes(),
        "disconnect request write timed out",
    )
    .await;
    drop(stream);
    timed_join(stalled_task, "stalled upstream task did not join").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        errors.request_count(),
        0,
        "a dead downstream must not trigger custom-error work"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn network_redirects_are_untouched_by_default_and_rewritten_when_enabled() {
    let upstream = EchoServer::start().await;
    let no_redirects = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let untouched = ProxyHarness::start(
        &["--no-include-prefix".to_owned()],
        &[("/external", upstream.target("/"))],
    )
    .await;
    let response = untouched
        .request(no_redirects.get(untouched.url("/external/redirect")))
        .await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        response.headers()[LOCATION],
        format!("http://{}/next", upstream.address)
    );

    let rewritten = ProxyHarness::start(
        &[
            "--no-include-prefix".to_owned(),
            "--auto-rewrite".to_owned(),
            "--protocol-rewrite".to_owned(),
            "https".to_owned(),
        ],
        &[("/external", upstream.target("/"))],
    )
    .await;
    let before = rewritten.route("/external").last_activity;
    let response = rewritten
        .request(no_redirects.get(rewritten.url("/external/redirect")))
        .await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    let public = Url::parse(&rewritten.base_url).unwrap();
    assert_eq!(
        response.headers()[LOCATION],
        format!("https://127.0.0.1:{}/next", public.port().unwrap())
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rewritten.route("/external").last_activity, before);
}

#[tokio::test]
#[serial_test::serial]
async fn network_custom_and_file_errors_follow_chp_fallback_policy() {
    let error_server = EchoServer::start().await;
    let custom = ProxyHarness::start(
        &["--error-target".to_owned(), error_server.target("/errors/")],
        &[],
    )
    .await;
    let response = custom
        .request(reqwest::Client::new().get(custom.url("/missing?q=%2F")))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["content-encoding"], "identity");
    assert!(!response.headers().contains_key("x-disallowed"));
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["method"], "GET");
    assert_eq!(body["uri"], "/errors/404?url=%2Fmissing%3Fq%3D%252F");

    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("404.html"), "specific 404").unwrap();
    fs::write(directory.path().join("error.html"), "generic error").unwrap();
    let files = ProxyHarness::start(
        &[
            "--error-path".to_owned(),
            directory
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ],
        &[],
    )
    .await;
    let response = files
        .request(reqwest::Client::new().get(files.url("/missing")))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["content-type"], "text/html");
    assert_eq!(response.text().await.unwrap(), "specific 404");

    fs::remove_file(directory.path().join("404.html")).unwrap();
    let response = files
        .request(reqwest::Client::new().get(files.url("/missing")))
        .await;
    assert_eq!(response.text().await.unwrap(), "generic error");
    fs::remove_file(directory.path().join("error.html")).unwrap();
    let response = files
        .request(reqwest::Client::new().get(files.url("/missing")))
        .await;
    assert_eq!(response.text().await.unwrap(), "Not Found");

    let mut unavailable = ReservedPort::new();
    let unavailable_target = format!("http://{}", unavailable.release());
    let unavailable_files = ProxyHarness::start(
        &[
            "--error-path".to_owned(),
            directory
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ],
        &[("/unavailable", unavailable_target)],
    )
    .await;
    let response = unavailable_files
        .request(reqwest::Client::new().get(unavailable_files.url("/unavailable")))
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.text().await.unwrap(), "Service Unavailable");
}

#[tokio::test]
#[serial_test::serial]
async fn network_custom_error_redirects_are_not_followed() {
    let destination = CustomErrorProbe::start().await;
    let redirect = RawErrorServer::start(
        format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/escaped\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            destination.address
        )
        .into_bytes(),
        Duration::ZERO,
    )
    .await;
    let harness = ProxyHarness::start(
        &["--error-target".to_owned(), redirect.target("/errors/")],
        &[],
    )
    .await;
    let response = harness
        .request(reqwest::Client::new().get(harness.url("/missing")))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(destination.request_count(), 0);
}

#[tokio::test]
#[serial_test::serial]
async fn network_custom_error_url_preserves_literal_target_shape_and_uri_component_encoding() {
    for (suffix, expected) in [
        ("", "/404?url=%2Fspecial%2F!~*()%27%2525"),
        ("/", "/404?url=%2Fspecial%2F!~*()%27%2525"),
        ("/base", "/base/404?url=%2Fspecial%2F!~*()%27%2525"),
        ("/base/", "/base/404?url=%2Fspecial%2F!~*()%27%2525"),
        (
            "/base?fixed=1",
            "/base?fixed=1/404?url=%2Fspecial%2F!~*()%27%2525",
        ),
    ] {
        let server = RawErrorServer::start(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec(),
            Duration::ZERO,
        )
        .await;
        let target = server.target(suffix);
        let harness = ProxyHarness::start(&["--error-target".to_owned(), target], &[]).await;
        let url = Url::parse(&harness.base_url).unwrap();
        let mut stream = tokio::time::timeout(
            RAW_NETWORK_TIMEOUT,
            tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())),
        )
        .await
        .expect("custom URL client connect timed out")
        .expect("custom URL client connect failed");
        timed_write_all(
            &mut stream,
            format!(
                "GET /special/!~*()'%25 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                url.authority()
            )
            .as_bytes(),
            "custom URL request write timed out",
        )
        .await;
        let mut response = Vec::new();
        timed_read_to_end(
            &mut stream,
            &mut response,
            "custom URL response read timed out",
        )
        .await;
        assert!(response.starts_with(b"HTTP/1.1 404"));
        let RawErrorServer { request, task, .. } = server;
        let request = tokio::time::timeout(RAW_NETWORK_TIMEOUT, request)
            .await
            .expect("custom URL request capture timed out")
            .unwrap();
        assert!(
            request.starts_with(&format!("GET {expected} HTTP/1.1\r\n")),
            "unexpected custom error request for target suffix {suffix:?}: {request:?}"
        );
        timed_join(task, "custom URL server task did not join").await;
    }
}

#[tokio::test]
#[serial_test::serial]
async fn network_custom_error_slow_and_oversized_responses_fall_back_within_bounds() {
    let slow = RawErrorServer::start(
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nslow".to_vec(),
        Duration::from_secs(2),
    )
    .await;
    let harness =
        ProxyHarness::start(&["--error-target".to_owned(), slow.target("/errors/")], &[]).await;
    let response = tokio::time::timeout(
        Duration::from_millis(1500),
        harness.request(reqwest::Client::new().get(harness.url("/missing"))),
    )
    .await
    .expect("custom errors have an explicit total deadline");
    assert_eq!(response.text().await.unwrap(), "Not Found");

    let large = vec![b'x'; 1_048_577];
    let mut fixed = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        large.len()
    )
    .into_bytes();
    fixed.extend_from_slice(&large);
    let oversized = RawErrorServer::start(fixed, Duration::ZERO).await;
    let harness = ProxyHarness::start(
        &["--error-target".to_owned(), oversized.target("/errors/")],
        &[],
    )
    .await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );

    let mut chunked =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    chunked.extend_from_slice(format!("{:x}\r\n", large.len()).as_bytes());
    chunked.extend_from_slice(&large);
    chunked.extend_from_slice(b"\r\n0\r\n\r\n");
    let oversized = RawErrorServer::start(chunked, Duration::ZERO).await;
    let harness = ProxyHarness::start(
        &["--error-target".to_owned(), oversized.target("/errors/")],
        &[],
    )
    .await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );

    let oversized_header = format!(
        "HTTP/1.1 200 OK\r\nX-Oversized: {}\r\nContent-Length: 4\r\nConnection: close\r\n\r\nleak",
        "h".repeat(20 * 1024)
    );
    let oversized = RawErrorServer::start(oversized_header.into_bytes(), Duration::ZERO).await;
    let harness = ProxyHarness::start(
        &["--error-target".to_owned(), oversized.target("/errors/")],
        &[],
    )
    .await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );

    for oversized_raw_header in [
        format!(
            "HTTP/1.1 200 OK\r\nX-OWS:{}x\r\nContent-Length: 4\r\n\r\nleak",
            " \t".repeat(9 * 1024)
        ),
        format!(
            "HTTP/1.1 200 OK\r\nX-Noncanonical {}: x\r\nContent-Length: 4\r\n\r\nleak",
            " ".repeat(17 * 1024)
        ),
    ] {
        let oversized =
            RawErrorServer::start(oversized_raw_header.into_bytes(), Duration::ZERO).await;
        let harness = ProxyHarness::start(
            &["--error-target".to_owned(), oversized.target("/errors/")],
            &[],
        )
        .await;
        assert_eq!(
            harness.get("/missing").await.text().await.unwrap(),
            "Not Found"
        );
        timed_join(oversized.task, "raw oversized-header server did not join").await;
    }
}

fn exact_wire_header_response(wire_header_bytes: usize) -> Vec<u8> {
    let prefix = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nX-Fill: ";
    let mut response = prefix.to_vec();
    response.extend(vec![b'x'; wire_header_bytes - prefix.len() - 4]);
    response.extend_from_slice(b"\r\n\r\naccepted");
    response
}

#[tokio::test]
#[serial_test::serial]
async fn network_custom_error_wire_header_bound_is_exact_for_http_https_and_unix() {
    for wire_header_bytes in [16 * 1024 - 1, 16 * 1024, 16 * 1024 + 1] {
        let expected = if wire_header_bytes <= 16 * 1024 {
            "accepted"
        } else {
            "Not Found"
        };

        let server = RawErrorServer::start(
            exact_wire_header_response(wire_header_bytes),
            Duration::ZERO,
        )
        .await;
        let harness = ProxyHarness::start(
            &["--error-target".to_owned(), server.target("/errors/")],
            &[],
        )
        .await;
        assert_eq!(
            harness.get("/missing").await.text().await.unwrap(),
            expected,
            "HTTP wire-header boundary {wire_header_bytes}"
        );
        tokio::time::timeout(Duration::from_secs(2), server.task)
            .await
            .expect("HTTP boundary server task did not join")
            .unwrap();

        let (target, server) = https_error_server(exact_wire_header_response(wire_header_bytes));
        let harness = ProxyHarness::start(
            &["--insecure".to_owned(), "--error-target".to_owned(), target],
            &[],
        )
        .await;
        assert_eq!(
            harness.get("/missing").await.text().await.unwrap(),
            expected
        );
        join_thread_with_timeout(server, "HTTPS boundary server thread did not join");

        #[cfg(unix)]
        {
            let (_directory, target, task) = unix_error_server(
                exact_wire_header_response(wire_header_bytes),
                Duration::ZERO,
            )
            .await;
            let harness = ProxyHarness::start(&["--error-target".to_owned(), target], &[]).await;
            assert_eq!(
                harness.get("/missing").await.text().await.unwrap(),
                expected
            );
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("Unix boundary server task did not join")
                .unwrap();
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn network_https_custom_errors_support_insecure_success_and_verified_failure() {
    let success = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\ntls error".to_vec();
    let (target, server) = https_error_server(success.clone());
    let harness = ProxyHarness::start(
        &["--insecure".to_owned(), "--error-target".to_owned(), target],
        &[],
    )
    .await;
    let response = harness.get("/missing").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "tls error");
    join_thread_with_timeout(server, "insecure HTTPS custom-error server did not join");

    let (target, server) =
        https_error_server_with_read_delay(success.clone(), Duration::from_millis(100));
    let harness = ProxyHarness::start(
        &["--insecure".to_owned(), "--error-target".to_owned(), target],
        &[],
    )
    .await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "tls error"
    );
    join_thread_with_timeout(server, "backpressured HTTPS server did not join");

    let (target, server) = https_error_server(success);
    let harness = ProxyHarness::start(&["--error-target".to_owned(), target], &[]).await;
    let response = harness.get("/missing").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "Not Found");
    join_thread_with_timeout(server, "verified-failure HTTPS server did not join");

    let large = vec![b'x'; 1_048_577];
    let mut fixed = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        large.len()
    )
    .into_bytes();
    fixed.extend_from_slice(&large);
    let (target, server) = https_error_server(fixed);
    let harness = ProxyHarness::start(
        &["--insecure".to_owned(), "--error-target".to_owned(), target],
        &[],
    )
    .await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );
    join_thread_with_timeout(server, "fixed-size HTTPS server did not join");

    let mut chunked =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    chunked.extend_from_slice(format!("{:x}\r\n", large.len()).as_bytes());
    chunked.extend_from_slice(&large);
    chunked.extend_from_slice(b"\r\n0\r\n\r\n");
    let (target, server) = https_error_server(chunked);
    let harness = ProxyHarness::start(
        &["--insecure".to_owned(), "--error-target".to_owned(), target],
        &[],
    )
    .await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );
    join_thread_with_timeout(server, "chunked HTTPS server did not join");
}

#[tokio::test]
#[serial_test::serial]
async fn network_https_custom_errors_apply_private_ca_and_client_identity() {
    let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nmtls error".to_vec();
    let directory = tempfile::tempdir().unwrap();
    let (target, ca, certificate, key, server) =
        private_ca_mtls_error_server(&directory, response.clone());
    let harness = ProxyHarness::start(
        &[
            "--error-target".to_owned(),
            target,
            "--client-ssl-ca".to_owned(),
            ca.to_string_lossy().into_owned(),
            "--client-ssl-cert".to_owned(),
            certificate.to_string_lossy().into_owned(),
            "--client-ssl-key".to_owned(),
            key.to_string_lossy().into_owned(),
        ],
        &[],
    )
    .await;
    let rendered = harness.get("/missing").await;
    assert_eq!(rendered.status(), StatusCode::NOT_FOUND);
    let rendered = rendered.text().await.unwrap();
    join_thread_with_timeout(server, "verified mTLS custom-error server did not finish");
    assert_eq!(rendered, "mtls error");

    let failed_directory = tempfile::tempdir().unwrap();
    let (target, ca, _certificate, _key, server) =
        private_ca_mtls_error_server(&failed_directory, response);
    let harness = ProxyHarness::start(
        &[
            "--error-target".to_owned(),
            target,
            "--client-ssl-ca".to_owned(),
            ca.to_string_lossy().into_owned(),
        ],
        &[],
    )
    .await;
    let rendered = harness.get("/missing").await;
    assert_eq!(rendered.status(), StatusCode::NOT_FOUND);
    assert_eq!(rendered.text().await.unwrap(), "Not Found");
    join_thread_with_timeout(
        server,
        "failed client-auth custom-error server did not finish",
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn network_error_files_are_bounded_and_cannot_escape_the_configured_directory() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), "outside secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), directory.path().join("404.html")).unwrap();
    let harness = ProxyHarness::start(
        &[
            "--error-path".to_owned(),
            directory
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ],
        &[],
    )
    .await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );

    fs::remove_file(directory.path().join("404.html")).unwrap();
    fs::write(directory.path().join("404.html"), vec![b'x'; 1_048_577]).unwrap();
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );

    fs::remove_file(directory.path().join("404.html")).unwrap();
    let fifo = directory.path().join("404.html");
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    tokio::time::timeout(Duration::from_secs(3), async {
        let client = reqwest::Client::new();
        for _ in 0..8 {
            let mut requests = tokio::task::JoinSet::new();
            for _ in 0..16 {
                let client = client.clone();
                let url = harness.url("/missing");
                requests.spawn(async move {
                    client.get(url).send().await.unwrap().text().await.unwrap()
                });
            }
            while let Some(response) = requests.join_next().await {
                assert_eq!(response.unwrap(), "Not Found");
            }
        }
    })
    .await
    .expect("FIFO error-file rejection blocked or exhausted the worker pool");
    assert!(exact_health_ready(&reqwest::Client::new(), &harness.base_url).await);
}

#[cfg(unix)]
async fn unix_error_server(
    response: Vec<u8>,
    delay: Duration,
) -> (tempfile::TempDir, String, tokio::task::JoinHandle<()>) {
    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("bounded-errors.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) =
            timed_unix_accept(&listener, "Unix custom-error server accept timed out").await;
        let mut request = [0; 2048];
        timed_read(
            &mut stream,
            &mut request,
            "Unix custom-error request read timed out",
        )
        .await;
        tokio::time::sleep(delay).await;
        timed_write_all_allow_disconnect(
            &mut stream,
            &response,
            "Unix custom-error response write timed out",
        )
        .await;
    });
    let encoded = socket_path.to_string_lossy().replace('/', "%2F");
    (directory, format!("http+unix://{encoded}/errors/"), task)
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn network_unix_custom_errors_apply_deadlines_and_fixed_chunked_header_bounds() {
    let (_directory, target, task) = unix_error_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nslow".to_vec(),
        Duration::from_secs(2),
    )
    .await;
    let harness = ProxyHarness::start(&["--error-target".to_owned(), target], &[]).await;
    let response = tokio::time::timeout(Duration::from_millis(1500), harness.get("/missing"))
        .await
        .expect("Unix custom errors have a total deadline");
    assert_eq!(response.text().await.unwrap(), "Not Found");
    task.abort();

    let large = vec![b'x'; 1_048_577];
    let mut fixed =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", large.len()).into_bytes();
    fixed.extend_from_slice(&large);
    let (_directory, target, task) = unix_error_server(fixed, Duration::ZERO).await;
    let harness = ProxyHarness::start(&["--error-target".to_owned(), target], &[]).await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );
    timed_join(task, "Unix fixed-size server task did not join").await;

    let mut chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    chunked.extend_from_slice(format!("{:x}\r\n", large.len()).as_bytes());
    chunked.extend_from_slice(&large);
    chunked.extend_from_slice(b"\r\n0\r\n\r\n");
    let (_directory, target, task) = unix_error_server(chunked, Duration::ZERO).await;
    let harness = ProxyHarness::start(&["--error-target".to_owned(), target], &[]).await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );
    timed_join(task, "Unix chunked server task did not join").await;

    let oversized_header = format!(
        "HTTP/1.1 200 OK\r\nX-Oversized: {}\r\nContent-Length: 4\r\n\r\nleak",
        "h".repeat(20 * 1024)
    );
    let (_directory, target, task) =
        unix_error_server(oversized_header.into_bytes(), Duration::ZERO).await;
    let harness = ProxyHarness::start(&["--error-target".to_owned(), target], &[]).await;
    assert_eq!(
        harness.get("/missing").await.text().await.unwrap(),
        "Not Found"
    );
    timed_join(task, "Unix oversized-header server task did not join").await;
}

#[tokio::test]
#[serial_test::serial]
async fn network_typed_internal_errors_are_500_and_custom_failure_uses_reason_phrase() {
    let invalid = ProxyHarness::start(&[], &[("/invalid", "not a URL".to_owned())]).await;
    let before = invalid.route("/invalid").last_activity;
    let response = invalid
        .request(reqwest::Client::new().get(invalid.url("/invalid")))
        .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.text().await.unwrap(), "Internal Server Error");
    assert_eq!(invalid.route("/invalid").last_activity, before);

    let upstream = EchoServer::start().await;
    let redirect = ProxyHarness::start(
        &[
            "--no-include-prefix".to_owned(),
            "--auto-rewrite".to_owned(),
        ],
        &[("/redirect", upstream.target("/"))],
    )
    .await;
    let url = Url::parse(&redirect.base_url).unwrap();
    let mut stream = tokio::time::timeout(
        RAW_NETWORK_TIMEOUT,
        tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())),
    )
    .await
    .expect("internal-error client connect timed out")
    .expect("internal-error client connect failed");
    timed_write_all(
        &mut stream,
        b"GET /redirect/redirect HTTP/1.1\r\nHost: bad%host\r\nConnection: close\r\n\r\n",
        "internal-error request write timed out",
    )
    .await;
    let mut response = Vec::new();
    timed_read_to_end(
        &mut stream,
        &mut response,
        "internal-error response read timed out",
    )
    .await;
    assert!(
        response.starts_with(b"HTTP/1.1 500"),
        "an upstream response-filter failure must remain an internal 500: {}",
        String::from_utf8_lossy(&response)
    );

    let mut unavailable = ReservedPort::new();
    let errors = ProxyHarness::start(
        &[
            "--error-target".to_owned(),
            format!("http://{}/", unavailable.release()),
        ],
        &[],
    )
    .await;
    let response = errors
        .request(reqwest::Client::new().get(errors.url("/missing")))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "Not Found");
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn network_unix_custom_error_target_uses_get_and_copies_only_content_headers() {
    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("errors.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let (request_tx, request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) =
            timed_unix_accept(&listener, "Unix content-header server accept timed out").await;
        let mut request = vec![0; 4096];
        let count = timed_read(
            &mut stream,
            &mut request,
            "Unix content-header request read timed out",
        )
        .await;
        let _ = request_tx.send(String::from_utf8_lossy(&request[..count]).into_owned());
        timed_write_all(
            &mut stream,
            b"HTTP/1.1 418 Teapot\r\nContent-Type: text/plain\r\nContent-Encoding: identity\r\nX-Disallowed: secret\r\nContent-Length: 10\r\nConnection: close\r\n\r\nunix error",
            "Unix content-header response write timed out",
        )
        .await;
    });
    let encoded_socket = socket_path.to_string_lossy().replace('/', "%2F");
    let harness = ProxyHarness::start(
        &[
            "--error-target".to_owned(),
            format!("http+unix://{encoded_socket}/errors/"),
        ],
        &[],
    )
    .await;
    let response = harness
        .request(reqwest::Client::new().get(harness.url("/missing?q=%2F")))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(response.headers()["content-encoding"], "identity");
    assert!(!response.headers().contains_key("x-disallowed"));
    assert_eq!(response.text().await.unwrap(), "unix error");
    let request = tokio::time::timeout(RAW_NETWORK_TIMEOUT, request_rx)
        .await
        .expect("Unix request capture timed out")
        .unwrap();
    assert!(request.starts_with("GET /errors/404?url=%2Fmissing%3Fq%3D%252F HTTP/1.1\r\n"));
    timed_join(server, "Unix content-header server task did not join").await;
}

struct FailingActivityStore {
    routes: tokio::sync::RwLock<BTreeMap<RouteKey, RouteData>>,
    attempts: AtomicUsize,
    panic_on_update: AtomicBool,
}

#[async_trait]
impl Store for FailingActivityStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: serde_json::Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut data = RouteData {
            target,
            last_activity: chrono::Utc::now(),
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(
        &self,
        _key: &RouteKey,
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StoreError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        if self.panic_on_update.swap(false, Ordering::AcqRel) {
            panic!("backend activity mutation panicked");
        }
        Err(StoreError::message("activity persistence unavailable"))
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        Ok(self.routes.write().await.remove(key))
    }
}

#[tokio::test]
async fn activity_memory_update_is_immediate_coalesced_and_survives_persistence_failure() {
    let key = RouteKey::parse("/activity").unwrap();
    let initial = chrono::Utc::now() - chrono::Duration::minutes(1);
    let store = Arc::new(FailingActivityStore {
        routes: tokio::sync::RwLock::new(BTreeMap::from([(
            key.clone(),
            RouteData {
                target: "http://upstream.example".to_owned(),
                last_activity: initial,
                extra: serde_json::Map::from_iter([(
                    "unknown".to_owned(),
                    serde_json::json!({"preserved": true}),
                )]),
            },
        )])),
        attempts: AtomicUsize::new(0),
        panic_on_update: AtomicBool::new(false),
    });
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let metrics = Arc::new(Metrics::new());
    let writer = ActivityWriter::start_with_metrics(Arc::clone(&registry), 1, Arc::clone(&metrics));
    let first = initial + chrono::Duration::seconds(1);
    let newest = initial + chrono::Duration::seconds(3);

    writer.record_at(&key, first);
    writer.record_at(&key, initial + chrono::Duration::seconds(2));
    writer.record_at(&key, newest);

    let observed = registry.get(&key).unwrap();
    assert_eq!(observed.last_activity, newest);
    assert_eq!(
        observed.extra["unknown"],
        serde_json::json!({"preserved": true})
    );
    timed_activity_flush(&writer).await;
    assert_eq!(store.attempts.load(Ordering::Relaxed), 1);
    assert_eq!(writer.persistence_errors(), 1);
    assert_eq!(metrics.snapshot().activity_persistence_failures, 1);
    assert_eq!(registry.get(&key).unwrap().last_activity, newest);

    let panic_store = Arc::new(FailingActivityStore {
        routes: tokio::sync::RwLock::new(BTreeMap::from([(
            key.clone(),
            RouteData {
                target: "http://panic.example".to_owned(),
                last_activity: initial,
                extra: Default::default(),
            },
        )])),
        attempts: AtomicUsize::new(0),
        panic_on_update: AtomicBool::new(true),
    });
    let panic_registry = RouteRegistry::load(panic_store.clone()).await.unwrap();
    let panic_writer = ActivityWriter::start(Arc::clone(&panic_registry), 1);
    panic_writer.record_at(&key, first);
    timed_activity_flush(&panic_writer).await;
    assert_eq!(panic_writer.persistence_errors(), 1);
    assert_eq!(
        panic_registry.mutation_status().seal,
        MutationSeal::BackendPanic
    );
    assert_eq!(panic_registry.mutation_status().active_mutations, 0);

    panic_writer.record_at(&key, newest);
    timed_activity_flush(&panic_writer).await;
    assert_eq!(panic_writer.persistence_errors(), 2);
    assert_eq!(panic_store.attempts.load(Ordering::Relaxed), 1);
    assert_eq!(panic_registry.mutation_status().active_mutations, 0);
}

struct StalledActivityStore {
    routes: tokio::sync::RwLock<BTreeMap<RouteKey, RouteData>>,
    entries: std::sync::Mutex<Vec<RouteKey>>,
    writes: std::sync::Mutex<Vec<(RouteKey, chrono::DateTime<chrono::Utc>)>>,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

#[async_trait]
impl Store for StalledActivityStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        _key: RouteKey,
        _target: String,
        _extra: serde_json::Map<String, serde_json::Value>,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!("activity test only persists timestamps")
    }

    async fn put(&self, _key: RouteKey, _data: RouteData) -> Result<(), StoreError> {
        unreachable!("activity test only persists timestamps")
    }

    async fn put_preserving_activity(
        &self,
        _key: RouteKey,
        _data: RouteData,
        _activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        unreachable!("activity test only persists timestamps")
    }

    async fn update_activity(
        &self,
        key: &RouteKey,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StoreError> {
        self.entries.lock().unwrap().push(key.clone());
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        self.writes.lock().unwrap().push((key.clone(), at));
        if let Some(route) = self.routes.write().await.get_mut(key) {
            route.last_activity = at;
        }
        Ok(())
    }

    async fn delete(&self, _key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        unreachable!("activity test only persists timestamps")
    }
}

#[tokio::test]
async fn activity_pending_keys_are_bounded_and_recover_with_newest_accepted_timestamp() {
    let initial = chrono::Utc::now() - chrono::Duration::minutes(1);
    let keys: Vec<_> = (0..4)
        .map(|index| RouteKey::parse(&format!("/churn/{index}")).unwrap())
        .collect();
    let routes = keys
        .iter()
        .cloned()
        .map(|key| {
            (
                key,
                RouteData {
                    target: "http://upstream.example".to_owned(),
                    last_activity: initial,
                    extra: Default::default(),
                },
            )
        })
        .collect();
    let store = Arc::new(StalledActivityStore {
        routes: tokio::sync::RwLock::new(routes),
        entries: std::sync::Mutex::new(Vec::new()),
        writes: std::sync::Mutex::new(Vec::new()),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let metrics = Arc::new(Metrics::new());
    let writer = ActivityWriter::start_with_metrics(Arc::clone(&registry), 2, metrics.clone());

    writer.record_at(&keys[0], initial + chrono::Duration::seconds(1));
    store.entered.acquire().await.unwrap().forget();
    writer.record_at(&keys[1], initial + chrono::Duration::seconds(1));
    let newest = initial + chrono::Duration::seconds(3);
    writer.record_at(&keys[1], newest);
    writer.record_at(&keys[2], initial + chrono::Duration::seconds(2));
    let in_flight_newest = initial + chrono::Duration::seconds(5);
    writer.record_at(&keys[0], in_flight_newest);
    writer.record_at(&keys[3], initial + chrono::Duration::seconds(4));

    assert_eq!(writer.pending_routes(), 2);
    assert_eq!(writer.dropped_observations(), 2);
    assert_eq!(metrics.snapshot().activity_dropped, 2);

    store.release.add_permits(3);
    timed_activity_flush(&writer).await;
    let writes = store.writes.lock().unwrap().clone();
    assert_eq!(writes.len(), 3);
    assert!(writes.contains(&(keys[1].clone(), newest)));
    assert!(writes.contains(&(keys[0].clone(), in_flight_newest)));
    assert!(writes
        .iter()
        .all(|(key, _)| key != &keys[2] && key != &keys[3]));
    assert_eq!(
        store.routes.read().await[&keys[0]].last_activity,
        in_flight_newest
    );
    assert_eq!(writer.pending_routes(), 0);
}

#[tokio::test]
async fn activity_flush_uses_an_acceptance_watermark_for_a_continuously_advancing_key() {
    let initial = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let key = RouteKey::parse("/hot-flush").unwrap();
    let store = Arc::new(StalledActivityStore {
        routes: tokio::sync::RwLock::new(BTreeMap::from([(
            key.clone(),
            RouteData {
                target: "http://upstream.example".to_owned(),
                last_activity: initial,
                extra: Default::default(),
            },
        )])),
        entries: std::sync::Mutex::new(Vec::new()),
        writes: std::sync::Mutex::new(Vec::new()),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let writer = ActivityWriter::start(Arc::clone(&registry), 1);
    let before_watermark = initial + chrono::Duration::seconds(1);
    let after_watermark = initial + chrono::Duration::seconds(2);
    let newest = initial + chrono::Duration::seconds(3);

    writer.record_at(&key, before_watermark);
    store.entered.acquire().await.unwrap().forget();
    let mut flush = Box::pin(writer.flush());
    tokio::select! {
        biased;
        () = &mut flush => panic!("blocked pre-watermark activity flushed early"),
        () = tokio::task::yield_now() => {}
    }

    writer.record_at(&key, after_watermark);
    store.release.add_permits(1);
    store.entered.acquire().await.unwrap().forget();
    writer.record_at(&key, newest);

    tokio::time::timeout(Duration::from_millis(100), &mut flush)
        .await
        .expect("post-watermark activity starved the earlier flush");
    assert_eq!(writer.pending_routes(), 1);
    assert_eq!(
        store.writes.lock().unwrap().as_slice(),
        &[(key.clone(), before_watermark)]
    );

    store.release.add_permits(1);
    store.entered.acquire().await.unwrap().forget();
    store.release.add_permits(1);
    timed_activity_flush(&writer).await;
    assert_eq!(
        store.writes.lock().unwrap().as_slice(),
        &[
            (key.clone(), before_watermark),
            (key.clone(), after_watermark),
            (key, newest),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn activity_flush_tracks_oldest_acceptance_in_a_dequeued_coalesced_write() {
    let initial = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let blocker = RouteKey::parse("/flush-blocker").unwrap();
    let key = RouteKey::parse("/coalesced-flush").unwrap();
    let store = Arc::new(StalledActivityStore {
        routes: tokio::sync::RwLock::new(BTreeMap::from([
            (
                blocker.clone(),
                RouteData {
                    target: "http://upstream.example".to_owned(),
                    last_activity: initial,
                    extra: Default::default(),
                },
            ),
            (
                key.clone(),
                RouteData {
                    target: "http://upstream.example".to_owned(),
                    last_activity: initial,
                    extra: Default::default(),
                },
            ),
        ])),
        entries: std::sync::Mutex::new(Vec::new()),
        writes: std::sync::Mutex::new(Vec::new()),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let writer = ActivityWriter::start(Arc::clone(&registry), 2);
    let blocker_at = initial + chrono::Duration::milliseconds(1);
    let before_watermark = initial + chrono::Duration::seconds(1);
    let coalesced = initial + chrono::Duration::seconds(2);
    let later = initial + chrono::Duration::seconds(3);

    writer.record_at(&blocker, blocker_at);
    store.entered.acquire().await.unwrap().forget();
    writer.record_at(&key, before_watermark);
    let mut flush = Box::pin(writer.flush());
    tokio::select! {
        biased;
        () = &mut flush => panic!("pending activity flushed before worker dequeue"),
        () = async {} => {}
    }
    writer.record_at(&key, coalesced);

    store.release.add_permits(1);
    store.entered.acquire().await.unwrap().forget();
    assert_eq!(
        store.entries.lock().unwrap().as_slice(),
        &[blocker.clone(), key.clone()]
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut flush)
            .await
            .is_err(),
        "flush returned while its oldest acceptance was in flight"
    );

    writer.record_at(&key, later);
    store.release.add_permits(1);
    store.entered.acquire().await.unwrap().forget();
    tokio::time::timeout(Duration::from_millis(100), &mut flush)
        .await
        .expect("later activity prevented the coalesced flush watermark from completing");
    assert_eq!(writer.pending_routes(), 1);
    assert_eq!(
        store.writes.lock().unwrap().as_slice(),
        &[(blocker.clone(), blocker_at), (key.clone(), coalesced)]
    );

    store.release.add_permits(1);
    timed_activity_flush(&writer).await;
    assert_eq!(
        store.writes.lock().unwrap().as_slice(),
        &[
            (blocker, blocker_at),
            (key.clone(), coalesced),
            (key, later),
        ]
    );
}

#[tokio::test]
async fn continuously_advancing_activity_key_yields_to_ready_peers() {
    let initial = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let keys: Vec<_> = ["/a-hot", "/b-ready", "/c-ready"]
        .into_iter()
        .map(|path| RouteKey::parse(path).unwrap())
        .collect();
    let routes = keys
        .iter()
        .cloned()
        .map(|key| {
            (
                key,
                RouteData {
                    target: "http://upstream.example".to_owned(),
                    last_activity: initial,
                    extra: Default::default(),
                },
            )
        })
        .collect();
    let store = Arc::new(StalledActivityStore {
        routes: tokio::sync::RwLock::new(routes),
        entries: std::sync::Mutex::new(Vec::new()),
        writes: std::sync::Mutex::new(Vec::new()),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let writer = ActivityWriter::start(Arc::clone(&registry), 3);

    writer.record_at(&keys[0], initial + chrono::Duration::seconds(1));
    store.entered.acquire().await.unwrap().forget();
    writer.record_at(&keys[1], initial + chrono::Duration::seconds(1));
    writer.record_at(&keys[2], initial + chrono::Duration::seconds(1));
    writer.record_at(&keys[0], initial + chrono::Duration::seconds(2));

    for (expected, hot_second) in [(&keys[1], 3), (&keys[2], 4)] {
        store.release.add_permits(1);
        store.entered.acquire().await.unwrap().forget();
        assert_eq!(store.entries.lock().unwrap().last(), Some(expected));
        writer.record_at(&keys[0], initial + chrono::Duration::seconds(hot_second));
    }

    store.release.add_permits(1);
    store.entered.acquire().await.unwrap().forget();
    assert_eq!(store.entries.lock().unwrap().last(), Some(&keys[0]));
    store.release.add_permits(1);

    tokio::time::timeout(Duration::from_secs(1), writer.flush())
        .await
        .expect("fair activity queue did not flush within its bound");
    let drained = registry.drain_mutations(Duration::from_secs(1)).await;
    assert!(!drained.timed_out);
    assert_eq!(drained.active_mutations, 0);
    assert_eq!(writer.pending_routes(), 0);
    let entries = store.entries.lock().unwrap().clone();
    assert_eq!(entries[..3], keys);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistenceOperation {
    Activity,
    Add,
    Put,
    Delete,
}

struct OrderedPersistenceStore {
    routes: tokio::sync::RwLock<BTreeMap<RouteKey, RouteData>>,
    blocked: PersistenceOperation,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    operations: std::sync::Mutex<Vec<PersistenceOperation>>,
    replacement_activity: chrono::DateTime<chrono::Utc>,
}

impl OrderedPersistenceStore {
    async fn gate(&self, operation: PersistenceOperation) {
        self.operations.lock().unwrap().push(operation);
        self.entered.add_permits(1);
        if operation == self.blocked {
            self.release.acquire().await.unwrap().forget();
        }
    }

    fn operations(&self) -> Vec<PersistenceOperation> {
        self.operations.lock().unwrap().clone()
    }
}

#[async_trait]
impl Store for OrderedPersistenceStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        Ok(self.routes.read().await.clone())
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: serde_json::Map<String, serde_json::Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.gate(PersistenceOperation::Add).await;
        let mut data = RouteData {
            target,
            last_activity: self.replacement_activity,
            extra,
        };
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.gate(PersistenceOperation::Put).await;
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.gate(PersistenceOperation::Put).await;
        let mut routes = self.routes.write().await;
        if let Some(floor) = activity_floor.current() {
            data.last_activity = data.last_activity.max(floor);
        }
        routes.insert(key, data.clone());
        Ok(data)
    }

    async fn update_activity(
        &self,
        key: &RouteKey,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StoreError> {
        // Deliberately model a backend that reads then writes the whole record.
        // Serialization must keep this stale clone from racing replacements/deletes.
        let stale = self.routes.read().await.get(key).cloned();
        self.gate(PersistenceOperation::Activity).await;
        if let Some(mut route) = stale {
            route.last_activity = at;
            self.routes.write().await.insert(key.clone(), route);
        }
        Ok(())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        self.gate(PersistenceOperation::Delete).await;
        Ok(self.routes.write().await.remove(key))
    }
}

async fn assert_activity_and_management_are_persisted_in_order(
    management: PersistenceOperation,
    activity_first: bool,
) {
    let key = RouteKey::parse("/ordered").unwrap();
    let initial = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let activity_at = initial + chrono::Duration::seconds(4);
    let replacement_at = initial + chrono::Duration::seconds(2);
    let store = Arc::new(OrderedPersistenceStore {
        routes: tokio::sync::RwLock::new(BTreeMap::from([(
            key.clone(),
            RouteData {
                target: "http://old.example".to_owned(),
                last_activity: initial,
                extra: Default::default(),
            },
        )])),
        blocked: if activity_first {
            PersistenceOperation::Activity
        } else {
            management
        },
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
        operations: std::sync::Mutex::new(Vec::new()),
        replacement_activity: replacement_at,
    });
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let writer = ActivityWriter::start(Arc::clone(&registry), 4);

    let start_management = || {
        let registry = Arc::clone(&registry);
        let key = key.clone();
        tokio::spawn(async move {
            match management {
                PersistenceOperation::Add => registry
                    .add(key, "http://new.example".to_owned(), Default::default())
                    .await
                    .map(|_| ()),
                PersistenceOperation::Put => {
                    registry
                        .put(
                            key,
                            RouteData {
                                target: "http://new.example".to_owned(),
                                last_activity: replacement_at,
                                extra: Default::default(),
                            },
                        )
                        .await
                }
                PersistenceOperation::Delete => registry.delete(&key).await.map(|_| ()),
                PersistenceOperation::Activity => unreachable!(),
            }
        })
    };

    let management_task;
    if activity_first {
        writer.record_at(&key, activity_at);
        tokio::time::timeout(Duration::from_secs(1), store.entered.acquire())
            .await
            .expect("activity backend write did not enter")
            .unwrap()
            .forget();
        management_task = start_management();
        tokio::time::timeout(Duration::from_secs(1), async {
            while registry.mutation_status().active_mutations < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("management mutation was not admitted behind activity");
        assert_eq!(store.operations(), vec![PersistenceOperation::Activity]);
    } else {
        management_task = start_management();
        tokio::time::timeout(Duration::from_secs(1), store.entered.acquire())
            .await
            .expect("management backend write did not enter")
            .unwrap()
            .forget();
        writer.record_at(&key, activity_at);
        tokio::time::timeout(Duration::from_secs(1), async {
            while registry.mutation_status().active_mutations < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("activity mutation was not admitted behind management");
        assert_eq!(store.operations(), vec![management]);
    }

    store.release.add_permits(4);
    if activity_first || management != PersistenceOperation::Delete {
        tokio::time::timeout(Duration::from_secs(1), async {
            while store.operations().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("serialized second backend operation did not enter");
    }
    tokio::time::timeout(Duration::from_secs(1), writer.flush())
        .await
        .expect("activity writer did not flush within its bound");
    tokio::time::timeout(Duration::from_secs(1), management_task)
        .await
        .expect("management task did not join")
        .unwrap()
        .unwrap();

    let persisted = store.snapshot().await.unwrap();
    let reloaded = RouteRegistry::load(store.clone()).await.unwrap();
    assert_eq!(
        reloaded.all(),
        persisted,
        "restart-equivalent load diverged"
    );
    let expected_operations = if activity_first {
        vec![PersistenceOperation::Activity, management]
    } else if management == PersistenceOperation::Delete {
        vec![management]
    } else {
        vec![management, PersistenceOperation::Activity]
    };
    assert_eq!(
        store.operations(),
        expected_operations,
        "add/put must not commit and then issue a corrective activity write"
    );
    match management {
        PersistenceOperation::Add | PersistenceOperation::Put => {
            let route = persisted.get(&key).unwrap();
            assert_eq!(route.target, "http://new.example");
            assert_eq!(route.last_activity, replacement_at.max(activity_at));
            assert_eq!(registry.get(&key).unwrap(), route.clone());
        }
        PersistenceOperation::Delete => assert!(!persisted.contains_key(&key)),
        PersistenceOperation::Activity => unreachable!(),
    }
}

#[tokio::test]
async fn activity_backend_writes_are_ordered_with_add_put_and_delete() {
    for management in [
        PersistenceOperation::Add,
        PersistenceOperation::Put,
        PersistenceOperation::Delete,
    ] {
        assert_activity_and_management_are_persisted_in_order(management, true).await;
        assert_activity_and_management_are_persisted_in_order(management, false).await;
    }
}

fn options(include_prefix: bool, prepend_path: bool) -> ProxyOptions {
    ProxyOptions {
        x_forward: true,
        prepend_path,
        include_prefix,
        auto_rewrite: false,
        change_origin: false,
        protocol_rewrite: None,
        custom_headers: BTreeMap::new(),
        verify_upstream_tls: true,
        host_routing: false,
        timeout_ms: None,
        proxy_timeout_ms: None,
        keep_alive_timeout_ms: Some(5_000),
    }
}

fn route(prefix: &str, target: &str) -> UpstreamRoute {
    UpstreamRoute::new(
        RouteKey::parse(prefix).unwrap(),
        Target::parse(&Url::parse(target).unwrap()).unwrap(),
    )
}

#[test]
fn uri_matrix_matches_chp_path_joining() {
    let cases = [
        (true, true, "/base/user/alice/tree?a=%2F&b=1+2"),
        (false, true, "/base/tree?a=%2F&b=1+2"),
        (true, false, "/user/alice/tree?a=%2F&b=1+2"),
        (false, false, "/tree?a=%2F&b=1+2"),
    ];

    for (include_prefix, prepend_path, expected) in cases {
        let output = build_upstream_uri(
            &route("/user/alice", "http://upstream.example/base/"),
            &Uri::from_static("/user/alice/tree?a=%2F&b=1+2"),
            &options(include_prefix, prepend_path),
        )
        .unwrap();
        assert_eq!(
            output, expected,
            "include={include_prefix} prepend={prepend_path}"
        );
    }
}

#[test]
fn uri_join_preserves_literal_duplicate_slashes_like_http_proxy() {
    let output = build_upstream_uri(
        &route("/user/alice", "http://upstream.example//base//"),
        &Uri::from_static("/user/alice//tree///leaf?q=//preserved"),
        &options(false, true),
    )
    .unwrap();
    assert_eq!(output, "//base///tree///leaf?q=//preserved");
}

#[test]
fn uri_root_route_and_target_root_keep_one_leading_slash() {
    for include_prefix in [false, true] {
        for prepend_path in [false, true] {
            let output = build_upstream_uri(
                &route("/", "http://upstream.example/"),
                &Uri::from_static("/escaped%2Fsegment?q=%25FF"),
                &options(include_prefix, prepend_path),
            )
            .unwrap();
            assert_eq!(output, "/escaped%2Fsegment?q=%25FF");
        }
    }
}

#[test]
fn uri_host_routing_prefix_is_removed_at_a_decoded_path_boundary() {
    let mut opts = options(false, true);
    opts.host_routing = true;
    let output = build_upstream_uri(
        &route("/example.test/user/alice", "http://upstream.example/base"),
        &Uri::from_static("/user/alice/tree?q=%2F"),
        &opts,
    )
    .unwrap();
    // CHP slices the raw request URL by the decoded route prefix's JS string
    // length, including the host-routing component. The pinned CHP 5.3.0
    // process therefore forwards only the target path in this case.
    assert_eq!(output, "/base");
}

#[test]
fn uri_no_include_prefix_reproduces_chp_raw_encoded_slice() {
    let output = build_upstream_uri(
        &route("/b@r/b r", "http://upstream.example/foo"),
        &Uri::from_static("/b%40r/b%20r/rest/of/it"),
        &options(false, true),
    )
    .unwrap();
    assert_eq!(output, "/foo/%20r/rest/of/it");
}

#[test]
fn uri_join_preserves_repeated_slashes_inside_each_segment() {
    let output = build_upstream_uri(
        &route("/user", "http://upstream.example/base//keep/"),
        &Uri::from_static("/user///tail"),
        &options(true, true),
    )
    .unwrap();
    assert_eq!(output, "/base//keep/user///tail");
}

proptest! {
    #[test]
    fn uri_query_bytes_are_preserved(
        include_prefix in any::<bool>(),
        prepend_path in any::<bool>(),
        query in prop::collection::vec(prop_oneof![Just(b'a'), Just(b'Z'), Just(b'0'), Just(b'%'), Just(b'2'), Just(b'F'), Just(b'+'), Just(b'='), Just(b'&'), Just(b';'), Just(b':')], 1..80),
    ) {
        let query = String::from_utf8(query).unwrap();
        let request: Uri = format!("/user/alice/a%2Fb?{query}").parse().unwrap();
        let output = build_upstream_uri(
            &route("/user/alice", "http://upstream.example/base"),
            &request,
            &options(include_prefix, prepend_path),
        ).unwrap();
        prop_assert_eq!(output.query(), request.query());
    }
}

#[test]
fn uri_empty_query_delimiter_is_dropped_like_http_proxy_url_join() {
    let request: Uri = "/user/alice/tree?".parse().unwrap();
    let output = build_upstream_uri(
        &route("/user/alice", "http://upstream.example/base"),
        &request,
        &options(true, true),
    )
    .unwrap();
    assert_eq!(output, "/base/user/alice/tree");
    assert_eq!(output.query(), None);
}

#[test]
fn uri_target_query_precedes_request_query_like_http_proxy() {
    let output = build_upstream_uri(
        &route(
            "/user/alice",
            "http://upstream.example/base?target=%2F+raw&shared=target",
        ),
        &Uri::from_static("/user/alice/tree?request=%25FF&shared=request"),
        &options(false, true),
    )
    .unwrap();

    assert_eq!(
        output,
        "/base/tree?target=%2F+raw&shared=target&request=%25FF&shared=request"
    );
}

#[test]
fn uri_empty_target_and_request_queries_are_dropped_like_http_proxy() {
    let output = build_upstream_uri(
        &route("/user", "http://upstream.example/base?"),
        &"/user/tree?".parse::<Uri>().unwrap(),
        &options(false, true),
    )
    .unwrap();

    assert_eq!(output, "/base/tree");
    assert_eq!(output.query(), None);
}

#[test]
fn uri_get_path_normalizes_literal_and_encoded_dot_segments_after_prefix_slicing() {
    let cases = [
        ("/user/a/./b?raw=%2F+%25", "/base/a/b?raw=%2F+%25"),
        ("/user/a/../b?raw=%2F+%25", "/base/b?raw=%2F+%25"),
        ("/user/a/%2e/b?raw=%2F+%25", "/base/a/b?raw=%2F+%25"),
        ("/user/a/%2e%2e/b?raw=%2F+%25", "/base/b?raw=%2F+%25"),
        ("/user/a/%2F/b?raw=%2F+%25", "/base/a/%2F/b?raw=%2F+%25"),
    ];

    for (request, expected) in cases {
        let output = build_upstream_uri(
            &route("/user", "http://upstream.example/base"),
            &request.parse::<Uri>().unwrap(),
            &options(false, true),
        )
        .unwrap();
        assert_eq!(output, expected, "request={request}");
    }
}

#[test]
fn uri_request_headers_apply_custom_origin_and_forwarded_host_policy() {
    let target = Target::parse(&Url::parse("https://upstream.example:8443/base").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.change_origin = true;
    opts.custom_headers
        .insert("x-custom".into(), "configured".into());

    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example:9000"));
    headers.insert("x-custom", HeaderValue::from_static("request"));
    apply_request_headers(&mut headers, &target, &opts).unwrap();

    assert_eq!(headers[HOST], "upstream.example:8443");
    assert_eq!(headers["x-forwarded-host"], "public.example:9000");
    assert_eq!(headers["x-custom"], "configured");
}

#[test]
fn uri_request_headers_remove_connection_named_hop_headers() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, X-Remove"));
    headers.insert("x-remove", HeaderValue::from_static("secret"));
    headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
    headers.insert("proxy-connection", HeaderValue::from_static("keep-alive"));
    headers.insert("te", HeaderValue::from_static("trailers"));
    headers.insert("trailer", HeaderValue::from_static("x-checksum"));
    headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));

    apply_request_headers(&mut headers, &target, &options(true, true)).unwrap();

    for removed in [
        "connection",
        "x-remove",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        assert!(!headers.contains_key(removed), "{removed} survived");
    }
}

#[test]
fn uri_request_headers_keep_websocket_upgrade_pair_only() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        CONNECTION,
        HeaderValue::from_static("keep-alive, Upgrade, X-Remove"),
    );
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert("x-remove", HeaderValue::from_static("secret"));

    apply_request_headers(&mut headers, &target, &options(true, true)).unwrap();

    assert_eq!(headers[CONNECTION], "upgrade");
    assert_eq!(headers[UPGRADE], "websocket");
    assert!(!headers.contains_key("x-remove"));
}

#[test]
fn uri_request_headers_final_scrub_blocks_malicious_custom_hop_headers() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut opts = options(true, true);
    for (name, value) in [
        ("connection", "keep-alive, x-smuggled"),
        ("x-smuggled", "secret"),
        ("keep-alive", "timeout=5"),
        ("proxy-connection", "keep-alive"),
        ("proxy-authorization", "Basic c2VjcmV0"),
        ("proxy-authenticate", "Basic realm=upstream"),
        ("te", "trailers"),
        ("trailer", "x-checksum"),
        ("transfer-encoding", "chunked"),
        ("upgrade", "h2c"),
    ] {
        opts.custom_headers.insert(name.into(), value.into());
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "proxy-authorization",
        HeaderValue::from_static("Basic inbound"),
    );
    headers.insert(
        "proxy-authenticate",
        HeaderValue::from_static("Basic inbound"),
    );
    apply_request_headers(&mut headers, &target, &opts).unwrap();

    for removed in [
        "connection",
        "x-smuggled",
        "keep-alive",
        "proxy-connection",
        "proxy-authorization",
        "proxy-authenticate",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(
            !headers.contains_key(removed),
            "{removed} survived final scrub"
        );
    }
}

#[test]
fn uri_request_headers_preserve_only_the_validated_inbound_websocket_pair() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.custom_headers
        .insert("connection".into(), "x-smuggled".into());
    opts.custom_headers.insert("upgrade".into(), "h2c".into());
    opts.custom_headers
        .insert("x-smuggled".into(), "secret".into());

    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    apply_request_headers(&mut headers, &target, &opts).unwrap();

    assert_eq!(headers[CONNECTION], "upgrade");
    assert_eq!(headers[UPGRADE], "websocket");
    assert!(!headers.contains_key("x-smuggled"));
}

#[test]
fn uri_invalid_custom_header_is_a_typed_error_not_a_panic() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.custom_headers
        .insert("bad header".into(), "value".into());
    let error = apply_request_headers(&mut HeaderMap::new(), &target, &opts).unwrap_err();
    assert!(error.to_string().contains("custom header"));
}

#[test]
fn uri_invalid_custom_header_leaves_request_headers_unchanged() {
    let target = Target::parse(&Url::parse("https://upstream.example").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.change_origin = true;
    opts.custom_headers
        .insert("x-invalid".into(), "value\r\ninjected: yes".into());
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example"));
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    let before = headers.clone();

    assert!(apply_request_headers(&mut headers, &target, &opts).is_err());
    assert_eq!(headers, before);
}

#[test]
fn uri_x_forwarded_values_append_exactly_like_http_proxy() {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example:9000"));
    headers.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.1"));
    headers.insert("x-forwarded-port", HeaderValue::from_static("443"));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    headers.insert("x-forwarded-host", HeaderValue::from_static("edge.example"));
    let context = ForwardedContext {
        client_address: "203.0.113.7",
        port: 9000,
        protocol: "http",
    };

    apply_forwarded_headers(&mut headers, &context, &options(true, true)).unwrap();

    assert_eq!(headers["x-forwarded-for"], "10.0.0.1,203.0.113.7");
    assert_eq!(headers["x-forwarded-port"], "443,9000");
    assert_eq!(headers["x-forwarded-proto"], "https,http");
    assert_eq!(headers["x-forwarded-host"], "edge.example");
}

#[test]
fn uri_x_forwarded_preserves_all_duplicate_values_before_appending() {
    let mut headers = HeaderMap::new();
    for value in ["10.0.0.1", "10.0.0.2"] {
        headers.append("x-forwarded-for", HeaderValue::from_str(value).unwrap());
    }
    for value in ["443", "8443"] {
        headers.append("x-forwarded-port", HeaderValue::from_str(value).unwrap());
    }
    for value in ["https", "wss"] {
        headers.append("x-forwarded-proto", HeaderValue::from_str(value).unwrap());
    }
    for value in ["edge-one.example", "edge-two.example"] {
        headers.append("x-forwarded-host", HeaderValue::from_str(value).unwrap());
    }

    apply_forwarded_headers(
        &mut headers,
        &ForwardedContext {
            client_address: "203.0.113.7",
            port: 9000,
            protocol: "http",
        },
        &options(true, true),
    )
    .unwrap();

    assert_eq!(headers["x-forwarded-for"], "10.0.0.1,10.0.0.2,203.0.113.7");
    assert_eq!(headers["x-forwarded-port"], "443,8443,9000");
    assert_eq!(headers["x-forwarded-proto"], "https,wss,http");
    assert_eq!(
        headers
            .get_all("x-forwarded-host")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>(),
        ["edge-one.example", "edge-two.example"]
    );
}

#[test]
fn uri_x_forwarded_host_is_set_to_an_empty_value_without_any_host() {
    let mut headers = HeaderMap::new();

    apply_forwarded_headers(
        &mut headers,
        &ForwardedContext {
            client_address: "203.0.113.7",
            port: 80,
            protocol: "http",
        },
        &options(true, true),
    )
    .unwrap();

    assert_eq!(headers["x-forwarded-host"], "");
}

#[test]
fn uri_x_forwarded_disabled_is_a_noop() {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example"));
    let before = headers.clone();
    let mut opts = options(true, true);
    opts.x_forward = false;
    apply_forwarded_headers(
        &mut headers,
        &ForwardedContext {
            client_address: "203.0.113.7",
            port: 80,
            protocol: "http",
        },
        &opts,
    )
    .unwrap();
    assert_eq!(headers, before);
}

#[test]
fn uri_redirect_rewrite_matrix_matches_http_proxy() {
    for auto_rewrite in [false, true] {
        for protocol_rewrite in [None, Some("https".to_owned())] {
            let target =
                Target::parse(&Url::parse("http://upstream.example:8080/base").unwrap()).unwrap();
            let mut opts = options(true, true);
            opts.auto_rewrite = auto_rewrite;
            opts.protocol_rewrite = protocol_rewrite.clone();
            let request = Request::builder()
                .uri("/from")
                .header(HOST, "public.example:9443")
                .body(())
                .unwrap();
            let mut response = Response::builder()
                .status(StatusCode::FOUND)
                .header(LOCATION, "http://upstream.example:8080/next?q=%2F")
                .body(())
                .unwrap();

            rewrite_location(&mut response, &request, &target, &opts).unwrap();

            let expected_host = if auto_rewrite {
                "public.example:9443"
            } else {
                "upstream.example:8080"
            };
            let expected_scheme = protocol_rewrite.as_deref().unwrap_or("http");
            assert_eq!(
                response.headers()[LOCATION],
                format!("{expected_scheme}://{expected_host}/next?q=%2F")
            );
        }
    }
}

#[test]
fn uri_redirect_rewrite_requires_matching_target_host_and_redirect_status() {
    let target = Target::parse(&Url::parse("http://upstream.example:8080").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.auto_rewrite = true;
    for (status, location) in [
        (StatusCode::OK, "http://upstream.example:8080/next"),
        (StatusCode::FOUND, "http://other.example/next"),
    ] {
        let request = Request::builder()
            .header(HOST, "public.example")
            .body(())
            .unwrap();
        let mut response = Response::builder()
            .status(status)
            .header(LOCATION, location)
            .body(())
            .unwrap();
        rewrite_location(&mut response, &request, &target, &opts).unwrap();
        assert_eq!(response.headers()[LOCATION], location);
    }
}

#[test]
fn uri_redirect_matches_host_and_port_and_preserves_location_userinfo() {
    let target =
        Target::parse(&Url::parse("http://target-user:target-pass@upstream.example:8080").unwrap())
            .unwrap();
    let mut opts = options(true, true);
    opts.protocol_rewrite = Some("https".to_owned());
    let request = Request::builder().body(()).unwrap();
    let mut response = Response::builder()
        .status(StatusCode::FOUND)
        .header(
            LOCATION,
            "http://redirect-user:redirect-pass@upstream.example:8080/next",
        )
        .body(())
        .unwrap();

    rewrite_location(&mut response, &request, &target, &opts).unwrap();

    assert_eq!(
        response.headers()[LOCATION],
        "https://redirect-user:redirect-pass@upstream.example:8080/next"
    );
}

#[test]
fn uri_protocol_rewrite_does_not_require_host_and_follows_whatwg_setter() {
    let target = Target::parse(&Url::parse("http://upstream.example:8080").unwrap()).unwrap();
    let request = Request::builder().body(()).unwrap();

    let mut opts = options(true, true);
    opts.auto_rewrite = true;
    opts.protocol_rewrite = Some("ftp".to_owned());
    let mut response = Response::builder()
        .status(StatusCode::FOUND)
        .header(LOCATION, "http://upstream.example:8080/next")
        .body(())
        .unwrap();
    rewrite_location(&mut response, &request, &target, &opts).unwrap();
    assert_eq!(
        response.headers()[LOCATION],
        "ftp://upstream.example:8080/next"
    );

    opts.protocol_rewrite = Some("1bad scheme".to_owned());
    let mut response = Response::builder()
        .status(StatusCode::FOUND)
        .header(LOCATION, "http://upstream.example:8080/next")
        .body(())
        .unwrap();
    rewrite_location(&mut response, &request, &target, &opts).unwrap();
    assert_eq!(
        response.headers()[LOCATION],
        "http://upstream.example:8080/next"
    );

    opts.protocol_rewrite = Some("ftp::".to_owned());
    let mut response = Response::builder()
        .status(StatusCode::FOUND)
        .header(LOCATION, "http://upstream.example:8080/next")
        .body(())
        .unwrap();
    rewrite_location(&mut response, &request, &target, &opts).unwrap();
    assert_eq!(
        response.headers()[LOCATION],
        "ftp://upstream.example:8080/next"
    );
}

#[test]
fn uri_redirect_matches_ipv6_with_a_default_port() {
    let target = Target::parse(&Url::parse("http://[::1]:80").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.auto_rewrite = true;
    let request = Request::builder()
        .header(HOST, "[2001:db8::1]:8080")
        .body(())
        .unwrap();
    let mut response = Response::builder()
        .status(StatusCode::FOUND)
        .header(LOCATION, "http://[::1]/next")
        .body(())
        .unwrap();

    rewrite_location(&mut response, &request, &target, &opts).unwrap();

    assert_eq!(
        response.headers()[LOCATION],
        "http://[2001:db8::1]:8080/next"
    );
}

#[test]
fn uri_target_parses_tcp_and_unix_variants() {
    let http = Target::parse(&Url::parse("http://example.test/base").unwrap()).unwrap();
    assert_eq!(http.authority(), "example.test");
    assert_eq!(http.path(), "/base");

    let https = Target::parse(&Url::parse("https://example.test:9443/").unwrap()).unwrap();
    assert_eq!(https.authority(), "example.test:9443");
    assert!(https.is_tls());

    let unix = Target::parse(&Url::parse("http+unix://%2Ftmp%2Fchp.sock/base").unwrap()).unwrap();
    assert_eq!(
        unix.unix_path(),
        Some(std::path::Path::new("/tmp/chp.sock"))
    );
    assert_eq!(unix.path(), "/base");
}

#[test]
fn uri_target_rejects_unsupported_or_invalid_unix_targets() {
    assert!(matches!(
        Target::parse(&Url::parse("ftp://example.test/file").unwrap()),
        Err(TargetError::UnsupportedScheme(_))
    ));
    assert!(Target::parse(&Url::parse("http+unix://relative/base").unwrap()).is_err());
    assert!(Target::parse(&Url::parse("http+unix://%00tmp/base").unwrap()).is_err());
}

#[test]
fn uri_http_peer_defaults_to_certificate_and_hostname_verification() {
    let target = Target::parse(&Url::parse("https://127.0.0.1:9443").unwrap()).unwrap();
    let peer = target.http_peer(&TlsClientConfig::default()).unwrap();
    assert!(peer.options.verify_cert);
    assert!(peer.options.verify_hostname);
}

#[test]
fn uri_http_peer_applies_transport_policy_without_panicking() {
    let target = Target::parse(&Url::parse("https://127.0.0.1:9443/base").unwrap()).unwrap();
    let tls = TlsClientConfig {
        verify_cert: false,
        verify_hostname: false,
        connection_timeout: Some(Duration::from_millis(10)),
        total_connection_timeout: Some(Duration::from_millis(15)),
        read_timeout: Some(Duration::from_millis(20)),
        write_timeout: Some(Duration::from_millis(30)),
        idle_timeout: Some(Duration::from_millis(40)),
        ca_file: None,
        client_certificate: None,
        client_key: None,
    };
    let peer = target.http_peer(&tls).unwrap();
    assert!(peer.is_tls());
    assert!(!peer.options.verify_cert);
    assert!(!peer.options.verify_hostname);
    assert_eq!(peer.options.connection_timeout, tls.connection_timeout);
    assert_eq!(
        peer.options.total_connection_timeout,
        tls.total_connection_timeout
    );
    assert_eq!(peer.options.read_timeout, tls.read_timeout);
    assert_eq!(peer.options.write_timeout, tls.write_timeout);
    assert_eq!(peer.options.idle_timeout, tls.idle_timeout);
}

#[test]
fn uri_unix_http_peer_uses_the_decoded_socket_path() {
    let target = Target::parse(&Url::parse("http+unix://%2Ftmp%2Fchp.sock/base").unwrap()).unwrap();
    let peer = target.http_peer(&TlsClientConfig::default()).unwrap();
    assert!(matches!(peer._address, SocketAddr::Unix(_)));
    assert!(!peer.is_tls());
}

#[test]
fn uri_unix_http_alias_peer_uses_the_decoded_socket_path() {
    let target = Target::parse(&Url::parse("unix+http://%2Ftmp%2Fchp.sock/base").unwrap()).unwrap();
    let peer = target.http_peer(&TlsClientConfig::default()).unwrap();
    assert!(matches!(peer._address, SocketAddr::Unix(_)));
    assert!(!peer.is_tls());
}

#[test]
fn uri_http_and_unix_peers_ignore_tls_identity_files() {
    let tls = TlsClientConfig {
        ca_file: Some(PathBuf::from("missing-ca.pem")),
        client_certificate: Some(PathBuf::from("missing-client.pem")),
        client_key: None,
        ..TlsClientConfig::default()
    };

    for target in [
        Target::parse(&Url::parse("http://127.0.0.1").unwrap()).unwrap(),
        Target::parse(&Url::parse("http+unix://%2Ftmp%2Fchp.sock").unwrap()).unwrap(),
        Target::parse(&Url::parse("unix+http://%2Ftmp%2Fchp.sock").unwrap()).unwrap(),
    ] {
        let peer = target.http_peer(&tls).unwrap();
        assert!(!peer.is_tls());
        assert!(peer.options.ca.is_none());
        assert!(peer.client_cert_key.is_none());
    }
}

#[test]
fn uri_ipv6_peer_uses_the_default_http_port() {
    let target = Target::parse(&Url::parse("http://[::1]:80").unwrap()).unwrap();
    assert_eq!(target.authority(), "[::1]");
    let peer = target.http_peer(&TlsClientConfig::default()).unwrap();
    let address = peer._address.as_inet().unwrap();
    assert!(address.ip().is_ipv6());
    assert_eq!(address.port(), 80);
}

#[test]
fn uri_https_peer_loads_generated_ca_and_client_identity() {
    let directory = tempfile::tempdir().unwrap();
    let ca_path = directory.path().join("ca.pem");
    let certificate_path = directory.path().join("client.pem");
    let key_path = directory.path().join("client-key.pem");

    let key = PKey::generate_ed25519().unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "task-6-client").unwrap();
    let name = name.build();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    certificate.set_subject_name(&name).unwrap();
    certificate.set_issuer_name(&name).unwrap();
    certificate.set_pubkey(&key).unwrap();
    certificate
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    certificate
        .set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    certificate.sign(&key, MessageDigest::null()).unwrap();
    let certificate = certificate.build();

    fs::write(&ca_path, certificate.to_pem().unwrap()).unwrap();
    fs::write(&certificate_path, certificate.to_pem().unwrap()).unwrap();
    fs::write(&key_path, key.private_key_to_pem_pkcs8().unwrap()).unwrap();

    let target = Target::parse(&Url::parse("https://127.0.0.1").unwrap()).unwrap();
    let peer = target
        .http_peer(&TlsClientConfig {
            ca_file: Some(ca_path),
            client_certificate: Some(certificate_path),
            client_key: Some(key_path),
            ..TlsClientConfig::default()
        })
        .unwrap();

    assert_eq!(peer.options.ca.as_ref().unwrap().len(), 1);
    assert!(peer.client_cert_key.is_some());
}

#[test]
fn uri_http_peer_reports_unresolvable_addresses_instead_of_panicking() {
    let target = Target::parse(&Url::parse("http://nonexistent.invalid:8080").unwrap()).unwrap();
    assert!(target.http_peer(&TlsClientConfig::default()).is_err());
}

#[test]
fn uri_http_peer_rejects_partial_client_identity() {
    let target = Target::parse(&Url::parse("https://127.0.0.1").unwrap()).unwrap();
    let tls = TlsClientConfig {
        client_certificate: Some(PathBuf::from("cert.pem")),
        ..TlsClientConfig::default()
    };
    assert!(target.http_peer(&tls).is_err());
}
