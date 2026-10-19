//! Per-download output-name reservation and work-dir locking.
//!
//! For stem `name` a download owns:
//! - `name.mp4.thrennix-reserve`: empty placeholder so no other download picks `name.mp4`,
//! - `name.thrennix-media/`: the work dir, with `lock` held open exclusively while running.
//!
//! A work dir that is locked, or whose `state.json` belongs to another download, is never
//! touched; the next free name (`name (1)`, ...) is used instead.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::state;

pub(crate) const PLACEHOLDER_SUFFIX: &str = ".thrennix-reserve";

#[derive(Debug)]
pub(crate) struct Reservation {
    pub stem: String,
    pub output: PathBuf,
    pub work_dir: PathBuf,
    pub placeholder: PathBuf,
    lock: Option<File>,
}

impl Reservation {
    /// Release the work-dir lock (needed before the dir can be deleted on Windows).
    pub fn unlock(&mut self) {
        self.lock = None;
    }
}

/// Exclusively open `<dir>/lock`. `None` if another job holds it.
fn try_lock(dir: &Path) -> Option<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    opts.open(dir.join("lock")).ok()
}

/// `try_lock` with a few short retries, for a dir we just created (another job may be
/// peeking at it for a moment).
fn lock_new_dir(dir: &Path) -> Option<File> {
    for _ in 0..20 {
        if let Some(f) = try_lock(dir) {
            return Some(f);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn candidate(stem: &str, i: u32) -> String {
    if i == 0 {
        stem.to_string()
    } else {
        format!("{stem} ({i})")
    }
}

/// Find this download's own (resumable) work dir, or reserve a fresh, unused name.
pub(crate) fn reserve(
    dest: &Path,
    stem: &str,
    url: &str,
    fingerprint: &str,
) -> io::Result<Reservation> {
    std::fs::create_dir_all(dest)?;
    // Pass 1: resume our own work dir if one exists under any candidate name.
    // Pass 2: take the first completely unused name.
    for pass in 0..2 {
        let mut gaps = 0;
        for i in 0..10_000u32 {
            let s = candidate(stem, i);
            let output = dest.join(format!("{s}.mp4"));
            let work_dir = dest.join(format!("{s}.thrennix-media"));
            let placeholder = dest.join(format!("{s}.mp4{PLACEHOLDER_SUFFIX}"));
            if work_dir.is_dir() {
                let Some(lock) = try_lock(&work_dir) else {
                    continue;
                };
                if state::matches(&work_dir, url, fingerprint) && !output.exists() {
                    // Our own paused/failed download: keep (or restore) the placeholder.
                    let _ = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&placeholder);
                    return Ok(Reservation {
                        stem: s,
                        output,
                        work_dir,
                        placeholder,
                        lock: Some(lock),
                    });
                }
                continue; // another download's work dir: leave it alone
            }
            if pass == 0 {
                // Names are handed out in order, so after a run of unused names there is
                // no older work dir of ours left to find.
                if !output.exists() && !placeholder.exists() {
                    gaps += 1;
                    if gaps > 32 {
                        break;
                    }
                }
                continue;
            }
            if output.exists() || placeholder.exists() {
                continue;
            }
            match std::fs::create_dir(&work_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            let Some(lock) = lock_new_dir(&work_dir) else {
                continue;
            };
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&placeholder)
            {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    drop(lock);
                    let _ = std::fs::remove_dir_all(&work_dir);
                    continue;
                }
                Err(e) => {
                    drop(lock);
                    let _ = std::fs::remove_dir_all(&work_dir);
                    return Err(e);
                }
            }
            if output.exists() {
                // Lost a race with something writing the output name directly.
                drop(lock);
                let _ = std::fs::remove_dir_all(&work_dir);
                let _ = std::fs::remove_file(&placeholder);
                continue;
            }
            return Ok(Reservation {
                stem: s,
                output,
                work_dir,
                placeholder,
                lock: Some(lock),
            });
        }
    }
    Err(io::Error::other("no free output file name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)] // exclusive lock files are a Windows feature here
    fn fresh_then_collision_then_resume() {
        let dir = tempfile::tempdir().unwrap();
        let a = reserve(dir.path(), "x", "http://a/1", "fp1").unwrap();
        assert_eq!(a.output, dir.path().join("x.mp4"));
        assert!(a.placeholder.is_file() && a.work_dir.is_dir());

        // Same name, while `a` holds its lock: next name.
        let b = reserve(dir.path(), "x", "http://a/1", "fp1").unwrap();
        assert_eq!(b.output, dir.path().join("x (1).mp4"));

        // `a` pauses (lock released) with saved state; a different download must not take it.
        crate::state::Tracker::open(&a.work_dir, "http://a/1", "fp1".into()).unwrap();
        drop(a);
        let c = reserve(dir.path(), "x", "http://other/2", "fp2").unwrap();
        assert_eq!(c.output, dir.path().join("x (2).mp4"));
        assert!(dir
            .path()
            .join("x.thrennix-media")
            .join("state.json")
            .is_file());

        // Resuming the first download finds its own dir again.
        let a2 = reserve(dir.path(), "x", "http://a/1", "fp1").unwrap();
        assert_eq!(a2.output, dir.path().join("x.mp4"));
        drop((b, c, a2));
    }

    #[test]
    fn existing_output_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("y.mp4"), b"done").unwrap();
        let r = reserve(dir.path(), "y", "u", "f").unwrap();
        assert_eq!(r.output, dir.path().join("y (1).mp4"));
    }
}
