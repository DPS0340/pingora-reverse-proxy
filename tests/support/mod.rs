//! Test harness for the CHP-compatible route management API contract tests.
//!
//! Builds a real `api::router` over a fresh in-memory `RouteRegistry` and drives
//! it through `tower::ServiceExt::oneshot`, so every assertion exercises the
//! production handlers rather than a mock.

use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use http::Request;
use pingora_reverse_proxy::api::{router, ApiState};
use pingora_reverse_proxy::metrics::Metrics;
use pingora_reverse_proxy::route_table::RouteRegistry;
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::Store;
use serde_json::Value;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tower::ServiceExt;

pub use http::StatusCode;

static ORACLE_START_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A built router plus the token it was configured with, so `get_json` can
/// authenticate itself without the caller repeating the credential.
#[derive(Clone)]
pub struct TestApi {
    pub router: Router,
    pub token: Option<String>,
    pub registry: Arc<RouteRegistry>,
    pub metrics: Arc<Metrics>,
}

impl TestApi {
    pub fn authorization(&self) -> Option<String> {
        self.token.as_ref().map(|token| format!("token {token}"))
    }
}

/// Build the route management API backed by an empty in-memory store.
pub async fn test_api(token: Option<&str>) -> TestApi {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    test_api_with_store(token, store).await
}

/// Build the route management API over a caller-provided persistence double.
pub async fn test_api_with_store(token: Option<&str>, store: Arc<dyn Store>) -> TestApi {
    let registry = RouteRegistry::load(store)
        .await
        .expect("empty memory store loads");
    let metrics = Arc::new(Metrics::new());
    let state = ApiState::new(Arc::clone(&registry), token, Arc::clone(&metrics));
    TestApi {
        router: router(state),
        token: token.map(str::to_owned),
        registry,
        metrics,
    }
}

/// Send a request whose body, when present, is a JSON value.
pub async fn request(
    app: &TestApi,
    method: &str,
    path: &str,
    body: Option<Value>,
    auth: Option<&str>,
) -> Response {
    let raw = body.map(|value| serde_json::to_vec(&value).expect("value serializes"));
    request_raw(app, method, path, raw, auth).await
}

/// Send a request with an arbitrary (possibly non-JSON) body.
pub async fn request_raw(
    app: &TestApi,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    auth: Option<&str>,
) -> Response {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        builder = builder.header(AUTHORIZATION, auth);
    }
    let body = match body {
        Some(bytes) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    let request = builder.body(body).expect("request builds");
    app.router
        .clone()
        .oneshot(request)
        .await
        .expect("router is infallible")
}

/// Read a response body into raw bytes.
pub async fn read_bytes(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects")
        .to_vec()
}

/// Read a response body into a UTF-8 string.
pub async fn read_text(response: Response) -> String {
    String::from_utf8(read_bytes(response).await).expect("body is utf-8")
}

/// Read a response body into a JSON value.
pub async fn read_json(response: Response) -> Value {
    serde_json::from_slice(&read_bytes(response).await).expect("body is json")
}

/// GET a route or the routing table, authenticating with the configured token.
pub async fn get_json(app: &TestApi, path: &str) -> Value {
    let auth = app.authorization();
    let response = request(app, "GET", path, None, auth.as_deref()).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "GET {path} should be 200"
    );
    read_json(response).await
}

#[allow(dead_code)]
struct CapturedProcess {
    child: Child,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<JoinHandle<()>>,
}

