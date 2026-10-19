//! The download task: probe, target selection, worker pool and the dynamic scheduler.

use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::fsutil::{rename_with_retry, write_mark_of_the_web};
use crate::http::{self, FailKind, Probe};
use crate::names::{choose_name, numbered};
use crate::segment::{initial_plan, rank_victims, split_point, Segment};
use crate::state::{part_path, state_path, DownloadState, SegmentState, STATE_VERSION};
use crate::writer::{self, WriteCmd, WriterError};
use crate::{
    DownloadRequest, EngineConfig, Error, Progress, Result, SegmentInfo, StartOptions, Status,
};

pub(crate) struct Inner {
    pub config: EngineConfig,
    pub client: Client,
}

pub(crate) enum Origin {
    Start(DownloadRequest, StartOptions),
    State {
        path: PathBuf,
        new_url: Option<String>,
    },
}

const TICK: Duration = Duration::from_millis(100);
const SAVE_EVERY: Duration = Duration::from_secs(1);
const SLOW_CHECK_EVERY: Duration = Duration::from_millis(500);
const REPLACE_COOLDOWN: Duration = Duration::from_secs(10);
const SPEED_TAU_SECS: f64 = 1.0;
/// End game: smallest half a tail piece may be split into.
const TAIL_MIN_SPLIT: u64 = 256 * 1024;
/// End game: only split a tail piece whose connection still needs this long.
const TAIL_MIN_ETA_SECS: f64 = 1.0;
/// New connections count as warmed up once each delivered this much.
const WARM_BYTES: u64 = 256 * 1024;
/// After a throttling response, wait this long after a slot frees before reconnecting
/// (the server may not have noticed the old connection closing yet).
const THROTTLED_RECONNECT_DELAY: Duration = Duration::from_millis(150);
/// Consecutive throttled attempts (without progress) before lowering the cap further.
const THROTTLE_STREAK_LOWER: u32 = 3;

fn empty_progress(status: Status, file_name: String, path: PathBuf) -> Progress {
    Progress {
        status,
        file_name,
        path,
        downloaded: 0,
        total: None,
        speed_bps: 0.0,
        connections: 0,
        resumable: false,
        segments: Vec::new(),
    }
}

fn jitter() -> f64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    0.75 + (h.finish() % 1000) as f64 / 2000.0
}

/// 250 ms × 2^(n-1), capped at 8 s, ±25 % jitter.
fn backoff(n: u32) -> Duration {
    let base = 0.25 * 2f64.powi(n.saturating_sub(1).min(10) as i32);
    Duration::from_secs_f64(base.min(8.0) * jitter())
}

/// Entry point of the spawned task.
pub(crate) async fn run(
    inner: Arc<Inner>,
    origin: Origin,
    ptx: Arc<watch::Sender<Progress>>,
    cancel: CancellationToken,
) -> Result<PathBuf> {
    let res = match origin {
        Origin::Start(req, opts) => start_flow(&inner, req, opts, &ptx, &cancel).await,
        Origin::State { path, new_url } => {
            state_flow(
                &inner,
                path,
                new_url,
                StartOptions::default(),
                &ptx,
                &cancel,
            )
            .await
        }
    };
    res.map_err(|e| match e {
        Error::Io(io) if io.kind() == std::io::ErrorKind::FileTooLarge => {
            Error::Other(io.to_string())
        }
        e => e,
    })
}

/// Publish the terminal status. Called only after the result is visible to `wait()`, so a
/// state the engine is about to retry is never shown as final.
pub(crate) fn publish_terminal(ptx: &watch::Sender<Progress>, res: &Result<PathBuf>) {
    ptx.send_modify(|p| {
        p.status = match res {
            Ok(_) => Status::Completed,
            Err(Error::Paused) => Status::Paused,
            Err(e) => Status::Failed(e.to_string()),
        };
        p.speed_bps = 0.0;
        p.connections = 0;
        for s in &mut p.segments {
            s.active = false;
            s.speed_bps = 0.0;
        }
    });
}

/// The server answered with a web page (login wall / expired link) instead of the file.
fn unexpected_html(p: &Probe, original_url: &str) -> bool {
    let is_html = p
        .content_type
        .as_deref()
        .map(|ct| ct.trim().to_ascii_lowercase())
        .is_some_and(|ct| ct.starts_with("text/html") || ct.starts_with("application/xhtml"));
    if !is_html {
        return false;
    }
    if p.content_disposition
        .as_deref()
        .is_some_and(|cd| cd.to_ascii_lowercase().contains("attachment"))
    {
        return false;
    }
    let page_like = |u: &str| match url::Url::parse(u) {
        Ok(u) => {
            let path = u.path().to_ascii_lowercase();
            path.is_empty() || path == "/" || path.ends_with(".htm") || path.ends_with(".html")
        }
        Err(_) => false,
    };
    !(page_like(&p.final_url) || page_like(original_url))
}

const HTML_INSTEAD_OF_FILE: &str =
    "server returned a web page instead of the file (link may need login or has expired)";

async fn probe_cancellable(
    inner: &Inner,
    url: &str,
    headers: HeaderMap,
    if_range: Option<HeaderValue>,
    cancel: &CancellationToken,
) -> Result<Probe> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Paused),
        r = http::probe(&inner.client, &inner.config, url, headers, if_range) => r,
    }
}

/// How the `.part` file should be opened.
enum Target {
    Fresh(File),
    Resume(File, Box<DownloadState>),
}

