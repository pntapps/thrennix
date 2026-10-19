//! Thin seam between the UI and the download engines, so the UI can run with a fake
//! backend (`--demo`).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use thrennix_core::{DownloadRequest, Engine, EngineConfig, Progress, SegmentInfo, Status};
use thrennix_media::{MediaKind, MediaRequest, Quality};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::protocol::{Kind, YtFormat};

pub type WaitFuture<'a> = Pin<Box<dyn Future<Output = thrennix_core::Result<PathBuf>> + Send + 'a>>;

pub trait Job: Send + Sync {
    fn progress(&self) -> Progress;
    fn pause(&self);
    /// Resolves once the job has really ended (completed, paused, or failed for good).
    fn wait(&self) -> WaitFuture<'_>;
    /// Media only: directory holding downloaded segments.
    fn work_dir(&self) -> Option<PathBuf> {
        None
    }
    /// Media only: final output file.
    fn output_path(&self) -> Option<PathBuf> {
        None
    }
    fn is_muxing(&self) -> bool {
        false
    }
}

pub struct YtInfoOut {
    pub title: String,
    pub formats: Vec<YtFormat>,
}

pub type ProbeFuture = Pin<Box<dyn Future<Output = Result<YtInfoOut, String>> + Send>>;

pub trait Backend: Send + Sync {
    /// `format` is the yt-dlp format id (kind Ytdlp only).
    fn start(&self, kind: Kind, req: DownloadRequest, format: Option<String>) -> Arc<dyn Job>;
    /// List yt-dlp qualities for a page URL.
    fn probe_yt(&self, url: String) -> ProbeFuture;
    fn resume_from_state(&self, state_file: PathBuf, new_url: Option<String>) -> Arc<dyn Job>;
    fn reconfigure(&self, config: EngineConfig);
}

impl Job for thrennix_core::DownloadHandle {
    fn progress(&self) -> Progress {
        thrennix_core::DownloadHandle::progress(self)
    }
    fn pause(&self) {
        thrennix_core::DownloadHandle::pause(self)
    }
    fn wait(&self) -> WaitFuture<'_> {
        Box::pin(thrennix_core::DownloadHandle::wait(self))
    }
}

impl Job for thrennix_media::MediaHandle {
    fn progress(&self) -> Progress {
        thrennix_media::MediaHandle::progress(self)
    }
    fn pause(&self) {
        thrennix_media::MediaHandle::pause(self)
    }
    fn wait(&self) -> WaitFuture<'_> {
        Box::pin(thrennix_media::MediaHandle::wait(self))
    }
    fn work_dir(&self) -> Option<PathBuf> {
        thrennix_media::MediaHandle::work_dir(self)
    }
    fn output_path(&self) -> Option<PathBuf> {
        thrennix_media::MediaHandle::output_path(self)
    }
    fn is_muxing(&self) -> bool {
        thrennix_media::MediaHandle::is_muxing(self)
    }
}

// ---------------------------------------------------------------- real

pub struct RealBackend {
    rt: Handle,
    engine: RwLock<Engine>,
}

impl RealBackend {
    pub fn new(rt: Handle, config: EngineConfig) -> Self {
        let engine = {
            let _g = rt.enter();
            Engine::new(config)
        };
        Self {
            rt,
            engine: RwLock::new(engine),
        }
    }

