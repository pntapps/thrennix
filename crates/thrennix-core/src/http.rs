//! HTTP client construction, request headers, Content-Range parsing and the probe.

use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Response, StatusCode};

use crate::{DownloadRequest, EngineConfig, Error, Result};

/// Build the per-engine client: HTTP/1.1 only (one TCP connection per worker), no
/// transparent decompression, rustls with session resumption (rustls default).
pub fn build_client(cfg: &EngineConfig) -> Client {
    let b = Client::builder()
        .http1_only()
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(60))
        .connect_timeout(cfg.timeout)
        .redirect(reqwest::redirect::Policy::limited(10))
        .no_gzip()
        .no_brotli()
        .use_rustls_tls();
    match b.build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to build tuned HTTP client ({e}); using defaults");
            Client::new()
        }
    }
}

/// Parsed `Content-Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRange {
    /// `bytes first-last/total` (`last` inclusive, `total` may be unknown `*`).
    Range {
        first: u64,
        last: u64,
        total: Option<u64>,
    },
    /// `bytes */total` (sent with 416).
    Unsatisfied { total: u64 },
}

pub fn parse_content_range(v: &str) -> Option<ContentRange> {
    let v = v.trim();
    let rest = v
        .strip_prefix("bytes")
        .or_else(|| v.strip_prefix("Bytes"))?;
    let rest = rest.trim_start_matches([' ', '=']).trim();
    let (range, total) = rest.split_once('/')?;
    let total = total.trim();
    let total = if total == "*" {
        None
    } else {
        Some(total.parse::<u64>().ok()?)
    };
    let range = range.trim();
    if range == "*" {
        return Some(ContentRange::Unsatisfied { total: total? });
    }
    let (a, b) = range.split_once('-')?;
    let first = a.trim().parse::<u64>().ok()?;
    let last = b.trim().parse::<u64>().ok()?;
    if last < first {
        return None;
    }
    if let Some(t) = total {
        if last >= t {
            return None;
        }
    }
    Some(ContentRange::Range { first, last, total })
}

fn header_str(resp: &Response, name: HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Headers sent on every request for a download (minus Range / If-Range).
pub fn base_headers(cfg: &EngineConfig, req: &DownloadRequest, with_cookies: bool) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in &req.headers {
        let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.trim().as_bytes()),
            HeaderValue::from_str(v.trim()),
        ) else {
            tracing::warn!("skipping invalid extra header {k:?}");
            continue;
        };
        if name == header::RANGE
            || name == header::IF_RANGE
            || name == header::ACCEPT_ENCODING
            || name == header::HOST
            || name == header::CONTENT_LENGTH
        {
            continue;
        }
        if !with_cookies && name == header::COOKIE {
            continue;
        }
        h.append(name, val);
    }
    let ua = req
        .user_agent
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&cfg.user_agent);
    if let Ok(v) = HeaderValue::from_str(ua) {
        h.insert(header::USER_AGENT, v);
    }
    if let Some(r) = req.referer.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Ok(v) = HeaderValue::from_str(r) {
            h.insert(header::REFERER, v);
        }
    }
    if with_cookies {
        if let Some(c) = req.cookies.as_deref().filter(|s| !s.trim().is_empty()) {
            if let Ok(v) = HeaderValue::from_str(c) {
                h.insert(header::COOKIE, v);
            }
        }
    }
    h.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    if !h.contains_key(header::ACCEPT) {
        h.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
    }
    h
}

/// If-Range validator: strong ETag preferred, else Last-Modified.
pub fn if_range_value(etag: Option<&str>, last_modified: Option<&str>) -> Option<HeaderValue> {
    if let Some(e) = etag.filter(|e| !e.starts_with("W/")) {
        if let Ok(v) = HeaderValue::from_str(e) {
            return Some(v);
        }
    }
    last_modified.and_then(|l| HeaderValue::from_str(l).ok())
}

pub fn same_origin(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// How a request failure should be handled by the scheduler.
#[derive(Debug)]
pub enum FailKind {
    /// Server is limiting connections (429/503/refused/reset before any data).
    Throttled(String),
    /// Transient: retry this segment from its cursor with backoff.
    Retry(String),
    /// Give up on the download.
    Fatal(Error),
}

fn io_kind_in_chain(e: &(dyn std::error::Error + 'static)) -> Option<std::io::ErrorKind> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        cur = err.source();
    }
    None
}

/// Classify an error from `send()` (no data received on this connection yet).
pub fn classify_send_error(e: &reqwest::Error) -> FailKind {
    let kind = io_kind_in_chain(e);
    let msg = format!("{e}");
    use std::io::ErrorKind::*;
    // A connection that is refused, reset, or closed before any response headers is how
    // many servers enforce a connection limit (not only with 429/503).
    let closed_early = e.is_request() && !e.is_timeout() && !e.is_builder() && !e.is_redirect();
    if e.is_connect()
        || closed_early
        || matches!(
            kind,
            Some(ConnectionRefused | ConnectionReset | ConnectionAborted)
        )
    {
        FailKind::Throttled(msg)
    } else {
        FailKind::Retry(msg)
    }
}

pub fn classify_status(s: StatusCode) -> FailKind {
    match s.as_u16() {
        429 | 503 => FailKind::Throttled(format!("server returned {s}")),
        408 | 500..=599 => FailKind::Retry(format!("server returned {s}")),
        c => FailKind::Fatal(Error::Status(c)),
    }
}

/// Result of the initial `GET Range: bytes=0-`.
pub struct Probe {
    /// The live response; its body becomes worker #1.
    pub response: Option<Response>,
    pub final_url: String,
    pub total: Option<u64>,
    /// Server honoured the range request and reported a total size.
    pub ranged: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
    pub content_type: Option<String>,
}

async fn send_with_timeout(rb: reqwest::RequestBuilder, t: Duration) -> Result<Response> {
    match tokio::time::timeout(t, rb.send()).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(Error::Http(e.to_string())),
        Err(_) => Err(Error::Http(format!("no response within {t:?}"))),
    }
}

