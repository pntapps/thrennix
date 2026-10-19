//! Format-independent description of what has to be fetched.

use url::Url;

use crate::{Error, Quality, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

/// Standard HLS AES-128 (full-segment CBC, PKCS#7) key reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Aes128Key {
    pub uri: String,
    pub iv: [u8; 16],
}

/// One resource (init section or media segment) of a track, in playback order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub url: String,
    pub range: Option<ByteRange>,
    pub key: Option<Aes128Key>,
    pub init: bool,
}

impl Item {
    pub fn plain(url: String) -> Self {
        Item {
            url,
            range: None,
            key: None,
            init: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Track {
    pub items: Vec<Item>,
    /// Extension of the concatenated file: ".ts", ".mp4", ".aac", ".webm", ...
    pub ext: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub video: Track,
    /// Separate audio rendition, if the chosen video does not carry audio itself.
    pub audio: Option<Track>,
}

pub(crate) fn drm_error() -> Error {
    Error::Other("DRM-protected stream".into())
}

pub(crate) fn live_error() -> Error {
    Error::Other("live streams are not supported yet".into())
}

pub(crate) fn resolve(base: &Url, reference: &str) -> Result<String> {
    base.join(reference.trim())
        .map(|u| u.to_string())
        .map_err(|e| Error::Other(format!("bad URL {reference:?}: {e}")))
}

/// Pick a rendition. `cands` are `(height, bandwidth)` pairs.
///
/// `Best` takes the highest bandwidth (ties broken by height). `MaxHeight(h)` takes the
/// highest-bandwidth rendition whose height is <= h; renditions without a known height
/// are only considered if no rendition reports one; if nothing fits, the smallest is used.
pub(crate) fn select_index(cands: &[(Option<u32>, u64)], quality: Quality) -> Option<usize> {
    if cands.is_empty() {
        return None;
    }
    let best_of = |idx: &mut dyn Iterator<Item = usize>| {
        idx.max_by_key(|&i| (cands[i].1, cands[i].0.unwrap_or(0)))
    };
    match quality {
        Quality::Best => best_of(&mut (0..cands.len())),
        Quality::MaxHeight(max) => {
            let any_height = cands.iter().any(|c| c.0.is_some());
            if !any_height {
                return best_of(&mut (0..cands.len()));
            }
            let fitting = best_of(
                &mut (0..cands.len()).filter(|&i| matches!(cands[i].0, Some(h) if h <= max)),
            );
            fitting.or_else(|| {
                (0..cands.len())
                    .filter(|&i| cands[i].0.is_some())
                    .min_by_key(|&i| (cands[i].0.unwrap_or(u32::MAX), cands[i].1))
            })
        }
    }
}

/// Lowercased extension of a URL's last path segment (without the dot).
pub(crate) fn url_ext(url: &str) -> String {
    let path = Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.to_string());
    let last = path.rsplit('/').next().unwrap_or("");
    match last.rfind('.') {
        Some(i) => last[i + 1..].to_ascii_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_best_and_max_height() {
        let c = [
            (Some(360), 800_000),
            (Some(1080), 5_000_000),
            (Some(720), 2_500_000),
            (Some(720), 3_000_000),
        ];
        assert_eq!(select_index(&c, Quality::Best), Some(1));
        assert_eq!(select_index(&c, Quality::MaxHeight(720)), Some(3));
        assert_eq!(select_index(&c, Quality::MaxHeight(480)), Some(0));
        // Nothing fits: smallest rendition.
        assert_eq!(select_index(&c, Quality::MaxHeight(144)), Some(0));
        // No heights at all: bandwidth decides.
        let n = [(None, 1), (None, 3), (None, 2)];
        assert_eq!(select_index(&n, Quality::MaxHeight(480)), Some(1));
        assert_eq!(select_index(&[], Quality::Best), None);
    }

    #[test]
    fn ext_of_url() {
        assert_eq!(url_ext("https://a/b/seg1.TS?x=1.mp4"), "ts");
        assert_eq!(url_ext("https://a/b/noext"), "");
    }
}
