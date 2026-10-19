//! In-process HTTP test server serving a deterministic file: byte i = (i*31 + i/7) as u8.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;

pub fn byte_at(i: u64) -> u8 {
    (i.wrapping_mul(31).wrapping_add(i / 7)) as u8
}

pub fn fill(start: u64, buf: &mut [u8]) {
    for (k, b) in buf.iter_mut().enumerate() {
        *b = byte_at(start + k as u64);
    }
}

/// Panics with the first mismatching offset.
pub fn verify(data: &[u8], size: u64) {
    assert_eq!(data.len() as u64, size, "file size");
    let mut exp = vec![0u8; 1 << 20];
    for (ci, chunk) in data.chunks(1 << 20).enumerate() {
        let off = (ci as u64) << 20;
        fill(off, &mut exp[..chunk.len()]);
        if chunk != &exp[..chunk.len()] {
            let k = chunk
                .iter()
                .zip(&exp)
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            panic!("content mismatch at byte {}", off + k as u64);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub size: u64,
    pub ranges: bool,
    pub max_concurrent: Option<usize>,
    /// Per-connection throttle, bytes/second.
    pub throttle_bps: Option<u64>,
    pub etag: Option<String>,
    pub disposition: Option<String>,
    pub content_type: String,
}

impl Opts {
    pub fn new(size: u64) -> Self {
        Self {
            size,
            ranges: true,
            max_concurrent: None,
            throttle_bps: None,
            etag: Some("\"v1\"".into()),
            disposition: None,
            content_type: "application/octet-stream".into(),
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicUsize,
    pub rejected: AtomicUsize,
    pub active: AtomicUsize,
    pub max_active: AtomicUsize,
    pub bytes_sent: AtomicU64,
}

struct AppState {
    opts: Mutex<Opts>,
    stats: Arc<Stats>,
}

pub struct TestServer {
    pub addr: SocketAddr,
    pub stats: Arc<Stats>,
    state: Arc<AppState>,
}

impl TestServer {
    pub async fn start(opts: Opts) -> Self {
        let stats = Arc::new(Stats::default());
        let state = Arc::new(AppState {
            opts: Mutex::new(opts),
            stats: stats.clone(),
        });
        let app = axum::Router::new()
            .fallback(handler)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, stats, state }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}/{}", self.addr, path.trim_start_matches('/'))
    }

    pub fn set_opts(&self, f: impl FnOnce(&mut Opts)) {
        f(&mut self.state.opts.lock().unwrap());
    }
}

struct ActiveGuard(Arc<Stats>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let r = h.trim().strip_prefix("bytes=")?;
    let (a, b) = r.split_once('-')?;
    let a: u64 = a.trim().parse().ok()?;
    let b: u64 = if b.trim().is_empty() {
        size.checked_sub(1)?
    } else {
        b.trim().parse().ok()?
    };
    let b = b.min(size.checked_sub(1)?);
    if a > b {
        return None;
    }
    Some((a, b + 1))
}

async fn handler(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let opts = st.opts.lock().unwrap().clone();
    let stats = st.stats.clone();
    stats.requests.fetch_add(1, Ordering::SeqCst);
    let now_active = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
    let guard = ActiveGuard(stats.clone());
    if let Some(max) = opts.max_concurrent {
        if now_active > max {
            stats.rejected.fetch_add(1, Ordering::SeqCst);
            drop(guard);
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("busy"))
                .unwrap();
        }
    }
    stats.max_active.fetch_max(now_active, Ordering::SeqCst);

    let size = opts.size;
    let mut b = Response::builder().header(header::CONTENT_TYPE, opts.content_type.as_str());
    if let Some(e) = &opts.etag {
        b = b.header(header::ETAG, e.as_str());
    }
    if let Some(d) = &opts.disposition {
        b = b.header(header::CONTENT_DISPOSITION, d.as_str());
    }
    let range_hdr = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let if_range_ok = match headers.get(header::IF_RANGE) {
        None => true,
        Some(v) => {
            opts.etag
                .as_deref()
                .map(HeaderValue::from_str)
                .and_then(Result::ok)
                .as_ref()
                == Some(v)
        }
    };
    let (status, start, end) = match (&range_hdr, opts.ranges && if_range_ok) {
        (Some(r), true) => {
            if size == 0 {
                return b
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, "bytes */0")
                    .body(Body::empty())
                    .unwrap();
            }
            match parse_range(r, size) {
                Some((a, e)) => {
                    b = b.header(
                        header::CONTENT_RANGE,
                        format!("bytes {}-{}/{}", a, e - 1, size),
                    );
                    (StatusCode::PARTIAL_CONTENT, a, e)
                }
                None => {
                    return b
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                        .body(Body::empty())
                        .unwrap()
                }
            }
        }
        _ => (StatusCode::OK, 0, size),
    };
    if opts.ranges {
        b = b.header(header::ACCEPT_RANGES, "bytes");
    }
    b = b.header(header::CONTENT_LENGTH, end - start);

    let chunk: u64 = if opts.throttle_bps.is_some() {
        16 * 1024
    } else {
        64 * 1024
    };
    let throttle = opts.throttle_bps;
    let began = Instant::now();
    let stream = futures_util::stream::unfold(
        (start, guard, stats),
        move |(pos, guard, stats)| async move {
            if pos >= end {
                return None;
            }
            if let Some(bps) = throttle {
                let sent = pos - start;
                let due = Duration::from_secs_f64(sent as f64 / bps as f64);
                let el = began.elapsed();
                if due > el {
                    tokio::time::sleep(due - el).await;
                }
            }
            let n = chunk.min(end - pos);
            let mut v = vec![0u8; n as usize];
            fill(pos, &mut v);
            stats.bytes_sent.fetch_add(n, Ordering::Relaxed);
            Some((
                Ok::<Bytes, std::io::Error>(Bytes::from(v)),
                (pos + n, guard, stats),
            ))
        },
    );
    b.status(status).body(Body::from_stream(stream)).unwrap()
}

/// Temporary directory removed on drop (also when the test panics).
pub struct TempDir(std::path::PathBuf);

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn temp_dir(name: &str) -> TempDir {
    let d = std::env::temp_dir().join(format!("thrennix-it-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}
