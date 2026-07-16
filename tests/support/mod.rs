//! Test harness for the CHP-compatible route management API contract tests.
//!
//! Builds a real `api::router` over a fresh in-memory `RouteRegistry` and drives
//! it through `tower::ServiceExt::oneshot`, so every assertion exercises the
//! production handlers rather than a mock.

use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use axum::Router;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::Request;
use pingora_reverse_proxy::api::{router, ApiState};
use pingora_reverse_proxy::metrics::Metrics;
use pingora_reverse_proxy::route_table::RouteRegistry;
use pingora_reverse_proxy::store::memory::MemoryStore;
use pingora_reverse_proxy::store::Store;
use serde_json::Value;
use tower::ServiceExt;

pub use http::StatusCode;
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
