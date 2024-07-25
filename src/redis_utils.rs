use once_cell::sync::Lazy;
use redis::*;

pub static REDIS_ENDPOINT: Lazy<String> = Lazy::new(|| {
    option_env!("REDIS_ENDPOINT")
        .unwrap_or("redis://127.0.0.1:8081/")
        .to_owned()
});

pub fn init_redis_connection() -> redis::RedisResult<Connection> {
    let client = redis::Client::open(REDIS_ENDPOINT.as_str())?;
    client.get_connection()
}
