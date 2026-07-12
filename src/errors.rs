//! CHP-compatible proxy error rendering.

use std::path::PathBuf;

use bytes::Bytes;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};
use pingora::http::ResponseHeader;
use pingora::proxy::Session;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::upstream::Target;

/// Classification used to select the public HTTP status without exposing an
/// internal error or target URL to clients and logs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyErrorClass {
    RouteMiss,
    UnavailableUpstream,
    Internal,
}

impl ProxyErrorClass {
    pub fn status(self) -> StatusCode {
        match self {
            Self::RouteMiss => StatusCode::NOT_FOUND,
            Self::UnavailableUpstream => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

struct RenderedError {
    body: Bytes,
    content_type: Option<HeaderValue>,
    content_encoding: Option<HeaderValue>,
}

/// Renders custom-target, file, and reason-phrase proxy errors in CHP order.
#[derive(Clone)]
pub struct ProxyErrorRenderer {
    client: reqwest::Client,
    error_target: Option<Url>,
    error_path: Option<PathBuf>,
}

impl ProxyErrorRenderer {
    pub fn new(error_target: Option<Url>, error_path: Option<PathBuf>) -> Self {
        Self {
            client: reqwest::Client::new(),
            error_target,
            error_path,
        }
    }

    pub async fn respond(
        &self,
        session: &mut Session,
        status: StatusCode,
        original_uri: &str,
    ) -> pingora::Result<()> {
        let rendered = self.render(status, original_uri).await;
        let mut response = ResponseHeader::build(status.as_u16(), Some(3))?;
        response.set_content_length(rendered.body.len())?;
        if let Some(value) = rendered.content_type {
            response.insert_header(CONTENT_TYPE, value)?;
        }
        if let Some(value) = rendered.content_encoding {
            response.insert_header(CONTENT_ENCODING, value)?;
        }
        session
            .as_downstream_mut()
            .write_error_response(response, rendered.body)
            .await
    }

    async fn render(&self, status: StatusCode, original_uri: &str) -> RenderedError {
        if let Some(target) = &self.error_target {
            if let Some(rendered) = self.custom_error(target, status, original_uri).await {
                return rendered;
            }
            return reason_phrase(status);
        }

        if let Some(path) = &self.error_path {
            for filename in [format!("{}.html", status.as_u16()), "error.html".to_owned()] {
                if let Ok(body) = std::fs::read(path.join(filename)) {
                    return RenderedError {
                        body: Bytes::from(body),
                        content_type: Some(HeaderValue::from_static("text/html")),
                        content_encoding: None,
                    };
                }
            }
        }
        reason_phrase(status)
    }

    async fn custom_error(
        &self,
        target: &Url,
        status: StatusCode,
        original_uri: &str,
    ) -> Option<RenderedError> {
        let mut url = target.clone();
        let mut path = url.path().to_owned();
        path.push_str(&status.as_u16().to_string());
        url.set_path(&path);
        let retained: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(name, _)| name != "url")
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        {
            let mut query = url.query_pairs_mut();
            query.extend_pairs(retained);
            query.append_pair("url", original_uri);
        }

        if matches!(url.scheme(), "http+unix" | "unix+http") {
            return unix_custom_error(&url).await;
        }
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let response = self.client.get(url).send().await.ok()?;
        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        let content_encoding = response.headers().get(CONTENT_ENCODING).cloned();
        let body = response.bytes().await.ok()?;
        Some(RenderedError {
            body,
            content_type,
            content_encoding,
        })
    }
}

#[cfg(unix)]
async fn unix_custom_error(url: &Url) -> Option<RenderedError> {
    let target = Target::parse(url).ok()?;
    let socket_path = target.unix_path()?;
    let mut stream = tokio::net::UnixStream::connect(socket_path).await.ok()?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        target.path()
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.ok()?;
    parse_http_response(&response)
}

#[cfg(not(unix))]
async fn unix_custom_error(_url: &Url) -> Option<RenderedError> {
    None
}

fn parse_http_response(response: &[u8]) -> Option<RenderedError> {
    let header_end = response.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&response[..header_end]).ok()?;
    let mut content_type = None;
    let mut content_encoding = None;
    let mut chunked = false;
    for line in headers.lines().skip(1) {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-type") {
            content_type = HeaderValue::from_bytes(value.as_bytes()).ok();
        } else if name.eq_ignore_ascii_case("content-encoding") {
            content_encoding = HeaderValue::from_bytes(value.as_bytes()).ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
    let raw_body = &response[header_end + 4..];
    let body = if chunked {
        decode_chunked(raw_body)?
    } else {
        raw_body.to_vec()
    };
    Some(RenderedError {
        body: Bytes::from(body),
        content_type,
        content_encoding,
    })
}

fn decode_chunked(mut input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let line_end = input.windows(2).position(|bytes| bytes == b"\r\n")?;
        let size = std::str::from_utf8(&input[..line_end])
            .ok()?
            .split(';')
            .next()
            .and_then(|value| usize::from_str_radix(value.trim(), 16).ok())?;
        input = &input[line_end + 2..];
        if size == 0 {
            return Some(output);
        }
        let chunk = input.get(..size)?;
        output.extend_from_slice(chunk);
        input = input.get(size + 2..)?;
    }
}

fn reason_phrase(status: StatusCode) -> RenderedError {
    RenderedError {
        body: Bytes::copy_from_slice(
            status
                .canonical_reason()
                .unwrap_or("Unknown Error")
                .as_bytes(),
        ),
        content_type: None,
        content_encoding: None,
    }
}
