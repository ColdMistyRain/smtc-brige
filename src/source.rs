// Common abstraction over the music providers (NetEase / QQ Music).
//
// `enriched_status` resolves lyrics through the sources as a chain: it tries
// each source in order and stops at the first one that returns lyrics, so the
// "QQ failed → fall back to NetEase" behaviour is expressed by the order of
// `AppState::sources` instead of hand-written if/else.

use async_trait::async_trait;

use crate::common::{LyricResult, MetaInfo, SmtcStatus};

#[async_trait]
pub trait MusicSource: Send + Sync {
    /// Canonical provider name (`"netease"` / `"qqmusic"`).
    fn name(&self) -> &'static str;

    /// Resolve the track identity for `status` (searching by title/artist when
    /// needed), fetch lyrics + metadata, and fill the provider hint fields
    /// (`lyric_provider`, `cover_provider`, …) on `status`.
    async fn resolve(&self, status: &mut SmtcStatus) -> (LyricResult, MetaInfo);

    /// Fetch lyrics directly by id / mid (used by the `/lyrics` endpoint).
    async fn fetch_lyrics(&self, id: u64, mid: &str) -> LyricResult;

    /// Remove expired entries from all of this source's caches.
    /// Returns the number of entries removed.
    async fn sweep_caches(&self) -> usize;
}
