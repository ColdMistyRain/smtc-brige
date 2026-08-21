// ── 配置常量 ─────────────────────────────────────────────────

use std::sync::LazyLock;

/// 绑定地址，可通过环境变量 `SMTC_BRIDGE_HOST` 覆盖。
pub static HOST: LazyLock<String> =
    LazyLock::new(|| std::env::var("SMTC_BRIDGE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()));
/// 绑定端口，可通过环境变量 `SMTC_BRIDGE_PORT` 覆盖。
pub static PORT: LazyLock<u16> = LazyLock::new(|| {
    std::env::var("SMTC_BRIDGE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(17865)
});
pub const CACHE_MS: u64 = 650;
pub const SEEK_MS: u64 = 15000;
/// "SMTC 断开/错误"警告的重新输出间隔。仪表盘每 1.5s 轮询一次 `/status`，
/// 若不限流，播放器一直断开时日志会很快刷满。
pub const DISCONNECT_LOG_INTERVAL_MS: u64 = 5 * 60 * 1000;
pub const LYRIC_CACHE_MS: u64 = 6 * 60 * 60 * 1000;
pub const SEARCH_CACHE_MS: u64 = 60 * 60 * 1000;
pub const META_CACHE_MS: u64 = 6 * 60 * 60 * 1000;
pub const THUMBNAIL_CACHE_MS: u64 = 5000;
pub const COVER_SIZE_MIN: u32 = 32;
pub const COVER_SIZE_MAX: u32 = 512;
pub const COVER_SIZE_DEFAULT: u32 = 96;
