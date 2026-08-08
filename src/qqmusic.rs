use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::sync::Mutex;

use crate::common::{
    decode_html, maybe_base64_text, merge_translation, normalize_text, parse_lrc, search_score,
    split_artists, urlencoding, CacheEntry, LyricResult, MetaInfo, SmtcStatus, TrackInfo, EDGE_UA,
};

// ── QQ Cover URL helpers ────────────────────────────────────────────────────

fn qq_cover_url(album_mid: &str, size: u32) -> String {
    let mid = album_mid.trim();
    if mid.is_empty() {
        return String::new();
    }
    let encoded = urlencoding(mid);
    format!("https://y.qq.com/music/photo_new/T002R{size}x{size}M000{encoded}.jpg?max_age=2592000")
}

fn qq_singer_cover_url(singer_mid: &str, size: u32) -> String {
    let mid = singer_mid.trim();
    if mid.is_empty() {
        return String::new();
    }
    let encoded = urlencoding(mid);
    format!("https://y.qq.com/music/photo_new/T001R{size}x{size}M000{encoded}.jpg?max_age=2592000")
}

// ── QQ Song ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QQSong {
    pub id: u64,
    pub mid: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub album_mid: String,
    pub singer_mid: String,
    pub cover_url: String,
}

// ── Precompiled Regexes ────────────────────────────────────────────────────

static QQMUSIC_MATCH_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)qqmusic|tencent").unwrap());
static JSONP_CALLBACK_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[\w$.]+\(([\s\S]*)\)\s*;?$").unwrap());

// ── QQMusic Source ──────────────────────────────────────────────────────────

pub struct QQMusicSource {
    pub lyric_cache: Arc<Mutex<HashMap<String, CacheEntry<LyricResult>>>>,
    pub search_cache: Arc<Mutex<HashMap<String, CacheEntry<Option<QQSong>>>>>,
    pub meta_cache: Arc<Mutex<HashMap<String, CacheEntry<QQSong>>>>,
    pub lyric_cache_ms: u64,
    pub search_cache_ms: u64,
    pub meta_cache_ms: u64,
    client: reqwest::Client,
}

impl QQMusicSource {
    pub fn new(
        lyric_cache: Arc<Mutex<HashMap<String, CacheEntry<LyricResult>>>>,
        search_cache: Arc<Mutex<HashMap<String, CacheEntry<Option<QQSong>>>>>,
        meta_cache: Arc<Mutex<HashMap<String, CacheEntry<QQSong>>>>,
        lyric_cache_ms: u64,
        search_cache_ms: u64,
        meta_cache_ms: u64,
    ) -> Self {
        Self {
            lyric_cache,
            search_cache,
            meta_cache,
            lyric_cache_ms,
            search_cache_ms,
            meta_cache_ms,
            client: reqwest::Client::builder()
                .user_agent(EDGE_UA)
                .timeout(std::time::Duration::from_secs(9))
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .pool_max_idle_per_host(2)
                .build()
                .expect("reqwest client"),
        }
    }

    #[allow(dead_code)]
    pub fn matches(&self, status: &SmtcStatus) -> bool {
        QQMUSIC_MATCH_RE.is_match(&status.source)
    }