fn compatible(st: &DownloadState, p: &Probe) -> bool {
    if !st.resumable || !p.ranged || st.total != p.total || !st.is_consistent() {
        return false;
    }
    match (&st.etag, &p.etag) {
        (Some(a), Some(b)) => a == b,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => match (&st.last_modified, &p.last_modified) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        },
    }
}

/// Find the final path: resume a matching `.thrennix`, otherwise reserve `name`, `name (1)`, …
fn pick_target(
    dest: &Path,
    name: &str,
    req: &DownloadRequest,
    p: &Probe,
    allow_resume: bool,
) -> Result<(PathBuf, Target)> {
    for n in 0..10_000u32 {
        let cand = dest.join(numbered(name, n));
        let sp = state_path(&cand);
        let pp = part_path(&cand);
        if sp.exists() {
            match DownloadState::load(&sp) {
                Ok(st) if st.url == req.url || st.final_url == p.final_url => {
                    if allow_resume && compatible(&st, p) && pp.exists() {
                        tracing::debug!(path = %cand.display(), "resuming from state file");
                        let f = writer::open_part(&pp, false, p.total)?;
                        return Ok((cand, Target::Resume(f, Box::new(st))));
                    }
                    tracing::debug!(path = %cand.display(), "state file is stale; restarting");
                    let _ = std::fs::remove_file(&pp);
                    let _ = std::fs::remove_file(&sp);
                    let f = open_fresh(&pp, p.total, true)?;
                    return Ok((cand, Target::Fresh(f)));
                }
                _ => continue,
            }
        }
        if cand.exists() || pp.exists() {
            continue;
        }
        match open_fresh(&pp, p.total, false) {
            Ok(f) => return Ok((cand, Target::Fresh(f))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Other(format!(
        "could not find a free file name for {name}"
    )))
}

fn open_fresh(pp: &Path, total: Option<u64>, truncate: bool) -> std::io::Result<File> {
    if truncate {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(pp)?;
        drop(f);
        writer::open_part(pp, false, total)
    } else {
        writer::open_part(pp, true, total)
    }
}

async fn start_flow(
    inner: &Arc<Inner>,
    req: DownloadRequest,
    opts: StartOptions,
    ptx: &Arc<watch::Sender<Progress>>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let mut allow_resume = true;
    loop {
        // Name and path stay empty until the target is resolved.
        ptx.send_replace(empty_progress(
            Status::Probing,
            String::new(),
            PathBuf::new(),
        ));
        let headers = http::base_headers(&inner.config, &req, true);
        let probe = probe_cancellable(inner, &req.url, headers, None, cancel).await?;
        if !opts.allow_html && unexpected_html(&probe, &req.url) {
            return Err(Error::Other(HTML_INSTEAD_OF_FILE.into()));
        }
        let name = choose_name(
            req.file_name.as_deref(),
            probe.content_disposition.as_deref(),
            &probe.final_url,
            &req.url,
        );
        let dest = req.dest_dir.clone();
        let req2 = req.clone();
        let (probe, picked) = {
            let mut probe = probe;
            let resp = probe.response.take();
            let r = tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&dest)?;
                let r = pick_target(&dest, &name, &req2, &probe, allow_resume);
                r.map(|t| (probe, t))
            })
            .await
            .map_err(|e| Error::Other(format!("file setup task failed: {e}")))?;
            let (mut probe, picked) = r?;
            probe.response = resp;
            (probe, picked)
        };
        let (final_path, target) = picked;
        let resumed = matches!(target, Target::Resume(..));
        let res = run_job(inner, &req, probe, final_path, target, ptx, cancel).await;
        match res {
            Err(Error::RemoteChanged) if resumed => {
                tracing::info!("remote file changed since the partial download; restarting");
                allow_resume = false;
                continue;
            }
            r => return r,
        }
    }
}

async fn state_flow(
    inner: &Arc<Inner>,
    state_file: PathBuf,
    new_url: Option<String>,
    opts: StartOptions,
    ptx: &Arc<watch::Sender<Progress>>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let st = DownloadState::load(&state_file)?;
    let final_path = {
        let s = state_file.to_string_lossy();
        let Some(stripped) = s.strip_suffix(".thrennix") else {
            return Err(Error::Other(format!(
                "not a .thrennix state file: {}",
                state_file.display()
            )));
        };
        PathBuf::from(stripped)
    };
    let dest = final_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    ptx.send_replace(empty_progress(
        Status::Probing,
        String::new(),
        PathBuf::new(),
    ));
    let req = DownloadRequest {
        url: new_url.clone().unwrap_or_else(|| st.url.clone()),
        dest_dir: dest,
        file_name: Some(st.file_name.clone()),
        cookies: st.cookies.clone(),
        referer: st.referer.clone(),
        user_agent: st.user_agent.clone(),
        headers: st.headers.clone(),
    };
    let headers = http::base_headers(&inner.config, &req, true);
    let if_range = if st.resumable {
        http::if_range_value(st.etag.as_deref(), st.last_modified.as_deref())
    } else {
        None
    };
    let mut probe = probe_cancellable(inner, &req.url, headers, if_range, cancel).await?;
    if !opts.allow_html && unexpected_html(&probe, &req.url) {
        return Err(Error::Other(HTML_INSTEAD_OF_FILE.into()));
    }
    let pp = part_path(&final_path);
    let resp = probe.response.take();
    let fp = final_path.clone();
    let (probe, target) = tokio::task::spawn_blocking(move || -> Result<(Probe, Target)> {
        if st.resumable {
            if !compatible(&st, &probe) {
                return Err(Error::RemoteChanged);
            }
            if pp.exists() {
                let f = writer::open_part(&pp, false, probe.total)?;
                return Ok((probe, Target::Resume(f, Box::new(st))));
            }
            tracing::warn!(path = %fp.display(), "partial file missing; restarting from zero");
        }
        let f = open_fresh(&pp, probe.total, true)?;
        Ok((probe, Target::Fresh(f)))
    })
    .await
    .map_err(|e| Error::Other(format!("file setup task failed: {e}")))??;
    let mut probe = probe;
    probe.response = resp;
    run_job(inner, &req, probe, final_path, target, ptx, cancel).await
}

