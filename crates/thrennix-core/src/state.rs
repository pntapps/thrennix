//! `<final>.thrennix` resume state (JSON), saved atomically.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::names::with_suffix;

pub const STATE_VERSION: u32 = 1;

/// One range in the state file. `cursor` = bytes durably written (`end` exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentState {
    pub start: u64,
    pub cursor: u64,
    pub end: u64,
}

/// Contents of a `.thrennix` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadState {
    #[serde(default)]
    pub version: u32,
    pub url: String,
    pub final_url: String,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub file_name: String,
    /// False for single-stream downloads (resume restarts from zero).
    pub resumable: bool,
    pub segments: Vec<SegmentState>,
    /// Request context needed to resume authenticated downloads.
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

impl DownloadState {
    /// Bytes already on disk according to the state.
    pub fn downloaded(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| s.cursor.saturating_sub(s.start))
            .sum()
    }

    /// Load a state file. Secrets protected with DPAPI are decrypted (plaintext values from
    /// older files are accepted as-is); a secret that cannot be decrypted is dropped.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let data = std::fs::read(path)?;
        let mut st: Self = serde_json::from_slice(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        st.cookies = st.cookies.as_deref().and_then(secret::unprotect);
        st.headers = std::mem::take(&mut st.headers)
            .into_iter()
            .filter_map(|(k, v)| {
                if secret::is_secret_header(&k) {
                    secret::unprotect(&v).map(|v| (k, v))
                } else {
                    Some((k, v))
                }
            })
            .collect();
        Ok(st)
    }

    /// Write to `<path>.tmp` then rename over `path`. On Windows the cookie value and any
    /// `Cookie`/`Authorization` headers are encrypted with DPAPI (current user).
    pub fn save_atomic(&self, path: &Path) -> std::io::Result<()> {
        let mut on_disk = self.clone();
        on_disk.cookies = on_disk.cookies.as_deref().map(secret::protect);
        for (k, v) in &mut on_disk.headers {
            if secret::is_secret_header(k) {
                *v = secret::protect(v);
            }
        }
        let tmp = with_suffix(path, ".tmp");
        let data = serde_json::to_vec_pretty(&on_disk)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, data)?;
        crate::fsutil::rename_with_retry(&tmp, path)
    }

    /// Segments are consistent: inside `[0,total)`, cursor within range.
    pub fn is_consistent(&self) -> bool {
        let Some(total) = self.total else {
            return !self.resumable;
        };
        self.segments
            .iter()
            .all(|s| s.start <= s.cursor && s.cursor <= s.end && s.end <= total)
    }
}

/// Kept for compatibility; see [`crate::fsutil::rename_with_retry`].
pub fn rename_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::fsutil::rename_with_retry(from, to)
}

mod secret {
    use base64::Engine as _;

    const PREFIX: &str = "dpapi:";

    pub fn is_secret_header(name: &str) -> bool {
        let n = name.trim();
        n.eq_ignore_ascii_case("cookie")
            || n.eq_ignore_ascii_case("authorization")
            || n.eq_ignore_ascii_case("proxy-authorization")
    }

    /// Encrypt for the current user (Windows); plaintext elsewhere or on failure.
    pub fn protect(s: &str) -> String {
        match dpapi(s.as_bytes(), true) {
            Some(enc) => format!(
                "{PREFIX}{}",
                base64::engine::general_purpose::STANDARD.encode(enc)
            ),
            None => s.to_string(),
        }
    }

    pub fn unprotect(s: &str) -> Option<String> {
        let Some(b64) = s.strip_prefix(PREFIX) else {
            return Some(s.to_string());
        };
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        match dpapi(&raw, false).and_then(|d| String::from_utf8(d).ok()) {
            Some(v) => Some(v),
            None => {
                tracing::warn!("could not decrypt a saved credential; dropping it");
                None
            }
        }
    }

    #[cfg(windows)]
    fn dpapi(data: &[u8], encrypt: bool) -> Option<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(data.len()).ok()?,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: input points to `data` for the duration of the call; on success `out` is a
        // LocalAlloc'd buffer that we copy and free.
        let ok = unsafe {
            if encrypt {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            }
        };
        if ok == 0 || out.pbData.is_null() {
            return None;
        }
        // SAFETY: DPAPI returned `cbData` valid bytes at `pbData`.
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData as _) };
        Some(v)
    }

    #[cfg(not(windows))]
    fn dpapi(_data: &[u8], _encrypt: bool) -> Option<Vec<u8>> {
        None
    }
}

pub fn state_path(final_path: &Path) -> PathBuf {
    with_suffix(final_path, ".thrennix")
}

pub fn part_path(final_path: &Path) -> PathBuf {
    with_suffix(final_path, ".part")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("thrennix-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("file.bin.thrennix");
        let st = DownloadState {
            version: STATE_VERSION,
            url: "https://example.com/file.bin".into(),
            final_url: "https://cdn.example.com/file.bin".into(),
            total: Some(1000),
            etag: Some("\"e\"".into()),
            last_modified: None,
            file_name: "file.bin".into(),
            resumable: true,
            segments: vec![
                SegmentState {
                    start: 0,
                    cursor: 100,
                    end: 500,
                },
                SegmentState {
                    start: 500,
                    cursor: 1000,
                    end: 1000,
                },
            ],
            cookies: Some("a=b".into()),
            referer: None,
            user_agent: None,
            headers: vec![
                ("X".into(), "y".into()),
                ("Authorization".into(), "Bearer t0k".into()),
            ],
        };
        st.save_atomic(&p).unwrap();
        // Overwrite works too (atomic replace).
        st.save_atomic(&p).unwrap();
        let back = DownloadState::load(&p).unwrap();
        assert_eq!(back, st);
        assert_eq!(back.downloaded(), 600);
        assert!(back.is_consistent());
        assert!(!with_suffix(&p, ".tmp").exists());
        if cfg!(windows) {
            let raw = std::fs::read_to_string(&p).unwrap();
            assert!(
                !raw.contains("a=b"),
                "cookie must not be stored in plaintext"
            );
            assert!(raw.contains("dpapi:"));
            assert!(
                !raw.contains("t0k"),
                "auth header must not be stored in plaintext"
            );
        }
        // Old plaintext files still load.
        let mut plain = serde_json::to_value(&st).unwrap();
        plain["cookies"] = serde_json::json!("legacy=1");
        std::fs::write(&p, serde_json::to_vec(&plain).unwrap()).unwrap();
        assert_eq!(
            DownloadState::load(&p).unwrap().cookies.as_deref(),
            Some("legacy=1")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("thrennix-state-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.thrennix");
        std::fs::write(&p, b"{not json").unwrap();
        assert!(DownloadState::load(&p).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
