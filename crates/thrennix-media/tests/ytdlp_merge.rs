//! yt-dlp downloads both parts of a 4K video but its merge fails: Thrennix merges them.
//! A stand-in yt-dlp (a .cmd script) reports the parts and fails like yt-dlp does. Its own
//! test binary, because it points `THRENNIX_YTDLP` at the stand-in for the whole process.
//! Skipped when ffmpeg is not installed.
#![cfg(windows)]

use std::path::Path;
use std::process::Command;

use thrennix_media::{ytdlp, Status};

fn make(ff: &Path, out: &Path, input: &str, codec: &[&str]) {
    let st = Command::new(ff)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            input,
        ])
        .args(codec)
        .arg(out)
        .status()
        .unwrap();
    assert!(st.success());
}

fn stream_types(ff: &Path, file: &Path) -> Vec<String> {
    let out = Command::new(ff.with_file_name("ffprobe.exe"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_merge_is_finished_by_thrennix() {
    let Some(ff) = thrennix_media::find_ffmpeg() else {
        eprintln!("ffmpeg not found; skipping");
        return;
    };
    let dest = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let stem = dest.path().join("Clip 4K [abc123]");
    let part = |suffix: &str| {
        let mut p = stem.as_os_str().to_owned();
        p.push(suffix);
        std::path::PathBuf::from(p)
    };
    // VP9 in WebM + AAC: what YouTube serves at 4K.
    make(
        &ff,
        &part(".f313.webm"),
        "testsrc=duration=2:size=320x240:rate=25",
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-b:v",
            "200k",
        ],
    );
    make(&ff, &part(".f140.m4a"), "sine=duration=2", &["-c:a", "aac"]);
    std::fs::write(part(".temp.mp4"), b"half-written").unwrap();

    let fake = tools.path().join("yt-dlp.cmd");
    std::fs::write(
        &fake,
        format!(
            "@echo off\r\necho THRENNIXMERGE 313 webm 140 m4a {}.mp4\r\n\
             echo ERROR: Postprocessing: Conversion failed! 1>&2\r\nexit /b 1\r\n",
            stem.display()
        ),
    )
    .unwrap();
    std::env::set_var("THRENNIX_YTDLP", &fake);

    let h = ytdlp::start(
        "https://www.youtube.com/watch?v=abc123".into(),
        dest.path().to_path_buf(),
        "bv*+ba".into(),
        Some("Clip 4K".into()),
    );
    let out = h.wait().await.unwrap();
    assert_eq!(h.progress().status, Status::Completed);
    assert_eq!(out, part(".mp4"));
    assert_eq!(stream_types(&ff, &out), ["audio", "video"]);
    // The parts and yt-dlp's leftover are gone.
    for s in [".f313.webm", ".f140.m4a", ".temp.mp4"] {
        assert!(!part(s).exists(), "{s} left behind");
    }
}
