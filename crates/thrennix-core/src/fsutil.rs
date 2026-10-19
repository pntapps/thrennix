//! File-system helpers shared with other crates (rename retry, Mark-of-the-Web).

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long [`rename_with_retry`] keeps retrying transient sharing errors.
pub const RENAME_RETRY_FOR: Duration = Duration::from_secs(10);

/// Errors caused by another process briefly holding the file (antivirus scanners,
/// Explorer previewers, indexers): `ERROR_ACCESS_DENIED` (5), `ERROR_SHARING_VIOLATION` (32),
/// `ERROR_LOCK_VIOLATION` (33).
pub fn is_transient_sharing_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(5 | 32 | 33)) || e.kind() == io::ErrorKind::PermissionDenied
}

/// `std::fs::rename` (replaces an existing target), retrying for up to ~10 s while the
/// failure is a transient sharing violation / access denied. Other errors fail immediately.
pub fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    rename_with_retry_for(from, to, RENAME_RETRY_FOR)
}

/// [`rename_with_retry`] with a custom retry budget.
pub fn rename_with_retry_for(from: &Path, to: &Path, budget: Duration) -> io::Result<()> {
    let started = Instant::now();
    let mut delay = Duration::from_millis(25);
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if is_transient_sharing_error(&e) && started.elapsed() < budget => {
                tracing::debug!(
                    "rename {} -> {} failed ({e}); retrying",
                    from.display(),
                    to.display()
                );
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Write the `Zone.Identifier` alternate data stream (Mark-of-the-Web, internet zone) so
/// SmartScreen / Office Protected View treat the file like a browser download.
/// Errors are ignored (FAT32/exFAT have no ADS). No-op off Windows.
pub fn write_mark_of_the_web(path: &Path, host_url: &str, referrer: Option<&str>) {
    if !cfg!(windows) {
        return;
    }
    let mut s = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referrer.filter(|r| !r.trim().is_empty()) {
        s.push_str(&format!("ReferrerUrl={}\r\n", one_line(r)));
    }
    s.push_str(&format!("HostUrl={}\r\n", one_line(host_url)));
    let mut ads = path.as_os_str().to_os_string();
    ads.push(":Zone.Identifier");
    if let Err(e) = std::fs::write(&ads, s) {
        tracing::debug!(
            "could not write Zone.Identifier for {}: {e}",
            path.display()
        );
    }
}

fn one_line(s: &str) -> String {
    s.chars().filter(|c| *c != '\r' && *c != '\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_replaces_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!("thrennix-fsutil-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(&b, b"old").unwrap();
        rename_with_retry(&a, &b).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"new");
        // Missing source is not transient: fails immediately.
        let t = Instant::now();
        assert!(rename_with_retry(&a, &b).is_err());
        assert!(t.elapsed() < Duration::from_secs(2));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn mark_of_the_web_written() {
        let dir = std::env::temp_dir().join(format!("thrennix-motw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.exe");
        std::fs::write(&f, b"MZ").unwrap();
        write_mark_of_the_web(&f, "https://cdn.example/x.exe", Some("https://example/"));
        let mut ads = f.as_os_str().to_os_string();
        ads.push(":Zone.Identifier");
        let z = std::fs::read_to_string(&ads).unwrap();
        assert!(z.starts_with("[ZoneTransfer]\r\nZoneId=3\r\n"));
        assert!(z.contains("ReferrerUrl=https://example/\r\n"));
        assert!(z.contains("HostUrl=https://cdn.example/x.exe\r\n"));
        assert_eq!(std::fs::read(&f).unwrap(), b"MZ");
        std::fs::remove_dir_all(&dir).ok();
    }
}
