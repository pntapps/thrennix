//! Shared app state: settings, the download list, persistence, and download control.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thrennix_core::{DownloadRequest, EngineConfig, Error, Progress, SegmentInfo, Status};
use tokio::runtime::Handle;

use crate::backend::{Backend, Job};
use crate::protocol::{sanitize_file_name, validate_url, DownloadMsg, Kind};
use crate::theme::{classify, Category, ThemeChoice};

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Show the file selected in Explorer, or open the folder. Errors carry the path, since
/// Explorer/ShellExecute fail on paths longer than MAX_PATH.
pub fn reveal(path: Option<&Path>, dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const MAX_PATH: usize = 260;
    if let Some(p) = path.filter(|p| p.exists()) {
        if p.as_os_str().len() >= MAX_PATH {
            let _ = opener::open(p.parent().unwrap_or(dir));
            return Err(format!("Path too long for Explorer: {}", p.display()));
        }
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", p.display()))
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Cannot open {} ({e})", p.display()))
    } else {
        opener::open(dir).map_err(|e| format!("Cannot open {} ({e})", dir.display()))
    }
}

/// Browser downloads waiting on the "Ask where to save" dialog, at most.
const MAX_PENDING_ASK: usize = 20;
/// How long "remove" waits for a running job to stop before deleting its files.
const REMOVE_WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- persistence helpers

/// Write to `<path>.tmp`, fsync, then rename over `path`.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = with_suffix(path, ".tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// `%APPDATA%\Thrennix`
pub fn data_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("Thrennix"))
}

fn load_json<T: for<'de> Deserialize<'de>>(name: &str) -> Option<T> {
    let bytes = std::fs::read(data_dir()?.join(name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_json<T: Serialize>(name: &str, value: &T) -> io::Result<()> {
    let dir = data_dir().ok_or_else(|| io::Error::other("no app data dir"))?;
    std::fs::create_dir_all(&dir)?;
    write_atomic(&dir.join(name), &serde_json::to_vec_pretty(value)?)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(suffix);
    PathBuf::from(s)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn remove_quietly(p: &Path) {
    let _ = std::fs::remove_file(p);
}

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub max_connections: usize,
    pub initial_connections: usize,
    pub ask_where: bool,
    pub theme: ThemeChoice,
    /// Open the per-download progress window when a browser download starts.
    pub show_progress_window: bool,
    /// Add URL dialog: skip the YouTube quality picker and take the best quality.
    /// Browser-extension downloads are not affected (the user picks there).
    pub yt_always_best: bool,
    /// Show an icon in the notification area (system tray).
    pub tray_icon: bool,
    /// The native minimize button hides the window to the tray.
    pub minimize_to_tray: bool,
    /// The close button hides the window to the tray; downloads keep running.
    pub close_to_tray: bool,
    /// Started with `--minimized` (Windows startup): stay hidden in the tray.
    pub start_minimized: bool,
    /// The one-time "still running in the tray" notification was shown.
    pub tray_hint_shown: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let download_dir = directories::UserDirs::new()
            .and_then(|u| u.download_dir().map(PathBuf::from))
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            download_dir,
            max_connections: 32,
            initial_connections: 8,
            ask_where: false,
            theme: ThemeChoice::System,
            show_progress_window: true,
            yt_always_best: false,
            tray_icon: true,
            minimize_to_tray: false,
            close_to_tray: true,
            start_minimized: true,
            tray_hint_shown: false,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut s: Settings = load_json("settings.json").unwrap_or_default();
        s.normalize();
        s
    }
    pub fn normalize(&mut self) {
        self.max_connections = self.max_connections.clamp(1, 32);
        self.initial_connections = self.initial_connections.clamp(1, self.max_connections);
    }
    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig {
            initial_connections: self.initial_connections,
            max_connections: self.max_connections,
            ..EngineConfig::default()
        }
    }
}

// ---------------------------------------------------------------- download records

/// A new download coming from the UI or the browser.
#[derive(Debug, Clone, Default)]
pub struct NewDownload {
    pub url: String,
    pub file_name: Option<String>,
    pub referer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
    pub kind: Kind,
    /// yt-dlp format id (kind Ytdlp only).
    pub format: Option<String>,
}

impl From<DownloadMsg> for NewDownload {
    fn from(m: DownloadMsg) -> Self {
        let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        NewDownload {
            url: m.url.trim().to_string(),
            file_name: m.filename.as_deref().and_then(sanitize_file_name),
            referer: nonempty(m.referer),
            cookies: nonempty(m.cookies),
            user_agent: nonempty(m.user_agent),
            headers: m.headers.unwrap_or_default().into_iter().collect(),
            kind: m.kind.unwrap_or_default(),
            format: m
                .format
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty()),
        }
    }
}

