//! Functional Pingora HTTP data plane.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{HeaderValue, CONTENT_LENGTH, CONTENT_TYPE, HOST, LOCATION, TRANSFER_ENCODING};
use http::{HeaderMap, Request, Response, StatusCode, Uri};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::{Error, ErrorSource, ErrorType, HttpPeer};
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use thiserror::Error as ThisError;
use url::Url;

use crate::activity::ActivityWriter;
use crate::api_server::TrafficLifecycle;
use crate::config::{AppConfig, ProxyOptions};
use crate::errors::{ErrorRendererBuildError, ProxyErrorClass, ProxyErrorRenderer};
use crate::route::RouteKey;
use crate::route_table::RouteRegistry;
use crate::store::StoreError;
use crate::upstream::{
    apply_forwarded_headers, apply_request_headers, build_upstream_uri, rewrite_location,
    ForwardedContext, Target, TargetError, TlsClientConfig, UpstreamRoute,
};

const HEALTH_PATH: &str = "/_chp_healthz";

/// Per-request state shared across Pingora's proxy phases.
#[derive(Debug)]
pub struct RequestContext {
    pub resolved_route_key: Option<RouteKey>,
    pub original_uri: Uri,
    pub original_host: Option<HeaderValue>,
    pub activity_eligible: bool,
    pub error_classification: Option<ProxyErrorClass>,
    upstream_route: Option<UpstreamRoute>,
    stream_data_seen: bool,
    activity_published: bool,
    traffic_admission: Option<Arc<TrafficLifecycle>>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            resolved_route_key: None,
            original_uri: Uri::from_static("/"),
            original_host: None,
            activity_eligible: false,
            error_classification: None,
            upstream_route: None,
            stream_data_seen: false,
            activity_published: false,
            traffic_admission: None,
        }
    }
}

impl Drop for RequestContext {
    fn drop(&mut self) {
        if let Some(traffic) = self.traffic_admission.take() {
            traffic.finish();
        }
    }
}

