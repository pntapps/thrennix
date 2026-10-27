//! The download task: plan → reserve name → parallel segment fetch → concat → ffmpeg.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::http::Http;
use crate::model::{live_error, select_index, Item, Plan, Track};
use crate::reserve::{reserve, Reservation};
use crate::state::{fingerprint, Tracker};
use crate::util::{
    find_ffmpeg, output_name, rename_with_retry, unique_path, write_zone_identifier,
};
use crate::{dash, hls, mux, Error, MediaKind, MediaRequest, Progress, Quality, Result, Status};

struct Meta {
    status: Status,
    file_name: String,
    /// Reserved (later: actual) output path; empty until reserved.
    path: PathBuf,
    total: Option<u64>,
}

pub(crate) struct Shared {
    pub progress: watch::Sender<Progress>,
    pub cancel: watch::Sender<bool>,
    result: watch::Sender<Option<Arc<Result<PathBuf>>>>,
    pub segs_done: AtomicUsize,
    pub segs_total: AtomicUsize,
    pub muxing: AtomicBool,
    active: AtomicUsize,
    /// Bytes received for finished segments.
    finished_bytes: AtomicU64,
    /// Every byte received (including in-flight and retried), for the speed meter.
    raw_bytes: AtomicU64,
    meta: Mutex<Meta>,
    work_dir: Mutex<Option<PathBuf>>,
    output: Mutex<Option<PathBuf>>,
    tracker: Mutex<Option<Arc<Tracker>>>,
}

impl Shared {
    pub fn new(req: &MediaRequest) -> Arc<Self> {
        Self::with_name(output_name(req.file_name.as_deref(), &req.url))
    }

    pub fn with_name(file_name: String) -> Arc<Self> {
        let progress = Progress {
            status: Status::Probing,
            file_name: file_name.clone(),
            path: PathBuf::new(),
            downloaded: 0,
            total: None,
            speed_bps: 0.0,
            connections: 0,
            resumable: true,
            segments: Vec::new(),
        };
        Arc::new(Shared {
            progress: watch::Sender::new(progress),
            cancel: watch::Sender::new(false),
            result: watch::Sender::new(None),
            segs_done: AtomicUsize::new(0),
            segs_total: AtomicUsize::new(0),
            muxing: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            finished_bytes: AtomicU64::new(0),
            raw_bytes: AtomicU64::new(0),
            meta: Mutex::new(Meta {
                status: Status::Probing,
                file_name,
                path: PathBuf::new(),
                total: None,
            }),
            work_dir: Mutex::new(None),
            output: Mutex::new(None),
            tracker: Mutex::new(None),
        })
    }

    pub fn work_dir(&self) -> Option<PathBuf> {
        self.work_dir.lock().unwrap().clone()
    }

    pub fn output_path(&self) -> Option<PathBuf> {
        self.output.lock().unwrap().clone()
    }

    pub fn set_status(&self, status: Status) {
        self.meta.lock().unwrap().status = status;
    }

    /// For backends that report byte progress directly (yt-dlp).
    pub fn set_bytes(&self, downloaded: u64, total: Option<u64>, raw: u64, connections: usize) {
        self.finished_bytes.store(downloaded, Ordering::Relaxed);
        self.raw_bytes.store(raw, Ordering::Relaxed);
        self.active.store(connections, Ordering::Relaxed);
        self.meta.lock().unwrap().total = total;
    }

    pub fn raw_bytes(&self) -> u64 {
        self.raw_bytes.load(Ordering::Relaxed)
    }

    /// Publish the final state and wake `wait()`ers.
    pub fn complete(&self, result: Result<PathBuf>) {
        self.active.store(0, Ordering::Relaxed);
        self.muxing.store(false, Ordering::Relaxed);
        match &result {
            Ok(path) => {
                self.set_output(path);
                self.set_status(Status::Completed);
            }
            Err(Error::Paused) => self.set_status(Status::Paused),
            Err(e) => {
                tracing::warn!("media download failed: {e}");
                self.set_status(Status::Failed(e.to_string()));
            }
        }
        self.publish(0.0);
        self.result.send_replace(Some(Arc::new(result)));
    }

    pub fn set_output(&self, path: &Path) {
        *self.output.lock().unwrap() = Some(path.to_path_buf());
        let mut m = self.meta.lock().unwrap();
        m.path = path.to_path_buf();
        if let Some(n) = path.file_name() {
            m.file_name = n.to_string_lossy().into_owned();
        }
    }