/// What gets persisted for each download. Cookies and header values are DPAPI-encrypted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    pub url: String,
    #[serde(default)]
    pub kind: Kind,
    /// yt-dlp format id (kind Ytdlp only).
    #[serde(default)]
    pub format: Option<String>,
    /// Display name (may be a guess until the engine resolves it).
    pub file_name: String,
    /// Name given by the user/extension; the only name passed to thrennix-media.
    #[serde(default)]
    pub requested_name: Option<String>,
    pub dest_dir: PathBuf,
    /// Final file path (file downloads) or media output path.
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub state_file: Option<PathBuf>,
    /// Media only: thrennix-media's segment directory.
    #[serde(default)]
    pub work_dir: Option<PathBuf>,
    pub status: Status,
    #[serde(default)]
    pub remote_changed: bool,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default, with = "crate::secret::opt")]
    pub cookies: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default, with = "crate::secret::headers")]
    pub headers: Vec<(String, String)>,
    /// Unix seconds of the last start/resume.
    #[serde(default)]
    pub last_try: Option<i64>,
    #[serde(default)]
    pub open_folder_when_done: bool,
    #[serde(default)]
    pub open_file_when_done: bool,
    /// Server supports byte ranges (None = not probed yet).
    #[serde(default)]
    pub resumable: Option<bool>,
}

impl Record {
    fn request(&self) -> DownloadRequest {
        let file_name = match self.kind {
            // Once the engine resolved the name, reuse it so the `.thrennix` state file is found.
            Kind::File => self
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| self.requested_name.clone()),
            // thrennix-media keys its resume state on the request: keep it identical.
            Kind::Hls | Kind::Dash | Kind::Ytdlp => self.requested_name.clone(),
        };
        DownloadRequest {
            url: self.url.clone(),
            dest_dir: self.dest_dir.clone(),
            file_name,
            cookies: self.cookies.clone(),
            referer: self.referer.clone(),
            user_agent: self.user_agent.clone(),
            headers: self.headers.clone(),
        }
    }

    /// A progress path is only meaningful once probing is over and it names a file.
    fn usable_path<'a>(&self, p: &'a Progress) -> Option<&'a Path> {
        let path = p.path.as_path();
        let ok = p.status != Status::Probing
            && !path.as_os_str().is_empty()
            && path != self.dest_dir
            && path.file_name().is_some()
            && path.extension().is_none_or(|e| e != "thrennix");
        ok.then_some(path)
    }

    /// Refresh from a live (not yet ended) job.
    fn apply_live(&mut self, p: &Progress, job: &dyn Job) {
        if p.status != Status::Probing {
            if let Some(path) = self.usable_path(p).map(Path::to_path_buf) {
                if self.kind == Kind::File {
                    self.state_file = Some(with_suffix(&path, ".thrennix"));
                }
                self.path = Some(path);
            }
            if !p.file_name.is_empty() {
                self.file_name = p.file_name.clone();
            }
            if p.total.is_some() {
                self.total = p.total;
            }
            self.downloaded = p.downloaded;
            self.resumable = Some(p.resumable);
        }
        if self.kind.is_media() {
            if let Some(w) = job.work_dir() {
                self.work_dir = Some(w);
            }
            if let Some(o) = job.output_path() {
                if let Some(n) = o.file_name() {
                    self.file_name = n.to_string_lossy().into_owned();
                }
                self.path = Some(o);
            }
        }
        // The job decides when it has ended (wait()); a transient Failed may be a retry.
        self.status = match &p.status {
            Status::Failed(_) | Status::Completed => Status::Downloading,
            s => s.clone(),
        };
    }

    fn drop_secrets(&mut self) {
        self.cookies = None;
        self.headers.clear();
    }

    /// Files to delete for "remove + delete file".
    fn files_to_delete(&self) -> (Vec<PathBuf>, Option<PathBuf>) {
        let done = self.status == Status::Completed;
        match (self.kind, done) {
            (_, true) => (self.path.iter().cloned().collect(), None),
            (Kind::File, false) => {
                let mut v: Vec<PathBuf> = self.state_file.iter().cloned().collect();
                if let Some(p) = &self.path {
                    // In-progress data lives in `<file>.part` until completion.
                    v.push(with_suffix(p, ".part"));
                    v.push(with_suffix(p, ".thrennix"));
                }
                (v, None)
            }
            (Kind::Hls | Kind::Dash, false) => {
                // thrennix-media holds the output name with an empty `<output>.thrennix-reserve`.
                let placeholder = self
                    .path
                    .iter()
                    .map(|p| with_suffix(p, ".thrennix-reserve"))
                    .collect();
                (placeholder, self.work_dir.clone())
            }
            (Kind::Ytdlp, false) => (
                self.path
                    .as_deref()
                    .map(ytdlp_leftovers)
                    .unwrap_or_default(),
                self.work_dir.clone(),
            ),
        }
    }
}

/// yt-dlp's partial files for an output like `Title [id].mp4`: `Title [id].f137.mp4.part`,
/// `Title [id].mp4.ytdl`, `...part-Frag12`. Only matched when the name carries a `[video id]`,
/// so unrelated files are never touched.
pub fn ytdlp_leftovers(output: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (output.parent(), output.file_stem().and_then(|s| s.to_str()))
    else {
        return Vec::new();
    };
    if !has_video_id(stem) {
        return Vec::new();
    }
    let prefix = format!("{stem}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&prefix)
                && (n.ends_with(".part") || n.ends_with(".ytdl") || n.contains(".part-Frag"))
        })
        .map(|e| e.path())
        .collect()
}

fn has_video_id(stem: &str) -> bool {
    stem.ends_with(']') && stem.rfind(" [").is_some_and(|i| stem.len() - i > 3)
}

/// Whether "remove + delete" can find this item's partial files.
pub fn partial_files_identifiable(kind: Kind, path: Option<&Path>) -> bool {
    match kind {
        Kind::Ytdlp => path
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .is_some_and(has_video_id),
        _ => true,
    }
}

