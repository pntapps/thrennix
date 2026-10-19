//! HLS playlist parsing (RFC 8216).

use url::Url;

use crate::model::{drm_error, resolve, url_ext, Aes128Key, ByteRange, Item, Track};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HlsVariant {
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub uri: String,
    pub audio_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HlsRendition {
    pub media_type: String,
    pub group_id: String,
    pub name: Option<String>,
    pub language: Option<String>,
    pub uri: Option<String>,
    pub default: bool,
    pub autoselect: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MasterPlaylist {
    pub variants: Vec<HlsVariant>,
    pub renditions: Vec<HlsRendition>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MediaPlaylist {
    /// Init sections (`init == true`) and media segments, in order.
    pub items: Vec<Item>,
    pub media_sequence: u64,
    pub ended: bool,
}

impl MediaPlaylist {
    pub fn into_track(self) -> Track {
        let ext = if self.items.iter().any(|i| i.init) {
            ".mp4"
        } else {
            match self.items.first().map(|i| url_ext(&i.url)).as_deref() {
                Some("aac") => ".aac",
                Some("mp3") => ".mp3",
                Some("mp4" | "m4s" | "m4a" | "m4v" | "cmfv" | "cmfa") => ".mp4",
                _ => ".ts",
            }
        };
        Track {
            items: self.items,
            ext,
        }
    }
}

/// Parse an attribute list: `A=1,B="x,y",C=0x10`. Keys are upper-cased, quotes stripped.
pub(crate) fn parse_attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c == ',' {
                break;
            }
            key.push(c);
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'=') {
            chars.next();
            if chars.peek() == Some(&'"') {
                chars.next();
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    value.push(c);
                }
                // Skip anything up to the next separator.
                while let Some(&c) = chars.peek() {
                    if c == ',' {
                        break;
                    }
                    chars.next();
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == ',' {
                        break;
                    }
                    value.push(c);
                    chars.next();
                }
            }
        }
        out.push((key.trim().to_ascii_uppercase(), value.trim().to_string()));
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
}

fn check_header(text: &str) -> Result<()> {
    match lines(text).next() {
        Some(l) if l.starts_with("#EXTM3U") => Ok(()),
        _ => Err(Error::Other("not an HLS playlist (missing #EXTM3U)".into())),
    }
}

pub(crate) fn is_master(text: &str) -> bool {
    lines(text).any(|l| l.starts_with("#EXT-X-STREAM-INF"))
}

/// `len[@offset]`
fn parse_byterange(s: &str) -> Option<(u64, Option<u64>)> {
    let s = s.trim();
    match s.split_once('@') {
        Some((l, o)) => Some((l.trim().parse().ok()?, Some(o.trim().parse().ok()?))),
        None => Some((s.parse().ok()?, None)),
    }
}

/// A non-empty range whose end fits in u64.
fn checked_range(offset: u64, length: u64, line: &str) -> Result<ByteRange> {
    if length == 0 || offset.checked_add(length).is_none() {
        return Err(Error::Other(format!("bad byte range in {line}")));
    }
    Ok(ByteRange { offset, length })
}

fn parse_iv(s: &str) -> Result<[u8; 16]> {
    let hex = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.is_empty() || hex.len() > 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Other(format!("invalid IV {s:?}")));
    }
    let padded = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for (i, b) in iv.iter_mut().enumerate() {
        *b = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).expect("validated hex");
    }
    Ok(iv)
}

fn sequence_iv(seq: u64) -> [u8; 16] {
    (seq as u128).to_be_bytes()
}

#[derive(Debug, Clone)]
struct KeyState {
    uri: String,
    iv: Option<[u8; 16]>,
}

/// Interpret an `#EXT-X-KEY` / `#EXT-X-SESSION-KEY` attribute list. Returns `None` for
/// METHOD=NONE, a key for standard AES-128 and a DRM error for anything else.
fn parse_key(attrs: &[(String, String)], base: &Url) -> Result<Option<KeyState>> {
    let method = attr(attrs, "METHOD").unwrap_or("NONE").to_ascii_uppercase();
    if method == "NONE" {
        return Ok(None);
    }
    if method != "AES-128" {
        // SAMPLE-AES, SAMPLE-AES-CTR, SAMPLE-AES-CENC, ISO-23001-7 ... are all DRM schemes.
        return Err(drm_error());
    }
    if let Some(fmt) = attr(attrs, "KEYFORMAT") {
        if !fmt.eq_ignore_ascii_case("identity") {
            return Err(drm_error());
        }
    }
    let uri = attr(attrs, "URI").ok_or_else(|| Error::Other("EXT-X-KEY without URI".into()))?;
    if uri.to_ascii_lowercase().starts_with("skd://") {
        return Err(drm_error());
    }
    let iv = attr(attrs, "IV").map(parse_iv).transpose()?;
    Ok(Some(KeyState {
        uri: resolve(base, uri)?,
        iv,
    }))
}

