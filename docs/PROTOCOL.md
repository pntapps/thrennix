# Browser ⇄ App protocol

## Components

```
Extension (MV3) ──Native Messaging (stdio, 4-byte LE length + JSON)──► thrennix.exe --native-host <origin>
                                                                         │
                                         named pipe \\.\pipe\thrennix-ipc-<user SID> (newline-delimited JSON)
                                                                         ▼
                                                                thrennix.exe (GUI, running)
```

- Chrome/Edge launch the host as `thrennix.exe chrome-extension://<id>/` (Firefox: `thrennix.exe <manifest path> <extension id>`).
  The app detects native-host mode when its first argument starts with `chrome-extension://`, or when
  it is `--native-host`, or when it ends with `.json` and a second argument is present (Firefox).
- Host mode forwards every message to the GUI's pipe. If the pipe does not exist, it starts
  `thrennix.exe` (GUI mode, detached) and retries for up to 5 s.
- Native messaging host name: `com.thrennix.host`.
- Extension ID (Chromium) lives in `extension/extension-id.txt` (pinned with the `key` in manifest.json).
  Firefox add-on ID: `thrennix@thrennix.app`.

## Messages: extension → app

All messages are JSON objects with a `type` field.

### `download`
```json
{
  "type": "download",
  "url": "https://example.com/file.zip",
  "filename": "file.zip",          // optional
  "referer": "https://example.com/", // optional
  "cookies": "a=1; b=2",           // optional, raw Cookie header
  "userAgent": "Mozilla/5.0 ...",  // optional
  "headers": {"Authorization": "..."}, // optional
  "kind": "file"                  // "file" | "hls" | "dash"
}
```

### `ping`
```json
{"type": "ping"}
```

### `ytFormats` (YouTube and other yt-dlp sites)
Ask the app which qualities are available for a page URL. The app runs `yt-dlp -J --no-playlist`.
```json
{"type": "ytFormats", "url": "https://www.youtube.com/watch?v=..."}
```
Reply:
```json
{
  "type": "ytFormats",
  "title": "Video title",
  "formats": [
    {"id": "bv*[height<=1080]+ba/b[height<=1080]", "label": "MP4 · 1080p HD", "height": 1080, "filesize": 123456789},
    {"id": "bv*[height<=720]+ba/b[height<=720]",   "label": "MP4 · 720p HD",  "height": 720,  "filesize": 65432100},
    {"id": "ba[ext=m4a]/ba",                       "label": "Audio · M4A",    "height": null, "filesize": 4567890}
  ]
}
```
`filesize` is approximate and may be `null`. On failure: `{"type":"error","message":"..."}`; when
yt-dlp is not installed the message starts with `yt-dlp not found`.

The app keeps each video's list for 30 minutes and shares a running probe between askers, so
asking again (or after a timeout) is cheap. `"prefetch": true` warms that cache when a video
opens: the host then does not start the app if it isn't running (reply: `app not running`).

### `download` with `kind: "ytdlp"`
```json
{"type": "download", "kind": "ytdlp", "url": "<page url>", "format": "<id from ytFormats>", "filename": "<title>"}
```

## Replies: app → extension (via host)

```json
{"type": "ok"}
{"type": "pong", "version": "0.1.0"}
{"type": "pong", "version": "0.1.0", "running": false}
{"type": "error", "message": "..."}
```

- `pong.running`: present and `false` only when the **host** answers the ping itself because the GUI
  is not running (the host is installed and will start the GUI on the next `download`). When the
  GUI answers, `running` is absent (equivalent to `true`). Both forms mean "healthy".

## Messages: host → GUI only (pipe)

### `show`
```json
{"type": "show"}
```
Single-instance support: asks an already-running GUI to bring its window to the front (e.g. when
`thrennix.exe` is launched a second time). Never sent by the extension.

## Pipe framing
Host → GUI: one JSON message per line (UTF-8, `\n` terminated). GUI answers one JSON line.

## Extension

### When a browser download is caught
A browser download is handed to Thrennix only if **all** of these hold:
- catching is enabled (popup switch / options), and the URL is `http(s)` (never `blob:`/`data:`);
- size is unknown or ≥ the minimum size (default 1 MB);
- the file extension (from the filename, URL, or MIME type) is in the catch list;
- neither the download URL nor its referrer/tab is on the user's per-site disabled list;
- for media types, the site is not one of the DRM-protected services (`EXCLUDED_SITES` in
  `extension/common.js`); YouTube media is handled only through `ytFormats` / `kind: "ytdlp"`;
- Chromium has not flagged the item as dangerous: `downloadItem.danger` must be absent, `safe`,
  `accepted`, or `file` (generic dangerous file type such as `.exe`; Thrennix writes Mark-of-the-Web so
  SmartScreen still applies). Any other verdict is left to the browser;
- `ping` succeeds (`pong`, with or without `running: false`). Healthy results are cached for 30 s.

### Hand-off order (pause → send → cancel or resume)
1. Chromium: `downloads.onDeterminingFilename` calls `suggest()` immediately (a download cannot
   be paused before that). Firefox: `downloads.onCreated`.
2. The extension pauses the browser download by id (`downloads.pause`). If the item has already
   finished or failed, it is left alone. If it can't be paused but is still in progress, the hand-off
   continues without the pause.
3. It re-reads the item (`downloads.search`) and re-checks the danger verdict, then pings.
4. It sends `download` (`kind: "file"`, cookies from `cookies.getAll`, referer, user agent).
   Timeout: 15 s, or 30 s for the first hand-off after a `pong` with `running: false` (or before any
   successful ping), so a cold start of the app has time to finish.
5. Only if the reply is `{"type":"ok"}`: `downloads.cancel` + `downloads.erase`. On any other outcome
   (error reply, timeout, host missing, danger flag) the paused download is resumed. No path cancels
   the browser download without an `ok` reply. A reply that arrives after the timeout can still
   produce two downloads.

Video/stream downloads (popup, in-page button, context menu) send `download` with `kind` `file`,
`hls` or `dash` and a filename derived from the page title. Message shapes are as above.

## Pipe security
The pipe name includes the current user's SID (`\.\pipe\thrennix-ipc-S-1-5-21-...`). It is created with a
DACL granting access only to that user (`D:P(A;;GA;;;<SID>)`) and rejects remote clients. The native
host refuses to talk to a pipe whose server process runs as a different user.
