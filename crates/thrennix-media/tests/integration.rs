//! End-to-end tests against an in-process HTTP server serving ffmpeg-generated streams.
//! Tests that need ffmpeg are skipped (with a message) when it cannot be found.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use thrennix_media::{list_variants, start, Error, MediaKind, MediaRequest, Quality, Status};

struct Srv {
    root: PathBuf,
    delay: Duration,
    segment_hits: AtomicUsize,
    any_hits: AtomicUsize,
    /// Requests to the "foreign" host (localhost) that carried a Cookie / a Referer.
    foreign_cookie: AtomicUsize,
    foreign_referer: AtomicUsize,
    foreign_hits: AtomicUsize,
}

async fn serve(State(st): State<Arc<Srv>>, headers: HeaderMap, uri: Uri) -> Response {
    st.any_hits.fetch_add(1, Ordering::SeqCst);
    let hdr = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let foreign = hdr("host").starts_with("localhost");
    if foreign {
        st.foreign_hits.fetch_add(1, Ordering::SeqCst);
        if !hdr("cookie").is_empty() {
            st.foreign_cookie.fetch_add(1, Ordering::SeqCst);
        }
        if hdr("referer") == "http://example.test/" {
            st.foreign_referer.fetch_add(1, Ordering::SeqCst);
        }
    } else if hdr("cookie") != "session=abc"
        || hdr("x-thrennix-test") != "yes"
        || hdr("referer") != "http://example.test/"
    {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("missing auth headers"))
            .unwrap();
    }
    let rel = uri.path().trim_start_matches('/');
    if rel.contains("..") {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    }
    let path = st.root.join(rel);
    let Ok(data) = tokio::fs::read(&path).await else {
        eprintln!("test server: 404 {}", uri.path());
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    };
    let is_segment = !(rel.ends_with(".m3u8") || rel.ends_with(".mpd") || rel.ends_with(".bin"));
    if is_segment {
        st.segment_hits.fetch_add(1, Ordering::SeqCst);
        if !st.delay.is_zero() {
            tokio::time::sleep(st.delay).await;
        }
    }
    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        let spec = range.trim_start_matches("bytes=");
        let (a, b) = spec.split_once('-').unwrap();
        let a: usize = a.parse().unwrap();
        let b: usize = if b.is_empty() {
            data.len() - 1
        } else {
            b.parse::<usize>().unwrap().min(data.len() - 1)
        };
        let slice = data[a..=b].to_vec();
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {a}-{b}/{}", data.len()),
            )
            .header(header::CONTENT_LENGTH, slice.len())
            .body(Body::from(slice))
            .unwrap();
    }
    Response::builder()
        .header(header::CONTENT_LENGTH, data.len())
        .body(Body::from(data))
        .unwrap()
}

async fn server(root: &Path, delay: Duration) -> (String, Arc<Srv>) {
    let st = Arc::new(Srv {
        root: root.to_path_buf(),
        delay,
        segment_hits: AtomicUsize::new(0),
        any_hits: AtomicUsize::new(0),
        foreign_cookie: AtomicUsize::new(0),
        foreign_referer: AtomicUsize::new(0),
        foreign_hits: AtomicUsize::new(0),
    });
    let app = Router::new().fallback(serve).with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), st)
}

fn request(url: String, dest: &Path) -> MediaRequest {
    MediaRequest {
        url,
        dest_dir: dest.to_path_buf(),
        file_name: None,
        cookies: Some("session=abc".into()),
        referer: Some("http://example.test/".into()),
        user_agent: Some("thrennix-test".into()),
        headers: vec![("X-Thrennix-Test".into(), "yes".into())],
    }
}

fn ffmpeg() -> Option<PathBuf> {
    let f = thrennix_media::find_ffmpeg();
    if f.is_none() {
        eprintln!("ffmpeg not found; skipping test");
    }
    f
}

