/*
 * Thrennix browser extension - background script.
 * Chrome/Edge: MV3 service worker (loads common.js via importScripts).
 * Firefox: MV3 event page (manifest "scripts": ["common.js", "background.js"]).
 *
 * Responsibilities:
 *  - catch browser downloads and hand them to the Thrennix app over Native Messaging
 *  - "Download with Thrennix" context menu
 *  - detect video/audio streams per tab (webRequest, observe only) for the popup and
 *    the in-page video button
 */
"use strict";

if (typeof importScripts === "function" && !globalThis.ThrennixCommon) {
  try {
    importScripts("common.js");
  } catch (_) {
    /* nothing we can do; listeners below guard against a missing ThrennixCommon */
  }
}

const C = globalThis.ThrennixCommon;
const api = C.api;
const IS_FIREFOX = typeof api.runtime.getBrowserInfo === "function";
const MB = 1024 * 1024;

/* ------------------------------------------------------------------ settings */

let settingsCache = null;

async function getSettings() {
  if (!settingsCache) settingsCache = await C.loadSettings();
  return settingsCache;
}

try {
  api.storage.onChanged.addListener((_changes, area) => {
    if (area === "sync") settingsCache = null;
  });
} catch (_) {
  /* ignore */
}

/* ---------------------------------------------------------- native messaging */

const PING_TTL_OK = 30 * 1000;
const PING_TTL_FAIL = 3 * 1000;
const PING_TIMEOUT = 8000; // host may need to launch the GUI (up to 5 s)
const DOWNLOAD_TIMEOUT = 15000;
// The first hand-off after the host reported the GUI as not running (or before any ping)
// may include a cold start of the app, so allow longer.
const DOWNLOAD_TIMEOUT_COLD = 30000;

let pingCache = null;
let pingInflight = null;

function nativeSend(msg, timeoutMs) {
  return new Promise((resolve) => {
    let done = false;
    const finish = (r) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      resolve(r);
    };
    const timer = setTimeout(
      () => finish({ type: "error", message: "Timed out waiting for the Thrennix app" }),
      timeoutMs
    );
    try {
      const p = api.runtime.sendNativeMessage(C.HOST_NAME, msg);
      if (p && typeof p.then === "function") {
        p.then(
          (r) => finish(r && typeof r === "object" ? r : { type: "error", message: "Empty reply from Thrennix" }),
          (e) => finish({ type: "error", message: C.errMsg(e) })
        );
      } else {
        finish({ type: "error", message: "Native messaging unavailable" });
      }
    } catch (e) {
      finish({ type: "error", message: C.errMsg(e) });
    }
  });
}

function ping(force) {
  const now = Date.now();
  // "Installed but not running" is cached briefly too, so the popup notices a freshly started app.
  const ttl = pingCache && pingCache.ok && pingCache.running ? PING_TTL_OK : PING_TTL_FAIL;
  if (!force && pingCache && now - pingCache.at < ttl) {
    return Promise.resolve(pingCache);
  }
  if (pingInflight) return pingInflight;
  pingInflight = (async () => {
    const r = await nativeSend({ type: "ping" }, PING_TIMEOUT);
    const res =
      r && r.type === "pong"
        ? {
            ok: true,
            version: typeof r.version === "string" ? r.version : "",
            // running:false = host is installed but the GUI isn't up yet (it starts on demand)
            running: r.running !== false,
            at: Date.now(),
          }
        : { ok: false, error: (r && r.message) || "No response from the Thrennix app", at: Date.now() };
    pingCache = res;
    return res;
  })().finally(() => {
    pingInflight = null;
  });
  return pingInflight;
}

async function sendDownload(payload) {
  const cold = !pingCache || !pingCache.ok || pingCache.running === false;
  const r = await nativeSend(payload, cold ? DOWNLOAD_TIMEOUT_COLD : DOWNLOAD_TIMEOUT);
  if (r && r.type === "ok") {
    pingCache = { ok: true, version: (pingCache && pingCache.version) || "", running: true, at: Date.now() };
    return { ok: true };
  }
  pingCache = null;
  return { ok: false, error: (r && r.message) || "Thrennix did not accept the download" };
}

/* ------------------------------------------------------------------- cookies */

