# Thrennix

A very fast, very simple download manager for Windows, written in Rust.

- **Multi-connection downloads**: starts with 8 connections and adds more (up to 32) while speed keeps rising
- **Dynamic segmentation**: when a connection frees up, it splits the piece that will finish *last*, so no connection sits idle and the end of the download doesn't drag
- **Slow-connection replacement**, fast TLS 1.3 reconnects, and no "assembling" step (each piece is written straight into place in the final file)
- **Pause / resume**, even after closing the app or rebooting, and "New URL…" for expired links
- **Browser extension** (Chrome, Edge, Firefox): catches downloads and detects videos (direct files, HLS and DASH)
- One small `.exe`, no runtime to install

- **YouTube** (and other sites yt-dlp supports): a "Download this video" button with an IDM-style quality list (needs `yt-dlp`, e.g. `winget install yt-dlp.yt-dlp`)

DRM-protected services aren't supported.

## Project layout

| Path | What |
|---|---|
| `crates/thrennix-core` | Download engine |
| `crates/thrennix-media` | HLS / DASH downloading (muxed with ffmpeg) |
| `crates/thrennix-app` | Desktop app (`thrennix.exe`, egui) and native messaging host |
| `crates/thrennix-cli` | Command-line downloader (`thrennix-cli.exe`) |
| `crates/thrennix-testserver` | Local HTTP server for tests and benchmarks |
| `extension/` | Browser extension (Manifest V3) |
| `bench/` | Benchmarks against aria2 and IDM |
| `docs/PROTOCOL.md` | Browser ⇄ app protocol |

## Install (no admin needed)

Download `thrennix-<version>-windows-x64.zip` from Releases, unzip it, and run:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

This installs to `%LOCALAPPDATA%\Programs\Thrennix`, registers the browser integration, and adds a Start Menu shortcut. Then load the extension (see below). To remove it, run `uninstall.ps1`.

## Build from source

Requirements: Rust (stable, MSVC toolchain) and the Visual Studio C++ Build Tools. ffmpeg is optional (needed for HLS/DASH video).

```powershell
cargo build --release
.\target\release\thrennix.exe --register   # registers the browser integration
.\target\release\thrennix.exe
```

Then load the extension: see [`extension/README.md`](extension/README.md).

## Command line

```powershell
.\target\release\thrennix-cli.exe https://example.com/big.iso -o D:\Downloads
```

Press Ctrl+C to pause. Run the same command again to resume.

## Benchmarks

See [`bench/README.md`](bench/README.md).

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
