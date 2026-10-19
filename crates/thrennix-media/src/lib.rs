//! thrennix-media: HLS (.m3u8) and DASH (.mpd) stream downloading.
//!
//! PUBLIC API CONTRACT — thrennix-app codes against this. Segments are fetched in parallel
//! and muxed into one file with ffmpeg (found on PATH or next to thrennix.exe).
//! Progress reuses `thrennix_core::Progress` so the UI can show media and file downloads the same way.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
pub use thrennix_core::{Error, Progress, Result, Status};

mod dash;
#[cfg(test)]
mod fuzz_tests;
mod hls;
mod http;
mod model;
mod mux;
mod reserve;
mod state;
mod task;
mod util;
pub mod ytdlp;

pub use util::find_ffmpeg;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Hls,
    Dash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    /// Highest bandwidth variant.
    Best,
    /// Best variant whose height is <= this.
    MaxHeight(u32),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaRequest {
    /// Master/media playlist (HLS) or MPD manifest (DASH) URL.
    pub url: String,
    pub dest_dir: PathBuf,
    /// Output name; ".mp4" is appended if missing. Default derived from the URL.
    pub file_name: Option<String>,
    pub cookies: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// One selectable rendition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub height: Option<u32>,
    pub width: Option<u32>,
    pub bandwidth: u64,
    pub codecs: Option<String>,
    pub url: String,
}

/// Fetch and parse the manifest, returning available video variants (best first).
///
/// For DASH, `url` is the manifest URL with `#<Representation id>` appended.
pub async fn list_variants(kind: MediaKind, request: &MediaRequest) -> Result<Vec<Variant>> {
    let http = http::Http::new(request)?;
    let (text, base) = http.get_text(&request.url).await?;
    let mut out = match kind {
        MediaKind::Hls => {
            if hls::is_master(&text) {
                let m = hls::parse_master(&text, &base)?;
                m.variants
                    .into_iter()
                    .map(|v| Variant {
                        height: v.height,
                        width: v.width,
                        bandwidth: v.bandwidth,
                        codecs: v.codecs,
                        url: v.uri,
                    })
                    .collect()
            } else {
                let p = hls::parse_media(&text, &base)?;
                if !p.ended {
                    return Err(model::live_error());
                }
                vec![Variant {
                    height: None,
                    width: None,
                    bandwidth: 0,
                    codecs: None,
                    url: base.to_string(),
                }]
            }
        }
        MediaKind::Dash => {
            let mpd = dash::parse_mpd(&text, &base)?;
            let mut manifest = base.clone();
            manifest.set_fragment(None);
            mpd.variant_reps()
                .iter()
                .map(|r| Variant {
                    height: r.height,
                    width: r.width,
                    bandwidth: r.bandwidth,
                    codecs: r.codecs.clone(),
                    url: format!("{manifest}#{}", r.id),
                })
                .collect()
        }
    };
    let mut seen = std::collections::HashSet::new();
    out.retain(|v: &Variant| seen.insert(v.url.clone()));
    out.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth).then(b.height.cmp(&a.height)));
    if out.is_empty() {
        return Err(Error::Other("manifest lists no playable streams".into()));
    }
    Ok(out)
}

/// Start downloading. Must be called inside a tokio runtime.
/// Encrypted segments using AES-128 (standard HLS) are supported; DRM (SAMPLE-AES with
/// FairPlay/Widevine/PlayReady key systems) is rejected with `Error::Other("DRM-protected stream")`.
pub fn start(
    kind: MediaKind,
    request: MediaRequest,
    quality: Quality,
    concurrency: usize,
) -> MediaHandle {
    let shared = task::Shared::new(&request);
    tokio::spawn(task::run(
        shared.clone(),
        kind,
        request,
        quality,
        if concurrency == 0 { 8 } else { concurrency },
    ));
    MediaHandle { shared }
}

pub struct MediaHandle {
    shared: std::sync::Arc<task::Shared>,
}

impl MediaHandle {
    pub fn progress(&self) -> Progress {
        self.shared.progress.borrow().clone()
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Progress> {
        self.shared.progress.subscribe()
    }
    /// Stop; already-downloaded segments are kept so `start` with the same request resumes.
    pub fn pause(&self) {
        self.shared.cancel.send_replace(true);
    }
    pub async fn wait(&self) -> Result<PathBuf> {
        self.shared.wait().await
    }
    /// `(finished, total)` segment count, including init sections. Total is 0 while probing.
    pub fn segments_done(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.shared.segs_done.load(Relaxed),
            self.shared.segs_total.load(Relaxed),
        )
    }
    /// This download's private work dir (`<name>.thrennix-media`); `None` until resolved.
    /// Kept while paused/failed so a later `start` resumes; removed on success.
    pub fn work_dir(&self) -> Option<PathBuf> {
        self.shared.work_dir()
    }
    /// The reserved output path (`None` until resolved); the actual final path once completed.
    /// While unfinished, an empty `<output>.thrennix-reserve` placeholder holds the name.
    pub fn output_path(&self) -> Option<PathBuf> {
        self.shared.output_path()
    }
    /// True while segments are being concatenated/muxed (status stays `Downloading`,
    /// `connections` is 0).
    pub fn is_muxing(&self) -> bool {
        self.shared
            .muxing
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;

    #[test]
    fn handle_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<MediaHandle>();
    }
}