#[allow(dead_code)]
impl CapturedProcess {
    fn spawn(mut command: Command) -> Self {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().expect("spawn differential process");
        let mut pipe = child.stderr.take().expect("differential stderr pipe");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&stderr);
        let stderr_thread = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            *captured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = bytes;
        });
        Self {
            child,
            stderr,
            stderr_thread: Some(stderr_thread),
        }
    }

    fn exited(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("poll differential process")
            .is_some()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(
            &self
                .stderr
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_owned()
    }

    fn join_stderr(&mut self) {
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CapturedProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

#[allow(dead_code)]
struct DifferentialEcho {
    address: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

#[allow(dead_code)]
impl DifferentialEcho {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind differential echo");
        let address = listener.local_addr().expect("differential echo address");
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let router = Router::new().fallback(any(move |request: AxumRequest| async move {
                let (parts, body) = request.into_parts();
                let body = axum::body::to_bytes(body, 2 * 1024 * 1024)
                    .await
                    .expect("read differential echo body");
                if parts.uri.path().ends_with("/redirect") {
                    return (
                        StatusCode::MOVED_PERMANENTLY,
                        [(http::header::LOCATION, format!("http://{address}/next"))],
                        "",
                    )
                        .into_response();
                }
                (
                    [(CONTENT_TYPE, "application/json")],
                    serde_json::json!({
                        "method": parts.method.as_str(),
                        "url": parts.uri.to_string(),
                        "host": parts.headers.get(HOST).and_then(|value| value.to_str().ok()),
                        "x_forwarded_for": parts.headers.get("x-forwarded-for").and_then(|value| value.to_str().ok()),
                        "x_forwarded_port": parts.headers.get("x-forwarded-port").and_then(|value| value.to_str().ok()),
                        "x_forwarded_proto": parts.headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()),
                        "body": String::from_utf8_lossy(&body),
                    })
                    .to_string(),
                )
                    .into_response()
            }));
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve differential echo");
        });
        Self {
            address,
            stop: Some(stop),
            task,
        }
    }
}

impl Drop for DifferentialEcho {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

#[allow(dead_code)]
pub(crate) struct PortLease {
    listener: StdTcpListener,
}

#[allow(dead_code)]
impl PortLease {
    pub(crate) fn new() -> Self {
        Self {
            listener: StdTcpListener::bind(("127.0.0.1", 0)).expect("reserve differential port"),
        }
    }

    pub(crate) fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .expect("differential port address")
            .port()
    }
}

#[derive(Debug, PartialEq)]
struct HttpObservation {
    status: u16,
    headers: std::collections::BTreeMap<String, String>,
    body: Value,
}

/// A live CHP 5.3.0 process and Rust process sharing one deterministic target.
#[allow(dead_code)]
pub struct OraclePair {
    chp: CapturedProcess,
    rust: CapturedProcess,
    chp_public: String,
    chp_public_port: u16,
    chp_api: String,
    chp_metrics: String,
    rust_public: String,
    rust_public_port: u16,
    rust_api: String,
    rust_metrics: String,
    ephemeral_addresses: Vec<String>,
    client: reqwest::Client,
    echo: DifferentialEcho,
    #[cfg(unix)]
    unix: Option<UnixPair>,
}

#[cfg(unix)]
#[allow(dead_code)]
struct UnixPair {
    chp_public: std::path::PathBuf,
    chp_api: std::path::PathBuf,
    chp_metrics: std::path::PathBuf,
    rust_public: std::path::PathBuf,
    rust_api: std::path::PathBuf,
    rust_metrics: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

#[allow(dead_code)]
impl OraclePair {
    pub async fn start() -> Self {
        Self::start_with_args(&[]).await
    }

