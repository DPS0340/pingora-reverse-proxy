use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Path, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, patch, put};
use axum::Router;
use chrono::{DateTime, SecondsFormat, Utc};
use pingora_reverse_proxy::route::RouteData;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};

pub const HEALTH: &str = "health";
pub const SNAPSHOT: &str = "snapshot";
pub const PUT: &str = "put";
pub const ACTIVITY: &str = "activity";
pub const DELETE: &str = "delete";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    ServerError,
    ClientError,
    Timeout,
    LoseReply,
    MalformedBody,
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: Method,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Default)]
struct FixtureControls {
    faults: HashMap<&'static str, VecDeque<Fault>>,
    health_body: Option<Vec<u8>>,
    health_protocol: Option<String>,
}

struct FixtureState {
    routes: RwLock<BTreeMap<String, RouteData>>,
    controls: Mutex<FixtureControls>,
    requests: Mutex<Vec<RecordedRequest>>,
    bearer_token: Option<String>,
}

pub struct SidecarFixture {
    base_url: String,
    state: Arc<FixtureState>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SidecarFixture {
    pub async fn start() -> Self {
        Self::start_with_token(None).await
    }

    pub async fn start_with_token(token: Option<&str>) -> Self {
        let state = Arc::new(FixtureState {
            routes: RwLock::new(BTreeMap::new()),
            controls: Mutex::new(FixtureControls::default()),
            requests: Mutex::new(Vec::new()),
            bearer_token: token.map(str::to_owned),
        });
        Self::serve(state).await
    }

    async fn serve(state: Arc<FixtureState>) -> Self {
        let app = Router::new()
            .route("/v1/health", get(health))
            .route("/v1/routes", get(snapshot))
            .route("/v1/routes/{key}", put(put_route).delete(delete_route))
            .route("/v1/routes/{key}/activity", patch(update_activity))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base_url: format!("http://{address}/"),
            state,
            task: Some(task),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn push_fault(&self, operation: &'static str, fault: Fault) {
        self.state
            .controls
            .lock()
            .await
            .faults
            .entry(operation)
            .or_default()
            .push_back(fault);
    }

    pub async fn set_health_body(&self, body: Value) {
        self.state.controls.lock().await.health_body = Some(serde_json::to_vec(&body).unwrap());
    }

    pub async fn set_raw_health_body(&self, body: &[u8]) {
        self.state.controls.lock().await.health_body = Some(body.to_vec());
    }

    pub async fn set_health_protocol(&self, protocol: &str) {
        self.state.controls.lock().await.health_protocol = Some(protocol.to_owned());
    }

    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().await.clone()
    }

    pub async fn request_count(&self, operation: &'static str) -> usize {
        self.requests()
            .await
            .iter()
            .filter(|request| operation_for(&request.method, &request.path) == operation)
            .count()
    }

    pub async fn routes(&self) -> BTreeMap<String, RouteData> {
        self.state.routes.read().await.clone()
    }

    pub async fn restart(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let mut replacement = Self::serve(Arc::clone(&self.state)).await;
        self.base_url = replacement.base_url.clone();
        self.task = replacement.task.take();
    }
}

impl Drop for SidecarFixture {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn operation_for(method: &Method, path: &str) -> &'static str {
    if path == "/v1/health" {
        HEALTH
    } else if path == "/v1/routes" {
        SNAPSHOT
    } else if method == Method::PATCH {
        ACTIVITY
    } else if method == Method::DELETE {
        DELETE
    } else {
        PUT
    }
}

fn protocol_response(status: StatusCode, body: impl Into<Body>) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("x-store-protocol", HeaderValue::from_static("v1"));
    response
}

fn json_response(status: StatusCode, body: &impl serde::Serialize) -> Response {
    let mut response = protocol_response(status, Body::from(serde_json::to_vec(body).unwrap()));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

async fn authorize_and_record(
    state: &FixtureState,
    method: Method,
    uri: OriginalUri,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<Response> {
    state.requests.lock().await.push(RecordedRequest {
        method,
        path: uri.path().to_owned(),
        body: body.to_vec(),
    });
    if headers
        .get("x-store-protocol")
        .and_then(|value| value.to_str().ok())
        != Some("v1")
    {
        return Some(protocol_response(StatusCode::BAD_REQUEST, Body::empty()));
    }
    if let Some(token) = &state.bearer_token {
        let expected = format!("Bearer {token}");
        if headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            != Some(&expected)
        {
            return Some(protocol_response(StatusCode::UNAUTHORIZED, Body::empty()));
        }
    }
    None
}

async fn next_fault(state: &FixtureState, operation: &'static str) -> Option<Fault> {
    state
        .controls
        .lock()
        .await
        .faults
        .get_mut(operation)
        .and_then(VecDeque::pop_front)
}

async fn precommit_fault(state: &FixtureState, operation: &'static str) -> Option<Response> {
    match next_fault(state, operation).await {
        Some(Fault::ServerError) => Some(protocol_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            Body::empty(),
        )),
        Some(Fault::ClientError) => Some(protocol_response(StatusCode::BAD_REQUEST, Body::empty())),
        Some(Fault::Timeout) => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            None
        }
        Some(Fault::MalformedBody) => Some(json_response(StatusCode::OK, &json!({"broken": true}))),
        Some(Fault::LoseReply) => None,
        None => None,
    }
}

