use bytes::Bytes;

use async_trait::async_trait;

use http::Uri;
use itertools::Itertools;
use log::info;
use once_cell::sync::Lazy;
use pingora::http::ResponseHeader;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use serde_json::Value;

use redis::*;

use crate::{redis_utils::init_redis_connection, utils::log_and_return_err};

pub struct DynamicGateway {}

pub struct ProxyCtx {
    buffer: Vec<u8>,
}

static FORWARD_PATH_HEADER: &str = "X-Forwarded-Path";

#[async_trait]
impl ProxyHttp for DynamicGateway {
    type CTX = ProxyCtx;
    fn new_ctx(&self) -> Self::CTX {
        ProxyCtx { buffer: vec![] }
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
        let path = uri.path();
        let mut prefixes = path
            .split("/")
            .collect_vec()
            .iter()
            .enumerate()
            .filter(|(i, _)| (*i as i32) > 0)
            .map(|(_, &e)| e)
            .collect_vec();

        info!("{:?}", prefixes);

        if prefixes.len() < 2 {
            return log_and_return_err(Err(pingora::Error::explain(
                pingora::ErrorType::HTTPStatus(400),
                format!("Prefixes too short: {:?}", prefixes),
            )));
        }

        let prefix = format!("/{}/{}", prefixes[0], prefixes[1]);

        for _ in 0..2 {
            prefixes.remove(0);
        }

        let rewrited_uri = uri
            .to_string()
            .replacen(prefix.as_str(), "", 1)
            .parse::<Uri>()
            .unwrap();

        session.req_header_mut().set_uri(rewrited_uri);

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

        let result = raw_result.ok_or(
            log_and_return_err(Result::<Box<()>, _>::Err(pingora::Error::explain(
                pingora::ErrorType::HTTPStatus(400),
                format!("Upstream peer which matches prefix not found: {}", prefix),
            )))
            .unwrap_err(),
        )?;

        let value: Value = serde_json::from_str(&result).unwrap();
        let addr = value["target"].as_str().unwrap();

        let _ = session
            .req_header_mut()
            .append_header("X-Forwarded-Path", &prefix);

        info!("Connecting to {addr:?}");

        let peer = Box::new(HttpPeer::new(addr, true, prefix));
        Ok(peer)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
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
        // buffer the data
        if let Some(b) = body {
            ctx.buffer.extend(&b[..]);
            // drop the body
            b.clear();
        }

        if end_of_stream {
            let response_body = String::from_utf8(ctx.buffer.clone()).ok();

            if response_body.is_none() {
                return Ok(None);
            }

            let headers = &session.req_header().headers;
            println!("{}", response_body.unwrap());

            if headers.contains_key(FORWARD_PATH_HEADER) {
                let _prefix = headers[FORWARD_PATH_HEADER].to_str().ok();
            }
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