    pub async fn start_with_args(extra_args: &[String]) -> Self {
        let _startup_guard = ORACLE_START_LOCK.lock().await;
        let echo = DifferentialEcho::start().await;
        let chp_public_lease = PortLease::new();
        let chp_api_lease = PortLease::new();
        let chp_metrics_lease = PortLease::new();
        let rust_public_lease = PortLease::new();
        let rust_api_lease = PortLease::new();
        let rust_metrics_lease = PortLease::new();
        let chp_public_port = chp_public_lease.port();
        let chp_api_port = chp_api_lease.port();
        let chp_metrics_port = chp_metrics_lease.port();
        let rust_public_port = rust_public_lease.port();
        let rust_api_port = rust_api_lease.port();
        let rust_metrics_port = rust_metrics_lease.port();

        let common = |public: u16, api: u16, metrics: u16| {
            vec![
                "--ip".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                public.to_string(),
                "--api-ip".to_owned(),
                "127.0.0.1".to_owned(),
                "--api-port".to_owned(),
                api.to_string(),
                "--metrics-ip".to_owned(),
                "127.0.0.1".to_owned(),
                "--metrics-port".to_owned(),
                metrics.to_string(),
                "--log-level".to_owned(),
                "error".to_owned(),
            ]
        };
        let mut chp_args = common(chp_public_port, chp_api_port, chp_metrics_port);
        chp_args.extend_from_slice(extra_args);
        let mut rust_args = common(rust_public_port, rust_api_port, rust_metrics_port);
        rust_args.extend_from_slice(extra_args);

        let mut node = Command::new("node");
        node.arg(format!(
            "{}/scripts/chp-oracle.mjs",
            env!("CARGO_MANIFEST_DIR")
        ));
        node.args(&chp_args);
        drop((chp_public_lease, chp_api_lease, chp_metrics_lease));
        let mut chp = CapturedProcess::spawn(node);

        let mut rust_command = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"));
        rust_command.args(&rust_args);
        drop((rust_public_lease, rust_api_lease, rust_metrics_lease));
        let mut rust = CapturedProcess::spawn(rust_command);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("differential client");
        let chp_public = format!("http://127.0.0.1:{chp_public_port}");
        let chp_api = format!("http://127.0.0.1:{chp_api_port}");
        let rust_public = format!("http://127.0.0.1:{rust_public_port}");
        let rust_api = format!("http://127.0.0.1:{rust_api_port}");
        let chp_metrics = format!("http://127.0.0.1:{chp_metrics_port}");
        let rust_metrics = format!("http://127.0.0.1:{rust_metrics_port}");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            if chp.exited() {
                chp.join_stderr();
                panic!("pinned CHP exited during readiness: {}", chp.stderr_text());
            }
            if rust.exited() {
                rust.join_stderr();
                panic!("Rust proxy exited during readiness: {}", rust.stderr_text());
            }
            let ready = async {
                for url in [
                    format!("{chp_api}/api/routes"),
                    format!("{rust_api}/api/routes"),
                ] {
                    let Ok(response) = client.get(url).send().await else {
                        return false;
                    };
                    if response.status() != StatusCode::OK {
                        return false;
                    }
                }
                true
            }
            .await;
            if ready {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "differential pair readiness timed out"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        Self {
            chp,
            rust,
            chp_public,
            chp_public_port,
            chp_api,
            chp_metrics,
            rust_public,
            rust_public_port,
            rust_api,
            rust_metrics,
            ephemeral_addresses: vec![
                format!("127.0.0.1:{chp_public_port}"),
                format!("127.0.0.1:{chp_api_port}"),
                format!("127.0.0.1:{chp_metrics_port}"),
                format!("127.0.0.1:{rust_public_port}"),
                format!("127.0.0.1:{rust_api_port}"),
                format!("127.0.0.1:{rust_metrics_port}"),
            ],
            client,
            echo,
            #[cfg(unix)]
            unix: None,
        }
    }

    #[cfg(unix)]
    pub async fn start_unix() -> Self {
        let echo = DifferentialEcho::start().await;
        let directory = tempfile::tempdir().expect("differential Unix directory");
        let chp_public = directory.path().join("chp-public.sock");
        let chp_api = directory.path().join("chp-api.sock");
        let chp_metrics = directory.path().join("chp-metrics.sock");
        let rust_public = directory.path().join("rust-public.sock");
        let rust_api = directory.path().join("rust-api.sock");
        let rust_metrics = directory.path().join("rust-metrics.sock");
        let arguments =
            |public: &std::path::Path, api: &std::path::Path, metrics: &std::path::Path| {
                vec![
                    "--socket".to_owned(),
                    public.display().to_string(),
                    "--api-socket".to_owned(),
                    api.display().to_string(),
                    "--metrics-socket".to_owned(),
                    metrics.display().to_string(),
                    "--log-level".to_owned(),
                    "error".to_owned(),
                ]
            };
        let mut node = Command::new("node");
        node.arg(format!(
            "{}/scripts/chp-oracle.mjs",
            env!("CARGO_MANIFEST_DIR")
        ))
        .args(arguments(&chp_public, &chp_api, &chp_metrics));
        let mut chp = CapturedProcess::spawn(node);
        let mut rust_command = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"));
        rust_command.args(arguments(&rust_public, &rust_api, &rust_metrics));
        let mut rust = CapturedProcess::spawn(rust_command);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            if chp.exited() {
                chp.join_stderr();
                panic!("pinned CHP Unix oracle exited: {}", chp.stderr_text());
            }
            if rust.exited() {
                rust.join_stderr();
                panic!("Rust Unix oracle exited: {}", rust.stderr_text());
            }
            if tokio::net::UnixStream::connect(&chp_api).await.is_ok()
                && tokio::net::UnixStream::connect(&rust_api).await.is_ok()
                && tokio::net::UnixStream::connect(&chp_public).await.is_ok()
                && tokio::net::UnixStream::connect(&rust_public).await.is_ok()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "differential Unix pair readiness timed out"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self {
            chp,
            rust,
            chp_public: String::new(),
            chp_public_port: 0,
            chp_api: String::new(),
            chp_metrics: String::new(),
            rust_public: String::new(),
            rust_public_port: 0,
            rust_api: String::new(),
            rust_metrics: String::new(),
            ephemeral_addresses: vec![
                chp_public.display().to_string(),
                chp_api.display().to_string(),
                chp_metrics.display().to_string(),
                rust_public.display().to_string(),
                rust_api.display().to_string(),
                rust_metrics.display().to_string(),
            ],
            client: reqwest::Client::new(),
            echo,
            unix: Some(UnixPair {
                chp_public,
                chp_api,
                chp_metrics,
                rust_public,
                rust_api,
                rust_metrics,
                _directory: directory,
            }),
        }
    }

    pub fn echo_target(&self) -> String {
        format!("http://{}", self.echo.address)
    }

    pub async fn post_route(&self, route: &str, body: Value) {
        #[cfg(unix)]
        if let Some(unix) = &self.unix {
            let body = serde_json::to_vec(&body).expect("serialize Unix route POST");
            let chp = unix_observation(
                &unix.chp_api,
                "POST",
                &format!("/api/routes{route}"),
                Some(&body),
                &self.ephemeral_addresses,
            )
            .await;
            let rust = unix_observation(
                &unix.rust_api,
                "POST",
                &format!("/api/routes{route}"),
                Some(&body),
                &self.ephemeral_addresses,
            )
            .await;
            assert_eq!(chp, rust, "CHP/Rust Unix route POST differs");
            return;
        }
        let chp = self
            .client
            .post(format!("{}/api/routes{route}", self.chp_api))
            .json(&body)
            .send()
            .await
            .expect("CHP route POST");
        let rust = self
            .client
            .post(format!("{}/api/routes{route}", self.rust_api))
            .json(&body)
            .send()
            .await
            .expect("Rust route POST");
        self.assert_same_response(chp, rust, "route POST").await;
    }

    pub async fn delete_route(&self, route: &str) {
        let chp = self
            .client
            .delete(format!("{}/api/routes{route}", self.chp_api))
            .send()
            .await
            .expect("CHP route DELETE");
        let rust = self
            .client
            .delete(format!("{}/api/routes{route}", self.rust_api))
            .send()
            .await
            .expect("Rust route DELETE");
        self.assert_same_response(chp, rust, "route DELETE").await;
    }

    pub async fn assert_same_http(&self, path: &str) {
        let chp = self
            .client
            .get(format!("{}{path}", self.chp_public))
            .send()
            .await
            .expect("CHP public request");
        let rust = self
            .client
            .get(format!("{}{path}", self.rust_public))
            .send()
            .await
            .expect("Rust public request");
        self.assert_same_response(chp, rust, path).await;
    }

    pub async fn assert_same_http_with_host(&self, path: &str, host: &str) {
        let chp = self
            .client
            .get(format!("{}{path}", self.chp_public))
            .header(HOST, host)
            .send()
            .await
            .expect("CHP public host request");
        let rust = self
            .client
            .get(format!("{}{path}", self.rust_public))
            .header(HOST, host)
            .send()
            .await
            .expect("Rust public host request");
        self.assert_same_response(chp, rust, path).await;
    }

    pub async fn assert_same_metrics(&self) {
        #[cfg(unix)]
        if let Some(unix) = &self.unix {
            let chp = unix_observation(
                &unix.chp_metrics,
                "GET",
                "/metrics",
                None,
                &self.ephemeral_addresses,
            )
            .await;
            let rust = unix_observation(
                &unix.rust_metrics,
                "GET",
                "/metrics",
                None,
                &self.ephemeral_addresses,
            )
            .await;
            let (Value::String(chp), Value::String(rust)) = (chp.body, rust.body) else {
                panic!("Unix metrics bodies must be text")
            };
            assert_eq!(
                metric_contract(&chp),
                metric_contract(&rust),
                "CHP/Rust Unix metric schema or status labels differ"
            );
            return;
        }
        let chp = self
            .client
            .get(format!("{}/metrics", self.chp_metrics))
            .send()
            .await
            .expect("CHP metrics request")
            .text()
            .await
            .expect("CHP metrics text");
        let rust = self
            .client
            .get(format!("{}/metrics", self.rust_metrics))
            .send()
            .await
            .expect("Rust metrics request")
            .text()
            .await
            .expect("Rust metrics text");
        let chp = metric_contract(&chp);
        let rust = metric_contract(&rust);
        assert_eq!(chp, rust, "CHP/Rust metric schema or status labels differ");
    }

    pub async fn assert_same_websocket(&self, path: &str, payload: &[u8]) {
        let chp = websocket_observation(
            &format!(
                "ws://{}{}",
                self.chp_public.trim_start_matches("http://"),
                path
            ),
            payload,
        )
        .await;
        let rust = websocket_observation(
            &format!(
                "ws://{}{}",
                self.rust_public.trim_start_matches("http://"),
                path
            ),
            payload,
        )
        .await;
        assert_eq!(chp, rust, "CHP/Rust websocket observations differ");
    }

    #[cfg(unix)]
    pub async fn assert_same_unix_http(&self, path: &str) {
        let unix = self.unix.as_ref().expect("Unix OraclePair");
        let chp = unix_observation(
            &unix.chp_public,
            "GET",
            path,
            None,
            &self.ephemeral_addresses,
        )
        .await;
        let rust = unix_observation(
            &unix.rust_public,
            "GET",
            path,
            None,
            &self.ephemeral_addresses,
        )
        .await;
        assert_eq!(chp, rust, "CHP/Rust Unix public response differs");
    }

    pub async fn assert_same_tls_health(&self, server_certificate: &std::path::Path) {
        let chp_client = tls_client(server_certificate, self.chp_public_port, None);
        let rust_client = tls_client(server_certificate, self.rust_public_port, None);
        let chp = chp_client
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.chp_public_port
            ))
            .send()
            .await
            .expect("CHP TLS health");
        let rust = rust_client
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.rust_public_port
            ))
            .send()
            .await
            .expect("Rust TLS health");
        self.assert_same_response(chp, rust, "TLS health").await;
    }

    pub async fn assert_same_mutual_tls_health(
        &self,
        server_certificate: &std::path::Path,
        client_certificate: &std::path::Path,
        client_key: &std::path::Path,
    ) {
        let mut identity = std::fs::read(client_certificate).expect("read client certificate");
        identity.extend_from_slice(&std::fs::read(client_key).expect("read client key"));
        let chp_client = tls_client(
            server_certificate,
            self.chp_public_port,
            Some(identity.as_slice()),
        );
        let rust_client = tls_client(
            server_certificate,
            self.rust_public_port,
            Some(identity.as_slice()),
        );
        let chp = chp_client
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.chp_public_port
            ))
            .send()
            .await
            .expect("CHP mutual TLS health");
        let rust = rust_client
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.rust_public_port
            ))
            .send()
            .await
            .expect("Rust mutual TLS health");
        self.assert_same_response(chp, rust, "mutual TLS health")
            .await;

        let chp_rejected = tls_client(server_certificate, self.chp_public_port, None)
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.chp_public_port
            ))
            .send()
            .await
            .is_err();
        let rust_rejected = tls_client(server_certificate, self.rust_public_port, None)
            .get(format!(
                "https://openrusty.org:{}/_chp_healthz",
                self.rust_public_port
            ))
            .send()
            .await
            .is_err();
        assert!(chp_rejected, "CHP accepted a client without mTLS identity");
        assert_eq!(
            chp_rejected, rust_rejected,
            "mTLS rejection outcome differs"
        );
    }

    pub async fn assert_same_route_tables(&self) {
        #[cfg(unix)]
        if let Some(unix) = &self.unix {
            let chp = unix_observation(
                &unix.chp_api,
                "GET",
                "/api/routes",
                None,
                &self.ephemeral_addresses,
            )
            .await;
            let rust = unix_observation(
                &unix.rust_api,
                "GET",
                "/api/routes",
                None,
                &self.ephemeral_addresses,
            )
            .await;
            assert_eq!(chp, rust, "CHP/Rust Unix route tables differ");
            return;
        }
        let chp = self
            .client
            .get(format!("{}/api/routes", self.chp_api))
            .send()
            .await
            .expect("CHP route table");
        let rust = self
            .client
            .get(format!("{}/api/routes", self.rust_api))
            .send()
            .await
            .expect("Rust route table");
        self.assert_same_response(chp, rust, "route tables").await;
    }

    async fn assert_same_response(
        &self,
        chp: reqwest::Response,
        rust: reqwest::Response,
        scenario: &str,
    ) {
        let chp = observe(chp, &self.ephemeral_addresses).await;
        let rust = observe(rust, &self.ephemeral_addresses).await;
        assert_eq!(
            chp, rust,
            "CHP/Rust mismatch for {scenario}; ephemeral={:?}",
            self.ephemeral_addresses
        );
    }
}