fn guess_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back().map(str::to_string))
                .filter(|s| !s.is_empty())
        })
        .and_then(|s| sanitize_file_name(&s))
        .unwrap_or_else(|| url.to_string())
}

/// Display-only name for a media item before thrennix-media reports its output.
fn media_display_name(url: &str) -> String {
    let mut n = guess_name(url);
    for ext in [".m3u8", ".mpd"] {
        if n.to_ascii_lowercase().ends_with(ext) {
            n.truncate(n.len() - ext.len());
        }
    }
    if !n.to_ascii_lowercase().ends_with(".mp4") {
        n.push_str(".mp4");
    }
    n
}

pub struct Item {
    pub rec: Record,
    job: Option<Arc<dyn Job>>,
    generation: u64,
    /// Shared strings for the UI, rebuilt only when the record changes (no per-frame copies).
    view_name: Arc<str>,
    view_url: Arc<str>,
    view_host: Arc<str>,
    category: Category,
}

impl Item {
    fn new(rec: Record) -> Self {
        let mut i = Item {
            rec,
            job: None,
            generation: 0,
            view_name: Arc::from(""),
            view_url: Arc::from(""),
            view_host: Arc::from(""),
            category: Category::Other,
        };
        i.refresh_view();
        i
    }

    fn refresh_view(&mut self) {
        if *self.view_name != *self.rec.file_name {
            self.view_name = Arc::from(self.rec.file_name.as_str());
            self.category = classify(self.rec.kind, &self.rec.file_name);
        }
        if *self.view_url != *self.rec.url {
            self.view_url = Arc::from(self.rec.url.as_str());
            let host = url::Url::parse(&self.rec.url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            self.view_host = Arc::from(host.as_str());
        }
    }
}

/// Immutable per-frame view of one download for the UI.
pub struct RowView {
    pub id: u64,
    pub name: Arc<str>,
    pub url: Arc<str>,
    pub host: Arc<str>,
    pub category: Category,
    pub resumable: Option<bool>,
    pub last_try: Option<i64>,
    pub open_folder_when_done: bool,
    pub open_file_when_done: bool,
    pub kind: Kind,
    pub status: Status,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed_bps: f64,
    pub eta_secs: Option<f64>,
    pub connections: usize,
    pub segments: Vec<SegmentInfo>,
    pub path: Option<PathBuf>,
    pub dest_dir: PathBuf,
    pub running: bool,
    pub muxing: bool,
    pub remote_changed: bool,
    /// "Remove + delete" can find the partial files (yt-dlp: needs the output name).
    pub parts_identifiable: bool,
}

// ---------------------------------------------------------------- shared state

pub struct Shared {
    pub rt: Handle,
    backend: Arc<dyn Backend>,
    items: Mutex<Vec<Item>>,
    settings: Mutex<Settings>,
    pending_ask: Mutex<Vec<NewDownload>>,
    /// Row to scroll to / flash (duplicate download from the browser).
    highlight: Mutex<Option<(u64, Instant)>>,
    /// Row whose "file changed on server" prompt should be shown.
    restart_prompt: Mutex<Option<u64>>,
    open_details: Mutex<Vec<u64>>,
    pub ctx: OnceLock<eframe::egui::Context>,
    next_id: AtomicU64,
    save_lock: Mutex<()>,
    persist: bool,
}

impl Shared {
    pub fn new(rt: Handle, backend: Arc<dyn Backend>, settings: Settings, persist: bool) -> Self {
        let mut records: Vec<Record> = if persist {
            load_json("downloads.json").unwrap_or_default()
        } else {
            Vec::new()
        };
        for r in &mut records {
            if matches!(r.status, Status::Probing | Status::Downloading) {
                r.status = Status::Paused;
            }
        }
        let next_id = records.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        Self {
            rt,
            backend,
            items: Mutex::new(records.into_iter().map(Item::new).collect()),
            settings: Mutex::new(settings),
            pending_ask: Mutex::new(Vec::new()),
            highlight: Mutex::new(None),
            restart_prompt: Mutex::new(None),
            open_details: Mutex::new(Vec::new()),
            ctx: OnceLock::new(),
            next_id: AtomicU64::new(next_id),
            save_lock: Mutex::new(()),
            persist,
        }
    }

    pub fn probe_yt(&self, url: String) -> crate::backend::ProbeFuture {
        self.backend.probe_yt(url)
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    pub fn apply_settings(&self, mut s: Settings) {
        s.normalize();
        let old = std::mem::replace(&mut *lock(&self.settings), s.clone());
        if old == s {
            return;
        }
        if old.max_connections != s.max_connections
            || old.initial_connections != s.initial_connections
        {
            // Building an engine creates an HTTP client: keep it off the UI thread.
            let backend = self.backend.clone();
            let config = s.engine_config();
            self.rt.spawn_blocking(move || backend.reconfigure(config));
        }
        if self.persist {
            let _ = save_json("settings.json", &s);
        }
    }

    pub fn take_pending_ask(&self) -> Vec<NewDownload> {
        std::mem::take(&mut *lock(&self.pending_ask))
    }

    pub fn has_pending_ask(&self) -> bool {
        !lock(&self.pending_ask).is_empty()
    }

    pub fn take_highlight(&self) -> Option<(u64, Instant)> {
        *lock(&self.highlight)
    }

    pub fn take_restart_prompt(&self) -> Option<u64> {
        lock(&self.restart_prompt).take()
    }

    pub fn save_list(&self) {
        if !self.persist {
            return;
        }
        // Hold the save lock across snapshot + write so an older snapshot never wins.
        let _g = lock(&self.save_lock);
        let records: Vec<Record> = lock(&self.items).iter().map(|i| i.rec.clone()).collect();
        let _ = save_json("downloads.json", &records);
    }

    fn repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    /// Bring the window to the front (once; Windows may only flash the taskbar button).
    pub fn focus(&self) {
        use eframe::egui::{UserAttentionType, ViewportCommand};
        // Hidden in the tray: eframe may not run frames, so un-hide with Win32 first.
        crate::tray::show_window();
        if let Some(ctx) = self.ctx.get() {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(
                UserAttentionType::Informational,
            ));
            ctx.request_repaint();
        }
    }

