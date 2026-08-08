"use strict";

function parseLrc(raw) {
  const lines = [];
  for (const line of String(raw || "").split(/\r?\n/)) {
    const stamps = [...line.matchAll(/\[(\d{1,2}):(\d{2})(?:[.:](\d{1,3}))?\]/g)];
    if (!stamps.length) continue;
    const text = line.replace(/\[[^\]]+\]/g, "").trim();
    if (!text) continue;
    for (const stamp of stamps) {
      const min = Number(stamp[1]) || 0;
      const sec = Number(stamp[2]) || 0;
      const frac = String(stamp[3] || "0").padEnd(3, "0").slice(0, 3);
      lines.push({ at_ms: min * 60000 + sec * 1000 + Number(frac), text });
    }
  }
  return lines.sort((a, b) => a.at_ms - b.at_ms);
}

function mergeTranslation(primary, translation) {
  if (!primary.length || !translation.length) return primary;
  const translatedByTime = new Map(translation.map((line) => [line.at_ms, line.text]));
  return primary.map((line) => {
    const translated = translatedByTime.get(line.at_ms);
    if (!translated || translated === line.text) return line;
    return { ...line, text: `${line.text} / ${translated}` };
  });
}

function normalizeText(value) {
  return String(value || "")
    .normalize("NFKC")
    .toLowerCase()
    .replace(/[()[\]{}【】（）。,，.!！?？'"]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function splitArtists(value) {
  return normalizeText(value)
    .split(/\s*(?:\/|&|,|，|;|；|\band\b|、)\s*/i)
    .map((item) => item.trim())
    .filter(Boolean);
}

function searchScore(song, track) {
  const title = normalizeText(track.title);
  const artist = normalizeText(track.artist);
  const songName = normalizeText(song?.name);
  const songArtists = Array.isArray(song?.artists)
    ? song.artists.map((item) => normalizeText(item?.name)).filter(Boolean)
    : [];
  let score = 0;
  if (songName === title) score += 80;
  else if (songName.includes(title) || title.includes(songName)) score += 45;
  for (const expected of splitArtists(artist)) {
    if (songArtists.some((actual) => actual === expected)) score += 30;
    else if (songArtists.some((actual) => actual.includes(expected) || expected.includes(actual))) score += 15;
  }
  if (Number(song?.duration) > 0 && Number(track.duration_ms) > 0) {
    const delta = Math.abs(Number(song.duration) - Number(track.duration_ms));
    if (delta < 2500) score += 15;
    else if (delta < 8000) score += 8;
  }
  return score;
}

function decodeHtml(value) {
  return String(value || "")
    .replace(/\\n/g, "\n")
    .replace(/&#(\d+);/g, (_, code) => String.fromCharCode(Number(code) || 0))
    .replace(/&#x([0-9a-f]+);/gi, (_, code) => String.fromCharCode(parseInt(code, 16) || 0))
    .replace(/&apos;/g, "'")
    .replace(/&quot;/g, '"')
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&amp;/g, "&");
}

function maybeBase64Text(value) {
  const text = String(value || "").trim();
  if (!text || text.includes("[") || /[^\w+/=\s]/.test(text)) return text;
  try {
    const decoded = Buffer.from(text, "base64").toString("utf8");
    return decoded.includes("[") ? decoded : text;
  } catch {
    return text;
  }
}

module.exports = {
  decodeHtml,
  maybeBase64Text,
  mergeTranslation,
  normalizeText,
  parseLrc,
  searchScore,
  splitArtists,
};
