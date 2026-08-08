mod common;
mod config;
mod handlers;
mod netease;
mod qqmusic;
mod smtc;
mod state;

use std::sync::Arc;
use axum::{routing::get, Router};
use handlers::*;
use state::AppState;

#[tokio::main]
async fn main() {
    env_logger::init();
    let state = Arc::new(AppState::new());

    let app = Router::new()
        .route("/", get(|| async { handlers::html_page() }))
        .route("/health", get(handle_health))
        .route("/status", get(handle_status))
        .route("/lyrics", get(handle_lyrics))
        .route("/cover", get(handle_cover))
        .route("/control", get(handle_control).post(handle_control))
        .fallback(axum::routing::any(handle_catch_all))
        .with_state(state);

    let addr = format!("{}:{}", config::HOST, config::PORT);
    log::info!("SMTC bridge starting on http://{addr}");
    println!("SMTC bridge listening on http://{addr}");
    println!("Lyrics: InfLink NCM-{{id}} -> NetEase, QQ Music SMTC -> QQ Music API");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
