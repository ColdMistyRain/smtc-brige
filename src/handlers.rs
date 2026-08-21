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

// ── 音乐源辅助函数 ──────────────────────────────────────────────────────────

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

/// "SMTC 断开/错误"警告的重新输出间隔。仪表盘每 1.5s 轮询一次 `/status`，
/// 若不限流，播放器一直断开时日志会很快刷满。
const DISCONNECT_LOG_INTERVAL: Duration = Duration::from_millis(DISCONNECT_LOG_INTERVAL_MS);

/// 在 `DISCONNECT_LOG_INTERVAL` 间隔内最多以 `level` 记录一次 `msg`；
/// 之后的记录降级为 debug，避免轮询刷屏日志。
fn throttled_log(last: &mut Option<Instant>, level: log::Level, msg: String) {
    let due = last.is_none_or(|t| t.elapsed() >= DISCONNECT_LOG_INTERVAL);
    if due {
        *last = Some(Instant::now());
        log::log!(level, "{msg}");
    } else {
        log::debug!("{msg}");
    }
}

// ── 位置估算 ─────────────────────────────────────────────────────

/// 用于检测播放媒体何时变化的标识字符串。
fn position_track_key(status: &SmtcStatus) -> String {
    format!(
        "{}|{}|{}|{}",
        status.source, status.title, status.artist, status.album
    )
}

/// 让 SMTC 时间线不可靠的播放器（网易云音乐上报 `Position=0` / `EndTime=0`，
/// 同时仍在刷新 `LastUpdatedTime`）的播放位置保持移动。
///
/// - 可信的原始采样（`position_base_ms > 0`）会成为新锚点（QQ 音乐等），
///   因此直接使用其真实位置。
/// - 否则桥接服务会持续依据自身的持久锚点外推，仅在媒体标识变化时归零。
/// - 暂停/停止时锚点冻结，因此恢复播放会从上次的位置继续（不会向前跳）。
/// - 若仍在播放时估算值到达曲目时长，说明曲目很可能已循环 —— 重新计时。
fn maintain_position(mut status: SmtcStatus, anchor: &mut Option<PositionAnchor>) -> SmtcStatus {
    if !status.connected || status.state.is_empty() || status.state == "none" {
        return status;
    }

    let now_ms = unix_now_ms();
    let key = position_track_key(&status);

    // 有效外推速率 —— 仅在实际播放时生效。
    let rate = if status.state == "Playing" {
        if status.playback_rate > 0.0 {
            status.playback_rate
        } else {
            1.0
        }
    } else {
        0.0
    };

    // 加载锚点，媒体标识变化时重置。
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
        // 播放器上报了真实位置 —— 优先使用它而非我们的估算值。
        a = PositionAnchor {
            track_key: key.clone(),
            position_ms: status.position_base_ms,
            time_ms: status.position_updated_at.max(0),
        };
    } else if status.state != "Playing" {
        // 暂停/停止且无真实采样 → 冻结位置。
        a.time_ms = now_ms;
    }

    // 依据锚点计算的实时位置。
    let mut live = a.live_position_ms(now_ms, rate, status.duration_ms) as f64;

    // 曲目很可能已循环 → 重新计时。
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

// ── 状态增强 ───────────────────────────────────────────────────────

