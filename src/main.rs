mod common;
mod netease;
mod qqmusic;
mod smtc;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, Method, StatusCode},
    response::{Html, Response},
    routing::get,
    Router,
};
use base64::Engine;
use serde::Deserialize;
use tokio::sync::Mutex;

use common::{
    infer_track_metadata, is_qqmusic_source, lyric_at, LyricPosition, SmtcStatus,
};
use netease::NeteaseSource;
use qqmusic::QQMusicSource;
use smtc::{resize_cover_jpeg, smtc_control, smtc_status_raw, smtc_thumbnail};

// ── Configuration ───────────────────────────────────────────────────────────

const HOST: &str = "0.0.0.0";
const PORT: u16 = 17865;
const CACHE_MS: u64 = 650;
const SEEK_MS: u64 = 15000;
const LYRIC_CACHE_MS: u64 = 6 * 60 * 60 * 1000;
const SEARCH_CACHE_MS: u64 = 60 * 60 * 1000;
const META_CACHE_MS: u64 = 6 * 60 * 60 * 1000;

const EDGE_UA: &str = common::EDGE_UA;

// ── Application State ───────────────────────────────────────────────────────

struct AppState {
    // Status cache
    status_cache: Mutex<Option<(Instant, SmtcStatus)>>,

    // Sources (each owns its own cache references)
    netease: NeteaseSource,
    qqmusic: QQMusicSource,

    // HTTP client for cover fetching
    http_client: reqwest::Client,
}

impl AppState {
    fn new() -> Self {
        let netease_lyric_cache = Arc::new(Mutex::new(HashMap::new()));
        let netease_search_cache = Arc::new(Mutex::new(HashMap::new()));
        let netease_meta_cache = Arc::new(Mutex::new(HashMap::new()));
        let qq_lyric_cache = Arc::new(Mutex::new(HashMap::new()));
        let qq_search_cache = Arc::new(Mutex::new(HashMap::new()));
        let qq_meta_cache = Arc::new(Mutex::new(HashMap::new()));

        let netease = NeteaseSource::new(
            netease_lyric_cache,
            netease_search_cache,
            netease_meta_cache,
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
        );

        let qqmusic = QQMusicSource::new(
            qq_lyric_cache,
            qq_search_cache,
            qq_meta_cache,
            LYRIC_CACHE_MS,
            SEARCH_CACHE_MS,
            META_CACHE_MS,
        );

        let http_client = reqwest::Client::builder()
            .user_agent(EDGE_UA)
            .timeout(std::time::Duration::from_secs(9))
            .build()
            .expect("reqwest client");

        Self {
            status_cache: Mutex::new(None),
            netease,
            qqmusic,
            http_client,
        }
    }
}

// ── SMTC Status Helpers ────────────────────────────────────────────────────

/// JS `sourceForStatus` uses Array.find() on [neteaseSource, qqMusicSource].
/// neteaseSource.matches always returns true and is first → always picks netease.
/// QQ Music source is only used when explicitly requested via query param.
fn source_for_status(_status: &SmtcStatus, _qqmusic: &QQMusicSource) -> &'static str {
    // Matching JS behaviour: neteaseSource.matches() always returns true,
    // and it's first in sourceAdapters, so find() always returns it.
    "netease"
}

/// Resolve provider aliases to canonical source names.
/// Matches JS `sourceByProvider` map: "qq"/"qqartist" -> "qqmusic".
fn resolve_provider(provider: &str) -> &str {
    match provider {
        "qq" | "qqartist" => "qqmusic",
        other => other,
    }
}

