//! yt-dlp backend: YouTube and every other site yt-dlp supports.
//!
//! Extraction is entirely yt-dlp's job; we only run the executable, offer an IDM-style
//! quality list (`probe`) and translate its progress output into `Progress` (`start`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::mux::{self, Container};
use crate::task::{cancelled, Shared, Speedo};
use crate::util::{find_ffmpeg, find_tool, rename_with_retry, sanitize, write_zone_identifier};
use crate::{Error, MediaHandle, Result, Status};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YtFormat {
    /// yt-dlp format selector to pass to `start`.
    pub id: String,
    /// Human label, e.g. `MP4 · 1080p60 HD`.
    pub label: String,
    pub height: Option<u32>,
    /// Approximate size in bytes (video + audio), if yt-dlp knows it.
    pub filesize: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YtInfo {
    pub title: String,
    pub formats: Vec<YtFormat>,
}

const AUDIO_ID: &str = "ba[ext=m4a]/ba";
/// A cold yt-dlp start on a slow tablet (unpacking itself, solving YouTube's JS challenge)
/// can take most of a minute.
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Locate yt-dlp: `THRENNIX_YTDLP`, PATH, next to the current exe, `WinGet\Links`,
/// `WinGet\Packages\yt-dlp*\yt-dlp.exe`.
pub fn find_ytdlp() -> Option<PathBuf> {
    find_tool("THRENNIX_YTDLP", "yt-dlp", "yt-dlp")
}

fn not_found() -> Error {
    Error::Other("yt-dlp not found — install with: winget install yt-dlp.yt-dlp".into())
}

fn check_url(url: &str) -> Result<()> {
    match url::Url::parse(url.trim()) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(()),
        _ => Err(Error::Other(format!("not an http(s) URL: {url}"))),
    }
}

fn command(exe: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Make the (Python) child print UTF-8 regardless of the console code page.
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        // Line-by-line progress instead of block-buffered pipe output.
        .env("PYTHONUNBUFFERED", "1");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let _ = CREATE_NO_WINDOW;
    cmd
}

fn last_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(str::to_string)
}

/// Run `yt-dlp -J` and build the quality list.
pub async fn probe(url: &str) -> Result<YtInfo> {
    check_url(url)?;
    let exe = find_ytdlp().ok_or_else(not_found)?;
    let mut cmd = command(&exe);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--", url.trim()]);
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| Error::Other("yt-dlp timed out reading the page".into()))??;
    if !out.status.success() {
        return Err(Error::Other(
            last_line(&out.stderr).unwrap_or_else(|| format!("yt-dlp failed ({})", out.status)),
        ));
    }
    let json: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| Error::Other(format!("yt-dlp printed invalid JSON: {e}")))?;
    build_info(&json)
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite() && *x >= 0.0)
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn size_of(f: &Value) -> Option<u64> {
    num(f, "filesize")
        .or_else(|| num(f, "filesize_approx"))
        .map(|s| s as u64)
}

fn is_audio_only(f: &Value) -> bool {
    match (text(f, "vcodec"), text(f, "acodec")) {
        (Some("none"), Some(a)) => a != "none",
        (Some("none"), None) => text(f, "resolution") == Some("audio only"),
        (None, _) => text(f, "resolution") == Some("audio only"),
        _ => false,
    }
}

fn height_of(f: &Value) -> Option<u32> {
    if text(f, "vcodec") == Some("none") {
        return None;
    }
    num(f, "height").map(|h| h as u32).filter(|h| *h > 0)
}

/// Bitrate used to rank formats of the same kind.
fn rate(f: &Value) -> f64 {
    num(f, "tbr")
        .or_else(|| num(f, "vbr"))
        .or_else(|| num(f, "abr"))
        .unwrap_or(0.0)
}

/// `MP4 · 1080p60 HD`
pub(crate) fn label(height: u32, fps: Option<f64>) -> String {
    let fps = match fps {
        Some(f) if f > 30.5 => format!("{}", f.round() as u32),
        _ => String::new(),
    };
    let tag = if height >= 4320 {
        " 8K"
    } else if height >= 2160 {
        " 4K"
    } else if height >= 720 {
        " HD"
    } else {
        ""
    };
    format!("MP4 · {height}p{fps}{tag}")
}

