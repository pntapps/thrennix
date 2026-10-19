//! thrennix-core: the multi-connection download engine.
//!
//! PUBLIC API CONTRACT — other crates (thrennix-cli, thrennix-app, thrennix-media) code against these
//! types and signatures. Keep them stable; add fields/methods rather than changing existing ones.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

mod download;
pub mod fsutil;
pub mod http;
pub mod names;
pub mod segment;
pub mod state;
mod writer;

pub use state::{DownloadState, SegmentState};

/// Tuning knobs for the engine. `Default` gives the recommended settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Connections opened right after the probe (default 8).
    pub initial_connections: usize,
    /// Hard cap on connections per download (default 32).
    pub max_connections: usize,
    /// Never split a piece whose remaining bytes are below this (default 2 MiB).
    pub min_split_size: u64,
    /// Socket read buffer / chunk size handed to the writer (default 512 KiB).
    pub read_buffer_size: usize,
    /// Writes are batched up to this size before hitting the disk (default 8 MiB).
    pub write_batch_size: usize,
    /// How often the adaptive controller re-evaluates connection count (default 2 s).
    pub adapt_interval: Duration,
    /// A connection below this fraction of the median speed is replaced (default 0.25).
    pub slow_conn_ratio: f64,
    /// Grace period before a connection can be judged slow (default 5 s).
    pub slow_conn_grace: Duration,
    /// Retries per piece on network errors, with exponential backoff (default 8).
    pub max_retries: u32,
    /// Connect + first-byte timeout (default 20 s).
    pub timeout: Duration,
    /// Default User-Agent when the request does not provide one.
    pub user_agent: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            initial_connections: 8,
            max_connections: 32,
            min_split_size: 2 * 1024 * 1024,
            read_buffer_size: 512 * 1024,
            write_batch_size: 8 * 1024 * 1024,
            adapt_interval: Duration::from_secs(2),
            slow_conn_ratio: 0.25,
            slow_conn_grace: Duration::from_secs(5),
            max_retries: 8,
            timeout: Duration::from_secs(20),
            user_agent: concat!("Thrennix/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

/// What to download and how to authenticate (cookies/referer come from the browser extension).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// Directory to save into.
    pub dest_dir: PathBuf,
    /// Override the file name; otherwise taken from Content-Disposition or the URL path.
    pub file_name: Option<String>,
    /// Raw `Cookie` header value.
    pub cookies: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    /// Any extra request headers.
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Probing,
    Downloading,
    Paused,
    Completed,
    Failed(String),
}

/// One byte range being fetched. `end` is exclusive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentInfo {
    pub start: u64,
    pub cursor: u64,
    pub end: u64,
    /// Bytes/second over the last ~1 s; 0 when idle.
    pub speed_bps: f64,
    pub active: bool,
}

/// Snapshot published by a running download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub status: Status,
    pub file_name: String,
    /// Final path of the file (valid once probing is done).
    pub path: PathBuf,
    pub downloaded: u64,
    /// `None` when the server does not report a size.
    pub total: Option<u64>,
    /// Smoothed total speed, bytes/second.
    pub speed_bps: f64,
    pub connections: usize,
    /// Whether the server supports byte ranges (pause/resume possible).
    pub resumable: bool,
    pub segments: Vec<SegmentInfo>,
}

impl Progress {
    /// Estimated seconds remaining, if known.
    pub fn eta_secs(&self) -> Option<f64> {
        let total = self.total?;
        if self.speed_bps <= 0.0 {
            return None;
        }
        Some(total.saturating_sub(self.downloaded) as f64 / self.speed_bps)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(String),
    #[error("server returned status {0}")]
    Status(u16),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("remote file changed since the download started")]
    RemoteChanged,
    #[error("download was paused")]
    Paused,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// `Error` is not `Clone` (it wraps `io::Error`); rebuild an equivalent value.
    fn duplicate(&self) -> Error {
        match self {
            Error::Http(s) => Error::Http(s.clone()),
            Error::Status(c) => Error::Status(*c),
            Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), e.to_string())),
            Error::RemoteChanged => Error::RemoteChanged,
            Error::Paused => Error::Paused,
            Error::Other(s) => Error::Other(s.clone()),
        }
    }
}

