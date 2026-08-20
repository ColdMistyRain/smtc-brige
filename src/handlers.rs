use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, Method, StatusCode},
    response::{Html, Response},
};
use serde::Deserialize;

use crate::common::{
    cache_insert_limited, infer_track_metadata, is_qqmusic_source, lyric_at, unix_now_ms,
    with_live_position, CacheEntry, LyricPosition, LyricResult, MetaInfo, PositionAnchor,
    SmtcStatus, MAX_CACHE_ENTRIES,
};
use crate::config::*;
use crate::smtc::{resize_cover_jpeg, smtc_control, smtc_status_raw, smtc_thumbnail};
use crate::source::MusicSource;
use crate::state::AppState;

// ── Source Helpers ──────────────────────────────────────────────────────────

pub fn source_for_status(status: &SmtcStatus) -> &'static str {
    if is_qqmusic_source(&status.source) {
        "qqmusic"
    } else {
        "netease"
    }
}

pub fn resolve_provider(provider: &str) -> &str {
    match provider {
        "qq" | "qqartist" => "qqmusic",
        other => other,
    }
}

/// How often the "SMTC disconnected/error" warning is re-emitted.  The
/// dashboard polls `/status` every 1.5s, so without throttling the log fills
/// up while the player stays disconnected.
const DISCONNECT_LOG_INTERVAL: Duration = Duration::from_millis(DISCONNECT_LOG_INTERVAL_MS);

/// Log `msg` at `level` at most once per `DISCONNECT_LOG_INTERVAL`; later
/// occurrences are downgraded to debug so polling doesn't spam the log.
fn throttled_log(last: &mut Option<Instant>, level: log::Level, msg: String) {
    let due = last.is_none_or(|t| t.elapsed() >= DISCONNECT_LOG_INTERVAL);
    if due {
        *last = Some(Instant::now());
        log::log!(level, "{msg}");
    } else {
        log::debug!("{msg}");
    }
}

// ── Position Estimation ─────────────────────────────────────────────────────

/// Identity string used to detect when the playing media changes.
fn position_track_key(status: &SmtcStatus) -> String {
    format!(
        "{}|{}|{}|{}",
        status.source, status.title, status.artist, status.album
    )
}

/// Keep the playback position moving for players whose SMTC timeline is
/// unreliable (NetEase Cloud Music reports `Position=0` / `EndTime=0` while
/// still refreshing `LastUpdatedTime`).
///
/// - A trustworthy raw sample (`position_base_ms > 0`) becomes the new anchor
///   (QQ Music etc.), so its real position is used directly.
/// - Otherwise the bridge keeps extrapolating from its own persistent anchor,
///   resetting to 0 only when the media identity changes.
/// - While paused/stopped the anchor is frozen, so resuming continues from
///   where it left off (no jump ahead).
/// - If the estimate reaches the track duration while still playing, the track
///   most likely looped — restart the clock.
fn maintain_position(mut status: SmtcStatus, anchor: &mut Option<PositionAnchor>) -> SmtcStatus {
    if !status.connected || status.state.is_empty() || status.state == "none" {
        return status;
    }

    let now_ms = unix_now_ms();
    let key = position_track_key(&status);

    // Effective extrapolation rate — only while actually playing.
    let rate = if status.state == "Playing" {
        if status.playback_rate > 0.0 {
            status.playback_rate
        } else {
            1.0
        }
    } else {
        0.0
    };

    // Load the anchor, resetting it when the media identity changed.
    let mut a = anchor.take().unwrap_or(PositionAnchor {
        track_key: key.clone(),
        position_ms: 0,
        time_ms: now_ms,
    });
    if a.track_key != key {
        log::debug!("position: track changed → new anchor at 0");
        a = PositionAnchor {
            track_key: key.clone(),
            position_ms: 0,
            time_ms: now_ms,
        };
    }

    let trustworthy = status.position_base_ms > 0;
    if trustworthy {
        // The player reported a real position — prefer it over our estimate.
        a = PositionAnchor {
            track_key: key.clone(),
            position_ms: status.position_base_ms,
            time_ms: status.position_updated_at.max(0),
        };
    } else if status.state != "Playing" {
        // Paused / stopped with no real sample → freeze the position.
        a.time_ms = now_ms;
    }

    // Live position from the anchor.
    let mut live = a.live_position_ms(now_ms, rate, status.duration_ms) as f64;

    // Track most likely looped → restart the clock.
    if status.state == "Playing" && status.duration_ms > 0 && live >= status.duration_ms as f64 {
        log::debug!("position: reached end while playing → looping anchor at 0");
        a = PositionAnchor {
            track_key: key.clone(),
            position_ms: 0,
            time_ms: now_ms,
        };
        live = 0.0;
    }

    status.position_base_ms = a.position_ms;
    status.position_updated_at = a.time_ms;
    status.playback_rate = rate;
    status.position_ms = live as i64;
    status.position_source = if trustworthy {
        "smtc".to_string()
    } else {
        "estimated".to_string()
    };
    status.position_live = rate > 0.0;

    *anchor = Some(a);
    status
}

