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
    pub session_count: i32,
    pub selected_current: bool,
    pub updated_at: i64,

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

// ── LRC Parser ──────────────────────────────────────────────────────────────

static LRC_TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[(\d{1,2}):(\d{2})(?:[.:](\d{1,3}))?\]").unwrap()
});

pub fn parse_lrc(raw: &str) -> Vec<LrcLine> {
    let mut lines: Vec<LrcLine> = Vec::new();
    for line in raw.lines() {
        let stamps: Vec<_> = LRC_TIMESTAMP_RE.captures_iter(line).collect();
        if stamps.is_empty() {
            continue;
        }
        let text = LRC_TIMESTAMP_RE.replace_all(line, "").trim().to_string();
        if text.is_empty() {
            continue;
        }
        for caps in stamps {
            let min: u64 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let sec: u64 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let frac_str = caps.get(3).map(|m| m.as_str()).unwrap_or("0");
            let frac: u64 = format!("{:0<3}", frac_str)[..3].parse().unwrap_or(0);
            let at_ms = min * 60000 + sec * 1000 + frac;
            lines.push(LrcLine {
                at_ms,
                text: text.clone(),
            });
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
    let re = Regex::new(r"\s*(?:/|&|,|，|;|；|\band\b|、)\s*").unwrap();
    re.split(&normalize_text(value))
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

    // Match JS behaviour: for each expected artist, add score at most once
    // (exact match takes precedence over partial for that expected artist).
    for expected in split_artists(&artist) {
        let exact_match = song_artists
            .iter()
            .any(|actual| normalize_text(actual) == expected);
        if exact_match {
            score += 30;
        } else {
            let partial_match = song_artists.iter().any(|actual| {
                let actual_norm = normalize_text(actual);
                actual_norm.contains(&expected) || expected.contains(&actual_norm)
            });
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
    let re_dec = Regex::new(r"&#(\d+);").unwrap();
    let re_hex = Regex::new(r"&#x([0-9a-fA-F]+);").unwrap();

    let result = value
        .replace("\\n", "\n")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");

    let result = re_dec
        .replace_all(&result, |caps: &regex::Captures| {
            let code: u32 = caps[1].parse().unwrap_or(0);
            char::from_u32(code).unwrap_or('\0').to_string()
        })
        .into_owned();

    re_hex
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
    if text.chars().any(|c| !c.is_ascii_alphanumeric() && c != '+' && c != '/' && c != '=' && !c.is_whitespace()) {
        return text.to_string();
    }
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(text) {
        Ok(decoded) => {
            match String::from_utf8(decoded) {
                Ok(s) if s.contains('[') => s,
                _ => text.to_string(),
            }
        }
        Err(_) => text.to_string(),
    }
}

pub fn lyric_at(lines: &[LrcLine], position_ms: u64) -> LyricPosition {
    let mut index: i32 = -1;
    for (i, line) in lines.iter().enumerate() {
        if line.at_ms <= position_ms {
            index = i as i32;
        } else {
            break;
        }
    }
    LyricPosition {
        index,
        at_ms: if index >= 0 { lines[index as usize].at_ms } else { 0 },
        next_at_ms: if (index as usize + 1) < lines.len() {
            lines[index as usize + 1].at_ms
        } else {
            0
        },
        current: if index >= 0 {
            lines[index as usize].text.clone()
        } else {
            String::new()
        },
        next: if (index as usize + 1) < lines.len() {
            lines[index as usize + 1].text.clone()
        } else {
            String::new()
        },
    }
}

pub fn is_qqmusic_source(source: &str) -> bool {
    let re = Regex::new(r"(?i)qqmusic|tencent").unwrap();
    re.is_match(source)
}

pub fn strip_playback_suffix(value: &str) -> String {
    let re = Regex::new(r"(?i)\s*[-–—|]\s*(?:qq\s*music|qq音乐|腾讯音乐)\s*$").unwrap();
    re.replace(value, "")
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
        let re = Regex::new(r"^(.+?)\s+[-–—]\s+(.+)$").unwrap();
        let title_snapshot = status.title.clone();
        if let Some(caps) = re.captures(&title_snapshot) {
            status.title = strip_playback_suffix(&caps[1]);
            status.artist = strip_playback_suffix(&caps[2]);
        }
    }
}