async fn observe(response: reqwest::Response, ephemeral: &[String]) -> HttpObservation {
    let status = response.status().as_u16();
    let mut headers = std::collections::BTreeMap::new();
    for (name, value) in response.headers() {
        let name = name.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "date" | "connection" | "keep-alive" | "transfer-encoding" | "content-length"
        ) {
            continue;
        }
        headers.insert(
            name,
            normalize_ephemeral(value.to_str().unwrap_or_default(), ephemeral),
        );
    }
    let bytes = response.bytes().await.expect("read differential response");
    let mut body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    normalize_value(&mut body, None, ephemeral);
    HttpObservation {
        status,
        headers,
        body,
    }
}

fn normalize_value(value: &mut Value, key: Option<&str>, ephemeral: &[String]) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                normalize_value(value, Some(key), ephemeral);
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_value(value, key, ephemeral);
            }
        }
        Value::String(text) if key == Some("last_activity") => {
            chrono::DateTime::parse_from_rfc3339(text).expect("oracle emitted RFC3339 Date value");
            *text = "<date>".to_owned();
        }
        Value::String(text)
            if key == Some("x_forwarded_port")
                && ephemeral
                    .iter()
                    .filter_map(|address| address.rsplit_once(':'))
                    .any(|(_, port)| port == text) =>
        {
            *text = "<ephemeral-port>".to_owned();
        }
        Value::String(text) => *text = normalize_ephemeral(text, ephemeral),
        _ => {}
    }
}

