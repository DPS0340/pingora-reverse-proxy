//! Client for the versioned HTTP/JSON sidecar storage protocol.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::route::{RouteData, RouteKey};

use super::{ActivityFloor, Store, StoreError};

const PROTOCOL_HEADER: &str = "x-store-protocol";
const PROTOCOL_VERSION: &str = "v1";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_ATTEMPTS: usize = 3;
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_millis(200);
const MAX_ATTEMPTS: usize = 10;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

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
}

#[derive(Serialize)]
struct PutEnvelope {
    version: &'static str,
    operation: &'static str,
    route: Value,
    #[serde(rename = "activityFloor")]
    activity_floor: Option<String>,
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
            .request(
                "connect",
                Method::GET,
                store.endpoint("v1/health")?,
                None,
                false,
            )
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

    async fn request(
        &self,
        operation: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        mutation: bool,
    ) -> Result<HttpReply, StoreError> {
        for attempt in 0..self.max_attempts {
            let mut request = self.client.request(method.clone(), url.clone());
            if let Some(body) = body.as_ref() {
                request = request
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone());
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) if mutation => {
                    return Err(StoreError::Indeterminate { operation });
                }
                Err(_) if attempt + 1 < self.max_attempts => {
                    self.sleep_before_retry(attempt).await;
                    continue;
                }
                Err(_) => return Err(backend(operation)),
            };
            if response
                .headers()
                .get(PROTOCOL_HEADER)
                .and_then(|value| value.to_str().ok())
                != Some(PROTOCOL_VERSION)
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
            if status.is_client_error() || status.is_redirection() {
                return Ok(HttpReply {
                    status,
                    body: Vec::new(),
                });
            }
            if status == StatusCode::OK && !is_json(response.headers()) {
                return Err(backend(operation));
            }
            let body = match bounded_body(response).await {
                Ok(body) => body,
                Err(()) if mutation => {
                    return Err(StoreError::Indeterminate { operation });
                }
                Err(()) if attempt + 1 < self.max_attempts => {
                    self.sleep_before_retry(attempt).await;
                    continue;
                }
                Err(()) => return Err(backend(operation)),
            };
            return Ok(HttpReply { status, body });
        }
        Err(backend(operation))
    }

    async fn sleep_before_retry(&self, attempt: usize) {
        let multiplier = 1_u32.checked_shl(attempt as u32).unwrap_or(u32::MAX);
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
        activity_floor: Option<DateTime<Utc>>,
    ) -> Result<(), StoreError> {
        let body = serde_json::to_vec(&PutEnvelope {
            version: PROTOCOL_VERSION,
            operation: protocol_operation,
            route: encode_route(error_operation, data)?,
            activity_floor: activity_floor.map(chp_timestamp),
        })
        .map_err(|_| corrupt(error_operation, "<outgoing>"))?;
        let reply = self
            .request(
                error_operation,
                Method::PUT,
                self.route_endpoint(key, "")?,
                Some(body),
                true,
            )
            .await?;
        if reply.status == StatusCode::NO_CONTENT {
            Ok(())
        } else {
            Err(backend(error_operation))
        }
    }
}

#[async_trait]
impl Store for SidecarStore {
    async fn snapshot(&self) -> Result<BTreeMap<RouteKey, RouteData>, StoreError> {
        let reply = self
            .request(
                "snapshot",
                Method::GET,
                self.endpoint("v1/routes")?,
                None,
                false,
            )
            .await?;
        if reply.status != StatusCode::OK {
            return Err(backend("snapshot"));
        }
        let records: BTreeMap<String, RouteData> =
            serde_json::from_slice(&reply.body).map_err(|_| corrupt("snapshot", "<response>"))?;
        let mut snapshot = BTreeMap::new();
        for (raw_key, data) in records {
            let key = decode_key(&raw_key)?;
            if snapshot.insert(key, data).is_some() {
                return Err(corrupt("snapshot", "<response>"));
            }
        }
        Ok(snapshot)
    }

    async fn add(
        &self,
        key: RouteKey,
        target: String,
        extra: Map<String, Value>,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let floor = activity_floor.current();
        let now = Utc::now();
        let data = RouteData {
            target,
            last_activity: floor.map_or(now, |floor| now.max(floor)),
            extra,
        };
        self.put_envelope("add", "add", &key, &data, floor).await?;
        Ok(data)
    }

    async fn put(&self, key: RouteKey, data: RouteData) -> Result<(), StoreError> {
        self.put_envelope("put", "put", &key, &data, None).await
    }

    async fn put_preserving_activity(
        &self,
        key: RouteKey,
        mut data: RouteData,
        activity_floor: ActivityFloor,
    ) -> Result<RouteData, StoreError> {
        let floor = activity_floor.current();
        if let Some(floor) = floor {
            data.last_activity = data.last_activity.max(floor);
        }
        self.put_envelope("put", "put_preserving_activity", &key, &data, floor)
            .await?;
        Ok(data)
    }

    async fn update_activity(&self, key: &RouteKey, at: DateTime<Utc>) -> Result<(), StoreError> {
        let body = serde_json::to_vec(&ActivityEnvelope {
            version: PROTOCOL_VERSION,
            last_activity: chp_timestamp(at),
        })
        .map_err(|_| corrupt("update_activity", "<outgoing>"))?;
        let reply = self
            .request(
                "update_activity",
                Method::PATCH,
                self.route_endpoint(key, "/activity")?,
                Some(body),
                true,
            )
            .await?;
        if reply.status == StatusCode::NO_CONTENT {
            Ok(())
        } else {
            Err(backend("update_activity"))
        }
    }

    async fn delete(&self, key: &RouteKey) -> Result<Option<RouteData>, StoreError> {
        let reply = self
            .request(
                "delete",
                Method::DELETE,
                self.route_endpoint(key, "")?,
                None,
                true,
            )
            .await?;
        match reply.status {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => serde_json::from_slice(&reply.body)
                .map(Some)
                .map_err(|_| corrupt("delete", key.as_str())),
            _ => Err(backend("delete")),
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
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String is infallible");
        }
    }
    encoded
}

fn decode_key(raw: &str) -> Result<RouteKey, StoreError> {
    if !raw.starts_with('/') {
        return Err(corrupt("snapshot", raw));
    }
    let parse_input = if raw.len() > 1 && raw.ends_with('/') {
        format!("{raw}/")
    } else {
        raw.to_owned()
    };
    Ok(RouteKey::parse(&parse_input).expect("route-key normalization is infallible"))
}

fn chp_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn encode_route(operation: &'static str, data: &RouteData) -> Result<Value, StoreError> {
    let mut encoded = serde_json::to_value(data).map_err(|_| corrupt(operation, "<outgoing>"))?;
    let object = encoded
        .as_object_mut()
        .ok_or_else(|| corrupt(operation, "<outgoing>"))?;
    object.insert(
        "last_activity".to_owned(),
        Value::String(chp_timestamp(data.last_activity)),
    );
    Ok(encoded)
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        == Some("application/json")
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, ()> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn backend(operation: &'static str) -> StoreError {
    StoreError::Backend { operation }
}

fn corrupt(operation: &'static str, key: &str) -> StoreError {
    StoreError::CorruptData {
        operation,
        key: key.to_owned(),
    }
}
