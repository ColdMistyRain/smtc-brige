// Linux MPRIS implementation via D-Bus.
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::Connection;

use crate::common::{RawSmtcInfo, SmtcStatus};

static NCM_ID_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^NCM-(\d+)$").unwrap());

// ── Helpers ─────────────────────────────────────────────────────────────────

fn variant_to_string(v: &dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>) -> String {
    v.0.as_str().map(|s| s.to_string()).unwrap_or_default()
}

fn variant_to_i64(v: &dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>, default: i64) -> i64 {
    v.0.as_i64().unwrap_or(default)
}

fn variant_to_string_list(v: &dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>) -> Vec<String> {
    if let Some(iter) = v.0.as_iter() {
        iter.filter_map(|a| a.as_str().map(|s| s.to_string()))
            .collect()
    } else {
        vec![]
    }
}

struct PlayerInfo {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    track_number: i32,
    duration_ms: i64,
    position_ms: i64,
    state: String,
    cover_url: String,
    genres: Vec<String>,
    ncm_id: i64,
}

fn read_player_info(conn: &Connection, bus_name: &str) -> Option<PlayerInfo> {
    let proxy = conn.with_proxy(
        bus_name,
        "/org/mpris/MediaPlayer2",
        Duration::from_millis(2000),
    );

    // Read PlaybackStatus
    let state: String = proxy
        .get("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
        .ok()?;

    // Read Metadata
    let metadata: HashMap<String, dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>> = proxy
        .get("org.mpris.MediaPlayer2.Player", "Metadata")
        .unwrap_or_default();

    let title = metadata
        .get("xesam:title")
        .map(variant_to_string)
        .unwrap_or_default();
    let artist_list = metadata
        .get("xesam:artist")
        .map(variant_to_string_list)
        .unwrap_or_default();
    let artist = artist_list.join(" / ");
    let album = metadata
        .get("xesam:album")
        .map(variant_to_string)
        .unwrap_or_default();
    let album_artist_list = metadata
        .get("xesam:albumArtist")
        .map(variant_to_string_list)
        .unwrap_or_default();
    let album_artist = album_artist_list.join(" / ");
    let track_number = metadata
        .get("xesam:trackNumber")
        .map(|v| variant_to_i64(v, 0) as i32)
        .unwrap_or(0);

    // mpris:length is in microseconds
    let duration_ms = metadata
        .get("mpris:length")
        .map(|v| variant_to_i64(v, 0) / 1000)
        .unwrap_or(0);

    let cover_url = metadata
        .get("mpris:artUrl")
        .map(variant_to_string)
        .unwrap_or_default();

    // Read Position
    let position_ms: i64 = proxy
        .get("org.mpris.MediaPlayer2.Player", "Position")
        .unwrap_or(0);
    // Position is in microseconds
    let position_ms = position_ms / 1000;

    // Extract NCM-{id} from genres (same as Windows)
    let genres: Vec<String> = metadata
        .get("xesam:genre")
        .map(variant_to_string_list)
        .unwrap_or_default();
    let ncm_id = {
        genres
            .iter()
            .find_map(|g| NCM_ID_RE.captures(g).and_then(|c| c[1].parse().ok()))
            .unwrap_or(0)
    };

    Some(PlayerInfo {
        title,
        artist,
        album,
        album_artist,
        track_number,
        duration_ms,
        position_ms,
        state,
        cover_url,
        genres,
        ncm_id,
    })
}

fn list_mpris_players(conn: &Connection) -> Vec<String> {
    let proxy = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Duration::from_millis(2000),
    );
    let names: Vec<String> = proxy
        .method_call("org.freedesktop.DBus", "ListNames", ())
        .map(|r: (Vec<String>,)| r.0)
        .unwrap_or_default();
    names
        .into_iter()
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
        .collect()
}

// ── Timeouts (mirror the Windows implementation) ────────────────────────────

const STATUS_TIMEOUT: Duration = Duration::from_secs(8);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(6);

// ── Status ──────────────────────────────────────────────────────────────────

pub async fn smtc_status_raw() -> Result<SmtcStatus, String> {
    let result = tokio::time::timeout(STATUS_TIMEOUT, async {
        tokio::task::spawn_blocking(|| -> Result<SmtcStatus, String> {
            let conn = Connection::new_session().map_err(|e| format!("D-Bus session: {e}"))?;

            let players = list_mpris_players(&conn);
            if players.is_empty() {
                return Ok(SmtcStatus {
                    ok: true,
                    connected: false,
                    state: "none".to_string(),
                    ..Default::default()
                });
            }

            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut best: Option<SmtcStatus> = None;
            let mut best_score: i32 = -999999;

            for bus_name in &players {
                let info = match read_player_info(&conn, bus_name) {
                    Some(i) => i,
                    None => continue,
                };

                let mut score: i32 = 0;
                if info.state == "Playing" {
                    score += 3000;
                }
                if info.ncm_id > 0 {
                    score += 1000;
                }
                if info.duration_ms > 0 {
                    score += 200;
                }
                if !info.album.is_empty() {
                    score += 80;
                }
                if !info.title.is_empty() {
                    score += 20;
                }
                // Prefer players with cover art
                if !info.cover_url.is_empty() {
                    score += 600;
                }

                if score > best_score {
                    best_score = score;
                    let playing = info.state == "Playing";
                    let raw = RawSmtcInfo {
                        source_app_user_model_id: bus_name.clone(),
                        playback_status: info.state.clone(),
                        title: info.title.clone(),
                        artist: info.artist.clone(),
                        album_title: info.album.clone(),
                        album_artist: info.album_artist.clone(),
                        track_number: info.track_number,
                        genres: info.genres.clone(),
                        thumbnail_available: !info.cover_url.is_empty(),
                        ..Default::default()
                    };
                    best = Some(SmtcStatus {
                        ok: true,
                        connected: true,
                        source: bus_name.clone(),
                        state: info.state,
                        title: info.title,
                        artist: info.artist,
                        album: info.album,
                        album_artist: info.album_artist,
                        track_number: info.track_number,
                        genres: info.genres,
                        ncm_id: info.ncm_id,
                        position_ms: info.position_ms,
                        // MPRIS `Position` is also a snapshot; record the sample
                        // time so `with_live_position` can keep progress moving.
                        position_base_ms: info.position_ms,
                        position_updated_at: now_ms,
                        playback_rate: if playing { 1.0 } else { 0.0 },
                        duration_ms: info.duration_ms,
                        session_count: players.len() as i32,
                        selected_current: false,
                        updated_at: now_ms,
                        raw,
                        ..Default::default()
                    });
                }
            }

            Ok(best.unwrap_or(SmtcStatus {
                ok: true,
                connected: false,
                state: "none".to_string(),
                ..Default::default()
            }))
        })
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
    })
    .await;

    match result {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => {
            log::warn!(
                "MPRIS status timed out after {}s — returning disconnected",
                STATUS_TIMEOUT.as_secs()
            );
            Ok(SmtcStatus {
                ok: true,
                connected: false,
                state: "timeout".to_string(),
                ..Default::default()
            })
        }
    }
}

