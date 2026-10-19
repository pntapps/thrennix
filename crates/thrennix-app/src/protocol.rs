//! Browser <-> app messages (see docs/PROTOCOL.md).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    File,
    Hls,
    Dash,
    /// YouTube and other sites, downloaded by yt-dlp.
    Ytdlp,
}

impl Kind {
    pub fn is_media(self) -> bool {
        self != Kind::File
    }

    /// Default yt-dlp format for links typed into the app (best video + audio).
    pub const YTDLP_BEST: &'static str = "bv*+ba/b";

    /// Guess from a URL typed by the user.
    pub fn guess(url: &str) -> Kind {
        let path = url::Url::parse(url)
            .map(|u| u.path().to_ascii_lowercase())
            .unwrap_or_default();
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        if is_youtube_host(&host) {
            Kind::Ytdlp
        } else if path.ends_with(".m3u8") {
            Kind::Hls
        } else if path.ends_with(".mpd") {
            Kind::Dash
        } else {
            Kind::File
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Request {
    Download(DownloadMsg),
    Ping,
    Show,
    #[serde(rename = "ytFormats")]
    YtFormats {
        url: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadMsg {
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub kind: Option<Kind>,
    /// yt-dlp format id from a ytFormats reply (required for kind: "ytdlp").
    #[serde(default)]
    pub format: Option<String>,
}

impl DownloadMsg {
    /// Checks that depend on more than one field.
    pub fn check(&self) -> Result<(), String> {
        let has_format = self.format.as_deref().is_some_and(|f| !f.trim().is_empty());
        if self.kind == Some(Kind::Ytdlp) && !has_format {
            return Err("missing \"format\" for a ytdlp download".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YtFormat {
    pub id: String,
    pub label: String,
    pub height: Option<u32>,
    pub filesize: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Response {
    Ok,
    Pong {
        version: String,
        /// Only set by the native host when it answers a ping itself (GUI not running).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        running: Option<bool>,
    },
    Error {
        message: String,
    },
    #[serde(rename = "ytFormats")]
    YtFormats {
        title: String,
        formats: Vec<YtFormat>,
    },
}

impl Response {
    pub fn error(msg: impl Into<String>) -> Self {
        Response::Error {
            message: msg.into(),
        }
    }
    pub fn pong() -> Self {
        Response::Pong {
            version: VERSION.to_string(),
            running: None,
        }
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("response serializes")
    }
}

pub fn is_youtube_host(host: &str) -> bool {
    let h = host
        .trim_start_matches("www.")
        .trim_start_matches("m.")
        .trim_start_matches("music.");
    h == "youtube.com" || h == "youtu.be" || h == "youtube-nocookie.com"
}

/// Accept only absolute http(s) URLs with a host.
pub fn validate_url(s: &str) -> Result<url::Url, String> {
    let u = url::Url::parse(s.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err("only http and https URLs are supported".into());
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err("invalid URL: missing host".into());
    }
    Ok(u)
}

/// Strip characters that are invalid in Windows file names; `None` if nothing is left.
pub fn sanitize_file_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').trim().to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Request, serde_json::Error> {
        serde_json::from_str(s)
    }

    #[test]
    fn ping_and_show() {
        assert_eq!(parse(r#"{"type":"ping"}"#).unwrap(), Request::Ping);
        assert_eq!(parse(r#"{"type":"show"}"#).unwrap(), Request::Show);
        assert_eq!(
            parse(r#"{"type":"ping","extra":1}"#).unwrap(),
            Request::Ping
        );
    }

    #[test]
    fn full_download() {
        let r = parse(
            r#"{"type":"download","url":"https://example.com/file.zip","filename":"file.zip",
                "referer":"https://example.com/","cookies":"a=1; b=2","userAgent":"Mozilla/5.0",
                "headers":{"Authorization":"Bearer x"},"kind":"hls"}"#,
        )
        .unwrap();
        let Request::Download(m) = r else { panic!() };
        assert_eq!(m.url, "https://example.com/file.zip");
        assert_eq!(m.filename.as_deref(), Some("file.zip"));
        assert_eq!(m.referer.as_deref(), Some("https://example.com/"));
        assert_eq!(m.cookies.as_deref(), Some("a=1; b=2"));
        assert_eq!(m.user_agent.as_deref(), Some("Mozilla/5.0"));
        assert_eq!(m.headers.unwrap()["Authorization"], "Bearer x");
        assert_eq!(m.kind, Some(Kind::Hls));
    }

    #[test]
    fn minimal_download() {
        let Request::Download(m) = parse(r#"{"type":"download","url":"http://a.b/c"}"#).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            m,
            DownloadMsg {
                url: "http://a.b/c".into(),
                ..Default::default()
            }
        );
        // nulls are treated as missing
        let Request::Download(m) =
            parse(r#"{"type":"download","url":"http://a.b/c","filename":null,"kind":"dash"}"#)
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(m.filename, None);
        assert_eq!(m.kind, Some(Kind::Dash));
    }

    #[test]
    fn invalid_messages() {
        assert!(parse(r#"{"type":"download"}"#).is_err()); // no url
        assert!(parse(r#"{"type":"nope"}"#).is_err());
        assert!(parse(r#"{"url":"x"}"#).is_err());
        assert!(parse(r#"{"type":"download","url":"x","kind":"video"}"#).is_err());
        assert!(parse("[]").is_err());
    }

    #[test]
    fn responses() {
        assert_eq!(Response::Ok.to_json(), r#"{"type":"ok"}"#);
        assert_eq!(
            Response::pong().to_json(),
            format!(r#"{{"type":"pong","version":"{VERSION}"}}"#)
        );
        assert_eq!(
            Response::error("app not running").to_json(),
            r#"{"type":"error","message":"app not running"}"#
        );
        let back: Response = serde_json::from_str(r#"{"type":"pong","version":"1"}"#).unwrap();
        assert_eq!(
            back,
            Response::Pong {
                version: "1".into(),
                running: None
            }
        );
    }

    #[test]
    fn yt_formats_request() {
        assert_eq!(
            parse(r#"{"type":"ytFormats","url":"https://www.youtube.com/watch?v=x"}"#).unwrap(),
            Request::YtFormats {
                url: "https://www.youtube.com/watch?v=x".into()
            }
        );
        assert!(parse(r#"{"type":"ytFormats"}"#).is_err());
        assert!(parse(r#"{"type":"ytformats","url":"x"}"#).is_err());
    }

    #[test]
    fn ytdlp_download() {
        let Request::Download(m) = parse(
            r#"{"type":"download","kind":"ytdlp","url":"https://youtu.be/x","format":"bv*[height<=720]+ba/b[height<=720]","filename":"My video"}"#,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(m.kind, Some(Kind::Ytdlp));
        assert_eq!(
            m.format.as_deref(),
            Some("bv*[height<=720]+ba/b[height<=720]")
        );
        assert_eq!(m.filename.as_deref(), Some("My video"));
        assert!(m.check().is_ok());
        // format is required for ytdlp ...
        let Request::Download(m) =
            parse(r#"{"type":"download","kind":"ytdlp","url":"https://youtu.be/x"}"#).unwrap()
        else {
            panic!()
        };
        assert!(m.check().unwrap_err().contains("format"));
        let Request::Download(m) =
            parse(r#"{"type":"download","kind":"ytdlp","url":"https://youtu.be/x","format":" "}"#)
                .unwrap()
        else {
            panic!()
        };
        assert!(m.check().is_err());
        // ... and optional otherwise.
        let Request::Download(m) = parse(r#"{"type":"download","url":"https://a/b"}"#).unwrap()
        else {
            panic!()
        };
        assert!(m.check().is_ok());
    }

    #[test]
    fn yt_formats_reply() {
        let r = Response::YtFormats {
            title: "T".into(),
            formats: vec![
                YtFormat {
                    id: "b".into(),
                    label: "MP4 · 720p".into(),
                    height: Some(720),
                    filesize: Some(5),
                },
                YtFormat {
                    id: "ba".into(),
                    label: "Audio".into(),
                    height: None,
                    filesize: None,
                },
            ],
        };
        let v: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(v["type"], "ytFormats");
        assert_eq!(v["title"], "T");
        assert_eq!(v["formats"][0]["id"], "b");
        assert_eq!(v["formats"][0]["height"], 720);
        assert!(v["formats"][1]["height"].is_null());
        assert!(v["formats"][1]["filesize"].is_null());
    }

    #[test]
    fn url_validation() {
        assert!(validate_url("https://example.com/a").is_ok());
        assert!(validate_url(" http://example.com ").is_ok());
        assert!(validate_url("ftp://example.com/a").is_err());
        assert!(validate_url("file:///C:/x").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("example.com").is_err());
    }

    #[test]
    fn kinds_and_names() {
        assert_eq!(Kind::guess("https://x.y/a/master.M3U8?t=1"), Kind::Hls);
        assert_eq!(Kind::guess("https://x.y/manifest.mpd"), Kind::Dash);
        assert_eq!(Kind::guess("https://x.y/a.zip"), Kind::File);
        assert_eq!(
            Kind::guess("https://www.youtube.com/watch?v=abc"),
            Kind::Ytdlp
        );
        assert_eq!(Kind::guess("https://youtu.be/abc"), Kind::Ytdlp);
        assert_eq!(Kind::guess("https://notyoutube.com/a.mp4"), Kind::File);
        assert_eq!(serde_json::to_string(&Kind::Ytdlp).unwrap(), r#""ytdlp""#);
        assert_eq!(
            sanitize_file_name("a/b:c?.zip").as_deref(),
            Some("a_b_c_.zip")
        );
        assert_eq!(sanitize_file_name(" .. "), None);
    }
}