/// Per-download context shared by workers.
struct WorkerCtx {
    client: Client,
    url: String,
    headers: HeaderMap,
    if_range: Option<HeaderValue>,
    ranged: bool,
    total: Option<u64>,
    timeout: Duration,
    read_buf: usize,
    tx: mpsc::Sender<WriteCmd>,
}

enum WorkerExit {
    Finished,
    Cancelled,
    Failed(FailKind),
}

async fn send_data(ctx: &WorkerCtx, seg: &Arc<Segment>, offset: u64, data: Bytes) -> bool {
    if data.is_empty() {
        return true;
    }
    ctx.tx
        .send(WriteCmd::Data {
            seg: seg.clone(),
            offset,
            data,
        })
        .await
        .is_ok()
}

async fn connect(
    ctx: &WorkerCtx,
    seg: &Segment,
    cancel: &CancellationToken,
) -> std::result::Result<Option<Response>, WorkerExit> {
    let start = seg.cursor();
    let end = seg.end();
    if start >= end {
        return Ok(None);
    }
    let mut rb = ctx.client.get(&ctx.url).headers(ctx.headers.clone());
    if ctx.ranged {
        rb = rb.header(header::RANGE, format!("bytes={}-{}", start, end - 1));
        if let Some(v) = &ctx.if_range {
            rb = rb.header(header::IF_RANGE, v.clone());
        }
    }
    let sent = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(WorkerExit::Cancelled),
        r = tokio::time::timeout(ctx.timeout, rb.send()) => r,
    };
    let resp = match sent {
        Err(_) => {
            return Err(WorkerExit::Failed(FailKind::Retry(format!(
                "no response within {:?}",
                ctx.timeout
            ))))
        }
        Ok(Err(e)) => return Err(WorkerExit::Failed(http::classify_send_error(&e))),
        Ok(Ok(r)) => r,
    };
    let status = resp.status();
    if !ctx.ranged {
        if status.is_success() {
            return Ok(Some(resp));
        }
        return Err(WorkerExit::Failed(http::classify_status(status)));
    }
    match status.as_u16() {
        206 => {
            let cr = resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(http::parse_content_range);
            match cr {
                Some(http::ContentRange::Range { first, total, .. }) if first == start => {
                    if let (Some(t), Some(want)) = (total, ctx.total) {
                        if t != want {
                            return Err(WorkerExit::Failed(FailKind::Fatal(Error::RemoteChanged)));
                        }
                    }
                    Ok(Some(resp))
                }
                other => Err(WorkerExit::Failed(FailKind::Retry(format!(
                    "unexpected Content-Range {other:?} for request starting at {start}"
                )))),
            }
        }
        200 => {
            if ctx.if_range.is_some() {
                Err(WorkerExit::Failed(FailKind::Fatal(Error::RemoteChanged)))
            } else {
                Err(WorkerExit::Failed(FailKind::Retry(
                    "server ignored the Range header".into(),
                )))
            }
        }
        416 => Err(WorkerExit::Failed(FailKind::Fatal(Error::RemoteChanged))),
        _ => Err(WorkerExit::Failed(http::classify_status(status))),
    }
}

async fn worker(
    ctx: Arc<WorkerCtx>,
    seg: Arc<Segment>,
    resp: Option<Response>,
    cancel: CancellationToken,
) -> WorkerExit {
    // Where the server's response ends; used to tell a natural end from a shrunk one.
    let resp_end = if resp.is_some() {
        ctx.total.unwrap_or(u64::MAX)
    } else {
        seg.end()
    };
    let mut resp = match resp {
        Some(r) => r,
        None => match connect(&ctx, &seg, &cancel).await {
            Ok(Some(r)) => r,
            Ok(None) => return WorkerExit::Finished,
            Err(exit) => return exit,
        },
    };
    seg.connected.store(true, Ordering::Release);
    let mut buf = BytesMut::new();
    let mut buf_off = 0u64;
    let exit = loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            r = tokio::time::timeout(ctx.timeout, resp.chunk()) => Some(r),
        };
        let chunk = match next {
            None => break WorkerExit::Cancelled,
            Some(Err(_)) => break WorkerExit::Failed(FailKind::Retry("read timed out".into())),
            Some(Ok(Err(e))) => {
                break WorkerExit::Failed(FailKind::Retry(format!("read error: {e}")))
            }
            Some(Ok(Ok(None))) => {
                if seg.is_done() || (!ctx.ranged && ctx.total.is_none()) {
                    break WorkerExit::Finished;
                }
                break WorkerExit::Failed(FailKind::Retry(format!(
                    "connection closed at {} before range end {}",
                    seg.cursor(),
                    seg.end()
                )));
            }
            Some(Ok(Ok(Some(c)))) => c,
        };
        let (off, take) = seg.claim(chunk.len() as u64);
        if take > 0 {
            let data = chunk.slice(..take as usize);
            if buf.is_empty() && data.len() >= ctx.read_buf {
                if !send_data(&ctx, &seg, off, data).await {
                    break WorkerExit::Failed(FailKind::Fatal(Error::Other(
                        "disk writer stopped".into(),
                    )));
                }
            } else {
                if buf.is_empty() {
                    buf_off = off;
                }
                buf.extend_from_slice(&data);
                if buf.len() >= ctx.read_buf {
                    let d = buf.split().freeze();
                    if !send_data(&ctx, &seg, buf_off, d).await {
                        break WorkerExit::Failed(FailKind::Fatal(Error::Other(
                            "disk writer stopped".into(),
                        )));
                    }
                }
            }
        }
        if (take as usize) < chunk.len() || seg.is_done() {
            if take as usize == chunk.len() && seg.cursor() == resp_end {
                // Natural end of the response: read the EOF so the connection goes back to
                // the pool instead of being closed (matters for servers that limit connections).
                let _ = tokio::time::timeout(Duration::from_secs(1), resp.chunk()).await;
            }
            // Otherwise the end was shrunk by a split: drop the response.
            break WorkerExit::Finished;
        }
    };
    if !buf.is_empty() && !send_data(&ctx, &seg, buf_off, buf.freeze()).await {
        return WorkerExit::Failed(FailKind::Fatal(Error::Other("disk writer stopped".into())));
    }
    exit
}

