use async_trait::async_trait;

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_load_balancing::{health_check, selection::RoundRobin, LoadBalancer};
use pingora_proxy::{ProxyHttp, Session};

use std::sync::Arc;

use redis::*;

use crate::redis_utils::init_redis_connection;

pub struct LB(Arc<LoadBalancer<RoundRobin>>);

pub struct DynamicGateway {
    redis_connection: Connection,
}

#[async_trait]
impl ProxyHttp for LB {
    type CTX = DynamicGateway;
    fn new_ctx(&self) -> Self::CTX {
        DynamicGateway {
            redis_connection: init_redis_connection().unwrap(),
        }
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let raw_result: String = _ctx
            .redis_connection
            .hgetall("configurable-proxy-redis-storage")
            .unwrap();

        println!(raw_result);

        let (even, odd): (Vec<_>, Vec<_>) = raw_result
            .split("\n")
            .map(|s| s.to_string())
            .partition(|&x, i| i % 2 == 0);

        let result: Vec<_> = even.iter().zip(odd).collect();

        info!("connecting to {addr:?}");

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
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

        self.req_metric.inc();
    }
}

fn main() {
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse();
    let mut server = Server::new(Some(opt)).unwrap();
    server.bootstrap();

    let mut proxy = pingora_proxy::http_proxy_service(
        &server.configuration,
        DynamicGateway {
            redis_connection: init_redis_connection().unwrap(),
        },
    );
    proxy.add_tcp("0.0.0.0:8080");
    server.add_service(proxy);

    server.run_forever();
}
