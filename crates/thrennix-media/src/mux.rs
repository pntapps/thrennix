//! Concatenating segments and muxing with ffmpeg.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::{Error, Result};

/// Concatenate `parts` (in order) into `out`.
pub(crate) async fn concat(parts: Vec<PathBuf>, out: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut w = std::io::BufWriter::with_capacity(4 << 20, std::fs::File::create(&out)?);
        for p in &parts {
            let mut r = std::fs::File::open(p)?;
            std::io::copy(&mut r, &mut w)?;
        }
        w.flush()?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(format!("concat task failed: {e}")))?
}

/// ffmpeg demuxer for a concatenated track file.
pub(crate) fn input_format(ext: &str) -> &'static str {
    match ext {
        ".ts" => "mpegts",
        ".aac" => "aac",
        ".mp3" => "mp3",
        ".webm" => "matroska",
        _ => "mp4",
    }
}

fn needs_adts_filter(ext: &str) -> bool {
    matches!(ext, ".ts" | ".aac")
}

async fn run_ffmpeg(
    ffmpeg: &Path,
    args: &[std::ffi::OsString],
) -> Result<std::result::Result<(), String>> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().await?;
    if out.status.success() {
        Ok(Ok(()))
    } else {
        Ok(Err(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

/// Remux `video` (+ optional separate `audio`) into an MP4 at `out` with stream copy.
pub(crate) async fn ffmpeg_mux(
    ffmpeg: &Path,
    video: (&Path, &str),
    audio: Option<(&Path, &str)>,
    out: &Path,
) -> Result<()> {
    // Force the demuxer and allow only local file access, so server-supplied bytes cannot
    // steer ffmpeg into other formats (playlists, concat lists) or other files/URLs.
    let input = |a: &mut Vec<std::ffi::OsString>, path: &Path, ext: &str| {
        for s in ["-protocol_whitelist", "file", "-f", input_format(ext), "-i"] {
            a.push(s.into());
        }
        a.push(path.into());
    };
    let build = |bsf: bool| {
        let mut a: Vec<std::ffi::OsString> = ["-y", "-hide_banner", "-loglevel", "error"]
            .iter()
            .map(Into::into)
            .collect();
        input(&mut a, video.0, video.1);
        if let Some((ap, ae)) = audio {
            input(&mut a, ap, ae);
            for s in ["-map", "0:v?", "-map", "1:a?"] {
                a.push(s.into());
            }
        }
        a.push("-c".into());
        a.push("copy".into());
        if bsf {
            a.push("-bsf:a".into());
            a.push("aac_adtstoasc".into());
        }
        for s in ["-movflags", "+faststart", "-f", "mp4"] {
            a.push(s.into());
        }
        a.push(out.into());
        a
    };
    let want_bsf = needs_adts_filter(video.1) || audio.is_some_and(|(_, e)| needs_adts_filter(e));
    let first = run_ffmpeg(ffmpeg, &build(want_bsf)).await?;
    let result = match first {
        Err(msg) if want_bsf => {
            tracing::debug!("ffmpeg with aac_adtstoasc failed ({msg}); retrying without it");
            run_ffmpeg(ffmpeg, &build(false)).await?
        }
        r => r,
    };
    result.map_err(|msg| Error::Other(format!("ffmpeg failed: {msg}")))
}
