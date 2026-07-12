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
use pingora_reverse_proxy::proxy::ChpProxy;
use pingora_reverse_proxy::route::RouteData;
use pingora_reverse_proxy::route::RouteKey;
use pingora_reverse_proxy::route_table::RouteRegistry;
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::{Store, StoreError};
use pingora_reverse_proxy::upstream::{
    apply_forwarded_headers, apply_request_headers, build_upstream_uri, rewrite_location,
    ForwardedContext, Target, TargetError, TlsClientConfig, UpstreamRoute,
};

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
                    if parts.uri.path().ends_with("/redirect") {
                        return (
                            StatusCode::MOVED_PERMANENTLY,
                            [(LOCATION, format!("http://{address}/next"))],
                            "",
                        )
                            .into_response();
                    }
                    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                    let response = serde_json::json!({
                        "method": parts.method.as_str(),
                        "uri": parts.uri.to_string(),
                        "host": parts.headers.get(HOST).and_then(|value| value.to_str().ok()),
                        "x_custom": parts.headers.get("x-custom").and_then(|value| value.to_str().ok()),
                        "x_forwarded_for": parts.headers.get("x-forwarded-for").and_then(|value| value.to_str().ok()),
                        "x_forwarded_port": parts.headers.get("x-forwarded-port").and_then(|value| value.to_str().ok()),
                        "x_forwarded_proto": parts.headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()),
                        "x_forwarded_host": parts.headers.get("x-forwarded-host").and_then(|value| value.to_str().ok()),
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

struct ProxyProcess {
    child: Child,
    base_url: String,
}

impl ProxyProcess {
    async fn start(extra_args: &[String]) -> Self {
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
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client
                .get(format!("{base_url}/_chp_healthz"))
                .send()
                .await
                .is_ok()
            {
                return Self { child, base_url };
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("proxy binary did not begin listening on its reserved port");
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
    registry: Arc<RouteRegistry>,
    activity: ActivityWriter,
    base_url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProxyHarness {
    async fn start(arguments: &[String], routes: &[(&str, String)]) -> Self {
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
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let registry = RouteRegistry::load(store).await.unwrap();
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
        let proxy =
            ChpProxy::from_config(Arc::clone(&registry), &config, activity.clone()).unwrap();
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
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client
                .get(format!("{base_url}/_chp_healthz"))
                .send()
                .await
                .is_ok()
            {
                return Self {
                    registry,
                    activity,
                    base_url,
                    stop,
                    thread: Some(thread),
                };
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.store(true, Ordering::Release);
        let _ = thread.join();
        panic!("in-process Pingora server did not begin listening");
    }

    async fn request(&self, request: reqwest::RequestBuilder) -> reqwest::Response {
        request.send().await.unwrap()
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
                self.activity.flush().await;
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
            let _ = thread.join();
        }
    }
}

struct ChunkedUpstream {
    address: StdSocketAddr,
    task: tokio::task::JoinHandle<()>,
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
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for chunk in chunks {
                stream
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .unwrap();
                stream.write_all(chunk).await.unwrap();
                stream.write_all(b"\r\n").await.unwrap();
                stream.flush().await.unwrap();
                tokio::time::sleep(chunk_delay).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
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
async fn network_binary_without_route_returns_404() {
    let proxy = ProxyProcess::start(&[]).await;
    let response = proxy.get("/missing").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "Not Found");
}

#[tokio::test]
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
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let host = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    stream
        .write_all(
            format!(
                "POST /external/tree?q=%2F HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    for chunk in [b"streamed-".as_slice(), b"request".as_slice()] {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(chunk).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stream.write_all(b"0\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
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
            directory.path().to_string_lossy().into_owned(),
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
            directory.path().to_string_lossy().into_owned(),
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
async fn network_typed_internal_errors_are_500_and_custom_failure_uses_reason_phrase() {
    let invalid = ProxyHarness::start(&[], &[("/invalid", "not a URL".to_owned())]).await;
    let before = invalid.route("/invalid").last_activity;
    let response = invalid
        .request(reqwest::Client::new().get(invalid.url("/invalid")))
        .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.text().await.unwrap(), "Internal Server Error");
    assert_eq!(invalid.route("/invalid").last_activity, before);

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
async fn network_unix_custom_error_target_uses_get_and_copies_only_content_headers() {
    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("errors.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let (request_tx, request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        let _ = request_tx.send(String::from_utf8_lossy(&request[..count]).into_owned());
        stream
            .write_all(
                b"HTTP/1.1 418 Teapot\r\nContent-Type: text/plain\r\nContent-Encoding: identity\r\nX-Disallowed: secret\r\nContent-Length: 10\r\nConnection: close\r\n\r\nunix error",
            )
            .await
            .unwrap();
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
    let request = request_rx.await.unwrap();
    assert!(request.starts_with("GET /errors/404?url=%2Fmissing%3Fq%3D%252F HTTP/1.1\r\n"));
    server.await.unwrap();
}

struct FailingActivityStore {
    routes: tokio::sync::RwLock<BTreeMap<RouteKey, RouteData>>,
    attempts: AtomicUsize,
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
    ) -> Result<RouteData, StoreError> {
        let data = RouteData {
            target,
            last_activity: chrono::Utc::now(),
            extra,
        };
        self.routes.write().await.insert(key, data.clone());
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.routes.write().await.insert(key, data);
        Ok(())
    }

    async fn update_activity(
        &self,
        _key: &RouteKey,
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StoreError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
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
    });
    let registry = RouteRegistry::load(store.clone()).await.unwrap();
    let writer = ActivityWriter::start(Arc::clone(&registry), 1);
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
    writer.flush().await;
    assert_eq!(store.attempts.load(Ordering::Relaxed), 1);
    assert_eq!(writer.persistence_errors(), 1);
    assert_eq!(registry.get(&key).unwrap().last_activity, newest);
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
