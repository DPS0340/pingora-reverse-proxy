use pingora::prelude::*;

fn main() {
    let mut server = Server::new(None).unwrap();
    server.bootstrap();
    server.run_forever();
}
