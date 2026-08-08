use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::common::{SmtcStatus, EDGE_UA};
use crate::config::*;
use crate::netease::NeteaseSource;
use crate::qqmusic::QQMusicSource;

pub struct AppState {
    pub status_cache: Mutex<Option<(Instant, SmtcStatus)>>,
    pub thumbnail_cache: Mutex<Option<(Instant, Vec<u8>, String)>>,
    pub netease: NeteaseSource,
    pub qqmusic: QQMusicSource,
    pub http_client: reqwest::Client,
}

impl AppState {
    pub fn new() -> Self {
        let netease = NeteaseSource::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
        );

        let qqmusic = QQMusicSource::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
        );

        let http_client = reqwest::Client::builder()
            .user_agent(EDGE_UA)
            .timeout(std::time::Duration::from_secs(9))
            .build()
            .expect("reqwest client");

        Self {
            status_cache: Mutex::new(None),
            thumbnail_cache: Mutex::new(None),
            netease,
            qqmusic,
            http_client,
        }
    }
}
