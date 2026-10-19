//! HTTP plumbing: one client, request headers from the `MediaRequest`, retries, size caps.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_RANGE, COOKIE,
    PROXY_AUTHORIZATION, RANGE, REFERER, USER_AGENT,
};
use reqwest::{Client, StatusCode};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::model::ByteRange;
use crate::{Error, MediaRequest, Result};

pub(crate) const MAX_TRIES: u32 = 8;
/// Largest playlist/MPD we accept.
pub(crate) const MAX_MANIFEST: u64 = 32 << 20;
/// Largest AES key response we accept (a key is 16 bytes).
pub(crate) const MAX_KEY: u64 = 1024;
/// Largest segment we buffer in memory (only encrypted segments are buffered).
pub(crate) const MAX_MEM_SEGMENT: u64 = 512 << 20;
const DEFAULT_UA: &str = concat!("Thrennix/", env!("CARGO_PKG_VERSION"));

/// Second-level labels under which registrations happen one level deeper (`example.co.uk`).
const MULTI_PART_SUFFIXES: [&str; 16] = [
    "co", "com", "net", "org", "gov", "edu", "ac", "or", "ne", "go", "gob", "nic", "ltd", "plc",
    "sch", "nhs",
];

#[derive(Clone)]
pub(crate) struct Http {
    client: Client,
    /// Sent everywhere: User-Agent, Referer, Accept-Encoding, non-credential extras.
    headers: HeaderMap,
    /// Cookie / Authorization: only sent to the manifest's site.
    sensitive: HeaderMap,
    origin_host: Option<String>,
}

/// Error from a single attempt, flagged with whether trying again could help.
struct Attempt {
    error: Error,
    retry: bool,
}

fn transient(error: Error) -> Attempt {
    Attempt { error, retry: true }
}

fn fatal(error: Error) -> Attempt {
    Attempt {
        error,
        retry: false,
    }
}

fn status_attempt(status: StatusCode) -> Attempt {
    let retry = status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429);
    Attempt {
        error: Error::Status(status.as_u16()),
        retry,
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((250u64 << attempt.min(6)).min(10_000))
}

/// The "site" of a host: the last two labels, or three under `co.uk`-style suffixes.
fn site_of(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() < 2 {
        return None;
    }
    let n = labels.len();
    let take = if n >= 3 && labels[n - 1].len() == 2 && MULTI_PART_SUFFIXES.contains(&labels[n - 2])
    {
        3
    } else {
        2
    };
    Some(labels[n - take..].join("."))
}

/// Whether credentials meant for `origin` may be sent to `host`: same host, or same site.
/// IP addresses only match themselves.
pub(crate) fn same_site(origin: &str, host: &str) -> bool {
    let norm = |h: &str| h.trim_end_matches('.').to_ascii_lowercase();
    let (a, b) = (norm(origin), norm(host));
    if a == b {
        return true;
    }
    let is_ip = |h: &str| h.starts_with('[') || h.parse::<std::net::IpAddr>().is_ok();
    if is_ip(&a) || is_ip(&b) {
        return false;
    }
    match (site_of(&a), site_of(&b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Where downloaded bytes go.
pub(crate) enum Sink<'a> {
    Mem(&'a mut Vec<u8>),
    File(&'a mut tokio::fs::File),
}

impl Sink<'_> {
    async fn reset(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Mem(v) => v.clear(),
            Sink::File(f) => {
                f.set_len(0).await?;
                f.seek(std::io::SeekFrom::Start(0)).await?;
            }
        }
        Ok(())
    }

    async fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Mem(v) => v.extend_from_slice(data),
            Sink::File(f) => f.write_all(data).await?,
        }
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        if let Sink::File(f) = self {
            f.flush().await?;
        }
        Ok(())
    }
}

fn io_fatal(e: std::io::Error) -> Attempt {
    fatal(Error::Io(e))
}

/// Start offset of a `Content-Range: bytes a-b/len` header.
fn content_range_start(v: &str) -> Option<u64> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

