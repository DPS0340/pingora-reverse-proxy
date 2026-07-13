//! Client for the versioned HTTP/JSON sidecar storage protocol.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{
    HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING,
};
use reqwest::{Method, StatusCode, Url};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::route::{RouteData, RouteKey};

use super::{ActivityFloor, Store, StoreError};

const PROTOCOL_HEADER: &str = "x-store-protocol";
const PROTOCOL_VERSION: &str = "v1";
const LAST_ACTIVITY_HEADER: &str = "x-store-last-activity";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_ATTEMPTS: usize = 3;
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_millis(200);
const MAX_ATTEMPTS: usize = 10;

/// Maximum serialized PUT or PATCH request body accepted before dispatch.
pub const MAX_MUTATION_REQUEST_BYTES: usize = 1024 * 1024;
/// Maximum response body accumulated by the client.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of received response header values accepted by the client.
pub const MAX_RESPONSE_HEADER_COUNT: usize = 64;
/// Maximum aggregate received response header name/value bytes.
pub const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1024;

/// Connection, authentication, and finite retry settings for [`SidecarStore`].
#[derive(Clone, PartialEq, Eq)]
pub struct SidecarConfig {
    base_url: String,
    bearer_token: Option<String>,
    connect_timeout: Duration,
    request_timeout: Duration,
    max_attempts: usize,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl fmt::Debug for SidecarConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SidecarConfig")
            .field("base_url", &"<redacted>")
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_attempts", &self.max_attempts)
            .field("initial_backoff", &self.initial_backoff)
            .field("max_backoff", &self.max_backoff)
            .finish()
    }
}

impl SidecarConfig {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer_token: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_attempts: DEFAULT_ATTEMPTS,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }

    pub fn with_bearer_token(mut self, bearer_token: impl Into<String>) -> Self {
        self.bearer_token = Some(bearer_token.into());
        self
    }

    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    pub fn with_retry_policy(
        mut self,
        max_attempts: usize,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        self.max_attempts = max_attempts.clamp(1, MAX_ATTEMPTS);
        self.initial_backoff = initial_backoff.min(max_backoff);
        self.max_backoff = max_backoff;
        self
    }
}

/// A bounded client for one v1 sidecar route store.
pub struct SidecarStore {
    client: reqwest::Client,
    base_url: Url,
    max_attempts: usize,
    initial_backoff: Duration,
    max_backoff: Duration,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl fmt::Debug for SidecarStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SidecarStore")
            .field("base_url", &"<redacted>")
            .field("max_attempts", &self.max_attempts)
            .field("initial_backoff", &self.initial_backoff)
            .field("max_backoff", &self.max_backoff)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct HttpReply {
    status: StatusCode,
    body: Vec<u8>,
    last_activity: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy)]
enum MutationAcknowledgment {
    Put,
    Empty,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyReadError {
    Transport,
    TooLarge,
}

#[derive(Serialize)]
struct PutEnvelope {
    version: &'static str,
    operation: &'static str,
    route: Value,
    #[serde(rename = "activityFloor")]
    activity_floor: String,
}

#[derive(Serialize)]
struct ActivityEnvelope {
    version: &'static str,
    #[serde(rename = "lastActivity")]
    last_activity: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Health {
    version: String,
    status: String,
}

impl SidecarStore {
    pub async fn connect(config: SidecarConfig) -> Result<Self, StoreError> {
        let base_url = validate_base_url(&config.base_url)?;
        let mut headers = HeaderMap::new();
        headers.insert(PROTOCOL_HEADER, HeaderValue::from_static(PROTOCOL_VERSION));
        if let Some(token) = config.bearer_token.as_deref() {
            let authorization = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| backend("connect"))?;
            headers.insert(AUTHORIZATION, authorization);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .http1_only()
            .default_headers(headers)
            .build()
            .map_err(|_| backend("connect"))?;
        let store = Self {
            client,
            base_url,
            max_attempts: config.max_attempts,
            initial_backoff: config.initial_backoff,
            max_backoff: config.max_backoff,
            connect_timeout: config.connect_timeout,
            request_timeout: config.request_timeout,
        };
        let reply = store
            .get_request("connect", store.endpoint("v1/health")?)
            .await?;
        if reply.status != StatusCode::OK {
            return Err(backend("connect"));
        }
        let health: Health = serde_json::from_slice(&reply.body).map_err(|_| backend("connect"))?;
        if health.version != PROTOCOL_VERSION || health.status != "ok" {
            return Err(backend("connect"));
        }
        Ok(store)
    }

    fn endpoint(&self, relative: &str) -> Result<Url, StoreError> {
        self.base_url.join(relative).map_err(|_| backend("connect"))
    }

    fn route_endpoint(&self, key: &RouteKey, suffix: &str) -> Result<Url, StoreError> {
        self.endpoint(&format!(
            "v1/routes/{}{suffix}",
            encode_segment(key.as_str())
        ))
    }