/// Build the quality list from yt-dlp's `-J` output.
pub(crate) fn build_info(json: &Value) -> Result<YtInfo> {
    if text(json, "_type").is_some_and(|t| t == "playlist") {
        return Err(Error::Other("playlists are not supported".into()));
    }
    let title = text(json, "title")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("video")
        .to_string();
    let formats: &[Value] = json
        .get("formats")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    // Prefer formats with a known size, then the higher bitrate. YouTube lists HLS
    // (`m3u8_native`) variants with a higher `tbr` but no size; yt-dlp downloads the sized
    // https format instead, so ranking by bitrate alone would hide the size estimate.
    let best_by = |pred: &dyn Fn(&Value) -> bool| {
        formats.iter().filter(|f| pred(f)).max_by(|a, b| {
            size_of(a)
                .is_some()
                .cmp(&size_of(b).is_some())
                .then(rate(a).total_cmp(&rate(b)))
        })
    };
    let best_audio = best_by(&is_audio_only);
    let audio_size = best_audio.map(size_of);

    let mut heights: Vec<u32> = formats.iter().filter_map(height_of).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();

    let mut out = Vec::new();
    for h in heights {
        let at_h = |f: &Value| height_of(f) == Some(h);
        let best = best_by(&at_h);
        let fps = formats
            .iter()
            .filter(|f| at_h(f))
            .filter_map(|f| num(f, "fps"))
            .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))));
        let video_size = best.and_then(size_of);
        // A format that already carries audio does not need the separate audio stream.
        let has_audio = best.is_some_and(|f| text(f, "acodec").is_some_and(|a| a != "none"));
        let filesize = match (video_size, audio_size) {
            (Some(v), _) if has_audio || best_audio.is_none() => Some(v),
            (Some(v), Some(Some(a))) => v.checked_add(a),
            _ => None,
        };
        out.push(YtFormat {
            // Exact height first so 4K/1440p never fall back to a lower H.264 rendition.
            // At that height prefer H.264 + AAC (plays everywhere), then any codec (YouTube
            // only offers AV1/VP9 above 1080p); a lower height is the last resort.
            id: format!(
                "bv*[height={h}][vcodec^=avc1]+ba[ext=m4a]/bv*[height={h}]+ba[ext=m4a]/bv*[height={h}]+ba/bv*[height<={h}]+ba/b[height<={h}]"
            ),
            label: label(h, fps),
            height: Some(h),
            filesize,
        });
    }
    if out.is_empty() && !formats.iter().all(is_audio_only) || formats.is_empty() {
        // Sites without height information (or a single direct format).
        out.push(YtFormat {
            id: "bv*+ba/b".into(),
            label: "MP4 · Best".into(),
            height: None,
            filesize: if formats.len() == 1 {
                size_of(&formats[0])
            } else {
                None
            },
        });
    }
    if best_audio.is_some() {
        let m4a = best_by(&|f: &Value| is_audio_only(f) && text(f, "ext") == Some("m4a"));
        out.push(YtFormat {
            id: AUDIO_ID.into(),
            label: "Audio · M4A".into(),
            height: None,
            filesize: m4a.or(best_audio).and_then(size_of),
        });
    }
    Ok(YtInfo {
        title,
        formats: out,
    })
}

/// One `THRENNIX ...` progress line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct YtProgress {
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    pub estimate: Option<u64>,
    pub speed: Option<f64>,
    pub fragment_index: Option<u64>,
    pub fragment_count: Option<u64>,
    pub filename: String,
}

const PROGRESS_TEMPLATE: &str = "download:THRENNIX %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.fragment_index)s %(progress.fragment_count)s %(progress.filename)s";
const POSTPROCESS_TEMPLATE: &str =
    "postprocess:THRENNIXPP %(progress.status)s %(progress.postprocessor)s";
const FILE_PRINT: &str = "after_move:THRENNIXFILE %(filepath)s";
/// Printed after format selection: the two formats of a merge and the merged file name.
const MERGE_PRINT: &str = "video:THRENNIXMERGE %(requested_formats.0.format_id)s %(requested_formats.0.ext)s %(requested_formats.1.format_id)s %(requested_formats.1.ext)s %(filename)s";