    pub fn publish(&self, speed_bps: f64) {
        let p = {
            let m = self.meta.lock().unwrap();
            Progress {
                status: m.status.clone(),
                file_name: m.file_name.clone(),
                path: m.path.clone(),
                downloaded: self.finished_bytes.load(Ordering::Relaxed),
                total: m.total,
                speed_bps,
                connections: if self.muxing.load(Ordering::Relaxed) {
                    0
                } else {
                    self.active.load(Ordering::Relaxed)
                },
                resumable: true,
                segments: Vec::new(),
            }
        };
        self.progress.send_replace(p);
    }

    pub async fn wait(&self) -> Result<PathBuf> {
        let mut rx = self.result.subscribe();
        loop {
            let current = rx.borrow_and_update().clone();
            if let Some(r) = current {
                return clone_result(&r);
            }
            if rx.changed().await.is_err() {
                return Err(Error::Other("download task vanished".into()));
            }
        }
    }
}

fn clone_error(e: &Error) -> Error {
    match e {
        Error::Http(s) => Error::Http(s.clone()),
        Error::Status(c) => Error::Status(*c),
        Error::Io(io) => Error::Io(std::io::Error::new(io.kind(), io.to_string())),
        Error::RemoteChanged => Error::RemoteChanged,
        Error::Paused => Error::Paused,
        Error::Other(s) => Error::Other(s.clone()),
    }
}

fn clone_result(r: &Result<PathBuf>) -> Result<PathBuf> {
    match r {
        Ok(p) => Ok(p.clone()),
        Err(e) => Err(clone_error(e)),
    }
}

/// Bytes/second over a sliding ~2 s window, lightly smoothed.
pub(crate) struct Speedo {
    samples: VecDeque<(Instant, u64)>,
    value: f64,
}

impl Speedo {
    pub(crate) fn new() -> Self {
        Speedo {
            samples: VecDeque::new(),
            value: 0.0,
        }
    }

