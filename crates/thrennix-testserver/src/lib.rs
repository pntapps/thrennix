//! thrennix-testserver: a reproducible HTTP server for download-manager tests and benchmarks.
//!
//! `GET /file/{size}` serves a virtual file whose byte `i` is [`pattern_byte(i)`](pattern_byte).
//! Query options simulate real-world servers (per-connection throttling, a shared line limit,
//! connection limits, no range support, latency, dropped connections, changed ETags).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;

/// Fixed Last-Modified date for every virtual file (stable across restarts).
pub const LAST_MODIFIED: &str = "Wed, 01 Jan 2025 00:00:00 GMT";

/// Pattern period: (31*i mod 256) has period 256, (i/7 mod 256) has period 1792, lcm = 1792.
const PERIOD: usize = 1792;
/// Largest chunk handed to hyper at once (unthrottled).
const MAX_CHUNK: usize = 256 * 1024;

/// Byte `i` of every virtual file.
#[inline]
pub fn pattern_byte(i: u64) -> u8 {
    (i.wrapping_mul(31).wrapping_add(i / 7)) as u8
}

/// Fill `buf` with the pattern starting at absolute offset `offset`.
pub fn fill_pattern(offset: u64, buf: &mut [u8]) {
    let table = pattern_table();
    let mut pos = 0;
    while pos < buf.len() {
        let start = ((offset + pos as u64) % PERIOD as u64) as usize;
        let n = (buf.len() - pos).min(table.len() - start);
        buf[pos..pos + n].copy_from_slice(&table[start..start + n]);
        pos += n;
    }
}

/// Precomputed pattern table of PERIOD + MAX_CHUNK bytes, so any chunk is a zero-copy slice.
fn pattern_table() -> &'static Bytes {
    static TABLE: OnceLock<Bytes> = OnceLock::new();
    TABLE.get_or_init(|| {
        let v: Vec<u8> = (0..(PERIOD + MAX_CHUNK) as u64).map(pattern_byte).collect();
        Bytes::from(v)
    })
}

fn pattern_chunk(offset: u64, len: usize) -> Bytes {
    debug_assert!(len <= MAX_CHUNK);
    let start = (offset % PERIOD as u64) as usize;
    pattern_table().slice(start..start + len)
}

/// Parse `1048576`, `100K`, `100M`, `2G`, `1T` (binary multiples; optional trailing `B`/`iB`).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let upper = s.to_ascii_uppercase();
    let trimmed = upper
        .strip_suffix("IB")
        .or_else(|| upper.strip_suffix('B'))
        .unwrap_or(&upper);
    let (num, mult) = match trimmed.chars().last()? {
        'K' => (&trimmed[..trimmed.len() - 1], 1u64 << 10),
        'M' => (&trimmed[..trimmed.len() - 1], 1u64 << 20),
        'G' => (&trimmed[..trimmed.len() - 1], 1u64 << 30),
        'T' => (&trimmed[..trimmed.len() - 1], 1u64 << 40),
        _ => (trimmed, 1),
    };
    let num = num.trim();
    if num.is_empty() {
        return None;
    }
    if let Ok(n) = num.parse::<u64>() {
        return n.checked_mul(mult);
    }
    let f: f64 = num.parse().ok()?;
    if !(f.is_finite() && f >= 0.0) {
        return None;
    }
    Some((f * mult as f64) as u64)
}

/// Result of interpreting a `Range` header against a resource of `size` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// No (usable) range: serve the full body with 200.
    Full,
    /// Serve bytes `start..=end` with 206.
    Partial { start: u64, end: u64 },
    /// 416 Range Not Satisfiable.
    Unsatisfiable,
}

