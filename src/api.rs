//! CHP-compatible authenticated route-management API.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use constant_time_eq::constant_time_eq;
use serde_json::{Map, Value};
use url::Url;

use crate::metrics::Metrics;
use crate::route::{RouteData, RouteKey};
use crate::route_table::RouteRegistry;

const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;

/// Shared dependencies for route-management requests.
#[derive(Clone)]
pub struct ApiState {
    pub registry: Arc<RouteRegistry>,
    auth_token: Option<Arc<[u8]>>,
    pub metrics: Arc<Metrics>,
}

impl ApiState {
    pub fn new(
        registry: Arc<RouteRegistry>,
        auth_token: Option<&str>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            registry,
            auth_token: auth_token.map(|token| Arc::from(token.as_bytes())),
            metrics,
        }
    }
}

/// Build the CHP route-management HTTP surface.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route(
            "/api/routes",
            get(get_all_routes)
                .post(post_root_route)
                .delete(delete_root_route),
        )
        .route(
            "/api/routes/",
            get(get_all_routes)
                .post(post_root_route)
                .delete(delete_root_route),
        )
        .route(
            "/api/routes/{*route}",
            get(get_one_route)
                .post(post_one_route)
                .delete(delete_one_route),
        )
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(state)
}

async fn get_all_routes(
    State(state): State<ApiState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if !authorized(&state, &headers) {
        return empty(StatusCode::FORBIDDEN);
    }

    let inactive_since = match inactive_since(query.as_deref()) {
        Ok(value) => value,
        Err(value) => return text(StatusCode::BAD_REQUEST, value),
    };
    let mut routes = state.registry.all();
    if let Some(timestamp) = inactive_since {
        routes.retain(|_, route| route.last_activity < timestamp);
    }

    state.metrics.record_api_route_get();
    Json(routes).into_response()
}

async fn get_one_route(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(route): Path<String>,
) -> Response {
    if !authorized(&state, &headers) {
        return empty(StatusCode::FORBIDDEN);
    }

    let key = route_key(&route);
    match state.registry.get(&key) {
        Some(route) => Json(route).into_response(),
        None => empty(StatusCode::NOT_FOUND),
    }
}

async fn post_root_route(State(state): State<ApiState>, request: Request) -> Response {
    post_route(state, route_key("/"), request).await
}

async fn post_one_route(
    State(state): State<ApiState>,
    Path(route): Path<String>,
    request: Request,
) -> Response {
    post_route(state, route_key(&route), request).await
}

async fn post_route(state: ApiState, key: RouteKey, request: Request) -> Response {
    if !authorized(&state, request.headers()) {
        return empty(StatusCode::FORBIDDEN);
    }

    let body = match to_bytes(request.into_body(), MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return empty(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return text(
                StatusCode::BAD_REQUEST,
                format!("Body not valid JSON: {error}"),
            );
        }
    };
    let mut object = match value {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    let Some(target) = object.remove("target").and_then(|value| match value {
        Value::String(target) => Some(target),
        _ => None,
    }) else {
        return text(StatusCode::BAD_REQUEST, "Must specify 'target' as string");
    };
    let target = match Url::parse(&target) {
        Ok(target) => target,
        Err(_) => return text(StatusCode::BAD_REQUEST, "Invalid target URL"),
    };

    object.remove("last_activity");
    let route = RouteData {
        target,
        last_activity: Utc::now(),
        extra: object,
    };
    if state.registry.put(key, route).await.is_err() {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
    }

    state.metrics.record_api_route_add();
    empty(StatusCode::CREATED)
}

async fn delete_root_route(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    delete_route(state, route_key("/"), &headers).await
}

async fn delete_one_route(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(route): Path<String>,
) -> Response {
    delete_route(state, route_key(&route), &headers).await
}

async fn delete_route(state: ApiState, key: RouteKey, headers: &HeaderMap) -> Response {
    if !authorized(&state, headers) {
        return empty(StatusCode::FORBIDDEN);
    }

    let status = match state.registry.delete(&key).await {
        Ok(Some(_)) => StatusCode::NO_CONTENT,
        Ok(None) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    state.metrics.record_api_route_delete();
    empty(status)
}

async fn method_not_allowed() -> Response {
    text(StatusCode::METHOD_NOT_ALLOWED, "Method not supported")
}

async fn not_found() -> Response {
    text(StatusCode::NOT_FOUND, "Not Found")
}

fn route_key(route: &str) -> RouteKey {
    match RouteKey::parse(route) {
        Ok(key) => key,
        Err(error) => match error {},
    }
}

fn authorized(state: &ApiState, headers: &HeaderMap) -> bool {
    let Some(expected) = &state.auth_token else {
        return true;
    };
    let Some(candidate) = headers
        .get(AUTHORIZATION)
        .and_then(|header| extract_token(header.as_bytes()))
    else {
        return false;
    };

    let equal_length = candidate.len() == expected.len();
    let compared = if equal_length {
        candidate
    } else {
        expected.as_ref()
    };
    equal_length & constant_time_eq(compared, expected)
}

fn extract_token(header: &[u8]) -> Option<&[u8]> {
    let marker = b"token";
    let mut offset = 0;
    while let Some(found) = header[offset..]
        .windows(marker.len())
        .position(|window| window == marker)
    {
        let after_marker = offset + found + marker.len();
        let mut start = after_marker;
        while header.get(start).is_some_and(|byte| is_whitespace(*byte)) {
            start += 1;
        }
        if start > after_marker {
            let end = header[start..]
                .iter()
                .position(|byte| is_whitespace(*byte))
                .map_or(header.len(), |length| start + length);
            if end > start {
                return Some(&header[start..end]);
            }
        }
        offset = after_marker;
    }
    None
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn inactive_since(query: Option<&str>) -> Result<Option<DateTime<Utc>>, String> {
    let parameters: Vec<_> = query
        .into_iter()
        .flat_map(|query| url::form_urlencoded::parse(query.as_bytes()))
        .collect();
    let current = first_value(&parameters, "inactiveSince")
        .filter(|value| !value.is_empty())
        .or_else(|| first_value(&parameters, "inactive_since").filter(|value| !value.is_empty()));
    let Some(raw) = current else {
        return Ok(None);
    };

    DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| Some(timestamp.with_timezone(&Utc)))
        .map_err(|_| format!("Invalid datestamp '{raw}' must be ISO8601."))
}

fn first_value<'a>(
    parameters: &'a [(std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)],
    key: &str,
) -> Option<&'a str> {
    parameters
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_ref())
}

fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    (status, body.into()).into_response()
}
