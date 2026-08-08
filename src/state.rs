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
    /// Serialises the heavy `enriched_status` fetch so concurrent requests
    /// do not stampede the SMTC / lyrics APIs.
    pub fetch_mutex: Mutex<()>,
    /// Last *successfully connected* status snapshot, used as a fallback when
    /// the SMTC session temporarily disconnects (e.g. player paused too long).
    pub last_known_status: Mutex<Option<SmtcStatus>>,
    /// At most one control action (play/pause/next…) in flight at a time.
    pub control_lock: Mutex<()>,
    pub netease: NeteaseSource,
    pub qqmusic: QQMusicSource,
    pub http_client: reqwest::Client,
}

impl AppState {
    pub fn new() -> Self {
        // Create a single shared HTTP client for all sources.
        let http_client = reqwest::Client::builder()
            .user_agent(EDGE_UA)
            .timeout(std::time::Duration::from_secs(9))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(2)
            .build()
            .expect("reqwest client");

        let netease = NeteaseSource::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
            http_client.clone(),
        );

        let qqmusic = QQMusicSource::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
            http_client.clone(),
        );

        Self {
            status_cache: Mutex::new(None),
            thumbnail_cache: Mutex::new(None),
            fetch_mutex: Mutex::new(()),
            last_known_status: Mutex::new(None),
            control_lock: Mutex::new(()),
            netease,
            qqmusic,
            http_client,
        }
    }

    /// Sweep all caches (lyric, search, meta) for both providers.
    pub async fn sweep_all_caches(&self) {
        use crate::common::sweep_cache;

        // NetEase caches
        {
            let mut c = self.netease.lyric_cache.lock().await;
            let before = c.len();
            sweep_cache(&mut c, self.netease.lyric_cache_ms);
            let after = c.len();
            if before != after {
                log::debug!("netease lyric cache swept: {before} -> {after}");
            }
        }
        {
            let mut c = self.netease.search_cache.lock().await;
            let before = c.len();
            sweep_cache(&mut c, self.netease.search_cache_ms);
            let after = c.len();
            if before != after {
                log::debug!("netease search cache swept: {before} -> {after}");
            }
        }
        {
            let mut c = self.netease.meta_cache.lock().await;
            let before = c.len();
            sweep_cache(&mut c, self.netease.meta_cache_ms);
            let after = c.len();
            if before != after {
                log::debug!("netease meta cache swept: {before} -> {after}");
            }
        }

        // QQ Music caches
        {
            let mut c = self.qqmusic.lyric_cache.lock().await;
            let before = c.len();
            sweep_cache(&mut c, self.qqmusic.lyric_cache_ms);
            let after = c.len();
            if before != after {
                log::debug!("qqmusic lyric cache swept: {before} -> {after}");
            }
        }
        {
            let mut c = self.qqmusic.search_cache.lock().await;
            let before = c.len();
            sweep_cache(&mut c, self.qqmusic.search_cache_ms);
            let after = c.len();
            if before != after {
                log::debug!("qqmusic search cache swept: {before} -> {after}");
            }
        }
        {
            let mut c = self.qqmusic.meta_cache.lock().await;
            let before = c.len();
            sweep_cache(&mut c, self.qqmusic.meta_cache_ms);
            let after = c.len();
            if before != after {
                log::debug!("qqmusic meta cache swept: {before} -> {after}");
            }
        }
    }
}