struct Running {
    cancel: CancellationToken,
    started: Instant,
    start_cursor: u64,
    replacing: bool,
}

struct Slot {
    seg: Arc<Segment>,
    worker: Option<Running>,
    pending: bool,
    retry_at: Instant,
    failures: u32,
    throttles: u32,
    last_replaced: Option<Instant>,
    speed: f64,
    last_cursor: u64,
}

impl Slot {
    fn new(seg: Segment, pending: bool) -> Self {
        let c = seg.cursor();
        Self {
            seg: Arc::new(seg),
            worker: None,
            pending,
            retry_at: Instant::now(),
            failures: 0,
            throttles: 0,
            last_replaced: None,
            speed: 0.0,
            last_cursor: c,
        }
    }
}

enum Stop {
    Paused,
    Failed(Error),
}

struct Controller<'a> {
    cfg: &'a EngineConfig,
    ctx: Arc<WorkerCtx>,
    slots: Vec<Slot>,
    joins: JoinSet<(usize, WorkerExit)>,
    stop_token: CancellationToken,
    ranged: bool,
    target: usize,
    cap: usize,
    growing: bool,
    baseline: Option<f64>,
    speed: f64,
    /// Connections added per growth step; doubles while each step pays off.
    batch: usize,
    adapt_phase: AdaptPhase,
    /// Slots with index >= this were created after the last growth step.
    grow_mark: usize,
    /// The server pushed back (429/503/refused): the cap is now a hard limit.
    throttled: bool,
    throttle_streak: u32,
    /// No new connection before this instant (throttled servers need a moment to notice
    /// closed connections).
    next_start_at: Instant,
}

/// Adaptive growth: after adding connections, wait for them to warm up, then measure the
/// total speed over a short window and compare with the previous step.
enum AdaptPhase {
    Warmup { since: Instant },
    Measure { since: Instant, bytes: u64 },
}