async fn enriched_status(state: &AppState, force: bool) -> SmtcStatus {
    let now = Instant::now();

    // Check cache
    if !force {
        let cache = state.status_cache.lock().await;
        if let Some((at, ref cached)) = *cache {
            if now.duration_since(at).as_millis() < CACHE_MS as u128 {
                return cached.clone();
            }
        }
    }

    // Fetch raw SMTC status
    match smtc_status_raw().await {
        Ok(raw) => {
            let mut status: SmtcStatus = raw.into();

            if status.connected {
                // Infer metadata (strip suffixes, etc.)
                infer_track_metadata(&mut status);

                // Determine SMTC adapter
                status.smtc_adapter = if is_qqmusic_source(&status.source) {
                    "qqmusic".to_string()
                } else {
                    "generic".to_string()
                };

                // Resolve lyrics & meta via appropriate source
                let source_name = source_for_status(&status, &state.qqmusic);
                let (found, meta) = if source_name == "qqmusic" {
                    state.qqmusic.resolve(&mut status).await
                } else {
                    state.netease.resolve(&mut status).await
                };

                // Fill in album & duration from meta
                if status.album.is_empty() && !meta.album.is_empty() {
                    status.album = meta.album;
                }
                if (status.duration_ms <= 0) && meta.duration_ms > 0 {
                    status.duration_ms = meta.duration_ms as i64;
                }

                status.ncm_id_text = if status.ncm_id > 0 {
                    status.ncm_id.to_string()
                } else {
                    String::new()
                };
                status.cover_url = meta.cover_url;
                status.lyrics_available = !found.lines.is_empty();
                status.translation_line_count = found.translation_line_count;
                status.lyric_source = found.source;
                status.lyric = lyric_at(&found.lines, status.position_ms.max(0) as u64);
            } else {
                status.lyrics_available = false;
                status.lyric = LyricPosition::default();
            }

            let mut cache = state.status_cache.lock().await;
            *cache = Some((now, status.clone()));
            status
        }
        Err(e) => {
            let fallback = SmtcStatus {
                ok: false,
                connected: false,
                error: e,
                state: "error".to_string(),
                lyric: LyricPosition::default(),
                ..Default::default()
            };
            let mut cache = state.status_cache.lock().await;
            *cache = Some((now, fallback.clone()));
            fallback
        }
    }
}

// ── Cover Image Helpers ────────────────────────────────────────────────────

async fn fetch_cover_buffer(
    client: &reqwest::Client,
    url: &str,
    referer: &str,
) -> Result<(Vec<u8>, String), String> {
    let resp = client
        .get(url)
        .header("Referer", referer)
        .send()
        .await
        .map_err(|e| format!("cover request: {e}"))?;

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();

    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("cover body: {e}"))?;

    // Detect content type from magic bytes
    let detected_type = if body.len() >= 8
        && body[0] == 0x89
        && body[1] == 0x50
        && body[2] == 0x4e
        && body[3] == 0x47
    {
        "image/png".to_string()
    } else if body.len() >= 2 && body[0] == 0xff && body[1] == 0xd8 {
        "image/jpeg".to_string()
    } else {
        content_type
    };

    Ok((body.to_vec(), detected_type))
}

async fn fetch_first_buffer(
    client: &reqwest::Client,
    urls: &[String],
    referer: &str,
) -> Result<(Vec<u8>, String), String> {
    let mut last_error = String::from("image not found");
    for url in urls {
        match fetch_cover_buffer(client, url, referer).await {
            Ok(result) => return Ok(result),
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

// ── JSON Helpers ────────────────────────────────────────────────────────────

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
        .body(Body::from(body))
        .unwrap()
}

fn binary_response(body: Vec<u8>, content_type: &str, cache: bool) -> Response {
    let len = body.len();
    let cache_header = if cache {
        "public, max-age=86400"
    } else {
        "no-store"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CACHE_CONTROL, cache_header)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(body))
        .unwrap()
}

// ── HTML Page ───────────────────────────────────────────────────────────────

fn html_page() -> Html<&'static str> {
    Html(
        r#"<!doctype html><meta charset="utf-8"><title>SMTC Bridge</title>
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:760px;margin:32px auto;padding:0 16px;color:#172033}code{background:#f1f5f9;padding:2px 5px;border-radius:6px}button{min-height:36px;margin:3px 3px 3px 0}</style>
<h1>SMTC Bridge</h1>
<p>Endpoints: <code>GET /status</code>, <code>GET /lyrics?provider=...&id=...</code>, <code>GET /cover?provider=...&id=...</code>, <code>GET /control?action=playpause</code>.</p>
<p>Lyrics are loaded from InfLink-rs <code>NCM-{id}</code> metadata or from QQ Music SMTC title/artist matching.</p>
<p><button onclick="cmd('previous')">Prev</button><button onclick="cmd('playpause')">Play/Pause</button><button onclick="cmd('next')">Next</button><button onclick="cmd('seek_back')">-15s</button><button onclick="cmd('seek_forward')">+15s</button></p>
<pre id="out">Loading...</pre>
<script>
async function refresh(){out.textContent=JSON.stringify(await fetch('/status').then(r=>r.json()),null,2)}
async function cmd(action){await fetch('/control?action='+encodeURIComponent(action));refresh()}
refresh();setInterval(refresh,1500)
</script>"#,
    )
}