    fn highlight(&self, id: u64) {
        *lock(&self.highlight) = Some((id, Instant::now()));
        self.focus();
    }

    fn new_record(&self, nd: NewDownload, dest_dir: PathBuf, status: Status) -> Record {
        let file_name = match (&nd.file_name, nd.kind) {
            (Some(n), _) => n.clone(),
            (None, Kind::File) => guess_name(&nd.url),
            (None, Kind::Ytdlp) => nd.url.clone(),
            (None, _) => media_display_name(&nd.url),
        };
        Record {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            url: nd.url,
            kind: nd.kind,
            format: nd.format,
            file_name,
            requested_name: nd.file_name,
            dest_dir,
            path: None,
            state_file: None,
            work_dir: None,
            status,
            remote_changed: false,
            total: None,
            downloaded: 0,
            cookies: nd.cookies,
            referer: nd.referer,
            user_agent: nd.user_agent,
            headers: nd.headers,
            last_try: Some(now_secs()),
            open_folder_when_done: false,
            open_file_when_done: false,
            resumable: None,
        }
    }

    /// Add and start a download. `dest` overrides the download folder (already asked).
    pub fn add(
        self: &Arc<Self>,
        nd: NewDownload,
        dest: Option<PathBuf>,
        from_browser: bool,
    ) -> Result<(), String> {
        validate_url(&nd.url)?;
        let settings = self.settings();
        let ask = dest.is_none() && settings.ask_where && from_browser;
        let target_dir = dest
            .clone()
            .unwrap_or_else(|| settings.download_dir.clone());
        let target_path = nd.file_name.as_ref().map(|n| target_dir.join(n));

        {
            let mut items = lock(&self.items);
            // Already downloading the same thing: keep that job, just show it.
            let live = items.iter().find(|i| {
                let r = &i.rec;
                i.job.is_some()
                    && ((r.url == nd.url
                        && r.kind == nd.kind
                        && r.format == nd.format
                        && (ask || r.dest_dir == target_dir))
                        || (target_path.is_some() && r.path == target_path))
            });
            if let Some(id) = live.map(|i| i.rec.id) {
                drop(items);
                self.highlight(id);
                return Ok(());
            }
            // Same link for a paused/failed entry: resume it (with fresh cookies).
            let stalled = items.iter_mut().find(|i| {
                let r = &i.rec;
                i.job.is_none()
                    && matches!(r.status, Status::Paused | Status::Failed(_))
                    && r.url == nd.url
                    && r.kind == nd.kind
                    && r.format == nd.format
                    && (ask || r.dest_dir == target_dir)
            });
            if let Some(item) = stalled {
                let r = &mut item.rec;
                r.cookies = nd.cookies.clone().or(r.cookies.take());
                r.referer = nd.referer.clone().or(r.referer.take());
                r.user_agent = nd.user_agent.clone().or(r.user_agent.take());
                if !nd.headers.is_empty() {
                    r.headers = nd.headers.clone();
                }
                let id = r.id;
                self.start_locked(item);
                drop(items);
                self.save_list();
                self.highlight(id);
                return Ok(());
            }
        }

        if ask {
            let mut pending = lock(&self.pending_ask);
            if pending.len() < MAX_PENDING_ASK {
                pending.push(nd);
                drop(pending);
                self.focus();
                return Ok(());
            }
            // Too many waiting dialogs: fall through to the default folder.
        }

        std::fs::create_dir_all(&target_dir)
            .map_err(|e| format!("cannot create {}: {e}", target_dir.display()))?;
        let rec = self.new_record(nd, target_dir, Status::Probing);
        {
            let mut items = lock(&self.items);
            let id = rec.id;
            items.insert(0, Item::new(rec));
            self.start_locked(&mut items[0]);
            if from_browser && settings.show_progress_window {
                lock(&self.open_details).push(id);
            }
        }
        self.save_list();
        if from_browser {
            self.focus();
        }
        Ok(())
    }

