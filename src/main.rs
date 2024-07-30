mod proxy;
mod redis_utils;
mod utils;
use pingora_core::server::Server;
use pingora_proxy::http_proxy_service;
use proxy::DynamicGateway;

fn main() {
    env_logger::init();

    let mut server = Server::new(None).unwrap();
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, DynamicGateway {});
    // proxy port: 8080
    // mgmt port: 8081
    //   -> admin api (prefix, service redis hset)
    //   -> pingora: forward 0.0.0.0:8081 -> 127.0.0.1:8091
    //   -> axum: implement api & 127.0.0.0:8091 listen
    //   * run pingora & axum in same function: main()
    svc.add_tcp("0.0.0.0:8080");

    server.add_service(svc);
    server.run_forever();
}
