"use strict";

module.exports = function createNeteaseSource(deps) {
  const {
    common,
    httpsJson,
    lyricCache,
    searchCache,
    metaCache,
    LYRIC_CACHE_MS,
    SEARCH_CACHE_MS,
    META_CACHE_MS,
  } = deps;

  async function searchSong(track) {
    const title = String(track.title || "").trim();
    const artist = String(track.artist || "").trim();
    if (!title) return 0;
    const key = common.normalizeText(`${title} ${artist}`);
    const cached = searchCache.get(key);
    if (cached && Date.now() - cached.at < SEARCH_CACHE_MS) return cached.id;

    const query = encodeURIComponent(`${title} ${artist}`.trim());
    const url = `https://music.163.com/api/search/get/web?csrf_token=&type=1&limit=8&s=${query}`;
    const doc = await httpsJson(url);
    const songs = doc?.result?.songs || [];
    let best = null;
    let bestScore = -1;
    for (const song of songs) {
      const score = common.searchScore(song, track);
      if (score > bestScore) {
        best = song;
        bestScore = score;
      }
    }
    const id = best && bestScore >= 45 ? Number(best.id) || 0 : 0;
    searchCache.set(key, { at: Date.now(), id });
    return id;
  }

  async function fetchLyrics(input) {
    const ncmId = Number(typeof input === "object" ? input.id : input) || 0;
    if (ncmId <= 0) return { source: "", lines: [] };

    const cached = lyricCache.get(ncmId);
    if (cached && Date.now() - cached.at < LYRIC_CACHE_MS) return cached.value;

    const url = `https://music.163.com/api/song/lyric?id=${ncmId}&lv=-1&kv=-1&tv=-1`;
    const doc = await httpsJson(url);
    const primary = common.parseLrc(doc?.lrc?.lyric || "");
    const translated = common.parseLrc(doc?.tlyric?.lyric || "");
    const lines = common.mergeTranslation(primary, translated);
    const value = { source: `netease:${ncmId}`, translation_line_count: translated.length, lines };
    lyricCache.set(ncmId, { at: Date.now(), value });
    return value;
  }

  async function fetchMeta(input) {
    const ncmId = Number(typeof input === "object" ? input.id : input) || 0;
    if (ncmId <= 0) return {};

    const cached = metaCache.get(ncmId);
    if (cached && Date.now() - cached.at < META_CACHE_MS) return cached.value;

    const url = `https://music.163.com/api/song/detail/?ids=%5B${ncmId}%5D`;
    const doc = await httpsJson(url);
    const song = Array.isArray(doc?.songs) ? doc.songs[0] : null;
    const album = song?.album || song?.al || {};
    const coverUrl = album.picUrl || album.pic || "";
    const value = {
      id: ncmId,
      title: song?.name || "",
      album: album.name || "",
      duration_ms: Number(song?.duration || song?.dt) || 0,
      cover_url: coverUrl ? `${coverUrl}?param=92y92&type=jpg` : "",
    };
    metaCache.set(ncmId, { at: Date.now(), value });
    return value;
  }

  async function resolve(status) {
    let ncmId = Number(status.ncm_id) || 0;
    let sourceHint = "smtc";
    if (ncmId <= 0) {
      ncmId = await searchSong(status);
      sourceHint = "search";
      status.ncm_id = ncmId;
    }
    const found = await fetchLyrics(ncmId);
    const meta = await fetchMeta(ncmId);
    status.lyric_provider = "netease";
    status.lyric_id_text = status.ncm_id ? String(status.ncm_id) : "";
    status.cover_provider = "netease";
    status.cover_id_text = status.ncm_id ? String(status.ncm_id) : "";
    return {
      found: {
        ...found,
        source: found.source ? `${found.source}:${sourceHint}` : "",
      },
      meta,
    };
  }

  return {
    id: "netease",
    matches: () => true,
    resolve,
    fetchLyrics,
    fetchMeta,
    coverCandidates: async (id) => {
      const meta = await fetchMeta(id);
      return meta.cover_url || "";
    },
  };
};