    /// `--demo`: fill the list with a varied set of fake downloads.
    pub fn seed_demo(self: &Arc<Self>, open_details: bool) {
        let dir = self.settings().download_dir;
        let mb = 1024 * 1024;
        let now = now_secs();
        // (url, name, kind, status, total MB, done %, minutes ago)
        let stopped: [(&str, &str, Kind, Status, u64, u64, i64); 9] = [
            (
                "https://downloads.example.org/arch/archlinux-2026.10.01-x86_64.iso",
                "archlinux-2026.10.01-x86_64.iso",
                Kind::File,
                Status::Paused,
                1_180,
                41,
                35,
            ),
            (
                "https://cdn.example.com/music/Album%20-%20Live.flac",
                "Album - Live.flac",
                Kind::File,
                Status::Paused,
                412,
                67,
                90,
            ),
            (
                "https://update.code.example.com/VSCodeUserSetup-x64.exe",
                "VSCodeUserSetup-x64.exe",
                Kind::File,
                Status::Completed,
                98,
                100,
                140,
            ),
            (
                "https://files.example.net/photos-2025.7z",
                "photos-2025.7z",
                Kind::File,
                Status::Completed,
                2_340,
                100,
                300,
            ),
            (
                "https://cdn.example.com/podcast/episode-112.mp3",
                "episode-112.mp3",
                Kind::File,
                Status::Completed,
                54,
                100,
                1_440,
            ),
            (
                "https://docs.example.com/slides/Quarterly%20review.pptx",
                "Quarterly review.pptx",
                Kind::File,
                Status::Completed,
                18,
                100,
                2_000,
            ),
            (
                "https://stream.example.com/trailer/master.m3u8",
                "Trailer 4K.mp4",
                Kind::Hls,
                Status::Completed,
                690,
                100,
                2_900,
            ),
            (
                "https://data.example.org/dumps/dataset-full.tar.gz",
                "dataset-full.tar.gz",
                Kind::File,
                Status::Failed("server returned status 403".into()),
                8_200,
                12,
                50,
            ),
            (
                "https://cdn.example.com/drivers/driver-pack.zip",
                "driver-pack.zip",
                Kind::File,
                Status::Failed("connection reset by peer (8 retries)".into()),
                760,
                58,
                20,
            ),
        ];
        for (url, name, kind, status, total, pct, ago) in stopped {
            let nd = NewDownload {
                url: url.into(),
                file_name: Some(name.into()),
                kind,
                ..Default::default()
            };
            let mut rec = self.new_record(nd, dir.clone(), status);
            rec.total = Some(total * mb);
            rec.downloaded = total * mb * pct / 100;
            rec.path = Some(dir.join(name));
            rec.resumable = Some(true);
            rec.last_try = Some(now - ago * 60);
            lock(&self.items).push(Item::new(rec));
        }
        let active: [(&str, Option<&str>, Kind); 6] = [
            ("https://releases.example.com/ubuntu-26.04-desktop-amd64.iso?size=5800&start=38&speed=6", None, Kind::File),
            ("https://nodejs.example.org/dist/node-v24.1.0-win-x64.zip?size=31&start=72&speed=1", None, Kind::File),
            ("https://www.youtube.com/watch?v=aqz-KE-bpKQ&merge", Some("Big Buck Bunny 60fps 4K"), Kind::Ytdlp),
            ("https://reports.example.com/annual-report-2026.pdf?size=64&start=15&speed=1", None, Kind::File),
            ("https://dl.example.com/tools/SetupTool.exe?size=240&start=5&speed=3", None, Kind::File),
            ("https://www.youtube.com/watch?v=dQw4w9WgXcQ&size=180&start=20&speed=2", Some("Never Gonna Give You Up"), Kind::Ytdlp),
        ];
        for (url, name, kind) in active {
            let nd = NewDownload {
                url: url.into(),
                file_name: name.map(String::from),
                kind,
                format: (kind == Kind::Ytdlp).then(|| Kind::YTDLP_BEST.to_string()),
                ..Default::default()
            };
            let _ = self.add(nd, Some(dir.clone()), false);
        }
        if open_details {
            // The newest active file download (the ISO) gets its progress window.
            let id = lock(&self.items)
                .iter()
                .find(|i| i.rec.url.contains("ubuntu"))
                .map(|i| i.rec.id);
            lock(&self.open_details).extend(id);
        }
    }

    /// Downloads whose progress window should be opened (browser downloads).
    pub fn take_open_details(&self) -> Vec<u64> {
        std::mem::take(&mut *lock(&self.open_details))
    }

    pub fn pause_all(&self) {
        for item in lock(&self.items).iter() {
            if let Some(job) = &item.job {
                job.pause();
            }
        }
    }

    /// Drop finished entries from the list (files are kept).
    pub fn remove_completed(&self) {
        lock(&self.items).retain(|i| i.rec.status != Status::Completed || i.job.is_some());
        self.save_list();
        self.repaint();
    }

    pub fn set_item_options(&self, id: u64, open_folder: bool, open_file: bool) {
        if let Some(item) = lock(&self.items).iter_mut().find(|i| i.rec.id == id) {
            item.rec.open_folder_when_done = open_folder;
            item.rec.open_file_when_done = open_file;
        }
        self.save_list();
    }

    /// Add without starting (e.g. the folder dialog was cancelled): shows up as Paused.
    pub fn add_paused(&self, nd: NewDownload, dest: Option<PathBuf>) {
        let dir = dest.unwrap_or_else(|| self.settings().download_dir);
        let rec = self.new_record(nd, dir, Status::Paused);
        lock(&self.items).insert(0, Item::new(rec));
        self.save_list();
        self.repaint();
    }

    /// Start the item's request. Caller holds the items lock, so two clicks can never
    /// start two jobs for one item.
    fn start_locked(self: &Arc<Self>, item: &mut Item) {
        if item.job.is_some() {
            return;
        }
        let _ = std::fs::create_dir_all(&item.rec.dest_dir);
        let job = self
            .backend
            .start(item.rec.kind, item.rec.request(), item.rec.format.clone());
        self.attach_locked(item, job);
    }