/// Shared engine: owns the HTTP client pool. Cheap to clone.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<download::Inner>,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        let client = http::build_client(&config);
        Self {
            inner: Arc::new(download::Inner { config, client }),
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.inner.config
    }

    /// Start (or resume, if a matching `<file>.thrennix` state file exists next to the target)
    /// a download. Must be called inside a tokio runtime.
    pub fn start(&self, request: DownloadRequest) -> DownloadHandle {
        self.start_with_options(request, StartOptions::default())
    }

    /// [`Engine::start`] with extra options.
    ///
    /// While the status is `Probing`, `Progress::file_name` and `Progress::path` are empty;
    /// they are filled in once the target file is resolved.
    pub fn start_with_options(
        &self,
        request: DownloadRequest,
        options: StartOptions,
    ) -> DownloadHandle {
        let initial = Progress {
            status: Status::Probing,
            file_name: String::new(),
            path: PathBuf::new(),
            downloaded: 0,
            total: None,
            speed_bps: 0.0,
            connections: 0,
            resumable: false,
            segments: Vec::new(),
        };
        self.spawn(download::Origin::Start(request, options), initial)
    }

    /// Resume from an explicit state file (`<file>.thrennix`). If `new_url` is given, continue
    /// using that URL (for expired links) after verifying size/ETag still match.
    pub fn resume_from_state(
        &self,
        state_file: PathBuf,
        new_url: Option<String>,
    ) -> DownloadHandle {
        let initial = Progress {
            status: Status::Probing,
            file_name: String::new(),
            path: PathBuf::new(),
            downloaded: 0,
            total: None,
            speed_bps: 0.0,
            connections: 0,
            resumable: false,
            segments: Vec::new(),
        };
        self.spawn(
            download::Origin::State {
                path: state_file,
                new_url,
            },
            initial,
        )
    }

    fn spawn(&self, origin: download::Origin, initial: Progress) -> DownloadHandle {
        let (ptx, prx) = watch::channel(initial);
        let ptx = Arc::new(ptx);
        let (dtx, drx) = watch::channel(None);
        let cancel = CancellationToken::new();
        let inner = self.inner.clone();
        let token = cancel.clone();
        tokio::spawn(async move {
            let res = Arc::new(download::run(inner, origin, ptx.clone(), token).await);
            if let Err(e) = res.as_ref() {
                tracing::debug!("download ended: {e}");
            }
            // Result first, then the terminal status: whoever sees Completed/Paused/Failed
            // gets the same answer from `wait()` / `is_finished()` right away.
            let _ = dtx.send(Some(res.clone()));
            download::publish_terminal(&ptx, &res);
        });
        DownloadHandle {
            progress: prx,
            cancel,
            done: drx,
        }
    }
}

/// Options for [`Engine::start_with_options`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartOptions {
    /// Accept a `text/html` response even when the URL does not look like a web page.
    /// By default such a response fails with "server returned a web page instead of the
    /// file" (typical for expired links and login walls).
    #[serde(default)]
    pub allow_html: bool,
}

/// Handle to one running download.
///
/// Dropping the handle does **not** stop the download: it keeps running in the background
/// until it completes, fails, or is paused (so an accidental drop never loses work), but it
/// can no longer be controlled. Call [`DownloadHandle::pause`] before dropping it if you want
/// it stopped. Use [`DownloadHandle::is_finished`] to see whether it has ended. Statuses
/// `Completed`, `Paused` and `Failed` are final: they are published only once `wait()` returns.
pub struct DownloadHandle {
    progress: watch::Receiver<Progress>,
    cancel: CancellationToken,
    done: watch::Receiver<Option<Arc<Result<PathBuf>>>>,
}

impl DownloadHandle {
    /// Latest progress snapshot (updated ~10x/second).
    pub fn progress(&self) -> Progress {
        self.progress.borrow().clone()
    }

    /// A watch channel that changes whenever progress updates.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// Stop all connections, flush data, and persist the state file. Idempotent.
    pub fn pause(&self) {
        self.cancel.cancel();
    }

    /// True once the download has ended (completed, paused or failed) and `wait()` would
    /// return immediately.
    pub fn is_finished(&self) -> bool {
        self.done.borrow().is_some()
    }

    /// Wait for the download to end. Ok(path) on completion, Err(Error::Paused) after pause.
    pub async fn wait(&self) -> Result<PathBuf> {
        let mut rx = self.done.clone();
        let res = match rx.wait_for(|r| r.is_some()).await {
            Ok(v) => v.clone(),
            Err(_) => None,
        };
        match res.as_deref() {
            Some(Ok(p)) => Ok(p.clone()),
            Some(Err(e)) => Err(e.duplicate()),
            None => Err(Error::Other("download task ended unexpectedly".into())),
        }
    }
}
