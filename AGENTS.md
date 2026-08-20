# AGENTS.md — SMTC Bridge 项目指南（供 AI 编程代理阅读）

## 项目概览

SMTC Bridge 是一个系统媒体传输控制（SMTC）桥接服务。它读取 Windows SMTC /
Linux MPRIS 暴露的"当前正在播放的媒体"信息，以 HTTP API 的形式提供给局域网内的
其他设备（如 ESP32 等嵌入式播放器），并支持歌词匹配、封面提取与远程控制
（播放/暂停/切歌/快进快退）。

- 单二进制、无外部服务依赖，监听 `0.0.0.0:17865`（日志中显示为
  `127.0.0.1` 便于复制打开）。
- 主入口 `src/main.rs`：初始化 fern 日志（滚动 10MB 到 `smtc-bridge.log`）→
  构建 axum 路由 → 启动后台缓存清扫任务（每 5 分钟）→ 监听服务。
- 旧的 Node.js 实现保留在 `smtc-bridge.js`（历史遗留，它 `require("./sources/*")`
  的模块已不存在，勿再维护；当前唯一活跃实现是 Rust 版）。

## 技术栈

- Rust 2021 + axum 0.7 + tokio 1（full features）+ reqwest 0.12 + serde
- Windows：windows-rs 0.58（`Media_Control` / `Storage_Streams` / `Foundation` /
  `Foundation_Collections`）
- Linux：dbus 0.9（MPRIS）
- 其他平台：`src/smtc/noop.rs` 空实现
- 图片处理：`image` 0.25（封面缩放为 JPEG）
- 发布：GitHub Actions（`v*` tag 触发，构建 Windows x86_64 exe）

## 模块职责（src/）

| 文件 | 职责 |
|------|------|
| `main.rs` | 日志初始化（fern，10MiB 轮转）、axum 路由、5 分钟缓存清扫任务 |
| `config.rs` | 全部硬编码常量：`HOST`/`PORT`(17865)/`CACHE_MS`(650)/`SEEK_MS`(15000)/缓存 TTL |
| `state.rs` | `AppState`：`status_cache`、`thumbnail_cache`、`fetch_mutex`、`last_known_status`、`position_anchor`、`control_lock`、两个音乐源、共享 `reqwest::Client` |
| `handlers.rs` | HTTP 路由处理器；`enriched_status` 编排 SMTC 采样→歌词解析→位置维护；`maintain_position` 位置锚点记账 |
| `common.rs` | `SmtcStatus`/`RawSmtcInfo` 类型、LRC 解析、文本归一化、缓存工具（`CacheEntry`/`sweep_cache`/`cache_insert_limited`，上限 `MAX_CACHE_ENTRIES=512`） |
| `netease.rs` | 网易云源：搜索、歌词、元数据（各带独立 TTL 缓存，实现 `MusicSource` trait） |
| `qqmusic.rs` | QQ 音乐源：搜索、歌词、元数据（各带独立 TTL 缓存，实现 `MusicSource` trait） |
| `source.rs` | `MusicSource` trait（async-trait）：`resolve`/`fetch_lyrics`/`sweep_caches`/`name`；`AppState.sources` 按序构成回退链 |
| `smtc.rs` | 封面 JPEG 缩放（`resize_cover_jpeg`）+ 平台分派（cfg 导出 win/mpris/noop） |
| `smtc/win.rs` | Windows SMTC：状态采样、控制、缩略图（见下方"关键设计"） |
| `smtc/mpris.rs` | Linux MPRIS 等价实现 |

## 关键业务逻辑（最容易踩坑的地方）

