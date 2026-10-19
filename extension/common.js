/*
 * Thrennix browser extension - shared helpers.
 * Loaded by the background (service worker via importScripts / Firefox event page via
 * manifest "scripts"), the content script, the popup and the options page.
 * Must be side-effect free apart from defining globalThis.ThrennixCommon.
 */
(function () {
  "use strict";
  if (globalThis.ThrennixCommon) return;

  const api = globalThis.browser ?? globalThis.chrome;

  const HOST_NAME = "com.thrennix.host";

  /*
   * DRM-protected streaming services.
   * These services protect their catalogue with DRM (Widevine/PlayReady/FairPlay), so
   * Thrennix never detects media on them, never shows the video button and never offers to
   * download their streams. Entries are a domain (matches the domain and all subdomains)
   * optionally followed by a path prefix.
   * YouTube is NOT listed here: it is handled only by the dedicated yt-dlp path
   * (ytFormats / download kind "ytdlp"); see YOUTUBE_DOMAINS. YouTube Music stays excluded.
   */
  const EXCLUDED_SITES = [
    "music.youtube.com",
    "netflix.com",
    "nflxvideo.net",
    "primevideo.com",
    "amazon.com/gp/video",
    "disneyplus.com",
    "hotstar.com",
    "hulu.com",
    "max.com",
    "hbomax.com",
    "peacocktv.com",
    "paramountplus.com",
    "crunchyroll.com",
    "spotify.com",
    "scdn.co",
    "music.apple.com",
    "tv.apple.com",
    "deezer.com",
    "tidal.com",
  ];

  // YouTube-related hosts. The generic webRequest detector ignores these entirely
  // (googlevideo.com responses are stream segments, not usable URLs).
  const YOUTUBE_DOMAINS = ["youtube.com", "youtu.be", "youtube-nocookie.com", "googlevideo.com", "ytimg.com"];

  const DEFAULT_EXTENSIONS = [
    // archives
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "lz", "lzma", "cab", "arj",
    // executables / installers / packages
    "exe", "msi", "msix", "msixbundle", "appx", "appxbundle", "apk", "xapk", "deb", "rpm",
    "pkg", "bin", "run", "jar",
    // disk images
    "iso", "img", "dmg", "vhd", "vhdx", "vmdk", "ova",
    // video
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "3gp", "ts",
    // audio
    "mp3", "m4a", "flac", "wav", "aac", "ogg", "opus", "wma",
    // documents
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "epub",
  ];

  const DEFAULT_SETTINGS = {
    catchEnabled: true,
    minSizeMB: 1,
    extensions: DEFAULT_EXTENSIONS,
    disabledSites: [],
    videoButton: true,
  };

  // MIME types -> extension, used when the URL/filename has no usable extension.
  const MIME_EXT = {
    "application/zip": "zip",
    "application/x-zip-compressed": "zip",
    "application/x-rar-compressed": "rar",
    "application/vnd.rar": "rar",
    "application/x-7z-compressed": "7z",
    "application/x-tar": "tar",
    "application/gzip": "gz",
    "application/x-gzip": "gz",
    "application/x-bzip2": "bz2",
    "application/x-xz": "xz",
    "application/zstd": "zst",
    "application/x-msdownload": "exe",
    "application/x-dosexec": "exe",
    "application/vnd.microsoft.portable-executable": "exe",
    "application/x-msi": "msi",
    "application/x-ms-installer": "msi",
    "application/vnd.android.package-archive": "apk",
    "application/x-iso9660-image": "iso",
    "application/x-apple-diskimage": "dmg",
    "application/java-archive": "jar",
    "application/pdf": "pdf",
    "application/msword": "doc",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document": "docx",
    "application/vnd.ms-excel": "xls",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet": "xlsx",
    "application/vnd.ms-powerpoint": "ppt",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation": "pptx",
    "application/epub+zip": "epub",
    "video/mp4": "mp4",
    "video/webm": "webm",
    "video/x-matroska": "mkv",
    "video/quicktime": "mov",
    "video/x-msvideo": "avi",
    "video/x-flv": "flv",
    "video/x-ms-wmv": "wmv",
    "video/mpeg": "mpg",
    "audio/mpeg": "mp3",
    "audio/mp4": "m4a",
    "audio/x-m4a": "m4a",
    "audio/flac": "flac",
    "audio/wav": "wav",
    "audio/x-wav": "wav",
    "audio/aac": "aac",
    "audio/ogg": "ogg",
    "audio/opus": "opus",
  };

  const MEDIA_EXTS = new Set([
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "3gp", "ts",
    "mp3", "m4a", "flac", "wav", "aac", "ogg", "opus", "wma", "m3u8", "mpd",
  ]);

  function safeUrl(u) {
    try {
      return new URL(u);
    } catch (_) {
      return null;
    }
  }

  function isHttpUrl(u) {
    return typeof u === "string" && /^https?:\/\//i.test(u);
  }

  function normalizeDomain(d) {
    if (typeof d !== "string") return "";
    let s = d.trim().toLowerCase();
    if (!s || s.startsWith("#")) return "";
    s = s.replace(/^[a-z]+:\/\//, "").replace(/^\*\./, "").replace(/^www\./, "");
    s = s.replace(/[?#].*$/, "").replace(/\/+$/, "");
    return s;
  }

  function hostMatches(host, domain) {
    host = (host || "").toLowerCase().replace(/\.$/, "");
    return host === domain || host.endsWith("." + domain);
  }

  // entry: "domain" or "domain/path/prefix"
  function urlMatchesEntry(url, entry) {
    const u = typeof url === "string" ? safeUrl(url) : url;
    if (!u || !entry) return false;
    const slash = entry.indexOf("/");
    const domain = slash < 0 ? entry : entry.slice(0, slash);
    const path = slash < 0 ? "" : entry.slice(slash);
    if (!hostMatches(u.hostname, domain)) return false;
    return !path || u.pathname.toLowerCase().startsWith(path.toLowerCase());
  }

  function isExcludedSite(url) {
    if (!url) return false;
    const u = safeUrl(url);
    if (!u) return false;
    return EXCLUDED_SITES.some((e) => urlMatchesEntry(u, e));
  }

  function isSiteDisabled(url, disabledSites) {
    if (!url || !Array.isArray(disabledSites)) return false;
    const u = safeUrl(url);
    if (!u) return false;
    return disabledSites.some((d) => {
      const n = normalizeDomain(d);
      return n && urlMatchesEntry(u, n);
    });
  }

  function isYouTubeRelated(url) {
    const u = url ? safeUrl(url) : null;
    return !!u && YOUTUBE_DOMAINS.some((d) => hostMatches(u.hostname, d));
  }

  const YT_ID = /^[A-Za-z0-9_-]{11}$/;

  /*
   * Parse a YouTube URL. Returns {id, kind, watchUrl} where kind is
   * "watch" | "shorts" | "live" | "embed", or null if it is not a single-video URL.
   * watchUrl is canonical (https://www.youtube.com/watch?v=<id>, no list/index/t params).
   */
  function youtubeInfo(url) {
    const u = url ? safeUrl(url) : null;
    if (!u || !/^https?:$/.test(u.protocol)) return null;
    const host = u.hostname.toLowerCase();
    if (hostMatches(host, "music.youtube.com")) return null;
    const p = u.pathname.split("/").filter(Boolean);
    let id = null;
    let kind = "watch";
    if (host === "youtu.be") {
      id = p[0];
    } else if (hostMatches(host, "youtube.com") || hostMatches(host, "youtube-nocookie.com")) {
      if (p[0] === "watch") id = u.searchParams.get("v");
      else if (p[0] === "shorts") (id = p[1]), (kind = "shorts");
      else if (p[0] === "live") (id = p[1]), (kind = "live");
      else if (p[0] === "embed") (id = p[1]), (kind = "embed");
      else if (p[0] === "v") id = p[1];
    } else {
      return null;
    }
    if (!id || !YT_ID.test(id)) return null;
    return { id, kind, watchUrl: "https://www.youtube.com/watch?v=" + id };
  }

  // Order yt-dlp formats like IDM: videos (highest first, as the app sent them), audio last.
  function ytOrderFormats(formats) {
    const valid = (Array.isArray(formats) ? formats : []).filter((f) => f && typeof f.id === "string" && f.id);
    const video = valid.filter((f) => typeof f.height === "number" && f.height > 0);
    const audio = valid.filter((f) => !(typeof f.height === "number" && f.height > 0));
    return video.concat(audio);
  }

  // "MP4 file, quality 1080p HD (≈ 123 MB)" / "M4A audio file (≈ 4.4 MB)"
  function ytFormatLabel(f) {
    const label = typeof f.label === "string" ? f.label : "";
    const parts = label.split("·").map((x) => x.trim());
    let s;
    if (typeof f.height === "number" && f.height > 0) {
      const container = parts.length > 1 && parts[0] && !/audio/i.test(parts[0]) ? parts[0].toUpperCase() : "MP4";
      s = container + " file, quality " + f.height + "p" + (f.height >= 720 ? " HD" : "");
    } else {
      const codec = parts.length > 1 ? parts[parts.length - 1].toUpperCase() : "";
      s = codec ? codec + " audio file" : label || "Audio file";
    }
    const size = formatSize(typeof f.filesize === "number" ? f.filesize : NaN);
    return size ? s + " (≈ " + size + ")" : s;
  }

  function extFromPath(pathOrName) {
    if (typeof pathOrName !== "string") return "";
    const clean = pathOrName.split(/[?#]/)[0];
    const base = clean.split(/[\\/]/).pop() || "";
    const m = /\.([a-z0-9]{1,12})$/i.exec(base);
    return m ? m[1].toLowerCase() : "";
  }

  function extFromUrl(url) {
    const u = safeUrl(url);
    if (!u) return "";
    let p = u.pathname;
    try {
      p = decodeURIComponent(p);
    } catch (_) {
      /* keep raw */
    }
    return extFromPath(p);
  }

  function baseMime(m) {
    return typeof m === "string" ? m.split(";")[0].trim().toLowerCase() : "";
  }

  function extFromMime(m) {
    return MIME_EXT[baseMime(m)] || "";
  }

  const WINDOWS_RESERVED = /^(con|prn|aux|nul|com[0-9]|lpt[0-9])$/i;

  function sanitizeFilename(name, fallback) {
    let s = typeof name === "string" ? name : "";
    // eslint-disable-next-line no-control-regex
    s = s.replace(/[<>:"/\\|?*\u0000-\u001f\u007f]+/g, " ");
    s = s.replace(/\s+/g, " ").trim().replace(/^[.\s]+|[.\s]+$/g, "");
    if (s.length > 120) s = s.slice(0, 120).trim();
    if (!s) s = fallback || "video";
    if (WINDOWS_RESERVED.test(s)) s = "_" + s;
    return s;
  }

  function formatSize(bytes) {
    if (typeof bytes !== "number" || !isFinite(bytes) || bytes <= 0) return "";
    const units = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    let v = bytes;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return (v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)) + " " + units[i];
  }

  function formatBitrate(bps) {
    if (!bps || !isFinite(bps)) return "";
    if (bps >= 1e6) return (bps / 1e6).toFixed(1) + " Mbps";
    return Math.round(bps / 1e3) + " kbps";
  }

  function mediaLabel(item, variant) {
    const parts = [];
    if (item.kind === "hls") parts.push(variant ? "HLS" : "HLS stream");
    else if (item.kind === "dash") parts.push(variant ? "DASH" : "DASH stream");
    else parts.push((item.ext || (item.audioOnly ? "audio" : "video")).toUpperCase());
    if (variant) {
      if (variant.height) parts.push(variant.height + "p");
      else if (variant.resolution) parts.push(variant.resolution);
      if (variant.bandwidth) parts.push(formatBitrate(variant.bandwidth));
    }
    else if (item.maxHeight) parts.push("up to " + item.maxHeight + "p");
    const size = formatSize(item.size);
    if (size && !variant) parts.push(size);
    if (item.audioOnly && item.kind === "file") parts.push("audio");
    return parts.join(" · ");
  }

  // Flatten detected media into user-facing choices (one per HLS variant).
  function mediaChoices(items) {
    const out = [];
    for (const it of Array.isArray(items) ? items : []) {
      if (!it || !it.url) continue;
      if (Array.isArray(it.variants) && it.variants.length) {
        for (const v of it.variants) {
          if (v && v.url) out.push({ label: mediaLabel(it, v), url: v.url, kind: it.kind, item: it });
        }
      } else {
        out.push({ label: mediaLabel(it), url: it.url, kind: it.kind, item: it });
      }
    }
    return out;
  }

  // Extension to use for a downloaded media item.
  function mediaExtension(item) {
    if (!item) return "mp4";
    if (item.kind === "hls" || item.kind === "dash") return "mp4";
    const e = item.ext || extFromUrl(item.url) || extFromMime(item.mime);
    if (e && MEDIA_EXTS.has(e) && e !== "m3u8" && e !== "mpd") return e;
    return item.audioOnly ? "mp3" : "mp4";
  }

  function mediaFilename(title, item) {
    return sanitizeFilename(title, "video") + "." + mediaExtension(item);
  }

  function errMsg(e) {
    if (!e) return "Unknown error";
    if (typeof e === "string") return e;
    return e.message || String(e);
  }

  async function loadSettings() {
    try {
      const got = await api.storage.sync.get(DEFAULT_SETTINGS);
      const s = Object.assign({}, DEFAULT_SETTINGS, got || {});
      if (!Array.isArray(s.extensions)) s.extensions = DEFAULT_EXTENSIONS.slice();
      if (!Array.isArray(s.disabledSites)) s.disabledSites = [];
      s.minSizeMB = Number(s.minSizeMB);
      if (!isFinite(s.minSizeMB) || s.minSizeMB < 0) s.minSizeMB = DEFAULT_SETTINGS.minSizeMB;
      s.catchEnabled = s.catchEnabled !== false;
      s.videoButton = s.videoButton !== false;
      return s;
    } catch (_) {
      return Object.assign({}, DEFAULT_SETTINGS, { extensions: DEFAULT_EXTENSIONS.slice() });
    }
  }

  async function saveSettings(patch) {
    try {
      await api.storage.sync.set(patch);
      return true;
    } catch (_) {
      return false;
    }
  }

  // runtime.sendMessage wrapper that never throws (returns null on failure).
  async function sendToBackground(msg) {
    try {
      if (!api || !api.runtime || !api.runtime.id) return null;
      const r = await api.runtime.sendMessage(msg);
      return r === undefined ? null : r;
    } catch (_) {
      return null;
    }
  }

  globalThis.ThrennixCommon = Object.freeze({
    api,
    HOST_NAME,
    EXCLUDED_SITES,
    DEFAULT_EXTENSIONS,
    DEFAULT_SETTINGS,
    MEDIA_EXTS,
    safeUrl,
    isHttpUrl,
    normalizeDomain,
    isExcludedSite,
    YOUTUBE_DOMAINS,
    isYouTubeRelated,
    youtubeInfo,
    ytOrderFormats,
    ytFormatLabel,
    isSiteDisabled,
    extFromPath,
    extFromUrl,
    extFromMime,
    baseMime,
    sanitizeFilename,
    formatSize,
    formatBitrate,
    mediaLabel,
    mediaChoices,
    mediaExtension,
    mediaFilename,
    errMsg,
    loadSettings,
    saveSettings,
    sendToBackground,
  });
})();