    fn engine(&self) -> Engine {
        self.engine
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn media_request(r: DownloadRequest) -> MediaRequest {
    MediaRequest {
        url: r.url,
        dest_dir: r.dest_dir,
        file_name: r.file_name,
        cookies: r.cookies,
        referer: r.referer,
        user_agent: r.user_agent,
        headers: r.headers,
    }
}

impl Backend for RealBackend {
    fn start(&self, kind: Kind, req: DownloadRequest, format: Option<String>) -> Arc<dyn Job> {
        let _g = self.rt.enter();
        match kind {
            Kind::File => Arc::new(self.engine().start(req)),
            Kind::Hls => Arc::new(thrennix_media::start(
                MediaKind::Hls,
                media_request(req),
                Quality::Best,
                8,
            )),
            Kind::Dash => Arc::new(thrennix_media::start(
                MediaKind::Dash,
                media_request(req),
                Quality::Best,
                8,
            )),
            Kind::Ytdlp => Arc::new(thrennix_media::ytdlp::start(
                req.url,
                req.dest_dir,
                format.unwrap_or_else(|| Kind::YTDLP_BEST.to_string()),
                req.file_name,
            )),
        }
    }

    fn probe_yt(&self, url: String) -> ProbeFuture {
        Box::pin(async move {
            let info = thrennix_media::ytdlp::probe(&url)
                .await
                .map_err(|e| e.to_string())?;
            Ok(YtInfoOut {
                title: info.title,
                formats: info
                    .formats
                    .into_iter()
                    .map(|f| YtFormat {
                        id: f.id,
                        label: f.label,
                        height: f.height,
                        filesize: f.filesize,
                    })
                    .collect(),
            })
        })
    }

    fn resume_from_state(&self, state_file: PathBuf, new_url: Option<String>) -> Arc<dyn Job> {
        let _g = self.rt.enter();
        Arc::new(self.engine().resume_from_state(state_file, new_url))
    }

    fn reconfigure(&self, config: EngineConfig) {
        let engine = {
            let _g = self.rt.enter();
            Engine::new(config)
        };
        *self.engine.write().unwrap_or_else(|e| e.into_inner()) = engine;
    }
}

// ---------------------------------------------------------------- demo

/// Simulated downloads for UI testing. A URL containing "fail" fails after a second,
/// "changed" fails with RemoteChanged; `ytFormats` for a URL with "noyt" fails.
pub struct DemoBackend {
    pub rt: Handle,
}

struct DemoJob {
    rx: watch::Receiver<Progress>,
    paused: Arc<AtomicBool>,
    muxing: Arc<AtomicBool>,
}

impl Job for DemoJob {
    fn is_muxing(&self) -> bool {
        self.muxing.load(Ordering::SeqCst)
    }
    fn progress(&self) -> Progress {
        self.rx.borrow().clone()
    }
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    fn wait(&self) -> WaitFuture<'_> {
        let mut rx = self.rx.clone();
        Box::pin(async move {
            let ended =
                |s: &Status| matches!(s, Status::Paused | Status::Completed | Status::Failed(_));
            let got = rx
                .wait_for(|p| ended(&p.status))
                .await
                .map(|p| p.clone())
                .ok();
            let p = got.unwrap_or_else(|| rx.borrow().clone());
            match p.status {
                Status::Completed => Ok(p.path),
                Status::Paused => Err(thrennix_core::Error::Paused),
                Status::Failed(e) if e.contains("changed") => {
                    Err(thrennix_core::Error::RemoteChanged)
                }
                Status::Failed(e) => Err(thrennix_core::Error::Other(e)),
                _ => Err(thrennix_core::Error::Other("demo job ended".into())),
            }
        })
    }
}

impl Backend for DemoBackend {
    fn start(&self, kind: Kind, req: DownloadRequest, _format: Option<String>) -> Arc<dyn Job> {
        let name = req.file_name.clone().unwrap_or_else(|| {
            let n = req
                .url
                .rsplit('/')
                .next()
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("");
            if n.is_empty() {
                "download.bin".into()
            } else {
                n.to_string()
            }
        });
        let name = if kind == Kind::Ytdlp {
            format!("{} [demo123].mp4", name.trim_start_matches("watch?v="))
        } else if kind.is_media() && !name.ends_with(".mp4") {
            format!(
                "{}.mp4",
                name.trim_end_matches(".m3u8").trim_end_matches(".mpd")
            )
        } else {
            name
        };
        // Demo knobs: "fail" in the URL = expired link, "changed" = remote file changed,
        // "merge" = stuck in yt-dlp merging; query `size=MB`, `start=%`, `speed=MB/s per conn`.
        let changed = req.url.contains("changed");
        let fail = req.url.contains("fail") || changed;
        let merge = req.url.contains("merge");
        let knob = |k: &str| -> Option<f64> {
            url::Url::parse(&req.url)
                .ok()?
                .query_pairs()
                .find(|(n, _)| n == k)?
                .1
                .parse()
                .ok()
        };
        let total: u64 = (knob("size").unwrap_or(300.0) * 1024.0 * 1024.0) as u64;
        let start_frac = if merge {
            1.0
        } else {
            knob("start").unwrap_or(0.0) / 100.0
        };
        let per_conn = knob("speed").unwrap_or(2.5) * 1024.0 * 1024.0;
        let muxing = Arc::new(AtomicBool::new(false));
        let mux2 = muxing.clone();
        let initial = Progress {
            status: Status::Probing,
            file_name: name.clone(),
            path: req.dest_dir.join(&name),
            downloaded: 0,
            total: None,
            speed_bps: 0.0,
            connections: 0,
            resumable: true,
            segments: vec![],
        };
        let (tx, rx) = watch::channel(initial.clone());
        let paused = Arc::new(AtomicBool::new(false));
        let p2 = paused.clone();
        self.rt.spawn(async move {
            let mut p = initial;
            p.downloaded = (total as f64 * start_frac) as u64;
            let mut ticks = 0u32;
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                ticks += 1;
                if p2.load(Ordering::SeqCst) {
                    p.status = Status::Paused;
                    p.speed_bps = 0.0;
                    p.connections = 0;
                    p.segments.iter_mut().for_each(|s| s.active = false);
                    let _ = tx.send(p);
                    return;
                }
                if fail && ticks >= 10 {
                    p.status = Status::Failed(if changed {
                        "remote file changed since the download started".into()
                    } else {
                        "demo: link expired (HTTP 403)".into()
                    });
                    p.connections = 0;
                    let _ = tx.send(p);
                    return;
                }
                p.status = Status::Downloading;
                p.total = Some(total);
                if merge {
                    mux2.store(true, Ordering::SeqCst);
                    p.downloaded = total;
                    p.speed_bps = 0.0;
                    p.connections = 0;
                    p.segments.clear();
                    let _ = tx.send(p.clone());
                    continue;
                }
                let conns = (8 + ticks / 10).min(16) as u64;
                p.connections = conns as usize;
                p.speed_bps = per_conn * conns as f64 * (0.9 + 0.2 * ((ticks % 7) as f64 / 7.0));
                p.downloaded = (p.downloaded + (p.speed_bps / 10.0) as u64).min(total);
                let frac = p.downloaded as f64 / total as f64;
                let piece = total / conns;
                p.segments = (0..conns)
                    .map(|i| {
                        let start = i * piece;
                        let end = if i + 1 == conns { total } else { start + piece };
                        let f = (frac * (0.85 + 0.3 * ((i % 3) as f64) / 2.0)).min(1.0);
                        SegmentInfo {
                            start,
                            cursor: start + ((end - start) as f64 * f) as u64,
                            end,
                            speed_bps: p.speed_bps / conns as f64,
                            active: f < 1.0,
                        }
                    })
                    .collect();
                if p.downloaded >= total {
                    p.status = Status::Completed;
                    p.speed_bps = 0.0;
                    p.connections = 0;
                    p.segments.clear();
                    let _ = tx.send(p);
                    return;
                }
                let _ = tx.send(p.clone());
            }
        });
        Arc::new(DemoJob { rx, paused, muxing })
    }