/// The video and audio files yt-dlp downloads before merging them into `merged`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeParts {
    pub video: PathBuf,
    pub audio: PathBuf,
    pub merged: PathBuf,
}

/// Parse a `THRENNIXMERGE` line. yt-dlp names the parts `<merged stem>.f<format id>.<ext>`.
pub(crate) fn parse_merge(line: &str) -> Option<MergeParts> {
    let rest = line.trim_end().strip_prefix("THRENNIXMERGE ")?;
    let mut it = rest.splitn(5, ' ');
    let mut field = || it.next().filter(|f| !f.is_empty() && *f != "NA");
    let (vid, vext, aid, aext) = (field()?, field()?, field()?, field()?);
    let merged = PathBuf::from(it.next()?.trim());
    let stem = merged.with_extension("");
    let part = |id: &str, ext: &str| {
        let mut p = stem.as_os_str().to_owned();
        p.push(format!(".f{id}.{ext}"));
        PathBuf::from(p)
    };
    Some(MergeParts {
        video: part(vid, vext),
        audio: part(aid, aext),
        merged,
    })
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

fn dot_ext(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default()
}

/// yt-dlp downloaded both parts but its merge failed: ffmpeg refused the MP4, or (on slow
/// PCs, with big 4K files) Windows Defender still had the merged file open when yt-dlp
/// renamed it. Merge the parts ourselves, MP4 first, then MKV, which takes any codec.
async fn merge_parts(parts: &MergeParts) -> Result<PathBuf> {
    let ffmpeg = find_ffmpeg().ok_or_else(|| {
        Error::Other("ffmpeg not found — install with: winget install Gyan.FFmpeg".into())
    })?;
    let stem = parts.merged.with_extension("");
    let (ve, ae) = (dot_ext(&parts.video), dot_ext(&parts.audio));
    let mut last = String::new();
    for (container, ext) in [(Container::Mp4, "mp4"), (Container::Mkv, "mkv")] {
        let tmp = with_suffix(&stem, &format!(".thrennix.{ext}"));
        let res = mux::ffmpeg_mux(
            &ffmpeg,
            (&parts.video, &ve),
            Some((&parts.audio, &ae)),
            &tmp,
            container,
        )
        .await;
        match res {
            Ok(()) => {
                let out = with_suffix(&stem, &format!(".{ext}"));
                rename_with_retry(&tmp, &out).await?;
                for p in [&parts.video, &parts.audio, &with_suffix(&stem, ".temp.mp4")] {
                    let _ = tokio::fs::remove_file(p).await;
                }
                return Ok(out);
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                last = e.to_string();
            }
        }
    }
    // ffmpeg's last line names the problem ("No space left on device", ...).
    Err(Error::Other(
        last.lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("ffmpeg failed")
            .to_string(),
    ))
}

pub(crate) fn parse_progress(line: &str) -> Option<YtProgress> {
    let rest = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("THRENNIX ")?;
    let mut parts = rest.splitn(7, ' ');
    let mut field = || -> Option<Option<f64>> {
        let p = parts.next()?;
        Some(p.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0))
    };
    let downloaded = field()?;
    let total = field()?;
    let estimate = field()?;
    let speed = field()?;
    let fragment_index = field()?;
    let fragment_count = field()?;
    let filename = parts.next().unwrap_or("").to_string();
    let u = |x: Option<f64>| x.map(|v| v as u64);
    Some(YtProgress {
        downloaded: u(downloaded),
        total: u(total),
        estimate: u(estimate),
        speed,
        fragment_index: u(fragment_index),
        fragment_count: u(fragment_count),
        filename,
    })
}

/// Accumulates progress over the separate files yt-dlp downloads (video, then audio), so
/// `downloaded` never jumps backwards.
#[derive(Debug, Default)]
pub(crate) struct Accumulator {
    offset: u64,
    file: Option<String>,
    cur_done: u64,
    cur_total: Option<u64>,
}