pub async fn probe(
    client: &Client,
    cfg: &EngineConfig,
    url: &str,
    headers: HeaderMap,
    if_range: Option<HeaderValue>,
) -> Result<Probe> {
    let mut rb = client
        .get(url)
        .headers(headers.clone())
        .header(header::RANGE, "bytes=0-");
    if let Some(v) = if_range {
        rb = rb.header(header::IF_RANGE, v);
    }
    let resp = send_with_timeout(rb, cfg.timeout).await?;
    let status = resp.status();
    tracing::debug!(%url, %status, final_url = %resp.url(), "probe response");
    let mut p = Probe {
        final_url: resp.url().to_string(),
        total: None,
        ranged: false,
        etag: header_str(&resp, header::ETAG),
        last_modified: header_str(&resp, header::LAST_MODIFIED),
        content_disposition: header_str(&resp, header::CONTENT_DISPOSITION),
        content_type: header_str(&resp, header::CONTENT_TYPE),
        response: None,
    };
    match status.as_u16() {
        206 => {
            let cr = header_str(&resp, header::CONTENT_RANGE).and_then(|v| parse_content_range(&v));
            match cr {
                Some(ContentRange::Range {
                    first: 0, total, ..
                }) => {
                    p.total = total;
                    p.ranged = total.is_some();
                }
                _ => {
                    return Err(Error::Http(format!(
                        "probe got 206 with unexpected Content-Range {:?}",
                        header_str(&resp, header::CONTENT_RANGE)
                    )))
                }
            }
            p.response = Some(resp);
        }
        200 => {
            p.total = resp.content_length();
            p.response = Some(resp);
        }
        416 => {
            let cr = header_str(&resp, header::CONTENT_RANGE).and_then(|v| parse_content_range(&v));
            if let Some(ContentRange::Unsatisfied { total: 0 }) = cr {
                // Empty file.
                p.total = Some(0);
                p.ranged = true;
                return Ok(p);
            }
            // Retry without a Range header.
            let resp = send_with_timeout(client.get(url).headers(headers), cfg.timeout).await?;
            if !resp.status().is_success() {
                return Err(Error::Status(resp.status().as_u16()));
            }
            p.final_url = resp.url().to_string();
            p.total = resp.content_length();
            p.etag = header_str(&resp, header::ETAG);
            p.last_modified = header_str(&resp, header::LAST_MODIFIED);
            p.content_disposition = header_str(&resp, header::CONTENT_DISPOSITION);
            p.content_type = header_str(&resp, header::CONTENT_TYPE);
            p.response = Some(resp);
        }
        c => return Err(Error::Status(c)),
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(
            parse_content_range("bytes 0-99/1000"),
            Some(ContentRange::Range {
                first: 0,
                last: 99,
                total: Some(1000)
            })
        );
        assert_eq!(
            parse_content_range("bytes 500-999/*"),
            Some(ContentRange::Range {
                first: 500,
                last: 999,
                total: None
            })
        );
        assert_eq!(
            parse_content_range("bytes */1234"),
            Some(ContentRange::Unsatisfied { total: 1234 })
        );
        assert_eq!(
            parse_content_range(" bytes  10-10/11 "),
            Some(ContentRange::Range {
                first: 10,
                last: 10,
                total: Some(11)
            })
        );
        assert_eq!(parse_content_range("bytes 10-5/100"), None);
        assert_eq!(parse_content_range("bytes 0-100/100"), None);
        assert_eq!(parse_content_range("bytes 0-x/100"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes */*"), None);
    }

    #[test]
    fn if_range_prefers_strong_etag() {
        assert_eq!(
            if_range_value(Some("\"abc\""), Some("x")).unwrap(),
            "\"abc\""
        );
        assert_eq!(
            if_range_value(Some("W/\"abc\""), Some("Mon")).unwrap(),
            "Mon"
        );
        assert!(if_range_value(Some("W/\"abc\""), None).is_none());
    }

    #[test]
    fn headers_built() {
        let cfg = EngineConfig::default();
        let req = DownloadRequest {
            cookies: Some("a=b".into()),
            referer: Some("https://r/".into()),
            headers: vec![
                ("X-Test".into(), "1".into()),
                ("Range".into(), "bytes=5-".into()),
            ],
            ..Default::default()
        };
        let h = base_headers(&cfg, &req, true);
        assert_eq!(h.get(header::COOKIE).unwrap(), "a=b");
        assert_eq!(h.get(header::REFERER).unwrap(), "https://r/");
        assert_eq!(h.get("x-test").unwrap(), "1");
        assert!(h.get(header::RANGE).is_none());
        assert_eq!(h.get(header::ACCEPT_ENCODING).unwrap(), "identity");
        assert!(h
            .get(header::USER_AGENT)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Thrennix/"));
        assert!(base_headers(&cfg, &req, false)
            .get(header::COOKIE)
            .is_none());
    }
}