async function cookieHeader(url, opts) {
  opts = opts || {};
  if (!C.isHttpUrl(url) || !api.cookies) return "";
  const base = { url };
  if (opts.incognito) base.storeId = IS_FIREFOX ? "firefox-private" : "1";
  const found = [];
  const tryGet = async (query) => {
    try {
      const r = await api.cookies.getAll(query);
      if (Array.isArray(r)) found.push(...r);
      return true;
    } catch (_) {
      return false;
    }
  };

  // Unpartitioned cookies. Firefox needs firstPartyDomain:null when first-party isolation is on.
  if (IS_FIREFOX) {
    if (!(await tryGet(Object.assign({}, base, { firstPartyDomain: null })))) await tryGet(base);
  } else if (!(await tryGet(base)) && base.storeId) {
    delete base.storeId;
    await tryGet(base);
  }

  // Partitioned (CHIPS / dFPI) cookies for the top-level site, where supported.
  const top = C.safeUrl(opts.topUrl || "");
  if (top && /^https?:$/.test(top.protocol)) {
    const q = Object.assign({}, base, { partitionKey: { topLevelSite: top.origin } });
    if (IS_FIREFOX) q.firstPartyDomain = null;
    await tryGet(q);
  }

  const seen = new Set();
  const list = [];
  for (const c of found) {
    if (!c || typeof c.name !== "string") continue;
    const k = c.name + "\u0000" + c.domain + "\u0000" + c.path;
    if (seen.has(k)) continue;
    seen.add(k);
    list.push(c);
  }
  list.sort((a, b) => (b.path || "").length - (a.path || "").length);
  return list.map((c) => c.name + "=" + c.value).join("; ");
}

/* ------------------------------------------------------------------- helpers */

async function getTab(tabId) {
  if (typeof tabId !== "number" || tabId < 0) return null;
  try {
    return (await api.tabs.get(tabId)) || null;
  } catch (_) {
    return null;
  }
}

async function activeTab() {
  try {
    const t = await api.tabs.query({ active: true, lastFocusedWindow: true });
    return (t && t[0]) || null;
  } catch (_) {
    return null;
  }
}

function basename(p) {
  if (typeof p !== "string" || !p) return "";
  return p.split(/[\\/]/).pop() || "";
}

function kindFromUrl(url) {
  const e = C.extFromUrl(url);
  return e === "m3u8" ? "hls" : e === "mpd" ? "dash" : "file";
}

/* --------------------------------------------------------- download catching */

const handledDownloads = new Set();
const selfInitiated = new Map(); // url -> expiry (downloads we started as a fallback)

function markSelf(url) {
  selfInitiated.set(url, Date.now() + 60 * 1000);
}

function isSelf(url) {
  const t = selfInitiated.get(url);
  if (!t) return false;
  if (t < Date.now()) {
    selfInitiated.delete(url);
    return false;
  }
  return true;
}

function evaluateDownload(item, settings) {
  const url = item.finalUrl || item.url;
  if (!C.isHttpUrl(url)) return null; // also rejects blob:, data:, file:, ftp:
  if (item.byExtensionId && item.byExtensionId === api.runtime.id) return null;
  if (isSelf(item.url) || isSelf(url)) return null;
  if (item.state && item.state !== "in_progress") return null;
  if (isFlaggedDangerous(item)) return null; // leave browser-flagged files to the browser

  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : -1;
  if (size > 0 && size < settings.minSizeMB * MB) return null;

  const ext = C.extFromPath(item.filename) || C.extFromUrl(url) || C.extFromMime(item.mime);
  const wanted = new Set(
    settings.extensions.map((e) => String(e).trim().toLowerCase().replace(/^\./, "")).filter(Boolean)
  );
  if (!ext || !wanted.has(ext)) return null;

  if (C.isSiteDisabled(url, settings.disabledSites)) return null;
  if (item.referrer && C.isSiteDisabled(item.referrer, settings.disabledSites)) return null;
  // Never hand off media from DRM/ToS-restricted services.
  if (C.MEDIA_EXTS.has(ext) && (C.isExcludedSite(url) || C.isExcludedSite(item.referrer))) return null;
  return { url, ext, size };
}

// Chromium sets downloadItem.danger. "file" (generic dangerous file type, e.g. .exe) is
// fine: Thrennix writes Mark-of-the-Web so SmartScreen still applies. Anything else that isn't
// safe/accepted (malware verdicts, uncommon, etc.) stays with the browser.
const OK_DANGER = new Set(["safe", "accepted", "file"]);

function isFlaggedDangerous(item) {
  return !!(item && typeof item.danger === "string" && item.danger && !OK_DANGER.has(item.danger));
}

async function findDownload(id) {
  try {
    const r = await api.downloads.search({ id });
    return (r && r[0]) || null;
  } catch (_) {
    return null;
  }
}

// Pause the browser's copy while Thrennix takes over. Returns "paused", "running" (could not
// pause but still in progress) or "gone" (finished/failed: leave it alone).
async function pauseDownload(id) {
  try {
    await api.downloads.pause(id);
    return "paused";
  } catch (_) {
    const d = await findDownload(id);
    return isAlive(d) ? "running" : "gone";
  }
}

// Firefox may report a paused download as "interrupted" with paused:true.
function isAlive(d) {
  return !!d && (d.state === "in_progress" || (d.paused === true && d.state !== "complete"));
}

async function resumeDownload(id) {
  try {
    const d = await findDownload(id);
    if (d && d.paused && d.state !== "complete") await api.downloads.resume(id);
  } catch (_) {
    /* nothing more we can do */
  }
}

