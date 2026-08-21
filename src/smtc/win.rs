// 基于 windows-rs crate 的 Windows SMTC 实现。
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Foundation::IAsyncOperation;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties,
    GlobalSystemMediaTransportControlsSessionPlaybackInfo,
    GlobalSystemMediaTransportControlsSessionTimelineProperties,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};

use crate::common::{RawSmtcInfo, SmtcStatus};

fn playback_status_str(
    status: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus,
) -> &'static str {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as Ps;
    match *status {
        Ps::Playing => "Playing",
        Ps::Paused => "Paused",
        Ps::Stopped => "Stopped",
        Ps::Closed => "Closed",
        Ps::Changing => "Changing",
        _ => "Unknown",
    }
}

fn to_string_lossy(h: &HSTRING) -> String {
    h.to_string_lossy()
}

/// 将 Windows `FILETIME` 风格的时间戳（自 1601-01-01 UTC 起的 100ns 刻度，
/// 由 `DateTime::UniversalTime` 暴露）转换为 Unix 纪元毫秒。
fn filetime_to_unix_ms(ticks: i64) -> i64 {
    // 11644473600000 ms = 1601-01-01 与 1970-01-01 之间的偏移量。
    ticks / 10_000 - 11_644_473_600_000
}

/// 原样收集 SMTC 会话暴露的*每一个*原始字段。
#[allow(clippy::too_many_arguments)]
fn build_raw_info(
    candidate: &GlobalSystemMediaTransportControlsSession,
    playback: &Option<GlobalSystemMediaTransportControlsSessionPlaybackInfo>,
    timeline: &Option<GlobalSystemMediaTransportControlsSessionTimelineProperties>,
    props: &Option<GlobalSystemMediaTransportControlsSessionMediaProperties>,
    playback_status: &str,
) -> RawSmtcInfo {
    let mut raw = RawSmtcInfo {
        source_app_user_model_id: to_string_lossy(
            &candidate.SourceAppUserModelId().unwrap_or_default(),
        ),
        playback_status: playback_status.to_string(),
        ..Default::default()
    };

    if let Some(pb) = playback {
        // `MediaPlaybackType` / `MediaPlaybackAutoRepeatMode` 是透明的
        // i32 包装 —— 把已知取值映射为可读名称。
        raw.playback_type = pb
            .PlaybackType()
            .ok()
            .and_then(|r| r.Value().ok())
            .map(|v| match v.0 {
                1 => "Music".to_string(),
                2 => "Video".to_string(),
                3 => "Image".to_string(),
                n => format!("Unknown({n})"),
            })
            .unwrap_or_default();
        raw.auto_repeat_mode = pb
            .AutoRepeatMode()
            .ok()
            .and_then(|r| r.Value().ok())
            .map(|v| match v.0 {
                1 => "Track".to_string(),
                2 => "List".to_string(),
                n => format!("None({n})"),
            })
            .unwrap_or_default();
        raw.playback_rate = pb.PlaybackRate().ok().and_then(|r| r.Value().ok());
        raw.shuffle_active = pb.IsShuffleActive().ok().and_then(|r| r.Value().ok());

        if let Ok(controls) = pb.Controls() {
            raw.is_play_enabled = controls.IsPlayEnabled().ok();
            raw.is_pause_enabled = controls.IsPauseEnabled().ok();
            raw.is_stop_enabled = controls.IsStopEnabled().ok();
            raw.is_next_enabled = controls.IsNextEnabled().ok();
            raw.is_previous_enabled = controls.IsPreviousEnabled().ok();
            raw.is_fast_forward_enabled = controls.IsFastForwardEnabled().ok();
            raw.is_rewind_enabled = controls.IsRewindEnabled().ok();
            raw.is_playback_rate_enabled = controls.IsPlaybackRateEnabled().ok();
            raw.is_shuffle_enabled = controls.IsShuffleEnabled().ok();
            raw.is_repeat_enabled = controls.IsRepeatEnabled().ok();
            raw.is_playback_position_enabled = controls.IsPlaybackPositionEnabled().ok();
        }
    }

    if let Some(tl) = timeline {
        let tick = |r: windows::core::Result<windows::Foundation::TimeSpan>| {
            r.map(|x| x.Duration).unwrap_or(0)
        };
        raw.start_time_ticks = tick(tl.StartTime());
        raw.end_time_ticks = tick(tl.EndTime());
        raw.min_seek_ticks = tick(tl.MinSeekTime());
        raw.max_seek_ticks = tick(tl.MaxSeekTime());
        raw.position_ticks = tick(tl.Position());
        // 网易云上报 `LastUpdatedTime` = 0（FILETIME 纪元），转换后会得到
        // 一个很大的负 unix 值 —— 对"未设置"显示 0。
        raw.last_updated_unix_ms = tl
            .LastUpdatedTime()
            .ok()
            .map(|dt| {
                if dt.UniversalTime <= 0 {
                    0
                } else {
                    filetime_to_unix_ms(dt.UniversalTime)
                }
            })
            .unwrap_or(0);
    }

    if let Some(p) = props {
        raw.title = to_string_lossy(&p.Title().unwrap_or_default());
        raw.artist = to_string_lossy(&p.Artist().unwrap_or_default());
        raw.album_title = to_string_lossy(&p.AlbumTitle().unwrap_or_default());
        raw.album_artist = to_string_lossy(&p.AlbumArtist().unwrap_or_default());
        raw.track_number = p.TrackNumber().unwrap_or(0);
        raw.genres = if let Ok(g) = p.Genres() {
            g.into_iter().map(|x| to_string_lossy(&x)).collect()
        } else {
            vec![]
        };
        raw.thumbnail_available = p.Thumbnail().is_ok();
    }

    raw
}

