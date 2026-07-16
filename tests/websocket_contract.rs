use std::io::Read;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::HeaderValue;
use serde_json::json;
use serial_test::serial;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

const IO_TIMEOUT: Duration = Duration::from_secs(3);

struct Ports {
    public: u16,
    api: u16,
}

impl Ports {
    fn reserve() -> Self {
        let public = StdTcpListener::bind(("127.0.0.1", 0)).expect("reserve public port");
        let api = StdTcpListener::bind(("127.0.0.1", 0)).expect("reserve API port");
        let ports = Self {
            public: public.local_addr().expect("public local address").port(),
            api: api.local_addr().expect("API local address").port(),
        };
        drop((public, api));
        ports
    }
}

struct Binary {
    child: Child,
    ports: Ports,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl Binary {
    async fn start() -> Self {
        for attempt in 1..=5 {
            let ports = Ports::reserve();
            let arguments = vec![
                "--ip".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                ports.public.to_string(),
                "--api-ip".to_owned(),
                "127.0.0.1".to_owned(),
                "--api-port".to_owned(),
                ports.api.to_string(),
            ];
            let mut binary = Self::spawn(ports, &arguments);
            if binary.wait_ready().await {
                return binary;
            }
            if attempt == 5 {
                panic!(
                    "proxy repeatedly exited during readiness: {}",
                    binary.stderr_text()
                );
            }
        }
        unreachable!("bounded startup loop returns or panics")
    }

    fn spawn(ports: Ports, arguments: &[String]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"));
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn proxy binary");
        let mut stdout_pipe = child.stdout.take().expect("proxy stdout pipe");
        let mut stderr_pipe = child.stderr.take().expect("proxy stderr pipe");
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&stdout);
        let stdout_thread = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut bytes);
            *captured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = bytes;
        });
        let captured = Arc::clone(&stderr);
        let stderr_thread = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut bytes);
            *captured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = bytes;
        });
        Self {
            child,
            ports,
            stdout,
            stderr,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        }
    }

    #[cfg(unix)]
    async fn start_public_unix(path: &std::path::Path) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for attempt in 1..=5 {
            let api = StdTcpListener::bind(("127.0.0.1", 0)).expect("reserve Unix WS API port");
            let api_port = api.local_addr().expect("Unix WS API address").port();
            drop(api);
            let ports = Ports {
                public: 0,
                api: api_port,
            };
            let arguments = vec![
                "--socket".to_owned(),
                path.display().to_string(),
                "--api-ip".to_owned(),
                "127.0.0.1".to_owned(),
                "--api-port".to_owned(),
                api_port.to_string(),
            ];
            let mut binary = Self::spawn(ports, &arguments);
            let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
            loop {
                if binary
                    .child
                    .try_wait()
                    .expect("poll Unix WS proxy")
                    .is_some()
                {
                    binary.join_stderr();
                    break;
                }
                let api_ready = reqwest::Client::new()
                    .get(format!("http://127.0.0.1:{api_port}/api/routes"))
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == reqwest::StatusCode::OK);
                if api_ready {
                    if let Ok(mut stream) = tokio::net::UnixStream::connect(path).await {
                        let request = b"GET /_chp_healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
                        if stream.write_all(request).await.is_ok() {
                            let mut response = Vec::new();
                            if stream.read_to_end(&mut response).await.is_ok()
                                && response.ends_with(br#"{"status":"OK"}"#)
                            {
                                return binary;
                            }
                        }
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "Unix WebSocket proxy did not become ready"
                );
                tokio::task::yield_now().await;
            }
            if attempt == 5 {
                panic!(
                    "Unix WebSocket proxy repeatedly exited during readiness: {}",
                    binary.stderr_text()
                );
            }
        }
        unreachable!("bounded Unix startup loop returns or panics")
    }

    async fn wait_ready(&mut self) -> bool {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(150))
            .build()
            .expect("readiness client");
        let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
        loop {
            if self.child.try_wait().expect("poll proxy").is_some() {
                self.join_stderr();
                return false;
            }
            let public = client
                .get(format!(
                    "http://127.0.0.1:{}/_chp_healthz",
                    self.ports.public
                ))
                .send()
                .await;
            let api = client
                .get(format!("http://127.0.0.1:{}/api/routes", self.ports.api))
                .send()
                .await;
            if let (Ok(public), Ok(api)) = (public, api) {
                if public.status() == reqwest::StatusCode::OK
                    && public.text().await.ok().as_deref() == Some(r#"{"status":"OK"}"#)
                    && api.status() == reqwest::StatusCode::OK
                    && api.text().await.ok().as_deref() == Some("{}")
                {
                    return true;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "proxy did not reach exact public/API readiness"
            );
            tokio::task::yield_now().await;
        }
    }

    fn join_stderr(&mut self) {
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
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

    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(
            &self
                .stdout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_owned()
    }

    #[cfg(unix)]
    fn terminate(&mut self) {
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
    }

    fn assert_running(&mut self, message: &str) {
        assert!(
            self.child.try_wait().expect("poll proxy process").is_none(),
            "{message}"
        );
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll proxy exit") {
                self.join_stderr();
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "proxy exit timed out");
            std::thread::yield_now();
        }
    }

    async fn add_route(&self, route: &str, target: &str) {
        let response = tokio::time::timeout(
            IO_TIMEOUT,
            reqwest::Client::new()
                .post(format!(
                    "http://127.0.0.1:{}/api/routes{}",
                    self.ports.api, route
                ))
                .json(&json!({ "target": target }))
                .send(),
        )
        .await
        .expect("route create timed out")
        .expect("route create request failed");
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        assert_eq!(response.bytes().await.expect("route body"), "");
    }

    fn ws_url(&self, path: &str) -> String {
        format!("ws://127.0.0.1:{}{path}", self.ports.public)
    }

    async fn route_last_activity(&self, route: &str) -> String {
        let response = tokio::time::timeout(
            IO_TIMEOUT,
            reqwest::Client::new()
                .get(format!(
                    "http://127.0.0.1:{}/api/routes{}",
                    self.ports.api, route
                ))
                .send(),
        )
        .await
        .expect("route activity request timed out")
        .expect("route activity request failed");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response
            .json::<serde_json::Value>()
            .await
            .expect("route activity JSON")["last_activity"]
            .as_str()
            .expect("route last_activity string")
            .to_owned()
    }
}

