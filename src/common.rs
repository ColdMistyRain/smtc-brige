use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;
use unicode_normalization::UnicodeNormalization;

// ── 类型 ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LrcLine {
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SmtcStatus {
    // 原始 SMTC 字段
    pub ok: bool,
    pub connected: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_artist: String,
    pub track_number: i32,
    #[serde(default)]
    pub genres: Vec<String>,
    pub ncm_id: i64,
    pub position_ms: i64,
    pub duration_ms: i64,
    // ── 位置外推 ─────────────────────────────────────────
    // 原始 SMTC `Position` 只是播放器偶尔更新的快照
    // （网易云音乐桌面版 / 浏览器中的网页视频几乎不更新或完全不更新）。
    // 我们保留原始采样以及随时间外推"实时"位置所需的信息。
    #[serde(default)]
    pub position_base_ms: i64,
    /// `position_base_ms` 最后一次由播放器上报的 Unix 毫秒时间戳
    /// （SMTC `LastUpdatedTime`，或我们采样时的时间）。
    #[serde(default)]
    pub position_updated_at: i64,
    /// 用于外推实时位置的有效播放速率
    /// （非播放状态时为 0，因此不会进行外推）。
    #[serde(default)]
    pub playback_rate: f64,
    /// 当 `position_ms` 是外推（实时）值而非播放器的原始快照时为 true。
    #[serde(default)]
    pub position_live: bool,
    /// `position_ms` 的获取方式：`"smtc"`（来自播放器的真实采样）或
    /// `"estimated"`（桥接服务依据自身锚点外推，因为播放器不报告可用时间线，
    /// 例如网易云音乐上报 `Position=0`）。
    #[serde(default)]
    pub position_source: String,
    pub session_count: i32,
    pub selected_current: bool,
    pub updated_at: i64,

    /// SMTC 会话（Windows）/ MPRIS 播放器（Linux）上报的每一个原始字段，
    /// 原样暴露，便于消费者查看桥接服务从系统媒体传输控件收到的全部数据。
    #[serde(default)]
    pub raw: RawSmtcInfo,

    // 增强字段
    #[serde(default)]
    pub smtc_adapter: String,
    #[serde(default)]
    pub ncm_id_text: String,
    #[serde(default)]
    pub cover_url: String,
    pub lyrics_available: bool,
    pub translation_line_count: usize,
    #[serde(default)]
    pub lyric_source: String,
    #[serde(default)]
    pub lyric: LyricPosition,
    /// 当前曲目的完整歌词（全部行），由后台解析器填充 —— 客户端无需再次
    /// 请求即可直接从 `/status` 获得完整歌词。
    #[serde(default)]
    pub full_lyrics: Vec<LrcLine>,

    // 提供商提示
    #[serde(default)]
    pub lyric_provider: String,
    #[serde(default)]
    pub lyric_id_text: String,
    #[serde(default)]
    pub cover_provider: String,
    #[serde(default)]
    pub cover_id_text: String,

    // QQ 音乐专有字段
    pub qq_song_id: i64,
    #[serde(default)]
    pub qq_song_mid: String,
    #[serde(default)]
    pub qq_album_mid: String,

    // 错误回退
    #[serde(default)]
    pub error: String,
}