// Only called after Thrennix replied {"type":"ok"}.
async function cancelAndErase(id) {
  try {
    await api.downloads.cancel(id);
  } catch (_) {
    // Could not cancel: if the browser already finished it, keep its entry and file.
    const d = await findDownload(id);
    if (d && d.state === "complete") return;
  }
  try {
    await api.downloads.erase({ id });
  } catch (_) {
    /* ignore */
  }
}

// Returns true if the download was handed to Thrennix (and the browser download cancelled).
async function tryCatchDownload(item) {
  if (!item || typeof item.id !== "number") return false;
  if (handledDownloads.has(item.id)) return false;
  handledDownloads.add(item.id);
  if (handledDownloads.size > 500) handledDownloads.clear();

  const settings = await getSettings();
  if (!settings.catchEnabled) return false;
  const c = evaluateDownload(item, settings);
  if (!c) return false;

  let referer = item.referrer || "";
  if (!referer) {
    const tab = await activeTab();
    if (tab && C.isHttpUrl(tab.url) && !!tab.incognito === !!item.incognito) referer = tab.url;
  }
  if (referer && C.isSiteDisabled(referer, settings.disabledSites)) return false;
  if (C.MEDIA_EXTS.has(c.ext) && C.isExcludedSite(referer)) return false;

  // Hold the browser's copy while we hand off. Every failure path below resumes it;
  // the only path that cancels is the one where Thrennix replied {"type":"ok"}.
  const pause = await pauseDownload(item.id);
  if (pause === "gone") return false;
  let handedOff = false;
  try {
    return (handedOff = await handOff(item, c, referer));
  } catch (_) {
    return false;
  } finally {
    if (!handedOff && pause === "paused") await resumeDownload(item.id);
  }
}

async function handOff(item, c, referer) {
  // Chromium computes the danger verdict after the filename is chosen; re-check it now.
  const fresh = await findDownload(item.id);
  if (fresh && (isFlaggedDangerous(fresh) || !isAlive(fresh))) return false;

  const status = await ping(false);
  if (!status.ok) return false; // app unavailable: leave the browser download alone

  const payload = { type: "download", url: c.url, kind: "file", userAgent: navigator.userAgent };
  const name = basename((fresh && fresh.filename) || item.filename);
  if (name) payload.filename = name;
  if (referer) payload.referer = referer;
  const cookies = await cookieHeader(c.url, { incognito: item.incognito, topUrl: referer });
  if (cookies) payload.cookies = cookies;

  // Hand off first; only cancel the browser's copy once Thrennix has accepted it.
  const r = await sendDownload(payload);
  if (!r.ok) return false;
  await cancelAndErase(item.id);
  return true;
}

try {
  if (api.downloads.onDeterminingFilename) {
    // Chromium: fires once the final URL / suggested filename are known.
    api.downloads.onDeterminingFilename.addListener((item, suggest) => {
      // Accept the browser's filename right away, then pause the item by id while
      // handing it off (a download can't be paused before suggest() is called).
      try {
        suggest();
      } catch (_) {
        /* ignore */
      }
      tryCatchDownload(item).catch(() => false);
    });
  } else {
    // Firefox
    api.downloads.onCreated.addListener((item) => {
      tryCatchDownload(item).catch(() => false);
    });
  }
} catch (_) {
  /* downloads API unavailable */
}

/* ------------------------------------------------------------ YouTube / yt-dlp */

const YT_FORMATS_TIMEOUT = 60000;
const YT_CACHE_MS = 10 * 60 * 1000;
const ytCache = new Map(); // video id -> { at, title, formats }
const ytInflight = new Map(); // video id -> Promise

// Returns { ok, title, formats } or { ok:false, error }. Successful results cached 10 min.
function ytFormats(url) {
  const yt = C.youtubeInfo(url);
  if (!yt) return Promise.resolve({ ok: false, error: "Not a YouTube video URL." });
  if (yt.kind === "live") return Promise.resolve({ ok: false, live: true, error: "Live streams aren't supported." });
  const hit = ytCache.get(yt.id);
  if (hit && Date.now() - hit.at < YT_CACHE_MS) {
    return Promise.resolve({ ok: true, id: yt.id, url: yt.watchUrl, title: hit.title, formats: hit.formats });
  }
  if (ytInflight.has(yt.id)) return ytInflight.get(yt.id);
  const p = (async () => {
    const r = await nativeSend({ type: "ytFormats", url: yt.watchUrl }, YT_FORMATS_TIMEOUT);
    if (!r || r.type !== "ytFormats") {
      return { ok: false, error: (r && r.message) || "Thrennix could not read this video's qualities." };
    }
    const formats = C.ytOrderFormats(r.formats);
    if (!formats.length) return { ok: false, error: "No downloadable qualities found." };
    const title = typeof r.title === "string" ? r.title : "";
    ytCache.set(yt.id, { at: Date.now(), title, formats });
    if (ytCache.size > 200) ytCache.delete(ytCache.keys().next().value);
    return { ok: true, id: yt.id, url: yt.watchUrl, title, formats };
  })().finally(() => ytInflight.delete(yt.id));
  ytInflight.set(yt.id, p);
  return p;
}

