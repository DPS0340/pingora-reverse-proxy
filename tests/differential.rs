#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use axum::body::Body;
use futures_util::{SinkExt, StreamExt};
use http::Request;
use pingora_reverse_proxy::api_server::metrics_router;
use serde_json::json;
use tower::ServiceExt;

use support::{read_text, request, test_api, OraclePair, PortLease, StatusCode};

/// The complete normalization allowlist for differential observations:
///
/// - HTTP `Date` values;
/// - server-generated connection/framing headers (`Connection`, `Keep-Alive`,
///   `Transfer-Encoding`, and `Content-Length`);
/// - listener/client addresses and ports allocated ephemerally by the harness.
///
/// Route keys, request paths and queries, status codes, semantic headers,
/// JSON metadata, bodies, metric family/label names, and TLS outcomes are never
/// normalized.
const NORMALIZATION_ALLOWLIST: &[&str] = &[
    "Date values",
    "server-generated connection/framing headers",
    "ephemeral addresses",
];

const CHP_METRIC_FAMILIES: &[(&str, &str)] = &[
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

#[test]
fn oracle_port_lease_holds_the_listener_until_child_launch() {
    let lease = PortLease::new();
    let port = lease.port();
    assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
    drop(lease);
    std::net::TcpListener::bind(("127.0.0.1", port))
        .expect("released differential port can be bound again");
}

struct CustomErrorFixture {
    address: std::net::SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl CustomErrorFixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind custom-error fixture");
        let address = listener.local_addr().expect("custom-error address");
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let router =
                axum::Router::new().fallback(axum::routing::any(|uri: http::Uri| async move {
                    (
                        [(http::header::CONTENT_TYPE, "text/plain")],
                        format!("custom:{uri}"),
                    )
                }));
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve custom-error fixture");
        });
        Self {
            address,
            stop: Some(stop),
            task,
        }
    }

    fn target(&self) -> String {
        format!("http://{}/errors/", self.address)
    }
}

impl Drop for CustomErrorFixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

