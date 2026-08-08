// ── Configuration Constants ─────────────────────────────────────────────────

pub const HOST: &str = "0.0.0.0";
pub const PORT: u16 = 17865;
pub const CACHE_MS: u64 = 650;
pub const SEEK_MS: u64 = 15000;
pub const LYRIC_CACHE_MS: u64 = 6 * 60 * 60 * 1000;
pub const SEARCH_CACHE_MS: u64 = 60 * 60 * 1000;
pub const META_CACHE_MS: u64 = 6 * 60 * 60 * 1000;
pub const THUMBNAIL_CACHE_MS: u64 = 5000;
pub const COVER_SIZE_MIN: u32 = 32;
pub const COVER_SIZE_MAX: u32 = 512;
pub const COVER_SIZE_DEFAULT: u32 = 96;
