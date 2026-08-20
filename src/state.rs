use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{watch, Mutex};

use crate::common::{
    sweep_cache, CacheEntry, LyricResult, MetaInfo, PositionAnchor, SmtcStatus, EDGE_UA,
};
use crate::config::*;
use crate::netease::NeteaseSource;
use crate::qqmusic::QQMusicSource;
use crate::source::MusicSource;

/// SMTC cover thumbnail cache: cover id -> (jpeg body, content-type).
type ThumbnailCache = HashMap<String, CacheEntry<(Vec<u8>, String)>>;

pub struct AppState {
    pub status_cache: Mutex<Option<(Instant, SmtcStatus)>>,
    /// SMTC cover thumbnails keyed by cover id (per-track), so switching
    /// tracks never serves the previous track's thumbnail.
    pub thumbnail_cache: Mutex<ThumbnailCache>,
    /// Serialises the heavy `enriched_status` fetch so concurrent requests
    /// do not stampede the SMTC / lyrics APIs.
    pub fetch_mutex: Mutex<()>,
    /// Last *successfully connected* status snapshot, used as a fallback when
    /// the SMTC session temporarily disconnects (e.g. player paused too long).
    pub last_known_status: Mutex<Option<SmtcStatus>>,
    /// Position anchor for estimating progress on unreliable SMTC timelines.
    pub position_anchor: Mutex<Option<PositionAnchor>>,
    /// At most one control action (play/pause/next…) in flight at a time.
    pub control_lock: Mutex<()>,
    /// When the "SMTC disconnected/error" warning was last logged — used to
    /// rate-limit log spam from the dashboard polling `/status` every 1.5s.
    pub disconnect_log_at: Mutex<Option<Instant>>,
    pub netease: Arc<NeteaseSource>,
    pub qqmusic: Arc<QQMusicSource>,
    /// Music sources in fallback order — `enriched_status` tries each in turn
    /// and stops at the first one that returns lyrics.
    pub sources: Vec<Arc<dyn MusicSource>>,
    /// Background lyric resolution results keyed by track identity.  Filled by
    /// `handlers::spawn_lyric_resolution` so `/status` never blocks on network
    /// calls (slow lyric APIs used to stall responses and starve clients with
    /// short HTTP timeouts, e.g. ESP32).
    pub lyric_cache: Mutex<HashMap<String, CacheEntry<(LyricResult, MetaInfo)>>>,
    /// Track identities currently being resolved in the background (dedup set,
    /// prevents spawning duplicate resolvers for the same track).
    pub lyric_fetching: Mutex<HashSet<String>>,
    pub http_client: reqwest::Client,
    /// Shutdown signal: `handle_shutdown` flips this to `true`, `main` waits
    /// for it and drains connections gracefully.
    pub shutdown: watch::Sender<bool>,
}

impl AppState {
    pub fn new(shutdown: watch::Sender<bool>) -> Self {
        // Create a single shared HTTP client for all sources.
        let http_client = reqwest::Client::builder()
            .user_agent(EDGE_UA)
            .timeout(std::time::Duration::from_secs(9))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(2)
            .build()
            .expect("reqwest client");

        let netease = Arc::new(NeteaseSource::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
            http_client.clone(),
        ));

        let qqmusic = Arc::new(QQMusicSource::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
            http_client.clone(),
        ));

        // Fallback order: QQ Music first for QQ sessions, NetEase as the
        // general source.  `enriched_status` picks the relevant order.
        let sources: Vec<Arc<dyn MusicSource>> = vec![qqmusic.clone(), netease.clone()];

        Self {
            status_cache: Mutex::new(None),
            thumbnail_cache: Mutex::new(HashMap::new()),
            fetch_mutex: Mutex::new(()),
            last_known_status: Mutex::new(None),
            position_anchor: Mutex::new(None),
            control_lock: Mutex::new(()),
            disconnect_log_at: Mutex::new(None),
            netease,
            qqmusic,
            sources,
            lyric_cache: Mutex::new(HashMap::new()),
            lyric_fetching: Mutex::new(HashSet::new()),
            http_client,
            shutdown,
        }
    }

    /// Sweep the caches of every music source plus the background lyric and
    /// cover-thumbnail caches.
    pub async fn sweep_all_caches(&self) {
        for source in &self.sources {
            source.sweep_caches().await;
        }
        {
            let mut c = self.lyric_cache.lock().await;
            sweep_cache(&mut c, LYRIC_CACHE_MS);
        }
        {
            let mut c = self.thumbnail_cache.lock().await;
            sweep_cache(&mut c, THUMBNAIL_CACHE_MS);
        }
    }
}
