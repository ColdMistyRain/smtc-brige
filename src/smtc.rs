use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use std::io::Cursor;
use std::process::Stdio;
use tokio::process::Command;

use crate::common::SmtcStatus;

// ── PowerShell Encoding ─────────────────────────────────────────────────────

fn ps_encode(script: &str) -> String {
    let utf16_bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(&utf16_bytes)
}

pub async fn run_powershell(script: &str, env: Vec<(&str, &str)>) -> Result<String, String> {
    let encoded = ps_encode(script);
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        &encoded,
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .stdin(Stdio::null())
    .kill_on_drop(true);

    for (key, val) in &env {
        cmd.env(key, val);
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        cmd.output(),
    )
    .await
    .map_err(|_| "PowerShell timeout".to_string())?
    .map_err(|e| format!("PowerShell spawn error: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err((stderr.to_string() + &stdout).trim().to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ── Cover Resize ────────────────────────────────────────────────────────────

pub fn resize_cover_jpeg(buffer: &[u8], size: u32) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(buffer).map_err(|e| format!("image decode: {e}"))?;
    let (w, h) = (img.width(), img.height());

    let scale = f64::max(size as f64 / w as f64, size as f64 / h as f64);
    let new_w = (w as f64 * scale).ceil() as u32;
    let new_h = (h as f64 * scale).ceil() as u32;

    let scaled = img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3);

    let x = ((size as i64 - new_w as i64) / 2).max(0) as u32;
    let y = ((size as i64 - new_h as i64) / 2).max(0) as u32;

    let mut canvas = image::DynamicImage::new_rgb8(size, size);
    image::imageops::overlay(&mut canvas, &scaled, x as i64, y as i64);

    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    let mut encoder = JpegEncoder::new_with_quality(&mut cursor, 90);
    encoder
        .encode(
            canvas.as_bytes(),
            canvas.width(),
            canvas.height(),
            canvas.color().into(),
        )
        .map_err(|e| format!("jpeg encode: {e}"))?;

    Ok(buf)
}

// ── SMTC Status Script ─────────────────────────────────────────────────────

const STATUS_SCRIPT: &str = r#"
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]
$null = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties, Windows.Media.Control, ContentType = WindowsRuntime]
function Await-WinRt($op, [Type]$resultType) {
  $method = [System.WindowsRuntimeSystemExtensions].GetMethods() |
    Where-Object { $_.Name -eq "AsTask" -and $_.IsGenericMethodDefinition -and $_.GetParameters().Count -eq 1 } |
    Select-Object -First 1
  $task = $method.MakeGenericMethod($resultType).Invoke($null, @($op))
  if (-not $task.Wait(4000)) { throw "WinRT timeout" }
  return $task.Result
}
$manager = Await-WinRt ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])
$currentSession = $manager.GetCurrentSession()
$sessions = @($manager.GetSessions())
if ($sessions.Count -eq 0) {
  @{ ok = $true; connected = $false; state = "none"; title = ""; artist = ""; album = ""; position_ms = 0; duration_ms = 0 } | ConvertTo-Json -Compress
  exit 0
}
$best = $null
$bestScore = -999999
$bestProps = $null
$bestPlayback = $null
$bestTimeline = $null
$bestGenres = @()
$bestNcmId = 0
$bestDuration = 0
$bestIsCurrent = $false
foreach ($candidate in $sessions) {
  $props = Await-WinRt ($candidate.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])
  $playback = $candidate.GetPlaybackInfo()
  $timeline = $candidate.GetTimelineProperties()
  $duration = 0
  if ($timeline.EndTime -and $timeline.StartTime) { $duration = [Math]::Max(0, ($timeline.EndTime - $timeline.StartTime).TotalMilliseconds) }
  $genres = @()
  try {
    foreach ($genre in $props.Genres) {
      $genres += [string]$genre
    }
  } catch {}
  $ncmId = 0
  foreach ($genre in $genres) {
    if ($genre -match '^NCM-(\d+)$') {
      $ncmId = [int64]$Matches[1]
      break
    }
  }
  $score = 0
  if ([string]$playback.PlaybackStatus -eq "Playing") { $score += 3000 }
  if ([object]::ReferenceEquals($candidate, $currentSession)) { $score += 1200 }
  if ($ncmId -gt 0) { $score += 1000 }
  if ($duration -gt 0) { $score += 200 }
  if ([string]$props.AlbumTitle -ne "") { $score += 80 }
  if ([string]$props.Title -ne "") { $score += 20 }
  if ($score -gt $bestScore) {
    $best = $candidate
    $bestScore = $score
    $bestProps = $props
    $bestPlayback = $playback
    $bestTimeline = $timeline
    $bestGenres = $genres
    $bestNcmId = $ncmId
    $bestDuration = $duration
    $bestIsCurrent = [object]::ReferenceEquals($candidate, $currentSession)
  }
}
@{
  ok = $true
  connected = $true
  source = [string]$best.SourceAppUserModelId
  state = [string]$bestPlayback.PlaybackStatus
  title = [string]$bestProps.Title
  artist = [string]$bestProps.Artist
  album = [string]$bestProps.AlbumTitle
  album_artist = [string]$bestProps.AlbumArtist
  track_number = [int]$bestProps.TrackNumber
  genres = $bestGenres
  ncm_id = [int64]$bestNcmId
  position_ms = [int64][Math]::Max(0, $bestTimeline.Position.TotalMilliseconds)
  duration_ms = [int64]$bestDuration
  session_count = [int]$sessions.Count
  selected_current = [bool]$bestIsCurrent
  updated_at = [int64]([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())
} | ConvertTo-Json -Compress
"#;

// ── SMTC Control Script ────────────────────────────────────────────────────

const CONTROL_SCRIPT: &str = r#"
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]
function Await-WinRt($op, [Type]$resultType) {
  $method = [System.WindowsRuntimeSystemExtensions].GetMethods() |
    Where-Object { $_.Name -eq "AsTask" -and $_.IsGenericMethodDefinition -and $_.GetParameters().Count -eq 1 } |
    Select-Object -First 1
  $task = $method.MakeGenericMethod($resultType).Invoke($null, @($op))
  if (-not $task.Wait(4000)) { throw "WinRT timeout" }
  return $task.Result
}
$manager = Await-WinRt ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])
$session = $manager.GetCurrentSession()
if ($null -eq $session) { @{ ok = $false; error = "no session" } | ConvertTo-Json -Compress; exit 0 }
$action = [string]$env:SMTC_ACTION
$seekMs = [int64]$env:SMTC_SEEK_MS
$ok = $false
switch ($action) {
  "play" { $ok = Await-WinRt ($session.TryPlayAsync()) ([Boolean]) }
  "pause" { $ok = Await-WinRt ($session.TryPauseAsync()) ([Boolean]) }
  "playpause" { $ok = Await-WinRt ($session.TryTogglePlayPauseAsync()) ([Boolean]) }
  "toggle" { $ok = Await-WinRt ($session.TryTogglePlayPauseAsync()) ([Boolean]) }
  "next" { $ok = Await-WinRt ($session.TrySkipNextAsync()) ([Boolean]) }
  "previous" { $ok = Await-WinRt ($session.TrySkipPreviousAsync()) ([Boolean]) }
  "stop" { $ok = Await-WinRt ($session.TryStopAsync()) ([Boolean]) }
  "seek_forward" {
    $timeline = $session.GetTimelineProperties()
    $target = [Math]::Max(0, $timeline.Position.Ticks + ([TimeSpan]::FromMilliseconds($seekMs)).Ticks)
    $ok = Await-WinRt ($session.TryChangePlaybackPositionAsync($target)) ([Boolean])
  }
  "seek_back" {
    $timeline = $session.GetTimelineProperties()
    $target = [Math]::Max(0, $timeline.Position.Ticks - ([TimeSpan]::FromMilliseconds($seekMs)).Ticks)
    $ok = Await-WinRt ($session.TryChangePlaybackPositionAsync($target)) ([Boolean])
  }
  default { @{ ok = $false; error = "unknown action"; action = $action } | ConvertTo-Json -Compress; exit 0 }
}
@{ ok = [bool]$ok; action = $action } | ConvertTo-Json -Compress
"#;

