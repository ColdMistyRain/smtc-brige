#!/usr/bin/env node
"use strict";

const http = require("http");
const https = require("https");
const fs = require("fs");
const os = require("os");
const path = require("path");
const zlib = require("zlib");
const { spawn } = require("child_process");
const common = require("./sources/common");
const createNeteaseSource = require("./sources/netease");
const createQQMusicSource = require("./sources/qqmusic");

const HOST = process.env.SMTC_BRIDGE_HOST || "0.0.0.0";
const PORT = Number(process.env.SMTC_BRIDGE_PORT || 17865);
const CACHE_MS = Number(process.env.SMTC_CACHE_MS || 650);
const SEEK_MS = Number(process.env.SMTC_SEEK_MS || 15000);
const LYRIC_CACHE_MS = Number(process.env.SMTC_LYRIC_CACHE_MS || 6 * 60 * 60 * 1000);
const SEARCH_CACHE_MS = Number(process.env.SMTC_SEARCH_CACHE_MS || 60 * 60 * 1000);
const META_CACHE_MS = Number(process.env.SMTC_META_CACHE_MS || 6 * 60 * 60 * 1000);
const EDGE_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";

let cache = { at: 0, value: null };
const lyricCache = new Map();
const searchCache = new Map();
const metaCache = new Map();
const qqSearchCache = new Map();
const qqMetaCache = new Map();
const sourceAdapters = [];
const sourceByProvider = new Map();

function psEncode(script) {
  return Buffer.from(script, "utf16le").toString("base64");
}

function runPowerShell(script, env = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn("powershell.exe", [
      "-NoProfile",
      "-ExecutionPolicy", "Bypass",
      "-EncodedCommand", psEncode(script),
    ], {
      windowsHide: true,
      env: { ...process.env, ...env },
    });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("PowerShell timeout"));
    }, 6000);
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error((stderr || stdout || `PowerShell exited ${code}`).trim()));
        return;
      }
      resolve(stdout.trim());
    });
  });
}