pub(crate) fn parse_master(text: &str, base: &Url) -> Result<MasterPlaylist> {
    check_header(text)?;
    let mut m = MasterPlaylist::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in lines(text) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attributes(rest));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = parse_attributes(rest);
            let yes = |k: &str| attr(&a, k).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
            m.renditions.push(HlsRendition {
                media_type: attr(&a, "TYPE").unwrap_or("").to_ascii_uppercase(),
                group_id: attr(&a, "GROUP-ID").unwrap_or("").to_string(),
                name: attr(&a, "NAME").map(str::to_string),
                language: attr(&a, "LANGUAGE").map(str::to_string),
                uri: attr(&a, "URI").map(|u| resolve(base, u)).transpose()?,
                default: yes("DEFAULT"),
                autoselect: yes("AUTOSELECT"),
            });
        } else if let Some(rest) = line.strip_prefix("#EXT-X-SESSION-KEY:") {
            parse_key(&parse_attributes(rest), base)?;
        } else if line.starts_with('#') {
            continue;
        } else if let Some(a) = pending.take() {
            let (width, height) =
                match attr(&a, "RESOLUTION").and_then(|r| r.split_once(['x', 'X'])) {
                    Some((w, h)) => (w.trim().parse().ok(), h.trim().parse().ok()),
                    None => (None, None),
                };
            let bandwidth = attr(&a, "BANDWIDTH")
                .or_else(|| attr(&a, "AVERAGE-BANDWIDTH"))
                .and_then(|b| b.parse().ok())
                .unwrap_or(0);
            m.variants.push(HlsVariant {
                bandwidth,
                width,
                height,
                codecs: attr(&a, "CODECS").map(str::to_string),
                uri: resolve(base, line)?,
                audio_group: attr(&a, "AUDIO").map(str::to_string),
            });
        }
    }
    Ok(m)
}

impl MasterPlaylist {
    /// The audio rendition to download alongside `variant`, if it lives in a separate playlist.
    pub fn audio_for(&self, variant: &HlsVariant) -> Option<&HlsRendition> {
        let group = variant.audio_group.as_deref()?;
        let cands: Vec<&HlsRendition> = self
            .renditions
            .iter()
            .filter(|r| r.media_type == "AUDIO" && r.group_id == group && r.uri.is_some())
            .collect();
        cands
            .iter()
            .find(|r| r.default)
            .or_else(|| cands.iter().find(|r| r.autoselect))
            .or_else(|| cands.first())
            .copied()
            // A rendition whose URI equals the variant's is already muxed in.
            .filter(|r| r.uri.as_deref() != Some(variant.uri.as_str()))
    }
}

const MAX_ITEMS: usize = 500_000;