impl Accumulator {
    /// Returns `(downloaded, total)` across all files so far.
    pub fn update(&mut self, p: &YtProgress) -> (u64, Option<u64>) {
        if self.file.as_deref() != Some(p.filename.as_str()) {
            if self.file.is_some() {
                self.offset = self
                    .offset
                    .saturating_add(self.cur_total.unwrap_or(0).max(self.cur_done));
            }
            self.file = Some(p.filename.clone());
            self.cur_done = 0;
            self.cur_total = None;
        }
        if let Some(d) = p.downloaded {
            // Fragment downloads can report a smaller count briefly; never go backwards.
            self.cur_done = self.cur_done.max(d);
        }
        if let Some(t) = p.total.or(p.estimate) {
            self.cur_total = Some(t);
        }
        let done = self.offset.saturating_add(self.cur_done);
        let total = self
            .cur_total
            .map(|t| self.offset.saturating_add(t.max(self.cur_done)));
        (done, total)
    }
}

/// Postprocessors that rewrite the media (shown as "muxing").
fn is_mux_step(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("THRENNIXPP ") {
        let mut it = rest.split_whitespace();
        let status = it.next().unwrap_or("");
        let pp = it.next().unwrap_or("");
        return status != "finished"
            && ["Merger", "Fixup", "Convert", "ExtractAudio", "Remux"]
                .iter()
                .any(|k| pp.contains(k));
    }
    [
        "[Merger]",
        "[FixupM3u8]",
        "[FixupM4a]",
        "[VideoRemuxer]",
        "[ExtractAudio]",
    ]
    .iter()
    .any(|p| line.starts_with(p))
}

/// Output template: the given title (sanitized, `%` escaped) or yt-dlp's own title.
fn output_template(title: Option<&str>) -> String {
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => {
            let mut t = sanitize(t);
            // Strip an extension the app may have appended; yt-dlp adds the real one.
            for ext in [".mp4", ".m4a", ".webm", ".mkv"] {
                if t.to_ascii_lowercase().ends_with(ext) && t.len() > ext.len() {
                    t.truncate(t.len() - ext.len());
                }
            }
            format!("{} [%(id)s].%(ext)s", t.replace('%', "%%"))
        }
        None => "%(title)s [%(id)s].%(ext)s".into(),
    }
}

pub(crate) fn build_args(
    url: &str,
    dest_dir: &Path,
    format_id: &str,
    title: Option<&str>,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "--no-playlist",
        "--newline",
        "--no-colors",
        "--continue",
        "--progress",
        // The env vars in `command` are not enough: yt-dlp still drops characters outside
        // the console code page (emoji, `｜`) from `--print`, so the file paths we read
        // back would not exist and the merge step fails.
        "--encoding",
        "utf-8",
        "-N",
        "8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(format!("--format={format_id}"));
    if !format_id.trim_start().starts_with("ba") {
        a.push("--merge-output-format".into());
        a.push("mp4".into());
    }
    if let Some(dir) = find_ffmpeg().and_then(|f| f.parent().map(Path::to_path_buf)) {
        a.push("--ffmpeg-location".into());
        a.push(dir.to_string_lossy().into_owned());
    }
    a.push("-P".into());
    a.push(dest_dir.to_string_lossy().into_owned());
    a.push("-o".into());
    a.push(output_template(title));
    a.push("--progress-template".into());
    a.push(PROGRESS_TEMPLATE.into());
    a.push("--progress-template".into());
    a.push(POSTPROCESS_TEMPLATE.into());
    a.push("--print".into());
    a.push(FILE_PRINT.into());
    a.push("--print".into());
    a.push(MERGE_PRINT.into());
    a.push("--".into());
    a.push(url.trim().to_string());
    a
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A job object that kills every process in it (yt-dlp and its ffmpeg children) when
    /// terminated or closed.
    pub struct Job(HANDLE);

    // The handle is only used through thread-safe Win32 calls.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new() -> Option<Job> {
            // SAFETY: plain Win32 calls with valid arguments; the handle is owned by `Job`.
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                Some(Job(h))
            }
        }

        pub fn assign(&self, process: RawHandle) -> bool {
            // SAFETY: `process` is a live process handle owned by tokio's Child.
            unsafe { AssignProcessToJobObject(self.0, process as HANDLE) != 0 }
        }

        pub fn kill(&self) {
            // SAFETY: valid job handle.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Kill yt-dlp and everything it started.
async fn kill_tree(child: &mut tokio::process::Child, #[allow(unused)] job: &Option<JobHandle>) {
    #[cfg(windows)]
    {
        if let Some(j) = job {
            j.kill();
        }
        if let Some(pid) = child.id() {
            // Fallback for children that escaped the job (started before assignment).
            let mut tk = tokio::process::Command::new("taskkill");
            tk.args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW);
            let _ = tk.status().await;
        }
    }
    let _ = child.kill().await;
}