impl Drop for Binary {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.join_stderr();
    }
}

#[allow(clippy::result_large_err)]
async fn websocket_contract_upstream() -> (
    SocketAddr,
    tokio::sync::oneshot::Receiver<(String, String)>,
    tokio::sync::oneshot::Receiver<CloseFrame>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind WebSocket contract upstream");
    let address = listener
        .local_addr()
        .expect("WebSocket contract upstream address");
    let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel();
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(IO_TIMEOUT, listener.accept())
            .await
            .expect("WebSocket contract accept timed out")
            .expect("WebSocket contract accept failed");
        let mut handshake_tx = Some(handshake_tx);
        let mut websocket = tokio::time::timeout(
            IO_TIMEOUT,
            tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &Request, response: Response| {
                    if let Some(sender) = handshake_tx.take() {
                        let header = request
                            .headers()
                            .get("x-ws-contract")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let _ = sender.send((request.uri().to_string(), header));
                    }
                    Ok(response)
                },
            ),
        )
        .await
        .expect("WebSocket contract handshake timed out")
        .expect("WebSocket contract handshake failed");
        while let Some(message) = tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("WebSocket contract receive timed out")
        {
            let message = message.expect("WebSocket contract message failed");
            match message {
                Message::Binary(bytes) => websocket
                    .send(Message::Binary(bytes))
                    .await
                    .expect("WebSocket binary echo failed"),
                Message::Close(frame) => {
                    if let Some(frame) = &frame {
                        let _ = close_tx.send(frame.clone());
                    }
                    websocket
                        .flush()
                        .await
                        .expect("flush WebSocket close reply");
                    break;
                }
                _ => {}
            }
        }
    });
    (address, handshake_rx, close_rx, task)
}