/// 来自系统媒体传输控件会话（Windows SMTC）或 MPRIS 播放器（Linux）的
/// 原始数据。所有值都是操作系统/播放器上报的原始内容；桥接服务在此
/// 不做任何转换。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RawSmtcInfo {
    // ── 会话 ─────────────────────────────────────────────────────────
    #[serde(default)]
    pub source_app_user_model_id: String,

    // ── 播放信息 ───────────────────────────────────────────────────
    #[serde(default)]
    pub playback_status: String,
    #[serde(default)]
    pub playback_type: String,
    #[serde(default)]
    pub auto_repeat_mode: String,
    #[serde(default)]
    pub playback_rate: Option<f64>,
    #[serde(default)]
    pub shuffle_active: Option<bool>,

    // ── 时间线（原始 100ns 刻度，除 `last_updated_unix_ms` 外） ───────
    #[serde(default)]
    pub start_time_ticks: i64,
    #[serde(default)]
    pub end_time_ticks: i64,
    #[serde(default)]
    pub min_seek_ticks: i64,
    #[serde(default)]
    pub max_seek_ticks: i64,
    #[serde(default)]
    pub position_ticks: i64,
    #[serde(default)]
    pub last_updated_unix_ms: i64,

    // ── 媒体属性 ────────────────────────────────────────────────
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album_title: String,
    #[serde(default)]
    pub album_artist: String,
    #[serde(default)]
    pub track_number: i32,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub thumbnail_available: bool,

    // ── 播放控制（播放器允许的动作） ─────────────
    #[serde(default)]
    pub is_play_enabled: Option<bool>,
    #[serde(default)]
    pub is_pause_enabled: Option<bool>,
    #[serde(default)]
    pub is_stop_enabled: Option<bool>,
    #[serde(default)]
    pub is_next_enabled: Option<bool>,
    #[serde(default)]
    pub is_previous_enabled: Option<bool>,
    #[serde(default)]
    pub is_fast_forward_enabled: Option<bool>,
    #[serde(default)]
    pub is_rewind_enabled: Option<bool>,
    #[serde(default)]
    pub is_playback_rate_enabled: Option<bool>,
    #[serde(default)]
    pub is_shuffle_enabled: Option<bool>,
    #[serde(default)]
    pub is_repeat_enabled: Option<bool>,
    #[serde(default)]
    pub is_playback_position_enabled: Option<bool>,
}

/// 当前墙钟时间，单位 Unix 纪元毫秒。
pub fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 持久化锚点，用于让 SMTC 会话不报告可用时间线的播放器（例如网易云音乐
/// 上报 `Position=0` 和 `EndTime=0`，同时仍在刷新 `LastUpdatedTime`）的
/// 播放位置持续"走动"。桥接服务在两次原始 SMTC 采样之间依据该锚点
/// 外推 `position + (now - time) * rate`。
#[derive(Debug, Clone)]
pub struct PositionAnchor {
    /// 该锚点所属曲目的标识（`source|title|artist|album`）。
    pub track_key: String,
    /// 截至 `time_ms` 时的基准位置（毫秒）。
    pub position_ms: i64,
    /// `position_ms` 的 Unix 毫秒时间戳。
    pub time_ms: i64,
}

impl PositionAnchor {
    /// 为以 `rate`（0 = 冻结）播放、时长为 `duration_ms`（0 = 未知）的曲目，
    /// 外推 `now_ms`（unix 毫秒）时刻的实时位置。
    /// 结果被限制在 `[0, duration_ms]` 范围内。
    pub fn live_position_ms(&self, now_ms: i64, rate: f64, duration_ms: i64) -> i64 {
        let elapsed = (now_ms - self.time_ms).max(0) as f64;
        let live = (self.position_ms as f64 + elapsed * rate).max(0.0);
        if duration_ms > 0 {
            live.min(duration_ms as f64) as i64
        } else {
            live as i64
        }
    }
}