fn normalize_ephemeral(input: &str, ephemeral: &[String]) -> String {
    ephemeral
        .iter()
        .fold(input.to_owned(), |normalized, value| {
            let normalized = normalized.replace(value, "<ephemeral-address>");
            value
                .rsplit_once(':')
                .map_or(normalized.clone(), |(_, port)| {
                    normalized.replace(&format!(":{port}"), ":<ephemeral-port>")
                })
        })
}

#[cfg(unix)]
async fn unix_observation(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    ephemeral: &[String],
) -> HttpObservation {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::UnixStream::connect(socket),
    )
    .await
    .expect("Unix differential connect timed out")
    .expect("Unix differential connect failed");
    let body = body.unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write Unix differential request headers");
    stream
        .write_all(body)
        .await
        .expect("write Unix differential request body");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .expect("Unix differential response timed out")
        .expect("read Unix differential response");

    let mut raw_headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut raw_headers);
    let offset = match parsed.parse(&response).expect("parse Unix HTTP response") {
        httparse::Status::Complete(offset) => offset,
        httparse::Status::Partial => panic!("partial Unix HTTP response"),
    };
    let status = parsed.code.expect("Unix HTTP status");
    let chunked = parsed.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(header.value).eq_ignore_ascii_case("chunked")
    });
    let mut headers = std::collections::BTreeMap::new();
    for header in parsed.headers {
        let name = header.name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "date" | "connection" | "keep-alive" | "transfer-encoding" | "content-length"
        ) {
            continue;
        }
        headers.insert(
            name,
            normalize_ephemeral(&String::from_utf8_lossy(header.value), ephemeral),
        );
    }
    let bytes = if chunked {
        decode_chunked(&response[offset..])
    } else {
        response[offset..].to_vec()
    };
    let mut body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    normalize_value(&mut body, None, ephemeral);
    HttpObservation {
        status,
        headers,
        body,
    }
}