pub(crate) fn parse_media(text: &str, base: &Url) -> Result<MediaPlaylist> {
    check_header(text)?;
    let mut pl = MediaPlaylist::default();
    let mut key: Option<KeyState> = None;
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    let mut last_end: Option<(String, u64)> = None;
    let mut seg_index: u64 = 0;
    let mut last_map: Option<Item> = None;
    let mut vod = false;

    for line in lines(text) {
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            pl.media_sequence = rest
                .trim()
                .parse()
                .map_err(|_| Error::Other("bad EXT-X-MEDIA-SEQUENCE".into()))?;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            key = parse_key(&parse_attributes(rest), base)?;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range =
                Some(parse_byterange(rest).ok_or_else(|| Error::Other(format!("bad {line}")))?);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attributes(rest);
            let uri =
                attr(&a, "URI").ok_or_else(|| Error::Other("EXT-X-MAP without URI".into()))?;
            let range = match attr(&a, "BYTERANGE") {
                Some(r) => {
                    let (length, off) =
                        parse_byterange(r).ok_or_else(|| Error::Other(format!("bad {line}")))?;
                    Some(checked_range(off.unwrap_or(0), length, line)?)
                }
                None => None,
            };
            let item = Item {
                url: resolve(base, uri)?,
                range,
                key: key.as_ref().map(|k| Aes128Key {
                    uri: k.uri.clone(),
                    iv: k
                        .iv
                        .unwrap_or_else(|| sequence_iv(pl.media_sequence.wrapping_add(seg_index))),
                }),
                init: true,
            };
            if last_map.as_ref() != Some(&item) {
                last_map = Some(item.clone());
                pl.items.push(item);
            }
        } else if line.starts_with("#EXT-X-ENDLIST") {
            pl.ended = true;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            vod = rest.trim().eq_ignore_ascii_case("VOD");
        } else if line.starts_with('#') {
            // #EXTINF, #EXT-X-DISCONTINUITY, #EXT-X-TARGETDURATION, comments ...
            continue;
        } else {
            let url = resolve(base, line)?;
            let range = match pending_range.take() {
                Some((length, Some(offset))) => Some(checked_range(offset, length, line)?),
                Some((length, None)) => {
                    let offset = match &last_end {
                        Some((u, end)) if *u == url => *end,
                        _ => 0,
                    };
                    Some(checked_range(offset, length, line)?)
                }
                None => None,
            };
            last_end = range.as_ref().map(|r| (url.clone(), r.offset + r.length)); // checked in checked_range
            let seq = pl.media_sequence.wrapping_add(seg_index);
            pl.items.push(Item {
                url,
                range,
                key: key.as_ref().map(|k| Aes128Key {
                    uri: k.uri.clone(),
                    iv: k.iv.unwrap_or_else(|| sequence_iv(seq)),
                }),
                init: false,
            });
            seg_index += 1;
            if pl.items.len() > MAX_ITEMS {
                return Err(Error::Other("playlist has too many segments".into()));
            }
        }
    }
    pl.ended |= vod;
    if seg_index == 0 && pl.ended {
        return Err(Error::Other("playlist has no segments".into()));
    }
    Ok(pl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::select_index;
    use crate::Quality;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/video/master.m3u8?token=1").unwrap()
    }

    const MASTER: &str = "#EXTM3U
