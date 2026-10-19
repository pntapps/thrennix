//! Resume bookkeeping: `state.json` inside the work dir lists finished segment files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::Plan;

const VERSION: u32 = 1;
const SAVE_EVERY: Duration = Duration::from_millis(1000);

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    url: String,
    /// Identifies the segment layout; a mismatch means the stream changed and we start over.
    fingerprint: String,
    /// Finished segment file name → bytes received for it.
    done: BTreeMap<String, u64>,
}

pub(crate) struct Tracker {
    path: PathBuf,
    state: Mutex<StateFile>,
    last_save: Mutex<Instant>,
}

/// FNV-1a over the segment layout (URL paths without query strings, so refreshed tokens
/// in signed URLs still resume).
pub(crate) fn fingerprint(plan: &Plan) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    for (tag, track) in [("v", Some(&plan.video)), ("a", plan.audio.as_ref())] {
        let Some(track) = track else { continue };
        feed(tag.as_bytes());
        feed(track.ext.as_bytes());
        feed(&(track.items.len() as u64).to_le_bytes());
        for it in &track.items {
            let path = it.url.split(['?', '#']).next().unwrap_or("");
            feed(path.as_bytes());
            if let Some(r) = &it.range {
                feed(&r.offset.to_le_bytes());
                feed(&r.length.to_le_bytes());
            }
            feed(&[it.init as u8, it.key.is_some() as u8]);
        }
    }
    format!("{h:016x}")
}

fn load(work_dir: &Path, url: &str, fingerprint: &str) -> Option<StateFile> {
    let bytes = std::fs::read(work_dir.join("state.json")).ok()?;
    serde_json::from_slice::<StateFile>(&bytes)
        .ok()
        .filter(|s| s.version == VERSION && s.fingerprint == fingerprint && s.url == url)
}

/// Whether `work_dir` holds the state of this very download (same URL and segment layout).
pub(crate) fn matches(work_dir: &Path, url: &str, fingerprint: &str) -> bool {
    load(work_dir, url, fingerprint).is_some()
}

impl Tracker {
    /// Load `state.json` from `work_dir` if it belongs to this download, otherwise start a
    /// fresh state. The caller owns (has reserved and locked) `work_dir`.
    pub fn open(work_dir: &Path, url: &str, fingerprint: String) -> std::io::Result<Tracker> {
        let path = work_dir.join("state.json");
        std::fs::create_dir_all(work_dir)?;
        let state = match load(work_dir, url, &fingerprint) {
            Some(mut s) => {
                // Only trust entries whose segment file is still on disk.
                s.done.retain(|file, _| work_dir.join(file).is_file());
                s
            }
            None => StateFile {
                version: VERSION,
                url: url.to_string(),
                fingerprint,
                done: BTreeMap::new(),
            },
        };
        let t = Tracker {
            path,
            state: Mutex::new(state),
            last_save: Mutex::new(Instant::now()),
        };
        t.save()?;
        Ok(t)
    }

    pub fn done_bytes(&self, file: &str) -> Option<u64> {
        self.state.lock().unwrap().done.get(file).copied()
    }

    pub fn mark_done(&self, file: &str, bytes: u64) {
        self.state
            .lock()
            .unwrap()
            .done
            .insert(file.to_string(), bytes);
    }

    /// Atomic write: temp file + rename.
    pub fn save(&self) -> std::io::Result<()> {
        let json = {
            let s = self.state.lock().unwrap();
            serde_json::to_vec_pretty(&*s).map_err(std::io::Error::other)?
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        crate::util::rename_with_retry_blocking(&tmp, &self.path, 10)?;
        *self.last_save.lock().unwrap() = Instant::now();
        Ok(())
    }

    pub fn maybe_save(&self) {
        let due = self.last_save.lock().unwrap().elapsed() >= SAVE_EVERY;
        if due {
            if let Err(e) = self.save() {
                tracing::warn!("could not save media state: {e}");
            }
        }
    }
}