static NCM_ID_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^NCM-(\d+)$").unwrap());

fn parse_ncm_id(genres: &[String]) -> i64 {
    for genre in genres {
        if let Some(caps) = NCM_ID_RE.captures(genre) {
            return caps[1].parse().unwrap_or(0);
        }
    }
    0
}

// ── 状态 ──────────────────────────────────────────────────────────────────

/// 等待单次 SMTC `TryGetMediaPropertiesAsync` 调用的最大时间。
const MEDIA_PROPS_TIMEOUT: Duration = Duration::from_secs(4);
/// 整个 SMTC 状态抓取（manager + 所有会话）的最大时间。
const SMTC_STATUS_TIMEOUT: Duration = Duration::from_secs(8);

/// 执行阻塞 SMTC 异步操作的工作线程数。
const PROPS_WORKERS: usize = 4;
/// 对媒体属性操作超时的会话停止探测多长时间
/// （永不完成的损坏会话不能一直占用一个工作线程）。
const HUNG_SESSION_COOLDOWN: Duration = Duration::from_secs(60);

type MediaProps = GlobalSystemMediaTransportControlsSessionMediaProperties;

struct PropsJob {
    op: IAsyncOperation<MediaProps>,
    reply: Sender<Option<MediaProps>>,
}

/// 一个执行阻塞 `IAsyncOperation::get()` 调用的小型固定 OS 线程池。
/// 每次调用都新建线程的话，每当损坏的 SMTC 会话永不完成其异步操作时，
/// 就会泄漏一个 OS 线程，因此所有调用都经由这个有界线程池，
/// 而不是创建无界线程。
struct PropsPool {
    inner: Arc<PropsInner>,
}

struct PropsInner {
    queue: StdMutex<VecDeque<PropsJob>>,
    cv: Condvar,
}