    fn attach_locked(self: &Arc<Self>, item: &mut Item, job: Arc<dyn Job>) {
        item.job = Some(job.clone());
        item.generation += 1;
        item.rec.status = Status::Probing;
        item.rec.remote_changed = false;
        item.rec.last_try = Some(now_secs());
        let (id, generation) = (item.rec.id, item.generation);
        let me = self.clone();
        // The job has ended exactly when wait() returns; keep the handle until then.
        self.rt.spawn(async move {
            let result = job.wait().await;
            me.finish(id, generation, result);
        });
        self.repaint();
    }

    fn finish(&self, id: u64, generation: u64, result: thrennix_core::Result<PathBuf>) {
        let completed = {
            let mut items = lock(&self.items);
            let Some(item) = items.iter_mut().find(|i| i.rec.id == id) else {
                return;
            };
            if item.generation != generation {
                return;
            }
            let Some(job) = item.job.take() else { return };
            let p = job.progress();
            let rec = &mut item.rec;
            rec.apply_live(&p, job.as_ref());
            rec.status = match &result {
                Ok(path) => {
                    if let Some(n) = path.file_name() {
                        rec.file_name = n.to_string_lossy().into_owned();
                    }
                    rec.path = Some(path.clone());
                    if let Some(t) = rec.total {
                        rec.downloaded = t;
                    }
                    rec.drop_secrets();
                    Status::Completed
                }
                Err(Error::Paused) => Status::Paused,
                Err(Error::RemoteChanged) => {
                    rec.remote_changed = true;
                    *lock(&self.restart_prompt) = Some(id);
                    Status::Failed("file changed on server".into())
                }
                Err(e) => Status::Failed(e.to_string()),
            };
            (rec.status == Status::Completed).then(|| {
                (
                    rec.file_name.clone(),
                    rec.path.clone(),
                    rec.dest_dir.clone(),
                    rec.open_folder_when_done,
                    rec.open_file_when_done,
                )
            })
        };
        self.save_list();
        if let Some((name, path, dir, open_folder, open_file)) = completed {
            let notify = self.persist;
            self.rt.spawn_blocking(move || {
                if open_file {
                    if let Some(p) = &path {
                        let _ = opener::open(p);
                    }
                } else if open_folder {
                    let _ = reveal(path.as_deref(), &dir);
                }
                if notify {
                    let _ = notify_rust::Notification::new()
                        .appname("Thrennix")
                        .summary("Download complete")
                        .body(&name)
                        .show();
                }
            });
        }
        self.repaint();
    }

    pub fn pause(&self, id: u64) {
        if let Some(item) = lock(&self.items).iter().find(|i| i.rec.id == id) {
            if let Some(job) = &item.job {
                job.pause();
            }
        }
    }

    pub fn resume(self: &Arc<Self>, id: u64) {
        let mut items = lock(&self.items);
        if let Some(item) = items.iter_mut().find(|i| i.rec.id == id) {
            if item.rec.status != Status::Completed {
                self.start_locked(item);
            }
        }
    }

    /// Continue a failed/expired download with a fresh link.
    pub fn retry_with_url(self: &Arc<Self>, id: u64, url: &str) -> Result<(), String> {
        let url = validate_url(url)?.to_string();
        {
            let mut items = lock(&self.items);
            let item = items
                .iter_mut()
                .find(|i| i.rec.id == id)
                .ok_or("download not found")?;
            if item.job.is_some() {
                return Err("download is already running".into());
            }
            item.rec.url = url.clone();
            let state = item.rec.state_file.clone().filter(|s| s.exists());
            match state {
                Some(sf) if item.rec.kind == Kind::File => {
                    let job = self.backend.resume_from_state(sf, Some(url));
                    self.attach_locked(item, job);
                }
                _ => self.start_locked(item),
            }
        }
        self.save_list();
        Ok(())
    }

    /// The remote file changed: throw away partial data and start over (current URL).
    pub fn restart_from_zero(self: &Arc<Self>, id: u64) {
        {
            let mut items = lock(&self.items);
            let Some(item) = items.iter_mut().find(|i| i.rec.id == id) else {
                return;
            };
            if item.job.is_some() {
                return;
            }
            let (files, dir) = item.rec.files_to_delete();
            files.iter().for_each(|f| remove_quietly(f));
            if let Some(d) = dir {
                let _ = std::fs::remove_dir_all(d);
            }
            item.rec.state_file = None;
            item.rec.work_dir = None;
            item.rec.downloaded = 0;
            self.start_locked(item);
        }
        self.save_list();
    }

    pub fn remove(&self, id: u64, delete_files: bool) {
        let removed = {
            let mut items = lock(&self.items);
            items
                .iter()
                .position(|i| i.rec.id == id)
                .map(|pos| items.remove(pos))
        };
        let Some(item) = removed else { return };
        self.save_list();
        self.repaint();
        let job = item.job.clone();
        if let Some(job) = &job {
            job.pause();
        }
        if !delete_files {
            return;
        }
        self.rt.spawn(async move {
            let mut rec = item.rec;
            if let Some(job) = job {
                // Let the job flush and close its files before deleting them.
                let _ = tokio::time::timeout(REMOVE_WAIT, job.wait()).await;
                if rec.kind.is_media() {
                    rec.work_dir = job.work_dir().or(rec.work_dir);
                }
                rec.status = Status::Paused; // it did not complete
            }
            let (files, dir) = rec.files_to_delete();
            tokio::task::spawn_blocking(move || {
                files.iter().for_each(|f| remove_quietly(f));
                if let Some(d) = dir {
                    let _ = std::fs::remove_dir_all(d);
                }
            });
        });
    }

