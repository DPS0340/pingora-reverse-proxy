use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::io;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Path, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, patch, put};
use axum::Router;
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::stream;
use pingora_reverse_proxy::route::RouteData;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::{io::AsyncReadExt, io::AsyncWriteExt};

pub const HEALTH: &str = "health";
pub const SNAPSHOT: &str = "snapshot";
pub const PUT: &str = "put";
pub const ACTIVITY: &str = "activity";
pub const DELETE: &str = "delete";

#[derive(Clone, Debug)]
pub enum Fault {
    ServerError,
    ServerErrorGate(FaultGate),
    ClientError,
    Timeout,
    LoseReply,
    LoseReplyGate(FaultGate),
    MalformedBody,
    MissingProtocol,
    DuplicateProtocol,
    WrongProtocol,
    WrongContentType,
    WrongSuccessStatus,
    TruncatedBody,
    OversizedBody,
    OversizedHeaders,
    TooManyHeaders,
    NonEmptyNoContent,
    NonEmptyNotFound,
    MissingLastActivity,
    DuplicateLastActivity,
    MalformedLastActivity,
    NoncanonicalLastActivity,
    RawBody(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct FaultGate {
    arrived: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl FaultGate {
    pub fn new() -> Self {
        Self {
            arrived: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        }
    }

    pub async fn wait_until_arrived(&self) {
        self.arrived.acquire().await.unwrap().forget();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }

    async fn block_response(&self) {
        self.arrived.add_permits(1);
        self.release.acquire().await.unwrap().forget();
    }
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
    health_content_type: Option<String>,
    snapshot_body: Option<Vec<u8>>,
    snapshot_content_type: Option<String>,
}

struct FixtureState {
    routes: RwLock<BTreeMap<String, RouteData>>,
    controls: Mutex<FixtureControls>,
    requests: Mutex<Vec<RecordedRequest>>,
    bearer_token: Option<String>,
    durable_path: PathBuf,
}

pub struct SidecarFixture {
    base_url: String,
    state: Arc<FixtureState>,
    task: Option<tokio::task::JoinHandle<()>>,
    durable_path: PathBuf,
    durable_dir: Option<tempfile::TempDir>,
}

pub struct RawMutationAckFixture {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl RawMutationAckFixture {
    pub async fn start(raw_acknowledgment: impl Into<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acknowledgment = Arc::new(raw_acknowledgment.into());
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acknowledgment = Arc::clone(&acknowledgment);
                tokio::spawn(async move {
                    serve_raw_ack_connection(stream, acknowledgment).await;
                });
            }
        });
        Self {
            base_url: format!("http://{address}/"),
            task,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for RawMutationAckFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_raw_ack_connection(mut stream: tokio::net::TcpStream, acknowledgment: Arc<Vec<u8>>) {
    while let Some(path) = read_raw_request_path(&mut stream).await {
        let response = match path.as_str() {
            "/v1/health" => raw_json_response(br#"{"version":"v1","status":"ok"}"#),
            "/v1/routes" => raw_json_response(b"{}"),
            _ => acknowledgment.as_ref().clone(),
        };
        if stream.write_all(&response).await.is_err() {
            return;
        }
        if !matches!(path.as_str(), "/v1/health" | "/v1/routes") {
            let _ = stream.shutdown().await;
            return;
        }
    }
}

fn raw_json_response(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nX-Store-Protocol: v1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

async fn read_raw_request_path(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    };
    let headers = std::str::from_utf8(&request[..header_end]).ok()?;
    let path = headers.split_whitespace().nth(1)?.to_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut remaining = content_length.saturating_sub(request.len().saturating_sub(header_end));
    while remaining != 0 {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer[..remaining.min(1024)]).await.ok()?;
        if read == 0 {
            return None;
        }
        remaining -= read;
    }
    Some(path)
}

impl SidecarFixture {
    pub async fn start() -> Self {
        Self::start_with_token(None).await
    }

    pub async fn start_with_token(token: Option<&str>) -> Self {
        let durable_dir = tempfile::tempdir().unwrap();
        let durable_path = durable_dir.path().join("routes.json");
        std::fs::write(&durable_path, b"{}").unwrap();
        let state = load_state(&durable_path, token.map(str::to_owned));
        Self::serve(state, durable_dir, durable_path).await
    }

    async fn serve(
        state: Arc<FixtureState>,
        durable_dir: tempfile::TempDir,
        durable_path: PathBuf,
    ) -> Self {
        let app = Router::new()
            .route("/v1/health", get(health))
            .route("/v1/routes", get(snapshot))
            .route("/v1/routes/{key}", put(put_route).delete(delete_route))
            .route("/v1/routes/{key}/activity", patch(update_activity))
            .fallback(not_found)
            .method_not_allowed_fallback(method_not_allowed)
            .with_state(Arc::clone(&state))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                protocol_and_auth,
            ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base_url: format!("http://{address}/"),
            state,
            task: Some(task),
            durable_path,
            durable_dir: Some(durable_dir),
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

    pub async fn set_health_content_type(&self, content_type: &str) {
        self.state.controls.lock().await.health_content_type = Some(content_type.to_owned());
    }

    pub async fn set_raw_snapshot_body(&self, body: &[u8]) {
        self.state.controls.lock().await.snapshot_body = Some(body.to_vec());
    }

    pub async fn set_snapshot_content_type(&self, content_type: &str) {
        self.state.controls.lock().await.snapshot_content_type = Some(content_type.to_owned());
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

    pub fn state_identity(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }

    pub fn durable_path(&self) -> Option<&StdPath> {
        Some(&self.durable_path)
    }

    pub async fn restart(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        let token = self.state.bearer_token.clone();
        let state = load_state(&self.durable_path, token);
        let durable_dir = self.durable_dir.take().unwrap();
        let mut replacement = Self::serve(state, durable_dir, self.durable_path.clone()).await;
        self.base_url = replacement.base_url.clone();
        self.state = Arc::clone(&replacement.state);
        self.task = replacement.task.take();
        self.durable_path = replacement.durable_path.clone();
        self.durable_dir = replacement.durable_dir.take();
    }
}

fn load_state(durable_path: &StdPath, bearer_token: Option<String>) -> Arc<FixtureState> {
    let bytes = std::fs::read(durable_path).unwrap();
    let routes = serde_json::from_slice(&bytes).unwrap();
    Arc::new(FixtureState {
        routes: RwLock::new(routes),
        controls: Mutex::new(FixtureControls::default()),
        requests: Mutex::new(Vec::new()),
        bearer_token,
        durable_path: durable_path.to_owned(),
    })
}

fn persist_routes(state: &FixtureState, routes: &BTreeMap<String, RouteData>) -> Result<(), ()> {
    let encoded: BTreeMap<_, _> = routes
        .iter()
        .map(|(key, route)| (key, route_value(route)))
        .collect();
    let body = serde_json::to_vec(&encoded).map_err(|_| ())?;
    let temporary = state.durable_path.with_extension("json.tmp");
    std::fs::write(&temporary, body).map_err(|_| ())?;
    std::fs::rename(temporary, &state.durable_path).map_err(|_| ())
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

#[derive(Clone, Copy)]
struct PreserveFaultedProtocolHeader;

async fn protocol_and_auth(
    State(state): State<Arc<FixtureState>>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let protocol_is_valid = headers
        .get_all("x-store-protocol")
        .iter()
        .map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .as_slice()
        == [Some("v1")];
    let auth_is_valid = state.bearer_token.as_ref().is_none_or(|token| {
        let expected = format!("Bearer {token}");
        let mut values = headers.get_all(AUTHORIZATION).iter();
        matches!(
            (values.next(), values.next()),
            (Some(value), None) if value.as_bytes() == expected.as_bytes()
        )
    });
    let rejected_method = request.method().clone();
    let rejected_path = request.uri().path().to_owned();
    let mut response = if !protocol_is_valid {
        protocol_response(StatusCode::BAD_REQUEST, Body::empty())
    } else if !auth_is_valid {
        protocol_response(StatusCode::UNAUTHORIZED, Body::empty())
    } else {
        next.run(request).await
    };
    if !protocol_is_valid || !auth_is_valid {
        state.requests.lock().await.push(RecordedRequest {
            method: rejected_method,
            path: rejected_path,
            body: Vec::new(),
        });
    }
    if response
        .extensions()
        .get::<PreserveFaultedProtocolHeader>()
        .is_none()
    {
        response
            .headers_mut()
            .insert("x-store-protocol", HeaderValue::from_static("v1"));
    }
    response
}

fn json_response(status: StatusCode, body: &impl serde::Serialize) -> Response {
    let mut response = protocol_response(status, Body::from(serde_json::to_vec(body).unwrap()));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

async fn not_found() -> Response {
    protocol_response(StatusCode::NOT_FOUND, Body::empty())
}

async fn method_not_allowed() -> Response {
    protocol_response(StatusCode::METHOD_NOT_ALLOWED, Body::empty())
}

fn postcommit_response(fault: Option<&Fault>, mut response: Response) -> Response {
    match fault {
        Some(Fault::MalformedBody) => json_response(StatusCode::OK, &json!({"broken": true})),
        Some(Fault::MissingProtocol) => {
            response.headers_mut().remove("x-store-protocol");
            response
                .extensions_mut()
                .insert(PreserveFaultedProtocolHeader);
            response
        }
        Some(Fault::DuplicateProtocol) => {
            response
                .headers_mut()
                .append("x-store-protocol", HeaderValue::from_static("v1"));
            response
                .extensions_mut()
                .insert(PreserveFaultedProtocolHeader);
            response
        }
        Some(Fault::WrongProtocol) => {
            response.headers_mut().insert(
                "x-store-protocol",
                HeaderValue::from_static("wrong-version"),
            );
            response
                .extensions_mut()
                .insert(PreserveFaultedProtocolHeader);
            response
        }
        Some(Fault::WrongContentType) => {
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            response
        }
        Some(Fault::WrongSuccessStatus) => {
            *response.status_mut() = StatusCode::FOUND;
            response
        }
        Some(Fault::TruncatedBody) => {
            if response.status() == StatusCode::NO_CONTENT {
                response.headers_mut().remove(CONTENT_TYPE);
                response
                    .headers_mut()
                    .insert("content-length", HeaderValue::from_static("29"));
                *response.body_mut() = Body::from_stream(stream::iter([
                    Ok::<_, io::Error>(Bytes::from_static(b"partial")),
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated")),
                ]));
            } else {
                *response.status_mut() = StatusCode::OK;
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                *response.body_mut() = Body::from_stream(stream::iter([
                    Ok::<_, io::Error>(Bytes::from_static(b"{\"target\":\"secret")),
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated")),
                ]));
            }
            response
        }
        Some(Fault::OversizedBody) => {
            if response.status() == StatusCode::NO_CONTENT {
                response.headers_mut().remove(CONTENT_TYPE);
                response
                    .headers_mut()
                    .insert("content-length", HeaderValue::from_static("4194305"));
            } else {
                *response.status_mut() = StatusCode::OK;
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                *response.body_mut() = Body::from(vec![b'x'; 4 * 1024 * 1024 + 1]);
            }
            response
        }
        Some(Fault::OversizedHeaders) => {
            let value = HeaderValue::from_bytes(&vec![b'x'; 2_048]).unwrap();
            for index in 0..20 {
                let name: axum::http::HeaderName = format!("x-padding-{index}").parse().unwrap();
                response.headers_mut().insert(name, value.clone());
            }
            response
        }
        Some(Fault::TooManyHeaders) => {
            for index in 0..65 {
                let name: axum::http::HeaderName = format!("x-count-{index}").parse().unwrap();
                response
                    .headers_mut()
                    .insert(name, HeaderValue::from_static("x"));
            }
            response
        }
        Some(Fault::NonEmptyNoContent) => {
            *response.status_mut() = StatusCode::NO_CONTENT;
            response.headers_mut().remove(CONTENT_TYPE);
            response
                .headers_mut()
                .insert("content-length", HeaderValue::from_static("11"));
            *response.body_mut() = Body::from("secret-body");
            response
        }
        Some(Fault::NonEmptyNotFound) => {
            *response.status_mut() = StatusCode::NOT_FOUND;
            *response.body_mut() = Body::from("secret-body");
            response
        }
        Some(Fault::MissingLastActivity) => {
            response.headers_mut().remove("x-store-last-activity");
            response
        }
        Some(Fault::DuplicateLastActivity) => {
            response.headers_mut().append(
                "x-store-last-activity",
                HeaderValue::from_static("1970-01-01T00:00:01.000000000Z"),
            );
            response
        }
        Some(Fault::MalformedLastActivity) => {
            response.headers_mut().insert(
                "x-store-last-activity",
                HeaderValue::from_static("not-a-timestamp"),
            );
            response
        }
        Some(Fault::NoncanonicalLastActivity) => {
            response.headers_mut().insert(
                "x-store-last-activity",
                HeaderValue::from_static("1970-01-01T00:00:01Z"),
            );
            response
        }
        Some(Fault::RawBody(body)) => {
            *response.status_mut() = StatusCode::OK;
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            *response.body_mut() = Body::from(body.clone());
            response
        }
        Some(
            Fault::ServerError
            | Fault::ServerErrorGate(_)
            | Fault::ClientError
            | Fault::Timeout
            | Fault::LoseReply
            | Fault::LoseReplyGate(_),
        )
        | None => response,
    }
}

async fn validate_content_type_and_record(
    state: &FixtureState,
    method: Method,
    uri: OriginalUri,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<Response> {
    state.requests.lock().await.push(RecordedRequest {
        method: method.clone(),
        path: uri.path().to_owned(),
        body: body.to_vec(),
    });
    if matches!(method, Method::PUT | Method::PATCH)
        && headers
            .get_all(CONTENT_TYPE)
            .iter()
            .map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .as_slice()
            != [Some("application/json")]
    {
        return Some(protocol_response(StatusCode::BAD_REQUEST, Body::empty()));
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
        Some(Fault::ServerErrorGate(gate)) => {
            gate.block_response().await;
            Some(protocol_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                Body::empty(),
            ))
        }
        Some(Fault::LoseReplyGate(gate)) => {
            gate.block_response().await;
            None
        }
        Some(Fault::Timeout) => std::future::pending::<Option<Response>>().await,
        Some(Fault::MalformedBody) => Some(json_response(StatusCode::OK, &json!({"broken": true}))),
        Some(
            fault @ (Fault::WrongContentType
            | Fault::OversizedBody
            | Fault::OversizedHeaders
            | Fault::TooManyHeaders),
        ) => {
            let response = if operation == HEALTH {
                json_response(StatusCode::OK, &json!({"version": "v1", "status": "ok"}))
            } else {
                json_response(StatusCode::OK, &json!({}))
            };
            Some(postcommit_response(Some(&fault), response))
        }
        Some(
            Fault::LoseReply
            | Fault::MissingProtocol
            | Fault::DuplicateProtocol
            | Fault::WrongProtocol
            | Fault::WrongSuccessStatus
            | Fault::NonEmptyNoContent
            | Fault::NonEmptyNotFound
            | Fault::MissingLastActivity
            | Fault::DuplicateLastActivity
            | Fault::MalformedLastActivity
            | Fault::NoncanonicalLastActivity
            | Fault::RawBody(_),
        ) => None,
        Some(Fault::TruncatedBody) => {
            let response = if operation == HEALTH {
                json_response(StatusCode::OK, &json!({"version": "v1", "status": "ok"}))
            } else {
                json_response(StatusCode::OK, &json!({}))
            };
            Some(postcommit_response(Some(&Fault::TruncatedBody), response))
        }
        None => None,
    }
}

async fn health(
    State(state): State<Arc<FixtureState>>,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(response) =
        validate_content_type_and_record(&state, Method::GET, uri, &headers, &[]).await
    {
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
    let content_type = controls.health_content_type.clone();
    drop(controls);
    let mut response = protocol_response(StatusCode::OK, Body::from(body));
    if let Some(protocol) = protocol {
        response.headers_mut().insert(
            "x-store-protocol",
            HeaderValue::from_str(&protocol).unwrap(),
        );
        response
            .extensions_mut()
            .insert(PreserveFaultedProtocolHeader);
    }
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type.as_deref().unwrap_or("application/json")).unwrap(),
    );
    response
}

async fn snapshot(
    State(state): State<Arc<FixtureState>>,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(response) =
        validate_content_type_and_record(&state, Method::GET, uri, &headers, &[]).await
    {
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
    let controls = state.controls.lock().await;
    let body = controls
        .snapshot_body
        .clone()
        .unwrap_or_else(|| serde_json::to_vec(&routes).unwrap());
    let content_type = controls.snapshot_content_type.clone();
    drop(controls);
    let mut response = protocol_response(StatusCode::OK, Body::from(body));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type.as_deref().unwrap_or("application/json")).unwrap(),
    );
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutEnvelope {
    version: String,
    operation: String,
    route: StrictRouteData,
    #[serde(rename = "activityFloor")]
    activity_floor: String,
}

async fn put_route(
    State(state): State<Arc<FixtureState>>,
    Path(route_key): Path<String>,
    uri: OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) =
        validate_content_type_and_record(&state, Method::PUT, uri, &headers, &body).await
    {
        return response;
    }
    let fault = next_fault(&state, PUT).await;
    match fault.as_ref() {
        Some(Fault::ServerError) => {
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty())
        }
        Some(Fault::ServerErrorGate(gate)) => {
            gate.block_response().await;
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty());
        }
        Some(Fault::ClientError) => {
            return protocol_response(StatusCode::BAD_REQUEST, Body::empty())
        }
        Some(Fault::Timeout) => std::future::pending::<()>().await,
        Some(
            Fault::LoseReply
            | Fault::LoseReplyGate(_)
            | Fault::MalformedBody
            | Fault::MissingProtocol
            | Fault::DuplicateProtocol
            | Fault::WrongProtocol
            | Fault::WrongContentType
            | Fault::WrongSuccessStatus
            | Fault::TruncatedBody
            | Fault::OversizedBody
            | Fault::OversizedHeaders
            | Fault::TooManyHeaders
            | Fault::NonEmptyNoContent
            | Fault::NonEmptyNotFound
            | Fault::MissingLastActivity
            | Fault::DuplicateLastActivity
            | Fault::MalformedLastActivity
            | Fault::NoncanonicalLastActivity
            | Fault::RawBody(_),
        )
        | None => {}
    }
    let Ok(envelope) = serde_json::from_slice::<PutEnvelope>(&body) else {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    let mut route = envelope.route.0;
    let Ok(floor) = strict_timestamp(&envelope.activity_floor) else {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    if envelope.version != "v1"
        || !route_key.starts_with('/')
        || !matches!(
            envelope.operation.as_str(),
            "add" | "put" | "put_preserving_activity"
        )
    {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    }
    let mut routes = state.routes.write().await;
    let mut next = routes.clone();
    match envelope.operation.as_str() {
        "add" | "put_preserving_activity" => {
            route.last_activity = route.last_activity.max(floor);
            if let Some(existing) = next.get(&route_key) {
                route.last_activity = route.last_activity.max(existing.last_activity);
            }
        }
        "put" => {}
        _ => return protocol_response(StatusCode::BAD_REQUEST, Body::empty()),
    }
    let committed_activity = route.last_activity;
    next.insert(route_key, route);
    if persist_routes(&state, &next).is_err() {
        return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty());
    }
    *routes = next;
    drop(routes);
    if let Some(Fault::LoseReplyGate(gate)) = fault.as_ref() {
        gate.block_response().await;
    }
    if matches!(fault, Some(Fault::LoseReply | Fault::LoseReplyGate(_))) {
        std::future::pending::<()>().await;
    }
    let mut response = protocol_response(StatusCode::NO_CONTENT, Body::empty());
    response.headers_mut().insert(
        "x-store-last-activity",
        HeaderValue::from_str(&chp_timestamp(committed_activity)).unwrap(),
    );
    postcommit_response(fault.as_ref(), response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    if let Some(response) =
        validate_content_type_and_record(&state, Method::PATCH, uri, &headers, &body).await
    {
        return response;
    }
    let fault = next_fault(&state, ACTIVITY).await;
    match fault.as_ref() {
        Some(Fault::ServerError) => {
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty())
        }
        Some(Fault::ServerErrorGate(gate)) => {
            gate.block_response().await;
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty());
        }
        Some(Fault::ClientError) => {
            return protocol_response(StatusCode::BAD_REQUEST, Body::empty())
        }
        Some(Fault::Timeout) => std::future::pending::<()>().await,
        Some(
            Fault::LoseReply
            | Fault::LoseReplyGate(_)
            | Fault::MalformedBody
            | Fault::MissingProtocol
            | Fault::DuplicateProtocol
            | Fault::WrongProtocol
            | Fault::WrongContentType
            | Fault::WrongSuccessStatus
            | Fault::TruncatedBody
            | Fault::OversizedBody
            | Fault::OversizedHeaders
            | Fault::TooManyHeaders
            | Fault::NonEmptyNoContent
            | Fault::NonEmptyNotFound
            | Fault::MissingLastActivity
            | Fault::DuplicateLastActivity
            | Fault::MalformedLastActivity
            | Fault::NoncanonicalLastActivity
            | Fault::RawBody(_),
        )
        | None => {}
    }
    let Ok(envelope) = serde_json::from_slice::<ActivityEnvelope>(&body) else {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    let Ok(at) = strict_timestamp(&envelope.last_activity) else {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    };
    if envelope.version != "v1" {
        return protocol_response(StatusCode::BAD_REQUEST, Body::empty());
    }
    let mut routes = state.routes.write().await;
    let mut next = routes.clone();
    if let Some(route) = next.get_mut(&route_key) {
        route.last_activity = at;
    }
    if persist_routes(&state, &next).is_err() {
        return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty());
    }
    *routes = next;
    drop(routes);
    if let Some(Fault::LoseReplyGate(gate)) = fault.as_ref() {
        gate.block_response().await;
    }
    if matches!(fault, Some(Fault::LoseReply | Fault::LoseReplyGate(_))) {
        std::future::pending::<()>().await;
    }
    postcommit_response(
        fault.as_ref(),
        protocol_response(StatusCode::NO_CONTENT, Body::empty()),
    )
}

async fn delete_route(
    State(state): State<Arc<FixtureState>>,
    Path(route_key): Path<String>,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(response) =
        validate_content_type_and_record(&state, Method::DELETE, uri, &headers, &[]).await
    {
        return response;
    }
    let fault = next_fault(&state, DELETE).await;
    match fault.as_ref() {
        Some(Fault::ServerError) => {
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty())
        }
        Some(Fault::ServerErrorGate(gate)) => {
            gate.block_response().await;
            return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty());
        }
        Some(Fault::ClientError) => {
            return protocol_response(StatusCode::BAD_REQUEST, Body::empty())
        }
        Some(Fault::Timeout) => std::future::pending::<()>().await,
        Some(
            Fault::LoseReply
            | Fault::LoseReplyGate(_)
            | Fault::MalformedBody
            | Fault::MissingProtocol
            | Fault::DuplicateProtocol
            | Fault::WrongProtocol
            | Fault::WrongContentType
            | Fault::WrongSuccessStatus
            | Fault::TruncatedBody
            | Fault::OversizedBody
            | Fault::OversizedHeaders
            | Fault::TooManyHeaders
            | Fault::NonEmptyNoContent
            | Fault::NonEmptyNotFound
            | Fault::MissingLastActivity
            | Fault::DuplicateLastActivity
            | Fault::MalformedLastActivity
            | Fault::NoncanonicalLastActivity
            | Fault::RawBody(_),
        )
        | None => {}
    }
    let mut routes = state.routes.write().await;
    let mut next = routes.clone();
    let prior = next.remove(&route_key);
    if persist_routes(&state, &next).is_err() {
        return protocol_response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty());
    }
    *routes = next;
    drop(routes);
    if let Some(Fault::LoseReplyGate(gate)) = fault.as_ref() {
        gate.block_response().await;
    }
    if matches!(fault, Some(Fault::LoseReply | Fault::LoseReplyGate(_))) {
        std::future::pending::<()>().await;
    }
    let response = match prior {
        Some(route) => json_response(StatusCode::OK, &route_value(&route)),
        None => protocol_response(StatusCode::NOT_FOUND, Body::empty()),
    };
    postcommit_response(fault.as_ref(), response)
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

fn strict_timestamp(raw: &str) -> Result<DateTime<Utc>, ()> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map_err(|_| ())?
        .with_timezone(&Utc);
    (chp_timestamp(parsed) == raw).then_some(parsed).ok_or(())
}

struct StrictRouteData(RouteData);

impl<'de> Deserialize<'de> for StrictRouteData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(StrictRouteVisitor)
    }
}