// ── Route Handlers ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct StatusQuery {
    fresh: Option<String>,
}

async fn handle_status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatusQuery>,
) -> Response {
    let force = params.fresh.as_deref() == Some("1");
    let status = enriched_status(&state, force).await;
    send_json(&status)
}

#[derive(Deserialize, Default)]
struct LyricsQuery {
    provider: Option<String>,
    id: Option<String>,
    ncm_id: Option<String>,
    songmid: Option<String>,
    fresh: Option<String>,
}

async fn handle_lyrics(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LyricsQuery>,
) -> Response {
    let result: Result<_, String> = async {
        let mut provider = params.provider.unwrap_or_default().to_lowercase();
        let mut id_text = params
            .id
            .or_else(|| params.ncm_id.clone())
            .unwrap_or_default();
        let mut song_mid = params.songmid.unwrap_or_default();

        if provider.is_empty() && params.ncm_id.is_some() {
            provider = "netease".to_string();
        }

        if id_text.is_empty() {
            let force = params.fresh.as_deref() == Some("1");
            let status = enriched_status(&state, force).await;
            // Match JS fallback: if lyric_provider is empty but ncm_id exists, default to netease.
            provider = if status.lyric_provider.is_empty() && status.ncm_id > 0 {
                "netease".to_string()
            } else {
                status.lyric_provider.clone()
            };
            // Match JS: lyric_id_text || ncm_id_text
            id_text = if status.lyric_id_text.is_empty() {
                status.ncm_id_text.clone()
            } else {
                status.lyric_id_text.clone()
            };
            song_mid.clone_from(&status.qq_song_mid);
        }

        // Resolve aliases (e.g. "qq" -> "qqmusic") — matches JS sourceForProvider.
        let canonical = resolve_provider(&provider);

        let found = if canonical == "qqmusic" {
            let song_id: u64 = id_text.parse().unwrap_or(0);
            state.qqmusic.fetch_lyrics(song_id, &song_mid).await
        } else {
            let ncm_id: u64 = id_text.parse().unwrap_or(0);
            state.netease.fetch_lyrics(ncm_id).await
        };

        Ok(serde_json::json!({
            "ok": true,
            "provider": canonical,
            "id": id_text,
            "ncm_id": if canonical == "netease" { id_text.parse::<u64>().unwrap_or(0) } else { 0 },
            "ncm_id_text": if canonical == "netease" { &id_text } else { "" },
            "source": found.source,
            "translation_line_count": found.translation_line_count,
            "line_count": found.lines.len(),
            "lines": found.lines,
        }))
    }
    .await;

    match result {
        Ok(value) => send_json(&value),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({"ok": false, "error": e, "lines": []}),
        ),
    }
}

#[derive(Deserialize, Default)]
struct CoverQuery {
    provider: Option<String>,
    id: Option<String>,
    ncm_id: Option<String>,
}