    /// Build the per-frame view, refreshing records from live progress.
    pub fn snapshot(&self) -> Vec<RowView> {
        let mut items = lock(&self.items);
        items
            .iter_mut()
            .map(|item| {
                let live = item.job.as_ref().map(|j| (j.progress(), j.is_muxing()));
                if let (Some((p, _)), Some(job)) = (&live, &item.job) {
                    item.rec.apply_live(p, job.as_ref());
                }
                item.refresh_view();
                let r = &item.rec;
                let muxing = live.as_ref().is_some_and(|l| l.1);
                let p = live.map(|l| l.0);
                RowView {
                    id: r.id,
                    name: item.view_name.clone(),
                    url: item.view_url.clone(),
                    host: item.view_host.clone(),
                    category: item.category,
                    resumable: r.resumable,
                    last_try: r.last_try,
                    open_folder_when_done: r.open_folder_when_done,
                    open_file_when_done: r.open_file_when_done,
                    kind: r.kind,
                    status: r.status.clone(),
                    downloaded: r.downloaded,
                    total: r.total,
                    speed_bps: p.as_ref().map_or(0.0, |p| p.speed_bps),
                    eta_secs: p.as_ref().and_then(|p| p.eta_secs()),
                    connections: p.as_ref().map_or(0, |p| p.connections),
                    segments: p.map(|p| p.segments).unwrap_or_default(),
                    path: r.path.clone(),
                    dest_dir: r.dest_dir.clone(),
                    running: item.job.is_some(),
                    muxing,
                    remote_changed: r.remote_changed,
                    parts_identifiable: r.status == Status::Completed
                        || partial_files_identifiable(r.kind, r.path.as_deref()),
                }
            })
            .collect()
    }