#[cfg(unix)]
fn decode_chunked(mut input: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|bytes| bytes == b"\r\n")
            .expect("chunk size terminator");
        let size = usize::from_str_radix(
            std::str::from_utf8(&input[..line_end])
                .expect("chunk size UTF-8")
                .split(';')
                .next()
                .expect("chunk size"),
            16,
        )
        .expect("hex chunk size");
        input = &input[line_end + 2..];
        if size == 0 {
            return decoded;
        }
        assert!(input.len() >= size + 2, "complete chunk body");
        decoded.extend_from_slice(&input[..size]);
        assert_eq!(&input[size..size + 2], b"\r\n");
        input = &input[size + 2..];
    }
}

fn tls_client(root: &std::path::Path, port: u16, identity: Option<&[u8]>) -> reqwest::Client {
    let root = reqwest::Certificate::from_pem(&std::fs::read(root).expect("read TLS root"))
        .expect("parse TLS root");
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(root)
        .danger_accept_invalid_certs(true)
        .resolve(
            "openrusty.org",
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        );
    if let Some(identity) = identity {
        builder =
            builder.identity(reqwest::Identity::from_pem(identity).expect("parse mTLS identity"));
    }
    builder.build().expect("build TLS differential client")
}

fn metric_contract(
    exposition: &str,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) {
    const EXPECTED: &[(&str, &str)] = &[
        ("api_route_get", "counter"),
        ("api_route_add", "counter"),
        ("api_route_delete", "counter"),
        ("find_target_for_req", "summary"),
        ("last_activity_updating", "summary"),
        ("requests_ws", "counter"),
        ("requests_web", "counter"),
        ("requests_proxy", "counter"),
        ("requests_api", "counter"),
    ];
    let mut types = std::collections::BTreeMap::new();
    let mut statuses: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for line in exposition.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            if let Some((name, kind)) = rest.split_once(' ') {
                if EXPECTED.iter().any(|(expected, _)| *expected == name) {
                    types.insert(name.to_owned(), kind.to_owned());
                }
            }
        }
        for name in ["requests_proxy", "requests_api"] {
            let prefix = format!("{name}{{status=\"");
            if let Some(rest) = line.strip_prefix(&prefix) {
                let status = rest
                    .split_once("\"}")
                    .expect("status metric closes its only label")
                    .0;
                assert!(
                    status.bytes().all(|byte| byte.is_ascii_digit()),
                    "metric status is numeric"
                );
                statuses
                    .entry(name.to_owned())
                    .or_default()
                    .insert(status.to_owned());
            }
        }
    }
    assert_eq!(
        types,
        EXPECTED
            .iter()
            .map(|(name, kind)| ((*name).to_owned(), (*kind).to_owned()))
            .collect(),
        "missing or mistyped CHP metric family"
    );
    (types, statuses)
}

async fn websocket_observation(url: &str, payload: &[u8]) -> (u16, String, Vec<u8>, bool) {
    let (mut socket, response) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .expect("websocket connect timed out")
    .expect("websocket connect failed");
    let greeting = socket
        .next()
        .await
        .expect("websocket greeting missing")
        .expect("websocket greeting failed")
        .into_text()
        .expect("websocket greeting is text")
        .to_string();
    socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            payload.to_vec().into(),
        ))
        .await
        .expect("send websocket differential payload");
    let echoed = socket
        .next()
        .await
        .expect("websocket echo missing")
        .expect("websocket echo failed")
        .into_data()
        .to_vec();
    socket
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .expect("send websocket close");
    let closed = matches!(
        tokio::time::timeout(Duration::from_secs(3), socket.next()).await,
        Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))) | Ok(None)
    );
    (response.status().as_u16(), greeting, echoed, closed)
}
