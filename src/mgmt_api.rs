use axum::serve::Serve;
use axum::{routing::get, Router};
use tokio::net::TcpListener;
use tokio::sync::OnceCell;

pub static APP: OnceCell<Router> = OnceCell::const_new();

pub async fn get_app() -> &'static Router {
    APP.get_or_init(|| async { Router::new().route("/", get(|| async { "Hello, World!" })) })
        .await
}

pub async fn get_listener() -> TcpListener {
    TcpListener::bind("0.0.0.0:8081").await.unwrap()
}
