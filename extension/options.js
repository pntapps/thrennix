"use strict";
(function () {
  const C = globalThis.ThrennixCommon;
  const $ = (id) => document.getElementById(id);

  function parseList(text, normalize) {
    const out = [];
    const seen = new Set();
    for (const raw of String(text || "").split(/[\s,;]+/)) {
      const v = normalize(raw);
      if (v && !seen.has(v)) {
        seen.add(v);
        out.push(v);
      }
    }
    return out;
  }

  const normExt = (s) => {
    const v = s.trim().toLowerCase().replace(/^\*?\./, "");
    return /^[a-z0-9]{1,12}$/.test(v) ? v : "";
  };

  async function load() {
    const s = await C.loadSettings();
    $("catchEnabled").checked = s.catchEnabled;
    $("minSizeMB").value = String(s.minSizeMB);
    $("extensions").value = s.extensions.join(" ");
    $("disabledSites").value = s.disabledSites.join("\n");
    $("videoButton").checked = s.videoButton;
    $("excluded").textContent = C.EXCLUDED_SITES.join(", ");
  }

  async function save() {
    let min = parseFloat($("minSizeMB").value);
    if (!isFinite(min) || min < 0) min = C.DEFAULT_SETTINGS.minSizeMB;
    const settings = {
      catchEnabled: $("catchEnabled").checked,
      minSizeMB: min,
      extensions: parseList($("extensions").value, normExt),
      disabledSites: parseList($("disabledSites").value, C.normalizeDomain),
      videoButton: $("videoButton").checked,
    };
    const ok = await C.saveSettings(settings);
    const el = $("saved");
    el.className = ok ? "ok" : "err";
    el.textContent = ok ? "Saved" : "Could not save settings";
    if (ok) {
      $("minSizeMB").value = String(min);
      $("extensions").value = settings.extensions.join(" ");
      $("disabledSites").value = settings.disabledSites.join("\n");
    }
    setTimeout(() => {
      el.textContent = "";
    }, 2500);
  }

  $("save").addEventListener("click", () => {
    save().catch(() => {});
  });
  $("resetExt").addEventListener("click", () => {
    $("extensions").value = C.DEFAULT_EXTENSIONS.join(" ");
  });
  load().catch(() => {});
})();