#[tokio::test]
#[serial]
async fn websocket_messages_cross_the_selected_user_route() {
    let (upstream, handshake, close, upstream_task) = websocket_contract_upstream().await;
    let mut proxy = Binary::start().await;
    proxy
        .add_route("/user/alice", &format!("http://{upstream}"))
        .await;
    let activity_before = proxy.route_last_activity("/user/alice").await;

    let mut request = proxy
        .ws_url("/user/alice/api/kernels/1/channels?session=abc%2Fdef")
        .into_client_request()
        .expect("WebSocket client request");
    request
        .headers_mut()
        .insert("x-ws-contract", HeaderValue::from_static("preserved-value"));
    let (mut websocket, response) =
        tokio::time::timeout(IO_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .expect("public WebSocket handshake timed out")
            .expect("public WebSocket handshake failed");
    assert_eq!(response.status(), 101);
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, handshake)
            .await
            .expect("upstream handshake observation timed out")
            .expect("upstream handshake observation dropped"),
        (
            "/user/alice/api/kernels/1/channels?session=abc%2Fdef".to_owned(),
            "preserved-value".to_owned(),
        )
    );
    websocket
        .send(Message::Binary(vec![0, 1, 2, 0xff].into()))
        .await
        .expect("send WebSocket binary message");
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("public WebSocket echo timed out")
            .expect("public WebSocket closed before echo")
            .expect("public WebSocket echo failed"),
        Message::Binary(vec![0, 1, 2, 0xff].into())
    );
    let activity_deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        if proxy.route_last_activity("/user/alice").await != activity_before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < activity_deadline,
            "WebSocket data did not update activity while the socket remained open"
        );
        tokio::task::yield_now().await;
    }
    let close_frame = CloseFrame {
        code: CloseCode::Away,
        reason: "task-8-close".into(),
    };
    websocket
        .send(Message::Close(Some(close_frame.clone())))
        .await
        .expect("close public WebSocket");
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, close)
            .await
            .expect("upstream close observation timed out")
            .expect("upstream close observation dropped"),
        close_frame
    );
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("public WebSocket close reply timed out")
            .expect("public WebSocket ended before close reply")
            .expect("public WebSocket close reply failed"),
        Message::Close(Some(close_frame))
    );
    tokio::time::timeout(IO_TIMEOUT, upstream_task)
        .await
        .expect("WebSocket upstream task timed out")
        .expect("WebSocket upstream task panicked");
    let _ = &mut proxy;
}