1. **网易云 SMTC 缺陷**：`cloudmusic.exe` 会话 `Position`/`EndTime` 恒为 0
   （duration=0），但 `LastUpdatedTime` 持续刷新。纯外推无效。
   **对策 = 锚点记账**：`handlers::maintain_position` 持久化
   `PositionAnchor(track_key, position_ms, time_ms)`。可信样本
   `position_base_ms > 0` 直接采用；否则按锚点外推；`track_key`
   （`源|标题|歌手|专辑`）变化则归零；暂停/停止冻结；到达时长仍在播判定循环归零。
   `duration_ms=0` 时用 `resolve()` 返回的 `MetaInfo.duration_ms` 补全。
   `SmtcStatus.position_source` 取值 `"smtc" | "estimated"`。
2. **windows-rs 0.58 细节**：`PlaybackRate()` 在
   `GlobalSystemMediaTransportControlsSessionPlaybackInfo`（返回
   `IReference<f64>`，需 `.Value()`），**不在** `TimelineProperties`。
   `DateTime.UniversalTime` 是 FILETIME（100ns since 1601）→ unix ms：
   `t/10000 - 11644473600000`。`MediaPlaybackType`/`AutoRepeatMode` 是 `pub i32`
   透明结构（1=Music/2=Video/3=Image；1=Track/2=List）。
3. **SMTC 采样会话选择**：遍历 `GetSessions()` 按评分选最佳会话
   （Playing +3000，含 NCM id +1000，有 duration +200，当前会话 +1200…）。
   `GetCurrentSession()` 不可靠（暂停过久会返回空），控制命令用
   `pick_control_session()`：优先 current，失败回退扫描会话
   （Playing > Paused > 任意）。
4. **异步属性获取不能新建裸线程**：`try_get_media_properties` 曾每次
   `std::thread::spawn` 执行 `IAsyncOperation::get()`，broken session 永不返回会
   永久泄漏 OS 线程（status 每 ~1.5s 轮询一次 → 约 40 线程/分钟）。
   现方案：固定 4-worker 线程池（`Mutex<VecDeque>` + `Condvar`，
   `PROPS_POOL`）+ 挂起会话熔断黑名单（`HUNG_SESSIONS`，按
   `SourceAppUserModelId` 键控，60s 冷却，`retain` 保持有界）。
   修改这块代码前先理解这两者，不要回退成每次新建线程。
5. **网易云不支持 seek**：其会话 `is_playback_position_enabled=false`，
   `seek_forward`/`seek_back` 对它必然失败，属播放器端限制而非 bug。
6. **缓存策略**：歌词 6h / 搜索 1h / 元数据 6h，每个 HashMap 上限 512 条
   （`cache_insert_limited` 超限时按插入时间淘汰到 75%），后台每 5 分钟
   `sweep_all_caches`（遍历 `sources` 链调各源 `sweep_caches()`）。
   `status_cache`(650ms)/`thumbnail_cache`(5s)/`last_known_status`/`position_anchor`
   都是单条目。
7. **HTTP 客户端**：全局共享一个 `reqwest::Client`（UA=EDGE_UA，9s 超时，
   90s 连接池空闲，每 host 2 空闲连接），不要为请求新建 Client。
8. **音乐源回退链**：`AppState.sources: Vec<Arc<dyn MusicSource>>` 按序尝试，
   第一个返回非空歌词的源胜出（QQ 状态 [qq, ne]，其他 [ne, qq]）。新增源只需
   实现 trait 并加入列表。
9. **优雅关停**：`AppState.shutdown: watch::Sender<bool>`；`GET /shutdown` 发信号，
   `main` 用 `with_graceful_shutdown` 排空连接后退出，勿用 `process::exit`。
10. **CORS**：由 `tower_http::cors::CorsLayer::permissive()` 统一添加
    （含 preflight），不要在响应里手写 CORS 头。
11. **异步歌词解析（关键）**：`/status` 响应**绝不阻塞在歌词解析上**。歌词 API
    （尤其 QQ 搜索 c.y.qq.com）可能卡数秒，若串行在 `/status` 里会饿死短超时的
    嵌入式客户端（ESP32）。设计：SMTC 采样（<1s）后立即返回状态；歌词走
    `AppState.lyric_cache`（按 `track_key=source|title|artist|album`，TTL=歌词 6h），
    miss 时由 `handlers::spawn_lyric_resolution` 后台 `tokio::spawn` 解析并写回
    （`lyric_fetching` 去重集合防重复）。provider hint：netease 的
    `lyric_id_text` 用 `ncm_id_text`（SMTC 直接给）；qqmusic 前台为空（SMTC 无
    song id，后台解析才拿到）。