// ── Control ─────────────────────────────────────────────────────────────────

pub async fn smtc_control(action: &str, seek_ms: u64) -> Result<(), String> {
    let action = action.to_string();
    let result = tokio::time::timeout(CONTROL_TIMEOUT, async {
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let conn = Connection::new_session().map_err(|e| format!("D-Bus session: {e}"))?;

            // Find a playing player, or fall back to any MPRIS player.
            let players = list_mpris_players(&conn);
            let target = players
                .iter()
                .find(|name| {
                    read_player_info(&conn, name)
                        .map(|i| i.state == "Playing")
                        .unwrap_or(false)
                })
                .or_else(|| players.first())
                .ok_or("no MPRIS player found".to_string())?
                .clone();

            let proxy = conn.with_proxy(
                &target,
                "/org/mpris/MediaPlayer2",
                Duration::from_millis(2000),
            );

            // 显式标注返回类型 R = ()，避免依赖 never-type fallback
            // （Rust 2024 中 `dependency_on_unit_never_type_fallback` 是硬错误）。
            let reply: Result<(), dbus::Error> = match action.as_str() {
                "play" => proxy.method_call("org.mpris.MediaPlayer2.Player", "Play", ()),
                "pause" => proxy.method_call("org.mpris.MediaPlayer2.Player", "Pause", ()),
                "playpause" | "toggle" => {
                    proxy.method_call("org.mpris.MediaPlayer2.Player", "PlayPause", ())
                }
                "next" => proxy.method_call("org.mpris.MediaPlayer2.Player", "Next", ()),
                "previous" => proxy.method_call("org.mpris.MediaPlayer2.Player", "Previous", ()),
                "stop" => proxy.method_call("org.mpris.MediaPlayer2.Player", "Stop", ()),
                "seek_forward" | "seek_back" => {
                    // MPRIS `Seek` takes a *relative* offset in microseconds,
                    // so `seek_back` must use a negative delta.
                    let delta = (seek_ms as i64) * 1000; // ms -> us
                    let signed = if action == "seek_back" { -delta } else { delta };
                    proxy.method_call("org.mpris.MediaPlayer2.Player", "Seek", (signed,))
                }
                _ => return Err(format!("unknown action: {action}")),
            };
            reply.map_err(|e| format!("MPRIS {action}: {e}"))?;

            Ok(())
        })
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => {
            log::warn!(
                "MPRIS control timed out after {}s",
                CONTROL_TIMEOUT.as_secs()
            );
            Err("control timed out".to_string())
        }
    }
}

// ── Thumbnail ───────────────────────────────────────────────────────────────

pub async fn smtc_thumbnail() -> Result<(Vec<u8>, String), String> {
    let result = tokio::time::timeout(THUMBNAIL_TIMEOUT, async {
        tokio::task::spawn_blocking(|| -> Result<(Vec<u8>, String), String> {
            let conn = Connection::new_session().map_err(|e| format!("D-Bus session: {e}"))?;

            let players = list_mpris_players(&conn);
            for bus_name in &players {
                if let Some(info) = read_player_info(&conn, bus_name) {
                    if !info.cover_url.is_empty() && info.state == "Playing" {
                        // The art URL is usually a local file:// or http(s):// URL.
                        // For http(s), we return the URL as a string for the caller to fetch.
                        if info.cover_url.starts_with("file://") {
                            let path = info
                                .cover_url
                                .strip_prefix("file://")
                                .unwrap_or(&info.cover_url);
                            let bytes =
                                std::fs::read(path).map_err(|e| format!("read file: {e}"))?;
                            return Ok((bytes, "image/jpeg".to_string()));
                        }
                        // Return URL as pseudo-content-type for caller to handle
                        return Err(format!("remote:{}", info.cover_url));
                    }
                }
            }
            Err("thumbnail not found".to_string())
        })
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
    })
    .await;

    match result {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => {
            log::warn!(
                "MPRIS thumbnail timed out after {}s",
                THUMBNAIL_TIMEOUT.as_secs()
            );
            Err("thumbnail fetch timed out".to_string())
        }
    }
}