    /// Pause everything so state files are flushed, keep pending asks, persist the list.
    pub fn shutdown(&self) {
        for nd in self.take_pending_ask() {
            self.add_paused(nd, None);
        }
        let jobs: Vec<Arc<dyn Job>> = lock(&self.items)
            .iter()
            .filter_map(|i| i.job.clone())
            .collect();
        jobs.iter().for_each(|j| j.pause());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        self.rt.block_on(async {
            for j in &jobs {
                let _ = tokio::time::timeout_at(deadline, j.wait()).await;
            }
        });
        for item in lock(&self.items).iter_mut() {
            if let Some(job) = item.job.take() {
                item.rec.apply_live(&job.progress(), job.as_ref());
                item.rec.status = Status::Paused;
            }
        }
        self.save_list();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yt_always_best_defaults_off() {
        assert!(!Settings::default().yt_always_best);
        // settings.json written before the option existed.
        let old: Settings = serde_json::from_str(r#"{"max_connections":8}"#).unwrap();
        assert!(!old.yt_always_best);
        let on: Settings = serde_json::from_str(r#"{"yt_always_best":true}"#).unwrap();
        assert!(on.yt_always_best);
    }

    fn rec(kind: Kind) -> Record {
        Record {
            id: 3,
            url: "https://a.b/f".into(),
            kind,
            format: None,
            file_name: "f.mp4".into(),
            requested_name: None,
            dest_dir: PathBuf::from(r"C:\d"),
            path: None,
            state_file: None,
            work_dir: None,
            status: Status::Failed("x".into()),
            remote_changed: false,
            total: Some(5),
            downloaded: 1,
            cookies: Some("a=1".into()),
            referer: None,
            user_agent: None,
            headers: vec![("Authorization".into(), "Bearer secret".into())],
            last_try: None,
            open_folder_when_done: false,
            open_file_when_done: false,
            resumable: None,
        }
    }

    fn progress(status: Status, path: &str, name: &str) -> Progress {
        Progress {
            status,
            file_name: name.into(),
            path: PathBuf::from(path),
            downloaded: 10,
            total: Some(100),
            speed_bps: 0.0,
            connections: 0,
            resumable: true,
            segments: vec![],
        }
    }

    struct NoJob;
    impl Job for NoJob {
        fn progress(&self) -> Progress {
            unreachable!()
        }
        fn pause(&self) {}
        fn wait(&self) -> crate::backend::WaitFuture<'_> {
            unreachable!()
        }
    }

    #[test]
    fn names() {
        assert_eq!(guess_name("https://a.b/x/file.zip?x=1"), "file.zip");
        assert_eq!(
            media_display_name("https://a.b/x/master.m3u8"),
            "master.mp4"
        );
        assert_eq!(
            with_suffix(Path::new(r"C:\d\f.zip"), ".thrennix"),
            PathBuf::from(r"C:\d\f.zip.thrennix")
        );
    }

    #[test]
    fn record_roundtrip_encrypts_secrets() {
        let r = rec(Kind::Hls);
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("a=1"), "{s}");
        assert!(!s.contains("Bearer secret"), "{s}");
        assert!(s.contains("Authorization"));
        let back: Record = serde_json::from_str(&s).unwrap();
        assert_eq!(back.status, Status::Failed("x".into()));
        assert_eq!(back.kind, Kind::Hls);
        assert_eq!(back.cookies.as_deref(), Some("a=1"));
        assert_eq!(back.headers, r.headers);
        // Old plaintext files still load.
        let old = s
            .replace(&serde_json::to_string(&back.cookies).unwrap(), "x")
            .replace(r#""cookies":"#, r#""cookies":"legacy=1","_old":"#);
        let back: Record = serde_json::from_str(&old).unwrap();
        assert_eq!(back.cookies.as_deref(), Some("legacy=1"));
    }

    #[test]
    fn probing_path_is_ignored() {
        let mut r = rec(Kind::File);
        // Probing publishes dest_dir (or the state file, or nothing): never a file path.
        r.apply_live(&progress(Status::Probing, r"C:\d", "Downloads"), &NoJob);
        assert_eq!(r.path, None);
        assert_eq!(r.file_name, "f.mp4");
        r.apply_live(&progress(Status::Downloading, r"C:\d", ""), &NoJob);
        assert_eq!(r.path, None);
        r.apply_live(&progress(Status::Downloading, "", ""), &NoJob);
        assert_eq!(r.path, None);
        r.apply_live(
            &progress(Status::Downloading, r"C:\d\f.zip.thrennix", ""),
            &NoJob,
        );
        assert_eq!(r.path, None);
        r.apply_live(
            &progress(Status::Downloading, r"C:\d\f.zip", "f.zip"),
            &NoJob,
        );
        assert_eq!(r.path.as_deref(), Some(Path::new(r"C:\d\f.zip")));
        assert_eq!(
            r.state_file.as_deref(),
            Some(Path::new(r"C:\d\f.zip.thrennix"))
        );
        assert_eq!(r.request().file_name.as_deref(), Some("f.zip"));
        // A transient Failed during a live job is shown as still downloading.
        r.apply_live(
            &progress(Status::Failed("retrying".into()), r"C:\d\f.zip", "f.zip"),
            &NoJob,
        );
        assert_eq!(r.status, Status::Downloading);
    }

    #[test]
    fn media_request_keeps_requested_name() {
        let mut r = rec(Kind::Dash);
        r.path = Some(PathBuf::from(r"C:\d\video.mp4"));
        assert_eq!(r.request().file_name, None);
        r.requested_name = Some("clip".into());
        assert_eq!(r.request().file_name.as_deref(), Some("clip"));
    }

    #[test]
    fn delete_targets() {
        let mut r = rec(Kind::File);
        r.path = Some(PathBuf::from(r"C:\d\f.zip"));
        r.state_file = Some(PathBuf::from(r"C:\d\f.zip.thrennix"));
        let (files, dir) = r.files_to_delete();
        assert!(files.contains(&PathBuf::from(r"C:\d\f.zip.part")));
        assert!(!files.contains(&PathBuf::from(r"C:\d\f.zip")));
        assert!(dir.is_none());
        r.status = Status::Completed;
        assert_eq!(r.files_to_delete().0, vec![PathBuf::from(r"C:\d\f.zip")]);

        let mut m = rec(Kind::Hls);
        m.path = Some(PathBuf::from(r"C:\d\v.mp4"));
        m.work_dir = Some(PathBuf::from(r"C:\d\.v.thrennix-media"));
        assert_eq!(m.files_to_delete().1, m.work_dir);
        m.status = Status::Completed;
        assert_eq!(
            m.files_to_delete(),
            (vec![PathBuf::from(r"C:\d\v.mp4")], None)
        );
    }

    #[test]
    fn ytdlp_partial_files() {
        let dir = std::env::temp_dir().join(format!("thrennix-app-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let names = [
            "Clip [abc123].f137.mp4.part",
            "Clip [abc123].f140.m4a.part",
            "Clip [abc123].mp4.ytdl",
            "Clip [abc123].f137.mp4.part-Frag3",
            "Clip [other].f137.mp4.part",
            "Clip.mp4.part",
            "Clip [abc123].mp4",
        ];
        for n in names {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let mut found: Vec<String> = ytdlp_leftovers(&dir.join("Clip [abc123].mp4"))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(
            found,
            names[..4]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .tap_sort()
        );
        // Without a [video id] nothing is matched.
        assert!(ytdlp_leftovers(&dir.join("Clip.mp4")).is_empty());
        assert!(partial_files_identifiable(
            Kind::Ytdlp,
            Some(&dir.join("Clip [abc123].mp4"))
        ));
        assert!(!partial_files_identifiable(
            Kind::Ytdlp,
            Some(&dir.join("Clip.mp4"))
        ));
        assert!(!partial_files_identifiable(Kind::Ytdlp, None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    trait TapSort {
        fn tap_sort(self) -> Self;
    }
    impl TapSort for Vec<String> {
        fn tap_sort(mut self) -> Self {
            self.sort();
            self
        }
    }

    #[test]
    fn new_download_from_msg() {
        let msg: DownloadMsg = serde_json::from_str(
            r#"{"url":" https://a.b/c ","filename":"x/y.zip","cookies":"","headers":{"H":"v"}}"#,
        )
        .unwrap();
        let nd = NewDownload::from(msg);
        assert_eq!(nd.url, "https://a.b/c");
        assert_eq!(nd.file_name.as_deref(), Some("x_y.zip"));
        assert_eq!(nd.cookies, None);
        assert_eq!(nd.headers, vec![("H".to_string(), "v".to_string())]);
        assert_eq!(nd.kind, Kind::File);
    }
}