async fn health(
    State(state): State<Arc<FixtureState>>,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = authorize_and_record(&state, Method::GET, uri, &headers, &[]).await {
        return response;
    }
    if let Some(response) = precommit_fault(&state, HEALTH).await {
        return response;
    }
    let controls = state.controls.lock().await;
    let body = controls
        .health_body
        .clone()
        .unwrap_or_else(|| serde_json::to_vec(&json!({"version": "v1", "status": "ok"})).unwrap());
    let protocol = controls.health_protocol.clone();
    drop(controls);
    let mut response = protocol_response(StatusCode::OK, Body::from(body));
    if let Some(protocol) = protocol {
        response.headers_mut().insert(
            "x-store-protocol",
            HeaderValue::from_str(&protocol).unwrap(),
        );
    }
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

async fn snapshot(
    State(state): State<Arc<FixtureState>>,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = authorize_and_record(&state, Method::GET, uri, &headers, &[]).await {
        return response;
    }
    if let Some(response) = precommit_fault(&state, SNAPSHOT).await {
        return response;
    }
    let routes: BTreeMap<_, _> = state
        .routes
        .read()
        .await
        .iter()
        .map(|(key, route)| (key.clone(), route_value(route)))
        .collect();
    json_response(StatusCode::OK, &routes)
}

#[derive(Deserialize)]
struct PutEnvelope {
    version: String,
    operation: String,
    route: RouteData,
    #[serde(rename = "activityFloor")]
    activity_floor: Option<String>,
}

async fn put_route(
    State(state): State<Arc<FixtureState>>,
    Path(route_key): Path<String>,
    uri: OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = authorize_and_record(&state, Method::PUT, uri, &headers, &body).await {
        return response;
    }
    let fault = next_fault(&state, PUT).await;
    match fault {
        Some(Fault::ServerError) => {
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty())
        }
        Some(Fault::ClientError) => {
            return protocol_response(StatusCode::BAD_REQUEST, Body::empty())
        }
        Some(Fault::Timeout) => tokio::time::sleep(Duration::from_secs(30)).await,
        Some(Fault::MalformedBody) => {
            return json_response(StatusCode::OK, &json!({"broken": true}))
        }
        Some(Fault::LoseReply) | None => {}
    }
    let Ok(mut envelope) = serde_json::from_slice::<PutEnvelope>(&body) else {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    if envelope.version != "v1"
        || !matches!(
            envelope.operation.as_str(),
            "add" | "put" | "put_preserving_activity"
        )
    {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    }
    let mut routes = state.routes.write().await;
    match envelope.operation.as_str() {
        "add" => {
            envelope.route.last_activity = envelope
                .route
                .last_activity
                .max(parse_floor(envelope.activity_floor.as_deref()));
        }
        "put_preserving_activity" => {
            envelope.route.last_activity = envelope
                .route
                .last_activity
                .max(parse_floor(envelope.activity_floor.as_deref()));
        }
        "put" => {}
        _ => unreachable!(),
    }
    routes.insert(route_key, envelope.route);
    drop(routes);
    if fault == Some(Fault::LoseReply) {
        std::future::pending::<()>().await;
    }
    protocol_response(StatusCode::NO_CONTENT, Body::empty())
}

fn parse_floor(floor: Option<&str>) -> DateTime<Utc> {
    floor
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

#[derive(Deserialize)]
struct ActivityEnvelope {
    version: String,
    #[serde(rename = "lastActivity")]
    last_activity: String,
}

async fn update_activity(
    State(state): State<Arc<FixtureState>>,
    Path(route_key): Path<String>,
    uri: OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = authorize_and_record(&state, Method::PATCH, uri, &headers, &body).await
    {
        return response;
    }
    let fault = next_fault(&state, ACTIVITY).await;
    match fault {
        Some(Fault::ServerError) => {
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty())
        }
        Some(Fault::ClientError) => {
            return protocol_response(StatusCode::BAD_REQUEST, Body::empty())
        }
        Some(Fault::Timeout) => tokio::time::sleep(Duration::from_secs(30)).await,
        Some(Fault::MalformedBody) => {
            return json_response(StatusCode::OK, &json!({"broken": true}))
        }
        Some(Fault::LoseReply) | None => {}
    }
    let Ok(envelope) = serde_json::from_slice::<ActivityEnvelope>(&body) else {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    let Ok(at) = DateTime::parse_from_rfc3339(&envelope.last_activity) else {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    if envelope.version != "v1" {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    }
    if let Some(route) = state.routes.write().await.get_mut(&route_key) {
        route.last_activity = at.with_timezone(&Utc);
    }
    if fault == Some(Fault::LoseReply) {
        std::future::pending::<()>().await;
    }
    protocol_response(StatusCode::NO_CONTENT, Body::empty())
}

async fn delete_route(
    State(state): State<Arc<FixtureState>>,
    Path(route_key): Path<String>,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = authorize_and_record(&state, Method::DELETE, uri, &headers, &[]).await {
        return response;
    }
    let fault = next_fault(&state, DELETE).await;
    match fault {
        Some(Fault::ServerError) => {
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty())
        }
        Some(Fault::ClientError) => {
            return protocol_response(StatusCode::BAD_REQUEST, Body::empty())
        }
        Some(Fault::Timeout) => tokio::time::sleep(Duration::from_secs(30)).await,
        Some(Fault::MalformedBody) => {
            return json_response(StatusCode::OK, &json!({"broken": true}))
        }
        Some(Fault::LoseReply) | None => {}
    }
    let prior = state.routes.write().await.remove(&route_key);
    if fault == Some(Fault::LoseReply) {
        std::future::pending::<()>().await;
    }
    match prior {
        Some(route) => json_response(StatusCode::OK, &route_value(&route)),
        None => protocol_response(StatusCode::NOT_FOUND, Body::empty()),
    }
}

pub fn chp_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn route_value(route: &RouteData) -> Value {
    let mut encoded = serde_json::to_value(route).unwrap();
    encoded.as_object_mut().unwrap().insert(
        "last_activity".to_owned(),
        Value::String(chp_timestamp(route.last_activity)),
    );
    encoded
}