#[tokio::test]
#[serial]
async fn unavailable_websocket_upstream_returns_empty_503_handshake() {
    let unavailable = StdTcpListener::bind(("127.0.0.1", 0)).expect("reserve unavailable port");
    let target = unavailable.local_addr().expect("unavailable address");
    drop(unavailable);
    let proxy = Binary::start().await;
    proxy
        .add_route("/user/alice", &format!("http://{target}"))
        .await;

    let error = tokio::time::timeout(
        IO_TIMEOUT,
        tokio_tungstenite::connect_async(proxy.ws_url("/user/alice/api/kernels/1/channels")),
    )
    .await
    .expect("unavailable WebSocket handshake timed out")
    .expect_err("unavailable WebSocket unexpectedly upgraded");
    let WsError::Http(response) = error else {
        panic!("expected HTTP WebSocket handshake error, got {error}");
    };
    assert_eq!(response.status(), 503);
    assert_eq!(response.body().as_ref().map(Vec::as_slice), Some(&[][..]));
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn public_unix_socket_carries_websocket_binary_headers_query_and_close() {
    let directory = tempfile::tempdir().expect("temporary public WebSocket Unix directory");
    let socket = directory.path().join("public.sock");
    let (upstream, handshake, close, upstream_task) = websocket_contract_upstream().await;
    let mut proxy = Binary::start_public_unix(&socket).await;
    proxy
        .add_route("/user/alice", &format!("http://{upstream}"))
        .await;

    let stream = tokio::time::timeout(IO_TIMEOUT, tokio::net::UnixStream::connect(&socket))
        .await
        .expect("public Unix WebSocket connect timed out")
        .expect("public Unix WebSocket connect failed");
    let mut request = "ws://localhost/user/alice/channels?token=a%2Fb"
        .into_client_request()
        .expect("public Unix WebSocket request");
    request
        .headers_mut()
        .insert("x-ws-contract", HeaderValue::from_static("unix-preserved"));
    let (mut websocket, response) =
        tokio::time::timeout(IO_TIMEOUT, tokio_tungstenite::client_async(request, stream))
            .await
            .expect("public Unix WebSocket handshake timed out")
            .expect("public Unix WebSocket handshake failed");
    assert_eq!(response.status(), 101);
    assert_eq!(
        handshake.await.expect("public Unix upstream handshake"),
        (
            "/user/alice/channels?token=a%2Fb".to_owned(),
            "unix-preserved".to_owned(),
        )
    );
    websocket
        .send(Message::Binary(vec![0xde, 0xad, 0xbe, 0xef].into()))
        .await
        .expect("send public Unix WebSocket binary");
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("public Unix WebSocket echo timed out")
            .expect("public Unix WebSocket closed before echo")
            .expect("public Unix WebSocket echo failed"),
        Message::Binary(vec![0xde, 0xad, 0xbe, 0xef].into())
    );
    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "unix-close".into(),
    };
    websocket
        .send(Message::Close(Some(close_frame.clone())))
        .await
        .expect("close public Unix WebSocket");
    assert_eq!(
        close.await.expect("public Unix upstream close"),
        close_frame.clone()
    );
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("public Unix WebSocket close reply timed out")
            .expect("public Unix WebSocket ended before close reply")
            .expect("public Unix WebSocket close reply failed"),
        Message::Close(Some(close_frame))
    );
    upstream_task
        .await
        .expect("public Unix WebSocket upstream task panicked");
    proxy.terminate();
    assert!(proxy.wait_for_exit().success());
    assert!(!socket.exists(), "public Unix WebSocket socket remained");
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn sigterm_drains_an_active_websocket_before_ordered_exit() {
    let (upstream, _handshake, close, upstream_task) = websocket_contract_upstream().await;
    let mut proxy = Binary::start().await;
    proxy
        .add_route("/user/alice", &format!("http://{upstream}"))
        .await;
    let (mut websocket, _) = tokio::time::timeout(
        IO_TIMEOUT,
        tokio_tungstenite::connect_async(proxy.ws_url("/user/alice/channels")),
    )
    .await
    .expect("active WebSocket handshake timed out")
    .expect("active WebSocket handshake failed");

    proxy.terminate();
    let hold = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < hold {
        proxy.assert_running("proxy exited while an admitted WebSocket remained active");
        tokio::task::yield_now().await;
    }
    websocket
        .send(Message::Binary(vec![7, 8, 9].into()))
        .await
        .expect("send active WebSocket data after SIGTERM");
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("active WebSocket post-SIGTERM echo timed out")
            .expect("active WebSocket closed during drain")
            .expect("active WebSocket post-SIGTERM echo failed"),
        Message::Binary(vec![7, 8, 9].into())
    );
    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "drained".into(),
    };
    websocket
        .send(Message::Close(Some(close_frame.clone())))
        .await
        .expect("close active draining WebSocket");
    assert_eq!(
        close.await.expect("draining upstream close"),
        close_frame.clone()
    );
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("draining WebSocket close reply timed out")
            .expect("draining WebSocket ended before close reply")
            .expect("draining WebSocket close reply failed"),
        Message::Close(Some(close_frame))
    );
    upstream_task
        .await
        .expect("draining WebSocket upstream task panicked");
    assert!(proxy.wait_for_exit().success());
    assert!(
        proxy
            .stdout_text()
            .contains("ordered shutdown lifecycle completed"),
        "ordered lifecycle completion was not reported: {}",
        proxy.stdout_text()
    );
}