## 常用命令

```bash
cargo check                 # 快速检查（Windows 上编译 win.rs 路径）
cargo run                   # debug 运行（带控制台）
cargo build --release       # 发布构建（windows_subsystem="windows" 无控制台）
```

- release profile：`opt-level="s"`、lto、codegen-units=1、strip、`panic="abort"`。
- 日志：运行目录下 `smtc-bridge.log`（超过 10MiB 轮转为 `.old`）。
- 停止：`stop.bat`（`taskkill /f /im smtc-brige.exe`）或 `GET /shutdown`。
- 测试：无单元测试；验证靠 `cargo check` + 手工跑 API。

## HTTP API

| 端点 | 说明 |
|------|------|
| `GET /` | HTML 仪表盘（内嵌 JS，每 1.5s 轮询 `/status`） |
| `GET /health` | 健康检查 |
| `GET /status?fresh=1` | 播放状态（`fresh=1` 强制绕过 650ms 缓存）；字段见 `SmtcStatus`，含 `raw` 原始 SMTC 数据 |
| `GET /lyrics?provider=&id=&ncm_id=&songmid=` | 歌词（provider 缺省按当前状态推断；`qq`/`qqartist` 归一为 `qqmusic`） |
| `GET /lyrics/now` | 当前播放曲目的完整歌词（无需参数；后台解析未完成时返回 `loading:true`，可稍后重试） |
| `GET /cover?provider=smtc&id=&size=96` | 封面（`provider=smtc` 走 SMTC 缩略图；size 夹在 32–512） |
| `GET/POST /control?action=...` | `play`/`pause`/`playpause`/`next`/`previous`/`seek_forward`/`seek_back`（±`SEEK_MS`=15s）。202 立即返回，后台 `control_lock` 串行执行，5s 超时 |
| `GET /shutdown` | 优雅退出（watch 信号 → 排空连接） |
| fallback | OPTIONS 返回 CORS 头，其余 404 JSON |

响应均为 JSON + CORS（`*`）；二进制封面带 `CONTENT_LENGTH`。

## 配置

在 `src/config.rs`：`SMTC_BRIDGE_HOST` / `SMTC_BRIDGE_PORT` 支持环境变量覆盖
（`LazyLock` 惰性读取）；其余 TTL / 封面尺寸等仍是硬编码常量，改动需改源码
并重编译。

## 编码约定

- 日志用 `log` 宏（`log::debug!/warn!/error!`），经 fern 输出到 stderr + 文件；
  外部库（reqwest）降为 `Warn`。
- 状态结构 `SmtcStatus` 所有字段可序列化；新增字段加 `#[serde(default)]`
  避免破坏旧客户端（`default` 需手动 impl 的用 `Default` 派生）。
- 阻塞 WinRT/网络调用：网络用 async；阻塞 COM 调用放 `tokio::task::spawn_blocking`
  并用 `tokio::time::timeout` 兜底（status 8s / control 5s / thumbnail 6s /
  属性 4s）。CPU 密集的封面缩放（Lanczos+JPEG）也在 `spawn_blocking` 上跑。
- 平台分派只通过 `src/smtc.rs` 的 `cfg` 导出，不要在业务代码里直接
  `#[cfg(target_os = "windows")]`。
- CI（`.github/workflows/ci.yml`）在 Windows 与 Linux 双平台跑
  `cargo check`/`test`，Windows 上另跑 `clippy -D warnings` 与 `fmt --check`；
  Linux 路径依赖 dbus，改动 `smtc/mpris.rs` 后务必让 CI 过。
