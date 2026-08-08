use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, Method, StatusCode},
    response::{Html, Response},
};
use serde::Deserialize;

use crate::common::{
    infer_track_metadata, is_qqmusic_source, lyric_at, LyricPosition, SmtcStatus,
};
use crate::config::*;
use crate::state::AppState;
use crate::smtc::{resize_cover_jpeg, smtc_control, smtc_status_raw, smtc_thumbnail};

// ── Source Helpers ──────────────────────────────────────────────────────────

pub fn source_for_status(status: &SmtcStatus) -> &'static str {
    if is_qqmusic_source(&status.source) { "qqmusic" } else { "netease" }
}

pub fn resolve_provider(provider: &str) -> &str {
    match provider {
        "qq" | "qqartist" => "qqmusic",
        other => other,
    }
}

// ── Status Enrichment ───────────────────────────────────────────────────────

pub async fn enriched_status(state: &AppState, force: bool) -> SmtcStatus {
    let now = Instant::now();

    // ── cache hit (fast path, no lock contention) ────────────────────────
    if !force {
        let cache = state.status_cache.lock().await;
        if let Some((at, ref cached)) = *cache {
            if now.duration_since(at).as_millis() < CACHE_MS as u128 {
                log::debug!("status cache hit (age: {}ms)", now.duration_since(at).as_millis());
                return cached.clone();
            }
        }
    }

    // ── serialised fetch to avoid stampeding SMTC / lyrics APIs ────────
    let _guard = state.fetch_mutex.lock().await;

    // Double-check: another request may have already populated the cache
    // while we were waiting for the mutex.
    if !force {
        let cache = state.status_cache.lock().await;
        if let Some((at, ref cached)) = *cache {
            if now.duration_since(at).as_millis() < CACHE_MS as u128 {
                return cached.clone();
            }
        }
    }

    log::debug!("fetching SMTC status…");

    match smtc_status_raw().await {
        Ok(mut status) => {
            log::debug!("SMTC: source={} state={} title={:?} artist={:?} ncm_id={}",
                status.source, status.state, status.title, status.artist, status.ncm_id);

            if status.connected {
                infer_track_metadata(&mut status);
                status.smtc_adapter = if is_qqmusic_source(&status.source) {
                    "qqmusic".to_string()
                } else {
                    "generic".to_string()
                };

                let source_name = source_for_status(&status);
                log::debug!("resolving lyrics via source={source_name}…");
                let found = if source_name == "qqmusic" {
                    let (f, _) = state.qqmusic.resolve(&mut status).await;
                    if f.lines.is_empty() {
                        log::debug!("QQ Music found nothing — falling back to NetEase");
                        state.netease.resolve(&mut status).await.0
                    } else { f }
                } else {
                    state.netease.resolve(&mut status).await.0
                };

                log::debug!("lyrics: {} lines from {} (translation: {})",
                    found.lines.len(), found.source, found.translation_line_count);

                let cover_id = {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    status.title.hash(&mut h);
                    status.artist.hash(&mut h);
                    h.finish()
                };
                status.cover_provider = "smtc".to_string();
                status.cover_id_text = format!("{cover_id}");
                status.cover_url = format!("/cover?provider=smtc&id={cover_id}&size=96");
                status.ncm_id_text = if status.ncm_id > 0 { status.ncm_id.to_string() } else { String::new() };
                status.lyrics_available = !found.lines.is_empty();
                status.translation_line_count = found.translation_line_count;
                status.lyric_source = found.source;
                status.lyric = lyric_at(&found.lines, status.position_ms.max(0) as u64);

                // Persist as last-known-good for disconnected-fallback.
                {
                    let mut last = state.last_known_status.lock().await;
                    *last = Some(status.clone());
                }
            } else {
                // ── Disconnected → serve last-known-good if fresh enough ──
                status.lyrics_available = false;
                status.lyric = LyricPosition::default();

                let last = state.last_known_status.lock().await;
                if let Some(ref last_status) = *last {
                    log::warn!("SMTC disconnected — returning last-known status (title={:?})",
                        last_status.title);
                    let mut fallback = last_status.clone();
                    // Let the caller distinguish stale data.
                    // We reuse the `connected` field: keep it false so
                    // consumers can still react.
                    fallback.connected = false;
                    fallback.state = format!("{} (stale)", fallback.state);

                    let mut cache = state.status_cache.lock().await;
                    *cache = Some((now, fallback.clone()));
                    return fallback;
                }
            }

            let mut cache = state.status_cache.lock().await;
            *cache = Some((now, status.clone()));
            status
        }
        Err(e) => {
            log::error!("SMTC status failed: {e}");

            // Try returning last-known-good before giving up entirely.
            let last = state.last_known_status.lock().await;
            if let Some(ref last_status) = *last {
                log::warn!("SMTC error — returning last-known status as fallback");
                let mut fallback = last_status.clone();
                fallback.connected = false;
                fallback.ok = false;
                fallback.error = e;
                fallback.state = format!("{} (stale)", fallback.state);
                let mut cache = state.status_cache.lock().await;
                *cache = Some((now, fallback.clone()));
                return fallback;
            }

            let fallback = SmtcStatus {
                ok: false, connected: false, error: e, state: "error".to_string(),
                lyric: LyricPosition::default(), ..Default::default()
            };
            let mut cache = state.status_cache.lock().await;
            *cache = Some((now, fallback.clone()));
            fallback
        }
    }
}

