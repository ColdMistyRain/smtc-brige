// 对音乐提供商（网易云 / QQ 音乐）的通用抽象。
//
// `enriched_status` 按链式顺序通过各音乐源解析歌词：依次尝试每个源，
// 在第一个返回歌词的源处停止，因此"QQ 失败 → 回退到网易云"的行为
// 由 `AppState::sources` 的顺序表达，而不是手写 if/else。

use async_trait::async_trait;

use crate::common::{LyricResult, MetaInfo, SmtcStatus};

#[async_trait]
pub trait MusicSource: Send + Sync {
    /// 规范化的提供商名称（`"netease"` / `"qqmusic"`）。
    fn name(&self) -> &'static str;

    /// 为 `status` 解析曲目标识（必要时按标题/歌手搜索），获取歌词与元数据，
    /// 并填充 `status` 上的提供商提示字段（`lyric_provider`、`cover_provider` …）。
    async fn resolve(&self, status: &mut SmtcStatus) -> (LyricResult, MetaInfo);

    /// 直接按 id / mid 获取歌词（供 `/lyrics` 端点使用）。
    async fn fetch_lyrics(&self, id: u64, mid: &str) -> LyricResult;

    /// 从该源的所有缓存中移除过期的条目。
    /// 返回被移除的条目数量。
    async fn sweep_caches(&self) -> usize;
}