async function ytDownload(url, format, title) {
  const yt = C.youtubeInfo(url);
  if (!yt) return { ok: false, error: "Not a YouTube video URL." };
  if (yt.kind === "live") return { ok: false, error: "Live streams aren't supported." };
  if (typeof format !== "string" || !format) return { ok: false, error: "No quality selected." };
  return sendDownload({
    type: "download",
    kind: "ytdlp",
    url: yt.watchUrl,
    format,
    filename: C.sanitizeFilename(title, "video"),
  });
}

// Context menu: best quality (first format) straight away.
async function ytDownloadBest(url, tabId) {
  const r = await ytFormats(url);
  const res = r.ok ? await ytDownload(r.url, r.formats[0].id, r.title) : r;
  if (!res.ok && typeof tabId === "number") {
    // No notifications permission: flag the failure on the badge briefly.
    try {
      await api.action.setBadgeText({ tabId, text: "!" });
      await api.action.setBadgeBackgroundColor({ tabId, color: "#C5221F" });
      setTimeout(() => {
        loadTab(tabId)
          .then((st) => updateBadge(tabId, st.drm ? 0 : st.items.length))
          .catch(() => {});
      }, 5000);
    } catch (_) {
      /* tab gone */
    }
  }
  return res;
}

/* --------------------------------------------------------------- context menu */

const MENU_ID = "thrennix-download";
const MENU_YT_ID = "thrennix-download-yt";

function createMenus() {
  try {
    Promise.resolve(api.contextMenus.removeAll())
      .catch(() => {})
      .then(() => {
        try {
          api.contextMenus.create(
            {
              id: MENU_ID,
              title: "Download with Thrennix",
              contexts: ["link", "image", "video", "audio"],
            },
            () => void api.runtime.lastError
          );
          api.contextMenus.create(
            {
              id: MENU_YT_ID,
              title: "Download video with Thrennix",
              contexts: ["link"],
              targetUrlPatterns: ["*://*.youtube.com/watch*", "*://*.youtube.com/shorts/*", "*://youtu.be/*"],
            },
            () => void api.runtime.lastError
          );
        } catch (_) {
          /* duplicate id etc. */
        }
      });
  } catch (_) {
    /* ignore */
  }
}

try {
  api.runtime.onInstalled.addListener(createMenus);
  if (api.runtime.onStartup) api.runtime.onStartup.addListener(createMenus);
} catch (_) {
  /* ignore */
}

async function onMenuClick(info, tab) {
  if (!info) return;
  const tabId = tab && typeof tab.id === "number" ? tab.id : undefined;
  if (info.menuItemId === MENU_YT_ID) {
    if (C.youtubeInfo(info.linkUrl)) await ytDownloadBest(info.linkUrl, tabId);
    return;
  }
  if (info.menuItemId !== MENU_ID) return;
  // A YouTube video link via the generic item: same as the YouTube item.
  if (info.linkUrl && C.youtubeInfo(info.linkUrl) && !info.mediaType) {
    await ytDownloadBest(info.linkUrl, tabId);
    return;
  }
  const isMediaCtx = info.mediaType === "video" || info.mediaType === "audio";
  const url = isMediaCtx ? info.srcUrl || info.linkUrl : info.linkUrl || info.srcUrl;
  if (!C.isHttpUrl(url)) return;
  const pageUrl = info.frameUrl || info.pageUrl || (tab && tab.url) || "";
  const topUrl = (tab && tab.url) || info.pageUrl || "";
  const kind = kindFromUrl(url);
  const isMedia = isMediaCtx || C.MEDIA_EXTS.has(C.extFromUrl(url));
  if (isMedia && (C.isExcludedSite(url) || C.isExcludedSite(pageUrl) || C.isExcludedSite(topUrl))) return;

  const payload = { type: "download", url, kind, userAgent: navigator.userAgent };
  if (C.isHttpUrl(pageUrl)) payload.referer = pageUrl;
  if (kind !== "file") payload.filename = C.mediaFilename(tab && tab.title, { kind, url });
  const cookies = await cookieHeader(url, { incognito: tab && tab.incognito, topUrl });
  if (cookies) payload.cookies = cookies;

  const r = await sendDownload(payload);
  if (r.ok) return;
  // Thrennix unavailable: fall back to a normal browser download so the click isn't lost.
  if (kind === "file") {
    markSelf(url);
    try {
      await api.downloads.download({ url });
    } catch (_) {
      /* ignore */
    }
  }
}

try {
  api.contextMenus.onClicked.addListener((info, tab) => {
    onMenuClick(info, tab).catch(() => {});
  });
} catch (_) {
  /* ignore */
}

/* ----------------------------------------------------------- media detection */

