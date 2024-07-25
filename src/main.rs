mod redis_utils;
use pingora::prelude::*;

fn main() {
    let con = redis_utils::init_redis_connection().unwrap();

    println!("Connect to redis succeed!");
    println!("Endpoint: {}", redis_utils::REDIS_ENDPOINT.as_str());

    let mut server = Server::new(None).unwrap();
    server.bootstrap();
    server.run_forever();
}
