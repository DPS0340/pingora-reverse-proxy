use log::info;
use once_cell::sync::Lazy;
use pingora::{Error, ErrorType};
use redis::*;

use crate::utils::log_and_return_err;

pub static REDIS_ENDPOINT: Lazy<String> = Lazy::new(|| {
    option_env!("REDIS_ENDPOINT")
        .unwrap_or("redis://127.0.0.1:8091")
        .to_owned()
});

pub fn init_redis_connection() -> Result<Connection, Box<Error>> {
    let client = redis::Client::open(REDIS_ENDPOINT.as_str()).map_err(|e| {
        Error::explain(
            ErrorType::HTTPStatus(400),
            format!("Open redis client failed: {}", e),
        )
    })?;

    info!("Connect to redis succeed!");
    info!("Endpoint: {}", REDIS_ENDPOINT.as_str());
    client.get_connection().map_err(|e| {
        log_and_return_err(Result::<Box<()>, _>::Err(Error::explain(
            ErrorType::HTTPStatus(400),
            format!("Connect to redis failed: {}", e),
        )))
        .unwrap_err()
    })
}
