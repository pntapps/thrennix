# Thrennix browser extension

Manifest V3 extension for Chrome, Edge and Firefox (128+). It sends downloads and detected
videos to the Thrennix desktop app over Native Messaging (host `com.thrennix.host`, see
`../docs/PROTOCOL.md`). Plain JavaScript with no build step and no dependencies.

## Before you install

Install the Thrennix app and run it once (or run `thrennix.exe --register`). That registers the
native messaging host for Chrome, Edge and Firefox. Until the host is registered, the popup
shows **App not connected** and every download stays in the browser.

## Install (development)

**Chrome / Edge**
1. Open `chrome://extensions` (or `edge://extensions`).
2. Turn on **Developer mode**.
3. Click **Load unpacked** and select this `extension/` folder.
4. The ID shown must be `dakhjjghlmmelmnnmemfeinodnhkmepd` (pinned by `"key"` in
   `manifest.json`, also in `extension-id.txt`). The native host manifest allows only this ID.

**Firefox (128+)**
1. Open `about:debugging#/runtime/this-firefox`.
2. Click **Load Temporary Add-on…** and pick `extension/manifest.json`.
3. The add-on ID is `thrennix@thrennix.app`. Firefox may ask you to grant access to all websites.
   Allow it under *about:addons → Thrennix → Permissions*, because download catching and video
   detection need it.

Firefox prints warnings for the Chromium-only `key`, `service_worker` and
`minimum_chrome_version` keys. They are harmless. Chrome ignores `background.scripts` and
`browser_specific_settings` in the same way.

## Features

- **Download catching.** Matching downloads are handed to Thrennix: the URL is http(s), the
  file type is in the list, the size is unknown or at least the minimum size, and the site
  is not disabled. The browser's copy is paused during the hand-off and cancelled **only after**
  Thrennix replies `ok`. If the app can't be reached or refuses the download, the browser's copy is
  resumed. Items the browser has flagged as dangerous (other than the generic "file" warning)
  always stay in the browser. See the "Extension" section of `docs/PROTOCOL.md`. Chromium uses
  `downloads.onDeterminingFilename`. Firefox uses `downloads.onCreated`.
- **Context menu.** "Download with Thrennix" on links, images, video and audio. If the app is
  unavailable, a normal file falls back to a browser download. YouTube watch, shorts and
  youtu.be links also get "Download video with Thrennix", which downloads the best quality
  straight away. If that fails, the toolbar badge shows "!" for 5 s.
- **Video detection.** Response headers are observed, never blocked. HLS, DASH and plain
  media files are detected and stream segments are ignored. Lists are kept per tab, and the
  toolbar badge shows the count. HLS master playlists are fetched to list the available
  resolutions.
- **In-page button.** A floating "Download video" button appears over large videos. It
  lives in a closed shadow DOM, follows the video, auto-hides after 5 s, and has a dismiss
  (×) button.
- **YouTube (via yt-dlp in the app).** On `youtube.com` and `m.youtube.com` watch, shorts and
  live pages, and inside `youtube.com/embed` iframes on other sites, a "⬇ Download this video"
  button with × appears at the top-right of the player. It is hidden while an ad is playing.
  The first click sends `ytFormats` for the canonical
  `https://www.youtube.com/watch?v=<id>` (timeout 60 s, cached per video for 10 min) and lists
  numbered rows like IDM: `1. <title> — MP4 file, quality 1080p HD (≈ 118 MB)`, with audio
  last. Choosing a row sends `download` with `kind: "ytdlp"`. `/live/` pages show "Live
  streams aren't supported". If the app reports `yt-dlp not found…`, the menu shows
  `winget install yt-dlp.yt-dlp` with a copy button. The button resets on YouTube's SPA
  navigation (`yt-navigate-finish` or a URL change). The generic detector ignores YouTube
  and googlevideo.com traffic, because those responses are segments.
- **Popup.** Turn catching on or off, see the connection status and app version, see the
  media detected on the current tab (the yt-dlp quality list on YouTube video pages), and
  open Options.
- **Options** (`storage.sync`). Catch on/off, minimum size, file types, sites where
  catching is disabled, and video button on/off.

**DRM services.** Media is never detected or offered on the DRM-protected services in
`EXCLUDED_SITES` (`common.js`), such as Netflix, Prime Video, Disney+, Spotify and YouTube
Music. It is also never
offered on any page that uses EME/DRM (detected through `video.mediaKeys` or the
`encrypted` event), or for HLS/DASH manifests that declare DRM (`SAMPLE-AES`, a
non-identity `KEYFORMAT`, or `ContentProtection`).

## Files

| File | Purpose |
|---|---|
| `manifest.json` | MV3 manifest (cross-browser) |
| `common.js` | Shared helpers, settings and the excluded-site list (`globalThis.ThrennixCommon`) |
| `background.js` | Service worker / event page: native messaging, catching, menu, detection |
| `content.js` | Video button (generic + YouTube mode) and DRM detection (all frames) |
| `popup.html/.css/.js` | Toolbar popup |
| `options.html/.css/.js` | Options page |
| `ui.css` | Shared light/dark styles |
| `icons/` | Generated PNG icons |
| `tools/make_icons.py` | Regenerates icons (Python standard library only) |
| `extension-id.txt` | Pinned Chromium extension ID |
| `.keys/key.pem` | Private key for the pinned ID (git-ignored, keep it safe) |

## Regenerating

- Icons: `python extension/tools/make_icons.py`
- ID from the key: SHA-256 of the DER public key → first 32 hex chars → map `0-f` to `a-p`.