impl PropsPool {
    fn new() -> Self {
        let inner = Arc::new(PropsInner {
            queue: StdMutex::new(VecDeque::new()),
            cv: Condvar::new(),
        });
        for _ in 0..PROPS_WORKERS {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("smtc-props".to_string())
                .spawn(move || loop {
                    let job = {
                        let mut q = inner.queue.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(job) = q.pop_front() {
                                break job;
                            }
                            q = inner.cv.wait(q).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let result = job.op.get().ok();
                    let _ = job.reply.send(result);
                })
                .expect("spawn smtc-props worker");
        }
        Self { inner }
    }

    fn submit(&self, job: PropsJob) {
        let mut q = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(job);
        self.inner.cv.notify_one();
    }
}

static PROPS_POOL: LazyLock<PropsPool> = LazyLock::new(PropsPool::new);
/// 媒体属性调用最近超时的会话（app id -> 时间），跳过这些会话，
/// 而不是让它们永远占用工作线程。仅在 `HUNG_SESSION_COOLDOWN`
/// 过后才重新探测。
static HUNG_SESSIONS: LazyLock<StdMutex<HashMap<String, Instant>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// 以按会话超时的方式调用 `TryGetMediaPropertiesAsync`，避免一个过期
/// 会话阻塞整个循环。
fn try_get_media_properties(
    session: &GlobalSystemMediaTransportControlsSession,
) -> Option<MediaProps> {
    let app_id = to_string_lossy(&session.SourceAppUserModelId().unwrap_or_default());

    // 跳过最近挂起的会话 —— 不要再为它们提交任务。
    {
        let mut hung = HUNG_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        hung.retain(|_, at| at.elapsed() < HUNG_SESSION_COOLDOWN);
        if hung.contains_key(&app_id) {
            log::debug!("smtc: skipping known-hung session {app_id:?}");
            return None;
        }
    }

    let op = session.TryGetMediaPropertiesAsync().ok()?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    PROPS_POOL.submit(PropsJob {
        op,
        reply: reply_tx,
    });

    let result = reply_rx.recv_timeout(MEDIA_PROPS_TIMEOUT).ok().flatten();
    if result.is_none() {
        // 记住该会话，停止为其提交任务（线程数现在有界，但挂起的操作
        // 仍占用一个工作线程）。
        let mut hung = HUNG_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        hung.insert(app_id, Instant::now());
        log::warn!(
            "TryGetMediaPropertiesAsync timed out after {}s — blacklisting session",
            MEDIA_PROPS_TIMEOUT.as_secs()
        );
    }
    result
}

pub async fn smtc_status_raw() -> Result<SmtcStatus, String> {
    let result = tokio::time::timeout(SMTC_STATUS_TIMEOUT, async {
        tokio::task::spawn_blocking(|| {
                let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
                    .map_err(|e| format!("SMTC RequestAsync: {e}"))
                    .and_then(|op| op.get().map_err(|e| format!("SMTC get: {e}")))?;

                let current_session = manager.GetCurrentSession().ok();
                let sessions = manager
                    .GetSessions()
                    .map_err(|e| format!("SMTC GetSessions: {e}"))?;

                let count = sessions.Size().map_err(|e| format!("SMTC Size: {e}"))? as i32;
                if count == 0 {
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

                for candidate in sessions.into_iter() {
                    let props = try_get_media_properties(&candidate);

                    let playback = candidate.GetPlaybackInfo().ok();
                    let timeline = candidate.GetTimelineProperties().ok();

                    let is_current = current_session
                        .as_ref()
                        .map(|cs| cs == &candidate)
                        .unwrap_or(false);

                    let mut score = 0;
                    let playback_status = playback
                        .as_ref()
                        .and_then(|p| p.PlaybackStatus().ok())
                        .map(|s| playback_status_str(&s).to_string())
                        .unwrap_or_default();

                    if playback_status.contains("Playing") {
                        score += 3000;
                    }
                    // 有有效元数据的暂停/停止会话仍然有用 —— 相比没有任何属性的
                    // 会话，不要过于严厉地扣分。
                    if is_current {
                        score += 1200;
                    }

                    let (
                        title,
                        artist,
                        album,
                        album_artist,
                        track_number,
                        genres,
                        ncm_id,
                        duration_ms,
                    ) = if let Some(ref p) = props {
                        let t = to_string_lossy(&p.Title().unwrap_or_default());
                        let a = to_string_lossy(&p.Artist().unwrap_or_default());
                        let al = to_string_lossy(&p.AlbumTitle().unwrap_or_default());
                        let aa = to_string_lossy(&p.AlbumArtist().unwrap_or_default());
                        let tn = p.TrackNumber().unwrap_or(0);
                        let gs: Vec<String> = if let Ok(genres) = p.Genres() {
                            genres.into_iter().map(|g| to_string_lossy(&g)).collect()
                        } else {
                            vec![]
                        };
                        let nid = parse_ncm_id(&gs);
                        let dur = timeline
                            .as_ref()
                            .and_then(|tl| {
                                let e = tl.EndTime().ok()?.Duration;
                                let s = tl.StartTime().ok()?.Duration;
                                Some(((e - s) / 10000).max(0))
                            })
                            .unwrap_or(0);
                        (t, a, al, aa, tn, gs, nid, dur)
                    } else {
                        Default::default()
                    };

                    if ncm_id > 0 {
                        score += 1000;
                    }
                    if duration_ms > 0 {
                        score += 200;
                    }
                    if !album.is_empty() {
                        score += 80;
                    }
                    if !title.is_empty() {
                        score += 20;
                    }

                    if score > best_score {
                        let pos_ticks = timeline
                            .as_ref()
                            .and_then(|tl| tl.Position().ok())
                            .map(|ts| ts.Duration)
                            .unwrap_or(0);
                        let position_base_ms = (pos_ticks / 10000).max(0);

                        // SMTC `Position` 只是快照 —— 播放器偶尔更新它。
                        // 将其与 `LastUpdatedTime` 和 `PlaybackRate` 结合，
                        // 使桥接服务即便对不频繁推送更新的播放器
                        // （网易云音乐、浏览器网页视频）也能外推实时位置。
                        let last_updated_ms = timeline
                            .as_ref()
                            .and_then(|tl| tl.LastUpdatedTime().ok())
                            .map(|dt| filetime_to_unix_ms(dt.UniversalTime))
                            // 防止零值 / 未来时间戳（时钟偏移）。
                            .filter(|&t| t > 0 && t <= now_ms)
                            .unwrap_or(now_ms);

                        // `PlaybackRate` 位于播放信息（可为 null）上，而不在时间线属性上。
                        let raw_rate = playback
                            .as_ref()
                            .and_then(|pb| pb.PlaybackRate().ok())
                            .and_then(|r| r.Value().ok())
                            .unwrap_or(0.0);

                        // 仅在实际播放时外推；若播放中的播放器忘记上报速率，
                        // 则假定为 1.0。
                        let rate = if playback_status == "Playing" {
                            if raw_rate > 0.0 {
                                raw_rate
                            } else {
                                1.0
                            }
                        } else {
                            0.0
                        };

                        let position_ms = (position_base_ms as f64
                            + (now_ms - last_updated_ms).max(0) as f64 * rate)
                            .max(0.0) as i64;
                        let position_ms = if duration_ms > 0 {
                            position_ms.min(duration_ms)
                        } else {
                            position_ms
                        };

                        best_score = score;
                        let raw = build_raw_info(
                            &candidate,
                            &playback,
                            &timeline,
                            &props,
                            &playback_status,
                        );
                        best = Some(SmtcStatus {
                            ok: true,
                            connected: true,
                            source: to_string_lossy(
                                &candidate.SourceAppUserModelId().unwrap_or_default(),
                            ),
                            state: playback_status,
                            title,
                            artist,
                            album,
                            album_artist,
                            track_number,
                            genres,
                            ncm_id,
                            position_ms,
                            position_base_ms,
                            position_updated_at: last_updated_ms,
                            playback_rate: rate,
                            position_live: rate > 0.0,
                            duration_ms,
                            session_count: count,
                            selected_current: is_current,
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
                "SMTC status timed out after {}s — returning disconnected",
                SMTC_STATUS_TIMEOUT.as_secs()
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

// ── 控制 ─────────────────────────────────────────────────────────────────

const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 选择控制动作应作用于的会话。仅靠 `GetCurrentSession()` 并不可靠：
/// 一旦播放器暂停一段时间（或其 SMTC 会话过期），它会返回"无会话"。
/// 回退为扫描 `GetSessions()` 并选择最活跃的会话
/// （Playing > Paused > 任意）。
fn pick_control_session(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Result<GlobalSystemMediaTransportControlsSession, String> {
    if let Ok(session) = manager.GetCurrentSession() {
        return Ok(session);
    }

    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as Ps;
    let sessions = manager
        .GetSessions()
        .map_err(|e| format!("SMTC GetSessions: {e}"))?;
    let mut best: Option<GlobalSystemMediaTransportControlsSession> = None;
    let mut best_score: i32 = -999999;
    for candidate in sessions.into_iter() {
        let mut score = 0;
        if let Ok(pb) = candidate.GetPlaybackInfo() {
            if let Ok(status) = pb.PlaybackStatus() {
                match status {
                    Ps::Playing => score += 3000,
                    Ps::Paused => score += 1000,
                    _ => {}
                }
            }
        }
        if score > best_score {
            best_score = score;
            best = Some(candidate);
        }
    }
    best.ok_or_else(|| "no session".to_string())
}

pub async fn smtc_control(action: &str, seek_ms: u64) -> Result<(), String> {
    let action = action.to_string();
    let result = tokio::time::timeout(CONTROL_TIMEOUT, async {
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
                .map_err(|e| format!("SMTC RequestAsync: {e}"))?
                .get()
                .map_err(|e| format!("SMTC get: {e}"))?;
            let session = pick_control_session(&manager)?;

            match action.as_str() {
                "play" => {
                    session
                        .TryPlayAsync()
                        .map_err(|e| format!("play: {e}"))?
                        .get()
                        .map_err(|e| format!("play: {e}"))?;
                }
                "pause" => {
                    session
                        .TryPauseAsync()
                        .map_err(|e| format!("pause: {e}"))?
                        .get()
                        .map_err(|e| format!("pause: {e}"))?;
                }
                "playpause" | "toggle" => {
                    session
                        .TryTogglePlayPauseAsync()
                        .map_err(|e| format!("toggle: {e}"))?
                        .get()
                        .map_err(|e| format!("toggle: {e}"))?;
                }
                "next" => {
                    session
                        .TrySkipNextAsync()
                        .map_err(|e| format!("next: {e}"))?
                        .get()
                        .map_err(|e| format!("next: {e}"))?;
                }
                "previous" => {
                    session
                        .TrySkipPreviousAsync()
                        .map_err(|e| format!("prev: {e}"))?
                        .get()
                        .map_err(|e| format!("prev: {e}"))?;
                }
                "stop" => {
                    session
                        .TryStopAsync()
                        .map_err(|e| format!("stop: {e}"))?
                        .get()
                        .map_err(|e| format!("stop: {e}"))?;
                }
                "seek_forward" | "seek_back" => {
                    let timeline = session
                        .GetTimelineProperties()
                        .map_err(|e| format!("timeline: {e}"))?;
                    let pos = timeline
                        .Position()
                        .map_err(|e| format!("position: {e}"))?
                        .Duration;
                    let delta = (seek_ms as i64) * 10000;
                    let target = if action == "seek_forward" {
                        (pos + delta).max(0)
                    } else {
                        (pos - delta).max(0)
                    };
                    session
                        .TryChangePlaybackPositionAsync(target)
                        .map_err(|e| format!("seek: {e}"))?
                        .get()
                        .map_err(|e| format!("seek: {e}"))?;
                }
                _ => return Err(format!("unknown action: {action}")),
            }
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
                "SMTC control timed out after {}s",
                CONTROL_TIMEOUT.as_secs()
            );
            Err("control timed out".to_string())
        }
    }
}

// ── 缩略图 ───────────────────────────────────────────────────────────────

const THUMBNAIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

pub async fn smtc_thumbnail() -> Result<(Vec<u8>, String), String> {
    let result = tokio::time::timeout(THUMBNAIL_TIMEOUT, async {
        tokio::task::spawn_blocking(|| -> Result<(Vec<u8>, String), String> {
            let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
                .map_err(|e| format!("SMTC RequestAsync: {e}"))?
                .get()
                .map_err(|e| format!("SMTC get: {e}"))?;
            let current_session = manager.GetCurrentSession().ok();
            let sessions = manager
                .GetSessions()
                .map_err(|e| format!("SMTC GetSessions: {e}"))?;

            let mut best_thumbnail: Option<IRandomAccessStreamReference> = None;
            let mut best_score: i32 = -999999;

            for candidate in sessions.into_iter() {
                // 复用有界属性线程池 + 挂起会话熔断机制 ——
                // 这里直接调用 `op.get()` 会在损坏的会话上永久挂起。
                let props = try_get_media_properties(&candidate);
                let playback = candidate.GetPlaybackInfo().ok();
                let is_current = current_session
                    .as_ref()
                    .map(|cs| cs == &candidate)
                    .unwrap_or(false);

                let mut score = 0;
                if let Some(ref pb) = playback {
                    if let Ok(status) = pb.PlaybackStatus() {
                        if playback_status_str(&status) == "Playing" {
                            score += 3000;
                        }
                    }
                }
                if is_current {
                    score += 1200;
                }
                if let Some(ref p) = props {
                    if let Ok(thumb) = p.Thumbnail() {
                        score += 600;
                        if score > best_score {
                            best_score = score;
                            best_thumbnail = Some(thumb);
                        }
                    }
                }
            }

            let thumbnail = best_thumbnail.ok_or("thumbnail not found".to_string())?;
            let stream = thumbnail
                .OpenReadAsync()
                .map_err(|e| format!("OpenReadAsync: {e}"))?
                .get()
                .map_err(|e| format!("OpenReadAsync: {e}"))?;
            let content_type = to_string_lossy(&stream.ContentType().unwrap_or_default());
            let size = stream.Size().map_err(|e| format!("Size: {e}"))? as u32;

            let input_stream = stream
                .GetInputStreamAt(0)
                .map_err(|e| format!("GetInputStreamAt: {e}"))?;
            let reader = DataReader::CreateDataReader(&input_stream)
                .map_err(|e| format!("CreateDataReader: {e}"))?;
            reader
                .LoadAsync(size)
                .map_err(|e| format!("LoadAsync: {e}"))?
                .get()
                .map_err(|e| format!("LoadAsync: {e}"))?;

            let mut bytes = vec![0u8; size as usize];
            reader
                .ReadBytes(&mut bytes)
                .map_err(|e| format!("ReadBytes: {e}"))?;
            Ok((bytes, content_type))
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
                "SMTC thumbnail timed out after {}s",
                THUMBNAIL_TIMEOUT.as_secs()
            );
            Err("thumbnail fetch timed out".to_string())
        }
    }
}
