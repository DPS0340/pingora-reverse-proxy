//! CHP-compatible proxy error rendering.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};
use pingora::http::ResponseHeader;
use pingora::proxy::Session;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::upstream::Target;

const ERROR_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const ERROR_READ_TIMEOUT: Duration = Duration::from_millis(500);
const ERROR_TOTAL_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ERROR_HEADER_BYTES: usize = 16 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;
const MAX_UNIX_WIRE_BYTES: usize = MAX_ERROR_HEADER_BYTES + MAX_ERROR_BODY_BYTES + 64 * 1024;

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
        Self::with_tls_verification(error_target, error_path, true)
    }

    /// Build a renderer using the proxy's configured upstream certificate policy.
    pub fn with_tls_verification(
        error_target: Option<Url>,
        error_path: Option<PathBuf>,
        verify_tls: bool,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(ERROR_CONNECT_TIMEOUT)
                .read_timeout(ERROR_READ_TIMEOUT)
                .timeout(ERROR_TOTAL_TIMEOUT)
                .danger_accept_invalid_certs(!verify_tls)
                .build()
                .expect("fixed custom error HTTP client configuration is valid"),
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
                if let Some(body) = read_bounded_file(path.clone(), filename).await {
                    return RenderedError {
                        body,
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
        let url = custom_error_url(target, status, original_uri)?;

        if matches!(url.scheme(), "http+unix" | "unix+http") {
            return unix_custom_error(&url).await;
        }
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let mut response = self.client.get(url).send().await.ok()?;
        let header_bytes = response
            .headers()
            .iter()
            .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
            .sum::<usize>();
        if header_bytes > MAX_ERROR_HEADER_BYTES
            || response
                .content_length()
                .is_some_and(|length| length > MAX_ERROR_BODY_BYTES as u64)
        {
            return None;
        }
        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        let content_encoding = response.headers().get(CONTENT_ENCODING).cloned();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len().saturating_add(chunk.len()) > MAX_ERROR_BODY_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        Some(RenderedError {
            body: Bytes::from(body),
            content_type,
            content_encoding,
        })
    }
}

#[cfg(unix)]
async fn unix_custom_error(url: &Url) -> Option<RenderedError> {
    tokio::time::timeout(ERROR_TOTAL_TIMEOUT, unix_custom_error_inner(url))
        .await
        .ok()?
}

#[cfg(unix)]
async fn unix_custom_error_inner(url: &Url) -> Option<RenderedError> {
    let target = Target::parse(url).ok()?;
    let socket_path = target.unix_path()?;
    let mut stream = tokio::time::timeout(
        ERROR_CONNECT_TIMEOUT,
        tokio::net::UnixStream::connect(socket_path),
    )
    .await
    .ok()?
    .ok()?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        target.path()
    );
    tokio::time::timeout(ERROR_READ_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .ok()?
        .ok()?;
    let mut response = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = tokio::time::timeout(ERROR_READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if count == 0 {
            return parse_http_response(&response, true);
        }
        if response.len().saturating_add(count) > MAX_UNIX_WIRE_BYTES {
            return None;
        }
        response.extend_from_slice(&chunk[..count]);
        if !response.windows(4).any(|bytes| bytes == b"\r\n\r\n")
            && response.len() > MAX_ERROR_HEADER_BYTES
        {
            return None;
        }
        if let Some(rendered) = parse_http_response(&response, false) {
            return Some(rendered);
        }
    }
}

#[cfg(not(unix))]
async fn unix_custom_error(_url: &Url) -> Option<RenderedError> {
    None
}

fn parse_http_response(response: &[u8], eof: bool) -> Option<RenderedError> {
    let header_end = response.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    if header_end > MAX_ERROR_HEADER_BYTES {
        return None;
    }
    let headers = std::str::from_utf8(&response[..header_end]).ok()?;
    let mut content_type = None;
    let mut content_encoding = None;
    let mut chunked = false;
    let mut content_length = None;
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
        } else if name.eq_ignore_ascii_case("content-length") {
            let length = value.parse::<usize>().ok()?;
            if length > MAX_ERROR_BODY_BYTES {
                return None;
            }
            content_length = Some(length);
        }
    }
    let raw_body = &response[header_end + 4..];
    let body = if chunked {
        decode_chunked(raw_body)?
    } else if let Some(length) = content_length {
        raw_body.get(..length)?.to_vec()
    } else if eof && raw_body.len() <= MAX_ERROR_BODY_BYTES {
        raw_body.to_vec()
    } else {
        return None;
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
            return input.starts_with(b"\r\n").then_some(output);
        }
        let chunk = input.get(..size)?;
        output.extend_from_slice(chunk);
        if output.len() > MAX_ERROR_BODY_BYTES {
            return None;
        }
        input = input.get(size..)?;
        input = input.strip_prefix(b"\r\n")?;
    }
}

fn custom_error_url(target: &Url, status: StatusCode, original_uri: &str) -> Option<Url> {
    let literal = format!(
        "{}{status}?url={}",
        target.as_str(),
        encode_uri_component(original_uri),
        status = status.as_u16()
    );
    Url::parse(&literal).ok()
}

fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

async fn read_bounded_file(root: PathBuf, filename: String) -> Option<Bytes> {
    tokio::task::spawn_blocking(move || read_bounded_file_sync(&root, &filename))
        .await
        .ok()?
}

fn read_bounded_file_sync(root: &Path, filename: &str) -> Option<Bytes> {
    if Path::new(filename).components().count() != 1 {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let path = root.join(filename).canonicalize().ok()?;
    if !path.starts_with(&root) {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_ERROR_BODY_BYTES as u64 {
        return None;
    }
    let mut body = Vec::new();
    file.by_ref()
        .take(MAX_ERROR_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() <= MAX_ERROR_BODY_BYTES).then(|| Bytes::from(body))
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

#[cfg(test)]
mod tests {
    use super::encode_uri_component;

    #[test]
    fn uri_component_encoding_matches_javascript_encode_uri_component() {
        assert_eq!(
            encode_uri_component("/!~*()'% already%20한글"),
            "%2F!~*()'%25%20already%2520%ED%95%9C%EA%B8%80"
        );
    }
}