const HLS_MIMES = new Set([
  "application/vnd.apple.mpegurl",
  "application/x-mpegurl",
  "audio/mpegurl",
  "audio/x-mpegurl",
]);
const DASH_MIMES = new Set(["application/dash+xml"]);
const VIDEO_EXTS = new Set(["mp4", "webm", "mkv", "mov", "m4v"]);
const AUDIO_EXTS = new Set(["mp3", "m4a"]);
const SEGMENT_EXTS = new Set(["ts", "m4s", "m4f", "m4v_seg", "cmfv", "cmfa", "aac"]);
const SEGMENT_MIMES = new Set(["video/mp2t", "video/iso.segment", "audio/iso.segment"]);
const SEGMENT_PATTERN = /(\/seg[-_]|segment|chunk|frag(ment)?[-_/=]|[?&](range|bytestart)=|\/sq\/)/i;
const INIT_PATTERN = /(^|[/_-])init[^/]*\.(mp4|m4s|m4v|m4a|cmfv|cmfa)$/i;
const VOLATILE_PARAMS = ["range", "rn", "rbuf", "bytestart", "byteend", "_"];
const MAX_ITEMS_PER_TAB = 40;
const BADGE_COLOR = "#1D63ED";

function headerMap(headers) {
  const h = {};
  for (const x of headers || []) {
    if (x && x.name) h[x.name.toLowerCase()] = x.value || "";
  }
  return h;
}

function mediaKey(url) {
  const u = C.safeUrl(url);
  if (!u) return url;
  u.hash = "";
  for (const p of VOLATILE_PARAMS) u.searchParams.delete(p);
  return u.toString();
}

function classify(details) {
  if (details.statusCode && details.statusCode !== 200 && details.statusCode !== 206) return null;
  const url = details.url;
  if (!C.isHttpUrl(url)) return null;
  const u = C.safeUrl(url);
  if (!u) return null;
  const h = headerMap(details.responseHeaders);
  const ct = C.baseMime(h["content-type"]);
  const ext = C.extFromUrl(url);

  if (ct.startsWith("text/html") || ct.startsWith("image/") || ct === "application/json") return null;
  if (SEGMENT_EXTS.has(ext) || SEGMENT_MIMES.has(ct) || INIT_PATTERN.test(u.pathname)) return null;

  let size = -1;
  const len = parseInt(h["content-length"], 10);
  const range = /\/\s*(\d+)\s*$/.exec(h["content-range"] || "");
  if (range) size = parseInt(range[1], 10);
  else if (!h["content-range"] && details.statusCode !== 206 && len > 0) size = len;

  let kind = null;
  let audioOnly = false;
  if (HLS_MIMES.has(ct) || ext === "m3u8") kind = "hls";
  else if (DASH_MIMES.has(ct) || ext === "mpd") kind = "dash";
  else if (ct.startsWith("video/") || (VIDEO_EXTS.has(ext) && !ct.startsWith("audio/"))) kind = "file";
  else if (ct.startsWith("audio/") || AUDIO_EXTS.has(ext)) {
    kind = "file";
    audioOnly = true;
  } else return null;

  if (kind === "file") {
    if (audioOnly && !(size > MB)) return null;
    // Segment-looking pieces of a stream: ignore unless clearly a big standalone file.
    if (SEGMENT_PATTERN.test(url) && !(size >= 2 * MB)) return null;
    // Partial response of unknown total that is small: a range/segment, not a file.
    if (size < 0 && details.statusCode === 206 && len > 0 && len < 2 * MB) return null;
    if (size > 0 && size < 256 * 1024) return null;
  }

  return {
    kind,
    audioOnly,
    size: kind === "file" ? size : -1, // a manifest's own length says nothing about the stream
    mime: ct,
    ext: kind === "file" ? ext || C.extFromMime(ct) : ext,
    url: mediaKey(url),
  };
}

/* per-tab state: { items: [...], children: [keys of HLS variants], drm: bool } */

const memStore = new Map();
const sessionStore = api.storage && api.storage.session ? api.storage.session : null;
const locks = new Map();

function emptyState() {
  return { items: [], children: [], drm: false };
}

async function loadTab(tabId) {
  const k = "tab:" + tabId;
  if (memStore.has(k)) return memStore.get(k);
  if (sessionStore) {
    try {
      const r = await sessionStore.get(k);
      if (r && r[k] && Array.isArray(r[k].items)) {
        memStore.set(k, r[k]);
        return r[k];
      }
    } catch (_) {
      /* fall back to memory */
    }
  }
  const st = emptyState();
  memStore.set(k, st);
  return st;
}

async function saveTab(tabId, st) {
  const k = "tab:" + tabId;
  memStore.set(k, st);
  if (sessionStore) {
    try {
      await sessionStore.set({ [k]: st });
    } catch (_) {
      /* quota etc.; memory copy still works */
    }
  }
}

