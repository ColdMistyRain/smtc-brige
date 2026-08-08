// Windows SMTC implementation using the windows-rs crate.
use windows::core::HSTRING;
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};

use crate::common::SmtcStatus;

fn playback_status_str(status: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus) -> &'static str {
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

fn parse_ncm_id(genres: &[String]) -> i64 {
    let re = regex::Regex::new(r"^NCM-(\d+)$").unwrap();
    for genre in genres {
        if let Some(caps) = re.captures(genre) {
            return caps[1].parse().unwrap_or(0);
        }
    }
    0
}

// ── Status ──────────────────────────────────────────────────────────────────

/// Maximum time to wait for a single SMTC `TryGetMediaPropertiesAsync` call.
const MEDIA_PROPS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
/// Maximum time for the entire SMTC status fetch (manager + all sessions).
const SMTC_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Call `TryGetMediaPropertiesAsync` with a per-session timeout so one stale
/// session cannot block the whole loop.
fn try_get_media_properties(
    session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession,
) -> Option<windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties> {
    let op = session.TryGetMediaPropertiesAsync().ok()?;
    // Run the blocking WinRT `get()` on a dedicated OS thread and wait with
    // a deadline.  If the thread hangs we abandon it (rare – only when the
    // session is in a broken state) but the status loop can continue.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(op.get().ok());
    });
    rx.recv_timeout(MEDIA_PROPS_TIMEOUT)
        .ok()
        .flatten()
        .or_else(|| {
            log::warn!(
                "TryGetMediaPropertiesAsync timed out after {}s — skipping session",
                MEDIA_PROPS_TIMEOUT.as_secs()
            );
            None
        })
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

                if playback_status.contains("Playing") { score += 3000; }
                // Paused/Paused sessions with valid metadata are still useful —
                // do not penalise them too harshly compared to sessions without
                // any props.
                if is_current { score += 1200; }

                let (title, artist, album, album_artist, track_number, genres, ncm_id, duration_ms) =
                    if let Some(ref p) = props {
                        let t = to_string_lossy(&p.Title().unwrap_or_default());
                        let a = to_string_lossy(&p.Artist().unwrap_or_default());
                        let al = to_string_lossy(&p.AlbumTitle().unwrap_or_default());
                        let aa = to_string_lossy(&p.AlbumArtist().unwrap_or_default());
                        let tn = p.TrackNumber().unwrap_or(0);
                        let gs: Vec<String> =
                            if let Ok(genres) = p.Genres() {
                                genres.into_iter().map(|g| to_string_lossy(&g)).collect()
                            } else { vec![] };
                        let nid = parse_ncm_id(&gs);
                        let dur = timeline.as_ref().and_then(|tl| {
                            let e = tl.EndTime().ok()?.Duration;
                            let s = tl.StartTime().ok()?.Duration;
                            Some(((e - s) / 10000).max(0))
                        }).unwrap_or(0);
                        (t, a, al, aa, tn, gs, nid, dur)
                    } else {
                        Default::default()
                    };

                if ncm_id > 0 { score += 1000; }
                if duration_ms > 0 { score += 200; }
                if !album.is_empty() { score += 80; }
                if !title.is_empty() { score += 20; }

                if score > best_score {
                    let pos_ticks = timeline.as_ref()
                        .and_then(|tl| tl.Position().ok())
                        .map(|ts| ts.Duration).unwrap_or(0);
                    let position_ms = (pos_ticks / 10000).max(0);

                    best_score = score;
                    best = Some(SmtcStatus {
                        ok: true, connected: true,
                        source: to_string_lossy(&candidate.SourceAppUserModelId().unwrap_or_default()),
                        state: playback_status,
                        title, artist, album, album_artist, track_number,
                        genres, ncm_id,
                        position_ms,
                        duration_ms,
                        session_count: count,
                        selected_current: is_current,
                        updated_at: now_ms,
                        ..Default::default()
                    });
                }
            }

            Ok(best.unwrap_or(SmtcStatus {
                ok: true, connected: false,
                state: "none".to_string(),
                ..Default::default()
            }))
        }).await.map_err(|e| format!("spawn_blocking: {e}"))?
    }).await;

    match result {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => {
            log::warn!("SMTC status timed out after {}s — returning disconnected",
                SMTC_STATUS_TIMEOUT.as_secs());
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

pub async fn smtc_control(action: &str, seek_ms: u64) -> Result<(), String> {
    let action = action.to_string();
    let result = tokio::time::timeout(CONTROL_TIMEOUT, async {
        tokio::task::spawn_blocking(move || -> Result<(), String> {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .map_err(|e| format!("SMTC RequestAsync: {e}"))?
            .get().map_err(|e| format!("SMTC get: {e}"))?;
        let session = manager.GetCurrentSession().map_err(|_| "no session".to_string())?;

        match action.as_str() {
            "play" => { session.TryPlayAsync().map_err(|e| format!("play: {e}"))?.get().map_err(|e| format!("play: {e}"))?; }
            "pause" => { session.TryPauseAsync().map_err(|e| format!("pause: {e}"))?.get().map_err(|e| format!("pause: {e}"))?; }
            "playpause" | "toggle" => { session.TryTogglePlayPauseAsync().map_err(|e| format!("toggle: {e}"))?.get().map_err(|e| format!("toggle: {e}"))?; }
            "next" => { session.TrySkipNextAsync().map_err(|e| format!("next: {e}"))?.get().map_err(|e| format!("next: {e}"))?; }
            "previous" => { session.TrySkipPreviousAsync().map_err(|e| format!("prev: {e}"))?.get().map_err(|e| format!("prev: {e}"))?; }
            "stop" => { session.TryStopAsync().map_err(|e| format!("stop: {e}"))?.get().map_err(|e| format!("stop: {e}"))?; }
            "seek_forward" | "seek_back" => {
                let timeline = session.GetTimelineProperties().map_err(|e| format!("timeline: {e}"))?;
                let pos = timeline.Position().map_err(|e| format!("position: {e}"))?.Duration;
                let delta = (seek_ms as i64) * 10000;
                let target = if action == "seek_forward" {
                    (pos + delta).max(0)
                } else {
                    (pos - delta).max(0)
                };
                session.TryChangePlaybackPositionAsync(target)
                    .map_err(|e| format!("seek: {e}"))?
                    .get().map_err(|e| format!("seek: {e}"))?;
            }
            _ => return Err(format!("unknown action: {action}")),
        }
        Ok(())
    }).await.map_err(|e| format!("spawn_blocking: {e}"))?
    }).await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => {
            log::warn!("SMTC control timed out after {}s", CONTROL_TIMEOUT.as_secs());
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
            .get().map_err(|e| format!("SMTC get: {e}"))?;
        let current_session = manager.GetCurrentSession().ok();
        let sessions = manager.GetSessions().map_err(|e| format!("SMTC GetSessions: {e}"))?;

        let mut best_thumbnail: Option<IRandomAccessStreamReference> = None;
        let mut best_score: i32 = -999999;

        for candidate in sessions.into_iter() {
            let props = candidate.TryGetMediaPropertiesAsync().ok().and_then(|op| op.get().ok());
            let playback = candidate.GetPlaybackInfo().ok();
            let is_current = current_session.as_ref().map(|cs| cs == &candidate).unwrap_or(false);

            let mut score = 0;
            if let Some(ref pb) = playback {
                if let Ok(status) = pb.PlaybackStatus() {
                    if playback_status_str(&status) == "Playing" { score += 3000; }
                }
            }
            if is_current { score += 1200; }
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
        let stream = thumbnail.OpenReadAsync()
            .map_err(|e| format!("OpenReadAsync: {e}"))?
            .get().map_err(|e| format!("OpenReadAsync: {e}"))?;
        let content_type = to_string_lossy(&stream.ContentType().unwrap_or_default());
        let size = stream.Size().map_err(|e| format!("Size: {e}"))? as u32;

        let input_stream = stream.GetInputStreamAt(0).map_err(|e| format!("GetInputStreamAt: {e}"))?;
        let reader = DataReader::CreateDataReader(&input_stream).map_err(|e| format!("CreateDataReader: {e}"))?;
        reader.LoadAsync(size).map_err(|e| format!("LoadAsync: {e}"))?.get().map_err(|e| format!("LoadAsync: {e}"))?;

        let mut bytes = vec![0u8; size as usize];
        reader.ReadBytes(&mut bytes).map_err(|e| format!("ReadBytes: {e}"))?;
        Ok((bytes, content_type))
    }).await.map_err(|e| format!("spawn_blocking: {e}"))?
    }).await;

    match result {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => {
            log::warn!("SMTC thumbnail timed out after {}s", THUMBNAIL_TIMEOUT.as_secs());
            Err("thumbnail fetch timed out".to_string())
        }
    }
}
