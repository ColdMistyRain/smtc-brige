// Windows SMTC implementation using the windows-rs crate.
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

/// Convert a Windows `FILETIME`-style timestamp (100ns ticks since
/// 1601-01-01 UTC, as exposed by `DateTime::UniversalTime`) to Unix epoch ms.
fn filetime_to_unix_ms(ticks: i64) -> i64 {
    // 11644473600000 ms = the offset between 1601-01-01 and 1970-01-01.
    ticks / 10_000 - 11_644_473_600_000
}

/// Collect *every* raw field the SMTC session exposes, verbatim.
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
        // `MediaPlaybackType` / `MediaPlaybackAutoRepeatMode` are transparent
        // i32 wrappers — map the well-known values to readable names.
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
        // NetEase reports `LastUpdatedTime` = 0 (FILETIME epoch), which
        // converts to a large negative unix value — show 0 for "not set".
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

// ── Status ──────────────────────────────────────────────────────────────────

/// Maximum time to wait for a single SMTC `TryGetMediaPropertiesAsync` call.
const MEDIA_PROPS_TIMEOUT: Duration = Duration::from_secs(4);
/// Maximum time for the entire SMTC status fetch (manager + all sessions).
const SMTC_STATUS_TIMEOUT: Duration = Duration::from_secs(8);

/// Number of worker threads that run blocking SMTC async ops.
const PROPS_WORKERS: usize = 4;
/// How long we stop probing a session whose media-properties op timed out
/// (a broken session that never completes must not keep occupying a worker).
const HUNG_SESSION_COOLDOWN: Duration = Duration::from_secs(60);

type MediaProps = GlobalSystemMediaTransportControlsSessionMediaProperties;

struct PropsJob {
    op: IAsyncOperation<MediaProps>,
    reply: Sender<Option<MediaProps>>,
}

/// A small, fixed pool of OS threads that execute blocking
/// `IAsyncOperation::get()` calls.  Creating a fresh thread per call would
/// leak an OS thread every time a broken SMTC session never completes its
/// async op, so all calls go through this bounded pool instead of spawning
/// unbounded threads.
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
/// Sessions whose media-properties call recently timed out (app id -> when),
/// so they are skipped instead of repeatedly occupying a worker forever.
/// Re-probed only after `HUNG_SESSION_COOLDOWN` elapses.
static HUNG_SESSIONS: LazyLock<StdMutex<HashMap<String, Instant>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Call `TryGetMediaPropertiesAsync` with a per-session timeout so one stale
/// session cannot block the whole loop.
fn try_get_media_properties(
    session: &GlobalSystemMediaTransportControlsSession,
) -> Option<MediaProps> {
    let app_id = to_string_lossy(&session.SourceAppUserModelId().unwrap_or_default());

    // Skip sessions that recently hung — don't keep submitting work for them.
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
        // Remember the session so we stop submitting work for it (the thread
        // count is bounded now, but the hung op still occupies a worker).
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
                    // Paused/Paused sessions with valid metadata are still useful —
                    // do not penalise them too harshly compared to sessions without
                    // any props.
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

                        // SMTC `Position` is only a snapshot — the player updates
                        // it sporadically.  Combine it with `LastUpdatedTime` and
                        // `PlaybackRate` so the bridge can extrapolate a live
                        // position even for players that don't push frequent
                        // updates (NetEase Cloud Music, web video in browsers).
                        let last_updated_ms = timeline
                            .as_ref()
                            .and_then(|tl| tl.LastUpdatedTime().ok())
                            .map(|dt| filetime_to_unix_ms(dt.UniversalTime))
                            // Guard against zero / future timestamps (clock skew).
                            .filter(|&t| t > 0 && t <= now_ms)
                            .unwrap_or(now_ms);

                        // `PlaybackRate` lives on the playback info (nullable),
                        // not on the timeline properties.
                        let raw_rate = playback
                            .as_ref()
                            .and_then(|pb| pb.PlaybackRate().ok())
                            .and_then(|r| r.Value().ok())
                            .unwrap_or(0.0);

                        // Only extrapolate while actually playing; if a playing
                        // player forgets to report a rate, assume 1.0.
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

// ── Control ─────────────────────────────────────────────────────────────────

const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Pick the session a control action should target.  `GetCurrentSession()`
/// alone is unreliable: once a player pauses for a while (or its SMTC session
/// goes stale) it reports "no session".  Fall back to scanning `GetSessions()`
/// and choosing the most active one (Playing > Paused > any).
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

// ── Thumbnail ───────────────────────────────────────────────────────────────

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
                // Reuse the bounded props pool + hung-session circuit breaker —
                // a bare `op.get()` here would hang forever on a broken session.
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