/// Failure while constructing the immutable proxy service.
#[derive(Debug, ThisError)]
pub enum ProxyBuildError {
    #[error("failed to install the default route")]
    DefaultRoute(#[source] StoreError),
    #[error("failed to configure custom error rendering")]
    ErrorRenderer(#[source] ErrorRendererBuildError),
}

/// CHP routing and policy implemented through Pingora's official callbacks.
pub struct ChpProxy {
    registry: Arc<RouteRegistry>,
    options: ProxyOptions,
    tls: TlsClientConfig,
    errors: ProxyErrorRenderer,
    activity: ActivityWriter,
    downstream_protocol: &'static str,
    traffic: Arc<TrafficLifecycle>,
}

impl ChpProxy {
    pub async fn from_config(
        registry: Arc<RouteRegistry>,
        config: &AppConfig,
        activity: ActivityWriter,
    ) -> Result<Self, ProxyBuildError> {
        let client_tls = config.client_tls.as_ref();
        let connection_timeout = config.proxy.timeout_ms.map(Duration::from_millis);
        let proxy_timeout = config.proxy.proxy_timeout_ms.map(Duration::from_millis);
        let tls = TlsClientConfig {
            verify_cert: config.proxy.verify_upstream_tls,
            verify_hostname: config.proxy.verify_upstream_tls,
            connection_timeout,
            total_connection_timeout: connection_timeout,
            read_timeout: proxy_timeout,
            write_timeout: proxy_timeout,
            idle_timeout: config
                .proxy
                .keep_alive_timeout_ms
                .map(Duration::from_millis),
            ca_file: client_tls.and_then(|tls| tls.ca.clone()),
            client_certificate: client_tls.and_then(|tls| tls.cert.clone()),
            client_key: client_tls.and_then(|tls| tls.key.clone()),
        };
        let errors = ProxyErrorRenderer::with_tls_policy(
            config.error_target.clone(),
            config.error_path.clone(),
            config.proxy.verify_upstream_tls,
            config.client_tls.as_ref(),
        )
        .map_err(ProxyBuildError::ErrorRenderer)?;
        if let Some(default_target) = &config.default_target {
            registry
                .add(
                    route_key("/"),
                    default_target.as_str().to_owned(),
                    Default::default(),
                )
                .await
                .map_err(ProxyBuildError::DefaultRoute)?;
        }
        Ok(Self {
            registry,
            options: config.proxy.clone(),
            tls,
            errors,
            activity,
            downstream_protocol: if config.public_tls.is_some() {
                "https"
            } else {
                "http"
            },
            traffic: Arc::new(TrafficLifecycle::new()),
        })
    }

    pub fn traffic_lifecycle(&self) -> Arc<TrafficLifecycle> {
        Arc::clone(&self.traffic)
    }

    fn internal_error(ctx: &mut RequestContext) -> Box<Error> {
        ctx.error_classification = Some(ProxyErrorClass::Internal);
        Error::new_in(ErrorType::HTTPStatus(500))
    }

    fn route_for_request(
        &self,
        session: &Session,
        ctx: &mut RequestContext,
    ) -> Result<Option<UpstreamRoute>, Box<Error>> {
        let decoded_path = percent_decode(session.req_header().uri.path())
            .ok_or_else(|| Self::internal_error(ctx))?;
        let lookup_path = if self.options.host_routing {
            let host = ctx
                .original_host
                .as_ref()
                .and_then(|value| value.to_str().ok())
                .map_or("undefined", |host| host.split(':').next().unwrap_or(""));
            format!("/{host}{decoded_path}")
        } else {
            decoded_path
        };
        if let Some(matched) = self.registry.resolve(&lookup_path) {
            let url = Url::parse(&matched.data.target).map_err(|_| Self::internal_error(ctx))?;
            let target = Target::parse(&url).map_err(|_| Self::internal_error(ctx))?;
            ctx.resolved_route_key = Some(matched.key.clone());
            return Ok(Some(UpstreamRoute::new(matched.key, target)));
        }
        Ok(None)
    }

    async fn send_health(session: &mut Session) -> pingora::Result<()> {
        let body = Bytes::from_static(br#"{"status":"OK"}"#);
        let mut response = ResponseHeader::build(200, Some(2))?;
        response.insert_header(CONTENT_TYPE, "application/json")?;
        response.set_content_length(body.len())?;
        session
            .write_response_header(Box::new(response), false)
            .await?;
        session.write_response_body(Some(body), true).await
    }

    async fn send_empty(session: &mut Session, status: StatusCode) -> pingora::Result<()> {
        let mut response = ResponseHeader::build(status.as_u16(), Some(1))?;
        response.set_content_length(0)?;
        session
            .write_response_header(Box::new(response), true)
            .await
    }

    fn observe_stream_data(&self, ctx: &mut RequestContext) {
        ctx.stream_data_seen = true;
        self.publish_activity_if_eligible(ctx);
    }

    fn publish_activity_if_eligible(&self, ctx: &mut RequestContext) {
        if !ctx.activity_eligible || ctx.activity_published {
            return;
        }
        if let Some(key) = &ctx.resolved_route_key {
            self.activity.record(key);
            ctx.activity_published = true;
        }
    }
}

#[async_trait]
impl ProxyHttp for ChpProxy {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext::default()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<bool> {
        if !self.traffic.try_admit() {
            Self::send_empty(session, StatusCode::SERVICE_UNAVAILABLE).await?;
            return Ok(true);
        }
        ctx.traffic_admission = Some(Arc::clone(&self.traffic));
        ctx.original_uri = session.req_header().uri.clone();
        ctx.original_host = session.req_header().headers.get(HOST).cloned();
        let original = ctx
            .original_uri
            .path_and_query()
            .map_or(ctx.original_uri.path(), |value| value.as_str())
            .to_owned();
        if original == HEALTH_PATH {
            Self::send_health(session).await?;
            return Ok(true);
        }

        let Some(route) = self.route_for_request(session, ctx)? else {
            ctx.error_classification = Some(ProxyErrorClass::RouteMiss);
            self.errors
                .respond(session, StatusCode::NOT_FOUND, &original)
                .await?;
            return Ok(true);
        };
        ctx.upstream_route = Some(route);
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<HttpPeer>> {
        let Some(route) = ctx.upstream_route.clone() else {
            return Err(Self::internal_error(ctx));
        };
        match route.target.http_peer(&self.tls) {
            Ok(peer) => Ok(Box::new(peer)),
            Err(error) => {
                let unavailable = matches!(
                    error,
                    TargetError::AddressResolution(_, _) | TargetError::NoResolvedAddress(_)
                );
                ctx.error_classification = Some(if unavailable {
                    ProxyErrorClass::UnavailableUpstream
                } else {
                    ProxyErrorClass::Internal
                });
                Err(if unavailable {
                    Error::new_up(ErrorType::ConnectError)
                } else {
                    Error::new_in(ErrorType::HTTPStatus(500))
                })
            }
        }
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        let Some(route) = ctx.upstream_route.clone() else {
            return Err(Self::internal_error(ctx));
        };
        let uri = build_upstream_uri(&route, &ctx.original_uri, &self.options)
            .map_err(|_| Self::internal_error(ctx))?;
        let mut headers = upstream_request.headers.clone();
        let client_address = session
            .as_downstream()
            .client_addr()
            .and_then(|address| address.as_inet())
            .map(|address| address.ip().to_string())
            .unwrap_or_default();
        let port = session
            .as_downstream()
            .server_addr()
            .and_then(|address| address.as_inet())
            .map_or(
                if self.downstream_protocol == "https" {
                    443
                } else {
                    80
                },
                |address| address.port(),
            );
        apply_forwarded_headers(
            &mut headers,
            &ForwardedContext {
                client_address: &client_address,
                port,
                protocol: self.downstream_protocol,
            },
            &self.options,
        )
        .map_err(|_| Self::internal_error(ctx))?;
        apply_request_headers(&mut headers, &route.target, &self.options)
            .map_err(|_| Self::internal_error(ctx))?;
        replace_request_headers(upstream_request, headers)
            .map_err(|_| Self::internal_error(ctx))?;
        if !session.is_body_empty() && !upstream_request.headers.contains_key(CONTENT_LENGTH) {
            upstream_request.insert_header(TRANSFER_ENCODING, "chunked")?;
        }
        upstream_request.set_uri(uri);
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        let Some(route) = ctx.upstream_route.clone() else {
            return Err(Self::internal_error(ctx));
        };
        let mut request = Request::builder()
            .uri(ctx.original_uri.clone())
            .body(())
            .map_err(|_| Self::internal_error(ctx))?;
        if let Some(host) = &ctx.original_host {
            request.headers_mut().insert(HOST, host.clone());
        }
        let mut response = Response::builder()
            .status(upstream_response.status)
            .body(())
            .map_err(|_| Self::internal_error(ctx))?;
        response
            .headers_mut()
            .clone_from(&upstream_response.headers);
        rewrite_location(&mut response, &request, &route.target, &self.options)
            .map_err(|_| Self::internal_error(ctx))?;
        match response.headers().get(LOCATION).cloned() {
            Some(location) => upstream_response.insert_header(LOCATION, location)?,
            None => {
                upstream_response.remove_header(&LOCATION);
            }
        }
        ctx.activity_eligible = upstream_response.status.as_u16() < 300;
        if ctx.stream_data_seen {
            self.publish_activity_if_eligible(ctx);
        }
        Ok(())
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        if body.as_ref().is_some_and(|body| !body.is_empty()) {
            self.observe_stream_data(ctx);
        }
        Ok(())
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Option<Duration>> {
        if body.as_ref().is_some_and(|body| !body.is_empty()) {
            self.observe_stream_data(ctx);
        }
        Ok(None)
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        error: Box<Error>,
    ) -> Box<Error> {
        ctx.error_classification = Some(ProxyErrorClass::UnavailableUpstream);
        error
    }

    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        mut error: Box<Error>,
        ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<Error> {
        if ctx.error_classification.is_none() && error.esource() == &ErrorSource::Upstream {
            ctx.error_classification = Some(ProxyErrorClass::UnavailableUpstream);
        }
        error = error.more_context(format!("Peer: {peer}"));
        error
            .retry
            .decide_reuse(client_reused && !session.as_ref().retry_buffer_truncated());
        error
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        error: &Error,
        ctx: &mut Self::CTX,
    ) -> FailToProxy {
        if let Some(response) = session.response_written() {
            return FailToProxy {
                error_code: response.status.as_u16(),
                can_reuse_downstream: false,
            };
        }
        if error.esource() == &ErrorSource::Downstream
            && matches!(
                error.etype(),
                ErrorType::ReadError | ErrorType::WriteError | ErrorType::ConnectionClosed
            )
        {
            return FailToProxy {
                error_code: 0,
                can_reuse_downstream: false,
            };
        }
        let classification = ctx.error_classification.unwrap_or_else(|| {
            if error.esource() == &ErrorSource::Upstream {
                ProxyErrorClass::UnavailableUpstream
            } else {
                ProxyErrorClass::Internal
            }
        });
        let status = classification.status();
        if session.is_upgrade_req() {
            let _ = Self::send_empty(session, status).await;
            return FailToProxy {
                error_code: status.as_u16(),
                can_reuse_downstream: false,
            };
        }
        let original = ctx
            .original_uri
            .path_and_query()
            .map_or(ctx.original_uri.path(), |value| value.as_str());
        let _ = self.errors.respond(session, status, original).await;
        FailToProxy {
            error_code: status.as_u16(),
            can_reuse_downstream: false,
        }
    }

    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX) {
        let successful = error.is_none()
            && session
                .response_written()
                .is_some_and(|response| response.status.as_u16() < 300);
        ctx.activity_eligible |= successful;
        if successful {
            self.publish_activity_if_eligible(ctx);
        }
        if let Some(traffic) = ctx.traffic_admission.take() {
            traffic.finish();
        }
    }

    fn request_summary(&self, session: &Session, _ctx: &Self::CTX) -> String {
        format!("{} request", session.req_header().method)
    }
}

fn route_key(value: &str) -> RouteKey {
    match RouteKey::parse(value) {
        Ok(key) => key,
        Err(error) => match error {},
    }
}

fn replace_request_headers(request: &mut RequestHeader, headers: HeaderMap) -> pingora::Result<()> {
    let names: Vec<_> = request.headers.keys().cloned().collect();
    for name in names {
        request.remove_header(&name);
    }
    for (name, value) in headers {
        if let Some(name) = name {
            request.append_header(name, value)?;
        }
    }
    Ok(())
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
