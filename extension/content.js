/*
 * Thrennix browser extension - content script (all frames).
 * Shows a floating "Download video" button over large playing videos when the
 * background has detected downloadable media for this tab. Also reports EME/DRM use
 * so the background stops offering downloads for the tab.
 */
(function () {
  "use strict";
  try {
    if (window.__thrennixContentLoaded) return;
    window.__thrennixContentLoaded = true;
  } catch (_) {
    return;
  }

  const C = globalThis.ThrennixCommon;
  if (!C || !C.api) return;
  const api = C.api;
  if (!/^https?:$/.test(location.protocol)) return;
  if (C.isExcludedSite(location.href)) return;

  // YouTube mode: top-level youtube.com pages and youtube.com/embed iframes use the yt-dlp
  // path (ytFormats -> download kind "ytdlp"). Other YouTube frames get nothing.
  let isTop = true;
  try {
    isTop = window.top === window;
  } catch (_) {
    isTop = false;
  }
  const onYouTube = C.isYouTubeRelated(location.href);
  const ytMode = onYouTube && (isTop || /^\/embed\//.test(location.pathname));
  if (onYouTube && !ytMode) return;

  const MIN_W = 200;
  const MIN_H = 120;
  const AUTO_HIDE_MS = 5000;
  const SCAN_MS = 1000;

  // ready: background has answered at least once (until then we don't know if the tab is excluded)
  const state = { ready: false, excluded: false, drm: false, items: [], videoButton: true };
  let refreshing = null;
  const dismissed = new WeakSet();
  const videos = document.getElementsByTagName("video"); // live collection
  let activeVideo = null;
  let ui = null;
  let hideTimer = null;
  let scanTimer = null;
  let posTimer = null;
  let resizeObs = null;
  let stopped = false;
  let drmReported = false;
  let lastMove = 0;
  // YouTube state (reset per video id)
  let ytShownId = null;
  let lastHref = location.href;
  const ytDismissed = new Set();
  const ytDrm = new Set();
  const YT_DLP_CMD = "winget install yt-dlp.yt-dlp";

  /* ------------------------------------------------------------ background */

  function refresh() {
    if (!refreshing) {
      refreshing = doRefresh().finally(() => {
        refreshing = null;
      });
    }
    return refreshing;
  }

  async function doRefresh() {
    const r = await C.sendToBackground({ cmd: "getMedia" });
    if (!r) return;
    state.ready = true;
    state.excluded = !!r.excluded;
    // On YouTube DRM is tracked per video locally (SPA navigation never clears the tab flag).
    if (!ytMode) state.drm = state.drm || !!r.drm;
    state.items = Array.isArray(r.items) ? r.items : [];
    state.videoButton = r.videoButton !== false;
    if (state.excluded) return stop();
    evaluate();
  }

  function reportDrm() {
    if (ytMode) {
      const y = C.youtubeInfo(location.href);
      if (y) ytDrm.add(y.id);
      removeUi();
      return;
    }
    if (drmReported) return;
    drmReported = true;
    state.drm = true;
    removeUi();
    C.sendToBackground({ cmd: "drmDetected" });
  }

  /* ------------------------------------------------------------ detection */

  function isPlaying(v) {
    return !v.paused && !v.ended && v.readyState > 2;
  }

  function checkDrm(v) {
    try {
      if (v.mediaKeys) reportDrm();
    } catch (_) {
      /* ignore */
    }
  }

  function inViewport(r) {
    const vw = window.innerWidth || document.documentElement.clientWidth;
    const vh = window.innerHeight || document.documentElement.clientHeight;
    return r.bottom > 0 && r.right > 0 && r.top < vh && r.left < vw;
  }

  function pickVideo() {
    let best = null;
    let bestScore = 0;
    for (let i = 0; i < videos.length; i++) {
      const v = videos[i];
      checkDrm(v);
      if (state.drm) return null;
      if (dismissed.has(v)) continue;
      const r = v.getBoundingClientRect();
      if (r.width < MIN_W || r.height < MIN_H || !inViewport(r)) continue;
      const playing = isPlaying(v);
      if (!playing && state.items.length === 0) continue;
      const score = r.width * r.height * (playing ? 4 : 1);
      if (score > bestScore) {
        bestScore = score;
        best = v;
      }
    }
    return best;
  }

  // Largest visible <video> on the page -> its YouTube player container (#movie_player).
  function pickYtAnchor(y) {
    let best = null;
    let bestScore = 0;
    for (let i = 0; i < videos.length; i++) {
      const v = videos[i];
      const r = v.getBoundingClientRect();
      if (r.width < MIN_W || r.height < MIN_H || !inViewport(r)) continue;
      const score = r.width * r.height * (isPlaying(v) ? 4 : 1);
      if (score > bestScore) {
        bestScore = score;
        best = v;
      }
    }
    if (!best) return null;
    try {
      if (best.mediaKeys) {
        ytDrm.add(y.id);
        return null;
      }
    } catch (_) {
      /* ignore */
    }
    const player = best.closest(".html5-video-player") || best;
    if (player.classList && player.classList.contains("ad-showing")) return null; // ad playing
    return player;
  }

  function evaluateYouTube() {
    if (!state.videoButton) return removeUi();
    const y = C.youtubeInfo(location.href);
    if (!y || ytDismissed.has(y.id) || ytDrm.has(y.id)) return removeUi();
    const a = pickYtAnchor(y);
    if (!a) {
      if (ui && !(ui.menuOpen && activeVideo && activeVideo.isConnected)) removeUi();
      return;
    }
    if (!ui || !ui.host.isConnected || a !== activeVideo || ytShownId !== y.id) {
      if (ytShownId !== y.id) removeUi();
      showFor(a);
      ytShownId = y.id;
    } else {
      position();
    }
  }

  function onYtNavigate() {
    lastHref = location.href;
    removeUi();
    evaluate();
  }

  function evaluate() {
    if (stopped || !state.ready) return;
    if (ytMode) {
      try {
        evaluateYouTube();
      } catch (_) {
        /* never throw */
      }
      return;
    }
    try {
      if (state.drm || state.excluded || !state.videoButton) {
        removeUi();
        return;
      }
      const v = pickVideo();
      if (!v) {
        if (ui && !(ui.menuOpen && activeVideo && activeVideo.isConnected)) removeUi();
        return;
      }
      if (v !== activeVideo || !ui) showFor(v);
      else position();
    } catch (_) {
      /* never throw */
    }
  }

  /* ------------------------------------------------------------------- UI */

  const CSS = `
    :host { all: initial; }
    .wrap { position: relative; display: flex; gap: 2px; align-items: stretch;
      font: 13px/1.2 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
      transition: opacity .2s ease; pointer-events: auto; }
    .wrap.hidden { opacity: 0; pointer-events: none; }
    button { all: unset; box-sizing: border-box; cursor: pointer; color: #fff;
      background: rgba(29, 99, 237, .95); padding: 6px 10px; white-space: nowrap;
      box-shadow: 0 2px 8px rgba(0,0,0,.35); }
    button:hover { background: #1550c4; }
    button:focus-visible { outline: 2px solid #fff; outline-offset: -3px; }
    .dl { border-radius: 6px 0 0 6px; font-weight: 600; }
    .x { border-radius: 0 6px 6px 0; padding: 6px 8px; }
    .menu { position: absolute; right: 0; top: calc(100% + 4px); min-width: 240px; max-width: 420px;
      max-height: min(440px, 70vh); overflow: auto; background: #1f2329; color: #e8eaed; border-radius: 8px;
      box-shadow: 0 6px 24px rgba(0,0,0,.45); padding: 4px; }
    .menu[hidden] { display: none; }
    .menu button { display: block; width: 100%; background: transparent; box-shadow: none;
      padding: 8px 10px; border-radius: 6px; color: #e8eaed; white-space: normal; }
    .menu button:hover { background: rgba(255,255,255,.1); }
    .menu.wide { min-width: min(520px, 90vw); max-width: min(680px, 90vw); }
    .menu button.yt { display: flex; gap: 12px; align-items: baseline; white-space: nowrap; }
    .yt .t { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
    .yt .q { flex: none; font-weight: 600; }
    .msg { padding: 8px 10px; color: #bdc1c6; }
    .msg.ok { color: #81c995; }
    .msg.err { color: #f28b82; }
    .msg code { font-family: ui-monospace, Consolas, monospace; user-select: text; }
    .menu button.copy { display: inline-block; width: auto; margin: 0 10px 8px; padding: 4px 10px;
      background: rgba(29, 99, 237, .95); color: #fff; }
    .menu button.copy:hover { background: #1550c4; }
    .spin { display: inline-block; width: 11px; height: 11px; margin-right: 8px; vertical-align: -1px;
      border: 2px solid currentColor; border-right-color: transparent; border-radius: 50%;
      animation: thrennix-spin .8s linear infinite; }
    @keyframes thrennix-spin { to { transform: rotate(360deg); } }
    @media (prefers-color-scheme: light) {
      .menu { background: #ffffff; color: #1f2329; box-shadow: 0 6px 24px rgba(0,0,0,.25); }
      .menu button { color: #1f2329; }
      .menu button:hover { background: rgba(0,0,0,.06); }
      .menu button.copy { color: #fff; }
      .msg { color: #5f6672; }
      .msg.ok { color: #188038; }
      .msg.err { color: #c5221f; }
    }
  `;

  function stopEvent(e) {
    e.stopPropagation();
  }

  function createUi() {
    const host = document.createElement("thrennix-video-button");
    const root = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = CSS;
    const wrap = document.createElement("div");
    wrap.className = "wrap";
    const btn = document.createElement("button");
    btn.className = "dl";
    btn.textContent = ytMode ? "⬇ Download this video" : "⬇ Download video";
    btn.title = "Download with Thrennix";
    const close = document.createElement("button");
    close.className = "x";
    close.textContent = "×";
    close.title = "Hide for this video";
    close.setAttribute("aria-label", "Hide");
    const menu = document.createElement("div");
    menu.className = "menu";
    menu.hidden = true;
    wrap.append(btn, close, menu);
    root.append(style, wrap);

    const hs = host.style;
    hs.setProperty("all", "initial", "important");
    hs.setProperty("position", "fixed", "important");
    hs.setProperty("z-index", "2147483647", "important");
    hs.setProperty("display", "block", "important");
    hs.setProperty("pointer-events", "none", "important");
    hs.setProperty("transform", "translateX(-100%)", "important");

    for (const t of ["click", "mousedown", "mouseup", "pointerdown", "pointerup", "dblclick", "contextmenu", "keydown"]) {
      host.addEventListener(t, stopEvent);
    }
    btn.addEventListener("click", () => (ytMode ? ytToggleMenu() : toggleMenu()).catch(() => {}));
    close.addEventListener("click", () => {
      if (ytMode && ytShownId) ytDismissed.add(ytShownId);
      else if (activeVideo) dismissed.add(activeVideo);
      removeUi();
    });
    wrap.addEventListener("mouseenter", () => {
      clearTimeout(hideTimer);
      setVisible(true);
    });
    wrap.addEventListener("mouseleave", scheduleHide);

    return { host, wrap, btn, menu, menuOpen: false };
  }

  function mountPoint() {
    const fs = document.fullscreenElement || document.webkitFullscreenElement || null;
    if (fs) {
      // A <video> itself in fullscreen can't host children: nothing we can overlay.
      if (fs.tagName === "VIDEO") return null;
      return fs;
    }
    return document.documentElement;
  }

  function attach() {
    if (!ui) return;
    const parent = mountPoint();
    if (!parent) {
      ui.host.style.setProperty("display", "none", "important");
      return;
    }
    ui.host.style.setProperty("display", "block", "important");
    if (ui.host.parentNode !== parent) {
      try {
        parent.appendChild(ui.host);
      } catch (_) {
        /* ignore */
      }
    }
  }

  function showFor(v) {
    if (!ui) ui = createUi();
    if (activeVideo !== v) {
      closeMenu();
      if (resizeObs) resizeObs.disconnect();
      activeVideo = v;
      try {
        resizeObs = typeof ResizeObserver === "function" ? new ResizeObserver(position) : null;
        if (resizeObs) resizeObs.observe(v);
      } catch (_) {
        resizeObs = null;
      }
    }
    attach();
    position();
    setVisible(true);
    scheduleHide();
    if (!posTimer) posTimer = setInterval(position, 300);
  }

  function removeUi() {
    clearTimeout(hideTimer);
    if (posTimer) clearInterval(posTimer);
    posTimer = null;
    if (resizeObs) resizeObs.disconnect();
    resizeObs = null;
    if (ui && ui.host.parentNode) ui.host.remove();
    if (ui) {
      ui.menuOpen = false;
      ui.menu.hidden = true;
    }
    activeVideo = null;
    ytShownId = null;
  }

  function position() {
    if (!ui || !activeVideo) return;
    if (!activeVideo.isConnected) {
      removeUi();
      return;
    }
    const r = activeVideo.getBoundingClientRect();
    if (r.width < MIN_W || r.height < MIN_H || !inViewport(r)) {
      if (!ui.menuOpen) ui.host.style.setProperty("visibility", "hidden", "important");
      return;
    }
    ui.host.style.setProperty("visibility", "visible", "important");
    const vw = window.innerWidth || document.documentElement.clientWidth;
    const top = Math.max(r.top, 0) + 8;
    const right = Math.min(r.right, vw) - 8;
    ui.host.style.setProperty("top", Math.round(top) + "px", "important");
    ui.host.style.setProperty("left", Math.round(right) + "px", "important");
  }

  function setVisible(on) {
    if (ui) ui.wrap.classList.toggle("hidden", !on);
  }

  function scheduleHide() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      if (ui && !ui.menuOpen) setVisible(false);
    }, AUTO_HIDE_MS);
  }

  function onMouseMove(e) {
    const now = Date.now();
    if (now - lastMove < 100) return;
    lastMove = now;
    if (!ui || !activeVideo) return;
    const r = activeVideo.getBoundingClientRect();
    const pad = 40;
    if (e.clientX >= r.left - pad && e.clientX <= r.right + pad && e.clientY >= r.top - pad && e.clientY <= r.bottom + pad) {
      setVisible(true);
      scheduleHide();
    }
  }

  /* ----------------------------------------------------------------- menu */

  function closeMenu() {
    if (!ui) return;
    ui.menuOpen = false;
    ui.menu.hidden = true;
    scheduleHide();
  }

  function msgEl(text, cls) {
    const d = document.createElement("div");
    d.className = "msg" + (cls ? " " + cls : "");
    d.textContent = text;
    return d;
  }

  async function toggleMenu() {
    if (!ui) return;
    if (ui.menuOpen) return closeMenu();
    ui.menuOpen = true;
    clearTimeout(hideTimer);
    ui.menu.hidden = false;
    ui.menu.replaceChildren(msgEl("Looking for streams…"));
    await refresh();
    if (!ui || !ui.menuOpen) return;

    const choices = C.mediaChoices(state.items);
    if (!choices.length && activeVideo) {
      const src = activeVideo.currentSrc || activeVideo.src;
      if (C.isHttpUrl(src) && !C.isExcludedSite(src)) {
        const ext = C.extFromUrl(src);
        const kind = ext === "m3u8" ? "hls" : ext === "mpd" ? "dash" : "file";
        const item = { url: src, kind, ext };
        choices.push({ label: C.mediaLabel(item), url: src, kind, item });
      }
    }
    if (!choices.length) {
      ui.menu.replaceChildren(msgEl("No downloadable stream detected yet. Play the video for a moment and try again."));
      return;
    }
    const nodes = choices.map((c) => {
      const b = document.createElement("button");
      b.textContent = c.label;
      b.title = c.url;
      b.addEventListener("click", () => startDownload(c).catch(() => {}));
      return b;
    });
    ui.menu.replaceChildren(...nodes);
  }

  async function startDownload(choice) {
    if (!ui) return;
    ui.menu.replaceChildren(msgEl("Sending to Thrennix…"));
    const r = await C.sendToBackground({
      cmd: "downloadMedia",
      url: choice.url,
      kind: choice.kind,
      title: document.title || "",
    });
    if (!ui) return;
    if (r && r.ok) {
      ui.menu.replaceChildren(msgEl("✓ Sent to Thrennix", "ok"));
      setTimeout(closeMenu, 1500);
    } else {
      const err = (r && r.error) || "Could not reach the extension.";
      ui.menu.replaceChildren(msgEl("Thrennix: " + err, "err"));
    }
  }

  /* -------------------------------------------------------- YouTube menu */

  function spinnerEl(text) {
    const d = msgEl(text);
    const sp = document.createElement("span");
    sp.className = "spin";
    d.prepend(sp);
    return d;
  }

  function ytPageTitle() {
    return (document.title || "").replace(/^\(\d+\)\s*/, "").replace(/\s*-\s*YouTube\s*$/i, "");
  }

  function ytErrorNodes(err) {
    const text = String(err || "Unknown error");
    if (!/^yt-dlp not found/i.test(text)) return [msgEl("Thrennix: " + text, "err")];
    const d = msgEl("Thrennix needs yt-dlp: run ", "err");
    const code = document.createElement("code");
    code.textContent = YT_DLP_CMD;
    d.append(code);
    const copy = document.createElement("button");
    copy.className = "copy";
    copy.textContent = "Copy command";
    copy.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(YT_DLP_CMD);
        copy.textContent = "Copied ✓";
      } catch (_) {
        copy.textContent = "Copy failed — select the text above";
      }
    });
    return [d, copy];
  }

  async function ytToggleMenu() {
    if (!ui) return;
    if (ui.menuOpen) return closeMenu();
    const y = C.youtubeInfo(location.href);
    if (!y) return;
    ui.menuOpen = true;
    clearTimeout(hideTimer);
    ui.menu.hidden = false;
    if (y.kind === "live") {
      ui.menu.replaceChildren(msgEl("Live streams aren't supported."));
      return;
    }
    ui.menu.replaceChildren(spinnerEl("Getting qualities…"));
    const r = await C.sendToBackground({ cmd: "ytFormats", url: y.watchUrl });
    if (!ui || !ui.menuOpen || ytShownId !== y.id) return;
    if (!r) return ui.menu.replaceChildren(msgEl("Could not reach the extension.", "err"));
    if (!r.ok) return ui.menu.replaceChildren(...ytErrorNodes(r.error));
    const title = r.title || ytPageTitle() || "video";
    const rows = r.formats.map((f, i) => {
      const b = document.createElement("button");
      b.className = "yt";
      const t = document.createElement("span");
      t.className = "t";
      t.textContent = i + 1 + ". " + title;
      const q = document.createElement("span");
      q.className = "q";
      q.textContent = C.ytFormatLabel(f);
      b.title = title + " — " + q.textContent;
      b.append(t, q);
      b.addEventListener("click", () => ytStart(y, f, title).catch(() => {}));
      return b;
    });
    ui.menu.classList.add("wide");
    ui.menu.replaceChildren(...rows);
  }

  async function ytStart(y, format, title) {
    if (!ui) return;
    ui.menu.replaceChildren(spinnerEl("Sending to Thrennix…"));
    const r = await C.sendToBackground({ cmd: "ytDownload", url: y.watchUrl, format: format.id, title });
    if (!ui) return;
    if (r && r.ok) {
      ui.menu.replaceChildren(msgEl("✓ Sent to Thrennix", "ok"));
      setTimeout(closeMenu, 1500);
    } else {
      ui.menu.replaceChildren(...ytErrorNodes((r && r.error) || "Could not reach the extension."));
    }
  }

  /* ------------------------------------------------------------ lifecycle */

  function onDocClick(e) {
    if (!ui || !ui.menuOpen) return;
    try {
      const path = e.composedPath ? e.composedPath() : [];
      if (!path.includes(ui.host)) closeMenu();
    } catch (_) {
      /* ignore */
    }
  }

  const onPlay = () => evaluate();
  const onScroll = () => position();
  const onFullscreen = () => {
    attach();
    position();
  };

  function stop() {
    stopped = true;
    removeUi();
    clearInterval(scanTimer);
    document.removeEventListener("playing", onPlay, true);
    document.removeEventListener("play", onPlay, true);
    window.removeEventListener("scroll", onScroll, true);
    window.removeEventListener("resize", onScroll);
    document.removeEventListener("mousemove", onMouseMove, true);
    document.removeEventListener("click", onDocClick, true);
    document.removeEventListener("fullscreenchange", onFullscreen);
    document.removeEventListener("webkitfullscreenchange", onFullscreen);
    window.removeEventListener("yt-navigate-finish", onYtNavigate);
    document.removeEventListener("yt-navigate-finish", onYtNavigate);
  }

  try {
    document.addEventListener("encrypted", reportDrm, true);
    document.addEventListener("playing", onPlay, true);
    document.addEventListener("play", onPlay, true);
    window.addEventListener("scroll", onScroll, { capture: true, passive: true });
    window.addEventListener("resize", onScroll, { passive: true });
    document.addEventListener("mousemove", onMouseMove, { capture: true, passive: true });
    document.addEventListener("click", onDocClick, true);
    document.addEventListener("fullscreenchange", onFullscreen);
    document.addEventListener("webkitfullscreenchange", onFullscreen);
    if (ytMode) {
      // YouTube is a SPA: reset per video on its navigation event and on any URL change.
      window.addEventListener("yt-navigate-finish", onYtNavigate);
      document.addEventListener("yt-navigate-finish", onYtNavigate);
    }
    scanTimer = setInterval(() => {
      // Extension reloaded/removed: the old content script loses its runtime.
      try {
        if (!api.runtime || !api.runtime.id) return stop();
      } catch (_) {
        return stop();
      }
      if (ytMode && location.href !== lastHref) onYtNavigate();
      if (!videos.length && !ui) return;
      if (!state.ready) refresh().catch(() => {});
      else evaluate();
    }, SCAN_MS);

    api.runtime.onMessage.addListener((msg) => {
      // Only frames that actually contain a video need the list.
      if (msg && msg.cmd === "mediaUpdated" && (videos.length || ui)) refresh().catch(() => {});
      return false;
    });
    api.storage.onChanged.addListener((changes, area) => {
      if (area === "sync" && changes.videoButton) refresh().catch(() => {});
    });
  } catch (_) {
    /* ignore */
  }

  if (videos.length) refresh().catch(() => {});
})();
