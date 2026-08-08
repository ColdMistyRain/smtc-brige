// Windows SMTC implementation using the windows-rs crate.
use windows::core::HSTRING;
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use crate::common::SmtcStatus;

fn init_com() -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx: {e}"))
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

pub async fn smtc_status_raw() -> Result<SmtcStatus, String> {
    let manager = tokio::task::spawn_blocking(|| {
        init_com()?;
        GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .map_err(|e| format!("SMTC RequestAsync: {e}"))
            .and_then(|op| op.get().map_err(|e| format!("SMTC get: {e}")))
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))??;

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
        let props = candidate
            .TryGetMediaPropertiesAsync()
            .ok()
            .and_then(|op| op.get().ok());

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
            .map(|s| format!("{:?}", s))
            .unwrap_or_default();

        if playback_status.contains("Playing") { score += 3000; }
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
}

// ── Control ─────────────────────────────────────────────────────────────────

pub async fn smtc_control(action: &str, seek_ms: u64) -> Result<(), String> {
    let action = action.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        init_com()?;
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
}

// ── Thumbnail ───────────────────────────────────────────────────────────────

pub async fn smtc_thumbnail() -> Result<(Vec<u8>, String), String> {
    tokio::task::spawn_blocking(|| -> Result<(Vec<u8>, String), String> {
        init_com()?;
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
                    if format!("{:?}", status).contains("Playing") { score += 3000; }
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
}