/// Parse a single-range `Range` header (`bytes=a-b`, `bytes=a-`, `bytes=-n`).
/// Non-`bytes` units and multi-range requests are ignored (full 200), as RFC 9110 permits.
pub fn parse_range(header: Option<&str>, size: u64) -> RangeSpec {
    let Some(h) = header else {
        return RangeSpec::Full;
    };
    let h = h.trim();
    let Some(spec) = h
        .strip_prefix("bytes=")
        .or_else(|| h.strip_prefix("bytes ="))
    else {
        return RangeSpec::Full;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeSpec::Full;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return RangeSpec::Unsatisfiable;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // suffix range: last n bytes
        let Ok(n) = b.parse::<u64>() else {
            return RangeSpec::Unsatisfiable;
        };
        if n == 0 || size == 0 {
            return RangeSpec::Unsatisfiable;
        }
        let n = n.min(size);
        return RangeSpec::Partial {
            start: size - n,
            end: size - 1,
        };
    }
    let Ok(start) = a.parse::<u64>() else {
        return RangeSpec::Unsatisfiable;
    };
    if start >= size {
        return RangeSpec::Unsatisfiable;
    }
    let end = if b.is_empty() {
        size - 1
    } else {
        let Ok(e) = b.parse::<u64>() else {
            return RangeSpec::Unsatisfiable;
        };
        if e < start {
            return RangeSpec::Unsatisfiable;
        }
        e.min(size - 1)
    };
    RangeSpec::Partial { start, end }
}

/// Async pacing token bucket. `take(n)` waits until `n` bytes may be sent.
/// Uses a virtual clock so timer oversleep is compensated (up to `burst`).
pub struct RateLimiter {
    bytes_per_sec: f64,
    burst: Duration,
    next: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: f64) -> Self {
        Self {
            bytes_per_sec: bytes_per_sec.max(1.0),
            burst: Duration::from_millis(100),
            next: Mutex::new(Instant::now()),
        }
    }

    /// Suggested chunk size for this rate: about 1/50 s of data, between 1 KiB and MAX_CHUNK.
    pub fn chunk_size(&self) -> usize {
        ((self.bytes_per_sec / 50.0) as usize).clamp(1024, MAX_CHUNK)
    }

    pub async fn take(&self, n: usize) {
        let deadline = {
            let mut next = self.next.lock().unwrap();
            let now = Instant::now();
            let floor = now.checked_sub(self.burst).unwrap_or(now);
            let base = if *next < floor { floor } else { *next };
            let t = base + Duration::from_secs_f64(n as f64 / self.bytes_per_sec);
            *next = t;
            t
        };
        // Sending is allowed once the virtual clock (minus this chunk) is reached; we sleep
        // until the end of the chunk's slot so the average rate is exact.
        let now = Instant::now();
        if deadline > now {
            tokio::time::sleep_until(deadline.into()).await;
        }
    }
}

/// Server-wide counters.
#[derive(Default)]
pub struct Stats {
    pub total_requests: AtomicU64,
    pub active: AtomicUsize,
    pub peak_concurrency: AtomicUsize,
    pub bytes_served: AtomicU64,
    pub rejected_503: AtomicU64,
    pub dropped: AtomicU64,
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Print one line per request to stdout.
    pub log: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { log: true }
    }
}

struct AppState {
    config: ServerConfig,
    stats: Stats,
    /// Shared (`total_rate`) limiters, keyed by bytes/second.
    shared_limiters: Mutex<HashMap<u64, Arc<RateLimiter>>>,
}

type Shared = Arc<AppState>;

#[derive(Debug, Default, Deserialize)]
struct FileQuery {
    /// KiB/s per connection.
    rate: Option<f64>,
    /// KiB/s shared across all connections.
    total_rate: Option<f64>,
    max_conns: Option<usize>,
    norange: Option<String>,
    latency: Option<u64>,
    fail: Option<f64>,
    etag: Option<String>,
}

/// Build the axum router (useful to embed the server in tests).
fn router(state: Shared) -> Router {
    Router::new()
        .route("/file/{size}", get(file_handler))
        .route("/stats", get(stats_handler))
        .route("/stats/reset", post(stats_reset))
        .route("/", get(index))
        .with_state(state)
}

fn new_state(config: ServerConfig) -> Shared {
    Arc::new(AppState {
        config,
        stats: Stats::default(),
        shared_limiters: Mutex::new(HashMap::new()),
    })
}

