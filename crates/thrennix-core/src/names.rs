//! File-name detection (Content-Disposition / URL) and Windows-safe sanitizing.

use std::path::{Path, PathBuf};

const MAX_NAME_CHARS: usize = 200;

/// Percent-decode into raw bytes (invalid escapes are kept literally).
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Split a header value on `;`, honouring double quotes.
fn split_params(v: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_q = false;
    let mut esc = false;
    let mut start = 0;
    for (i, ch) in v.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match ch {
            '\\' if in_q => esc = true,
            '"' => in_q = !in_q,
            ';' if !in_q => {
                parts.push(&v[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&v[start..]);
    parts
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut esc = false;
        for ch in inner.chars() {
            if esc {
                out.push(ch);
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        v.to_string()
    }
}

/// Keep only the last path component (servers sometimes send full paths).
fn basename(s: &str) -> &str {
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}

/// Parse a `Content-Disposition` value. `filename*` (RFC 5987) wins over `filename`.
pub fn parse_content_disposition(v: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut ext: Option<String> = None;
    for p in split_params(v).into_iter().skip(1) {
        let Some((k, val)) = p.split_once('=') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        if k == "filename*" {
            let val = unquote(val);
            // charset'lang'value
            let mut it = val.splitn(3, '\'');
            let (cs, _lang, enc) = (it.next(), it.next(), it.next());
            let (cs, enc) = match (cs, enc) {
                (Some(cs), Some(enc)) => (cs.to_ascii_lowercase(), enc),
                _ => (String::from("utf-8"), val.as_str()),
            };
            let bytes = percent_decode(enc);
            let s = if cs == "iso-8859-1" || cs == "latin1" {
                bytes.iter().map(|&b| b as char).collect()
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            if !s.trim().is_empty() {
                ext = Some(s);
            }
        } else if k == "filename" {
            let mut s = unquote(val);
            if s.contains('%') {
                if let Ok(d) = String::from_utf8(percent_decode(&s)) {
                    s = d;
                }
            }
            if !s.trim().is_empty() {
                plain = Some(s);
            }
        }
    }
    ext.or(plain)
        .map(|s| basename(&s).to_string())
        .filter(|s| !s.trim().is_empty())
}

/// Last non-empty path segment of a URL, percent-decoded.
pub fn name_from_url(u: &str) -> Option<String> {
    let parsed = url::Url::parse(u).ok()?;
    let seg = parsed.path_segments()?.rev().find(|s| !s.is_empty())?;
    let s = String::from_utf8_lossy(&percent_decode(seg)).into_owned();
    let s = basename(&s).to_string();
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

fn is_reserved(stem: &str) -> bool {
    let up = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((up.starts_with("COM") || up.starts_with("LPT"))
            && up.len() == 4
            && up.as_bytes()[3].is_ascii_digit()
            && up.as_bytes()[3] != b'0')
}

/// Make a name safe for Windows file systems.
pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 || c as u32 == 0x7f => '_',
            c => c,
        })
        .collect();
    s = s
        .trim_start_matches(' ')
        .trim_end_matches([' ', '.'])
        .to_string();
    if s.is_empty() {
        s = "download".to_string();
    }
    let stem = s.split('.').next().unwrap_or("");
    if is_reserved(stem) {
        s = format!("_{s}");
    }
    if s.chars().count() > MAX_NAME_CHARS {
        let (stem, ext) = split_ext(&s);
        let ext: String = ext.chars().take(20).collect();
        let keep = MAX_NAME_CHARS.saturating_sub(ext.chars().count());
        let stem: String = stem.chars().take(keep).collect();
        s = format!("{}{}", stem.trim_end_matches([' ', '.']), ext);
    }
    s
}

/// Split `"name.ext"` into `("name", ".ext")`. Dot-files have no extension.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// `name (n).ext`
pub fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    format!("{stem} ({n}){ext}")
}

/// Pick the final file name for a download.
pub fn choose_name(
    override_name: Option<&str>,
    content_disposition: Option<&str>,
    final_url: &str,
    original_url: &str,
) -> String {
    let raw = override_name
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| content_disposition.and_then(parse_content_disposition))
        .or_else(|| name_from_url(final_url))
        .or_else(|| name_from_url(original_url))
        .unwrap_or_else(|| "download".to_string());
    sanitize(&raw)
}

/// `<path><suffix>` (e.g. `file.zip` + `.part`).
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_variants() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"report.pdf\"").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=plain.txt").as_deref(),
            Some("plain.txt")
        );
        assert_eq!(
            parse_content_disposition(
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E2%82%AC%20rates.zip"
            )
            .as_deref(),
            Some("€ rates.zip")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename*=utf-8'en'na%C3%AFve.txt").as_deref(),
            Some("naïve.txt")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=\"a;b \\\"q\\\".txt\"").as_deref(),
            Some("a;b \"q\".txt")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=\"C:\\\\temp\\\\x.exe\"").as_deref(),
            Some("x.exe")
        );
        assert_eq!(parse_content_disposition("inline").as_deref(), None);
        assert_eq!(
            parse_content_disposition("attachment; filename=\"\"").as_deref(),
            None
        );
    }

    #[test]
    fn url_names() {
        assert_eq!(
            name_from_url("https://x.com/a/b/My%20File.iso?x=1#f").as_deref(),
            Some("My File.iso")
        );
        assert_eq!(name_from_url("https://x.com/dir/").as_deref(), Some("dir"));
        assert_eq!(name_from_url("https://x.com/").as_deref(), None);
        assert_eq!(
            choose_name(None, None, "https://x.com/", "https://x.com/"),
            "download"
        );
        assert_eq!(
            choose_name(
                Some("over.bin"),
                Some("attachment; filename=cd.bin"),
                "https://x/u.bin",
                ""
            ),
            "over.bin"
        );
        assert_eq!(
            choose_name(
                None,
                Some("attachment; filename=cd.bin"),
                "https://x/u.bin",
                ""
            ),
            "cd.bin"
        );
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a<b>c:d\"e/f\\g|h?i*j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("bad\u{1}name\t.txt"), "bad_name_.txt");
        assert_eq!(sanitize("trailing. . "), "trailing");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("nul.txt"), "_nul.txt");
        assert_eq!(sanitize("com1.log"), "_com1.log");
        assert_eq!(sanitize("COM0.log"), "COM0.log");
        assert_eq!(sanitize("console.txt"), "console.txt");
        assert_eq!(sanitize(""), "download");
        assert_eq!(sanitize("..."), "download");
        let long = format!("{}.zip", "x".repeat(500));
        let s = sanitize(&long);
        assert!(s.chars().count() <= MAX_NAME_CHARS);
        assert!(s.ends_with(".zip"));
    }

    #[test]
    fn numbering() {
        assert_eq!(numbered("file.zip", 0), "file.zip");
        assert_eq!(numbered("file.zip", 2), "file (2).zip");
        assert_eq!(numbered("README", 1), "README (1)");
        assert_eq!(numbered(".bashrc", 1), ".bashrc (1)");
        assert_eq!(numbered("a.tar.gz", 1), "a.tar (1).gz");
    }

    #[test]
    fn percent() {
        assert_eq!(percent_decode("a%20b%zz%4"), b"a b%zz%4".to_vec());
        assert_eq!(percent_decode("%41"), b"A".to_vec());
    }
}
