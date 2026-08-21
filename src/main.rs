#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod common;
mod config;
mod handlers;
mod netease;
mod qqmusic;
mod smtc;
mod source;
mod state;

use axum::{routing::get, Router};
use chrono::Local;
use fern::Dispatch;
use handlers::*;
use state::AppState;
use std::io;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[tokio::main]
async fn main() {
    // ── 日志轮转 ─────────────────────────────────────────────────────
    const LOG_PATH: &str = "smtc-bridge.log";
    const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

    if let Ok(meta) = std::fs::metadata(LOG_PATH) {
        if meta.len() > MAX_LOG_BYTES {
            let backup = format!("{LOG_PATH}.old");
            let _ = std::fs::remove_file(&backup);
            if let Err(e) = std::fs::rename(LOG_PATH, &backup) {
                eprintln!("log rotation failed: {e}");
            } else {
                eprintln!(
                    "rotated {LOG_PATH} -> {backup} ({:.1} MiB)",
                    meta.len() as f64 / (1024.0 * 1024.0)
                );
            }
        }
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .expect("open log file");

    Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .level_for("reqwest", log::LevelFilter::Warn)
        .chain(io::stderr())
        .chain(log_file)
        .apply()
        .expect("logger init");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = Arc::new(AppState::new(shutdown_tx));

    // ── 后台缓存清扫任务 ─────────────────────────────────────────
    {
        let state = state.clone();
        tokio::spawn(async move {
            // 每 5 分钟清扫一次，保持缓存精简。
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                state.sweep_all_caches().await;
            }
        });
    }

    let app = Router::new()
        .route("/", get(|| async { handlers::html_page() }))
        .route("/health", get(handle_health))
        .route("/status", get(handle_status))
        .route("/lyrics", get(handle_lyrics))
        .route("/lyrics/now", get(handle_lyrics_now))
        .route("/cover", get(handle_cover))
        .route("/control", get(handle_control).post(handle_control))
        .route("/shutdown", get(handle_shutdown))
        .fallback(axum::routing::any(handle_catch_all))
        .with_state(state)
        // 集中处理 CORS（替代在单个响应上手动添加响应头）。
        .layer(CorsLayer::permissive());

    let addr = format!("{}:{}", config::HOST.as_str(), *config::PORT);
    // 通配绑定（0.0.0.0/::）时打印 127.0.0.1，方便把 URL 直接复制到浏览器；
    // 服务本身仍监听配置的地址。
    let display_host = if config::HOST.as_str() == "0.0.0.0" || config::HOST.as_str() == "::" {
        "127.0.0.1"
    } else {
        config::HOST.as_str()
    };
    log::info!(
        "SMTC bridge listening on http://{display_host}:{}",
        *config::PORT
    );

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut rx = shutdown_rx;
            let _ = rx.wait_for(|v| *v).await;
            log::info!("shutdown signal received — draining connections…");
        })
        .await
        .unwrap();
}