async function dropTab(tabId) {
  const k = "tab:" + tabId;
  memStore.delete(k);
  if (sessionStore) {
    try {
      await sessionStore.remove(k);
    } catch (_) {
      /* ignore */
    }
  }
}

// Serialize read-modify-write of one tab's state. fn(st) returns {changed, value}.
function withTab(tabId, fn) {
  const prev = locks.get(tabId) || Promise.resolve();
  const run = prev
    .catch(() => {})
    .then(async () => {
      const st = await loadTab(tabId);
      const out = (await fn(st)) || {};
      if (out.changed) await saveTab(tabId, st);
      return out.value;
    });
  locks.set(
    tabId,
    run.catch(() => {})
  );
  return run;
}

async function updateBadge(tabId, count) {
  try {
    await api.action.setBadgeText({ tabId, text: count > 0 ? String(count) : "" });
    await api.action.setBadgeBackgroundColor({ tabId, color: BADGE_COLOR });
    if (api.action.setBadgeTextColor) await api.action.setBadgeTextColor({ tabId, color: "#FFFFFF" });
  } catch (_) {
    /* tab gone */
  }
}

function notifyTab(tabId, count) {
  try {
    const p = api.tabs.sendMessage(tabId, { cmd: "mediaUpdated", count });
    if (p && typeof p.catch === "function") p.catch(() => {});
  } catch (_) {
    /* no content script in tab */
  }
}

async function changed(tabId) {
  const st = await loadTab(tabId);
  const count = st.drm ? 0 : st.items.length;
  await updateBadge(tabId, count);
  notifyTab(tabId, count);
}

async function clearTab(tabId) {
  await withTab(tabId, (st) => {
    const had = st.items.length > 0 || st.drm || st.children.length > 0;
    st.items = [];
    st.children = [];
    st.drm = false;
    return { changed: had };
  });
  await updateBadge(tabId, 0);
}

function requestFromExcluded(details) {
  if (C.isExcludedSite(details.url)) return true;
  if (C.isExcludedSite(details.initiator) || C.isExcludedSite(details.documentUrl)) return true;
  if (C.isExcludedSite(details.originUrl)) return true;
  if (Array.isArray(details.frameAncestors)) {
    for (const f of details.frameAncestors) if (f && C.isExcludedSite(f.url)) return true;
  }
  return false;
}

async function onMediaResponse(details, info) {
  if (requestFromExcluded(details)) return;
  // YouTube goes only through the yt-dlp path; its googlevideo responses are segments.
  if (C.isYouTubeRelated(details.url) || C.isYouTubeRelated(details.initiator)) return;
  if (C.isYouTubeRelated(details.documentUrl) || C.isYouTubeRelated(details.originUrl)) return;
  const tab = await getTab(details.tabId);
  if (!tab || C.isExcludedSite(tab.url) || C.isYouTubeRelated(tab.url)) return;
  const referer = C.isHttpUrl(details.documentUrl) ? details.documentUrl : tab.url || "";

  const added = await withTab(details.tabId, (st) => {
    if (st.drm || st.children.includes(info.url)) return {};
    const existing = st.items.find((x) => x.url === info.url);
    if (existing) {
      if (info.size > 0 && info.size > (existing.size || 0)) {
        existing.size = info.size;
        return { changed: true, value: "updated" };
      }
      return {};
    }
    const item = {
      url: info.url,
      kind: info.kind,
      mime: info.mime,
      ext: info.ext,
      size: info.size,
      audioOnly: info.audioOnly,
      variants: [],
      referer,
      ts: Date.now(),
    };
    st.items.push(item);
    if (st.items.length > MAX_ITEMS_PER_TAB) st.items.splice(0, st.items.length - MAX_ITEMS_PER_TAB);
    return { changed: true, value: item };
  });

  if (!added) return;
  await changed(details.tabId);
  if (typeof added === "object" && (added.kind === "hls" || added.kind === "dash")) {
    inspectManifest(details.tabId, added).catch(() => {});
  }
}

/* -------------------------------------------------- HLS / DASH manifest parse */

const MANIFEST_MAX_BYTES = 2 * MB;