// ── SMTC Thumbnail Script ──────────────────────────────────────────────────

const THUMBNAIL_SCRIPT: &str = r#"
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]
$null = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties, Windows.Media.Control, ContentType = WindowsRuntime]
$null = [Windows.Storage.Streams.IRandomAccessStreamWithContentType, Windows.Storage.Streams, ContentType = WindowsRuntime]
$null = [Windows.Storage.Streams.DataReader, Windows.Storage.Streams, ContentType = WindowsRuntime]
function Await-WinRt($op, [Type]$resultType) {
  $method = [System.WindowsRuntimeSystemExtensions].GetMethods() |
    Where-Object { $_.Name -eq "AsTask" -and $_.IsGenericMethodDefinition -and $_.GetParameters().Count -eq 1 } |
    Select-Object -First 1
  $task = $method.MakeGenericMethod($resultType).Invoke($null, @($op))
  if (-not $task.Wait(4000)) { throw "WinRT timeout" }
  return $task.Result
}
$manager = Await-WinRt ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])
$currentSession = $manager.GetCurrentSession()
$sessions = @($manager.GetSessions())
$best = $null
$bestScore = -999999
$bestProps = $null
foreach ($candidate in $sessions) {
  $props = Await-WinRt ($candidate.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])
  $playback = $candidate.GetPlaybackInfo()
  $timeline = $candidate.GetTimelineProperties()
  $score = 0
  if ([string]$playback.PlaybackStatus -eq "Playing") { $score += 3000 }
  if ([object]::ReferenceEquals($candidate, $currentSession)) { $score += 1200 }
  if ($props.Thumbnail) { $score += 600 }
  if ([string]$props.Title -ne "") { $score += 20 }
  if ($score -gt $bestScore) {
    $best = $candidate
    $bestScore = $score
    $bestProps = $props
  }
}
if ($null -eq $bestProps -or $null -eq $bestProps.Thumbnail) {
  @{ ok = $false; error = "thumbnail not found" } | ConvertTo-Json -Compress
  exit 0
}
$stream = Await-WinRt ($bestProps.Thumbnail.OpenReadAsync()) ([Windows.Storage.Streams.IRandomAccessStreamWithContentType])
$inputStream = [Windows.Storage.Streams.IInputStream]$stream
$netStream = [System.IO.WindowsRuntimeStreamExtensions]::AsStreamForRead($inputStream)
$memory = [System.IO.MemoryStream]::new()
$netStream.CopyTo($memory)
$bytes = $memory.ToArray()
@{
  ok = $true
  content_type = [string]$stream.ContentType
  bytes = [int]$bytes.Length
  base64 = [Convert]::ToBase64String($bytes)
} | ConvertTo-Json -Compress
"#;