    async fn get_request(
        &self,
        operation: &'static str,
        url: Url,
    ) -> Result<HttpReply, StoreError> {
        for attempt in 0..self.max_attempts {
            let response = match self.client.get(url.clone()).send().await {
                Ok(response) => response,
                Err(_) if attempt + 1 < self.max_attempts => {
                    self.sleep_before_retry(attempt).await;
                    continue;
                }
                Err(_) => return Err(backend(operation)),
            };
            if !headers_within_limits(response.headers())
                || !has_exact_protocol_header(response.headers())
            {
                return Err(backend(operation));
            }
            let status = response.status();
            if status.is_server_error() {
                if attempt + 1 < self.max_attempts {
                    self.sleep_before_retry(attempt).await;
                    continue;
                }
                return Err(backend(operation));
            }
            if status != StatusCode::OK {
                return Ok(HttpReply {
                    status,
                    body: Vec::new(),
                    last_activity: None,
                });
            }
            if !has_exact_content_type(response.headers(), "application/json") {
                return Err(backend(operation));
            }
            match bounded_body(response).await {
                Ok(body) => {
                    return Ok(HttpReply {
                        status,
                        body,
                        last_activity: None,
                    })
                }
                Err(BodyReadError::TooLarge) if operation == "snapshot" => {
                    return Err(corrupt("snapshot", "<response>"));
                }
                Err(BodyReadError::TooLarge) => return Err(backend(operation)),
                Err(BodyReadError::Transport) if attempt + 1 < self.max_attempts => {
                    self.sleep_before_retry(attempt).await;
                }
                Err(BodyReadError::Transport) => return Err(backend(operation)),
            }
        }
        Err(backend(operation))
    }

    async fn mutation_request<T, F>(
        &self,
        operation: &'static str,
        method: Method,
        url: Url,
        acknowledgment: MutationAcknowledgment,
        mut make_attempt: F,
    ) -> Result<(HttpReply, T), StoreError>
    where
        F: FnMut() -> Result<(Option<Vec<u8>>, T), StoreError>,
    {
        for attempt in 0..self.max_attempts {
            let (body, value) = make_attempt()?;
            if body
                .as_ref()
                .is_some_and(|body| body.len() > MAX_MUTATION_REQUEST_BYTES)
            {
                return Err(backend(operation));
            }
            let mut request = self.client.request(method.clone(), url.clone());
            if let Some(body) = body {
                request = request.header(CONTENT_TYPE, "application/json").body(body);
            }
            let response = request.send().await.map_err(|_| indeterminate(operation))?;
            if !headers_within_limits(response.headers())
                || !has_exact_protocol_header(response.headers())
            {
                return Err(indeterminate(operation));
            }
            let status = response.status();
            if status.is_server_error() {
                if attempt + 1 < self.max_attempts {
                    self.sleep_before_retry(attempt).await;
                    continue;
                }
                return Err(backend(operation));
            }
            if status.is_client_error()
                && !(matches!(acknowledgment, MutationAcknowledgment::Delete)
                    && status == StatusCode::NOT_FOUND)
            {
                return Err(backend(operation));
            }
            let mut last_activity = None;
            let body = match acknowledgment {
                MutationAcknowledgment::Put if status == StatusCode::NO_CONTENT => {
                    if response.headers().contains_key(CONTENT_TYPE)
                        || has_nonempty_or_ambiguous_body_framing(response.headers())
                    {
                        return Err(indeterminate(operation));
                    }
                    last_activity = Some(
                        single_header(response.headers(), LAST_ACTIVITY_HEADER)
                            .and_then(|value| parse_timestamp(value).ok())
                            .ok_or_else(|| indeterminate(operation))?,
                    );
                    read_mutation_body(response, operation).await?
                }
                MutationAcknowledgment::Empty if status == StatusCode::NO_CONTENT => {
                    if response.headers().contains_key(CONTENT_TYPE)
                        || has_nonempty_or_ambiguous_body_framing(response.headers())
                    {
                        return Err(indeterminate(operation));
                    }
                    read_mutation_body(response, operation).await?
                }
                MutationAcknowledgment::Delete if status == StatusCode::OK => {
                    if !has_exact_content_type(response.headers(), "application/json") {
                        return Err(indeterminate(operation));
                    }
                    read_mutation_body(response, operation).await?
                }
                MutationAcknowledgment::Delete if status == StatusCode::NOT_FOUND => {
                    if response.headers().contains_key(CONTENT_TYPE)
                        || has_nonempty_or_ambiguous_body_framing(response.headers())
                    {
                        return Err(indeterminate(operation));
                    }
                    let body = read_mutation_body(response, operation).await?;
                    if !body.is_empty() {
                        return Err(indeterminate(operation));
                    }
                    body
                }
                MutationAcknowledgment::Put
                | MutationAcknowledgment::Empty
                | MutationAcknowledgment::Delete => {
                    return Err(indeterminate(operation));
                }
            };
            if matches!(
                acknowledgment,
                MutationAcknowledgment::Put | MutationAcknowledgment::Empty
            ) && !body.is_empty()
            {
                return Err(indeterminate(operation));
            }
            return Ok((
                HttpReply {
                    status,
                    body,
                    last_activity,
                },
                value,
            ));
        }
        Err(backend(operation))
    }

