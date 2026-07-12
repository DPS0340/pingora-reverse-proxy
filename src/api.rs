//! CHP-compatible authenticated route-management API.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{OriginalUri, RawQuery, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use constant_time_eq::constant_time_eq;
use serde_json::{Map, Value};

use crate::metrics::Metrics;
use crate::route::RouteKey;
use crate::route_table::RouteRegistry;

const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;

enum RoutePathError {
    BadRequest,
    NotFound,
}

impl IntoResponse for RoutePathError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest => text(StatusCode::BAD_REQUEST, "Bad Request"),
            Self::NotFound => text(StatusCode::NOT_FOUND, "Not Found"),
        }
    }
}

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
            auth_token: auth_token
                .filter(|token| !token.is_empty())
                .map(|token| Arc::from(token.as_bytes())),
            metrics,
        }
    }
}

/// Build the CHP route-management HTTP surface.
pub fn router(state: ApiState) -> Router {
    let metrics = Arc::clone(&state.metrics);
    Router::new()
        .route(
            "/api/routes",
            get(get_all_routes)
                .post(post_root_route)
                .delete(delete_root_route)
                .head(method_not_allowed),
        )
        .route(
            "/api/routes/",
            get(get_all_routes)
                .post(post_root_route)
                .delete(delete_root_route)
                .head(method_not_allowed),
        )
        .route(
            "/api/routes/{*route}",
            get(get_one_route)
                .post(post_one_route)
                .delete(delete_one_route)
                .head(method_not_allowed),
        )
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(middleware::from_fn(move |request, next| {
            record_completed_request(Arc::clone(&metrics), request, next)
        }))
        .with_state(state)
}

async fn record_completed_request(metrics: Arc<Metrics>, request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    metrics.record_api_request(response.status().as_u16());
    response
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
    OriginalUri(uri): OriginalUri,
) -> Response {
    if !authorized(&state, &headers) {
        return empty(StatusCode::FORBIDDEN);
    }

    let key = match route_key_from_uri(&uri, 2) {
        Ok(key) => key,
        Err(error) => return error.into_response(),
    };
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
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Response {
    let key = match route_key_from_uri(&uri, 2) {
        Ok(key) => key,
        Err(error) => return error.into_response(),
    };
    post_route(state, key, request).await
}

async fn post_route(state: ApiState, key: RouteKey, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return empty(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return text(
                StatusCode::BAD_REQUEST,
                format!(
                    "Body not valid JSON: {}",
                    javascript_json_error(&body, &error)
                ),
            );
        }
    };
    if !authorized(&state, &parts.headers) {
        return empty(StatusCode::FORBIDDEN);
    }
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
    object.remove("last_activity");
    if state.registry.add(key, target, object).await.is_err() {
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
    OriginalUri(uri): OriginalUri,
) -> Response {
    let key = match route_key_from_uri(&uri, 1) {
        Ok(key) => key,
        Err(error) => return error.into_response(),
    };
    delete_route(state, key, &headers).await
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
    if !status.is_server_error() {
        state.metrics.record_api_route_delete();
    }
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
        while let Some(width) = javascript_whitespace_width(&header[start..]) {
            start += width;
        }
        if start > after_marker {
            let mut end = start;
            while end < header.len() && javascript_whitespace_width(&header[end..]).is_none() {
                end += 1;
            }
            if end > start {
                return Some(&header[start..end]);
            }
        }
        offset = after_marker;
    }
    None
}

fn javascript_whitespace_width(input: &[u8]) -> Option<usize> {
    let first = *input.first()?;
    if matches!(first, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0xa0) {
        return Some(1);
    }

    let text = std::str::from_utf8(input).ok()?;
    let character = text.chars().next()?;
    matches!(
        character,
        '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
    .then_some(character.len_utf8())
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

    parse_javascript_date(raw)
        .map(Some)
        .map_err(|_| format!("Invalid datestamp '{raw}' must be ISO8601."))
}

fn parse_javascript_date(raw: &str) -> Result<DateTime<Utc>, ()> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(raw) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|value| value.and_utc())
            .ok_or(());
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc2822(raw) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f%#z")
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| ())
}

fn route_key_from_uri(uri: &Uri, clean_count: usize) -> Result<RouteKey, RoutePathError> {
    let Some(raw) = uri.path().strip_prefix("/api/routes") else {
        return Err(RoutePathError::NotFound);
    };
    let decoded = percent_decode(raw).ok_or(RoutePathError::BadRequest)?;
    // CHP cleans GET/POST paths in the handler and store, while DELETE's
    // existence lookup has only reached the store layer.
    let mut key = route_key(if decoded.is_empty() { "/" } else { &decoded });
    for _ in 1..clean_count {
        key = route_key(key.as_str());
    }
    Ok(key)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn javascript_json_error(body: &[u8], error: &serde_json::Error) -> String {
    let Ok(text) = std::str::from_utf8(body) else {
        return format!("SyntaxError: {error}");
    };
    let trimmed = text.trim_end();

    if has_unterminated_string(trimmed) {
        return syntax_error_at("Unterminated string in JSON", text, text.len());
    }
    if trimmed.ends_with("{") {
        return syntax_error_at("Expected property name or '}' in JSON", text, trimmed.len());
    }
    if error.is_eof() {
        return "SyntaxError: Unexpected end of JSON input".to_owned();
    }
    if let Some(position) = trailing_data_position(text) {
        return syntax_error_at(
            "Unexpected non-whitespace character after JSON",
            text,
            position,
        );
    }
    if trimmed.ends_with(",}") {
        let position = trimmed.len() - 1;
        return syntax_error_at(
            "Expected double-quoted property name in JSON",
            text,
            position,
        );
    }
    if trimmed.ends_with(",]") {
        let quoted = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned());
        return format!("SyntaxError: Unexpected token ']', {quoted} is not valid JSON");
    }
    if let Some(position) = unquoted_property_position(text) {
        return syntax_error_at("Expected property name or '}' in JSON", text, position);
    }

    format!("SyntaxError: {error}")
}

fn has_unterminated_string(input: &str) -> bool {
    let mut escaped = false;
    let mut quotes = 0;
    for character in input.chars() {
        if character == '"' && !escaped {
            quotes += 1;
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }
    quotes % 2 == 1
}

fn trailing_data_position(input: &str) -> Option<usize> {
    let mut stream = serde_json::Deserializer::from_str(input).into_iter::<Value>();
    stream.next()?.ok()?;
    let offset = stream.byte_offset();
    input[offset..]
        .char_indices()
        .find(|(_, character)| !character.is_whitespace())
        .map(|(position, _)| offset + position)
}

fn unquoted_property_position(input: &str) -> Option<usize> {
    let open = input.find('{')?;
    input[open + 1..]
        .char_indices()
        .find(|(_, character)| !character.is_whitespace())
        .and_then(|(position, character)| {
            (character != '"' && character != '}').then_some(open + 1 + position)
        })
}

fn syntax_error_at(message: &str, input: &str, position: usize) -> String {
    let prefix = &input[..position.min(input.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.len() + 1, |(_, tail)| tail.len() + 1);
    format!("SyntaxError: {message} at position {position} (line {line} column {column})")
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