    fn resume_from_state(&self, state_file: PathBuf, new_url: Option<String>) -> Arc<dyn Job> {
        let path = state_file.with_extension("");
        self.start(
            Kind::File,
            DownloadRequest {
                url: new_url.unwrap_or_default(),
                dest_dir: path.parent().map(PathBuf::from).unwrap_or_default(),
                file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                ..Default::default()
            },
            None,
        )
    }

    fn probe_yt(&self, url: String) -> ProbeFuture {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            if url.contains("noyt") {
                return Err("yt-dlp not found (demo)".into());
            }
            let f = |id: &str, label: &str, height: Option<u32>, mb: u64| YtFormat {
                id: id.into(),
                label: label.into(),
                height,
                filesize: Some(mb * 1024 * 1024),
            };
            Ok(YtInfoOut {
                title: "Demo video".into(),
                formats: vec![
                    f(
                        "bv*[height<=1080]+ba/b[height<=1080]",
                        "MP4 · 1080p HD",
                        Some(1080),
                        120,
                    ),
                    f(
                        "bv*[height<=720]+ba/b[height<=720]",
                        "MP4 · 720p HD",
                        Some(720),
                        64,
                    ),
                    f("ba[ext=m4a]/ba", "Audio · M4A", None, 4),
                ],
            })
        })
    }

    fn reconfigure(&self, _config: EngineConfig) {}
}
