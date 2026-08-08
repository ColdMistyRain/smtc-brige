# SMTC Bridge

A Windows System Media Transport Controls (SMTC) bridge service that exposes currently playing music information via HTTP API, with support for lyrics matching, cover art extraction, and remote control.

## Features

- 🎵 **Auto-detect Player** — QQ Music first, fallback to NetEase Cloud search
- 🎤 **Lyrics Matching** — Dual-source: NetEase Cloud & QQ Music, with automatic line wrapping
- 🖼️ **Cover Art Extraction** — Directly from SMTC thumbnail, no external CDN required
- 🎮 **Remote Control** — Play/Pause/Next/Previous/Seek forward & backward
- 🌐 **Cross-Platform** — Windows (SMTC) / Linux (MPRIS) / macOS (no-op stub)
- 📝 **Logging** — Auto-logged to `smtc-bridge.log`

## Quick Start

### Installation

Download `smtc-brige.exe` from [Releases](../../releases) and double-click to run.

### Development

```bash
cargo run                  # debug mode (with console)
cargo build --release      # release mode (no console window)
```

## API Reference

| Endpoint | Description |
|----------|-------------|
| `GET /` | Web dashboard page |
| `GET /health` | Health check |
| `GET /status` | Current playback status (title, artist, album, cover URL, progress) |
| `GET /lyrics?provider=&id=` | Fetch lyrics from specified provider |
| `GET /cover?provider=smtc&size=96` | Cover art image (size: 32–512, defaults to 96) |
| `GET /control?action=playpause` | Media control command |
| `GET /shutdown` | Graceful shutdown |

### Control Actions

| Action | Effect |
|--------|--------|
| `play` | Resume playback |
| `pause` | Pause playback |
| `playpause` | Toggle play/pause |
| `next` | Next track |
| `previous` | Previous track |
| `seek_forward` | Fast-forward 15 seconds |
| `seek_back` | Rewind 15 seconds |

## Configuration

Optional environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `SMTC_BRIDGE_HOST` | `0.0.0.0` | Listen address |
| `SMTC_BRIDGE_PORT` | `17865` | Listen port |

## Tech Stack

- **Backend**: Rust + axum + tokio
- **SMTC**: windows-rs (Windows) / dbus MPRIS (Linux)
- **Lyrics Sources**: NetEase Cloud Music API / QQ Music API

## License

MIT

## 配置

可选环境变量：

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `SMTC_BRIDGE_HOST` | `0.0.0.0` | 监听地址 |
| `SMTC_BRIDGE_PORT` | `17865` | 监听端口 |

## 技术栈

- **后端**: Rust + axum + tokio
- **SMTC**: windows-rs (Windows) / dbus MPRIS (Linux)
- **歌词源**: 网易云音乐 API / QQ 音乐 API

## License

MIT