async function fetchText(url) {
  const ctrl = typeof AbortController === "function" ? new AbortController() : null;
  const timer = setTimeout(() => ctrl && ctrl.abort(), 10000);
  try {
    // Only manifests the page itself loaded; cookies included so authenticated playlists work.
    const res = await fetch(url, { credentials: "include", signal: ctrl ? ctrl.signal : undefined });
    if (!res.ok) return null;
    const text = await res.text();
    return text.length > MANIFEST_MAX_BYTES ? text.slice(0, MANIFEST_MAX_BYTES) : text;
  } catch (_) {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

function parseAttrs(s) {
  const out = {};
  const re = /([A-Z0-9-]+)=("[^"]*"|[^,]*)/g;
  let m;
  while ((m = re.exec(s))) out[m[1]] = m[2].replace(/^"|"$/g, "");
  return out;
}

function resolveUrl(rel, base) {
  try {
    return new URL(rel, base).toString();
  } catch (_) {
    return null;
  }
}

function parseHls(text, baseUrl) {
  if (!text || !/^﻿?\s*#EXTM3U/.test(text)) return null;
  const drm =
    /#EXT-X-(SESSION-)?KEY:[^\r\n]*METHOD=SAMPLE-AES/i.test(text) ||
    /#EXT-X-(SESSION-)?KEY:[^\r\n]*KEYFORMAT="(?!identity")[^"]+"/i.test(text);
  const lines = text.split(/\r?\n/);
  const variants = [];
  const children = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim();
    if (line.startsWith("#EXT-X-STREAM-INF:")) {
      const a = parseAttrs(line.slice(18));
      let uri = "";
      for (let j = i + 1; j < lines.length; j++) {
        const l = lines[j].trim();
        if (!l) continue;
        if (l.startsWith("#")) {
          if (l.startsWith("#EXT-X-STREAM-INF")) break;
          continue;
        }
        uri = l;
        i = j;
        break;
      }
      const abs = uri && resolveUrl(uri, baseUrl);
      if (!abs) continue;
      const res = /^(\d+)x(\d+)$/i.exec(a.RESOLUTION || "");
      variants.push({
        url: abs,
        width: res ? parseInt(res[1], 10) : 0,
        height: res ? parseInt(res[2], 10) : 0,
        resolution: a.RESOLUTION || "",
        bandwidth: parseInt(a["AVERAGE-BANDWIDTH"] || a.BANDWIDTH, 10) || 0,
      });
      children.push(mediaKey(abs));
    } else if (line.startsWith("#EXT-X-MEDIA:") || line.startsWith("#EXT-X-I-FRAME-STREAM-INF:")) {
      const a = parseAttrs(line.slice(line.indexOf(":") + 1));
      const abs = a.URI && resolveUrl(a.URI, baseUrl);
      if (abs) children.push(mediaKey(abs));
    }
  }
  // Dedupe variants by URL, best first.
  const seen = new Set();
  const uniq = variants
    .filter((v) => (seen.has(v.url) ? false : (seen.add(v.url), true)))
    .sort((a, b) => b.height - a.height || b.bandwidth - a.bandwidth);
  return { drm, variants: uniq, children };
}

function parseDash(text) {
  if (!text || !/<MPD[\s>]/i.test(text)) return null;
  const drm = /<ContentProtection[\s>]/i.test(text);
  let maxHeight = 0;
  const re = /\s(?:height|maxHeight)="(\d+)"/gi;
  let m;
  while ((m = re.exec(text))) maxHeight = Math.max(maxHeight, parseInt(m[1], 10) || 0);
  return { drm, maxHeight };
}

const inspected = new Set();

async function inspectManifest(tabId, item) {
  const key = tabId + "|" + item.url;
  if (inspected.has(key)) return;
  inspected.add(key);
  if (inspected.size > 2000) inspected.clear();

  const text = await fetchText(item.url);
  if (!text) return; // label stays "HLS stream" / "DASH stream"

  if (item.kind === "hls") {
    const p = parseHls(text, item.url);
    if (!p) return;
    await withTab(tabId, (st) => {
      const it = st.items.find((x) => x.url === item.url);
      if (p.drm) {
        st.items = st.items.filter((x) => x.url !== item.url);
        return { changed: true };
      }
      for (const c of p.children) if (!st.children.includes(c)) st.children.push(c);
      if (st.children.length > 500) st.children.splice(0, st.children.length - 500);
      // Hide variant/media playlists of this master that were already listed.
      st.items = st.items.filter((x) => !p.children.includes(x.url));
      if (it) it.variants = p.variants;
      return { changed: true };
    });
  } else {
    const p = parseDash(text);
    if (!p) return;
    await withTab(tabId, (st) => {
      if (p.drm) {
        st.items = st.items.filter((x) => x.url !== item.url);
        return { changed: true };
      }
      const it = st.items.find((x) => x.url === item.url);
      if (it && p.maxHeight) {
        it.maxHeight = p.maxHeight;
        return { changed: true };
      }
      return {};
    });
  }
  await changed(tabId);
}

/* ------------------------------------------------------- webRequest + tabs */

try {
  api.webRequest.onHeadersReceived.addListener(
    (details) => {
      try {
        if (typeof details.tabId !== "number" || details.tabId < 0) return;
        const info = classify(details);
        if (info) onMediaResponse(details, info).catch(() => {});
      } catch (_) {
        /* never throw from a listener */
      }
    },
    { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other"] },
    ["responseHeaders"]
  );

  // Top-frame navigation: start a fresh list for the tab.
  api.webRequest.onBeforeRequest.addListener(
    (details) => {
      try {
        if (details.tabId >= 0) clearTab(details.tabId).catch(() => {});
      } catch (_) {
        /* ignore */
      }
    },
    { urls: ["<all_urls>"], types: ["main_frame"] }
  );
} catch (_) {
  /* webRequest unavailable */
}

try {
  api.tabs.onRemoved.addListener((tabId) => {
    locks.delete(tabId);
    dropTab(tabId).catch(() => {});
  });
  if (api.tabs.onReplaced) {
    api.tabs.onReplaced.addListener((_added, removed) => {
      locks.delete(removed);
      dropTab(removed).catch(() => {});
    });
  }
} catch (_) {
  /* ignore */
}

/* ------------------------------------------------------------------ messages */

function findMedia(st, url) {
  for (const it of st.items) {
    if (it.url === url) return { item: it, variant: null };
    for (const v of it.variants || []) if (v.url === url) return { item: it, variant: v };
  }
  return null;
}

async function downloadMedia(msg, sender) {
  const tabId = sender.tab ? sender.tab.id : msg.tabId;
  const tab = sender.tab || (await getTab(tabId));
  const url = msg.url;
  if (!C.isHttpUrl(url)) return { ok: false, error: "This media cannot be downloaded (not an http(s) URL)." };
  if (!tab) return { ok: false, error: "Tab not found." };
  if (C.isExcludedSite(url) || C.isExcludedSite(tab.url) || C.isExcludedSite(sender.url)) {
    return { ok: false, error: "Downloads are not available on this site." };
  }
  const st = await loadTab(tab.id);
  if (st.drm) return { ok: false, error: "This page uses DRM-protected media." };

  const found = findMedia(st, url);
  const item = found ? found.item : { url, kind: kindFromUrl(url), ext: C.extFromUrl(url) };
  const kind = ["file", "hls", "dash"].includes(msg.kind) ? msg.kind : item.kind || "file";
  // The tab title is the page title; a frame's own document.title is used only for top frames.
  const title = (sender.tab && sender.frameId === 0 && msg.title) || tab.title || msg.title || "";

  const referer = sender.tab && C.isHttpUrl(sender.url) ? sender.url : item.referer || tab.url || "";
  const payload = {
    type: "download",
    url,
    kind,
    filename: C.mediaFilename(title, Object.assign({}, item, { kind })),
    userAgent: navigator.userAgent,
  };
  if (C.isHttpUrl(referer)) payload.referer = referer;
  const cookies = await cookieHeader(url, { incognito: tab.incognito, topUrl: tab.url });
  if (cookies) payload.cookies = cookies;
  return sendDownload(payload);
}

async function handleMessage(msg, sender) {
  switch (msg.cmd) {
    case "getMedia": {
      const tab = sender.tab;
      if (!tab) return { excluded: true, items: [] };
      const settings = await getSettings();
      const excluded = C.isExcludedSite(tab.url) || C.isExcludedSite(sender.url);
      const st = excluded ? emptyState() : await loadTab(tab.id);
      return { excluded, drm: !!st.drm, items: st.drm ? [] : st.items, videoButton: settings.videoButton };
    }
    case "drmDetected": {
      const tab = sender.tab;
      if (!tab) return { ok: false };
      await withTab(tab.id, (st) => {
        st.drm = true;
        st.items = [];
        return { changed: true };
      });
      await changed(tab.id);
      return { ok: true };
    }
    case "downloadMedia":
      return downloadMedia(msg, sender);
    case "ytFormats":
      return ytFormats(msg.url);
    case "ytDownload":
      return ytDownload(msg.url, msg.format, msg.title);
    case "popupState": {
      const tab = await getTab(msg.tabId);
      const settings = await getSettings();
      const excluded = !tab || C.isExcludedSite(tab.url);
      const st = tab && !excluded ? await loadTab(tab.id) : emptyState();
      return {
        settings,
        excluded,
        drm: !!st.drm,
        items: st.drm ? [] : st.items,
        title: (tab && tab.title) || "",
        url: (tab && tab.url) || "",
        youtube: (tab && C.youtubeInfo(tab.url)) || null,
      };
    }
    case "ping":
      return ping(!!msg.force);
    default:
      return { error: "Unknown command" };
  }
}

try {
  api.runtime.onMessage.addListener((msg, sender, sendResponse) => {
    if (!msg || typeof msg.cmd !== "string") return false;
    if (sender && sender.id && sender.id !== api.runtime.id) return false;
    handleMessage(msg, sender || {}).then(
      (r) => {
        try {
          sendResponse(r);
        } catch (_) {
          /* port closed */
        }
      },
      (e) => {
        try {
          sendResponse({ ok: false, error: C.errMsg(e) });
        } catch (_) {
          /* port closed */
        }
      }
    );
    return true; // async response
  });
} catch (_) {
  /* ignore */
}

// Global safety net: never surface unhandled rejections as errors.
try {
  self.addEventListener("unhandledrejection", (e) => {
    try {
      e.preventDefault();
    } catch (_) {
      /* ignore */
    }
  });
} catch (_) {
  /* ignore */
}