impl Controller<'_> {
    fn active(&self) -> usize {
        self.slots.iter().filter(|s| s.worker.is_some()).count()
    }

    fn connected_active(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.worker.is_some() && s.seg.connected.load(Ordering::Acquire))
            .count()
    }

    fn start(&mut self, i: usize, resp: Option<Response>) {
        let token = self.stop_token.child_token();
        let slot = &mut self.slots[i];
        slot.seg.connected.store(resp.is_some(), Ordering::Release);
        slot.pending = false;
        slot.speed = 0.0;
        slot.last_cursor = slot.seg.cursor();
        slot.worker = Some(Running {
            cancel: token.clone(),
            started: Instant::now(),
            start_cursor: slot.seg.cursor(),
            replacing: false,
        });
        let ctx = self.ctx.clone();
        let seg = slot.seg.clone();
        tracing::debug!(
            seg = i,
            cursor = seg.cursor(),
            end = seg.end(),
            "starting worker"
        );
        self.joins
            .spawn(async move { (i, worker(ctx, seg, resp, token).await) });
    }

    /// Split the segment that would finish last and start a worker on its second half.
    fn try_split(&mut self) -> bool {
        let min = self.cfg.min_split_size;
        let idx: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].worker.is_some())
            .collect();
        let cands: Vec<(u64, f64)> = idx
            .iter()
            .map(|&i| (self.slots[i].seg.remaining(), self.slots[i].speed))
            .collect();
        for r in rank_victims(&cands, min) {
            let seg = self.slots[idx[r]].seg.clone();
            let Some(mid) = split_point(seg.cursor(), seg.end(), min) else {
                continue;
            };
            if let Some(old_end) = seg.try_shrink(mid) {
                tracing::debug!(victim = idx[r], mid, old_end, "split");
                self.slots
                    .push(Slot::new(Segment::new(mid, mid, old_end), false));
                let n = self.slots.len() - 1;
                self.start(n, None);
                return true;
            }
            // The victim's cursor passed the split point meanwhile: try the next one.
        }
        // End game: nothing is >= 2*min_split any more, but a slow tail would still decide
        // the finish time. Split pieces down to TAIL_MIN_SPLIT if the victim needs > 1 s.
        let tail: Vec<(u64, f64)> = idx
            .iter()
            .map(|&i| {
                let s = &self.slots[i];
                let measured = s.seg.connected.load(Ordering::Acquire) && s.speed > 0.0;
                let eta = if measured {
                    s.seg.remaining() as f64 / s.speed
                } else {
                    0.0
                };
                if eta > TAIL_MIN_ETA_SECS {
                    (s.seg.remaining(), s.speed)
                } else {
                    (0, 0.0)
                }
            })
            .collect();
        for r in rank_victims(&tail, TAIL_MIN_SPLIT) {
            let seg = self.slots[idx[r]].seg.clone();
            let Some(mid) = split_point(seg.cursor(), seg.end(), TAIL_MIN_SPLIT) else {
                continue;
            };
            if let Some(old_end) = seg.try_shrink(mid) {
                tracing::debug!(victim = idx[r], mid, old_end, "end-game split");
                self.slots
                    .push(Slot::new(Segment::new(mid, mid, old_end), false));
                let n = self.slots.len() - 1;
                self.start(n, None);
                return true;
            }
        }
        false
    }

    fn fill(&mut self) {
        let limit = self.target.min(self.cap).max(1);
        let now = Instant::now();
        if self.throttled && now < self.next_start_at {
            return;
        }
        while self.active() < limit {
            if let Some(i) = self
                .slots
                .iter()
                .position(|s| s.pending && s.retry_at <= now)
            {
                self.start(i, None);
                continue;
            }
            if !self.ranged || self.slots.iter().any(|s| s.pending) {
                break;
            }
            if !self.try_split() {
                break;
            }
        }
    }

    fn all_done(&self) -> bool {
        self.slots
            .iter()
            .all(|s| s.worker.is_none() && !s.pending && s.seg.is_done())
    }

    /// Handle a finished worker. Returns `Some(e)` when the whole download must fail.
    async fn on_exit(&mut self, i: usize, exit: WorkerExit) -> Option<Error> {
        let now = Instant::now();
        let run = self.slots[i].worker.take();
        let stopping = self.stop_token.is_cancelled();
        if self.throttled {
            self.next_start_at = now + THROTTLED_RECONNECT_DELAY;
        }
        if run
            .as_ref()
            .is_some_and(|r| self.slots[i].seg.cursor() > r.start_cursor)
        {
            self.throttle_streak = 0;
        }
        match exit {
            WorkerExit::Finished => {
                let seg = &self.slots[i].seg;
                if !self.ranged && self.ctx.total.is_none() {
                    seg.set_end(seg.cursor());
                }
                if !seg.is_done() && !stopping {
                    self.slots[i].pending = true;
                }
                None
            }
            WorkerExit::Cancelled => {
                if !stopping {
                    let slot = &mut self.slots[i];
                    slot.pending = true;
                    slot.retry_at = now;
                    if run.as_ref().is_some_and(|r| r.replacing) {
                        slot.last_replaced = Some(now);
                    }
                }
                None
            }
            WorkerExit::Failed(kind) => {
                if stopping {
                    if let FailKind::Fatal(e) = kind {
                        return Some(e);
                    }
                    return None;
                }
                let others = self.connected_active();
                let max_retries = self.cfg.max_retries;
                let slot = &mut self.slots[i];
                let progressed = run
                    .as_ref()
                    .is_some_and(|r| slot.seg.cursor() > r.start_cursor);
                if progressed {
                    slot.failures = 0;
                    slot.throttles = 0;
                }
                let delay;
                match kind {
                    FailKind::Fatal(e) => return Some(e),
                    FailKind::Throttled(msg) if others > 0 && !progressed => {
                        slot.throttles += 1;
                        if slot.throttles > max_retries.max(1) * 3 {
                            return Some(Error::Http(format!(
                                "server keeps refusing connections: {msg}"
                            )));
                        }
                        delay = backoff(slot.throttles);
                        // Cap at the connections that are working. The segment is retried
                        // only once a working connection frees a slot (never above the cap).
                        if !self.throttled {
                            self.throttled = true;
                            self.cap = self.cap.min(others).max(1);
                        } else {
                            self.throttle_streak += 1;
                            if self.throttle_streak >= THROTTLE_STREAK_LOWER {
                                self.throttle_streak = 0;
                                self.cap = self.cap.saturating_sub(1).min(others).max(1);
                            }
                        }
                        self.growing = false;
                        self.next_start_at = now + THROTTLED_RECONNECT_DELAY.max(delay / 2);
                        tracing::debug!(
                            seg = i,
                            others,
                            cap = self.cap,
                            "connection throttled ({msg})"
                        );
                    }
                    FailKind::Throttled(msg) | FailKind::Retry(msg) => {
                        slot.failures += 1;
                        tracing::debug!(seg = i, failures = slot.failures, "segment failed: {msg}");
                        if slot.failures > max_retries {
                            return Some(Error::Http(format!(
                                "bytes {}-{}: giving up after {} retries: {msg}",
                                slot.seg.cursor(),
                                slot.seg.end(),
                                max_retries
                            )));
                        }
                        delay = backoff(slot.failures);
                    }
                }
                let slot = &mut self.slots[i];
                slot.pending = true;
                slot.retry_at = now + delay;
                if !self.ranged {
                    // A single stream cannot continue mid-file: restart from zero.
                    if let Err(e) = flush_writer(&self.ctx.tx).await {
                        return Some(e.into());
                    }
                    self.slots[i].seg.reset_to(0);
                    self.slots[i].last_cursor = 0;
                }
                None
            }
        }
    }

    fn sample(&mut self, dt: f64) -> u64 {
        let alpha = 1.0 - (-dt / SPEED_TAU_SECS).exp();
        let mut total_delta = 0u64;
        for s in &mut self.slots {
            let c = s.seg.cursor();
            let d = c.saturating_sub(s.last_cursor);
            s.last_cursor = c;
            total_delta += d;
            let inst = d as f64 / dt;
            s.speed += alpha * (inst - s.speed);
            if s.worker.is_none() {
                s.speed = 0.0;
            }
        }
        let inst = total_delta as f64 / dt;
        self.speed += alpha * (inst - self.speed);
        if let AdaptPhase::Measure { bytes, .. } = &mut self.adapt_phase {
            *bytes += total_delta;
        }
        total_delta
    }

    /// Every connection started since the last growth step has delivered `WARM_BYTES`.
    fn new_connections_warm(&self) -> bool {
        let fresh: Vec<&Slot> = self.slots[self.grow_mark.min(self.slots.len())..]
            .iter()
            .filter(|s| s.worker.is_some())
            .collect();
        !fresh.is_empty()
            && fresh.iter().all(|s| {
                s.worker
                    .as_ref()
                    .is_some_and(|r| s.seg.cursor().saturating_sub(r.start_cursor) >= WARM_BYTES)
            })
    }

    /// Adaptive connection count, called every tick. Grows by `batch` (2, 4, 8, ...) while
    /// each step raises the measured total speed by more than 5 %; stops at the plateau.
    fn adapt(&mut self, now: Instant) {
        if !self.ranged || !self.growing {
            return;
        }
        let limit = self.cap.min(self.cfg.max_connections).max(1);
        if self.target >= limit {
            return;
        }
        let window = self
            .cfg
            .adapt_interval
            .min(Duration::from_secs(1))
            .max(Duration::from_millis(200));
        match self.adapt_phase {
            AdaptPhase::Warmup { since } => {
                if self.new_connections_warm() || now.duration_since(since) >= window {
                    self.adapt_phase = AdaptPhase::Measure {
                        since: now,
                        bytes: 0,
                    };
                }
            }
            AdaptPhase::Measure { since, bytes } => {
                let el = now.duration_since(since);
                if el < (window * 3 / 10).max(Duration::from_millis(300)) {
                    return;
                }
                let speed = bytes as f64 / el.as_secs_f64().max(1e-3);
                if self.active() < self.target {
                    // Slots we cannot fill (nothing left to split): growing is pointless now.
                    self.baseline = Some(speed);
                    self.adapt_phase = AdaptPhase::Measure {
                        since: now,
                        bytes: 0,
                    };
                    return;
                }
                let grow = match self.baseline {
                    None => true,
                    Some(b) if speed > b * 1.05 => {
                        self.batch = self.batch.saturating_mul(2);
                        true
                    }
                    Some(_) => false,
                };
                self.baseline = Some(speed);
                if grow {
                    self.target = (self.target + self.batch).min(limit);
                    self.grow_mark = self.slots.len();
                    self.adapt_phase = AdaptPhase::Warmup { since: now };
                } else {
                    self.growing = false;
                }
                tracing::debug!(
                    target = self.target,
                    batch = self.batch,
                    growing = self.growing,
                    speed,
                    "adapt"
                );
            }
        }
    }

    fn replace_slow(&mut self) {
        let now = Instant::now();
        let conn: Vec<usize> = (0..self.slots.len())
            .filter(|&i| {
                let s = &self.slots[i];
                s.worker.as_ref().is_some_and(|r| !r.replacing)
                    && s.seg.connected.load(Ordering::Acquire)
            })
            .collect();
        if conn.len() < 3 {
            return;
        }
        for &i in &conn {
            let s = &self.slots[i];
            let Some(run) = &s.worker else { continue };
            if now.duration_since(run.started) < self.cfg.slow_conn_grace
                || s.last_replaced
                    .is_some_and(|t| now.duration_since(t) < REPLACE_COOLDOWN)
                || s.seg.remaining() < self.cfg.min_split_size
            {
                continue;
            }
            let mut others: Vec<f64> = conn
                .iter()
                .filter(|&&j| j != i)
                .map(|&j| self.slots[j].speed)
                .collect();
            others.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = if others.len() % 2 == 1 {
                others[others.len() / 2]
            } else {
                (others[others.len() / 2 - 1] + others[others.len() / 2]) / 2.0
            };
            if median > 0.0 && s.speed < self.cfg.slow_conn_ratio * median {
                tracing::debug!(
                    seg = i,
                    speed = s.speed,
                    median,
                    "replacing slow connection"
                );
                if let Some(run) = self.slots[i].worker.as_mut() {
                    run.replacing = true;
                    run.cancel.cancel();
                }
                return; // one per check
            }
        }
    }

    fn segments_state(&self) -> Vec<SegmentState> {
        self.slots
            .iter()
            .map(|s| SegmentState {
                start: s.seg.start,
                cursor: s.seg.written(),
                end: s.seg.end(),
            })
            .collect()
    }

    fn progress(&self, status: Status, base: &Progress) -> Progress {
        let downloaded: u64 = self
            .slots
            .iter()
            .map(|s| s.seg.cursor().saturating_sub(s.seg.start))
            .sum();
        let total = if self.ranged || self.ctx.total.is_some() {
            self.ctx.total
        } else if self.all_done() {
            Some(downloaded)
        } else {
            None
        };
        Progress {
            status,
            file_name: base.file_name.clone(),
            path: base.path.clone(),
            downloaded,
            total,
            speed_bps: if self.active() == 0 {
                0.0
            } else {
                self.speed.max(0.0)
            },
            connections: self.active(),
            resumable: self.ranged,
            segments: self
                .slots
                .iter()
                .map(|s| SegmentInfo {
                    start: s.seg.start,
                    cursor: s.seg.cursor(),
                    end: if s.seg.end() == u64::MAX {
                        s.seg.cursor()
                    } else {
                        s.seg.end()
                    },
                    speed_bps: s.speed.max(0.0),
                    active: s.worker.is_some(),
                })
                .collect(),
        }
    }
}

