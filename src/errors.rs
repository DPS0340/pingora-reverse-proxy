//! CHP-compatible proxy error rendering.

use std::fs::File;
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

use crate::config::TlsConfig;
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

/// A typed, redacted failure while configuring the custom-error HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum ErrorRendererBuildError {
    #[error("failed to read custom-error TLS {kind}")]
    ReadTls {
        kind: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid custom-error TLS CA certificate")]
    InvalidCa(#[source] reqwest::Error),
    #[error("invalid custom-error TLS client identity")]
    InvalidIdentity(#[source] reqwest::Error),
    #[error("failed to build custom-error HTTP client")]
    Client(#[source] reqwest::Error),
}

impl ProxyErrorRenderer {
    /// Build a renderer using the proxy's complete upstream TLS policy.
    pub fn with_tls_policy(
        error_target: Option<Url>,
        error_path: Option<PathBuf>,
        verify_tls: bool,
        tls: Option<&TlsConfig>,
    ) -> Result<Self, ErrorRendererBuildError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(ERROR_CONNECT_TIMEOUT)
            .read_timeout(ERROR_READ_TIMEOUT)
            .timeout(ERROR_TOTAL_TIMEOUT)
            .http1_only()
            .danger_accept_invalid_certs(!verify_tls);
        if let Some(path) = tls.and_then(|tls| tls.ca.as_ref()) {
            let pem = std::fs::read(path).map_err(|source| ErrorRendererBuildError::ReadTls {
                kind: "CA certificate",
                source,
            })?;
            let certificate =
                reqwest::Certificate::from_pem(&pem).map_err(ErrorRendererBuildError::InvalidCa)?;
            builder = builder.add_root_certificate(certificate);
        }
        if let Some(tls) = tls {
            if let (Some(certificate), Some(key)) = (&tls.cert, &tls.key) {
                let mut pem = std::fs::read(certificate).map_err(|source| {
                    ErrorRendererBuildError::ReadTls {
                        kind: "client certificate",
                        source,
                    }
                })?;
                pem.push(b'\n');
                pem.extend(std::fs::read(key).map_err(|source| {
                    ErrorRendererBuildError::ReadTls {
                        kind: "client key",
                        source,
                    }
                })?);
                let identity = reqwest::Identity::from_pem(&pem)
                    .map_err(ErrorRendererBuildError::InvalidIdentity)?;
                builder = builder.identity(identity);
            }
        }
        Ok(Self {
            client: builder.build().map_err(ErrorRendererBuildError::Client)?,
            error_target,
            error_path,
        })
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
        let header_bytes = response_wire_header_bytes(&response)?;
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
    if header_end.saturating_add(4) > MAX_ERROR_HEADER_BYTES {
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
    read_bounded_file_sync_with_hook(root, filename, || {})
}

fn read_bounded_file_sync_with_hook(
    root: &Path,
    filename: &str,
    after_root_open: impl FnOnce(),
) -> Option<Bytes> {
    if Path::new(filename).components().count() != 1 {
        return None;
    }
    let mut file = open_bounded_regular_file(root, filename, after_root_open)?;
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

#[cfg(unix)]
fn open_bounded_regular_file(
    root: &Path,
    filename: &str,
    after_root_open: impl FnOnce(),
) -> Option<File> {
    use rustix::fs::{open, openat, Mode, OFlags};

    let root = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    after_root_open();
    let child = openat(
        &root,
        filename,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(child);
    file.metadata().ok()?.is_file().then_some(file)
}

#[cfg(not(unix))]
fn open_bounded_regular_file(
    _root: &Path,
    _filename: &str,
    after_root_open: impl FnOnce(),
) -> Option<File> {
    after_root_open();
    // Secure descriptor-relative, no-follow traversal is implemented for the
    // supported macOS/Linux/Unix deployment targets. Other platforms fail
    // closed instead of falling back to a pathname check/open sequence.
    None
}

fn response_wire_header_bytes(response: &reqwest::Response) -> Option<usize> {
    let version = match response.version() {
        reqwest::Version::HTTP_09 => "HTTP/0.9",
        reqwest::Version::HTTP_10 => "HTTP/1.0",
        reqwest::Version::HTTP_11 => "HTTP/1.1",
        _ => return None,
    };
    let reason = response
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .map_or_else(
            || {
                response
                    .status()
                    .canonical_reason()
                    .unwrap_or("")
                    .as_bytes()
            },
            hyper::ext::ReasonPhrase::as_bytes,
        );
    let reason_separator = usize::from(!reason.is_empty());
    let status_line = version.len() + 1 + 3 + reason_separator + reason.len() + 2;
    let fields = response
        .headers()
        .iter()
        .try_fold(0usize, |total, (name, value)| {
            total.checked_add(name.as_str().len() + 2 + value.as_bytes().len() + 2)
        })?;
    status_line.checked_add(fields)?.checked_add(2)
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
    use std::fs;

    use super::{encode_uri_component, parse_http_response, read_bounded_file_sync_with_hook};

    #[test]
    fn uri_component_encoding_matches_javascript_encode_uri_component() {
        assert_eq!(
            encode_uri_component("/!~*()'% already%20한글"),
            "%2F!~*()'%25%20already%2520%ED%95%9C%EA%B8%80"
        );
    }

    #[test]
    fn wire_header_limit_includes_status_line_and_final_delimiter() {
        let prefix = b"HTTP/1.1 200 OK\r\nX-Fill: ";
        let suffix = b"\r\n\r\nbody";
        for (wire_header_bytes, accepted) in [
            (16 * 1024 - 1, true),
            (16 * 1024, true),
            (16 * 1024 + 1, false),
        ] {
            let mut response = prefix.to_vec();
            response.extend(vec![b'x'; wire_header_bytes - prefix.len() - 4]);
            response.extend_from_slice(suffix);
            assert_eq!(
                parse_http_response(&response, true).is_some(),
                accepted,
                "unexpected result for {wire_header_bytes} wire header bytes"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_open_defeats_a_deterministic_symlink_swap() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "outside secret").unwrap();
        fs::write(directory.path().join("404.html"), "safe").unwrap();

        let result = read_bounded_file_sync_with_hook(directory.path(), "404.html", || {
            fs::remove_file(directory.path().join("404.html")).unwrap();
            std::os::unix::fs::symlink(outside.path(), directory.path().join("404.html")).unwrap();
        });

        assert!(
            result.is_none(),
            "a swapped symlink must never disclose its target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_error_files_are_rejected_without_blocking() {
        use std::os::unix::fs::FileTypeExt as _;
        use std::process::Command;
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("404.html");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        assert!(fs::metadata(&fifo).unwrap().file_type().is_fifo());

        let started = Instant::now();
        assert!(read_bounded_file_sync_with_hook(directory.path(), "404.html", || {}).is_none());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "FIFO rejection blocked the error-rendering worker"
        );

        fs::remove_file(&fifo).unwrap();
        fs::create_dir(&fifo).unwrap();
        assert!(read_bounded_file_sync_with_hook(directory.path(), "404.html", || {}).is_none());
        fs::remove_dir(&fifo).unwrap();

        let _socket = std::os::unix::net::UnixListener::bind(&fifo).unwrap();
        assert!(read_bounded_file_sync_with_hook(directory.path(), "404.html", || {}).is_none());
    }
}
