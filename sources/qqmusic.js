"use strict";

function qqCoverUrl(albumMid, size = 300) {
  albumMid = String(albumMid || "").trim();
  if (!albumMid) return "";
  return `https://y.qq.com/music/photo_new/T002R${size}x${size}M000${encodeURIComponent(albumMid)}.jpg?max_age=2592000`;
}

function qqSingerCoverUrl(singerMid, size = 300) {
  singerMid = String(singerMid || "").trim();
  if (!singerMid) return "";
  return `https://y.qq.com/music/photo_new/T001R${size}x${size}M000${encodeURIComponent(singerMid)}.jpg?max_age=2592000`;
}

module.exports = function createQQMusicSource(deps) {
  const {
    common,
    lyricCache,
    qqSearchCache,
    qqMetaCache,
    requestJson,
    LYRIC_CACHE_MS,
    SEARCH_CACHE_MS,
    META_CACHE_MS,
  } = deps;

  function matches(status) {
    return /qqmusic|tencent/i.test(String(status?.source || ""));
  }

  function normalizeSong(song) {
    if (!song) return null;
    const singer = Array.isArray(song.singer) ? song.singer : [];
    const album = song.album || {};
    const id = Number(song.songid || song.id || song.musicid) || 0;
    const mid = String(song.songmid || song.mid || "").trim();
    const albumMid = String(album.mid || song.albummid || "").trim();
    const singerMid = String(singer[0]?.mid || singer[0]?.pmid || "").trim();
    const value = {
      provider: "qqmusic",
      id,
      mid,
      title: song.songname || song.name || "",
      artist: singer.map((item) => item.name).filter(Boolean).join(" / "),
      album: album.name || song.albumname || "",
      duration_ms: (Number(song.interval) || 0) * 1000,
      album_mid: albumMid,
      singer_mid: singerMid,
      cover_url: qqCoverUrl(albumMid) || qqSingerCoverUrl(singerMid),
    };
    if (value.id > 0) qqMetaCache.set(`id:${value.id}`, { at: Date.now(), value });
    if (value.mid) qqMetaCache.set(`mid:${value.mid}`, { at: Date.now(), value });
    if (value.album_mid) qqMetaCache.set(`album:${value.album_mid}`, { at: Date.now(), value });
    return value;
  }

  async function searchSong(track) {
    const title = String(track.title || "").trim();
    const artist = String(track.artist || "").trim();
    if (!title) return null;
    const key = common.normalizeText(`${title} ${artist}`);
    const cached = qqSearchCache.get(key);
    if (cached && Date.now() - cached.at < SEARCH_CACHE_MS) return cached.value;

    const query = encodeURIComponent(`${title} ${artist}`.trim());
    const endpoints = [
      `https://c.y.qq.com/soso/fcgi-bin/client_search_cp?ct=24&qqmusic_ver=1298&new_json=1&remoteplace=txt.yqq.song&searchid=1&t=0&aggr=1&cr=1&catZhida=1&lossless=0&flag_qc=0&p=1&n=8&w=${query}&format=json&platform=yqq.json&needNewCode=0`,
      `https://c.y.qq.com/soso/fcgi-bin/search_cp?g_tk=5381&uin=0&format=json&inCharset=utf-8&outCharset=utf-8&notice=0&platform=yqq&needNewCode=0&w=${query}&zhidaqu=1&catZhida=1&t=0&flag=1&ie=utf-8&sem=1&aggr=0&perpage=8&n=8&p=1&remoteplace=txt.mqq.all`,
    ];
    let songs = [];
    for (const endpoint of endpoints) {
      try {
        const doc = await requestJson(endpoint, { referer: "https://y.qq.com/" });
        songs = doc?.data?.song?.list || doc?.data?.list || [];
        if (songs.length) break;
      } catch {}
    }
    let best = null;
    let bestScore = -1;
    for (const rawSong of songs) {
      const song = normalizeSong(rawSong);
      if (!song) continue;
      const score = common.searchScore({
        name: song.title,
        artists: common.splitArtists(song.artist).map((name) => ({ name })),
        duration: song.duration_ms,
      }, track);
      if (score > bestScore) {
        best = song;
        bestScore = score;
      }
    }
    const value = best && bestScore >= 45 ? best : null;
    qqSearchCache.set(key, { at: Date.now(), value });
    return value;
  }

  async function fetchLyrics(track) {
    const songId = Number(track?.id) || 0;
    let songMid = String(track?.mid || track?.songMid || "").trim();
    if (!songMid && songId > 0) {
      const cachedMeta = qqMetaCache.get(`id:${songId}`);
      if (cachedMeta && Date.now() - cachedMeta.at < META_CACHE_MS) songMid = cachedMeta.value.mid || "";
    }
    if (songId <= 0 && !songMid) return { source: "", lines: [] };

    const cacheKey = `qq:${songId || songMid}`;
    const cached = lyricCache.get(cacheKey);
    if (cached && Date.now() - cached.at < LYRIC_CACHE_MS) return cached.value;

    const params = [
      "nobase64=1",
      "format=json",
      "inCharset=utf8",
      "outCharset=utf-8",
      "notice=0",
      "platform=yqq.json",
      "needNewCode=0",
      "g_tk=5381",
      "hostUin=0",
      "loginUin=0",
      "trans=1",
      `pcachetime=${Date.now()}`,
    ].join("&");
    const idParts = [];
    if (songId > 0 && songMid) idParts.push(`musicid=${songId}&songmid=${encodeURIComponent(songMid)}`);
    if (songId > 0) idParts.push(`musicid=${songId}`);
    if (songMid) idParts.push(`songmid=${encodeURIComponent(songMid)}`);
    const endpoints = idParts.flatMap((idPart) => [
      `https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?${params}&${idPart}`,
      `https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric.fcg?${params}&${idPart}`,
    ]);
    const referer = "https://y.qq.com/n/ryqq/player";
    for (const endpoint of endpoints) {
      try {
        const doc = await requestJson(endpoint, {
          referer,
          headers: { origin: "https://y.qq.com" },
        });
        const lyricRaw = common.decodeHtml(common.maybeBase64Text(doc?.lyric || ""));
        const transRaw = common.decodeHtml(common.maybeBase64Text(doc?.trans || doc?.translate || ""));
        const primary = common.parseLrc(lyricRaw);
        const translated = common.parseLrc(transRaw);
        const lines = common.mergeTranslation(primary, translated);
        if (lines.length) {
          const value = { source: `qqmusic:${songId || songMid}`, translation_line_count: translated.length, lines };
          lyricCache.set(cacheKey, { at: Date.now(), value });
          return value;
        }
      } catch {}
    }

    const value = { source: "", translation_line_count: 0, lines: [] };
    lyricCache.set(cacheKey, { at: Date.now(), value });
    return value;
  }

  async function resolve(status) {
    const qq = await searchSong(status);
    if (!qq) {
      status.lyric_provider = "qqmusic";
      status.lyric_id_text = "";
      status.cover_provider = "qqmusic";
      status.cover_id_text = "";
      return { found: { source: "", lines: [] }, meta: {} };
    }
    status.qq_song_id = qq.id;
    status.qq_song_mid = qq.mid;
    status.qq_album_mid = qq.album_mid;
    status.lyric_provider = "qqmusic";
    status.lyric_id_text = qq.id ? String(qq.id) : qq.mid;
    status.cover_provider = qq.album_mid ? "qqmusic" : (qq.singer_mid ? "qqartist" : "smtc");
    status.cover_id_text = qq.album_mid || qq.singer_mid || (qq.id ? String(qq.id) : qq.mid || "current");
    const found = await fetchLyrics(qq);
    return { found, meta: { ...qq, cover_url: qq.cover_url || "smtc:current" } };
  }

  return {
    id: "qqmusic",
    aliases: ["qq", "qqartist"],
    matches,
    resolve,
    fetchLyrics,
    coverCandidates: (id, provider) => {
      if (provider === "qqartist") return [92, 150, 300, 500].map((size) => qqSingerCoverUrl(id, size));
      return [92, 150, 300, 500].map((size) => qqCoverUrl(id, size));
    },
    normalizeCover: true,
  };
};