/// 返回 `status` 的一个副本，其 `position_ms` 使用最后一次 SMTC 位置采样、
/// 采样时间戳与播放速率外推到"当前时刻"。
///
/// 许多播放器（网易云音乐桌面版、播放网页视频的浏览器）只会偶尔推送一次
/// 位置快照，因此原始 SMTC `Position` 看起来像是冻结的。使用
/// `Position + (now - LastUpdatedTime) * rate` 可以让进度条在两次采样之间
/// 保持移动。暂停/停止时不进行任何外推，结果被限制在曲目时长内。
pub fn with_live_position(status: &SmtcStatus) -> SmtcStatus {
    let mut s = status.clone();
    if s.state == "Playing" && s.playback_rate > 0.0 && s.position_updated_at > 0 {
        let anchor = PositionAnchor {
            track_key: String::new(),
            position_ms: s.position_base_ms,
            time_ms: s.position_updated_at,
        };
        s.position_ms = anchor.live_position_ms(unix_now_ms(), s.playback_rate, s.duration_ms);
        s.position_live = true;
    } else {
        s.position_live = false;
    }
    s
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyricPosition {
    pub index: i32,
    #[serde(default)]
    pub at_ms: u64,
    #[serde(default)]
    pub next_at_ms: u64,
    #[serde(default)]
    pub current: String,
    #[serde(default)]
    pub next: String,
}

impl Default for LyricPosition {
    fn default() -> Self {
        Self {
            index: -1,
            at_ms: 0,
            next_at_ms: 0,
            current: String::new(),
            next: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyricResult {
    pub source: String,
    pub translation_line_count: usize,
    pub lines: Vec<LrcLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetaInfo {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub album: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub cover_url: String,
    #[serde(default)]
    pub artist: String,
}

pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct CacheEntry<T> {
    pub at: Instant,
    pub value: T,
}

impl<T> CacheEntry<T> {
    pub fn new(value: T) -> Self {
        Self {
            at: Instant::now(),
            value,
        }
    }

    pub fn is_fresh(&self, ttl_ms: u64) -> bool {
        self.at.elapsed().as_millis() < ttl_ms as u128
    }
}

/// 从 HashMap 缓存中清扫过期的条目。返回新的容量大小。
pub fn sweep_cache<K: std::cmp::Eq + std::hash::Hash, V>(
    cache: &mut HashMap<K, CacheEntry<V>>,
    ttl_ms: u64,
) -> usize {
    cache.retain(|_, entry| entry.is_fresh(ttl_ms));
    cache.len()
}

/// 带淘汰的插入：先清扫过期条目，若仍超出上限，则移除任意旧条目
/// 以保持在 max_entries 以内。
pub fn cache_insert_limited<K, V>(
    cache: &mut HashMap<K, CacheEntry<V>>,
    key: K,
    entry: CacheEntry<V>,
    ttl_ms: u64,
    max_entries: usize,
) where
    K: std::cmp::Eq + std::hash::Hash,
{
    // 首先，清扫过期条目。
    sweep_cache(cache, ttl_ms);
    // 若仍超出上限，则按插入时间淘汰最旧的条目，降至上限的 75%。
    if cache.len() >= max_entries {
        let target = max_entries * 3 / 4;
        let mut vec: Vec<_> = cache.drain().collect();
        vec.sort_by_key(|(_, entry)| entry.at);
        for (k, v) in vec.into_iter().take(target) {
            cache.insert(k, v);
        }
    }
    cache.insert(key, entry);
}

/// 每个独立缓存的最大条目数，防止内存无限制增长。
pub const MAX_CACHE_ENTRIES: usize = 512;

// ── 常量 ───────────────────────────────────────────────────────────────

pub const EDGE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";

// ── LRC 解析器 ──────────────────────────────────────────────────────────────

static LRC_TIMESTAMP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d{1,2}):(\d{2})(?:[.:](\d{1,3}))?\]").unwrap());

// ── 预编译正则表达式 ────────────────────────────────────────────────────

static ARTIST_SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*(?:/|&|,|，|;|；|\band\b|、)\s*").unwrap());
static HTML_DEC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(\d+);").unwrap());
static HTML_HEX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#x([0-9a-fA-F]+);").unwrap());
static QQMUSIC_SOURCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)qqmusic|tencent").unwrap());
static PLAYBACK_SUFFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*[-–—|]\s*(?:qq\s*music|qq音乐|腾讯音乐)\s*$").unwrap());
static TITLE_ARTIST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)\s+[-–—]\s+(.+)$").unwrap());

pub fn parse_lrc(raw: &str) -> Vec<LrcLine> {
    let mut lines: Vec<LrcLine> = Vec::new();
    for line in raw.lines() {
        // 去掉所有时间戳方括号，得到歌词文本。
        let text = LRC_TIMESTAMP_RE.replace_all(line, "").trim().to_string();
        if text.is_empty() {
            continue;
        }
        let mut has_stamps = false;
        for caps in LRC_TIMESTAMP_RE.captures_iter(line) {
            has_stamps = true;
            let min: u64 = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let sec: u64 = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let frac_str = caps.get(3).map(|m| m.as_str()).unwrap_or("0");
            let frac: u64 = format!("{:0<3}", frac_str)[..3].parse().unwrap_or(0);
            let at_ms = min * 60000 + sec * 1000 + frac;
            lines.push(LrcLine {
                at_ms,
                text: text.clone(),
            });
        }
        if !has_stamps {
            continue;
        }
    }
    lines.sort_by_key(|l| l.at_ms);
    lines
}

pub fn merge_translation(primary: &[LrcLine], translation: &[LrcLine]) -> Vec<LrcLine> {
    if primary.is_empty() || translation.is_empty() {
        return primary.to_vec();
    }
    let translated_by_time: HashMap<u64, &str> = translation
        .iter()
        .map(|l| (l.at_ms, l.text.as_str()))
        .collect();
    primary
        .iter()
        .map(|line| {
            if let Some(translated) = translated_by_time.get(&line.at_ms) {
                if *translated != line.text {
                    return LrcLine {
                        at_ms: line.at_ms,
                        text: format!("{} / {}", line.text, translated),
                    };
                }
            }
            line.clone()
        })
        .collect()
}

// ── 文本工具 ──────────────────────────────────────────────────────────

pub fn normalize_text(value: &str) -> String {
    value
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            '(' | ')' | '[' | ']' | '{' | '}' | '【' | '】' | '（' | '）' | '。' | '，' | '.'
            | ',' | '!' | '！' | '?' | '？' | '\'' | '"' => ' ',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn split_artists(value: &str) -> Vec<String> {
    ARTIST_SPLIT_RE
        .split(&normalize_text(value))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn search_score(
    song_name: &str,
    song_artists: &[String],
    song_duration: u64,
    track: &TrackInfo,
) -> i32 {
    let title = normalize_text(&track.title);
    let artist = normalize_text(&track.artist);
    let song_name_norm = normalize_text(song_name);

    let mut score = 0;

    if song_name_norm == title {
        score += 80;
    } else if song_name_norm.contains(&title) || title.contains(&song_name_norm) {
        score += 45;
    }

    // 预先归一化歌曲艺人，使每个艺人只被归一化一次。
    let song_artists_norm: Vec<String> = song_artists.iter().map(|a| normalize_text(a)).collect();

    // 匹配 JS 行为：对每个期望的艺人，至多加一次分
    // （对于该期望艺人，精确匹配优先于部分匹配）。
    for expected in split_artists(&artist) {
        let exact_match = song_artists_norm.contains(&expected);
        if exact_match {
            score += 30;
        } else {
            let partial_match = song_artists_norm
                .iter()
                .any(|actual| actual.contains(&expected) || expected.contains(actual));
            if partial_match {
                score += 15;
            }
        }
    }

    if song_duration > 0 && track.duration_ms > 0 {
        let delta = (song_duration as i64 - track.duration_ms as i64).unsigned_abs();
        if delta < 2500 {
            score += 15;
        } else if delta < 8000 {
            score += 8;
        }
    }

    score
}

pub fn decode_html(value: &str) -> String {
    let result = value
        .replace("\\n", "\n")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");

    let result = HTML_DEC_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let code: u32 = caps[1].parse().unwrap_or(0);
            char::from_u32(code).unwrap_or('\0').to_string()
        })
        .into_owned();

    HTML_HEX_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let code = u32::from_str_radix(&caps[1], 16).unwrap_or(0);
            char::from_u32(code).unwrap_or('\0').to_string()
        })
        .into_owned()
}