/// Serve plain HTTP on an already-bound listener until the process ends.
pub async fn serve(listener: tokio::net::TcpListener, config: ServerConfig) -> std::io::Result<()> {
    let app = router(new_state(config));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

/// Bind `127.0.0.1:0`, spawn the server on the current tokio runtime, return the base URL
/// (e.g. `http://127.0.0.1:53211`). Intended for tests.
pub async fn spawn_local(config: ServerConfig) -> std::io::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = serve(listener, config).await;
    });
    Ok(format!("http://{addr}"))
}

/// Paths of a generated self-signed certificate.
pub struct GeneratedCert {
    pub cert_pem_path: std::path::PathBuf,
    pub key_pem_path: std::path::PathBuf,
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate a self-signed certificate for `localhost` / `127.0.0.1` and write it to `dir`.
pub fn generate_cert(dir: &std::path::Path) -> anyhow::Result<GeneratedCert> {
    let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let ck = rcgen::generate_simple_self_signed(names)?;
    let cert_pem = ck.cert.pem();
    let key_pem = ck.key_pair.serialize_pem();
    std::fs::create_dir_all(dir)?;
    let cert_pem_path = dir.join("thrennix-testserver-cert.pem");
    let key_pem_path = dir.join("thrennix-testserver-key.pem");
    std::fs::write(&cert_pem_path, &cert_pem)?;
    std::fs::write(&key_pem_path, &key_pem)?;
    Ok(GeneratedCert {
        cert_pem_path,
        key_pem_path,
        cert_pem,
        key_pem,
    })
}

/// Serve HTTPS (rustls, TLS 1.2/1.3 with session resumption) on a std listener.
pub async fn serve_tls(
    listener: std::net::TcpListener,
    config: ServerConfig,
    cert: &GeneratedCert,
) -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        cert.cert_pem.clone().into_bytes(),
        cert.key_pem.clone().into_bytes(),
    )
    .await?;
    listener.set_nonblocking(true)?;
    let app = router(new_state(config));
    axum_server::from_tcp_rustls(listener, tls)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    Ok(())
}

async fn index() -> &'static str {
    "thrennix-testserver\n\
     GET  /file/{size}[?rate=KiB/s&total_rate=KiB/s&max_conns=n&norange=1&latency=ms&fail=p&etag=v]\n\
     GET  /stats\n\
     POST /stats/reset\n"
}

async fn stats_handler(State(st): State<Shared>) -> Json<serde_json::Value> {
    let s = &st.stats;
    Json(serde_json::json!({
        "total_requests": s.total_requests.load(Ordering::Relaxed),
        "active": s.active.load(Ordering::Relaxed),
        "peak_concurrency": s.peak_concurrency.load(Ordering::Relaxed),
        "bytes_served": s.bytes_served.load(Ordering::Relaxed),
        "rejected_503": s.rejected_503.load(Ordering::Relaxed),
        "dropped": s.dropped.load(Ordering::Relaxed),
    }))
}

async fn stats_reset(State(st): State<Shared>) -> StatusCode {
    let s = &st.stats;
    s.total_requests.store(0, Ordering::Relaxed);
    s.peak_concurrency
        .store(s.active.load(Ordering::Relaxed), Ordering::Relaxed);
    s.bytes_served.store(0, Ordering::Relaxed);
    s.rejected_503.store(0, Ordering::Relaxed);
    s.dropped.store(0, Ordering::Relaxed);
    StatusCode::NO_CONTENT
}

fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some(s) if !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("false"))
}

