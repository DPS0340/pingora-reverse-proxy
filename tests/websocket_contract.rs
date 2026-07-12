use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use serial_test::serial;
use tokio::net::TcpListener;
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
}

impl Binary {
    async fn start() -> Self {
        let ports = Ports::reserve();
        let child = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"))
            .args([
                "--ip",
                "127.0.0.1",
                "--port",
                &ports.public.to_string(),
                "--api-ip",
                "127.0.0.1",
                "--api-port",
                &ports.api.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn proxy binary");
        let mut binary = Self { child, ports };
        binary.wait_ready().await;
        binary
    }

    async fn wait_ready(&mut self) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(150))
            .build()
            .expect("readiness client");
        let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
        loop {
            assert!(
                self.child.try_wait().expect("poll proxy").is_none(),
                "proxy exited before exact public/API readiness"
            );
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
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "proxy did not reach exact public/API readiness"
            );
            tokio::task::yield_now().await;
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
}

impl Drop for Binary {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

async fn websocket_echo_upstream() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind WebSocket upstream");
    let address = listener.local_addr().expect("WebSocket upstream address");
    let task = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(IO_TIMEOUT, listener.accept())
            .await
            .expect("WebSocket upstream accept timed out")
            .expect("WebSocket upstream accept failed");
        let mut websocket =
            tokio::time::timeout(IO_TIMEOUT, tokio_tungstenite::accept_async(stream))
                .await
                .expect("WebSocket upstream handshake timed out")
                .expect("WebSocket upstream handshake failed");
        while let Some(message) = tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("WebSocket upstream receive timed out")
        {
            let message = message.expect("WebSocket upstream message failed");
            if message.is_close() {
                break;
            }
            websocket
                .send(message)
                .await
                .expect("WebSocket echo failed");
        }
    });
    (address, task)
}

#[tokio::test]
#[serial]
async fn websocket_messages_cross_the_selected_user_route() {
    let (upstream, upstream_task) = websocket_echo_upstream().await;
    let mut proxy = Binary::start().await;
    proxy
        .add_route("/user/alice", &format!("http://{upstream}"))
        .await;

    let (mut websocket, response) = tokio::time::timeout(
        IO_TIMEOUT,
        tokio_tungstenite::connect_async(proxy.ws_url("/user/alice/api/kernels/1/channels")),
    )
    .await
    .expect("public WebSocket handshake timed out")
    .expect("public WebSocket handshake failed");
    assert_eq!(response.status(), 101);
    websocket
        .send(Message::Text("ping".into()))
        .await
        .expect("send WebSocket message");
    assert_eq!(
        tokio::time::timeout(IO_TIMEOUT, websocket.next())
            .await
            .expect("public WebSocket echo timed out")
            .expect("public WebSocket closed before echo")
            .expect("public WebSocket echo failed"),
        Message::Text("ping".into())
    );
    websocket.close(None).await.expect("close public WebSocket");
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