pub fn maybe_base64_text(value: &str) -> String {
    let text = value.trim();
    if text.is_empty() || text.contains('[') {
        return text.to_string();
    }
    // 检查它是否看起来像 base64
    if text.chars().any(|c| {
        !c.is_ascii_alphanumeric() && c != '+' && c != '/' && c != '=' && !c.is_whitespace()
    }) {
        return text.to_string();
    }
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(text) {
        Ok(decoded) => match String::from_utf8(decoded) {
            Ok(s) if s.contains('[') => s,
            _ => text.to_string(),
        },
        Err(_) => text.to_string(),
    }
}

pub fn lyric_at(lines: &[LrcLine], position_ms: u64) -> LyricPosition {
    // 二分查找 —— 歌词行按 at_ms 排序。
    let i = lines.partition_point(|line| line.at_ms <= position_ms);
    let index: i32 = if i > 0 { (i - 1) as i32 } else { -1 };

    LyricPosition {
        index,
        at_ms: if index >= 0 {
            lines[index as usize].at_ms
        } else {
            0
        },
        next_at_ms: if index >= 0 {
            let idx = index as usize;
            if idx + 1 < lines.len() {
                lines[idx + 1].at_ms
            } else {
                0
            }
        } else {
            0
        },
        current: if index >= 0 {
            lines[index as usize].text.clone()
        } else {
            String::new()
        },
        next: if index >= 0 {
            let idx = index as usize;
            if idx + 1 < lines.len() {
                lines[idx + 1].text.clone()
            } else {
                String::new()
            }
        } else {
            String::new()
        },
    }
}