/// Decrements the active counter and logs the request when the response body is finished
/// (or dropped because the client went away).
struct ActiveGuard {
    state: Shared,
    method: Method,
    path: String,
    range: String,
    status: u16,
    sent: u64,
    started: Instant,
    concurrent: usize,
    note: &'static str,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.state.stats.active.fetch_sub(1, Ordering::Relaxed);
        if self.state.config.log {
            log_line(
                &self.method,
                &self.path,
                &self.range,
                self.status,
                self.sent,
                self.started.elapsed(),
                self.concurrent,
                self.note,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn log_line(
    method: &Method,
    path: &str,
    range: &str,
    status: u16,
    sent: u64,
    dur: Duration,
    concurrent: usize,
    note: &str,
) {
    let secs = dur.as_secs_f64();
    let mbps = if secs > 0.0 {
        sent as f64 / secs / 1_048_576.0
    } else {
        0.0
    };
    println!(
        "{method:<4} {path} range={range} status={status} sent={sent} dur={secs:.3}s ({mbps:.2} MiB/s) conc={concurrent}{}{}",
        if note.is_empty() { "" } else { " " },
        note
    );
}

fn plain(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

async fn file_handler(
    State(st): State<Shared>,
    method: Method,
    Path(size_str): Path<String>,
    Query(q): Query<FileQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response<Body> {
    let started = Instant::now();
    st.stats.total_requests.fetch_add(1, Ordering::Relaxed);
    let path = uri.to_string();
    let range_hdr = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let range_log = range_hdr.clone().unwrap_or_else(|| "-".into());
    let log_now = |status: u16, note: &str| {
        if st.config.log {
            let c = st.stats.active.load(Ordering::Relaxed);
            log_line(
                &method,
                &path,
                &range_log,
                status,
                0,
                started.elapsed(),
                c,
                note,
            );
        }
    };

    let Some(size) = parse_size(&size_str) else {
        log_now(400, "bad-size");
        return plain(StatusCode::BAD_REQUEST, "bad size");
    };

    // Simulated latency before the first byte (including headers).
    if let Some(ms) = q.latency.filter(|&ms| ms > 0) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    let etag = match &q.etag {
        Some(v) if !v.is_empty() => format!("\"{}\"", v.trim_matches('"')),
        _ => format!("\"size-{size}\""),
    };

    // Range decision.
    let norange = truthy(&q.norange);
    let mut spec = if norange {
        RangeSpec::Full
    } else {
        parse_range(range_hdr.as_deref(), size)
    };
    if spec != RangeSpec::Full {
        if let Some(ir) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) {
            let ir = ir.trim();
            let matches = if ir.starts_with('"') || ir.starts_with("W/") {
                ir == etag // strong comparison
            } else {
                ir == LAST_MODIFIED
            };
            if !matches {
                spec = RangeSpec::Full;
            }
        }
    }

    let mut builder = Response::builder()
        .header(header::ETAG, &etag)
        .header(header::LAST_MODIFIED, LAST_MODIFIED)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"test-{size_str}.bin\""),
        );
    if !norange {
        builder = builder.header(header::ACCEPT_RANGES, "bytes");
    } else {
        builder = builder.header(header::ACCEPT_RANGES, "none");
    }

    let (status, start, len) = match spec {
        RangeSpec::Full => (StatusCode::OK, 0u64, size),
        RangeSpec::Partial { start, end } => {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
        }
        RangeSpec::Unsatisfiable => {
            log_now(416, "");
            return builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap();
        }
    };
    builder = builder
        .status(status)
        .header(header::CONTENT_LENGTH, HeaderValue::from(len));

    if method == Method::HEAD {
        log_now(status.as_u16(), "head");
        return builder.body(Body::empty()).unwrap();
    }

    // Concurrency accounting / max_conns.
    let concurrent = st.stats.active.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some(max) = q.max_conns {
        if concurrent > max {
            st.stats.active.fetch_sub(1, Ordering::Relaxed);
            st.stats.rejected_503.fetch_add(1, Ordering::Relaxed);
            log_now(503, "max_conns");
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::RETRY_AFTER, "1")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("too many connections"))
                .unwrap();
        }
    }
    st.stats
        .peak_concurrency
        .fetch_max(concurrent, Ordering::Relaxed);

    let per_conn = q
        .rate
        .filter(|r| *r > 0.0)
        .map(|r| Arc::new(RateLimiter::new(r * 1024.0)));
    let shared = q.total_rate.filter(|r| *r > 0.0).map(|r| {
        let bps = (r * 1024.0) as u64;
        st.shared_limiters
            .lock()
            .unwrap()
            .entry(bps)
            .or_insert_with(|| Arc::new(RateLimiter::new(bps as f64)))
            .clone()
    });
    let chunk = [per_conn.as_ref(), shared.as_ref()]
        .into_iter()
        .flatten()
        .map(|l| l.chunk_size())
        .min()
        .unwrap_or(MAX_CHUNK);

