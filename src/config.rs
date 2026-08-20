// ── Configuration Constants ─────────────────────────────────────────────────

use std::sync::LazyLock;

/// Bind address, overridable via the `SMTC_BRIDGE_HOST` env var.
pub static HOST: LazyLock<String> =
    LazyLock::new(|| std::env::var("SMTC_BRIDGE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()));
/// Bind port, overridable via the `SMTC_BRIDGE_PORT` env var.
pub static PORT: LazyLock<u16> = LazyLock::new(|| {
    std::env::var("SMTC_BRIDGE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(17865)
});
pub const CACHE_MS: u64 = 650;
pub const SEEK_MS: u64 = 15000;
/// How often the "SMTC disconnected/error" warning is re-emitted.  The
/// dashboard polls `/status` every 1.5s, so without throttling the log fills
/// up while the player stays disconnected.
pub const DISCONNECT_LOG_INTERVAL_MS: u64 = 5 * 60 * 1000;
pub const LYRIC_CACHE_MS: u64 = 6 * 60 * 60 * 1000;
pub const SEARCH_CACHE_MS: u64 = 60 * 60 * 1000;
pub const META_CACHE_MS: u64 = 6 * 60 * 60 * 1000;
pub const THUMBNAIL_CACHE_MS: u64 = 5000;
pub const COVER_SIZE_MIN: u32 = 32;
pub const COVER_SIZE_MAX: u32 = 512;
pub const COVER_SIZE_DEFAULT: u32 = 96;