async fn flush_writer(tx: &mpsc::Sender<WriteCmd>) -> std::io::Result<()> {
    let (otx, orx) = oneshot::channel();
    if tx.send(WriteCmd::Flush(otx)).await.is_err() {
        return Err(std::io::Error::other("disk writer stopped"));
    }
    match orx.await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::other("disk writer stopped")),
    }
}

/// `sync_data()` the partial file, then save the state (whose cursors were snapshotted
/// before the sync), so the `.thrennix` never claims bytes that are not on disk.
async fn sync_and_save(
    file: Option<Arc<File>>,
    st: DownloadState,
    path: PathBuf,
) -> std::io::Result<()> {
    let res = tokio::task::spawn_blocking(move || {
        if let Some(f) = &file {
            f.sync_data()?;
        }
        drop(file);
        st.save_atomic(&path)
    })
    .await;
    match res {
        Ok(r) => r,
        Err(e) => Err(std::io::Error::other(e.to_string())),
    }
}

async fn save_state(st: DownloadState, path: PathBuf) -> std::io::Result<()> {
    match tokio::task::spawn_blocking(move || st.save_atomic(&path)).await {
        Ok(r) => r,
        Err(e) => Err(std::io::Error::other(e.to_string())),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_job(
    inner: &Arc<Inner>,
    req: &DownloadRequest,
    mut probe: Probe,
    final_path: PathBuf,
    target: Target,
    ptx: &Arc<watch::Sender<Progress>>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let cfg = &inner.config;
    let file_name = final_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    let sp = state_path(&final_path);
    let pp = part_path(&final_path);
    let ranged = probe.ranged;
    let total = probe.total;
    let mut base = empty_progress(Status::Downloading, file_name.clone(), final_path.clone());
    base.total = total;
    base.resumable = ranged;

    let (file, resume) = match target {
        Target::Fresh(f) => (f, None),
        Target::Resume(f, st) => (f, Some(st)),
    };

    let mut state = DownloadState {
        version: STATE_VERSION,
        url: req.url.clone(),
        final_url: probe.final_url.clone(),
        total,
        etag: probe.etag.clone(),
        last_modified: probe.last_modified.clone(),
        file_name: file_name.clone(),
        resumable: ranged,
        segments: Vec::new(),
        cookies: req.cookies.clone(),
        referer: req.referer.clone(),
        user_agent: req.user_agent.clone(),
        headers: req.headers.clone(),
    };

    // Empty file: nothing to fetch.
    if total == Some(0) {
        drop(probe.response.take());
        drop(file);
        let (pp2, fp2, sp2) = (pp.clone(), final_path.clone(), sp.clone());
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            rename_with_retry(&pp2, &fp2)?;
            let _ = std::fs::remove_file(&sp2);
            Ok(())
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))??;
        let p = base.clone();
        ptx.send_replace(p);
        return Ok(final_path);
    }

    // Writer thread.
    let cap = (cfg.write_batch_size.saturating_mul(2) / cfg.read_buffer_size.max(1)).clamp(8, 1024);
    let (tx, rx) = mpsc::channel::<WriteCmd>(cap);
    let werr: WriterError = Arc::new(Mutex::new(None));
    let batch = cfg.write_batch_size.max(64 * 1024);
    let werr2 = werr.clone();
    // Second handle used only to flush data to disk before each state save.
    let mut sync_file = file.try_clone().ok().map(Arc::new);
    let writer_join =
        tokio::task::spawn_blocking(move || writer::writer_loop(file, rx, batch, werr2));

    let same = http::same_origin(&req.url, &probe.final_url);
    let ctx = Arc::new(WorkerCtx {
        client: inner.client.clone(),
        url: probe.final_url.clone(),
        headers: http::base_headers(cfg, req, same),
        if_range: if ranged {
            http::if_range_value(probe.etag.as_deref(), probe.last_modified.as_deref())
        } else {
            None
        },
        ranged,
        total,
        timeout: cfg.timeout,
        read_buf: cfg.read_buffer_size.max(16 * 1024),
        tx,
    });

    let stop_token = cancel.child_token();
    let mut c = Controller {
        cfg,
        ctx,
        slots: Vec::new(),
        joins: JoinSet::new(),
        stop_token: stop_token.clone(),
        ranged,
        target: if ranged {
            cfg.initial_connections.clamp(1, cfg.max_connections.max(1))
        } else {
            1
        },
        cap: if ranged {
            cfg.max_connections.max(1)
        } else {
            1
        },
        growing: ranged,
        baseline: None,
        speed: 0.0,
        batch: 2,
        adapt_phase: AdaptPhase::Warmup {
            since: Instant::now(),
        },
        grow_mark: 0,
        throttled: false,
        throttle_streak: 0,
        next_start_at: Instant::now(),
    };

    let mut probe_resp = probe.response.take();
    match (&resume, ranged, total) {
        (Some(st), true, Some(_)) => {
            for s in &st.segments {
                let seg = Segment::new(s.start, s.cursor, s.end);
                let pending = !seg.is_done();
                c.slots.push(Slot::new(seg, pending));
            }
            // Reuse the probe connection if some segment starts at 0.
            if let Some(i) = c
                .slots
                .iter()
                .position(|s| s.pending && s.seg.cursor() == 0)
            {
                if let Some(r) = probe_resp.take() {
                    c.start(i, Some(r));
                }
            }
        }
        (_, true, Some(t)) => {
            let plan = initial_plan(t, c.target, cfg.min_split_size);
            for (k, (a, b)) in plan.iter().enumerate() {
                c.slots.push(Slot::new(Segment::new(*a, *a, *b), k != 0));
            }
            if let Some(r) = probe_resp.take() {
                c.start(0, Some(r));
            } else if let Some(s) = c.slots.first_mut() {
                s.pending = true;
            }
        }
        _ => {
            c.slots.push(Slot::new(
                Segment::new(0, 0, total.unwrap_or(u64::MAX)),
                true,
            ));
            if let Some(r) = probe_resp.take() {
                c.start(0, Some(r));
            }
        }
    }
    drop(probe_resp);
    ptx.send_replace(c.progress(Status::Downloading, &base));

    // Persist an initial state file so a crash right away is still resumable.
    state.segments = c.segments_state();
    if let Err(e) = save_state(state.clone(), sp.clone()).await {
        tracing::warn!("could not save state file: {e}");
    }

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_tick = Instant::now();
    let mut last_save = Instant::now();
    let mut last_slow = Instant::now();
    let mut save_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut stop: Option<Stop> = None;

    loop {
        if stop.is_none() {
            c.fill();
            if c.joins.is_empty() && c.all_done() {
                break;
            }
        } else if c.joins.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled(), if stop.is_none() => {
                tracing::debug!("pause requested");
                stop = Some(Stop::Paused);
                stop_token.cancel();
            }
            j = c.joins.join_next(), if !c.joins.is_empty() => {
                let err = match j {
                    Some(Ok((i, exit))) => c.on_exit(i, exit).await,
                    Some(Err(e)) => Some(Error::Other(format!("worker task failed: {e}"))),
                    None => None,
                };
                if let Some(e) = err {
                    if stop.is_none() {
                        tracing::debug!("stopping download: {e}");
                        stop = Some(Stop::Failed(e));
                        stop_token.cancel();
                    }
                }
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                let dt = now.duration_since(last_tick).as_secs_f64().max(1e-3);
                last_tick = now;
                c.sample(dt);
                if stop.is_none() {
                    let werr_now = werr.lock().ok().and_then(|g| g.clone());
                    if let Some((kind, msg)) = werr_now {
                        stop = Some(Stop::Failed(Error::Io(std::io::Error::new(kind, msg))));
                        stop_token.cancel();
                        continue;
                    }
                    c.adapt(now);
                    if now.duration_since(last_slow) >= SLOW_CHECK_EVERY {
                        last_slow = now;
                        c.replace_slow();
                    }
                    let saving = save_task.as_ref().is_some_and(|t| !t.is_finished());
                    if now.duration_since(last_save) >= SAVE_EVERY && !saving {
                        last_save = now;
                        // Snapshot the written cursors *before* syncing, so the state file
                        // never claims bytes that are not on disk. Runs in the background so
                        // a slow flush does not stall the scheduler.
                        state.segments = c.segments_state();
                        let (st, path, f) = (state.clone(), sp.clone(), sync_file.clone());
                        save_task = Some(tokio::spawn(async move {
                            if let Err(e) = sync_and_save(f, st, path).await {
                                tracing::warn!("could not save state file: {e}");
                            }
                        }));
                    }
                    ptx.send_replace(c.progress(Status::Downloading, &base));
                }
            }
        }
    }

    // Drain the writer and close the file before touching it.
    let flush_res = flush_writer(&c.ctx.tx).await;
    let mut final_progress = c.progress(Status::Downloading, &base);
    final_progress.speed_bps = 0.0;
    final_progress.connections = 0;
    state.segments = c.segments_state();
    drop(c);
    if let Err(e) = writer_join.await {
        tracing::error!("writer thread failed: {e}");
    }
    if let Some(t) = save_task.take() {
        let _ = t.await;
    }
    let werr_now = werr.lock().ok().and_then(|g| g.clone());
    if stop.is_none() {
        if let Some((kind, msg)) = werr_now {
            stop = Some(Stop::Failed(Error::Io(std::io::Error::new(kind, msg))));
        } else if let Err(e) = flush_res {
            stop = Some(Stop::Failed(e.into()));
        }
    }

    match stop {
        None => {
            let downloaded = final_progress.downloaded;
            let (pp2, fp2, sp2) = (pp.clone(), final_path.clone(), sp.clone());
            let fix_len = if !ranged && total.is_none() {
                Some(downloaded)
            } else {
                None
            };
            // Close our last handle on the .part file before renaming it.
            drop(sync_file);
            let host_url = probe.final_url.clone();
            let referer = req.referer.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                if let Some(len) = fix_len {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&pp2)?
                        .set_len(len)?;
                }
                rename_with_retry(&pp2, &fp2)?;
                let _ = std::fs::remove_file(&sp2);
                write_mark_of_the_web(&fp2, &host_url, referer.as_deref());
                Ok(())
            })
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
            final_progress.total = Some(total.unwrap_or(downloaded));
            final_progress.downloaded = total.unwrap_or(downloaded);
            ptx.send_replace(final_progress);
            tracing::info!(path = %final_path.display(), "download complete");
            Ok(final_path)
        }
        Some(stop) => {
            if let Err(e) = sync_and_save(sync_file.take(), state, sp).await {
                tracing::warn!("could not save state file: {e}");
            }
            match stop {
                Stop::Paused => {
                    ptx.send_replace(final_progress);
                    Err(Error::Paused)
                }
                // `run` publishes Failed (only once nothing will retry this download).
                Stop::Failed(e) => {
                    ptx.send_replace(final_progress);
                    Err(e)
                }
            }
        }
    }
}
