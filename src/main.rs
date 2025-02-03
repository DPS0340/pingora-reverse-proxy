mod mgmt_api;
mod proxy;
mod redis_utils;
mod utils;
use mgmt_api::{get_app, get_listener};
use pingora_core::server::Server;
use pingora_proxy::http_proxy_service;
use proxy::DynamicGateway;

#[tokio::main]
async fn main() {
    env_logger::init();

    let mut server = Server::new(None).unwrap();
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, DynamicGateway {});
    // proxy port: 8080
    // mgmt port: 8081
    //   -> admin api (redis hset by prefix & domain, axum based)
    //   * Run pingora & axum in same function: main()
    svc.add_tcp("0.0.0.0:8080");

    server.add_service(svc);

    let app = get_app().await.clone();
    let listener = get_listener().await;

    let mgmt_server = axum::serve(listener, app);

    // Serve pingora server and mgmt api
    futures::join!(async { mgmt_server.await.unwrap() }, async {
        server.run_forever()
    });
}