impl Http {
    pub fn new(req: &MediaRequest) -> Result<Self> {
        let client = Client::builder()
            .http1_only()
            .use_rustls_tls()
            .tcp_nodelay(true)
            .no_gzip()
            .no_brotli()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(64)
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        let mut headers = HeaderMap::new();
        let mut sensitive = HeaderMap::new();
        let bad = |what: &str| Error::Other(format!("invalid {what} header value"));
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        let ua = req
            .user_agent
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_UA);
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(ua).map_err(|_| bad("User-Agent"))?,
        );
        if let Some(r) = req.referer.as_deref().filter(|s| !s.is_empty()) {
            headers.insert(
                REFERER,
                HeaderValue::from_str(r).map_err(|_| bad("Referer"))?,
            );
        }
        if let Some(c) = req.cookies.as_deref().filter(|s| !s.is_empty()) {
            sensitive.insert(COOKIE, HeaderValue::from_str(c).map_err(|_| bad("Cookie"))?);
        }
        for (k, v) in &req.headers {
            let name = HeaderName::from_bytes(k.trim().as_bytes())
                .map_err(|_| Error::Other(format!("invalid header name {k:?}")))?;
            if name == RANGE || name == ACCEPT_ENCODING {
                continue;
            }
            let value = HeaderValue::from_str(v).map_err(|_| bad(k))?;
            if name == COOKIE || name == AUTHORIZATION || name == PROXY_AUTHORIZATION {
                sensitive.insert(name, value);
            } else {
                headers.insert(name, value);
            }
        }
        let origin_host = Url::parse(&req.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string));
        Ok(Http {
            client,
            headers,
            sensitive,
            origin_host,
        })
    }

    fn headers_for(&self, url: &str) -> HeaderMap {
        let mut h = self.headers.clone();
        let allowed = match (&self.origin_host, Url::parse(url).ok()) {
            (Some(origin), Some(u)) => u.host_str().is_some_and(|host| same_site(origin, host)),
            _ => false,
        };
        if allowed {
            for (k, v) in &self.sensitive {
                h.insert(k.clone(), v.clone());
            }
        }
        h
    }

    /// One GET. Keeps `range` (or the whole body up to `cap`) in `sink`; returns the final URL.
    async fn attempt(
        &self,
        url: &str,
        range: Option<&ByteRange>,
        counter: &AtomicU64,
        cap: u64,
        sink: &mut Sink<'_>,
    ) -> std::result::Result<Url, Attempt> {
        let mut rb = self.client.get(url).headers(self.headers_for(url));
        let want = match range {
            Some(r) => {
                let end = r
                    .offset
                    .checked_add(r.length)
                    .filter(|_| r.length > 0)
                    .ok_or_else(|| fatal(Error::Other(format!("invalid byte range for {url}"))))?;
                if r.length > cap {
                    return Err(fatal(Error::Other(format!("{url}: segment too large"))));
                }
                rb = rb.header(RANGE, format!("bytes={}-{}", r.offset, end - 1));
                Some((r.offset, r.length))
            }
            None => None,
        };
        let mut resp = rb.send().await.map_err(|e| net(e, url))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status_attempt(status));
        }
        let final_url = resp.url().clone();
        let partial = status == StatusCode::PARTIAL_CONTENT;
        if let (Some((off, _)), true) = (want, partial) {
            let start = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(content_range_start);
            if start.is_some_and(|s| s != off) {
                return Err(fatal(Error::Other(format!(
                    "{url}: server returned a different byte range"
                ))));
            }
        }
        // Bytes of the body to skip / keep. A server that ignores Range sends everything.
        let (skip, keep) = match want {
            Some((off, len)) => (if partial { 0 } else { off }, len),
            None => (0, cap),
        };
        // Read it up front: afterwards it reflects the consumed body's size hint.
        let expected = resp.content_length();
        if want.is_none() && expected.is_some_and(|e| e > cap) {
            return Err(fatal(Error::Other(format!("{url}: response too large"))));
        }
        sink.reset().await.map_err(io_fatal)?;
        let mut pos: u64 = 0;
        let mut kept: u64 = 0;
        let keep_end = skip.saturating_add(keep);
        while let Some(chunk) = resp.chunk().await.map_err(|e| net(e, url))? {
            let len = chunk.len() as u64;
            counter.fetch_add(len, Ordering::Relaxed);
            let start = pos;
            pos = pos.saturating_add(len);
            let lo = skip.saturating_sub(start).min(len);
            let hi = keep_end.saturating_sub(start).min(len);
            if hi > lo {
                sink.write(&chunk[lo as usize..hi as usize])
                    .await
                    .map_err(io_fatal)?;
                kept += hi - lo;
            }
            if want.is_some() && kept >= keep {
                break;
            }
            if want.is_none() && pos > cap {
                return Err(fatal(Error::Other(format!("{url}: response too large"))));
            }
        }
        match want {
            Some((_, len)) if kept != len => {
                if !partial && expected.is_some_and(|e| e < keep_end) {
                    return Err(fatal(Error::Other(format!(
                        "{url} is shorter than its byte range"
                    ))));
                }
                return Err(transient(Error::Http(format!("short body from {url}"))));
            }
            None if expected.is_some_and(|e| e != pos) => {
                return Err(transient(Error::Http(format!("short body from {url}"))));
            }
            _ => {}
        }
        sink.finish().await.map_err(io_fatal)?;
        Ok(final_url)
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<&ByteRange>,
        counter: &AtomicU64,
        cap: u64,
        sink: &mut Sink<'_>,
    ) -> Result<Url> {
        let mut attempt = 0;
        loop {
            match self.attempt(url, range, counter, cap, sink).await {
                Ok(u) => return Ok(u),
                Err(a) => {
                    attempt += 1;
                    if !a.retry || attempt >= MAX_TRIES {
                        tracing::warn!(
                            "giving up on {url} after {attempt} attempt(s): {}",
                            a.error
                        );
                        return Err(a.error);
                    }
                    tracing::debug!("retrying {url} (attempt {attempt}): {}", a.error);
                    tokio::time::sleep(backoff(attempt - 1)).await;
                }
            }
        }
    }

    /// GET a text document (playlist/manifest). Returns the body and the final URL after redirects.
    pub async fn get_text(&self, url: &str) -> Result<(String, Url)> {
        let mut buf = Vec::new();
        let counter = AtomicU64::new(0);
        let final_url = self
            .fetch(url, None, &counter, MAX_MANIFEST, &mut Sink::Mem(&mut buf))
            .await?;
        Ok((String::from_utf8_lossy(&buf).into_owned(), final_url))
    }

    /// GET a small key file.
    pub async fn get_key(&self, url: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let counter = AtomicU64::new(0);
        self.fetch(url, None, &counter, MAX_KEY, &mut Sink::Mem(&mut buf))
            .await?;
        Ok(buf)
    }

    /// GET a whole resource or a byte range of it into memory. `counter` is bumped as bytes arrive.
    pub async fn get_bytes(
        &self,
        url: &str,
        range: Option<&ByteRange>,
        counter: &AtomicU64,
    ) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.fetch(
            url,
            range,
            counter,
            MAX_MEM_SEGMENT,
            &mut Sink::Mem(&mut buf),
        )
        .await?;
        Ok(buf)
    }

    /// GET a whole resource or a byte range of it straight into `file`. Returns bytes written.
    pub async fn download_to(
        &self,
        url: &str,
        range: Option<&ByteRange>,
        counter: &AtomicU64,
        file: &mut tokio::fs::File,
    ) -> Result<u64> {
        self.fetch(url, range, counter, u64::MAX, &mut Sink::File(file))
            .await?;
        Ok(file.metadata().await?.len())
    }
}

