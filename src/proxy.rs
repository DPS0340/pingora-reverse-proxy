use bytes::Bytes;

use regex::Regex;

use async_trait::async_trait;

use http::{StatusCode, Uri};
use log::info;
use once_cell::sync::Lazy;
use pingora::http::ResponseHeader;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use serde_json::Value;

use dashmap::DashMap;

use redis::*;

use crate::{
    redis_utils::init_redis_connection,
    utils::{log_and_return_err, parse_prefix},
};

pub struct DynamicGateway {}

// TODO: Cache context using hashmap
pub struct UpstreamResCtx {
    buffer: Vec<u8>,
    content_type: Option<String>,
}

pub struct ProxyCtx {
    map: DashMap<String, UpstreamResCtx>,
}

static FORWARD_PATH_HEADER: &str = "X-Forwarded-Path";
static PORT_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r":\d+").unwrap());
static PROTOCOL_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(https?|wss?)://").unwrap());

#[async_trait]
impl ProxyHttp for DynamicGateway {
    type CTX = ProxyCtx;
    fn new_ctx(&self) -> Self::CTX {
        ProxyCtx {
            map: DashMap::new(),
        }
    }

    async fn request_filter(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let uri = &session.req_header().uri;

        let prefix = parse_prefix(uri);

        if let Err(e) = prefix {
            return e.into_err();
        }

        let prefix = prefix.unwrap().to_string();

        let raw_result: Option<String> =
            match init_redis_connection()?.hget("configurable-proxy-redis-storage", &prefix) {
                Ok(res) => res,
                Err(e) => {
                    return log_and_return_err(Err(pingora::Error::explain(
                        pingora::ErrorType::HTTPStatus(400),
                        format!("configurable-proxy-redis-storage lookup failed: {}", e),
                    )))
                }
            };

        let result = raw_result.ok_or_else(|| {
            log_and_return_err(Result::<Box<()>, _>::Err(pingora::Error::explain(
                pingora::ErrorType::HTTPStatus(400),
                format!("Upstream peer which matches prefix not found: {}", prefix),
            )))
            .unwrap_err()
        })?;

        let value: Value = serde_json::from_str(&result).unwrap();
        let mut addr = value["target"].as_str().unwrap().to_string();
        let addr_ = &addr.clone();

        let mut sni: bool = false;

        if PROTOCOL_REGEX.is_match(addr_) {
            let protocol = PROTOCOL_REGEX.find(addr_).unwrap().as_str();
            info!("{}", protocol);
            addr = addr.replace(protocol, "");
            let port = match protocol {
                "http://" => {
                    sni = false;
                    Some("80")
                }
                "https://" => {
                    sni = true;
                    Some("443")
                }
                // TODO: handle ws / wss
                _ => None,
            };
            if port.is_some() && !PORT_REGEX.is_match(&addr) {
                let p = port.unwrap();
                addr = format!("{}:{}", addr, p);
            }
        }

        let rewrited_uri_str = uri.to_string().replacen(prefix.as_str(), "", 1);

        let original_uri = &session.req_header().uri;

        info!("{}", uri.to_string());
        info!("{}", rewrited_uri_str);
        info!("{:?}", original_uri);

        let rewrited_uri = uri
            .to_string()
            .replacen(format!("{}/", prefix.as_str()).as_str(), "/", 1)
            .replacen(prefix.as_str(), "/", 1)
            .parse::<Uri>()
            .unwrap();

        session.req_header_mut().set_uri(rewrited_uri);

        let addr_without_port = PORT_REGEX.replace_all(addr.as_str(), "").to_string();

        let req_header = session.req_header_mut();

        let _ = req_header.insert_header(
            "X-Forwarded-Host",
            req_header.headers.clone()["Host"].to_str().unwrap_or(""),
        );

        let _ = req_header.insert_header("Host", addr_);

        let _ = req_header.insert_header("Accept-Encoding", "identity");

        let _ = session
            .req_header_mut()
            .insert_header("Host", &addr_without_port);

        let _ = session
            .req_header_mut()
            .append_header(FORWARD_PATH_HEADER, &prefix);

        info!("prefix: {prefix}");

        info!("Connecting to {addr:?}");

        let peer = Box::new(HttpPeer::new(addr.as_str(), sni, addr_without_port));
        Ok(peer)
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        // Remove content-length because the size of the new body is unknown
        upstream_response.remove_header("Content-Length");
        upstream_response
            .insert_header("Transfer-Encoding", "chunked")
            .unwrap();

        let uri = &session.req_header().uri;

        let prefix = parse_prefix(uri);

        if let Err(e) = prefix {
            return e.into_err();
        }

        let prefix = prefix.unwrap().to_string();

        let mut e = match ctx.map.get_mut(&prefix) {
            Some(e) => e,
            None => {
                ctx.map.insert(
                    prefix.clone(),
                    UpstreamResCtx {
                        buffer: vec![],
                        content_type: None,
                    },
                );
                ctx.map.get_mut(&prefix).unwrap()
            }
        };

        if let Some(t) = upstream_response.headers.get("Content-Type") {
            e.content_type = Some(t.to_str().unwrap().to_string());
        } else {
            e.content_type = None;
        }

        let req_header = session.req_header_mut();

        if !req_header.headers.contains_key(FORWARD_PATH_HEADER) {
            info!("!headers.contains_key(FORWARD_PATH_HEADER)");
            return Ok(());
        }

        let prefix = req_header.headers[FORWARD_PATH_HEADER].to_str().unwrap();

        let upstream_host = req_header.headers["Host"].to_str().unwrap();
        let status = upstream_response.status;

        if [StatusCode::MOVED_PERMANENTLY, StatusCode::FOUND].contains(&status) {
            info!("301 or 302");
            let host = req_header.headers["X-Forwarded-Host"].to_str().unwrap();
            req_header.uri.host().unwrap_or("");
            let location = upstream_response
                .headers
                .get("Location")
                .map(|e| e.to_str().unwrap_or(""))
                .unwrap_or("");

            info!("upstream_host: {}", upstream_host);
            info!("host: {}", host);
            info!("location: {}", location);
            info!(
                "replaced location: {}",
                location.replace(upstream_host, format!("{}/{}", host, prefix).as_str())
            );

            let _ = upstream_response.insert_header(
                "Location",
                location.replace(upstream_host, format!("{}/{}", host, prefix).as_str()),
            );
        }
        // if (res.statusCode == 301 || res.statusCode == 302) {
        // 	res.setHeader('Location', `${data.proxyUrl}${res.getHeader('Location').toString().replace(serviceAccessUrlSuffix + '/', '')}`)
        // }
        Ok(())
    }