pub fn is_qqmusic_source(source: &str) -> bool {
    QQMUSIC_SOURCE_RE.is_match(source)
}

pub fn strip_playback_suffix(value: &str) -> String {
    PLAYBACK_SUFFIX_RE
        .replace(value, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

pub fn infer_track_metadata(status: &mut SmtcStatus) {
    status.title = strip_playback_suffix(&status.title);
    status.artist = strip_playback_suffix(&status.artist);
    status.album = strip_playback_suffix(&status.album);

    if status.artist.is_empty() && is_qqmusic_source(&status.source) {
        let title_snapshot = status.title.clone();
        if let Some(caps) = TITLE_ARTIST_RE.captures(&title_snapshot) {
            status.title = strip_playback_suffix(&caps[1]);
            status.artist = strip_playback_suffix(&caps[2]);
        }
    }
}

/// 对字符串进行百分号编码（与 JS `encodeURIComponent` 逻辑相同）。
pub fn urlencoding(s: &str) -> String {
    let mut result = String::new();
    for byte in s.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(*byte as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_parse_timestamps() {
        let lines = parse_lrc("[00:01.50]first\n[00:03.000]second\n[01:00]minute");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].at_ms, 1500);
        assert_eq!(lines[0].text, "first");
        assert_eq!(lines[1].at_ms, 3000);
        assert_eq!(lines[1].text, "second");
        assert_eq!(lines[2].at_ms, 60000);
        assert_eq!(lines[2].text, "minute");
    }

    #[test]
    fn lrc_skips_untagged_lines() {
        let lines = parse_lrc("no timestamp here\n[00:00.00]tagged");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "tagged");
    }

    #[test]
    fn normalize_text_strips_punctuation() {
        assert_eq!(normalize_text("Hello (World)!"), "hello world");
        assert_eq!(normalize_text("  Multiple   Spaces  "), "multiple spaces");
    }

    #[test]
    fn split_artists_separates() {
        assert_eq!(split_artists("A / B & C"), vec!["a", "b", "c"]);
    }

    #[test]
    fn search_score_exact_title_artist() {
        let track = TrackInfo {
            title: "Tik Tok".to_string(),
            artist: "Kesha".to_string(),
            duration_ms: 200_000,
        };
        let artists = vec!["Kesha".to_string()];
        // 80 (title) + 30 (artist) + 15 (duration delta < 2500) = 125
        assert_eq!(search_score("Tik Tok", &artists, 201_000, &track), 125);
    }

    #[test]
    fn cache_insert_limited_evicts_over_limit() {
        let mut cache: HashMap<u64, CacheEntry<u64>> = HashMap::new();
        for i in 0..10 {
            cache_insert_limited(&mut cache, i, CacheEntry::new(i), 60_000, 4);
        }
        assert!(cache.len() <= 4);
    }

    #[test]
    fn live_position_extrapolates_and_clamps() {
        let anchor = PositionAnchor {
            track_key: String::new(),
            position_ms: 1_000,
            time_ms: 1_000,
        };
        assert_eq!(anchor.live_position_ms(6_000, 1.0, 0), 6_000);
        assert_eq!(anchor.live_position_ms(6_000, 1.0, 5_000), 5_000);
        assert_eq!(anchor.live_position_ms(6_000, 0.0, 0), 1_000);
        // 时钟回拨不能产生负的位置。
        assert_eq!(anchor.live_position_ms(500, 1.0, 0), 1_000);
        // 2 倍播放速率。
        assert_eq!(anchor.live_position_ms(3_000, 2.0, 0), 5_000);
    }

    #[test]
    fn with_live_position_freezes_when_paused() {
        let s = SmtcStatus {
            state: "Paused".to_string(),
            position_base_ms: 1_000,
            position_updated_at: 1_000,
            playback_rate: 0.0,
            position_ms: 1_000,
            ..Default::default()
        };
        let out = with_live_position(&s);
        assert!(!out.position_live);
        assert_eq!(out.position_ms, 1_000);
    }

    #[test]
    fn with_live_position_extrapolates_playing() {
        let s = SmtcStatus {
            state: "Playing".to_string(),
            position_base_ms: 1_000,
            position_updated_at: unix_now_ms() - 5_000,
            playback_rate: 1.0,
            duration_ms: 240_000,
            ..Default::default()
        };
        let out = with_live_position(&s);
        assert!(out.position_live);
        assert!(
            out.position_ms >= 5_000,
            "expected >= 5000, got {}",
            out.position_ms
        );
        assert!(out.position_ms <= 240_000);
    }

    #[test]
    fn lyric_at_finds_current_and_next() {
        let lines = vec![
            LrcLine {
                at_ms: 1_000,
                text: "a".to_string(),
            },
            LrcLine {
                at_ms: 2_000,
                text: "b".to_string(),
            },
        ];
        let pos = lyric_at(&lines, 1_500);
        assert_eq!(pos.index, 0);
        assert_eq!(pos.current, "a");
        assert_eq!(pos.next, "b");

        let before = lyric_at(&lines, 500);
        assert_eq!(before.index, -1);
        assert_eq!(before.current, "");
        assert_eq!(before.next, "");
    }

    #[test]
    fn merge_translation_appends_translated_text() {
        let primary = vec![LrcLine {
            at_ms: 1_000,
            text: "你好".to_string(),
        }];
        let translation = vec![LrcLine {
            at_ms: 1_000,
            text: "Hello".to_string(),
        }];
        let merged = merge_translation(&primary, &translation);
        assert_eq!(merged[0].text, "你好 / Hello");
    }

    #[test]
    fn urlencoding_percent_encodes() {
        assert_eq!(urlencoding("a b"), "a+b");
        assert_eq!(urlencoding("你"), "%E4%BD%A0");
        assert_eq!(urlencoding("a/b?c=d"), "a%2Fb%3Fc%3Dd");
    }
}