pub async fn enriched_status(state: &Arc<AppState>, force: bool) -> SmtcStatus {
    let now = Instant::now();

    // ── 缓存命中（快速路径，无锁竞争） ────────────────────────
    if !force {
        let cache = state.status_cache.lock().await;
        if let Some((at, ref cached)) = *cache {
            if now.duration_since(at).as_millis() < CACHE_MS as u128 {
                log::debug!(
                    "status cache hit (age: {}ms)",
                    now.duration_since(at).as_millis()
                );
                // 外推位置，使缓存响应在两次原始 SMTC 采样之间进度仍持续移动。
                return with_live_position(cached);
            }
        }
    }

    // ── 串行化抓取，避免冲击 SMTC / 歌词 API ────────
    let _guard = state.fetch_mutex.lock().await;

    // 二次检查：等待互斥锁期间，另一个请求可能已经填充了缓存。
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

            // 使用实时（外推）位置，让歌词与进度反映"当前时刻"而非播放器
            // 推送的最后一次快照。
            status = with_live_position(&status);

            if status.connected {
                infer_track_metadata(&mut status);
                status.smtc_adapter = if is_qqmusic_source(&status.source) {
                    "qqmusic".to_string()
                } else {
                    "generic".to_string()
                };
                let source_name = source_for_status(&status);

                // 维护持久的位置锚点，让不报告可用 SMTC 时间线的播放器
                // 进度保持移动。
                {
                    let mut anchor = state.position_anchor.lock().await;
                    status = maintain_position(status, &mut anchor);
                }

                status.ncm_id_text = if status.ncm_id > 0 {
                    status.ncm_id.to_string()
                } else {
                    String::new()
                };

                // 提供商提示 —— 本地计算，此处绝不通过网络获取。
                status.lyric_provider = source_name.to_string();
                status.lyric_id_text = if source_name == "netease" {
                    status.ncm_id_text.clone()
                } else {
                    String::new()
                };

                // ── 歌词 ─────────────────────────────────────────────
                // 仅从后台缓存提供。歌词解析与 `/status` 解耦：慢速歌词 API
                // （例如 QQ 搜索卡住约 5s）曾导致响应阻塞、饿死短超时客户端
                // （ESP32），直到网页仪表盘的轮询预热了缓存。
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
                // 在 `/status` 上暴露完整歌词，让客户端（ESP32）一次请求拿到全部。
                status.full_lyrics = lyric.lines.clone();

                // 封面标识：仅用标题+歌手会对相同/空标题的曲目产生碰撞
                // （这曾导致例如《模特》显示另一首曲目的缓存封面）。
                // 额外包含源与专辑，这些在 SMTC 采样时已知 —— 因此该 id
                // 在后台歌词解析期间保持稳定（解析后变化的封面 id 曾使客户端
                // 下载并闪烁封面两次）。
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

                // 暴露已解析的曲目 id，让客户端（例如 ESP32）能用真实 id
                // 构造 `/lyrics` 请求。前端没有 QQ 歌曲 id（SMTC 不暴露），
                // 而网易云可能只在后台搜索后才得知 —— 两者都会从缓存中的
                // `MetaInfo` 回到这里。
                if meta.id > 0 {
                    if source_name == "qqmusic" {
                        status.lyric_id_text = meta.id.to_string();
                    } else if status.ncm_id_text.is_empty() {
                        status.ncm_id_text = meta.id.to_string();
                    }
                }

                // 当播放器不报告时长时（网易云上报 EndTime=0），从缓存的
                // 元数据填充。
                if status.duration_ms <= 0 && meta.duration_ms > 0 {
                    log::debug!(
                        "SMTC duration=0 — using cached duration {}ms",
                        meta.duration_ms
                    );
                    status.duration_ms = meta.duration_ms as i64;
                }

                // 仅在没有新鲜缓存条目时才启动后台解析。空结果也会被缓存，
                // 因此真正没有歌词的曲目不会在每次轮询时重复解析
                // （否则 `/status` 一直返回"无歌词"，客户端永远显示
                // "歌词加载中"）。
                if cached_lyric.is_none() {
                    spawn_lyric_resolution(state, track_key, status.clone()).await;
                }

                // 持久化为"最后已知可用"状态，供断开时回退。
                {
                    let mut last = state.last_known_status.lock().await;
                    *last = Some(status.clone());
                }
            } else {
                // ── 断开连接 → 若足够新鲜则返回最后已知可用状态 ──
                status.lyrics_available = false;
                status.lyric = LyricPosition::default();

                let last = state.last_known_status.lock().await;
                if let Some(ref last_status) = *last {
                    // 限流警告 —— 仪表盘每 1.5s 轮询一次。
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
                    // 让调用方能够区分过期数据。
                    // 复用 `connected` 字段：保持为 false，
                    // 使消费者仍能据此做出反应。
                    fallback.connected = false;
                    fallback.state = format!("{} (stale)", fallback.state);

                    let mut cache = state.status_cache.lock().await;
                    *cache = Some((now, fallback.clone()));
                    return fallback;
                }
            }

            let mut cache = state.status_cache.lock().await;
            *cache = Some((now, status.clone()));
            // 用最新的时钟重新外推 —— 上面的歌词抓取可能耗时较长，
            // 导致位置发生漂移。
            with_live_position(&status)
        }
        Err(e) => {
            // 限流错误 —— 仪表盘每 1.5s 轮询一次。
            {
                let mut warn_at = state.disconnect_log_at.lock().await;
                throttled_log(
                    &mut warn_at,
                    log::Level::Error,
                    format!("SMTC status failed: {e}"),
                );
            }

            // 彻底放弃前，先尝试返回"最后已知可用"状态。
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

/// 为 `track_key` 启动一次后台歌词解析，除非已有一个正在执行。结果会写入
/// `AppState::lyric_cache`，因此下一次 `/status` 轮询即可直接返回歌词，
/// 无需阻塞在慢速歌词 API 上（例如 QQ 搜索卡住约 5s，曾导致 `/status`
/// 卡顿并饿死 ESP32 等短超时客户端）。
async fn spawn_lyric_resolution(state: &Arc<AppState>, track_key: String, status: SmtcStatus) {
    // 去重：每首曲目只有一个正在执行的解析任务。
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

        // 缓存结果（即使为空也要缓存，避免每次轮询都重新解析）。
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

// ── JSON / 二进制响应辅助函数 ──────────────────────────────────────────

fn send_json<T: serde::Serialize>(value: &T) -> Response {
    json_response(StatusCode::OK, value)
}

fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    // CORS 响应头由 `tower_http::cors::CorsLayer` 集中添加。
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
    // CORS 响应头由 `tower_http::cors::CorsLayer` 集中添加。
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

// ── 封面辅助函数 ───────────────────────────────────────────────────────────

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

// ── 路由处理器 ──────────────────────────────────────────────────────────

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

            // 优先使用当前曲目已后台解析的歌词。
            // 前端永远没有 QQ 歌曲 id/mid（SMTC 不暴露它），
            // 因此回退到 `fetch_lyrics(0, "")` 对 QQ 音乐总会返回空歌词。
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

/// 返回*当前正在播放*曲目的完整歌词，无需任何参数。提供后台解析的结果；
/// 当解析尚未完成时返回 `loading: true`（调用方可稍后重试）。
#[derive(Deserialize, Default)]
pub struct LyricsNowQuery {
    pub fresh: Option<String>,
}

pub async fn handle_lyrics_now(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LyricsNowQuery>,
) -> Response {
    let result: Result<_, String> = async {
        let status = enriched_status(&state, params.fresh.as_deref() == Some("1")).await;
        let track_key = position_track_key(&status);
        let cached = {
            let cache = state.lyric_cache.lock().await;
            cache
                .get(&track_key)
                .filter(|e| e.is_fresh(LYRIC_CACHE_MS))
                .map(|e| e.value.0.clone())
        };
        match cached {
            Some(found) => Ok(serde_json::json!({
                "ok": true,
                "loading": false,
                "source": found.source,
                "translation_line_count": found.translation_line_count,
                "line_count": found.lines.len(),
                "lines": found.lines,
            })),
            None => Ok(serde_json::json!({
                "ok": true,
                "loading": true,
                "source": "",
                "translation_line_count": 0,
                "line_count": 0,
                "lines": [],
            })),
        }
    }
    .await;

    match result {
        Ok(v) => send_json(&v),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({"ok":false,"error":e,"lines":[],"loading":true}),
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
            // 按封面 id 键控缩略图缓存，使切歌时绝不会返回上一首歌的缩略图。
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
            // 图片解码 + Lanczos 缩放 + JPEG 编码属于 CPU 密集型 —— 应在阻塞
            // 线程池上执行，而非异步工作线程。
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

    // 串行化控制操作 —— SMTC 不能优雅地处理并发的播放/暂停/快进快退。
    tokio::spawn(async move {
        let _guard = state_clone.control_lock.lock().await;
        match smtc_control(&action_clone, SEEK_MS).await {
            Ok(()) => {
                log::debug!("SMTC control {action_clone} OK");
                let mut cache = state_clone.status_cache.lock().await;
                *cache = None;
            }
            Err(e) => {
                // 不要把错误状态写入缓存 —— 否则下一次 /status 轮询会报告
                // 虚假的播放错误。仅记录日志；下一次状态抓取会反映真实的
                // 播放器状态。
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
    // 预检响应从 `CorsLayer` 获取其 CORS 响应头。
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
    // 通知 `main` 排空连接并优雅退出。
    let _ = state.shutdown.send(true);
    send_json(&serde_json::json!({"ok":true,"message":"shutting down"}))
}