fn run(ffmpeg: &Path, args: &[&str]) {
    let out = Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const SOURCES: [&str; 8] = [
    "-f",
    "lavfi",
    "-i",
    "testsrc=duration=4:size=320x240:rate=25",
    "-f",
    "lavfi",
    "-i",
    "sine=duration=4",
];
const CODECS: [&str; 10] = [
    "-c:v",
    "libx264",
    "-preset",
    "ultrafast",
    "-g",
    "25",
    "-c:a",
    "aac",
    "-b:a",
    "64k",
];

fn gen_hls(ffmpeg: &Path, dir: &Path, extra: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    let playlist = dir.join("index.m3u8");
    let mut args: Vec<&str> = SOURCES.to_vec();
    args.extend(CODECS);
    args.extend(["-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod"]);
    args.extend(extra);
    args.push(playlist.to_str().unwrap());
    run(ffmpeg, &args);
}

/// Stream types (e.g. ["video", "audio"]) and duration of a media file, via ffprobe.
fn probe(ffmpeg: &Path, file: &Path) -> (Vec<String>, f64) {
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let out = Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut types = Vec::new();
    let mut dur = 0.0;
    for l in text.lines().map(str::trim) {
        match l.parse::<f64>() {
            Ok(d) => dur = d,
            Err(_) if !l.is_empty() => types.push(l.to_string()),
            _ => {}
        }
    }
    types.sort();
    (types, dur)
}

fn assert_av_mp4(ffmpeg: &Path, out: &Path) {
    assert_eq!(out.extension().unwrap(), "mp4");
    let bytes = std::fs::read(out).unwrap();
    assert_eq!(&bytes[4..8], b"ftyp", "not an MP4");
    let (types, dur) = probe(ffmpeg, out);
    assert_eq!(types, ["audio", "video"], "streams in {}", out.display());
    assert!(dur > 3.5 && dur < 4.6, "duration {dur}");
}

fn no_work_dirs(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        assert!(
            !e.file_name().to_string_lossy().ends_with(".thrennix-media"),
            "work dir left behind"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_ts_download() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    gen_hls(&ff, &src.path().join("show"), &[]);
    let (base, st) = server(src.path(), Duration::ZERO).await;

    let mut req = request(format!("{base}/show/index.m3u8"), dest.path());
    let vars = list_variants(MediaKind::Hls, &req).await.unwrap();
    assert_eq!(vars.len(), 1);

    let h = start(MediaKind::Hls, req.clone(), Quality::Best, 0);
    let out = h.wait().await.unwrap();
    assert_eq!(out, dest.path().join("show.mp4"));
    assert_av_mp4(&ff, &out);
    let p = h.progress();
    assert_eq!(p.status, Status::Completed);
    assert!(p.downloaded > 0);
    assert_eq!(h.output_path(), Some(out.clone()));
    assert_eq!(p.path, out);
    assert!(!h.is_muxing());
    assert!(!h.work_dir().unwrap().exists());
    #[cfg(windows)]
    {
        let mut ads = out.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        let motw = std::fs::read_to_string(PathBuf::from(ads)).unwrap();
        assert!(motw.contains("ZoneId=3"), "{motw}");
        assert!(
            motw.contains(&format!("HostUrl={base}/show/index.m3u8")),
            "{motw}"
        );
        assert!(motw.contains("ReferrerUrl=http://example.test/"), "{motw}");
    }
    let (done, total) = h.segments_done();
    assert_eq!(done, total);
    assert!(total >= 4);
    assert_eq!(st.segment_hits.load(Ordering::SeqCst), total);
    // wait() can be called again.
    assert_eq!(h.wait().await.unwrap(), out);
    no_work_dirs(dest.path());

    // Same name again: "show (1).mp4".
    req.file_name = None;
    let out2 = start(MediaKind::Hls, req, Quality::Best, 4)
        .wait()
        .await
        .unwrap();
    assert_eq!(out2, dest.path().join("show (1).mp4"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_missing_headers_fail() {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("a.m3u8"),
        "#EXTM3U\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let (base, _st) = server(src.path(), Duration::ZERO).await;
    let mut req = request(format!("{base}/a.m3u8"), src.path());
    req.cookies = None;
    let r = start(MediaKind::Hls, req, Quality::Best, 2).wait().await;
    assert!(matches!(r, Err(Error::Status(403))), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_aes128_download() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let dir = src.path().join("enc");
    std::fs::create_dir_all(&dir).unwrap();
    // Key generated from the clock so it differs per run.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let key: Vec<u8> = (0..16u32)
        .map(|i| {
            ((seed >> (i % 8 * 8)) as u8)
                .wrapping_mul(31)
                .wrapping_add(i as u8 * 17)
        })
        .collect();
    std::fs::write(dir.join("key.bin"), &key).unwrap();
    let keyinfo = dir.join("keyinfo.txt");
    std::fs::write(
        &keyinfo,
        format!("key.bin\n{}\n", dir.join("key.bin").display()),
    )
    .unwrap();
    gen_hls(
        &ff,
        &dir,
        &["-hls_key_info_file", keyinfo.to_str().unwrap()],
    );
    let playlist = std::fs::read_to_string(dir.join("index.m3u8")).unwrap();
    assert!(playlist.contains("METHOD=AES-128"), "{playlist}");
    // Segments really are encrypted (no TS sync byte).
    assert_ne!(std::fs::read(dir.join("index0.ts")).unwrap()[0], 0x47);

    let (base, _st) = server(src.path(), Duration::ZERO).await;
    let mut req = request(format!("{base}/enc/index.m3u8"), dest.path());
    req.file_name = Some("encrypted".into());
    let out = start(MediaKind::Hls, req, Quality::Best, 3)
        .wait()
        .await
        .unwrap();
    assert_eq!(out.file_name().unwrap(), "encrypted.mp4");
    assert_av_mp4(&ff, &out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_fmp4_byterange_download() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    gen_hls(
        &ff,
        &src.path().join("f"),
        &["-hls_segment_type", "fmp4", "-hls_flags", "single_file"],
    );
    let playlist = std::fs::read_to_string(src.path().join("f").join("index.m3u8")).unwrap();
    assert!(
        playlist.contains("#EXT-X-MAP") && playlist.contains("#EXT-X-BYTERANGE"),
        "{playlist}"
    );
    let (base, _st) = server(src.path(), Duration::ZERO).await;
    let mut req = request(format!("{base}/f/index.m3u8"), dest.path());
    req.file_name = Some("frag".into());
    let h = start(MediaKind::Hls, req, Quality::Best, 4);
    let out = h.wait().await.unwrap();
    assert_av_mp4(&ff, &out);
    // All sizes known from byte ranges.
    let p = h.progress();
    assert_eq!(p.total, Some(p.downloaded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_master_with_separate_audio() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    gen_hls(&ff, &src.path().join("v240"), &["-an"]);
    gen_hls(&ff, &src.path().join("aud"), &["-vn"]);
    // A second (smaller) video variant, to check quality selection.
    {
        let dir = src.path().join("v120");
        std::fs::create_dir_all(&dir).unwrap();
        let mut args: Vec<&str> = SOURCES.to_vec();
        args.extend(CODECS);
        let pl = dir.join("index.m3u8");
        args.extend([
            "-an",
            "-vf",
            "scale=160:120",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_playlist_type",
            "vod",
            pl.to_str().unwrap(),
        ]);
        run(&ff, &args);
    }
    std::fs::write(
        src.path().join("master.m3u8"),
        "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Main\",DEFAULT=YES,URI=\"aud/index.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=300000,RESOLUTION=160x120,CODECS=\"avc1.42c00d,mp4a.40.2\",AUDIO=\"a\"
v120/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=320x240,CODECS=\"avc1.42c00d,mp4a.40.2\",AUDIO=\"a\"
v240/index.m3u8
",
    )
    .unwrap();
    let (base, _st) = server(src.path(), Duration::ZERO).await;
    let mut req = request(format!("{base}/master.m3u8"), dest.path());
    let vars = list_variants(MediaKind::Hls, &req).await.unwrap();
    assert_eq!(
        vars.iter().map(|v| v.height).collect::<Vec<_>>(),
        [Some(240), Some(120)]
    );
    assert!(vars[0].url.ends_with("/v240/index.m3u8"));

    req.file_name = Some("small".into());
    let out = start(MediaKind::Hls, req, Quality::MaxHeight(200), 4)
        .wait()
        .await
        .unwrap();
    assert_av_mp4(&ff, &out);
    let wh = Command::new(ff.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    }))
    .args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=height",
        "-of",
        "csv=p=0",
    ])
    .arg(&out)
    .output()
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&wh.stdout).trim(), "120");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dash_download() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let dir = src.path().join("movie");
    std::fs::create_dir_all(&dir).unwrap();
    // ffmpeg's dash muxer only understands '/' when deriving the segment directory.
    let mpd = dir
        .join("manifest.mpd")
        .to_str()
        .unwrap()
        .replace('\\', "/");
    let mut args: Vec<&str> = SOURCES.to_vec();
    args.extend(CODECS);
    args.extend([
        "-f",
        "dash",
        "-seg_duration",
        "1",
        "-use_template",
        "1",
        "-use_timeline",
        "1",
        &mpd,
    ]);
    run(&ff, &args);
    assert!(
        dir.join("init-stream0.m4s").is_file(),
        "segments not written next to the MPD"
    );
    let (base, _st) = server(src.path(), Duration::ZERO).await;
    let req = request(format!("{base}/movie/manifest.mpd"), dest.path());
    let vars = list_variants(MediaKind::Dash, &req).await.unwrap();
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].height, Some(240));
    let h = start(MediaKind::Dash, req, Quality::Best, 4);
    let out = h.wait().await.unwrap();
    assert_eq!(out.file_name().unwrap(), "movie.mp4");
    assert_av_mp4(&ff, &out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_and_drm_rejected() {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("live.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na.ts\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("drm.m3u8"),
        "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("drm.mpd"),
        r#"<MPD type="static" mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><ContentProtection schemeIdUri="urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95"/><Representation id="v" bandwidth="1"><SegmentTemplate duration="1" media="$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#,
    )
    .unwrap();
    let (base, _st) = server(src.path(), Duration::ZERO).await;
    let msg = |r: thrennix_media::Result<PathBuf>| match r {
        Err(Error::Other(m)) => m,
        other => panic!("unexpected {other:?}"),
    };
    let live = start(
        MediaKind::Hls,
        request(format!("{base}/live.m3u8"), src.path()),
        Quality::Best,
        1,
    );
    assert_eq!(msg(live.wait().await), "live streams are not supported yet");
    assert!(matches!(live.progress().status, Status::Failed(_)));
    let drm = start(
        MediaKind::Hls,
        request(format!("{base}/drm.m3u8"), src.path()),
        Quality::Best,
        1,
    );
    assert_eq!(msg(drm.wait().await), "DRM-protected stream");
    let drm = start(
        MediaKind::Dash,
        request(format!("{base}/drm.mpd"), src.path()),
        Quality::Best,
        1,
    );
    assert_eq!(msg(drm.wait().await), "DRM-protected stream");
    let r = list_variants(
        MediaKind::Dash,
        &request(format!("{base}/drm.mpd"), src.path()),
    )
    .await;
    assert!(matches!(r, Err(Error::Other(m)) if m == "DRM-protected stream"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_pause_and_resume() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    // Short segments so there are many of them.
    std::fs::create_dir_all(src.path().join("p")).unwrap();
    {
        let pl = src.path().join("p").join("index.m3u8");
        let mut args: Vec<&str> = SOURCES.to_vec();
        args.extend([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
        ]);
        args.extend([
            "-f",
            "hls",
            "-hls_time",
            "0.2",
            "-hls_playlist_type",
            "vod",
            pl.to_str().unwrap(),
        ]);
        run(&ff, &args);
    }
    let (base, st) = server(src.path(), Duration::from_millis(150)).await;
    let req = request(format!("{base}/p/index.m3u8"), dest.path());

    let h = start(MediaKind::Hls, req.clone(), Quality::Best, 2);
    let mut rx = h.subscribe();
    loop {
        rx.changed().await.unwrap();
        if h.segments_done().0 >= 3 {
            break;
        }
    }
    h.pause();
    h.pause(); // idempotent
    assert!(matches!(h.wait().await, Err(Error::Paused)));
    assert!(matches!(h.wait().await, Err(Error::Paused)));
    assert_eq!(h.progress().status, Status::Paused);
    let (done_at_pause, total) = h.segments_done();
    assert!(
        total > done_at_pause,
        "finished before pause ({done_at_pause}/{total})"
    );
    let work = dest.path().join("p.thrennix-media");
    assert_eq!(h.work_dir(), Some(work.clone()));
    assert_eq!(h.output_path(), Some(dest.path().join("p.mp4")));
    assert_eq!(h.progress().path, dest.path().join("p.mp4"));
    assert!(dest.path().join("p.mp4.thrennix-reserve").is_file());

    // A different download with the same name must not touch the paused work dir.
    gen_hls(&ff, &src.path().join("other"), &[]);
    let mut other = request(format!("{base}/other/index.m3u8"), dest.path());
    other.file_name = Some("p".into());
    let other_out = start(MediaKind::Hls, other, Quality::Best, 4)
        .wait()
        .await
        .unwrap();
    assert_eq!(other_out, dest.path().join("p (1).mp4"));
    assert!(work.join("state.json").is_file());
    assert!(work.join("v_00001.seg").is_file());
    assert!(work.join("state.json").is_file());
    assert!(work.join("v_00001.seg").is_file());
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.join("state.json")).unwrap()).unwrap();
    let recorded = state["done"].as_object().unwrap().len();
    assert!(
        recorded >= done_at_pause.min(3),
        "state.json records {recorded}"
    );

    // Let any aborted in-flight requests settle before counting.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hits_before = st.segment_hits.load(Ordering::SeqCst);
    let h2 = start(MediaKind::Hls, req, Quality::Best, 4);
    let out = h2.wait().await.unwrap();
    let refetched = st.segment_hits.load(Ordering::SeqCst) - hits_before;
    assert!(
        refetched <= total - recorded,
        "refetched {refetched}, total {total}, recorded {recorded}"
    );
    assert_eq!(h2.segments_done(), (total, total));
    assert_av_mp4(&ff, &out);
    assert!(!work.exists());
    assert_eq!(out, dest.path().join("p.mp4"));
    assert!(!dest.path().join("p.mp4.thrennix-reserve").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_name_concurrent_downloads() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    gen_hls(&ff, &src.path().join("s1"), &[]);
    gen_hls(&ff, &src.path().join("s2"), &[]);
    let (base, _st) = server(src.path(), Duration::from_millis(30)).await;
    let mk = |dir: &str| {
        let mut r = request(format!("{base}/{dir}/index.m3u8"), dest.path());
        r.file_name = Some("same".into());
        r
    };
    // Also the very same request twice at once.
    let hs: Vec<_> = [mk("s1"), mk("s2"), mk("s1")]
        .into_iter()
        .map(|r| start(MediaKind::Hls, r, Quality::Best, 2))
        .collect();
    let mut outs = Vec::new();
    for h in &hs {
        let out = h.wait().await.unwrap();
        assert_eq!(h.output_path(), Some(out.clone()));
        assert_av_mp4(&ff, &out);
        outs.push(out);
    }
    outs.sort();
    assert_eq!(
        outs,
        [
            dest.path().join("same (1).mp4"),
            dest.path().join("same (2).mp4"),
            dest.path().join("same.mp4"),
        ]
    );
    no_work_dirs(dest.path());
    for e in std::fs::read_dir(dest.path()).unwrap().flatten() {
        assert!(
            !e.file_name()
                .to_string_lossy()
                .ends_with(".thrennix-reserve"),
            "placeholder left behind"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookies_not_sent_to_other_hosts() {
    let Some(ff) = ffmpeg() else { return };
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    gen_hls(&ff, &src.path().join("c"), &[]);
    let (base, st) = server(src.path(), Duration::ZERO).await;
    let port = base.rsplit(':').next().unwrap();
    // Segments live on "localhost" while the manifest is on 127.0.0.1: a different site.
    let pl = std::fs::read_to_string(src.path().join("c").join("index.m3u8")).unwrap();
    let rewritten: String = pl
        .lines()
        .map(|l| {
            if l.starts_with('#') || l.is_empty() {
                format!("{l}\n")
            } else {
                format!("http://localhost:{port}/c/{l}\n")
            }
        })
        .collect();
    std::fs::write(src.path().join("c").join("cross.m3u8"), rewritten).unwrap();
    let out = start(
        MediaKind::Hls,
        request(format!("{base}/c/cross.m3u8"), dest.path()),
        Quality::Best,
        4,
    )
    .wait()
    .await
    .unwrap();
    assert_av_mp4(&ff, &out);
    let hits = st.foreign_hits.load(Ordering::SeqCst);
    assert!(hits >= 4);
    assert_eq!(st.foreign_cookie.load(Ordering::SeqCst), 0, "cookie leaked");
    assert_eq!(
        st.foreign_referer.load(Ordering::SeqCst),
        hits,
        "referer missing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmuxable_stream_is_kept_raw() {
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("j1.ts"), b"this is not a transport stream").unwrap();
    std::fs::write(src.path().join("j2.ts"), b" -- neither is this").unwrap();
    std::fs::write(
        src.path().join("junk.m3u8"),
        "#EXTM3U\n#EXTINF:1,\nj1.ts\n#EXTINF:1,\nj2.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let (base, _st) = server(src.path(), Duration::ZERO).await;
    let out = start(
        MediaKind::Hls,
        request(format!("{base}/junk.m3u8"), dest.path()),
        Quality::Best,
        2,
    )
    .wait()
    .await
    .unwrap();
    assert_eq!(out, dest.path().join("junk.ts"));
    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"this is not a transport stream -- neither is this"
    );
    no_work_dirs(dest.path());
}
