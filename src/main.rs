#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod common;
mod config;
mod handlers;
mod netease;
mod qqmusic;
mod smtc;
mod state;

use std::io;
use std::sync::Arc;
use axum::{routing::get, Router};
use chrono::Local;
use fern::Dispatch;
use handlers::*;
use state::AppState;

#[tokio::main]
async fn main() {
    // ── Log rotation ─────────────────────────────────────────────────────
    const LOG_PATH: &str = "smtc-bridge.log";
    const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

    if let Ok(meta) = std::fs::metadata(LOG_PATH) {
        if meta.len() > MAX_LOG_BYTES {
            let backup = format!("{LOG_PATH}.old");
            let _ = std::fs::remove_file(&backup);
            if let Err(e) = std::fs::rename(LOG_PATH, &backup) {
                eprintln!("log rotation failed: {e}");
            } else {
                eprintln!("rotated {LOG_PATH} -> {backup} ({:.1} MiB)",
                    meta.len() as f64 / (1024.0 * 1024.0));
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

    let state = Arc::new(AppState::new());

    // ── Background cache sweeper ─────────────────────────────────────────
    {
        let state = state.clone();
        tokio::spawn(async move {
            // Sweep every 5 minutes to keep caches lean.
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
        .route("/cover", get(handle_cover))
        .route("/control", get(handle_control).post(handle_control))
        .route("/shutdown", get(handle_shutdown))
        .fallback(axum::routing::any(handle_catch_all))
        .with_state(state);

    let addr = format!("{}:{}", config::HOST, config::PORT);
    log::info!("SMTC bridge listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