fn net(e: reqwest::Error, url: &str) -> Attempt {
    let retry = !e.is_builder() && !e.is_redirect();
    Attempt {
        error: Error::Http(format!("{url}: {e}")),
        retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_scoping() {
        assert!(same_site("cdn.example.com", "cdn.example.com"));
        assert!(same_site("www.example.com", "cdn.example.com"));
        assert!(same_site("example.com", "video.example.com"));
        assert!(same_site("EXAMPLE.com.", "example.com"));
        assert!(!same_site("example.com", "evil.com"));
        assert!(!same_site("example.com", "example.com.evil.net"));
        assert!(same_site("a.bbc.co.uk", "b.bbc.co.uk"));
        assert!(!same_site("bbc.co.uk", "evil.co.uk"));
        assert!(!same_site("127.0.0.1", "localhost"));
        assert!(!same_site("10.0.0.1", "10.0.0.2"));
        assert!(same_site("127.0.0.1", "127.0.0.1"));
        assert!(!same_site("[::1]", "localhost"));
        assert!(!same_site("localhost", "otherhost"));
    }

    #[test]
    fn credentials_only_on_same_site() {
        let req = MediaRequest {
            url: "https://www.example.com/v/master.m3u8".into(),
            cookies: Some("a=1".into()),
            referer: Some("https://www.example.com/".into()),
            headers: vec![
                ("Authorization".into(), "Bearer x".into()),
                ("X-Other".into(), "1".into()),
            ],
            ..Default::default()
        };
        let http = Http::new(&req).unwrap();
        let same = http.headers_for("https://cdn.example.com/seg.ts");
        assert!(same.contains_key(COOKIE) && same.contains_key(AUTHORIZATION));
        let other = http.headers_for("https://cdn.akamai.net/seg.ts");
        assert!(!other.contains_key(COOKIE) && !other.contains_key(AUTHORIZATION));
        assert!(other.contains_key(REFERER) && other.contains_key("x-other"));
        assert!(other.contains_key(USER_AGENT));
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(content_range_start("bytes 100-199/1000"), Some(100));
        assert_eq!(content_range_start("bytes */1000"), None);
        assert_eq!(content_range_start("garbage"), None);
    }
}