    async fn sleep_before_retry(&self, attempt: usize) {
        let multiplier = 2_u32.saturating_pow(attempt as u32);
        let delay = self
            .initial_backoff
            .saturating_mul(multiplier)
            .min(self.max_backoff);
        tokio::time::sleep(delay).await;
    }

    async fn put_envelope(
        &self,
        error_operation: &'static str,
        protocol_operation: &'static str,
        key: &RouteKey,
        data: &RouteData,
        activity_floor: Option<&ActivityFloor>,
    ) -> Result<RouteData, StoreError> {
        let route = encode_route(error_operation, data)?;
        let mut sent_floor = data.last_activity;
        let (reply, ()) = self
            .mutation_request(
                error_operation,
                Method::PUT,
                self.route_endpoint(key, "")?,
                MutationAcknowledgment::Put,
                move || {
                    let current_floor = activity_floor.and_then(ActivityFloor::current);
                    if let Some(floor) = current_floor {
                        sent_floor = sent_floor.max(floor);
                    }
                    let body = serde_json::to_vec(&PutEnvelope {
                        version: PROTOCOL_VERSION,
                        operation: protocol_operation,
                        route: route.clone(),
                        activity_floor: chp_timestamp(sent_floor),
                    })
                    .map_err(|_| corrupt(error_operation, "<outgoing>"))?;
                    Ok((Some(body), ()))
                },
            )
            .await?;
        let mut committed = data.clone();
        committed.last_activity = reply
            .last_activity
            .ok_or_else(|| indeterminate(error_operation))?;
        Ok(committed)
    }
}

#[async_trait]
impl Store for SidecarStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        let reply = self
            .get_request("snapshot", self.endpoint("v1/routes")?)
            .await?;
        if reply.status != StatusCode::OK {
            return Err(backend("snapshot"));
        }
        decode_snapshot(&reply.body)
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let data = RouteData {
            target,
            last_activity: Utc::now(),
            extra,
        };
        self.put_envelope("add", "add", &key, &data, Some(&activity_floor))
            .await
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.put_envelope("put", "put", &key, &data, None)
            .await
            .map(|_| ())
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        self.put_envelope(
            "put",
            "put_preserving_activity",
            &key,
            &data,
            Some(&activity_floor),
        )
        .await
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        let body = serde_json::to_vec(&ActivityEnvelope {
            version: PROTOCOL_VERSION,
            last_activity: chp_timestamp(at),
        })
        .map_err(|_| corrupt("update_activity", "<outgoing>"))?;
        self.mutation_request(
            "update_activity",
            Method::PATCH,
            self.route_endpoint(key, "/activity")?,
            MutationAcknowledgment::Empty,
            || Ok((Some(body.clone()), ())),
        )
        .await
        .map(|_| ())
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        let (reply, ()) = self
            .mutation_request(
                "delete",
                Method::DELETE,
                self.route_endpoint(key, "")?,
                MutationAcknowledgment::Delete,
                || Ok((None, ())),
            )
            .await?;
        match reply.status {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => decode_route(&reply.body)
                .map(Some)
                .map_err(|_| indeterminate("delete")),
            _ => Err(indeterminate("delete")),
        }
    }
}

fn validate_base_url(raw: &str) -> Result<Url, StoreError> {
    let mut url = Url::parse(raw).map_err(|_| backend("connect"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(backend("connect"));
    }
    url.set_path("/");
    Ok(url)
}

fn encode_segment(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn hex_digit(nibble: u8) -> char {
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + nibble - 10
    })
}

fn decode_key(raw: &str) -> Result<RouteKey, StoreError> {
    if !raw.starts_with('/') {
        return Err(corrupt("snapshot", "<route-key>"));
    }
    let parse_input = if raw.len() > 1 && raw.ends_with('/') {
        format!("{raw}/")
    } else {
        raw.to_owned()
    };
    RouteKey::parse(&parse_input).map_err(|_| corrupt("snapshot", "<route-key>"))
}

fn chp_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_timestamp(raw: &str) -> Result<DateTime<Utc>, ()> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map_err(|_| ())?
        .with_timezone(&Utc);
    if chp_timestamp(parsed) == raw {
        Ok(parsed)
    } else {
        Err(())
    }
}