// ── JSON / Binary Response Helpers ──────────────────────────────────────────

fn send_json<T: serde::Serialize>(value: &T) -> Response {
    json_response(StatusCode::OK, value)
}

fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type")
        .body(Body::from(body)).unwrap()
}

fn binary_response(body: Vec<u8>, content_type: &str, cache: bool) -> Response {
    let len = body.len();
    let cache_header = if cache { "public, max-age=86400" } else { "no-store" };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CACHE_CONTROL, cache_header)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(body)).unwrap()
}

pub fn html_page() -> Html<&'static str> {
    Html(r#"<!doctype html><meta charset="utf-8"><title>SMTC Bridge</title>
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:760px;margin:32px auto;padding:0 16px;color:#172033}code{background:#f1f5f9;padding:2px 5px;border-radius:6px}button{min-height:36px;margin:3px 3px 3px 0}</style>
<h1>SMTC Bridge</h1>
<p>Endpoints: <code>GET /status</code>, <code>GET /lyrics?provider=...&id=...</code>, <code>GET /cover?provider=...&id=...</code>, <code>GET /control?action=playpause</code>.</p>
<p><button onclick="cmd('previous')">Prev</button><button onclick="cmd('playpause')">Play/Pause</button><button onclick="cmd('next')">Next</button><button onclick="cmd('seek_back')">-15s</button><button onclick="cmd('seek_forward')">+15s</button></p>
<pre id="out">Loading...</pre>
<script>async function refresh(){out.textContent=JSON.stringify(await fetch('/status').then(r=>r.json()),null,2)}
async function cmd(action){await fetch('/control?action='+encodeURIComponent(action));refresh()}
refresh();setInterval(refresh,1500)</script>"#)
}

// ── Cover Helpers ───────────────────────────────────────────────────────────

async fn fetch_cover_buffer(client: &reqwest::Client, url: &str, referer: &str) -> Result<(Vec<u8>, String), String> {
    log::debug!("fetching cover: {url}");
    let resp = client.get(url)
        .header("Referer", referer)
        .header("Accept", "image/jpeg,image/png,image/*;q=0.8,*/*;q=0.5")
        .send().await.map_err(|e| format!("cover request: {e}"))?;

    let content_type = resp.headers().get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok()).unwrap_or("image/jpeg").to_string();
    let body = resp.bytes().await.map_err(|e| format!("cover body: {e}"))?;

    let detected_type = if body.len() >= 8 && body[0] == 0x89 && body[1] == 0x50 && body[2] == 0x4e && body[3] == 0x47 {
        "image/png".to_string()
    } else if body.len() >= 2 && body[0] == 0xff && body[1] == 0xd8 {
        "image/jpeg".to_string()
    } else { content_type };

    Ok((body.to_vec(), detected_type))
}

// ── Route Handlers ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct StatusQuery { pub fresh: Option<String> }

pub async fn handle_status(State(state): State<Arc<AppState>>, Query(params): Query<StatusQuery>) -> Response {
    let force = params.fresh.as_deref() == Some("1");
    send_json(&enriched_status(&state, force).await)
}

#[derive(Deserialize, Default)]
pub struct LyricsQuery {
    pub provider: Option<String>,
    pub id: Option<String>,
    pub ncm_id: Option<String>,
    pub songmid: Option<String>,
    pub fresh: Option<String>,
}

pub async fn handle_lyrics(State(state): State<Arc<AppState>>, Query(params): Query<LyricsQuery>) -> Response {
    let result: Result<_, String> = async {
        let mut provider = params.provider.unwrap_or_default().to_lowercase();
        let mut id_text = params.id.or_else(|| params.ncm_id.clone()).unwrap_or_default();
        let mut song_mid = params.songmid.unwrap_or_default();

        if provider.is_empty() && params.ncm_id.is_some() { provider = "netease".to_string(); }

        if id_text.is_empty() {
            let status = enriched_status(&state, params.fresh.as_deref() == Some("1")).await;
            provider = if status.lyric_provider.is_empty() && status.ncm_id > 0 {
                "netease".to_string()
            } else { status.lyric_provider.clone() };
            id_text = if status.lyric_id_text.is_empty() { status.ncm_id_text.clone() } else { status.lyric_id_text.clone() };
            song_mid.clone_from(&status.qq_song_mid);
        }

        let canonical = resolve_provider(&provider);
        let found = if canonical == "qqmusic" {
            state.qqmusic.fetch_lyrics(id_text.parse().unwrap_or(0), &song_mid).await
        } else {
            state.netease.fetch_lyrics(id_text.parse().unwrap_or(0)).await
        };

        Ok(serde_json::json!({
            "ok": true, "provider": canonical, "id": id_text,
            "ncm_id": if canonical == "netease" { id_text.parse::<u64>().unwrap_or(0) } else { 0 },
            "ncm_id_text": if canonical == "netease" { &id_text } else { "" },
            "source": found.source, "translation_line_count": found.translation_line_count,
            "line_count": found.lines.len(), "lines": found.lines,
        }))
    }.await;

    match result {
        Ok(v) => send_json(&v),
        Err(e) => json_response(StatusCode::INTERNAL_SERVER_ERROR, &serde_json::json!({"ok":false,"error":e,"lines":[]})),
    }
}

