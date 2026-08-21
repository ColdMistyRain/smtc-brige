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

/// SMTC 封面缩略图缓存：封面 id -> (jpeg 数据, content-type)。
type ThumbnailCache = HashMap<String, CacheEntry<(Vec<u8>, String)>>;

pub struct AppState {
    pub status_cache: Mutex<Option<(Instant, SmtcStatus)>>,
    /// SMTC 封面缩略图，按封面 id 键控（每首曲目独立），这样切歌时
    /// 绝不会返回上一首歌的缩略图。
    pub thumbnail_cache: Mutex<ThumbnailCache>,
    /// 序列化耗时的 `enriched_status` 抓取，避免并发请求
    /// 冲击 SMTC / 歌词 API。
    pub fetch_mutex: Mutex<()>,
    /// 最后一次*成功连接*的状态快照，当 SMTC 会话暂时断开时
    /// （例如播放器暂停过久）用作回退数据。
    pub last_known_status: Mutex<Option<SmtcStatus>>,
    /// 位置锚点，用于在 SMTC 时间线不可靠时估算播放进度。
    pub position_anchor: Mutex<Option<PositionAnchor>>,
    /// 同一时刻最多只有一个控制动作（播放/暂停/切歌…）在执行。
    pub control_lock: Mutex<()>,
    /// 上次记录"SMTC 断开/错误"警告的时间 —— 用于限制仪表盘每 1.5s
    /// 轮询 `/status` 产生的日志刷屏。
    pub disconnect_log_at: Mutex<Option<Instant>>,
    pub netease: Arc<NeteaseSource>,
    pub qqmusic: Arc<QQMusicSource>,
    /// 音乐源按回退顺序排列 —— `enriched_status` 依次尝试每个源，
    /// 在第一个返回歌词的源处停止。
    pub sources: Vec<Arc<dyn MusicSource>>,
    /// 后台歌词解析结果，按曲目标识键控。由 `handlers::spawn_lyric_resolution`
    /// 填充，确保 `/status` 永不阻塞在网络调用上（慢速歌词 API 曾导致响应
    /// 卡顿，饿死 HTTP 短超时的客户端，例如 ESP32）。
    pub lyric_cache: Mutex<HashMap<String, CacheEntry<(LyricResult, MetaInfo)>>>,
    /// 当前正在后台解析的曲目标识（去重集合，
    /// 防止为同一曲目重复启动解析任务）。
    pub lyric_fetching: Mutex<HashSet<String>>,
    pub http_client: reqwest::Client,
    /// 关闭信号：`handle_shutdown` 将其置为 `true`，`main` 等待该信号
    /// 并优雅地排空连接。
    pub shutdown: watch::Sender<bool>,
}

impl AppState {
    pub fn new(shutdown: watch::Sender<bool>) -> Self {
        // 为所有音乐源创建一个共享的 HTTP 客户端。
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

        // 回退顺序：QQ 会话优先 QQ 音乐，网易云作为通用源。
        // `enriched_status` 会选择相应的顺序。
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

    /// 清扫所有音乐源的缓存，以及后台歌词与封面缩略图缓存。
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
