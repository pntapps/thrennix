"use strict";
(function () {
  const C = globalThis.ThrennixCommon;
  const api = C.api;
  const $ = (id) => document.getElementById(id);
  let tab = null;

  function setStatus(s) {
    const el = $("status");
    const text = $("status-text");
    el.classList.remove("ok", "err", "muted");
    if (!s) {
      el.classList.add("muted");
      text.textContent = "Checking…";
    } else if (s.ok) {
      el.classList.add("ok");
      if (s.running === false) {
        text.textContent = "Installed — app will start on first download";
        el.title = "Thrennix is installed" + (s.version ? " (v" + s.version + ")" : "") + " but not running yet.";
      } else {
        text.textContent = "Connected" + (s.version ? " · v" + s.version : "");
        el.title = "The Thrennix app is reachable";
      }
    } else {
      el.classList.add("err");
      text.textContent = "App not connected";
      el.title = (s.error || "") + "\nInstall Thrennix and run it once (thrennix.exe --register).";
    }
  }

  async function checkConnection(force) {
    setStatus(null);
    const s = await C.sendToBackground({ cmd: "ping", force: !!force });
    setStatus(s || { ok: false, error: "Background not responding" });
  }

  function message(text, cls) {
    const p = document.createElement("p");
    p.className = cls || "muted";
    p.textContent = text;
    return p;
  }

  function renderMedia(state) {
    const box = $("media");
    if (!state) return box.replaceChildren(message("Could not read this tab.", "err"));
    if (!C.isHttpUrl(state.url)) return box.replaceChildren(message("Not available on this page."));
    if (state.excluded) {
      return box.replaceChildren(
        message("Thrennix doesn't download from this service (DRM-protected / not permitted by its terms).")
      );
    }
    if (state.youtube) return renderYouTube(state.youtube, state.title);
    if (state.drm) return box.replaceChildren(message("This page uses DRM-protected media."));
    const choices = C.mediaChoices(state.items).reverse(); // newest first
    if (!choices.length) return box.replaceChildren(message("No media detected yet. Start playing a video."));

    const rows = choices.map((c) => {
      const row = document.createElement("div");
      row.className = "item";
      const info = document.createElement("div");
      info.className = "info";
      const label = document.createElement("div");
      label.className = "label";
      label.textContent = c.label;
      const url = document.createElement("div");
      url.className = "url";
      url.textContent = c.url;
      url.title = c.url;
      info.append(label, url);
      const btn = document.createElement("button");
      btn.textContent = "Download";
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        btn.textContent = "Sending…";
        const r = await C.sendToBackground({
          cmd: "downloadMedia",
          tabId: tab && tab.id,
          url: c.url,
          kind: c.kind,
          title: (tab && tab.title) || "",
        });
        if (r && r.ok) {
          btn.textContent = "Sent ✓";
        } else {
          btn.disabled = false;
          btn.textContent = "Retry";
          btn.title = (r && r.error) || "Failed";
          label.textContent = c.label + " — " + ((r && r.error) || "failed");
          label.classList.add("err");
        }
      });
      row.append(info, btn);
      return row;
    });
    box.replaceChildren(...rows);
  }

  const YT_DLP_CMD = "winget install yt-dlp.yt-dlp";

  function ytError(err) {
    const text = String(err || "Unknown error");
    if (!/^yt-dlp not found/i.test(text)) return [message("Thrennix: " + text, "err")];
    const p = message("Thrennix needs yt-dlp: run ", "err");
    const code = document.createElement("code");
    code.textContent = YT_DLP_CMD;
    p.append(code);
    const copy = document.createElement("button");
    copy.className = "secondary";
    copy.textContent = "Copy command";
    copy.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(YT_DLP_CMD);
        copy.textContent = "Copied ✓";
      } catch (_) {
        copy.textContent = "Copy failed";
      }
    });
    return [p, copy];
  }

  async function renderYouTube(yt, tabTitle) {
    const box = $("media");
    if (yt.kind === "live") return box.replaceChildren(message("Live streams aren't supported."));
    const wait = message("Getting qualities…");
    wait.prepend(Object.assign(document.createElement("span"), { className: "spin" }));
    box.replaceChildren(wait);
    const r = await C.sendToBackground({ cmd: "ytFormats", url: yt.watchUrl });
    if (!r) return box.replaceChildren(message("Background not responding.", "err"));
    if (!r.ok) return box.replaceChildren(...ytError(r.error));
    const title = r.title || String(tabTitle || "").replace(/^\(\d+\)\s*/, "").replace(/\s*-\s*YouTube\s*$/i, "");
    const rows = r.formats.map((f, i) => {
      const row = document.createElement("div");
      row.className = "item";
      const info = document.createElement("div");
      info.className = "info";
      const label = document.createElement("div");
      label.className = "label";
      label.textContent = i + 1 + ". " + C.ytFormatLabel(f);
      const sub = document.createElement("div");
      sub.className = "url";
      sub.textContent = title;
      sub.title = title;
      info.append(label, sub);
      const btn = document.createElement("button");
      btn.textContent = "Download";
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        btn.textContent = "Sending…";
        const res = await C.sendToBackground({ cmd: "ytDownload", url: yt.watchUrl, format: f.id, title });
        if (res && res.ok) {
          btn.textContent = "Sent ✓";
        } else {
          btn.disabled = false;
          btn.textContent = "Retry";
          const err = (res && res.error) || "failed";
          btn.title = err;
          if (/^yt-dlp not found/i.test(err)) box.replaceChildren(...ytError(err));
          else {
            label.textContent = i + 1 + ". " + C.ytFormatLabel(f) + " — " + err;
            label.classList.add("err");
          }
        }
      });
      row.append(info, btn);
      return row;
    });
    box.replaceChildren(...rows);
  }

  async function init() {
    try {
      const tabs = await api.tabs.query({ active: true, currentWindow: true });
      tab = (tabs && tabs[0]) || null;
    } catch (_) {
      tab = null;
    }

    const settings = await C.loadSettings();
    const toggle = $("catch");
    toggle.checked = settings.catchEnabled;
    toggle.addEventListener("change", () => {
      C.saveSettings({ catchEnabled: toggle.checked });
    });

    $("options").addEventListener("click", (e) => {
      e.preventDefault();
      try {
        Promise.resolve(api.runtime.openOptionsPage()).catch(() => {});
      } catch (_) {
        /* ignore */
      }
      window.close();
    });
    $("recheck").addEventListener("click", (e) => {
      e.preventDefault();
      checkConnection(true);
    });

    checkConnection(false);
    const state = tab ? await C.sendToBackground({ cmd: "popupState", tabId: tab.id }) : null;
    renderMedia(state && !state.error ? state : null);
  }

  init().catch(() => renderMedia(null));
})();