async fn handle_cover(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CoverQuery>,
) -> Response {
    // Use (StatusCode, String) error so we can distinguish 404 (not found)
    // from 500 (server error) — matching JS behaviour.
    let result: Result<Response, (StatusCode, String)> = async {
        let mut provider = params.provider.unwrap_or_default().to_lowercase();
        let id_text = params
            .id
            .or_else(|| params.ncm_id.clone())
            .unwrap_or_default();

        if provider.is_empty() && params.ncm_id.is_some() {
            provider = "netease".to_string();
        }

        if provider == "smtc" {
            // Get thumbnail from SMTC directly
            let thumb = smtc_thumbnail().await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            if !thumb.ok || thumb.base64.is_empty() {
                return Err((StatusCode::NOT_FOUND, thumb.error));
            }
            let body = base64::engine::general_purpose::STANDARD
                .decode(&thumb.base64)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("base64: {e}")))?;
            let content_type = if thumb.content_type.is_empty() {
                "image/jpeg"
            } else {
                &thumb.content_type
            };
            return Ok(binary_response(body, content_type, false));
        }

        // Resolve provider aliases ("qq"/"qqartist" -> "qqmusic")
        let canonical = resolve_provider(&provider);

        if canonical == "qqmusic" {
            let urls = state.qqmusic.cover_candidates(&id_text, &provider);
            if urls.is_empty() {
                return Err((StatusCode::NOT_FOUND, "cover not found".to_string()));
            }
            let (body, _content_type) =
                fetch_first_buffer(&state.http_client, &urls, "https://y.qq.com/")
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            // QQ Music covers need to be resized to device size (88x88)
            let resized = resize_cover_jpeg(&body, 88)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            return Ok(binary_response(resized, "image/jpeg", true));
        }

        // NetEase or generic (non-qqmusic providers like "generic", "netease" etc.)
        let cover_url = {
            let ncm_id: u64 = id_text.parse().unwrap_or(0);
            state.netease.cover_candidates(&ncm_id.to_string()).await
        };

        if cover_url.is_empty() {
            return Err((StatusCode::NOT_FOUND, "cover not found".to_string()));
        }

        let (body, content_type) =
            fetch_cover_buffer(&state.http_client, &cover_url, "https://music.163.com/")
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(binary_response(body, &content_type, true))
    }
    .await;

    match result {
        Ok(response) => response,
        Err((status, msg)) => json_response(
            status,
            &serde_json::json!({"ok": false, "error": msg}),
        ),
    }
}

#[derive(Deserialize)]
struct ControlQuery {
    action: Option<String>,
}

async fn handle_control(
    State(state): State<Arc<AppState>>,
    _method: Method,
    Query(params): Query<ControlQuery>,
) -> Response {
    let action = params.action.unwrap_or_else(|| "playpause".to_string());

    // Respond immediately, then perform the action in background
    let accepted = serde_json::json!({"ok": true, "accepted": true, "action": action});

    // Spawn background task for the actual SMTC control
    let state_clone = state.clone();
    let action_clone = action.clone();
    tokio::spawn(async move {
        match smtc_control(&action_clone, SEEK_MS).await {
            Ok(_) => {
                // Invalidate cache so next status request refreshes
                let mut cache = state_clone.status_cache.lock().await;
                *cache = None;
            }
            Err(e) => {
                let mut cache = state_clone.status_cache.lock().await;
                *cache = Some((
                    Instant::now(),
                    SmtcStatus {
                        ok: false,
                        connected: false,
                        error: e,
                        state: "error".to_string(),
                        lyric: LyricPosition::default(),
                        ..Default::default()
                    },
                ));
            }
        }
    });

    // JS returns 202 Accepted
    json_response(StatusCode::ACCEPTED, &accepted)
}

async fn handle_health() -> Response {
    send_json(&serde_json::json!({
        "ok": true,
        "service": "smtc-bridge",
        "lyric_sources": ["smtc-genres-ncm-id", "qqmusic"]
    }))
}

async fn handle_options() -> Response {
    // JS: sendJson(res, 204, {})
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type")
        .body(Body::empty())
        .unwrap()
}

async fn handle_not_found() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        &serde_json::json!({"ok": false, "error": "not found"}),
    )
}

/// Catch-all fallback: OPTIONS returns 204 (CORS preflight), everything
/// else returns 404 — matching JS behaviour.
async fn handle_catch_all(method: Method) -> Response {
    if method == Method::OPTIONS {
        handle_options().await
    } else {
        handle_not_found().await
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    env_logger::init();

    let state = Arc::new(AppState::new());

    let app = Router::new()
        .route("/", get(|| async { html_page() }))
        .route("/health", get(handle_health))
        .route("/status", get(handle_status))
        .route("/lyrics", get(handle_lyrics))
        .route("/cover", get(handle_cover))
        .route("/control", get(handle_control).post(handle_control))
        .fallback(axum::routing::any(handle_catch_all))
        .with_state(state);

    let addr = format!("{HOST}:{PORT}");
    println!("SMTC bridge listening on http://{addr}");
    println!("Lyrics: InfLink NCM-{{id}} -> NetEase, QQ Music SMTC -> QQ Music API");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

