mod mgmt_api;
mod proxy;
mod redis_utils;
mod utils;
use mgmt_api::{get_app, get_listener};
use pingora_core::server::Server;
use pingora_proxy::http_proxy_service;
use proxy::DynamicGateway;

fn main() {
    env_logger::init();

    let mut server = Server::new(None).unwrap();
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, DynamicGateway {});
    // proxy port: 8170
    // mgmt port: 8171
    //   -> admin api (redis hset by prefix & domain, axum based)
    //   * Run pingora & axum in same function: main()
    svc.add_tcp("0.0.0.0:8170");

    server.add_service(svc);

    let binding = pingora_runtime::Runtime::new_steal(8, "pingora-reverse-proxy");
    let rt = binding.get_handle();

    let app = get_app();
    let listener = get_listener();

    // Serve pingora server and mgmt api
    let (_e1, _e2) = rt.block_on(async {
        tokio::join!(
            async {
                let mgmt_server = axum::serve(listener.await, app.await.clone());
                mgmt_server.await.unwrap()
            },
            async { server.run_forever() }
        )
    });
}