#EXT-X-VERSION:6
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/en.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"French\",LANGUAGE=\"fr\",URI=\"audio/fr.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"
360p/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aud\"
/abs/1080p.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"aud\"
https://other.example.com/720p.m3u8
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=100000,URI=\"iframes.m3u8\"
";

    #[test]
    fn attributes_with_quoted_commas() {
        let a = parse_attributes("BANDWIDTH=1,CODECS=\"avc1.4d401e,mp4a.40.2\",RESOLUTION=640x360");
        assert_eq!(attr(&a, "CODECS"), Some("avc1.4d401e,mp4a.40.2"));
        assert_eq!(attr(&a, "RESOLUTION"), Some("640x360"));
        assert_eq!(attr(&a, "BANDWIDTH"), Some("1"));
    }

    #[test]
    fn master_playlist() {
        assert!(is_master(MASTER));
        let m = parse_master(MASTER, &base()).unwrap();
        assert_eq!(m.variants.len(), 3);
        let v = &m.variants[0];
        assert_eq!(v.bandwidth, 800_000);
        assert_eq!((v.width, v.height), (Some(640), Some(360)));
        assert_eq!(v.codecs.as_deref(), Some("avc1.4d401e,mp4a.40.2"));
        assert_eq!(v.uri, "https://cdn.example.com/video/360p/index.m3u8");
        assert_eq!(m.variants[1].uri, "https://cdn.example.com/abs/1080p.m3u8");
        assert_eq!(m.variants[2].uri, "https://other.example.com/720p.m3u8");
        assert_eq!(m.renditions.len(), 2);
        let a = m.audio_for(v).unwrap();
        assert_eq!(a.name.as_deref(), Some("English"));
        assert_eq!(
            a.uri.as_deref(),
            Some("https://cdn.example.com/video/audio/en.m3u8")
        );

        let cands: Vec<_> = m.variants.iter().map(|v| (v.height, v.bandwidth)).collect();
        assert_eq!(select_index(&cands, Quality::Best), Some(1));
        assert_eq!(select_index(&cands, Quality::MaxHeight(720)), Some(2));
        assert_eq!(select_index(&cands, Quality::MaxHeight(719)), Some(0));
    }

    #[test]
    fn media_playlist_basic_and_relative() {
        let text = "#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:10
#EXTINF:4.0,
seg10.ts
#EXTINF:4.0,
../other/seg11.ts?x=1
#EXTINF:2.0,
https://abs.example.com/seg12.ts
#EXT-X-ENDLIST
";
        let p = parse_media(text, &Url::parse("https://h.com/a/b/index.m3u8").unwrap()).unwrap();
        assert!(p.ended);
        assert_eq!(p.media_sequence, 10);
        let urls: Vec<_> = p.items.iter().map(|i| i.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://h.com/a/b/seg10.ts",
                "https://h.com/a/other/seg11.ts?x=1",
                "https://abs.example.com/seg12.ts"
            ]
        );
        assert_eq!(p.into_track().ext, ".ts");
    }

    #[test]
    fn media_playlist_live_has_no_endlist() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\na.ts\n";
        assert!(!parse_media(text, &base()).unwrap().ended);
        let vod = "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4,\na.ts\n";
        assert!(parse_media(vod, &base()).unwrap().ended);
    }

    #[test]
    fn media_playlist_byterange_and_map() {
        let text = "#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"720@0\"
#EXTINF:4.0,
#EXT-X-BYTERANGE:1000@720
main.mp4
#EXTINF:4.0,
#EXT-X-BYTERANGE:500
main.mp4
#EXTINF:4.0,
#EXT-X-BYTERANGE:300
main.mp4
#EXT-X-ENDLIST
";
        let p = parse_media(text, &base()).unwrap();
        let ranges: Vec<_> = p
            .items
            .iter()
            .map(|i| i.range.clone().map(|r| (r.offset, r.length)))
            .collect();
        assert_eq!(
            ranges,
            [
                Some((0, 720)),
                Some((720, 1000)),
                Some((1720, 500)),
                Some((2220, 300))
            ]
        );
        assert!(p.items[0].init);
        assert!(!p.items[1].init);
        assert_eq!(p.items[0].url, "https://cdn.example.com/video/main.mp4");
        assert_eq!(p.into_track().ext, ".mp4");
    }

    #[test]
    fn media_playlist_aes128() {
        let text = "#EXTM3U
#EXT-X-MEDIA-SEQUENCE:5
#EXT-X-KEY:METHOD=AES-128,URI=\"keys/k1.bin\"
#EXTINF:4,
s5.ts
#EXTINF:4,
s6.ts
#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example.com/k2\",IV=0x000102030405060708090A0B0C0D0E0F,KEYFORMAT=\"identity\"
#EXTINF:4,
s7.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:4,
s8.ts
#EXT-X-ENDLIST
";
        let p = parse_media(text, &base()).unwrap();
        let k0 = p.items[0].key.as_ref().unwrap();
        assert_eq!(k0.uri, "https://cdn.example.com/video/keys/k1.bin");
        let mut iv5 = [0u8; 16];
        iv5[15] = 5;
        assert_eq!(k0.iv, iv5);
        assert_eq!(p.items[1].key.as_ref().unwrap().iv[15], 6);
        let k2 = p.items[2].key.as_ref().unwrap();
        assert_eq!(k2.uri, "https://keys.example.com/k2");
        assert_eq!(
            k2.iv,
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert!(p.items[3].key.is_none());
        // Short IVs are left-padded.
        assert_eq!(parse_iv("0x1").unwrap()[15], 1);
        assert!(parse_iv("0xZZ").is_err());
    }

    fn is_drm(r: Result<MediaPlaylist>) -> bool {
        matches!(r, Err(Error::Other(m)) if m == "DRM-protected stream")
    }

    #[test]
    fn drm_detection() {
        let b = base();
        let sample = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://abc\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(is_drm(parse_media(sample, &b)));
        let ctr = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:text/plain;base64,AAAA\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(is_drm(parse_media(ctr, &b)));
        let fmt = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",KEYFORMAT=\"com.microsoft.playready\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(is_drm(parse_media(fmt, &b)));
        let ok = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(parse_media(ok, &b).is_ok());
        let session = "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n";
        assert!(
            matches!(parse_master(session, &b), Err(Error::Other(m)) if m == "DRM-protected stream")
        );
    }

    #[test]
    fn rejects_non_playlists() {
        assert!(parse_media("<html></html>", &base()).is_err());
    }
}
