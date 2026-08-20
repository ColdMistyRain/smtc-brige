use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;
use unicode_normalization::UnicodeNormalization;

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LrcLine {
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SmtcStatus {
    // Raw SMTC fields
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
    // ── Position extrapolation ─────────────────────────────────────────
    // The raw SMTC `Position` is only a snapshot that the player updates
    // sporadically (NetEase Cloud Music desktop / web video in browsers
    // update it rarely or not at all).  We keep the raw sample plus the
    // info needed to extrapolate a "live" position as time passes.
    #[serde(default)]
    pub position_base_ms: i64,
    /// Unix ms timestamp when `position_base_ms` was last reported by the
    /// player (SMTC `LastUpdatedTime`, or the time we sampled it).
    #[serde(default)]
    pub position_updated_at: i64,
    /// Effective playback rate used to extrapolate the live position
    /// (0 when not Playing, so no extrapolation happens).
    #[serde(default)]
    pub playback_rate: f64,
    /// True when `position_ms` is extrapolated (live) rather than a raw
    /// snapshot from the player.
    #[serde(default)]
    pub position_live: bool,
    /// How `position_ms` was obtained: `"smtc"` (a real sample from the
    /// player) or `"estimated"` (the bridge extrapolated from its own anchor
    /// because the player reports no usable timeline, e.g. NetEase Cloud
    /// Music reporting `Position=0`).
    #[serde(default)]
    pub position_source: String,
    pub session_count: i32,
    pub selected_current: bool,
    pub updated_at: i64,

    /// Every raw field reported by the SMTC session (Windows) / MPRIS player
    /// (Linux), exposed verbatim so consumers can see all data the bridge
    /// receives from the system transport controls.
    #[serde(default)]
    pub raw: RawSmtcInfo,

    // Enriched fields
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
    /// Full lyrics of the current track (all lines), filled by the background
    /// resolver — lets clients get the complete lyrics straight from `/status`
    /// without a second request.
    #[serde(default)]
    pub full_lyrics: Vec<LrcLine>,

    // Provider hints
    #[serde(default)]
    pub lyric_provider: String,
    #[serde(default)]
    pub lyric_id_text: String,
    #[serde(default)]
    pub cover_provider: String,
    #[serde(default)]
    pub cover_id_text: String,

    // QQ Music specific
    pub qq_song_id: i64,
    #[serde(default)]
    pub qq_song_mid: String,
    #[serde(default)]
    pub qq_album_mid: String,

    // Error fallback
    #[serde(default)]
    pub error: String,
}

/// Verbatim raw data from the system media transport controls session
/// (Windows SMTC) or the MPRIS player (Linux).  All values are exactly what
/// the OS/player reported; the bridge does not transform them here.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RawSmtcInfo {
    // ── Session ─────────────────────────────────────────────────────────
    #[serde(default)]
    pub source_app_user_model_id: String,

    // ── Playback info ───────────────────────────────────────────────────
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

    // ── Timeline (raw 100ns ticks, except `last_updated_unix_ms`) ───────
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

    // ── Media properties ────────────────────────────────────────────────
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

    // ── Playback controls (which actions the player allows) ─────────────
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

/// Current wall-clock time as Unix epoch milliseconds.
pub fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Persistent anchor used to keep the playback position "ticking" for players
/// whose SMTC session does not report a usable timeline (e.g. NetEase Cloud
/// Music reports `Position=0` and `EndTime=0` while still refreshing
/// `LastUpdatedTime`).  The bridge extrapolates `position + (now - time) * rate`
/// from this anchor between raw SMTC samples.
#[derive(Debug, Clone)]
pub struct PositionAnchor {
    /// Identity of the track this anchor belongs to (`source|title|artist|album`).
    pub track_key: String,
    /// Base position in ms as of `time_ms`.
    pub position_ms: i64,
    /// Unix ms timestamp of `position_ms`.
    pub time_ms: i64,
}

impl PositionAnchor {
    /// Extrapolate the live position at `now_ms` (unix ms) for a track playing
    /// at `rate` (0 = frozen) and lasting `duration_ms` (0 = unknown).
    /// The result is clamped to `[0, duration_ms]`.
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

/// Return a copy of `status` whose `position_ms` is extrapolated to "now"
/// using the last SMTC position sample, its timestamp and the playback rate.
///
/// Many players (NetEase Cloud Music desktop, browsers playing web video)
/// only push a position snapshot occasionally, so the raw SMTC `Position`
/// appears frozen.  Using `Position + (now - LastUpdatedTime) * rate` keeps
/// the progress bar moving between samples.  Nothing is extrapolated while
/// paused/stopped, and the result is clamped to the track duration.
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

/// Sweep expired entries from a HashMap cache. Returns the new size.
pub fn sweep_cache<K: std::cmp::Eq + std::hash::Hash, V>(
    cache: &mut HashMap<K, CacheEntry<V>>,
    ttl_ms: u64,
) -> usize {
    cache.retain(|_, entry| entry.is_fresh(ttl_ms));
    cache.len()
}

/// Insert with eviction: sweep expired, then if still over limit, remove
/// arbitrary old entries to stay under max_entries.
pub fn cache_insert_limited<K, V>(
    cache: &mut HashMap<K, CacheEntry<V>>,
    key: K,
    entry: CacheEntry<V>,
    ttl_ms: u64,
    max_entries: usize,
) where
    K: std::cmp::Eq + std::hash::Hash,
{
    // First, sweep expired entries.
    sweep_cache(cache, ttl_ms);
    // If still over limit, evict oldest (by insertion time) down to 75% of max.
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

/// Max entries per individual cache to prevent unbounded memory growth.
pub const MAX_CACHE_ENTRIES: usize = 512;

// ── Constants ───────────────────────────────────────────────────────────────

pub const EDGE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";

// ── LRC Parser ──────────────────────────────────────────────────────────────

static LRC_TIMESTAMP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d{1,2}):(\d{2})(?:[.:](\d{1,3}))?\]").unwrap());

// ── Precompiled Regexes ────────────────────────────────────────────────────

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
        // Strip all timestamp brackets to get the lyric text.
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

// ── Text Utilities ──────────────────────────────────────────────────────────

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

    // Pre-normalize song artists so each is only normalised once.
    let song_artists_norm: Vec<String> = song_artists.iter().map(|a| normalize_text(a)).collect();

    // Match JS behaviour: for each expected artist, add score at most once
    // (exact match takes precedence over partial for that expected artist).
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
    // Check if it looks like base64
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
    // Binary search — lines are sorted by at_ms.
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

/// Percent-encode a string (same logic in JS `encodeURIComponent`).
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
        // Clock going backwards must not produce a negative position.
        assert_eq!(anchor.live_position_ms(500, 1.0, 0), 1_000);
        // 2x playback rate.
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