struct StrictRouteVisitor;

impl<'de> Visitor<'de> for StrictRouteVisitor {
    type Value = StrictRouteData;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a route object with unique fields")
    }

    fn visit_map<A>(self, mut fields: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut target = None;
        let mut last_activity = None;
        let mut extra = serde_json::Map::new();
        while let Some(field) = fields.next_key::<String>()? {
            match field.as_str() {
                "target" => {
                    if target.is_some() {
                        return Err(de::Error::duplicate_field("target"));
                    }
                    target = Some(fields.next_value::<String>()?);
                }
                "last_activity" => {
                    if last_activity.is_some() {
                        return Err(de::Error::duplicate_field("last_activity"));
                    }
                    let raw = fields.next_value::<String>()?;
                    last_activity = Some(
                        strict_timestamp(&raw)
                            .map_err(|_| de::Error::custom("noncanonical timestamp"))?,
                    );
                }
                _ => {
                    if extra.contains_key(&field) {
                        return Err(de::Error::custom("duplicate metadata field"));
                    }
                    extra.insert(field, fields.next_value::<Value>()?);
                }
            }
        }
        Ok(StrictRouteData(RouteData {
            target: target.ok_or_else(|| de::Error::missing_field("target"))?,
            last_activity: last_activity
                .ok_or_else(|| de::Error::missing_field("last_activity"))?,
            extra,
        }))
    }
}
