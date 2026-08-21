# SMTC Bridge（中文）

[简体中文](README_zh.md) | [English](README.md)

Windows 系统媒体传输控制（SMTC）桥接服务，将当前播放的音乐信息通过 HTTP API 暴露，支持歌词匹配、封面提取和远程控制。

## 特性

- 🎵 **自动检测播放器** — QQ 音乐优先，自动回退网易云搜索
- 🎤 **歌词匹配** — 网易云 / QQ 音乐双源，自动换行
- 🖼️ **封面提取** — 直接从 SMTC 缩略图获取，无需外部 CDN
- 🎮 **远程控制** — 播放/暂停/上下曲/快进快退
- 🌐 **平台** — Windows (SMTC)，其他平台为空实现
- 📝 **日志记录** — 自动写入 `smtc-bridge.log`

## 快速开始

### 安装

下载 [Releases](../../releases) 中的 `smtc-brige.exe`，双击运行即可。

### 开发

```bash
cargo run                  # debug 模式（有控制台）
cargo build --release      # 发布版（无控制台窗口）
```

## API 参考

| 端点 | 说明 |
|------|------|
| `GET /` | Web 仪表盘页面 |
| `GET /health` | 健康检查 |
| `GET /status` | 当前播放状态（歌名、歌手、专辑、封面URL、进度、完整歌词 `full_lyrics`） |
| `GET /lyrics?provider=&id=` | 获取歌词 |
| `GET /lyrics/now` | 当前播放曲目的完整歌词（无需参数；后台解析中返回 `loading:true`） |
| `GET /cover?provider=smtc&size=96` | 封面图片（size: 32–512，默认 96） |
| `GET /control?action=playpause` | 媒体控制命令 |
| `GET /shutdown` | 优雅退出 |

### 控制命令

| Action | 效果 |
|--------|------|
| `play` | 播放 |
| `pause` | 暂停 |
| `playpause` | 播放/暂停切换 |
| `next` | 下一曲 |
| `previous` | 上一曲 |
| `seek_forward` | 快进 15 秒 |
| `seek_back` | 后退 15 秒 |

## 配置

可选环境变量：

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `SMTC_BRIDGE_HOST` | `0.0.0.0` | 监听地址（日志中显示为 `127.0.0.1`，可直接复制打开） |
| `SMTC_BRIDGE_PORT` | `17865` | 监听端口 |

## 技术栈

- **后端**: Rust + axum + tokio
- **SMTC**: windows-rs (Windows)
- **歌词源**: 网易云音乐 API / QQ 音乐 API

## License

MIT