    async fn normalize_song(&self, song: &serde_json::Value) -> Option<QQSong> {
        let singer = song["singer"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        let album = &song["album"];

        let id = song["songid"]
            .as_u64()
            .or_else(|| song["id"].as_u64())
            .or_else(|| song["musicid"].as_u64())
            .unwrap_or(0);

        let mid = song["songmid"]
            .as_str()
            .or_else(|| song["mid"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let album_mid = album["mid"]
            .as_str()
            .or_else(|| song["albummid"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let singer_mid = singer
            .first()
            .and_then(|s| s["mid"].as_str().or_else(|| s["pmid"].as_str()))
            .unwrap_or("")
            .trim()
            .to_string();

        let cover_url = if !album_mid.is_empty() {
            qq_cover_url(&album_mid, 300)
        } else if !singer_mid.is_empty() {
            qq_singer_cover_url(&singer_mid, 300)
        } else {
            String::new()
        };

        let qq_song = QQSong {
            id,
            mid: mid.clone(),
            title: song["songname"]
                .as_str()
                .or_else(|| song["name"].as_str())
                .unwrap_or("")
                .to_string(),
            artist: singer
                .iter()
                .filter_map(|s| s["name"].as_str())
                .collect::<Vec<_>>()
                .join(" / "),
            album: album["name"]
                .as_str()
                .or_else(|| song["albumname"].as_str())
                .unwrap_or("")
                .to_string(),
            duration_ms: song["interval"].as_u64().unwrap_or(0) * 1000,
            album_mid: album_mid.clone(),
            singer_mid,
            cover_url,
        };

        // Update meta caches immediately (match JS eager caching in normalizeSong)
        let entry = CacheEntry::new(qq_song.clone());
        {
            let mut cache = self.meta_cache.lock().await;
            if id > 0 {
                cache.insert(format!("id:{id}"), entry.clone());
            }
            if !mid.is_empty() {
                cache.insert(format!("mid:{mid}"), entry.clone());
            }
            if !album_mid.is_empty() {
                cache.insert(format!("album:{album_mid}"), entry);
            }
        }

        Some(qq_song)
    }

    pub async fn search_song(&self, track: &TrackInfo) -> Option<QQSong> {
        let title = track.title.trim();
        let artist = track.artist.trim();
        if title.is_empty() {
            return None;
        }

        let key = normalize_text(&format!("{title} {artist}"));
        {
            let cache = self.search_cache.lock().await;
            if let Some(entry) = cache.get(&key) {
                if entry.is_fresh(self.search_cache_ms) {
                    log::debug!("qqmusic search cache hit: {key}");
                    return entry.value.clone();
                }
            }
        }

        log::debug!("qqmusic search: title={title:?} artist={artist:?}");
        let query = urlencoding(&format!("{title} {artist}"));
        let endpoints = [
            format!("https://c.y.qq.com/soso/fcgi-bin/client_search_cp?ct=24&qqmusic_ver=1298&new_json=1&remoteplace=txt.yqq.song&searchid=1&t=0&aggr=1&cr=1&catZhida=1&lossless=0&flag_qc=0&p=1&n=8&w={query}&format=json&platform=yqq.json&needNewCode=0"),
            format!("https://c.y.qq.com/soso/fcgi-bin/search_cp?g_tk=5381&uin=0&format=json&inCharset=utf-8&outCharset=utf-8&notice=0&platform=yqq&needNewCode=0&w={query}&zhidaqu=1&catZhida=1&t=0&flag=1&ie=utf-8&sem=1&aggr=0&perpage=8&n=8&p=1&remoteplace=txt.mqq.all"),
        ];

        let mut songs: Vec<serde_json::Value> = vec![];
        for endpoint in &endpoints {
            if let Ok(resp) = self
                .client
                .get(endpoint)
                .header("Referer", "https://y.qq.com/")
                .send()
                .await
            {
                if let Ok(text) = resp.text().await {
                    if let Ok(doc) = parse_loose_json(&text) {
                        songs = doc["data"]["song"]["list"]
                            .as_array()
                            .or_else(|| doc["data"]["list"].as_array())
                            .cloned()
                            .unwrap_or_default();
                        if !songs.is_empty() {
                            break;
                        }
                    }
                }
            }
        }

        let mut best: Option<QQSong> = None;
        let mut best_score: i32 = -1;

        for raw_song in &songs {
            if let Some(song) = self.normalize_song(raw_song).await {
                let score = search_score(
                    &song.title,
                    &split_artists(&song.artist),
                    song.duration_ms,
                    track,
                );
                if score > best_score {
                    best_score = score;
                    best = Some(song);
                }
            }
        }

        let value = best.filter(|_| best_score >= 45);
        if let Some(ref song) = value {
            log::debug!("qqmusic search result: id={} mid={} score={best_score}", song.id, song.mid);
        } else {
            log::debug!("qqmusic search: no match (best_score={best_score})");
        }
        let mut cache = self.search_cache.lock().await;
        cache.insert(key, CacheEntry::new(value.clone()));
        value
    }

    pub async fn fetch_lyrics(&self, song_id: u64, song_mid: &str) -> LyricResult {
        let mut mid = song_mid.to_string();
        if mid.is_empty() && song_id > 0 {
            let cache = self.meta_cache.lock().await;
            if let Some(entry) = cache.get(&format!("id:{song_id}")) {
                if entry.is_fresh(self.meta_cache_ms) {
                    mid = entry.value.mid.clone();
                }
            }
        }

        if song_id == 0 && mid.is_empty() {
            return LyricResult {
                source: String::new(),
                translation_line_count: 0,
                lines: vec![],
            };
        }

        let cache_key = format!("qq:{}", if song_id > 0 { song_id.to_string() } else { mid.clone() });
        {
            let cache = self.lyric_cache.lock().await;
            if let Some(entry) = cache.get(&cache_key) {
                if entry.is_fresh(self.lyric_cache_ms) {
                    log::debug!("qqmusic lyric cache hit: {cache_key}");
                    return entry.value.clone();
                }
            }
        }

        log::debug!("qqmusic fetch lyrics: song_id={song_id} mid={mid}");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let params = [
            "nobase64=1",
            "format=json",
            "inCharset=utf8",
            "outCharset=utf-8",
            "notice=0",
            "platform=yqq.json",
            "needNewCode=0",
            "g_tk=5381",
            "hostUin=0",
            "loginUin=0",
            "trans=1",
            &format!("pcachetime={now}"),
        ]
        .join("&");

        let mut id_parts: Vec<String> = vec![];
        if song_id > 0 && !mid.is_empty() {
            id_parts.push(format!("musicid={song_id}&songmid={mid}"));
        }
        if song_id > 0 {
            id_parts.push(format!("musicid={song_id}"));
        }
        if !mid.is_empty() {
            id_parts.push(format!("songmid={mid}"));
        }

        let endpoints: Vec<String> = id_parts
            .iter()
            .flat_map(|id_part| {
                vec![
                    format!("https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?{params}&{id_part}"),
                    format!("https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric.fcg?{params}&{id_part}"),
                ]
            })
            .collect();

        let referer = "https://y.qq.com/n/ryqq/player";

        for endpoint in &endpoints {
            if let Ok(resp) = self
                .client
                .get(endpoint)
                .header("Referer", referer)
                .header("Origin", "https://y.qq.com")
                .send()
                .await
            {
                if let Ok(text) = resp.text().await {
                    if let Ok(doc) = parse_loose_json(&text) {
                        let lyric_raw = decode_html(&maybe_base64_text(
                            doc["lyric"].as_str().unwrap_or(""),
                        ));
                        let trans_raw = decode_html(&maybe_base64_text(
                            doc["trans"]
                                .as_str()
                                .or_else(|| doc["translate"].as_str())
                                .unwrap_or(""),
                        ));

                        let primary = parse_lrc(&lyric_raw);
                        let translated = parse_lrc(&trans_raw);
                        let translation_count = translated.len();
                        let lines = merge_translation(&primary, &translated);

                        if !lines.is_empty() {
                            let value = LyricResult {
                                source: format!("qqmusic:{}", if song_id > 0 { song_id.to_string() } else { mid.clone() }),
                                translation_line_count: translation_count,
                                lines,
                            };
                            let mut cache = self.lyric_cache.lock().await;
                            cache.insert(cache_key, CacheEntry::new(value.clone()));
                            return value;
                        }
                    }
                }
            }
        }

        let value = LyricResult {
            source: String::new(),
            translation_line_count: 0,
            lines: vec![],
        };
        let mut cache = self.lyric_cache.lock().await;
        cache.insert(cache_key, CacheEntry::new(value.clone()));
        value
    }

    pub async fn resolve(&self, status: &mut SmtcStatus) -> (LyricResult, MetaInfo) {
        log::debug!("qqmusic resolve: title={:?} artist={:?}", status.title, status.artist);
        let track = TrackInfo {
            title: status.title.clone(),
            artist: status.artist.clone(),
            duration_ms: status.duration_ms.max(0) as u64,
        };

        if let Some(qq) = self.search_song(&track).await {
            status.qq_song_id = qq.id as i64;
            status.qq_song_mid = qq.mid.clone();
            status.qq_album_mid = qq.album_mid.clone();
            status.lyric_provider = "qqmusic".to_string();
            status.lyric_id_text = if qq.id > 0 {
                qq.id.to_string()
            } else {
                qq.mid.clone()
            };
            status.cover_provider = if !qq.album_mid.is_empty() {
                "qqmusic"
            } else if !qq.singer_mid.is_empty() {
                "qqartist"
            } else {
                "smtc"
            }
            .to_string();
            status.cover_id_text = if !qq.album_mid.is_empty() {
                qq.album_mid.clone()
            } else if !qq.singer_mid.is_empty() {
                qq.singer_mid.clone()
            } else if qq.id > 0 {
                qq.id.to_string()
            } else if !qq.mid.is_empty() {
                qq.mid.clone()
            } else {
                "current".to_string()
            };

            // Meta cache already populated by normalize_song; no need to re-insert here.

            let found = self.fetch_lyrics(qq.id, &qq.mid).await;
            let cover_url = if qq.cover_url.is_empty() {
                "smtc:current".to_string()
            } else {
                qq.cover_url.clone()
            };
            let meta = MetaInfo {
                id: qq.id,
                title: qq.title,
                album: qq.album,
                duration_ms: qq.duration_ms,
                cover_url,
                artist: qq.artist,
            };
            (found, meta)
        } else {
            status.lyric_provider = "qqmusic".to_string();
            status.lyric_id_text = String::new();
            status.cover_provider = "qqmusic".to_string();
            status.cover_id_text = String::new();
            (
                LyricResult {
                    source: String::new(),
                    translation_line_count: 0,
                    lines: vec![],
                },
                MetaInfo::default(),
            )
        }
    }

    #[allow(dead_code)]
    pub fn cover_candidates(&self, id: &str, provider: &str) -> Vec<String> {
        if provider == "qqartist" {
            [92, 150, 300, 500]
                .iter()
                .map(|&size| qq_singer_cover_url(id, size))
                .collect()
        } else {
            [92, 150, 300, 500]
                .iter()
                .map(|&size| qq_cover_url(id, size))
                .collect()
        }
    }
}

// ── JSONP Parsing ───────────────────────────────────────────────────────────

fn parse_loose_json(raw: &str) -> Result<serde_json::Value, serde_json::Error> {
    let text = raw.trim();
    // Strip JSONP callback wrapper: callback({...});
    let text = if let Some(caps) = JSONP_CALLBACK_RE.captures(text) {
        caps[1].to_string()
    } else {
        text.to_string()
    };
    serde_json::from_str(&text)
}