#[derive(Deserialize, Default)]
pub struct CoverQuery {
    pub provider: Option<String>,
    pub id: Option<String>,
    pub ncm_id: Option<String>,
    #[serde(default)] pub size: Option<u32>,
}

pub async fn handle_cover(State(state): State<Arc<AppState>>, Query(params): Query<CoverQuery>) -> Response {
    let result: Result<Response, (StatusCode, String)> = async {
        let mut provider = params.provider.unwrap_or_default().to_lowercase();
        let id_text = params.id.or_else(|| params.ncm_id.clone()).unwrap_or_default();
        if provider.is_empty() { provider = "smtc".to_string(); }

        if provider == "smtc" {
            let size = params.size.unwrap_or(COVER_SIZE_DEFAULT).clamp(COVER_SIZE_MIN, COVER_SIZE_MAX);
            log::debug!("cover: smtc thumbnail (size={size})");
            let now = Instant::now();
            {
                let cache = state.thumbnail_cache.lock().await;
                if let Some((at, ref body, ref ct)) = *cache {
                    if now.duration_since(at).as_millis() < THUMBNAIL_CACHE_MS as u128 {
                        return Ok(binary_response(body.clone(), ct, false));
                    }
                }
            }
            let (body, _) = smtc_thumbnail().await.map_err(|e| { log::error!("cover: smtc_thumbnail failed: {e}"); (StatusCode::NOT_FOUND, e) })?;
            let resized = resize_cover_jpeg(&body, size).map_err(|e| { log::error!("cover: resize failed: {e}"); (StatusCode::INTERNAL_SERVER_ERROR, e) })?;
            log::debug!("cover: OK, {} -> {} bytes", body.len(), resized.len());
            let mut cache = state.thumbnail_cache.lock().await;
            *cache = Some((now, resized.clone(), "image/jpeg".to_string()));
            return Ok(binary_response(resized, "image/jpeg", false));
        }

        let _canonical = resolve_provider(&provider);
        let cover_url = { let ncm_id: u64 = id_text.parse().unwrap_or(0); state.netease.cover_candidates(&ncm_id.to_string()).await };
        if cover_url.is_empty() { return Err((StatusCode::NOT_FOUND, "cover not found".to_string())); }
        let (body, ct) = fetch_cover_buffer(&state.http_client, &cover_url, "https://music.163.com/").await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(binary_response(body, &ct, true))
    }.await;

    match result { Ok(r) => r, Err((s, m)) => json_response(s, &serde_json::json!({"ok":false,"error":m})) }
}

#[derive(Deserialize)]
pub struct ControlQuery { pub action: Option<String> }

pub async fn handle_control(State(state): State<Arc<AppState>>, _method: Method, Query(params): Query<ControlQuery>) -> Response {
    let action = params.action.unwrap_or_else(|| "playpause".to_string());
    log::debug!("control action: {action}");
    let accepted = serde_json::json!({"ok": true, "accepted": true, "action": action});

    let state_clone = state.clone();
    let action_clone = action.clone();

    // Serialise control operations — SMTC does not handle concurrent
    // play/pause/seek gracefully.
    tokio::spawn(async move {
        let _guard = state_clone.control_lock.lock().await;
        match smtc_control(&action_clone, SEEK_MS).await {
            Ok(()) => {
                log::debug!("SMTC control {action_clone} OK");
                let mut cache = state_clone.status_cache.lock().await;
                *cache = None;
            }
            Err(e) => {
                log::error!("SMTC control {action_clone} failed: {e}");
                let mut cache = state_clone.status_cache.lock().await;
                *cache = Some((Instant::now(), SmtcStatus {
                    ok: false, connected: false, error: e, state: "error".to_string(),
                    lyric: LyricPosition::default(), ..Default::default()
                }));
            }
        }
    });

    json_response(StatusCode::ACCEPTED, &accepted)
}

pub async fn handle_health() -> Response {
    send_json(&serde_json::json!({"ok":true,"service":"smtc-bridge","lyric_sources":["smtc-genres-ncm-id","qqmusic"]}))
}

pub async fn handle_options() -> Response {
    Response::builder().status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type")
        .body(Body::empty()).unwrap()
}

pub async fn handle_not_found() -> Response {
    json_response(StatusCode::NOT_FOUND, &serde_json::json!({"ok":false,"error":"not found"}))
}

pub async fn handle_catch_all(method: Method) -> Response {
    if method == Method::OPTIONS { handle_options().await } else { handle_not_found().await }
}

pub async fn handle_shutdown() -> Response {
    log::info!("shutdown requested");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    send_json(&serde_json::json!({"ok":true,"message":"shutting down"}))
}