struct WebSocketFixture {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl WebSocketFixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind websocket fixture");
        let address = listener.local_addr().expect("websocket fixture address");
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream)
                        .await
                        .expect("accept fixture websocket");
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            "connected".into(),
                        ))
                        .await
                        .expect("send fixture greeting");
                    while let Some(message) = socket.next().await {
                        let message = message.expect("fixture websocket message");
                        match message {
                            tokio_tungstenite::tungstenite::Message::Binary(bytes) => socket
                                .send(tokio_tungstenite::tungstenite::Message::Binary(bytes))
                                .await
                                .expect("echo fixture binary"),
                            tokio_tungstenite::tungstenite::Message::Close(frame) => {
                                let _ = frame;
                                break;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        Self { address, task }
    }

    fn target(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for WebSocketFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn metrics_expose_exact_chp_families_and_status_labels() {
    let app = test_api(None).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/routes/metric",
            Some(json!({ "target": "http://127.0.0.1:9" })),
            None,
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        request(&app, "GET", "/api/routes", None, None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "DELETE", "/api/routes/metric", None, None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );

    let response = metrics_router(app.metrics)
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("metrics request"),
        )
        .await
        .expect("metrics router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let body = read_text(response).await;

    for (family, kind) in CHP_METRIC_FAMILIES {
        assert!(
            body.contains(&format!("# TYPE {family} {kind}\n")),
            "missing exact CHP metric family {family}:\n{body}"
        );
    }
    assert!(body.contains("requests_api{status=\"201\"} 1\n"));
    assert!(body.contains("requests_api{status=\"200\"} 1\n"));
    assert!(body.contains("requests_api{status=\"204\"} 1\n"));
    assert!(body.contains("last_activity_updating_count 1\n"));
    assert!(!body.contains("requests_api{code="));
    assert!(!body.contains("requests_proxy{code="));
}

#[tokio::test]
async fn chp_and_rust_agree_on_api_crud_and_encoded_unicode_route() {
    assert_eq!(NORMALIZATION_ALLOWLIST.len(), 3);
    let pair = OraclePair::start().await;
    pair.post_route(
        "/user/%E7%A7%80%E6%A8%B9",
        json!({
            "target": pair.echo_target(),
            "jupyterhub": true,
            "user": "秀樹"
        }),
    )
    .await;
    pair.assert_same_http("/user/%E7%A7%80%E6%A8%B9/tree?x=%2F")
        .await;
    pair.assert_same_route_tables().await;
    pair.post_route(
        "/user/%E7%A7%80%E6%A8%B9",
        json!({
            "target": format!("{}/updated", pair.echo_target()),
            "jupyterhub": true,
            "user": "秀樹-updated"
        }),
    )
    .await;
    pair.assert_same_http("/user/%E7%A7%80%E6%A8%B9/tree?x=%2F")
        .await;
    pair.assert_same_route_tables().await;
    pair.delete_route("/user/%E7%A7%80%E6%A8%B9").await;
    pair.assert_same_route_tables().await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_longest_http_route_selection() {
    let pair = OraclePair::start().await;
    pair.post_route(
        "/selection",
        json!({ "target": format!("{}/broad", pair.echo_target()) }),
    )
    .await;
    pair.post_route(
        "/selection/specific",
        json!({ "target": format!("{}/specific", pair.echo_target()) }),
    )
    .await;
    pair.assert_same_http("/selection/specific/tree?x=%2F")
        .await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_every_path_option_combination() {
    for include_prefix in [false, true] {
        for prepend_path in [false, true] {
            let mut args = Vec::new();
            if !include_prefix {
                args.push("--no-include-prefix".to_owned());
            }
            if !prepend_path {
                args.push("--no-prepend-path".to_owned());
            }
            let pair = OraclePair::start_with_args(&args).await;
            pair.post_route(
                "/base",
                json!({ "target": format!("{}/target", pair.echo_target()) }),
            )
            .await;
            pair.assert_same_http("/base/tail?x=%2F&unicode=%E7%A7%80")
                .await;
        }
    }
}

#[tokio::test]
async fn chp_and_rust_agree_on_host_routing_and_redirect_policy() {
    let pair = OraclePair::start_with_args(&[
        "--host-routing".to_owned(),
        "--auto-rewrite".to_owned(),
        "--protocol-rewrite".to_owned(),
        "https".to_owned(),
    ])
    .await;
    pair.post_route(
        "/example.test/user",
        json!({ "target": pair.echo_target() }),
    )
    .await;
    pair.assert_same_http_with_host("/user/tree?x=%2F", "example.test")
        .await;
    pair.assert_same_http_with_host("/user/redirect", "example.test")
        .await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_health_errors_and_metric_schema() {
    let pair = OraclePair::start().await;
    pair.assert_same_http("/_chp_healthz").await;
    pair.assert_same_http("/missing?url=%2Fsemantic").await;

    let unavailable =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve unavailable target");
    let target = unavailable.local_addr().expect("unavailable address");
    drop(unavailable);
    pair.post_route(
        "/unavailable",
        json!({ "target": format!("http://{target}") }),
    )
    .await;
    pair.assert_same_http("/unavailable").await;
    pair.assert_same_metrics().await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_custom_404_and_503_errors() {
    let errors = CustomErrorFixture::start().await;
    let pair = OraclePair::start_with_args(&["--error-target".to_owned(), errors.target()]).await;
    pair.assert_same_http("/missing?next=%2Ftree").await;

    let unavailable = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve custom-error unavailable target");
    let target = unavailable.local_addr().expect("unavailable address");
    drop(unavailable);
    pair.post_route(
        "/unavailable",
        json!({ "target": format!("http://{target}") }),
    )
    .await;
    pair.assert_same_http("/unavailable?next=%2Ftree").await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_websocket_upgrade_and_messages() {
    let websocket = WebSocketFixture::start().await;
    let pair = OraclePair::start().await;
    pair.post_route("/channels", json!({ "target": websocket.target() }))
        .await;
    pair.assert_same_websocket("/channels/kernel?token=a%2Fb", b"task11-websocket")
        .await;
    pair.assert_same_metrics().await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_public_tls() {
    let server_cert = fixture_path("vendor/pingora-core-0.8.1/examples/keys/server/cert.pem");
    let server_key = fixture_path("vendor/pingora-core-0.8.1/examples/keys/server/key.pem");
    let pair = OraclePair::start_with_args(&[
        "--ssl-cert".to_owned(),
        server_cert.display().to_string(),
        "--ssl-key".to_owned(),
        server_key.display().to_string(),
    ])
    .await;
    pair.assert_same_tls_health(&server_cert).await;
}

#[tokio::test]
async fn chp_and_rust_agree_on_public_mutual_tls() {
    let server_cert = fixture_path("vendor/pingora-core-0.8.1/examples/keys/server/cert.pem");
    let server_key = fixture_path("vendor/pingora-core-0.8.1/examples/keys/server/key.pem");
    let client_ca = fixture_path("vendor/pingora-core-0.8.1/examples/keys/client-ca/cert.pem");
    let client_cert = fixture_path("vendor/pingora-core-0.8.1/examples/keys/clients/cert-1.pem");
    let client_key = fixture_path("vendor/pingora-core-0.8.1/examples/keys/clients/key-1.pem");
    let pair = OraclePair::start_with_args(&[
        "--ssl-cert".to_owned(),
        server_cert.display().to_string(),
        "--ssl-key".to_owned(),
        server_key.display().to_string(),
        "--ssl-ca".to_owned(),
        client_ca.display().to_string(),
        "--ssl-request-cert".to_owned(),
        "--ssl-reject-unauthorized".to_owned(),
    ])
    .await;
    pair.assert_same_mutual_tls_health(&server_cert, &client_cert, &client_key)
        .await;
}

fn fixture_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

#[cfg(unix)]
#[tokio::test]
async fn chp_and_rust_agree_on_unix_public_api_and_metrics_sockets() {
    let pair = OraclePair::start_unix().await;
    pair.post_route("/unix", json!({ "target": pair.echo_target() }))
        .await;
    pair.assert_same_unix_http("/unix/tree?x=%2F").await;
    pair.assert_same_route_tables().await;
    pair.assert_same_metrics().await;
}