    pub(crate) fn update(&mut self, total: u64) -> f64 {
        let now = Instant::now();
        self.samples.push_back((now, total));
        while self.samples.len() > 2
            && now.duration_since(self.samples[0].0) > Duration::from_secs(2)
        {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        let dt = now.duration_since(t0).as_secs_f64();
        let inst = if dt > 0.05 {
            total.saturating_sub(b0) as f64 / dt
        } else {
            self.value
        };
        self.value = 0.6 * self.value + 0.4 * inst;
        if self.value < 1.0 {
            self.value = 0.0;
        }
        self.value
    }
}

pub(crate) async fn cancelled(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub(crate) async fn run(
    shared: Arc<Shared>,
    kind: MediaKind,
    req: MediaRequest,
    quality: Quality,
    concurrency: usize,
) {
    let mut cancel_rx = shared.cancel.subscribe();
    let mut work = Box::pin(run_inner(shared.clone(), kind, req, quality, concurrency));
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut speedo = Speedo::new();
    let result = loop {
        tokio::select! {
            r = &mut work => break r,
            _ = cancelled(&mut cancel_rx) => break Err(Error::Paused),
            _ = tick.tick() => {
                let s = speedo.update(shared.raw_bytes.load(Ordering::Relaxed));
                shared.publish(s);
            }
        }
    };
    // Dropping the future aborts the workers (JoinSet), kills ffmpeg (kill_on_drop) and
    // releases the work-dir lock (Reservation).
    drop(work);
    let tracker = shared.tracker.lock().unwrap().clone();
    if let Some(t) = tracker {
        if result.is_err() {
            if let Err(e) = t.save() {
                tracing::warn!("could not save media state: {e}");
            }
        }
    }
    shared.complete(result);
}

async fn load_hls_media(http: &Http, url: &str) -> Result<Track> {
    let (text, base) = http.get_text(url).await?;
    let p = hls::parse_media(&text, &base)?;
    if !p.ended {
        return Err(live_error());
    }
    Ok(p.into_track())
}

async fn plan_hls(http: &Http, url: &str, quality: Quality) -> Result<Plan> {
    let (text, base) = http.get_text(url).await?;
    if !hls::is_master(&text) {
        let p = hls::parse_media(&text, &base)?;
        if !p.ended {
            return Err(live_error());
        }
        return Ok(Plan {
            video: p.into_track(),
            audio: None,
        });
    }
    let m = hls::parse_master(&text, &base)?;
    let cands: Vec<_> = m.variants.iter().map(|v| (v.height, v.bandwidth)).collect();
    let i = select_index(&cands, quality)
        .ok_or_else(|| Error::Other("master playlist lists no variants".into()))?;
    let v = &m.variants[i];
    let video = load_hls_media(http, &v.uri).await?;
    let audio = match m.audio_for(v).and_then(|r| r.uri.clone()) {
        Some(u) => Some(load_hls_media(http, &u).await?),
        None => None,
    };
    Ok(Plan { video, audio })
}

async fn plan_dash(http: &Http, url: &str, quality: Quality) -> Result<Plan> {
    let (text, base) = http.get_text(url).await?;
    dash::parse_mpd(&text, &base)?.plan(quality)
}

struct Job {
    file: String,
    item: Item,
}

struct KeyCache {
    keys: tokio::sync::Mutex<HashMap<String, [u8; 16]>>,
}

pub(crate) fn parse_key(raw: &[u8]) -> Option<[u8; 16]> {
    if let Ok(k) = <[u8; 16]>::try_from(raw) {
        return Some(k);
    }
    // Some servers hand out the key hex-encoded.
    let text = std::str::from_utf8(raw).ok()?.trim();
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut k = [0u8; 16];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(k)
}

impl KeyCache {
    async fn get(&self, http: &Http, uri: &str) -> Result<[u8; 16]> {
        let mut keys = self.keys.lock().await;
        if let Some(k) = keys.get(uri) {
            return Ok(*k);
        }
        let raw = http.get_key(uri).await?;
        let key = parse_key(&raw).ok_or_else(|| {
            Error::Other(format!(
                "AES-128 key from {uri} is {} bytes, expected 16",
                raw.len()
            ))
        })?;
        keys.insert(uri.to_string(), key);
        Ok(key)
    }
}

pub(crate) fn decrypt_aes128(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| {
            Error::Other("AES-128 decryption failed (wrong key or corrupt segment)".into())
        })
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl<'a> ActiveGuard<'a> {
    fn new(c: &'a AtomicUsize) -> Self {
        c.fetch_add(1, Ordering::Relaxed);
        ActiveGuard(c)
    }
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct WorkerCtx {
    shared: Arc<Shared>,
    http: Http,
    tracker: Arc<Tracker>,
    jobs: Vec<Job>,
    next: AtomicUsize,
    keys: KeyCache,
    work_dir: PathBuf,
    run_id: String,
}

/// Fetch one segment into `part`; returns bytes received from the network.
async fn fetch_job(ctx: &WorkerCtx, job: &Job, part: &Path) -> Result<u64> {
    let _active = ActiveGuard::new(&ctx.shared.active);
    let item = &job.item;
    match &item.key {
        None => {
            let mut f = tokio::fs::File::create(part).await?;
            let n = ctx
                .http
                .download_to(
                    &item.url,
                    item.range.as_ref(),
                    &ctx.shared.raw_bytes,
                    &mut f,
                )
                .await?;
            Ok(n)
        }
        Some(k) => {
            let data = ctx
                .http
                .get_bytes(&item.url, item.range.as_ref(), &ctx.shared.raw_bytes)
                .await?;
            let key = ctx.keys.get(&ctx.http, &k.uri).await?;
            let plain = decrypt_aes128(&data, &key, &k.iv)?;
            tokio::fs::write(part, &plain).await?;
            Ok(data.len() as u64)
        }
    }
}

async fn worker(ctx: Arc<WorkerCtx>) -> Result<()> {
    loop {
        let i = ctx.next.fetch_add(1, Ordering::SeqCst);
        let Some(job) = ctx.jobs.get(i) else {
            return Ok(());
        };
        if ctx.tracker.done_bytes(&job.file).is_some() {
            continue;
        }
        let dest = ctx.work_dir.join(&job.file);
        let part = ctx
            .work_dir
            .join(format!("{}.{}.part", job.file, ctx.run_id));
        let received = match fetch_job(&ctx, job, &part).await {
            Ok(n) => n,
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
        };
        rename_with_retry(&part, &dest).await?;
        ctx.tracker.mark_done(&job.file, received);
        ctx.shared
            .finished_bytes
            .fetch_add(received, Ordering::Relaxed);
        ctx.shared.segs_done.fetch_add(1, Ordering::Relaxed);
        ctx.tracker.maybe_save();
    }
}

fn segment_files(prefix: &str, track: &Track) -> Vec<String> {
    (1..=track.items.len())
        .map(|i| format!("{prefix}_{i:05}.seg"))
        .collect()
}

fn run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}{:x}{:x}",
        std::process::id(),
        nanos & 0xffff_ffff,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T> {
    Ok(tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(format!("background task failed: {e}")))??)
}

async fn run_inner(
    shared: Arc<Shared>,
    kind: MediaKind,
    req: MediaRequest,
    quality: Quality,
    concurrency: usize,
) -> Result<PathBuf> {
    let http = Http::new(&req)?;
    let plan = match kind {
        MediaKind::Hls => plan_hls(&http, &req.url, quality).await?,
        MediaKind::Dash => plan_dash(&http, &req.url, quality).await?,
    };

    let file_name = shared.meta.lock().unwrap().file_name.clone();
    // output_name() guarantees a (case-insensitive) ".mp4" suffix.
    let stem = file_name
        .get(..file_name.len().saturating_sub(4))
        .filter(|s| !s.is_empty())
        .unwrap_or("video")
        .to_string();
    let fp = fingerprint(&plan);
    let mut reservation: Reservation = {
        let (dest, stem, url, fp) = (req.dest_dir.clone(), stem, req.url.clone(), fp.clone());
        blocking(move || reserve(&dest, &stem, &url, &fp)).await?
    };
    shared.set_output(&reservation.output);
    *shared.work_dir.lock().unwrap() = Some(reservation.work_dir.clone());
    let work_dir = reservation.work_dir.clone();

    let tracker = {
        let (w, u) = (work_dir.clone(), req.url.clone());
        Arc::new(blocking(move || Tracker::open(&w, &u, fp)).await?)
    };
    *shared.tracker.lock().unwrap() = Some(tracker.clone());

    let v_files = segment_files("v", &plan.video);
    let a_files = plan
        .audio
        .as_ref()
        .map(|a| segment_files("a", a))
        .unwrap_or_default();

    // Interleave video and audio jobs by relative position so both progress together.
    let mut keyed: Vec<(f64, Job)> = Vec::new();
    for (prefix_files, track) in [
        (&v_files, Some(&plan.video)),
        (&a_files, plan.audio.as_ref()),
    ] {
        let Some(track) = track else { continue };
        let n = track.items.len().max(1) as f64;
        for (i, (f, it)) in prefix_files.iter().zip(&track.items).enumerate() {
            keyed.push((
                i as f64 / n,
                Job {
                    file: f.clone(),
                    item: it.clone(),
                },
            ));
        }
    }
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let jobs: Vec<Job> = keyed.into_iter().map(|(_, j)| j).collect();

    let total = plan
        .video
        .items
        .iter()
        .chain(plan.audio.iter().flat_map(|a| a.items.iter()))
        .try_fold(0u64, |acc, i| acc.checked_add(i.range.as_ref()?.length));
    let (done_n, done_b) = jobs
        .iter()
        .filter_map(|j| tracker.done_bytes(&j.file))
        .fold((0usize, 0u64), |(n, b), x| (n + 1, b.saturating_add(x)));
    shared.segs_total.store(jobs.len(), Ordering::Relaxed);
    shared.segs_done.store(done_n, Ordering::Relaxed);
    shared.finished_bytes.store(done_b, Ordering::Relaxed);
    {
        let mut m = shared.meta.lock().unwrap();
        m.total = total;
        m.status = Status::Downloading;
    }
    shared.publish(0.0);

    if done_n < jobs.len() {
        let workers = concurrency.min(jobs.len() - done_n).max(1);
        let ctx = Arc::new(WorkerCtx {
            shared: shared.clone(),
            http: http.clone(),
            tracker: tracker.clone(),
            jobs,
            next: AtomicUsize::new(0),
            keys: KeyCache {
                keys: tokio::sync::Mutex::new(HashMap::new()),
            },
            work_dir: work_dir.clone(),
            run_id: run_id(),
        });
        let mut set = JoinSet::new();
        for _ in 0..workers {
            set.spawn(worker(ctx.clone()));
        }
        while let Some(r) = set.join_next().await {
            let r = r.unwrap_or_else(|e| Err(Error::Other(format!("segment worker crashed: {e}"))));
            if let Err(e) = r {
                set.abort_all();
                while set.join_next().await.is_some() {}
                tracker.save()?;
                return Err(e);
            }
        }
    }
    tracker.save()?;

    shared.muxing.store(true, Ordering::Relaxed);
    shared.publish(0.0);
    let out = finish(&plan, &v_files, &a_files, &mut reservation).await?;
    write_zone_identifier(&out, &req.url, req.referer.as_deref());
    Ok(out)
}

/// Keep the concatenated streams as they are (no ffmpeg, or ffmpeg failed).
async fn keep_raw(
    video: &Path,
    video_ext: &str,
    audio: Option<&(PathBuf, &'static str)>,
    dest_dir: &Path,
    stem: &str,
) -> Result<PathBuf> {
    let out = unique_path(dest_dir, stem, video_ext);
    rename_with_retry(video, &out).await?;
    if let Some((a, ext)) = audio {
        let aout = unique_path(dest_dir, &format!("{stem}.audio"), ext);
        rename_with_retry(a, &aout).await?;
        tracing::warn!("separate audio track saved as {}", aout.display());
    }
    Ok(out)
}

async fn finish(
    plan: &Plan,
    v_files: &[String],
    a_files: &[String],
    res: &mut Reservation,
) -> Result<PathBuf> {
    let work_dir = res.work_dir.clone();
    let dest_dir = res
        .output
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let video = work_dir.join(format!("video{}", plan.video.ext));
    mux::concat(
        v_files.iter().map(|f| work_dir.join(f)).collect(),
        video.clone(),
    )
    .await?;
    let audio = match &plan.audio {
        Some(a) => {
            let p = work_dir.join(format!("audio{}", a.ext));
            mux::concat(
                a_files.iter().map(|f| work_dir.join(f)).collect(),
                p.clone(),
            )
            .await?;
            Some((p, a.ext))
        }
        None => None,
    };

    let muxed = match find_ffmpeg() {
        Some(ffmpeg) => {
            let tmp = work_dir.join("output.mp4");
            match mux::ffmpeg_mux(
                &ffmpeg,
                (&video, plan.video.ext),
                audio.as_ref().map(|(p, e)| (p.as_path(), *e)),
                &tmp,
                mux::Container::Mp4,
            )
            .await
            {
                Ok(()) => Some(tmp),
                Err(e) => {
                    tracing::warn!("{e}; keeping the concatenated stream without muxing");
                    None
                }
            }
        }
        None => {
            tracing::warn!("ffmpeg not found; keeping the concatenated stream without muxing");
            None
        }
    };
    let final_path = match muxed {
        Some(tmp) => {
            // The name is reserved, but never overwrite something a user put there meanwhile.
            let out = if res.output.exists() {
                unique_path(&dest_dir, &res.stem, ".mp4")
            } else {
                res.output.clone()
            };
            rename_with_retry(&tmp, &out).await?;
            out
        }
        None => keep_raw(&video, plan.video.ext, audio.as_ref(), &dest_dir, &res.stem).await?,
    };
    let _ = tokio::fs::remove_file(&res.placeholder).await;
    res.unlock();
    if let Err(e) = tokio::fs::remove_dir_all(&work_dir).await {
        tracing::warn!("could not remove work dir {}: {e}", work_dir.display());
    }
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockEncryptMut;

    #[test]
    fn aes128_roundtrip() {
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let plain = b"hello media segment, not a multiple of 16".to_vec();
        let enc = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plain);
        assert_eq!(decrypt_aes128(&enc, &key, &iv).unwrap(), plain);
        assert!(
            decrypt_aes128(&enc, &[8u8; 16], &iv).is_err()
                || decrypt_aes128(&enc, &[8u8; 16], &iv).unwrap() != plain
        );
        // Truncated / non-block-sized ciphertext is an error, not a panic.
        assert!(decrypt_aes128(&enc[..5], &key, &iv).is_err());
        assert!(decrypt_aes128(&[], &key, &iv).is_err());
    }

    #[test]
    fn key_parsing() {
        assert_eq!(parse_key(&[1u8; 16]), Some([1u8; 16]));
        let hex = b"000102030405060708090a0b0c0d0e0f";
        assert_eq!(parse_key(hex).unwrap()[15], 15);
        assert_eq!(parse_key(b"short"), None);
        assert_eq!(parse_key("é".repeat(16).as_bytes()), None);
        assert_eq!(parse_key(&[0xff; 32]), None);
    }

    #[test]
    fn error_cloning() {
        assert!(matches!(
            clone_result(&Err(Error::Paused)),
            Err(Error::Paused)
        ));
        assert!(matches!(
            clone_result(&Err(Error::Status(404))),
            Err(Error::Status(404))
        ));
    }
}
