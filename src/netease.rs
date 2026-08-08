use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::common::{
    merge_translation, normalize_text, parse_lrc, search_score, urlencoding, CacheEntry,
    LyricResult, MetaInfo, SmtcStatus, TrackInfo, EDGE_UA,
};

pub struct NeteaseSource {
    pub lyric_cache: Arc<Mutex<HashMap<u64, CacheEntry<LyricResult>>>>,
    pub search_cache: Arc<Mutex<HashMap<String, CacheEntry<u64>>>>,
    pub meta_cache: Arc<Mutex<HashMap<u64, CacheEntry<MetaInfo>>>>,
    pub lyric_cache_ms: u64,
    pub search_cache_ms: u64,
    pub meta_cache_ms: u64,
    client: reqwest::Client,
}

impl NeteaseSource {
    pub fn new(
        lyric_cache: Arc<Mutex<HashMap<u64, CacheEntry<LyricResult>>>>,
        search_cache: Arc<Mutex<HashMap<String, CacheEntry<u64>>>>,
        meta_cache: Arc<Mutex<HashMap<u64, CacheEntry<MetaInfo>>>>,
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
                .timeout(std::time::Duration::from_secs(7))
                .build()
                .expect("reqwest client"),
        }
    }

    pub async fn search_song(&self, track: &TrackInfo) -> u64 {
        let title = track.title.trim();
        let artist = track.artist.trim();
        if title.is_empty() {
            return 0;
        }

        let key = normalize_text(&format!("{title} {artist}"));
        {
            let cache = self.search_cache.lock().await;
            if let Some(entry) = cache.get(&key) {
                if entry.is_fresh(self.search_cache_ms) {
                    return entry.value;
                }
            }
        }

        let query = urlencoding(&format!("{title} {artist}"));
        let url = format!(
            "https://music.163.com/api/search/get/web?csrf_token=&type=1&limit=8&s={query}"
        );

        let mut best_id: u64 = 0;
        let mut best_score: i32 = -1;

        match self
            .client
            .get(&url)
            .header("Referer", "https://music.163.com/")
            .header("Accept", "application/json,text/plain,*/*")
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(doc) = resp.json::<serde_json::Value>().await {
                    if let Some(songs) = doc["result"]["songs"].as_array() {
                        for song in songs {
                            let song_name = song["name"].as_str().unwrap_or("");
                            let artists: Vec<String> = song["artists"]
                                .as_array()
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|a| {
                                            a["name"].as_str().map(|n| n.to_string())
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let duration = song["duration"].as_u64().unwrap_or(0);

                            let score = search_score(song_name, &artists, duration, track);
                            if score > best_score {
                                best_score = score;
                                best_id = song["id"].as_u64().unwrap_or(0);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("NetEase search HTTP error: {e}");
            }
        }

        let id = if best_score >= 45 { best_id } else { 0 };
        let mut cache = self.search_cache.lock().await;
        cache.insert(key, CacheEntry::new(id));
        id
    }

    pub async fn fetch_lyrics(&self, ncm_id: u64) -> LyricResult {
        if ncm_id == 0 {
            return LyricResult {
                source: String::new(),
                translation_line_count: 0,
                lines: vec![],
            };
        }

        {
            let cache = self.lyric_cache.lock().await;
            if let Some(entry) = cache.get(&ncm_id) {
                if entry.is_fresh(self.lyric_cache_ms) {
                    return entry.value.clone();
                }
            }
        }

        let url = format!("https://music.163.com/api/song/lyric?id={ncm_id}&lv=-1&kv=-1&tv=-1");
        let mut result = LyricResult {
            source: format!("netease:{ncm_id}"),
            translation_line_count: 0,
            lines: vec![],
        };

        match self
            .client
            .get(&url)
            .header("Referer", "https://music.163.com/")
            .header("Accept", "application/json,text/plain,*/*")
            .send()
            .await
        {
            Ok(resp) => {
                match resp.json::<serde_json::Value>().await {
                    Ok(doc) => {
                        let primary = parse_lrc(doc["lrc"]["lyric"].as_str().unwrap_or(""));
                        let translated = parse_lrc(doc["tlyric"]["lyric"].as_str().unwrap_or(""));
                        let translation_count = translated.len();
                        let lines = merge_translation(&primary, &translated);
                        result.translation_line_count = translation_count;
                        result.lines = lines;
                    }
                    Err(e) => {
                        log::warn!("NetEase lyric JSON parse error for id={ncm_id}: {e}");
                    }
                }
            }
            Err(e) => {
                log::warn!("NetEase lyric HTTP error for id={ncm_id}: {e}");
            }
        }

        let mut cache = self.lyric_cache.lock().await;
        cache.insert(ncm_id, CacheEntry::new(result.clone()));
        result
    }

    pub async fn fetch_meta(&self, ncm_id: u64) -> MetaInfo {
        if ncm_id == 0 {
            return MetaInfo::default();
        }

        {
            let cache = self.meta_cache.lock().await;
            if let Some(entry) = cache.get(&ncm_id) {
                if entry.is_fresh(self.meta_cache_ms) {
                    return entry.value.clone();
                }
            }
        }

        let url = format!("https://music.163.com/api/song/detail/?ids=%5B{ncm_id}%5D");
        let mut meta = MetaInfo::default();

        match self
            .client
            .get(&url)
            .header("Referer", "https://music.163.com/")
            .header("Accept", "application/json,text/plain,*/*")
            .send()
            .await
        {
            Ok(resp) => {
                match resp.json::<serde_json::Value>().await {
                    Ok(doc) => {
                        if let Some(songs) = doc["songs"].as_array() {
                            if let Some(song) = songs.first() {
                                // JS: song?.album || song?.al || {}
                                let album = song["album"]
                                    .as_object()
                                    .or_else(|| song["al"].as_object());

                                let cover_raw = album
                                    .and_then(|a| a["picUrl"].as_str())
                                    .or_else(|| album.and_then(|a| a["pic"].as_str()))
                                    .unwrap_or("");

                                let album_name = album
                                    .and_then(|a| a["name"].as_str())
                                    .unwrap_or("");

                                meta = MetaInfo {
                                    id: ncm_id,
                                    title: song["name"].as_str().unwrap_or("").to_string(),
                                    album: album_name.to_string(),
                                    duration_ms: song["duration"]
                                        .as_u64()
                                        .or_else(|| song["dt"].as_u64())
                                        .unwrap_or(0),
                                    cover_url: if cover_raw.is_empty() {
                                        String::new()
                                    } else {
                                        format!("{cover_raw}?param=92y92&type=jpg")
                                    },
                                    artist: String::new(),
                                };
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("NetEase meta JSON parse error for id={ncm_id}: {e}");
                    }
                }
            }
            Err(e) => {
                log::warn!("NetEase meta HTTP error for id={ncm_id}: {e}");
            }
        }

        let mut cache = self.meta_cache.lock().await;
        cache.insert(ncm_id, CacheEntry::new(meta.clone()));
        meta
    }

    pub async fn resolve(&self, status: &mut SmtcStatus) -> (LyricResult, MetaInfo) {
        let mut ncm_id = status.ncm_id as u64;
        let mut source_hint = "smtc";

        if ncm_id == 0 {
            let track = TrackInfo {
                title: status.title.clone(),
                artist: status.artist.clone(),
                duration_ms: status.duration_ms as u64,
            };
            ncm_id = self.search_song(&track).await;
            source_hint = "search";
            status.ncm_id = ncm_id as i64;
        }

        let found = self.fetch_lyrics(ncm_id).await;
        let meta = self.fetch_meta(ncm_id).await;

        status.lyric_provider = "netease".to_string();
        status.lyric_id_text = if status.ncm_id > 0 {
            status.ncm_id.to_string()
        } else {
            String::new()
        };
        status.cover_provider = "netease".to_string();
        status.cover_id_text = if status.ncm_id > 0 {
            status.ncm_id.to_string()
        } else {
            String::new()
        };

        let source = if found.source.is_empty() {
            String::new()
        } else {
            format!("{}:{source_hint}", found.source)
        };

        (
            LyricResult {
                source,
                ..found
            },
            meta,
        )
    }

    pub async fn cover_candidates(&self, id: &str) -> String {
        let ncm_id: u64 = id.parse().unwrap_or(0);
        self.fetch_meta(ncm_id).await.cover_url
    }
}