async function resizeCoverJpeg(buffer, size = 88) {
  const tag = `${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
  const input = path.join(os.tmpdir(), `smtc-cover-${tag}.img`);
  const output = path.join(os.tmpdir(), `smtc-cover-${tag}.jpg`);
  fs.writeFileSync(input, buffer);
  const script = String.raw`
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
$in = [string]$env:SMTC_COVER_INPUT
$out = [string]$env:SMTC_COVER_OUTPUT
$size = [int]$env:SMTC_COVER_SIZE
$src = [System.Drawing.Image]::FromFile($in)
$bmp = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
$g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$g.Clear([System.Drawing.Color]::Black)
$scale = [Math]::Max($size / $src.Width, $size / $src.Height)
$w = [int][Math]::Ceiling($src.Width * $scale)
$h = [int][Math]::Ceiling($src.Height * $scale)
$x = [int][Math]::Floor(($size - $w) / 2)
$y = [int][Math]::Floor(($size - $h) / 2)
$g.DrawImage($src, $x, $y, $w, $h)
$codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object { $_.MimeType -eq "image/jpeg" } | Select-Object -First 1
$params = [System.Drawing.Imaging.EncoderParameters]::new(1)
$params.Param[0] = [System.Drawing.Imaging.EncoderParameter]::new([System.Drawing.Imaging.Encoder]::Quality, [int64]90)
$bmp.Save($out, $codec, $params)
$g.Dispose()
$bmp.Dispose()
$src.Dispose()
`;
  try {
    await runPowerShell(script, {
      SMTC_COVER_INPUT: input,
      SMTC_COVER_OUTPUT: output,
      SMTC_COVER_SIZE: String(size),
    });
    return fs.readFileSync(output);
  } finally {
    try { fs.unlinkSync(input); } catch {}
    try { fs.unlinkSync(output); } catch {}
  }
}

const statusScript = String.raw`
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
`;

const controlScript = String.raw`
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
`;

const thumbnailScript = String.raw`
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
`;

function isQQMusicSource(source) {
  return /qqmusic|tencent/i.test(String(source || ""));
}

function stripPlaybackSuffix(value) {
  return String(value || "")
    .replace(/\s*[-–—|]\s*(?:qq\s*music|qq音乐|腾讯音乐)\s*$/i, "")
    .replace(/\s+/g, " ")
    .trim();
}

function inferTrackMetadata(status) {
  const next = { ...status };
  next.title = stripPlaybackSuffix(next.title);
  next.artist = stripPlaybackSuffix(next.artist);
  next.album = stripPlaybackSuffix(next.album);

  if (!next.artist && isQQMusicSource(next.source)) {
    const match = String(next.title || "").match(/^(.+?)\s+[-–—]\s+(.+)$/);
    if (match) {
      next.title = stripPlaybackSuffix(match[1]);
      next.artist = stripPlaybackSuffix(match[2]);
    }
  }

  return next;
}

function httpsJson(url) {
  return new Promise((resolve, reject) => {
    const req = https.get(url, {
      headers: {
        "user-agent": EDGE_UA,
        "referer": "https://music.163.com/",
        "accept": "application/json,text/plain,*/*",
      },
      timeout: 7000,
    }, (res) => {
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => {
        const body = Buffer.concat(chunks).toString("utf8");
        if (res.statusCode < 200 || res.statusCode >= 300) {
          reject(new Error(`lyric http ${res.statusCode}: ${body.slice(0, 120)}`));
          return;
        }
        try { resolve(JSON.parse(body)); }
        catch (error) { reject(error); }
      });
    });
    req.on("timeout", () => req.destroy(new Error("lyric request timeout")));
    req.on("error", reject);
  });
}

function requestText(url, options = {}, redirects = 3) {
  return new Promise((resolve, reject) => {
    const target = new URL(url);
    const client = target.protocol === "http:" ? http : https;
    const req = client.get(target, {
      headers: {
        "user-agent": EDGE_UA,
        "referer": options.referer || "https://y.qq.com/",
        "accept": options.accept || "application/json,text/plain,*/*",
        "accept-encoding": "gzip, deflate, br",
        ...(options.headers || {}),
      },
      timeout: options.timeout || 9000,
    }, (res) => {
      const location = res.headers.location;
      if (location && res.statusCode >= 300 && res.statusCode < 400 && redirects > 0) {
        res.resume();
        const next = new URL(location, target).toString();
        requestText(next, options, redirects - 1).then(resolve, reject);
        return;
      }
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => {
        let body = Buffer.concat(chunks);
        if (res.statusCode < 200 || res.statusCode >= 300) {
          reject(new Error(`http ${res.statusCode}: ${body.toString("utf8", 0, 120)}`));
          return;
        }
        try {
          const encoding = String(res.headers["content-encoding"] || "").toLowerCase();
          if (encoding.includes("gzip")) body = zlib.gunzipSync(body);
          else if (encoding.includes("deflate")) body = zlib.inflateSync(body);
          else if (encoding.includes("br")) body = zlib.brotliDecompressSync(body);
        } catch (error) {
          reject(error);
          return;
        }
        resolve(body.toString("utf8"));
      });
    });
    req.on("timeout", () => req.destroy(new Error("request timeout")));
    req.on("error", reject);
  });
}

function parseLooseJson(raw) {
  let text = String(raw || "").trim();
  const jsonp = text.match(/^[\w$.]+\(([\s\S]*)\)\s*;?$/);
  if (jsonp) text = jsonp[1];
  return JSON.parse(text);
}

async function requestJson(url, options) {
  return parseLooseJson(await requestText(url, options));
}

function fetchBuffer(url, options = {}, redirects = 3) {
  return new Promise((resolve, reject) => {
    const target = new URL(url);
    const client = target.protocol === "http:" ? http : https;
    const req = client.get(target, {
      headers: {
        "user-agent": EDGE_UA,
        "referer": options.referer || "https://music.163.com/",
        "accept": "image/jpeg,image/png,image/*;q=0.8,*/*;q=0.5",
      },
      timeout: 9000,
    }, (res) => {
      const location = res.headers.location;
      if (location && res.statusCode >= 300 && res.statusCode < 400 && redirects > 0) {
        res.resume();
        const next = new URL(location, target).toString();
        fetchBuffer(next, options, redirects - 1).then(resolve, reject);
        return;
      }
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => {
        const body = Buffer.concat(chunks);
        if (res.statusCode < 200 || res.statusCode >= 300) {
          reject(new Error(`image http ${res.statusCode}: ${body.toString("utf8", 0, 120)}`));
          return;
        }
        let contentType = String(res.headers["content-type"] || "image/jpeg");
        if (body.length >= 8 && body[0] === 0x89 && body[1] === 0x50 && body[2] === 0x4e && body[3] === 0x47) {
          contentType = "image/png";
        } else if (body.length >= 2 && body[0] === 0xff && body[1] === 0xd8) {
          contentType = "image/jpeg";
        }
        resolve({
          body,
          contentType,
        });
      });
    });
    req.on("timeout", () => req.destroy(new Error("image request timeout")));
    req.on("error", reject);
  });
}

async function fetchFirstBuffer(urls, options) {
  let lastError = null;
  for (const url of urls) {
    try {
      return await fetchBuffer(url, options);
    } catch (error) {
      lastError = error;
    }
  }
  throw lastError || new Error("image not found");
}

function registerSource(source) {
  sourceAdapters.push(source);
  sourceByProvider.set(source.id, source);
  for (const alias of source.aliases || []) sourceByProvider.set(alias, source);
  return source;
}

const neteaseSource = registerSource(createNeteaseSource({
  common,
  httpsJson,
  lyricCache,
  metaCache,
  searchCache,
  LYRIC_CACHE_MS,
  META_CACHE_MS,
  SEARCH_CACHE_MS,
}));

const qqMusicSource = registerSource(createQQMusicSource({
  common,
  lyricCache,
  qqMetaCache,
  qqSearchCache,
  requestJson,
  LYRIC_CACHE_MS,
  META_CACHE_MS,
  SEARCH_CACHE_MS,
}));

function sourceForStatus(status) {
  return sourceAdapters.find((source) => source.matches(status)) || neteaseSource;
}

function sourceForProvider(provider) {
  return sourceByProvider.get(String(provider || "").toLowerCase()) || neteaseSource;
}

async function resolveMedia(status) {
  return sourceForStatus(status).resolve(status);
}

function lyricAt(lines, positionMs) {
  let index = -1;
  for (let i = 0; i < lines.length; i += 1) {
    if (lines[i].at_ms <= positionMs) index = i;
    else break;
  }
  return {
    index,
    at_ms: index >= 0 ? lines[index].at_ms : 0,
    next_at_ms: index + 1 < lines.length ? lines[index + 1].at_ms : 0,
    current: index >= 0 ? lines[index].text : "",
    next: index + 1 < lines.length ? lines[index + 1].text : "",
  };
}

async function smtcStatus(force = false) {
  const now = Date.now();
  if (!force && cache.value && now - cache.at < CACHE_MS) return cache.value;
  try {
    const raw = await runPowerShell(statusScript);
    const status = inferTrackMetadata(JSON.parse(raw || "{}"));
    if (status.connected) {
      status.smtc_adapter = isQQMusicSource(status.source) ? "qqmusic" : "generic";
      const { found, meta } = await resolveMedia(status);
      if (!status.album && meta.album) status.album = meta.album;
      if ((!Number(status.duration_ms) || Number(status.duration_ms) <= 0) && meta.duration_ms) status.duration_ms = meta.duration_ms;
      status.ncm_id_text = status.ncm_id ? String(status.ncm_id) : "";
      status.cover_url = meta.cover_url || "";
      status.lyrics_available = found.lines.length > 0;
      status.translation_line_count = found.translation_line_count || 0;
      status.lyric_source = found.source;
      status.lyric = lyricAt(found.lines, Number(status.position_ms) || 0);
    } else {
      status.lyrics_available = false;
      status.lyric = { index: -1, current: "", next: "" };
    }
    cache = { at: now, value: status };
    return status;
  } catch (error) {
    const fallback = { ok: false, connected: false, error: error.message, state: "error", lyric: { index: -1, current: "", next: "" } };
    cache = { at: now, value: fallback };
    return fallback;
  }
}

function sendJson(res, code, value) {
  const body = JSON.stringify(value);
  res.writeHead(code, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(body),
    "cache-control": "no-store",
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET, POST, OPTIONS",
    "access-control-allow-headers": "content-type",
  });
  res.end(body);
}

function html() {
  return `<!doctype html><meta charset="utf-8"><title>SMTC Bridge</title>
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
</script>`;
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url || "/", `http://${req.headers.host || "localhost"}`);
  if (req.method === "OPTIONS") {
    sendJson(res, 204, {});
    return;
  }
  if (req.method === "GET" && url.pathname === "/") {
    const body = html();
    res.writeHead(200, { "content-type": "text/html; charset=utf-8", "content-length": Buffer.byteLength(body) });
    res.end(body);
    return;
  }
  if (req.method === "GET" && url.pathname === "/health") {
    sendJson(res, 200, { ok: true, service: "smtc-bridge", lyric_sources: ["smtc-genres-ncm-id", "qqmusic"] });
    return;
  }
  if (req.method === "GET" && url.pathname === "/status") {
    sendJson(res, 200, await smtcStatus(url.searchParams.get("fresh") === "1"));
    return;
  }
  if (req.method === "GET" && url.pathname === "/lyrics") {
    try {
      let provider = String(url.searchParams.get("provider") || "").toLowerCase();
      let idText = String(url.searchParams.get("id") || url.searchParams.get("ncm_id") || "");
      let songMid = String(url.searchParams.get("songmid") || "");
      if (!provider && url.searchParams.has("ncm_id")) provider = "netease";
      if (!idText) {
        const status = await smtcStatus(url.searchParams.get("fresh") === "1");
        provider = String(status.lyric_provider || (status.ncm_id ? "netease" : "")).toLowerCase();
        idText = String(status.lyric_id_text || status.ncm_id_text || "");
        songMid = String(status.qq_song_mid || "");
      }
      let found;
      const source = sourceForProvider(provider);
      if (source.id === "qqmusic") {
        found = await source.fetchLyrics({ id: Number(idText) || 0, mid: songMid });
      } else {
        idText = String(Number(idText) || 0);
        found = await source.fetchLyrics({ id: idText });
      }
      provider = source.id;
      sendJson(res, 200, {
        ok: true,
        provider,
        id: idText,
        ncm_id: provider === "netease" ? Number(idText) || 0 : 0,
        ncm_id_text: provider === "netease" ? idText : "",
        source: found.source,
        translation_line_count: found.translation_line_count || 0,
        line_count: found.lines.length,
        lines: found.lines,
      });
    } catch (error) {
      sendJson(res, 500, { ok: false, error: error.message, lines: [] });
    }
    return;
  }
  if (req.method === "GET" && url.pathname === "/cover") {
    try {
      let provider = String(url.searchParams.get("provider") || "").toLowerCase();
      let idText = String(url.searchParams.get("id") || url.searchParams.get("ncm_id") || "");
      if (!provider && url.searchParams.has("ncm_id")) provider = "netease";
      let coverUrl = "";
      let referer = "https://music.163.com/";
      let normalizeToDeviceSize = false;
      const source = sourceForProvider(provider);
      if (source.id === "qqmusic") {
        coverUrl = source.coverCandidates(idText, provider);
        referer = "https://y.qq.com/";
        normalizeToDeviceSize = source.normalizeCover;
      } else if (provider === "smtc") {
        const raw = await runPowerShell(thumbnailScript);
        const thumb = JSON.parse(raw || "{}");
        if (!thumb.ok || !thumb.base64) {
          sendJson(res, 404, { ok: false, error: thumb.error || "thumbnail not found" });
          return;
        }
        const body = Buffer.from(thumb.base64, "base64");
        res.writeHead(200, {
          "content-type": thumb.content_type || "image/jpeg",
          "content-length": body.length,
          "cache-control": "no-store",
          "access-control-allow-origin": "*",
        });
        res.end(body);
        return;
      } else {
        coverUrl = await source.coverCandidates(idText);
      }
      if (!coverUrl || (Array.isArray(coverUrl) && !coverUrl.length)) {
        sendJson(res, 404, { ok: false, error: "cover not found" });
        return;
      }
      const image = Array.isArray(coverUrl)
        ? await fetchFirstBuffer(coverUrl, { referer })
        : await fetchBuffer(coverUrl, { referer });
      if (normalizeToDeviceSize) {
        image.body = await resizeCoverJpeg(image.body, 88);
        image.contentType = "image/jpeg";
      }
      res.writeHead(200, {
        "content-type": image.contentType,
        "content-length": image.body.length,
        "cache-control": "public, max-age=86400",
        "access-control-allow-origin": "*",
      });
      res.end(image.body);
    } catch (error) {
      sendJson(res, 500, { ok: false, error: error.message });
    }
    return;
  }
  if ((req.method === "GET" || req.method === "POST") && url.pathname === "/control") {
    const action = url.searchParams.get("action") || "playpause";
    sendJson(res, 202, { ok: true, accepted: true, action });
    runPowerShell(controlScript, { SMTC_ACTION: action, SMTC_SEEK_MS: String(SEEK_MS) })
      .then(() => { cache.at = 0; })
      .catch((error) => {
        cache = {
          at: Date.now(),
          value: { ok: false, connected: false, error: error.message, state: "error", action, lyric: { index: -1, current: "", next: "" } },
        };
      });
    return;
  }
  sendJson(res, 404, { ok: false, error: "not found" });
});

server.listen(PORT, HOST, () => {
  process.stdout.write(`SMTC bridge listening on http://${HOST}:${PORT}\n`);
  process.stdout.write("Lyrics: InfLink NCM-{id} -> NetEase, QQ Music SMTC -> QQ Music API\n");
});

function shutdown() {
  server.close(() => process.exit(0));
}
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