    // Optional mid-body drop.
    let drop_at = q.fail.filter(|p| *p > 0.0).and_then(|p| {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        if len > 0 && rng.gen_bool(p.min(1.0)) {
            Some(rng.gen_range(0..len))
        } else {
            None
        }
    });

    let guard = ActiveGuard {
        state: st.clone(),
        method: method.clone(),
        path: path.clone(),
        range: range_log.clone(),
        status: status.as_u16(),
        sent: 0,
        started,
        concurrent,
        note: "",
    };

    struct BodyState {
        pos: u64,
        end: u64,
        chunk: usize,
        drop_at: Option<u64>,
        per_conn: Option<Arc<RateLimiter>>,
        shared: Option<Arc<RateLimiter>>,
        guard: ActiveGuard,
    }

    let body_state = BodyState {
        pos: start,
        end: start + len,
        chunk,
        drop_at: drop_at.map(|d| start + d),
        per_conn,
        shared,
        guard,
    };

    let stream = futures_util::stream::unfold(body_state, |mut s| async move {
        if s.pos >= s.end {
            return None;
        }
        let mut n = ((s.end - s.pos) as usize).min(s.chunk);
        if let Some(d) = s.drop_at {
            if s.pos >= d {
                s.guard.note = "dropped(fail)";
                s.guard.state.stats.dropped.fetch_add(1, Ordering::Relaxed);
                s.drop_at = None;
                s.end = s.pos; // terminate after the error
                let err =
                    std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "simulated drop");
                return Some((Err(err), s));
            }
            n = n.min((d - s.pos).max(1) as usize);
        }
        if let Some(l) = &s.per_conn {
            l.take(n).await;
        }
        if let Some(l) = &s.shared {
            l.take(n).await;
        }
        let bytes = pattern_chunk(s.pos, n);
        s.pos += n as u64;
        s.guard.sent += n as u64;
        s.guard
            .state
            .stats
            .bytes_served
            .fetch_add(n as u64, Ordering::Relaxed);
        Some((Ok::<Bytes, std::io::Error>(bytes), s))
    });

    builder.body(Body::from_stream(stream)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1048576"), Some(1048576));
        assert_eq!(parse_size("100M"), Some(100 << 20));
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size("64k"), Some(64 << 10));
        assert_eq!(parse_size("1.5M"), Some(1572864));
        assert_eq!(parse_size("10MB"), Some(10 << 20));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn ranges() {
        use RangeSpec::*;
        assert_eq!(parse_range(None, 100), Full);
        assert_eq!(
            parse_range(Some("bytes=0-9"), 100),
            Partial { start: 0, end: 9 }
        );
        assert_eq!(
            parse_range(Some("bytes=10-"), 100),
            Partial { start: 10, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=-10"), 100),
            Partial { start: 90, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=-1000"), 100),
            Partial { start: 0, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=50-1000"), 100),
            Partial { start: 50, end: 99 }
        );
        assert_eq!(parse_range(Some("bytes=100-"), 100), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=20-10"), 100), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 100), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=x-y"), 100), Unsatisfiable);
        assert_eq!(parse_range(Some("items=0-1"), 100), Full);
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), Full);
        assert_eq!(parse_range(Some("bytes=0-0"), 0), Unsatisfiable);
    }

    #[test]
    fn pattern_table_matches_formula() {
        let mut buf = vec![0u8; 10_000];
        for off in [0u64, 1, 1791, 1792, 123_456_789, (5u64 << 30) + 3] {
            fill_pattern(off, &mut buf);
            for (k, b) in buf.iter().enumerate() {
                assert_eq!(*b, pattern_byte(off + k as u64));
            }
            let c = pattern_chunk(off, 5000);
            for (k, b) in c.iter().enumerate() {
                assert_eq!(*b, pattern_byte(off + k as u64));
            }
        }
    }
}