fn encode_route(operation: &'static str, data: &RouteData) -> Result<Value, StoreError> {
    if data.extra.contains_key("target") || data.extra.contains_key("last_activity") {
        return Err(corrupt(operation, "<outgoing>"));
    }
    let mut object = data.extra.clone();
    object.insert("target".to_owned(), Value::String(data.target.clone()));
    object.insert(
        "last_activity".to_owned(),
        Value::String(chp_timestamp(data.last_activity)),
    );
    Ok(Value::Object(object))
}

fn has_exact_protocol_header(headers: &HeaderMap) -> bool {
    single_header(headers, PROTOCOL_HEADER) == Some(PROTOCOL_VERSION)
}

fn has_exact_content_type(headers: &HeaderMap, expected: &str) -> bool {
    single_header(headers, CONTENT_TYPE.as_str()) == Some(expected)
}

fn has_nonempty_or_ambiguous_body_framing(headers: &HeaderMap) -> bool {
    if headers.contains_key(TRANSFER_ENCODING) {
        return true;
    }
    let mut lengths = headers.get_all(CONTENT_LENGTH).iter();
    match (lengths.next(), lengths.next()) {
        (None, None) => false,
        (Some(length), None) => length.as_bytes() != b"0",
        (None, Some(_)) | (Some(_), Some(_)) => true,
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_none() {
        Some(value)
    } else {
        None
    }
}

fn headers_within_limits(headers: &HeaderMap) -> bool {
    if headers.len() > MAX_RESPONSE_HEADER_COUNT {
        return false;
    }
    headers
        .iter()
        .try_fold(0_usize, |total, (name, value)| {
            total
                .checked_add(name.as_str().len())?
                .checked_add(value.as_bytes().len())
        })
        .is_some_and(|total| total <= MAX_RESPONSE_HEADER_BYTES)
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(BodyReadError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BodyReadError::Transport)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(BodyReadError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_mutation_body(
    response: reqwest::Response,
    operation: &'static str,
) -> Result<Vec<u8>, StoreError> {
    bounded_body(response)
        .await
        .map_err(|_| indeterminate(operation))
}

struct StrictRouteData(RouteData);

impl<'de> Deserialize<'de> for StrictRouteData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RouteDataVisitor)
    }
}

struct RouteDataVisitor;

impl<'de> Visitor<'de> for RouteDataVisitor {
    type Value = StrictRouteData;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a route data object with unique fields")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut target = None;
        let mut last_activity = None;
        let mut extra = Map::new();
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "target" => {
                    if target.is_some() {
                        return Err(de::Error::duplicate_field("target"));
                    }
                    target = Some(map.next_value::<String>()?);
                }
                "last_activity" => {
                    if last_activity.is_some() {
                        return Err(de::Error::duplicate_field("last_activity"));
                    }
                    let raw = map.next_value::<String>()?;
                    last_activity = Some(
                        parse_timestamp(&raw)
                            .map_err(|_| de::Error::custom("noncanonical timestamp"))?,
                    );
                }
                _ => {
                    if extra.contains_key(&field) {
                        return Err(de::Error::custom("duplicate metadata field"));
                    }
                    extra.insert(field, map.next_value::<Value>()?);
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

struct StrictSnapshot(BTreeMap<RouteKey, RouteData>);

impl<'de> Deserialize<'de> for StrictSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SnapshotVisitor(PhantomData))
    }
}

struct SnapshotVisitor(PhantomData<()>);

impl<'de> Visitor<'de> for SnapshotVisitor {
    type Value = StrictSnapshot;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a snapshot object with unique normalized route keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut routes = BTreeMap::new();
        while let Some(raw_key) = map.next_key::<String>()? {
            let key = decode_key(&raw_key).map_err(|_| de::Error::custom("invalid route key"))?;
            let data = map.next_value::<StrictRouteData>()?.0;
            if routes.insert(key, data).is_some() {
                return Err(de::Error::custom("duplicate route key"));
            }
        }
        Ok(StrictSnapshot(routes))
    }
}

fn decode_snapshot(body: &[u8]) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
    serde_json::from_slice::<StrictSnapshot>(body)
        .map(|snapshot| snapshot.0)
        .map_err(|_| corrupt("snapshot", "<response>"))
}

fn decode_route(body: &[u8]) -> Result<RouteData, ()> {
    serde_json::from_slice::<StrictRouteData>(body)
        .map(|route| route.0)
        .map_err(|_| ())
}

fn backend(operation: &'static str) -> StoreError {
    StoreError::Backend { operation }
}

fn indeterminate(operation: &'static str) -> StoreError {
    StoreError::Indeterminate { operation }
}

fn corrupt(operation: &'static str, key: &str) -> StoreError {
    StoreError::CorruptData {
        operation,
        key: key.to_owned(),
    }
}
