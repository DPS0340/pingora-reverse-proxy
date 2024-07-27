use async_trait::async_trait;

use itertools::Itertools;
use log::info;
use pingora::http::ResponseHeader;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use serde_json::Value;

use redis::*;

use crate::{redis_utils::init_redis_connection, utils::log_and_return_err};

pub struct DynamicGateway {}

#[async_trait]
impl ProxyHttp for DynamicGateway {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();
        let prefixes = path
            .split("/")
            .collect_vec()
            .iter()
            .enumerate()
            .filter(|(i, e)| (*i as i32) > 0)
            .map(|(i, &e)| e)
            .collect_vec();

        info!("{:?}", prefixes);

        if prefixes.len() < 2 {
            return log_and_return_err(Err(pingora::Error::explain(
                pingora::ErrorType::HTTPStatus(400),
                format!("Prefixes too short: {:?}", prefixes),
            )));
        }

        let prefix = format!("{}/{}", prefixes[0], prefixes[1]);

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

        info!("Connecting to {addr:?}");

        let peer = Box::new(HttpPeer::new(addr, true, prefix));
        Ok(peer)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        // replace existing header if any
        upstream_response
            .insert_header("Server", "MyGateway")
            .unwrap();
        // because we don't support h3
        upstream_response.remove_header("alt-svc");

        Ok(())
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
