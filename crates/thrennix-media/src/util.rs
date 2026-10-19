//! File naming and ffmpeg discovery.

use std::path::{Path, PathBuf};

use url::Url;

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Make `name` a valid Windows file name (no path separators, reserved characters or names).
pub(crate) fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.chars().count() > 200 {
        s = s
            .chars()
            .take(200)
            .collect::<String>()
            .trim_end_matches(['.', ' '])
            .to_string();
    }
    let stem = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    if s.is_empty() {
        s = "video".into();
    }
    s
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
        {
            let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Output stem derived from the manifest URL (`.../my-movie/master.m3u8` → `my-movie`).
pub(crate) fn name_from_url(url: &str) -> String {
    const GENERIC: [&str; 12] = [
        "",
        "index",
        "master",
        "playlist",
        "manifest",
        "main",
        "prog_index",
        "stream",
        "chunklist",
        "media",
        "video",
        "dash",
    ];
    let Ok(u) = Url::parse(url) else {
        return "video".into();
    };
    let segs: Vec<String> = u
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).map(percent_decode).collect())
        .unwrap_or_default();
    for seg in segs.iter().rev() {
        let stem = match seg.rfind('.') {
            Some(i) if i > 0 => &seg[..i],
            _ => seg.as_str(),
        };
        if !GENERIC.contains(&stem.to_ascii_lowercase().as_str()) {
            return stem.to_string();
        }
    }
    u.host_str()
        .map(|h| h.trim_start_matches("www.").to_string())
        .unwrap_or_else(|| "video".into())
}

/// Final file name: sanitized, ".mp4" appended if missing.
pub(crate) fn output_name(file_name: Option<&str>, url: &str) -> String {
    let raw = match file_name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => name_from_url(url),
    };
    let mut n = sanitize(&raw);
    if !n.to_ascii_lowercase().ends_with(".mp4") {
        n.push_str(".mp4");
    }
    n
}

/// `dir/name.ext`, or `dir/name (1).ext`, `name (2).ext`... if taken.
pub(crate) fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}{ext}"));
    if !first.exists() {
        return first;
    }
    (1..u32::MAX)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Errors worth retrying on Windows: another process (AV scanner, indexer) has the file open.
fn is_transient_fs_error(e: &std::io::Error) -> bool {
    // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33)
    e.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// `rename`, retried for ~10 s while the file is locked by someone else.
pub(crate) async fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match tokio::fs::rename(from, to).await {
            Err(e) if is_transient_fs_error(&e) && std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            r => return r,
        }
    }
}

/// Blocking `rename` with a few short retries (for small bookkeeping files).
pub(crate) fn rename_with_retry_blocking(
    from: &Path,
    to: &Path,
    tries: u32,
) -> std::io::Result<()> {
    let mut left = tries.max(1);
    loop {
        match std::fs::rename(from, to) {
            Err(e) if is_transient_fs_error(&e) && left > 1 => {
                left -= 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            r => return r,
        }
    }
}

/// Mark-of-the-Web: tag a finished download as coming from the internet. Errors are ignored.
pub(crate) fn write_zone_identifier(path: &Path, manifest_url: &str, referer: Option<&str>) {
    if !cfg!(windows) {
        return;
    }
    let clean = |s: &str| s.replace(['\r', '\n'], "");
    let mut text = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referer.filter(|r| !r.is_empty()) {
        text.push_str(&format!("ReferrerUrl={}\r\n", clean(r)));
    }
    text.push_str(&format!("HostUrl={}\r\n", clean(manifest_url)));
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    if let Err(e) = std::fs::write(PathBuf::from(ads), text) {
        tracing::debug!(
            "could not write Zone.Identifier for {}: {e}",
            path.display()
        );
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Locate a helper tool: `$env_var`, PATH, next to the current exe, `WinGet\Links`, then
/// `WinGet\Packages\*<pkg_hint>*\` (directly, in `bin\`, or in `*\bin\`).
pub(crate) fn find_tool(env_var: &str, name: &str, pkg_hint: &str) -> Option<PathBuf> {
    let exe = exe(name);
    if let Some(p) = std::env::var_os(env_var).map(PathBuf::from) {
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(&exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            let p = dir.join(&exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let winget = local.join("Microsoft").join("WinGet");
    let link = winget.join("Links").join(&exe);
    if link.is_file() {
        return Some(link);
    }
    for pkg in std::fs::read_dir(winget.join("Packages")).ok()?.flatten() {
        if !pkg
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains(pkg_hint)
        {
            continue;
        }
        let root = pkg.path();
        for p in [root.join(&exe), root.join("bin").join(&exe)] {
            if p.is_file() {
                return Some(p);
            }
        }
        // Gyan.FFmpeg_xxx/ffmpeg-7.x-full_build/bin/ffmpeg.exe
        if let Ok(inner) = std::fs::read_dir(&root) {
            for sub in inner.flatten() {
                let p = sub.path().join("bin").join(&exe);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Locate ffmpeg: `THRENNIX_FFMPEG`, PATH, next to the current exe, then the winget package dir.
pub fn find_ffmpeg() -> Option<PathBuf> {
    find_tool("THRENNIX_FFMPEG", "ffmpeg", "ffmpeg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a<b>c:d\"e/f\\g|h?i*j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("nul.txt"), "_nul.txt");
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize("   "), "video");
    }

    #[test]
    fn names() {
        assert_eq!(
            name_from_url("https://x.com/shows/My%20Show/master.m3u8?t=1"),
            "My Show"
        );
        assert_eq!(name_from_url("https://x.com/v/clip.m3u8"), "clip");
        assert_eq!(name_from_url("https://www.x.com/index.m3u8"), "x.com");
        assert_eq!(output_name(None, "https://x.com/v/clip.mpd"), "clip.mp4");
        assert_eq!(
            output_name(Some("My: video"), "https://x.com/a.m3u8"),
            "My_ video.mp4"
        );
        assert_eq!(
            output_name(Some("done.MP4"), "https://x.com/a.m3u8"),
            "done.MP4"
        );
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            unique_path(dir.path(), "a", ".mp4"),
            dir.path().join("a.mp4")
        );
        std::fs::write(dir.path().join("a.mp4"), b"x").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a", ".mp4"),
            dir.path().join("a (1).mp4")
        );
    }
}