    fn response_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<std::time::Duration>>
    where
        Self::CTX: Send + Sync,
    {
        let uri = &session.req_header().uri;

        let prefix = parse_prefix(uri);

        if let Err(e) = prefix {
            return e.into_err();
        }

        let prefix = prefix.unwrap().to_string();

        info!("prefix: {}", prefix);

        let mut e = match ctx.map.get_mut(&prefix) {
            Some(e) => e,
            None => {
                ctx.map.insert(
                    prefix.clone(),
                    UpstreamResCtx {
                        buffer: vec![],
                        content_type: None,
                    },
                );
                ctx.map.get_mut(&prefix).unwrap()
            }
        };

        // buffer the data
        if let Some(b) = body {
            e.buffer.extend(&b[..]);
            // drop the body
            b.clear();
        }

        if end_of_stream {
            let response_body = String::from_utf8_lossy(e.buffer.as_slice());

            info!("response_body: {}", response_body);

            let req_header = session.req_header_mut();
            let _prefix = req_header.headers[FORWARD_PATH_HEADER].to_str().unwrap();

            let replaced = response_body
                .replace(r#"=/"#, format!(r#"={}/"#, _prefix).as_str())
                .replace(r#""/"#, format!(r#""{}/"#, _prefix).as_str());

            let content_type = e.content_type.as_ref();

            if ["text/", "application/"]
                .iter()
                .any(|&e| content_type.map(|c| c.contains(e)).unwrap_or(false))
            {
                *body = Some(Bytes::copy_from_slice(replaced.as_bytes()));
            } else {
                *body = Some(Bytes::copy_from_slice(e.buffer.as_slice()));
            }

            ctx.map.remove(&prefix);
        }
        Ok(None)
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) {
        let response_code = session
            .response_written()
            .map_or(0, |resp| resp.status.as_u16());
        info!(
            "{} response code: {response_code}",
            self.request_summary(session, ctx)
        );
    }
}
