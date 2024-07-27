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
        let results: Vec<String> =
            match init_redis_connection()?.hgetall("configurable-proxy-redis-storage") {
                Ok(res) => res,
                Err(e) => {
                    return log_and_return_err(Err(pingora::Error::explain(
                        pingora::ErrorType::HTTPStatus(400),
                        format!("configurable-proxy-redis-storage lookup failed: {}", e),
                    )))
                }
            };

        let (even, odd): (Vec<_>, Vec<_>) =
            results
                .iter()
                .enumerate()
                .partition_map(|(i, s)| match i % 2 {
                    0 => itertools::Either::Left(s),
                    _ => itertools::Either::Right(s),
                });

        let result: Vec<(_, _)> = even
            .iter()
            .zip(
                odd.iter()
                    .map(|s| serde_json::from_str(s).unwrap())
                    .collect::<Vec<Value>>(),
            )
            .collect();

        let path = session.req_header().uri.path();

        let found = result
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix.as_str()));

        if found.is_none() {
            let e = Err(pingora::Error::explain(
                pingora::ErrorType::HTTPStatus(404),
                "Upstream peer which matches prefix not found",
            ));
            info!("An error occurred: {e:?}");
            return e;
        }

        let (&prefix, value): (&&String, Value) = found.unwrap().to_owned();

        let addr = value["target"].as_str().unwrap();

        info!("connecting to {addr:?}");

        let peer = Box::new(HttpPeer::new(addr, true, prefix.to_string()));
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
