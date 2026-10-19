//! yt-dlp backend against a local HLS stream (yt-dlp's generic extractor), no internet needed.
//! Skipped when yt-dlp or ffmpeg is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use thrennix_media::{ytdlp, Error, Status};

struct Srv {
    root: PathBuf,
    delay: Duration,
    hits: AtomicUsize,
}

async fn serve(State(st): State<Arc<Srv>>, uri: Uri) -> Response {
    let rel = uri.path().trim_start_matches('/');
    let Ok(data) = tokio::fs::read(st.root.join(rel)).await else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    };
    if rel.ends_with(".ts") {
        st.hits.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(st.delay).await;
    }
    let ct = if rel.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp2t"
    };
    Response::builder()
        .header("content-type", ct)
        .body(Body::from(data))
        .unwrap()
}

fn tools() -> Option<PathBuf> {
    let ff = thrennix_media::find_ffmpeg();
    if ytdlp::find_ytdlp().is_none() || ff.is_none() {
        eprintln!("yt-dlp or ffmpeg not found; skipping");
        return None;
    }
    ff
}

fn gen(ff: &Path, dir: &Path, extra: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    let pl = dir.join("index.m3u8");
    let out = Command::new(ff)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=4:size=320x240:rate=25",
        ])
        .args(["-f", "lavfi", "-i", "sine=duration=4"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
        ])
        .args(extra)
        .args(["-f", "hls", "-hls_time", "0.4", "-hls_playlist_type", "vod"])
        .arg(&pl)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn setup(ff: &Path, root: &Path, delay: Duration) -> (String, Arc<Srv>) {
    gen(ff, &root.join("v"), &["-an"]);
    gen(ff, &root.join("a"), &["-vn"]);
    std::fs::write(
        root.join("master.m3u8"),
        "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Main\",DEFAULT=YES,AUTOSELECT=YES,URI=\"a/index.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=400000,RESOLUTION=320x240,CODECS=\"avc1.42c00d,mp4a.40.2\",AUDIO=\"aud\"
v/index.m3u8
",
    )
    .unwrap();
    let st = Arc::new(Srv {
        root: root.to_path_buf(),
        delay,
        hits: AtomicUsize::new(0),
    });
    let app = Router::new().fallback(serve).with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/master.m3u8"), st)
}

fn stream_types(ff: &Path, file: &Path) -> Vec<String> {
    let out = Command::new(ff.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    }))
    .args([
        "-v",
        "error",
        "-show_entries",
        "stream=codec_type",
        "-of",
        "csv=p=0",
    ])
    .arg(file)
    .output()
    .unwrap();
    let mut t: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ytdlp_probe_and_download_local_hls() {
    let Some(ff) = tools() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let (url, _st) = setup(&ff, src.path(), Duration::ZERO).await;

    let info = ytdlp::probe(&url).await.unwrap();
    let ids: Vec<_> = info.formats.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "bv*[height=240][vcodec^=avc1]+ba[ext=m4a]/bv*[height=240]+ba[ext=m4a]/bv*[height=240]+ba/bv*[height<=240]+ba/b[height<=240]",
            "ba[ext=m4a]/ba"
        ],
        "{info:#?}"
    );
    assert_eq!(info.formats[0].label, "MP4 · 240p");

    let h = Arc::new(ytdlp::start(
        url.clone(),
        dest.path().to_path_buf(),
        info.formats[0].id.clone(),
        Some("local clip".into()),
    ));
    // The merge step (ffmpeg) is reported as muxing.
    let watcher = {
        let h = h.clone();
        tokio::spawn(async move {
            let mut saw = false;
            while h.progress().status != Status::Completed {
                saw |= h.is_muxing();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            saw
        })
    };
    let out = h.wait().await.unwrap();
    assert!(
        watcher.await.unwrap(),
        "merge phase was not reported as muxing"
    );
    assert!(!h.is_muxing());
    assert_eq!(h.progress().status, Status::Completed);
    assert_eq!(h.output_path(), Some(out.clone()));
    assert!(
        out.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("local clip ["),
        "{}",
        out.display()
    );
    assert_eq!(out.extension().unwrap(), "mp4");
    assert_eq!(stream_types(&ff, &out), ["audio", "video"]);
    assert!(h.progress().downloaded > 0);
    #[cfg(windows)]
    {
        let mut ads = out.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        let motw = std::fs::read_to_string(PathBuf::from(ads)).unwrap();
        assert!(motw.contains(&format!("HostUrl={url}")), "{motw}");
    }

    assert!(matches!(
        ytdlp::probe("file:///C:/Windows/win.ini").await,
        Err(Error::Other(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ytdlp_pause_and_resume() {
    let Some(ff) = tools() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let (url, st) = setup(&ff, src.path(), Duration::from_millis(250)).await;
    let id = "bv*[height<=240]+ba/b[height<=240]".to_string();

    let h = ytdlp::start(
        url.clone(),
        dest.path().to_path_buf(),
        id.clone(),
        Some("paused".into()),
    );
    // Wait until some fragments have been served, then pause.
    for _ in 0..200 {
        if st.hits.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.pause();
    h.pause();
    assert!(matches!(h.wait().await, Err(Error::Paused)));
    assert_eq!(h.progress().status, Status::Paused);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let hits_at_pause = st.hits.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        st.hits.load(Ordering::SeqCst),
        hits_at_pause,
        "yt-dlp kept downloading after pause"
    );

    let h2 = ytdlp::start(url, dest.path().to_path_buf(), id, Some("paused".into()));
    let out = h2.wait().await.unwrap();
    assert_eq!(stream_types(&ff, &out), ["audio", "video"]);
}