#[cfg(windows)]
type JobHandle = job::Job;
#[cfg(not(windows))]
type JobHandle = ();

/// Start a yt-dlp download. `format_id` is an id from `probe` (any yt-dlp selector works).
/// `title` overrides the output name (`<title> [<video id>].<ext>`). Must be called inside a
/// tokio runtime. Pausing kills yt-dlp; starting again with the same arguments resumes from
/// its `.part` files.
pub fn start(
    url: String,
    dest_dir: PathBuf,
    format_id: String,
    title: Option<String>,
) -> MediaHandle {
    let name = title
        .as_deref()
        .map(sanitize)
        .unwrap_or_else(|| "video".into());
    let shared = Shared::with_name(name);
    tokio::spawn(run(shared.clone(), url, dest_dir, format_id, title));
    MediaHandle { shared }
}

async fn run(
    shared: Arc<Shared>,
    url: String,
    dest_dir: PathBuf,
    format_id: String,
    title: Option<String>,
) {
    let res = run_inner(&shared, &url, &dest_dir, &format_id, title.as_deref()).await;
    if let Ok(p) = &res {
        write_zone_identifier(p, url.trim(), None);
    }
    shared.complete(res);
}

async fn run_inner(
    shared: &Arc<Shared>,
    url: &str,
    dest_dir: &Path,
    format_id: &str,
    title: Option<&str>,
) -> Result<PathBuf> {
    check_url(url)?;
    if format_id.trim().is_empty() {
        return Err(Error::Other("no format selected".into()));
    }
    let exe = find_ytdlp().ok_or_else(not_found)?;
    tokio::fs::create_dir_all(dest_dir).await?;
    let mut cancel_rx = shared.cancel.subscribe();
    if *cancel_rx.borrow() {
        return Err(Error::Paused);
    }
    let mut cmd = command(&exe);
    cmd.args(build_args(url, dest_dir, format_id, title));
    let mut child = cmd.spawn()?;
    #[cfg(windows)]
    let job: Option<JobHandle> =
        job::Job::new().filter(|j| child.raw_handle().is_some_and(|h| j.assign(h)));
    #[cfg(not(windows))]
    let job: Option<JobHandle> = None;

    let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));
    if let Some(err) = child.stderr.take() {
        let tail = stderr_tail.clone();
        let sh = shared.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let l = l.trim().to_string();
                if l.is_empty() {
                    continue;
                }
                // Postprocessor progress (merging) is printed on stderr.
                if is_mux_step(&l) {
                    sh.muxing.store(true, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                if l.starts_with("THRENNIXPP ") {
                    continue;
                }
                tracing::debug!("yt-dlp: {l}");
                let mut t = tail.lock().unwrap();
                t.push(l);
                if t.len() > 20 {
                    t.remove(0);
                }
            }
        });
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("yt-dlp stdout unavailable".into()))?;
    let mut lines = BufReader::new(stdout).lines();

    shared.set_status(Status::Downloading);
    shared.publish(0.0);
    let mut acc = Accumulator::default();
    let mut final_path: Option<PathBuf> = None;
    let mut merge: Option<MergeParts> = None;
    let mut speed = 0.0;
    let mut speedo = Speedo::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let line = line.trim_end();
                if let Some(p) = parse_progress(line) {
                    let (done, total) = acc.update(&p);
                    let conns = if p.fragment_count.is_some() { 8 } else { 1 };
                    shared.set_bytes(done, total, done, conns);
                    speed = p.speed.filter(|s| *s > 0.0).unwrap_or(-1.0);
                } else if let Some(path) = line.strip_prefix("THRENNIXFILE ") {
                    final_path = Some(PathBuf::from(path.trim()));
                } else if line.starts_with("THRENNIXMERGE ") {
                    merge = parse_merge(line);
                } else if is_mux_step(line) {
                    shared.muxing.store(true, std::sync::atomic::Ordering::Relaxed);
                } else if !line.is_empty() {
                    tracing::debug!("yt-dlp: {line}");
                }
            }
            _ = cancelled(&mut cancel_rx) => {
                kill_tree(&mut child, &job).await;
                return Err(Error::Paused);
            }
            _ = tick.tick() => {
                let measured = speedo.update(shared.raw_bytes());
                shared.publish(if speed >= 0.0 { speed } else { measured });
            }
        }
    }
    let status = tokio::select! {
        s = child.wait() => s?,
        _ = cancelled(&mut cancel_rx) => {
            kill_tree(&mut child, &job).await;
            return Err(Error::Paused);
        }
    };
    drop(job);
    if !status.success() {
        let msg = {
            let tail = stderr_tail.lock().unwrap();
            tail.iter()
                .rev()
                .find(|l| l.starts_with("ERROR"))
                .or(tail.last())
                .cloned()
                .unwrap_or_else(|| format!("yt-dlp failed ({status})"))
        };
        let downloaded = merge
            .as_ref()
            .filter(|m| m.video.is_file() && m.audio.is_file());
        let Some(parts) = downloaded else {
            return Err(Error::Other(msg));
        };
        tracing::warn!("{msg}; merging the downloaded parts ourselves");
        shared
            .muxing
            .store(true, std::sync::atomic::Ordering::Relaxed);
        return merge_parts(parts)
            .await
            .map_err(|e| Error::Other(format!("Merging video and audio failed: {e}")));
    }
    let path =
        final_path.ok_or_else(|| Error::Other("yt-dlp did not report the output file".into()))?;
    let path = if path.is_absolute() {
        path
    } else {
        dest_dir.join(path)
    };
    if !path.is_file() {
        return Err(Error::Other(format!(
            "yt-dlp output {} is missing",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-written, `-J`-shaped fixture (not a real video's metadata).
    const FIXTURE: &str = r#"{
      "id": "abc123XYZ_0", "title": "Sample Clip: 100% synthetic", "_type": "video",
      "formats": [
        {"format_id": "sb0", "ext": "mhtml", "vcodec": "none", "acodec": "none", "format_note": "storyboard", "width": 160, "height": 90},
        {"format_id": "139", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.5", "abr": 48.0, "tbr": 48.0, "filesize": 1000000},
        {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 129.0, "tbr": 129.0, "filesize": 3000000},
        {"format_id": "251", "ext": "webm", "vcodec": "none", "acodec": "opus", "abr": 140.0, "tbr": 140.0, "filesize_approx": 3500000},
        {"format_id": "18", "ext": "mp4", "vcodec": "avc1.42001E", "acodec": "mp4a.40.2", "height": 360, "width": 640, "fps": 30, "tbr": 500.0, "filesize": 9000000},
        {"format_id": "134", "ext": "mp4", "vcodec": "avc1.4d401e", "acodec": "none", "height": 360, "width": 640, "fps": 30, "tbr": 600.0, "filesize": 8000000},
        {"format_id": "136", "ext": "mp4", "vcodec": "avc1.4d401f", "acodec": "none", "height": 720, "width": 1280, "fps": 30, "tbr": 1500.0, "filesize": 20000000},
        {"format_id": "298", "ext": "mp4", "vcodec": "avc1.4d4020", "acodec": "none", "height": 720, "width": 1280, "fps": 60, "tbr": 2500.0, "filesize": 30000000},
        {"format_id": "299", "ext": "mp4", "vcodec": "avc1.64002a", "acodec": "none", "height": 1080, "width": 1920, "fps": 60, "tbr": 4500.0},
        {"format_id": "401", "ext": "mp4", "vcodec": "av01.0.12M.08", "acodec": "none", "height": 2160, "width": 3840, "fps": 30, "tbr": 18000.0, "filesize_approx": 250000000}
      ]
    }"#;

    #[test]
    fn format_list_from_fixture() {
        let info = build_info(&serde_json::from_str(FIXTURE).unwrap()).unwrap();
        assert_eq!(info.title, "Sample Clip: 100% synthetic");
        let labels: Vec<_> = info.formats.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "MP4 · 2160p 4K",
                "MP4 · 1080p60 HD",
                "MP4 · 720p60 HD",
                "MP4 · 360p",
                "Audio · M4A"
            ]
        );
        let f = &info.formats;
        assert_eq!(
            f[0].id,
            "bv*[height=2160][vcodec^=avc1]+ba[ext=m4a]/bv*[height=2160]+ba[ext=m4a]/bv*[height=2160]+ba/bv*[height<=2160]+ba/b[height<=2160]"
        );
        assert_eq!(f[0].height, Some(2160));
        // Best video (approx) + best audio (opus, approx).
        assert_eq!(f[0].filesize, Some(250_000_000 + 3_500_000));
        // 1080p size unknown.
        assert_eq!(f[1].filesize, None);
        // 720p: the 60 fps one has the higher bitrate.
        assert_eq!(f[2].filesize, Some(30_000_000 + 3_500_000));
        // 360p: video-only 134 (higher tbr) + audio.
        assert_eq!(f[3].filesize, Some(8_000_000 + 3_500_000));
        let a = &f[4];
        assert_eq!(a.id, "ba[ext=m4a]/ba");
        assert_eq!(a.height, None);
        assert_eq!(a.filesize, Some(3_000_000));
        // Serializes in the protocol's shape.
        let j = serde_json::to_value(a).unwrap();
        assert_eq!(j["height"], Value::Null);
    }

    #[test]
    fn format_list_edge_cases() {
        let none = build_info(&serde_json::json!({"title": "", "formats": []})).unwrap();
        assert_eq!(none.title, "video");
        assert_eq!(none.formats.len(), 1);
        assert_eq!(none.formats[0].id, "bv*+ba/b");
        let direct = build_info(&serde_json::json!({"title": "t", "formats": [{"format_id": "0", "url": "x", "ext": "mp4", "filesize": 5}]})).unwrap();
        assert_eq!(direct.formats.len(), 1);
        assert_eq!(direct.formats[0].filesize, Some(5));
        let audio_only = build_info(&serde_json::json!({"title": "t", "formats": [{"vcodec": "none", "acodec": "mp3", "ext": "mp3"}]})).unwrap();
        assert_eq!(
            audio_only
                .formats
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            ["ba[ext=m4a]/ba"]
        );
        assert!(build_info(&serde_json::json!({"_type": "playlist", "entries": []})).is_err());
        // Garbage values do not panic.
        let junk = serde_json::json!({"title": 5, "formats": [{"height": -3, "fps": "x", "filesize": 1e300}, {"height": 1e30}, 7, null]});
        let _ = build_info(&junk).unwrap();
    }

    #[test]
    fn size_ignores_unsized_hls_variant() {
        // YouTube-shaped: the m3u8 variant has the higher tbr but no size.
        let info = build_info(&serde_json::json!({"title": "t", "formats": [
            {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "tbr": 130.0, "filesize": 100000},
            {"format_id": "270", "ext": "mp4", "vcodec": "avc1.640028", "acodec": "none", "height": 1080, "tbr": 1880.0, "protocol": "m3u8_native"},
            {"format_id": "137", "ext": "mp4", "vcodec": "avc1.640028", "acodec": "none", "height": 1080, "tbr": 1646.0, "filesize": 1345256, "protocol": "https"}
        ]}))
        .unwrap();
        assert_eq!(info.formats[0].height, Some(1080));
        assert_eq!(info.formats[0].filesize, Some(1345256 + 100000));
    }

    #[test]
    fn labels() {
        assert_eq!(label(1080, Some(30.0)), "MP4 · 1080p HD");
        assert_eq!(label(1080, Some(59.94)), "MP4 · 1080p60 HD");
        assert_eq!(label(720, None), "MP4 · 720p HD");
        assert_eq!(label(480, Some(25.0)), "MP4 · 480p");
        assert_eq!(label(144, None), "MP4 · 144p");
        assert_eq!(label(2160, Some(60.0)), "MP4 · 2160p60 4K");
        assert_eq!(label(4320, None), "MP4 · 4320p 8K");
    }

    #[test]
    fn progress_lines() {
        let p = parse_progress("THRENNIX 1024 4096 NA 512.5 NA NA C:\\Videos\\a b [x].f137.mp4")
            .unwrap();
        assert_eq!(p.downloaded, Some(1024));
        assert_eq!(p.total, Some(4096));
        assert_eq!(p.estimate, None);
        assert_eq!(p.speed, Some(512.5));
        assert_eq!(p.fragment_count, None);
        assert_eq!(p.filename, "C:\\Videos\\a b [x].f137.mp4");
        let p = parse_progress("THRENNIX 2000 NA 10000.7 NA 3 40 f.mp4\r").unwrap();
        assert_eq!((p.total, p.estimate, p.speed), (None, Some(10000), None));
        assert_eq!((p.fragment_index, p.fragment_count), (Some(3), Some(40)));
        assert_eq!(parse_progress("[download] 10%"), None);
        assert_eq!(parse_progress("THRENNIX 1 2"), None);
        assert!(parse_progress("THRENNIX x y z w v u").is_some());
        assert!(is_mux_step("[Merger] Merging formats into \"x.mp4\""));
        assert!(is_mux_step("THRENNIXPP started Merger"));
        assert!(!is_mux_step("THRENNIXPP finished Merger"));
        assert!(!is_mux_step("THRENNIXPP started MoveFiles"));
    }

    #[test]
    fn merge_lines() {
        let m = parse_merge(r"THRENNIXMERGE 701 mp4 140-drc m4a C:\dl\My v1.2 [abc].mp4").unwrap();
        assert_eq!(m.video, PathBuf::from(r"C:\dl\My v1.2 [abc].f701.mp4"));
        assert_eq!(m.audio, PathBuf::from(r"C:\dl\My v1.2 [abc].f140-drc.m4a"));
        assert_eq!(m.merged, PathBuf::from(r"C:\dl\My v1.2 [abc].mp4"));
        // A single format: nothing to merge.
        assert_eq!(parse_merge(r"THRENNIXMERGE NA NA NA NA C:\dl\a.m4a"), None);
        assert_eq!(parse_merge("THRENNIXMERGE 1 mp4"), None);
    }

    #[test]
    fn accumulates_across_files() {
        let mut acc = Accumulator::default();
        let p = |d: u64, t: u64, f: &str| YtProgress {
            downloaded: Some(d),
            total: Some(t),
            estimate: None,
            speed: None,
            fragment_index: None,
            fragment_count: None,
            filename: f.into(),
        };
        assert_eq!(acc.update(&p(50, 100, "v")), (50, Some(100)));
        assert_eq!(acc.update(&p(100, 100, "v")), (100, Some(100)));
        // Audio starts: bar continues from 100.
        assert_eq!(acc.update(&p(10, 40, "a")), (110, Some(140)));
        assert_eq!(acc.update(&p(40, 40, "a")), (140, Some(140)));
    }

    #[test]
    fn args() {
        let a = build_args(
            " https://example.com/watch?v=1 ",
            Path::new("D:\\dl"),
            "-x",
            Some("My 100% clip.mp4"),
        );
        assert!(a.contains(&"--format=-x".to_string()));
        assert!(a.contains(&"My 100%% clip [%(id)s].%(ext)s".to_string()));
        assert_eq!(a[a.len() - 2], "--");
        assert_eq!(a[a.len() - 1], "https://example.com/watch?v=1");
        assert!(a.contains(&"--merge-output-format".to_string()));
        assert!(a.windows(2).any(|w| w == ["--encoding", "utf-8"]));
        let audio = build_args("https://e.com/", Path::new("x"), AUDIO_ID, None);
        assert!(!audio.contains(&"--merge-output-format".to_string()));
        assert!(audio.contains(&"%(title)s [%(id)s].%(ext)s".to_string()));
        assert!(check_url("file:///C:/x").is_err());
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("https://www.youtube.com/watch?v=x").is_ok());
    }

    /// Live: probes a Creative-Commons video (Big Buck Bunny, Blender Foundation, CC BY 3.0).
    /// Run manually: `cargo test -p thrennix-media ytdlp_live_probe -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn ytdlp_live_probe() {
        if find_ytdlp().is_none() {
            eprintln!("yt-dlp not installed; skipping");
            return;
        }
        let info = probe("https://www.youtube.com/watch?v=aqz-KE-bpKQ")
            .await
            .unwrap();
        eprintln!("{info:#?}");
        assert!(info.title.to_lowercase().contains("bunny"));
        assert!(info.formats.iter().any(|f| f.height == Some(1080)));
        assert_eq!(info.formats.last().unwrap().id, AUDIO_ID);
    }
}