// ── Public API ──────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct SmtcRawStatus {
    pub ok: bool,
    #[serde(default)]
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
    #[serde(default)]
    pub track_number: i32,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub ncm_id: i64,
    #[serde(default)]
    pub position_ms: i64,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub session_count: i32,
    #[serde(default)]
    pub selected_current: bool,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, serde::Deserialize)]
pub struct SmtcThumbnail {
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub content_type: String,
    #[serde(default)]
    pub base64: String,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub struct SmtcControlResult {
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub action: String,
}

pub async fn smtc_status_raw() -> Result<SmtcRawStatus, String> {
    let raw = run_powershell(STATUS_SCRIPT, vec![]).await?;
    serde_json::from_str(&raw).map_err(|e| format!("parse status: {e}"))
}

pub async fn smtc_control(action: &str, seek_ms: u64) -> Result<SmtcControlResult, String> {
    let seek_str = seek_ms.to_string();
    let env = vec![
        ("SMTC_ACTION", action),
        ("SMTC_SEEK_MS", &seek_str),
    ];
    let raw = run_powershell(CONTROL_SCRIPT, env).await?;
    serde_json::from_str(&raw).map_err(|e| format!("parse control: {e}"))
}

pub async fn smtc_thumbnail() -> Result<SmtcThumbnail, String> {
    let raw = run_powershell(THUMBNAIL_SCRIPT, vec![]).await?;
    serde_json::from_str(&raw).map_err(|e| format!("parse thumbnail: {e}"))
}

impl From<SmtcRawStatus> for SmtcStatus {
    fn from(r: SmtcRawStatus) -> Self {
        SmtcStatus {
            ok: r.ok,
            connected: r.connected,
            source: r.source,
            state: r.state,
            title: r.title,
            artist: r.artist,
            album: r.album,
            album_artist: r.album_artist,
            track_number: r.track_number,
            genres: r.genres,
            ncm_id: r.ncm_id,
            position_ms: r.position_ms,
            duration_ms: r.duration_ms,
            session_count: r.session_count,
            selected_current: r.selected_current,
            updated_at: r.updated_at,
            ..Default::default()
        }
    }
}