// ── Status Enrichment ───────────────────────────────────────────────────────

pub async fn enriched_status(state: &Arc<AppState>, force: bool) -> SmtcStatus {
    let now = Instant::now();

    // ── cache hit (fast path, no lock contention) ────────────────────────
    if !force {
        let cache = state.status_cache.lock().await;
        if let Some((at, ref cached)) = *cache {
            if now.duration_since(at).as_millis() < CACHE_MS as u128 {
                log::debug!(
                    "status cache hit (age: {}ms)",
                    now.duration_since(at).as_millis()
                );
                // Extrapolate position so progress keeps moving between
                // raw SMTC samples even on cached responses.
                return with_live_position(cached);
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
                return with_live_position(cached);
            }
        }
    }

    log::debug!("fetching SMTC status…");

    match smtc_status_raw().await {
        Ok(mut status) => {
            log::debug!(
                "SMTC: source={} state={} title={:?} artist={:?} ncm_id={}",
                status.source,
                status.state,
                status.title,
                status.artist,
                status.ncm_id
            );

            // Use the live (extrapolated) position so lyrics and progress
            // reflect "now" rather than the last snapshot the player pushed.
            status = with_live_position(&status);

            if status.connected {
                infer_track_metadata(&mut status);
                status.smtc_adapter = if is_qqmusic_source(&status.source) {
                    "qqmusic".to_string()
                } else {
                    "generic".to_string()
                };
                let source_name = source_for_status(&status);

                // Maintain a persistent position anchor so progress keeps
                // moving for players that report no usable SMTC timeline.
                {
                    let mut anchor = state.position_anchor.lock().await;
                    status = maintain_position(status, &mut anchor);
                }

                status.ncm_id_text = if status.ncm_id > 0 {
                    status.ncm_id.to_string()
                } else {
                    String::new()
                };

                // Provider hints — computed locally, never via network here.
                status.lyric_provider = source_name.to_string();
                status.lyric_id_text = if source_name == "netease" {
                    status.ncm_id_text.clone()
                } else {
                    String::new()
                };

                // ── Lyrics ─────────────────────────────────────────────
                // Served from the background cache only.  Lyric resolution is
                // decoupled from `/status`: slow lyric APIs (e.g. QQ search
                // stalling ~5s) used to block the response and starve
                // short-timeout clients (ESP32) until the web dashboard's
                // polling warmed the caches.
                let track_key = position_track_key(&status);
                let cached_lyric: Option<(LyricResult, MetaInfo)> = {
                    let cache = state.lyric_cache.lock().await;
                    cache
                        .get(&track_key)
                        .filter(|e| e.is_fresh(LYRIC_CACHE_MS))
                        .map(|e| e.value.clone())
                };
                let (lyric, meta) = cached_lyric.clone().unwrap_or_else(|| {
                    (
                        LyricResult {
                            source: String::new(),
                            translation_line_count: 0,
                            lines: vec![],
                        },
                        MetaInfo::default(),
                    )
                });
                status.lyrics_available = !lyric.lines.is_empty();
                status.translation_line_count = lyric.translation_line_count;
                status.lyric_source = lyric.source;
                status.lyric = lyric_at(&lyric.lines, status.position_ms.max(0) as u64);

                // Cover identity: title+artist alone collide for tracks with
                // the same/empty title (this made e.g. 《模特》 show the
                // cached cover of another track).  Include source + album too,
                // which are already known at SMTC sample time — so the id is
                // stable across the background lyric resolution (a cover id
                // that changed after resolution made clients download & flash
                // the cover twice).
                let cover_id = {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    status.source.hash(&mut h);
                    status.title.hash(&mut h);
                    status.artist.hash(&mut h);
                    status.album.hash(&mut h);
                    h.finish()
                };
                status.cover_provider = "smtc".to_string();
                status.cover_id_text = format!("{cover_id}");
                status.cover_url = format!("/cover?provider=smtc&id={cover_id}&size=96");

                // Expose the resolved track id so clients (e.g. ESP32) can
                // build a `/lyrics` request with a real id.  The front-end has
                // no QQ song id (SMTC doesn't expose it) and NetEase may only
                // be known after a background search — both come back here
                // from the cached `MetaInfo`.
                if meta.id > 0 {
                    if source_name == "qqmusic" {
                        status.lyric_id_text = meta.id.to_string();
                    } else if status.ncm_id_text.is_empty() {
                        status.ncm_id_text = meta.id.to_string();
                    }
                }

                // Fill duration from cached metadata when the player doesn't
                // report one (NetEase reports EndTime=0).
                if status.duration_ms <= 0 && meta.duration_ms > 0 {
                    log::debug!(
                        "SMTC duration=0 — using cached duration {}ms",
                        meta.duration_ms
                    );
                    status.duration_ms = meta.duration_ms as i64;
                }

                // Only kick off a background resolution when there is no fresh
                // cached entry.  Empty results are cached too, so tracks that
                // genuinely have no lyrics are not re-resolved on every poll
                // (which kept `/status` returning "no lyrics" and clients
                // showing "Loading lyrics" forever).
                if cached_lyric.is_none() {
                    spawn_lyric_resolution(state, track_key, status.clone()).await;
                }

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
                    // Rate-limited warning — the dashboard polls every 1.5s.
                    {
                        let mut warn_at = state.disconnect_log_at.lock().await;
                        throttled_log(
                            &mut warn_at,
                            log::Level::Warn,
                            format!(
                                "SMTC disconnected — returning last-known status (title={:?})",
                                last_status.title
                            ),
                        );
                    }
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
            // Re-extrapolate with the freshest clock — the lyric fetch above
            // may have taken long enough for the position to drift.
            with_live_position(&status)
        }
        Err(e) => {
            // Rate-limited error — the dashboard polls every 1.5s.
            {
                let mut warn_at = state.disconnect_log_at.lock().await;
                throttled_log(
                    &mut warn_at,
                    log::Level::Error,
                    format!("SMTC status failed: {e}"),
                );
            }

            // Try returning last-known-good before giving up entirely.
            let last = state.last_known_status.lock().await;
            if let Some(ref last_status) = *last {
                {
                    let mut warn_at = state.disconnect_log_at.lock().await;
                    throttled_log(
                        &mut warn_at,
                        log::Level::Warn,
                        "SMTC error — returning last-known status as fallback".to_string(),
                    );
                }
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

/// Kick off a background lyric resolution for `track_key` unless one is
/// already in flight.  The result lands in `AppState::lyric_cache`, so the
/// next `/status` poll serves lyrics without blocking on slow lyric APIs
/// (e.g. QQ search stalling ~5s, which used to stall `/status` and starve
/// short-timeout clients such as ESP32).
async fn spawn_lyric_resolution(state: &Arc<AppState>, track_key: String, status: SmtcStatus) {
    // Dedup: only one in-flight resolution per track.
    {
        let mut fetching = state.lyric_fetching.lock().await;
        if fetching.contains(&track_key) {
            return;
        }
        fetching.insert(track_key.clone());
    }

    let state = state.clone();
    tokio::spawn(async move {
        let source_name = source_for_status(&status);
        let qq: Arc<dyn MusicSource> = state.qqmusic.clone();
        let ne: Arc<dyn MusicSource> = state.netease.clone();
        let chain: Vec<Arc<dyn MusicSource>> = if source_name == "qqmusic" {
            vec![qq, ne]
        } else {
            vec![ne]
        };

        let mut found = LyricResult {
            source: String::new(),
            translation_line_count: 0,
            lines: vec![],
        };
        let mut meta = MetaInfo::default();
        let mut working = status;
        for source in &chain {
            let (f, m) = source.resolve(&mut working).await;
            meta = m;
            found = f;
            if !found.lines.is_empty() {
                break;
            }
            log::debug!("{} found nothing — trying next source", source.name());
        }
        log::debug!(
            "background lyrics resolved: {} lines from {} (track={:?})",
            found.lines.len(),
            found.source,
            working.title
        );

        // Cache the result (even empty, to avoid re-resolving every poll).
        {
            let mut cache = state.lyric_cache.lock().await;
            cache_insert_limited(
                &mut cache,
                track_key.clone(),
                CacheEntry::new((found, meta)),
                LYRIC_CACHE_MS,
                MAX_CACHE_ENTRIES,
            );
        }
        state.lyric_fetching.lock().await.remove(&track_key);
    });
}

// ── JSON / Binary Response Helpers ──────────────────────────────────────────

fn send_json<T: serde::Serialize>(value: &T) -> Response {
    json_response(StatusCode::OK, value)
}

fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    // CORS headers are added centrally by `tower_http::cors::CorsLayer`.
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
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
    // CORS headers are added centrally by `tower_http::cors::CorsLayer`.
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CACHE_CONTROL, cache_header)
        .body(Body::from(body))
        .unwrap()
}

pub fn html_page() -> Html<&'static str> {
    Html(
        r#"<!doctype html><meta charset="utf-8"><title>SMTC Bridge</title>
<style>
body{font:15px/1.5 system-ui,sans-serif;max-width:760px;margin:32px auto;padding:0 16px;color:#172033}
code{background:#f1f5f9;padding:2px 5px;border-radius:6px}
button{min-height:36px;margin:3px 3px 3px 0;cursor:pointer}
#now{display:flex;gap:14px;align-items:center;margin:18px 0 6px}
#cover{width:96px;height:96px;border-radius:8px;object-fit:cover;background:#eef2f7}
#meta{min-width:0;flex:1}
#title{font-size:18px;font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
#sub{color:#5b6577;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
progress{width:100%;height:10px;accent-color:#1f6feb}
#time{display:flex;justify-content:space-between;font-variant-numeric:tabular-nums;color:#5b6577;margin-top:4px}
#state{display:inline-block;margin-top:10px;padding:2px 10px;border-radius:999px;font-size:12px;background:#eef2f7}
#state.playing{background:#dcfce7;color:#15803d}
#state.paused{background:#fef3c7;color:#b45309}
.badge{font-size:12px;color:#0b6bcb;background:#e7f1ff;border-radius:999px;padding:1px 8px;margin-left:6px}
#rawbox{margin:12px 0}
#rawbox summary{cursor:pointer;font-weight:600;margin-bottom:4px}
table#raw{border-collapse:collapse;width:100%;font-size:13px}
table#raw td{border:1px solid #e5eaf0;padding:3px 8px;vertical-align:top}
table#raw td:first-child{font-weight:600;width:38%;white-space:nowrap}
table#raw td:last-child{word-break:break-all}
</style>
<h1>SMTC Bridge</h1>
<p>Endpoints: <code>GET /status</code>, <code>GET /lyrics?provider=...&id=...</code>, <code>GET /cover?provider=...&id=...</code>, <code>GET /control?action=playpause</code>.</p>
<div id="now">
  <img id="cover" src="" alt="">
  <div id="meta">
    <div id="title">Loading…</div>
    <div id="sub"></div>
    <progress id="bar" value="0" max="100"></progress>
    <div id="time"><span id="cur">0:00</span><span id="dur">0:00</span></div>
    <div><span id="state"></span><span class="badge" id="live" hidden></span></div>
  </div>
</div>
<p><button onclick="cmd('previous')">Prev</button><button onclick="cmd('playpause')">Play/Pause</button><button onclick="cmd('next')">Next</button><button onclick="cmd('seek_back')">-15s</button><button onclick="cmd('seek_forward')">+15s</button></p>
<details id="rawbox"><summary>SMTC 原始数据（raw）</summary><table id="raw"></table></details>
<pre id="out">Loading...</pre>
<script>
const el=id=>document.getElementById(id);
function fmt(ms){ms=Math.max(0,ms|0);const s=Math.floor(ms/1000);return Math.floor(s/60)+':'+String(s%60).padStart(2,'0')}
function renderRaw(s){
  const r=s.raw||{}, rows=[
    ['sourceAppUserModelId',r.source_app_user_model_id],
    ['playbackStatus',r.playback_status],
    ['playbackType',r.playback_type],
    ['autoRepeatMode',r.auto_repeat_mode],
    ['playbackRate',r.playback_rate],
    ['shuffleActive',r.shuffle_active],
    ['startTime (100ns)',r.start_time_ticks],
    ['endTime (100ns)',r.end_time_ticks],
    ['minSeekTime (100ns)',r.min_seek_ticks],
    ['maxSeekTime (100ns)',r.max_seek_ticks],
    ['position (100ns)',r.position_ticks],
    ['lastUpdatedTime (unix ms)',r.last_updated_unix_ms],
    ['title',r.title],
    ['artist',r.artist],
    ['albumTitle',r.album_title],
    ['albumArtist',r.album_artist],
    ['trackNumber',r.track_number],
    ['genres',(r.genres||[]).join(', ')],
    ['thumbnailAvailable',r.thumbnail_available],
    ['isPlayEnabled',r.is_play_enabled],
    ['isPauseEnabled',r.is_pause_enabled],
    ['isStopEnabled',r.is_stop_enabled],
    ['isNextEnabled',r.is_next_enabled],
    ['isPreviousEnabled',r.is_previous_enabled],
    ['isFastForwardEnabled',r.is_fast_forward_enabled],
    ['isRewindEnabled',r.is_rewind_enabled],
    ['isPlaybackRateEnabled',r.is_playback_rate_enabled],
    ['isShuffleEnabled',r.is_shuffle_enabled],
    ['isRepeatEnabled',r.is_repeat_enabled],
    ['isPlaybackPositionEnabled',r.is_playback_position_enabled],
  ];
  el('raw').innerHTML=rows.map(([k,v])=>`<tr><td>${k}</td><td>${v===undefined?'':v}</td></tr>`).join('');
}
async function refresh(){
  const s=await fetch('/status').then(r=>r.json());
  document.title=s.title||'SMTC Bridge';
  el('title').textContent=s.title||'(no media)';
  el('sub').textContent=[s.artist,s.album].filter(Boolean).join(' — ');
  el('cover').src=s.cover_url?s.cover_url:'';
  const bar=el('bar'),cur=el('cur'),dur=el('dur');
  if(s.duration_ms>0){
    bar.max=s.duration_ms;bar.value=Math.min(s.position_ms,s.duration_ms);
    cur.textContent=fmt(s.position_ms);dur.textContent=fmt(s.duration_ms);
  } else {bar.max=1;bar.value=0;cur.textContent='';dur.textContent=''}
  const st=el('state');
  st.textContent=s.state||'';
  const stl=(s.state||'').toLowerCase();
  st.className=stl.includes('play')?'playing':stl.includes('paus')?'paused':'';
  const liveEl=el('live');
  liveEl.hidden=!s.position_live;
  liveEl.textContent=s.position_live?(s.position_source==='estimated'?'estimate':'smtc'):'';
  renderRaw(s);
  el('out').textContent=JSON.stringify(s,null,2);
}
async function cmd(action){await fetch('/control?action='+encodeURIComponent(action));refresh()}
refresh();setInterval(refresh,1500)
</script>"#,
    )
}

// ── Cover Helpers ───────────────────────────────────────────────────────────

async fn fetch_cover_buffer(
    client: &reqwest::Client,
    url: &str,
    referer: &str,
) -> Result<(Vec<u8>, String), String> {
    log::debug!("fetching cover: {url}");
    let resp = client
        .get(url)
        .header("Referer", referer)
        .header("Accept", "image/jpeg,image/png,image/*;q=0.8,*/*;q=0.5")
        .send()
        .await
        .map_err(|e| format!("cover request: {e}"))?;

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let body = resp.bytes().await.map_err(|e| format!("cover body: {e}"))?;

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

// ── Route Handlers ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct StatusQuery {
    pub fresh: Option<String>,
}

pub async fn handle_status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatusQuery>,
) -> Response {
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

pub async fn handle_lyrics(
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
            let status = enriched_status(&state, params.fresh.as_deref() == Some("1")).await;

            // Prefer the background-resolved lyrics for the current track.
            // The front-end never has the QQ song id/mid (SMTC doesn't expose
            // it), so falling back to `fetch_lyrics(0, "")` would always
            // return empty lyrics for QQ Music.
            let track_key = position_track_key(&status);
            let cached = {
                let cache = state.lyric_cache.lock().await;
                cache
                    .get(&track_key)
                    .filter(|e| e.is_fresh(LYRIC_CACHE_MS))
                    .map(|e| e.value.0.clone())
            };
            if let Some(found) = cached {
                return Ok(serde_json::json!({
                    "ok": true, "provider": "cache", "id": "",
                    "ncm_id": 0, "ncm_id_text": "",
                    "source": found.source, "translation_line_count": found.translation_line_count,
                    "line_count": found.lines.len(), "lines": found.lines,
                }));
            }

            provider = if status.lyric_provider.is_empty() && status.ncm_id > 0 {
                "netease".to_string()
            } else {
                status.lyric_provider.clone()
            };
            id_text = if status.lyric_id_text.is_empty() {
                status.ncm_id_text.clone()
            } else {
                status.lyric_id_text.clone()
            };
            song_mid.clone_from(&status.qq_song_mid);
        }

        let canonical = resolve_provider(&provider);
        let found = match state.sources.iter().find(|s| s.name() == canonical) {
            Some(source) => {
                source
                    .fetch_lyrics(id_text.parse().unwrap_or(0), &song_mid)
                    .await
            }
            None => LyricResult {
                source: String::new(),
                translation_line_count: 0,
                lines: vec![],
            },
        };

        Ok(serde_json::json!({
            "ok": true, "provider": canonical, "id": id_text,
            "ncm_id": if canonical == "netease" { id_text.parse::<u64>().unwrap_or(0) } else { 0 },
            "ncm_id_text": if canonical == "netease" { &id_text } else { "" },
            "source": found.source, "translation_line_count": found.translation_line_count,
            "line_count": found.lines.len(), "lines": found.lines,
        }))
    }
    .await;

    match result {
        Ok(v) => send_json(&v),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({"ok":false,"error":e,"lines":[]}),
        ),
    }
}

#[derive(Deserialize, Default)]
pub struct CoverQuery {
    pub provider: Option<String>,
    pub id: Option<String>,
    pub ncm_id: Option<String>,
    #[serde(default)]
    pub size: Option<u32>,
}

pub async fn handle_cover(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CoverQuery>,
) -> Response {
    let result: Result<Response, (StatusCode, String)> = async {
        let mut provider = params.provider.unwrap_or_default().to_lowercase();
        let id_text = params
            .id
            .or_else(|| params.ncm_id.clone())
            .unwrap_or_default();
        if provider.is_empty() {
            provider = "smtc".to_string();
        }

        if provider == "smtc" {
            let size = params
                .size
                .unwrap_or(COVER_SIZE_DEFAULT)
                .clamp(COVER_SIZE_MIN, COVER_SIZE_MAX);
            log::debug!("cover: smtc thumbnail (size={size})");
            // Key the thumbnail cache per cover id so switching tracks never
            // serves the previous track's thumbnail.
            let cache_key = format!("smtc:{id_text}:{size}");
            {
                let cache = state.thumbnail_cache.lock().await;
                if let Some(entry) = cache.get(&cache_key) {
                    if entry.is_fresh(THUMBNAIL_CACHE_MS) {
                        let (body, ct) = entry.value.clone();
                        return Ok(binary_response(body, &ct, false));
                    }
                }
            }
            let (body, _) = smtc_thumbnail().await.map_err(|e| {
                log::error!("cover: smtc_thumbnail failed: {e}");
                (StatusCode::NOT_FOUND, e)
            })?;
            let body_len = body.len();
            // Image decode + Lanczos resize + JPEG encode is CPU-heavy — run
            // it on the blocking pool instead of the async worker threads.
            let resized = tokio::task::spawn_blocking(move || resize_cover_jpeg(&body, size))
                .await
                .map_err(|e| {
                    log::error!("cover: resize task failed: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "cover resize failed".to_string(),
                    )
                })?
                .map_err(|e| {
                    log::error!("cover: resize failed: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, e)
                })?;
            log::debug!("cover: OK, {} -> {} bytes", body_len, resized.len());
            let mut cache = state.thumbnail_cache.lock().await;
            cache_insert_limited(
                &mut cache,
                cache_key,
                CacheEntry::new((resized.clone(), "image/jpeg".to_string())),
                THUMBNAIL_CACHE_MS,
                64,
            );
            return Ok(binary_response(resized, "image/jpeg", false));
        }

        let _canonical = resolve_provider(&provider);
        let cover_url = {
            let ncm_id: u64 = id_text.parse().unwrap_or(0);
            state.netease.cover_candidates(&ncm_id.to_string()).await
        };
        if cover_url.is_empty() {
            return Err((StatusCode::NOT_FOUND, "cover not found".to_string()));
        }
        let (body, ct) =
            fetch_cover_buffer(&state.http_client, &cover_url, "https://music.163.com/")
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(binary_response(body, &ct, true))
    }
    .await;

    match result {
        Ok(r) => r,
        Err((s, m)) => json_response(s, &serde_json::json!({"ok":false,"error":m})),
    }
}

#[derive(Deserialize)]
pub struct ControlQuery {
    pub action: Option<String>,
}

pub async fn handle_control(
    State(state): State<Arc<AppState>>,
    _method: Method,
    Query(params): Query<ControlQuery>,
) -> Response {
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
                // Don't write an error status into the cache — that would make
                // the next /status poll report a fake playback error.  Just log
                // it; the next status fetch reflects the real player state.
                log::error!("SMTC control {action_clone} failed: {e}");
            }
        }
    });

    json_response(StatusCode::ACCEPTED, &accepted)
}

pub async fn handle_health() -> Response {
    send_json(
        &serde_json::json!({"ok":true,"service":"smtc-bridge","lyric_sources":["smtc-genres-ncm-id","qqmusic"]}),
    )
}

pub async fn handle_options() -> Response {
    // Preflight responses get their CORS headers from `CorsLayer`.
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

pub async fn handle_not_found() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        &serde_json::json!({"ok":false,"error":"not found"}),
    )
}

pub async fn handle_catch_all(method: Method) -> Response {
    if method == Method::OPTIONS {
        handle_options().await
    } else {
        handle_not_found().await
    }
}

pub async fn handle_shutdown(State(state): State<Arc<AppState>>) -> Response {
    log::info!("shutdown requested");
    // Signal `main` to drain connections and exit gracefully.
    let _ = state.shutdown.send(true);
    send_json(&serde_json::json!({"ok":true,"message":"shutting down"}))
}
