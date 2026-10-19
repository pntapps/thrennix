mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::{temp_dir, verify, Opts, TestServer};
use thrennix_core::{DownloadRequest, Engine, EngineConfig, Error, Status};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn cfg() -> EngineConfig {
    EngineConfig {
        min_split_size: 256 * KIB,
        timeout: Duration::from_secs(10),
        ..EngineConfig::default()
    }
}

fn req(url: String, dir: &std::path::Path) -> DownloadRequest {
    DownloadRequest {
        url,
        dest_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

async fn download_and_check(size: u64, tag: &str) {
    let srv = TestServer::start(Opts::new(size)).await;
    let dir = temp_dir(tag);
    let engine = Engine::new(cfg());
    let h = engine.start(req(srv.url(&format!("files/{tag}.bin")), &dir));
    let path = h.wait().await.expect("download should succeed");
    assert_eq!(path, dir.join(format!("{tag}.bin")));
    let data = std::fs::read(&path).unwrap();
    verify(&data, size);
    assert!(!dir.join(format!("{tag}.bin.part")).exists());
    assert!(!dir.join(format!("{tag}.bin.thrennix")).exists());
    let p = h.progress();
    assert_eq!(p.status, Status::Completed);
    assert_eq!(p.downloaded, size);
    assert_eq!(p.total, Some(size));
    // wait() is callable more than once.
    assert_eq!(h.wait().await.unwrap(), path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_zero() {
    download_and_check(0, "zero").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_one() {
    download_and_check(1, "one").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_1mib_plus_3() {
    download_and_check(MIB + 3, "mib3").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_50mib_multi_connection() {
    let size = 50 * MIB;
    let srv = TestServer::start(Opts::new(size)).await;
    let dir = temp_dir("fifty");
    let engine = Engine::new(cfg());
    let h = engine.start(req(srv.url("big.bin"), &dir));
    let path = h.wait().await.expect("download should succeed");
    verify(&std::fs::read(&path).unwrap(), size);
    assert!(
        srv.stats.max_active.load(Ordering::SeqCst) >= 4,
        "should use several connections"
    );
    // Every byte fetched at most a little more than once (dropped responses may over-read).
    assert!(srv.stats.bytes_sent.load(Ordering::SeqCst) < size * 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_range_support() {
    let size = 3 * MIB + 17;
    let mut o = Opts::new(size);
    o.ranges = false;
    let srv = TestServer::start(o).await;
    let dir = temp_dir("norange");
    let engine = Engine::new(cfg());
    let h = engine.start(req(srv.url("plain.bin"), &dir));
    let path = h.wait().await.expect("download should succeed");
    verify(&std::fs::read(&path).unwrap(), size);
    assert!(!h.progress().resumable);
    assert_eq!(
        srv.stats.requests.load(Ordering::SeqCst),
        1,
        "single stream reuses the probe"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn content_disposition_and_numbering() {
    let size = 100 * KIB;
    let mut o = Opts::new(size);
    o.disposition = Some("attachment; filename*=UTF-8''r%C3%A9sum%C3%A9%3F.pdf".into());
    let srv = TestServer::start(o).await;
    let dir = temp_dir("cd");
    let engine = Engine::new(cfg());
    let p1 = engine
        .start(req(srv.url("dl?id=5"), &dir))
        .wait()
        .await
        .unwrap();
    assert_eq!(p1, dir.join("résumé_.pdf"));
    let p2 = engine
        .start(req(srv.url("dl?id=5"), &dir))
        .wait()
        .await
        .unwrap();
    assert_eq!(p2, dir.join("résumé_ (1).pdf"));
    verify(&std::fs::read(&p2).unwrap(), size);
}

async fn pause_midway(srv: &TestServer, engine: &Engine, dir: &std::path::Path, size: u64) {
    let h = engine.start(req(srv.url("pause.bin"), dir));
    let t0 = Instant::now();
    loop {
        let p = h.progress();
        if p.downloaded > size / 5 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "no progress: {p:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.pause();
    h.pause(); // idempotent
    match h.wait().await {
        Err(Error::Paused) => {}
        other => panic!("expected Paused, got {other:?}"),
    }
    assert!(matches!(h.wait().await, Err(Error::Paused)));
    let p = h.progress();
    assert_eq!(p.status, Status::Paused);
    assert!(p.downloaded < size, "should pause before completion");
    assert!(dir.join("pause.bin.thrennix").exists());
    assert!(dir.join("pause.bin.part").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_resume_via_start() {
    let size = 12 * MIB;
    let mut o = Opts::new(size);
    o.throttle_bps = Some(2 * MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("pause");
    let engine = Engine::new(cfg());
    pause_midway(&srv, &engine, &dir, size).await;
    let sent_before = srv.stats.bytes_sent.load(Ordering::SeqCst);
    let st = thrennix_core::DownloadState::load(&dir.join("pause.bin.thrennix")).unwrap();
    let saved = st.downloaded();
    assert!(saved > 0);

    let h = engine.start(req(srv.url("pause.bin"), &dir));
    let mut rx = h.subscribe();
    let first = loop {
        rx.changed().await.expect("progress channel closed early");
        let p = rx.borrow().clone();
        if p.status != Status::Probing {
            break p;
        }
    };
    assert!(
        first.downloaded >= saved,
        "resume should start from saved progress ({} < {saved})",
        first.downloaded
    );
    srv.set_opts(|o| o.throttle_bps = None);
    let path = h.wait().await.expect("resume should succeed");
    assert_eq!(
        path,
        dir.join("pause.bin"),
        "resume must not create a (1) copy"
    );
    verify(&std::fs::read(&path).unwrap(), size);
    // Resumed bytes: the remaining part plus at most what the dropped probe over-read.
    let resumed_bytes = srv.stats.bytes_sent.load(Ordering::SeqCst) - sent_before;
    eprintln!("saved {saved}, refetched {resumed_bytes} of {size}");
    assert!(!dir.join("pause.bin.thrennix").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_resume_from_state_with_new_url() {
    let size = 8 * MIB;
    let mut o = Opts::new(size);
    o.throttle_bps = Some(2 * MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("pause2");
    let engine = Engine::new(cfg());
    pause_midway(&srv, &engine, &dir, size).await;
    srv.set_opts(|o| o.throttle_bps = None);
    let h = engine.resume_from_state(
        dir.join("pause.bin.thrennix"),
        Some(srv.url("fresh-link/pause.bin")),
    );
    let path = h.wait().await.expect("resume should succeed");
    assert_eq!(path, dir.join("pause.bin"));
    verify(&std::fs::read(&path).unwrap(), size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_from_state_detects_remote_change() {
    let size = 8 * MIB;
    let mut o = Opts::new(size);
    o.throttle_bps = Some(2 * MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("changed");
    let engine = Engine::new(cfg());
    pause_midway(&srv, &engine, &dir, size).await;
    srv.set_opts(|o| {
        o.throttle_bps = None;
        o.etag = Some("\"v2\"".into());
    });
    let h = engine.resume_from_state(dir.join("pause.bin.thrennix"), None);
    match h.wait().await {
        Err(Error::RemoteChanged) => {}
        other => panic!("expected RemoteChanged, got {other:?}"),
    }
    // `start` restarts from scratch instead.
    // (it must never publish a transient Failed status before restarting).
    let h = engine.start(req(srv.url("pause.bin"), &dir));
    let mut rx = h.subscribe();
    let watcher = tokio::spawn(async move {
        let mut seen = Vec::new();
        loop {
            seen.push(rx.borrow_and_update().status.clone());
            if rx.changed().await.is_err() {
                break seen;
            }
        }
    });
    let path = h.wait().await.unwrap();
    assert!(h.is_finished());
    drop(h);
    let seen = watcher.await.unwrap();
    assert!(
        !seen.iter().any(|s| matches!(s, Status::Failed(_))),
        "transient Failed published: {seen:?}"
    );
    assert_eq!(path, dir.join("pause.bin"));
    verify(&std::fs::read(&path).unwrap(), size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_limits_concurrent_connections() {
    let size = 20 * MIB;
    let mut o = Opts::new(size);
    o.max_concurrent = Some(4);
    o.throttle_bps = Some(8 * MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("limit");
    let engine = Engine::new(EngineConfig {
        initial_connections: 8,
        ..cfg()
    });
    let h = engine.start(req(srv.url("limited.bin"), &dir));
    let path = h.wait().await.expect("should complete despite 503s");
    verify(&std::fs::read(&path).unwrap(), size);
    let rejected = srv.stats.rejected.load(Ordering::SeqCst);
    let requests = srv.stats.requests.load(Ordering::SeqCst);
    eprintln!("rejected {rejected} of {requests} requests");
    assert!(rejected > 0, "server should have rejected some");
    // The engine should lower its connection cap instead of hammering the server.
    assert!(rejected <= 12, "too many rejected requests: {rejected}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_connections_beat_per_connection_throttle() {
    let size = 4 * MIB;
    let mut o = Opts::new(size);
    o.throttle_bps = Some(MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("throttle");

    let one = Engine::new(EngineConfig {
        initial_connections: 1,
        max_connections: 1,
        ..cfg()
    });
    let t = Instant::now();
    let p = one.start(DownloadRequest {
        file_name: Some("one.bin".into()),
        ..req(srv.url("t.bin"), &dir)
    });
    verify(&std::fs::read(p.wait().await.unwrap()).unwrap(), size);
    let t_one = t.elapsed();

    let many = Engine::new(EngineConfig {
        initial_connections: 8,
        max_connections: 16,
        ..cfg()
    });
    let t = Instant::now();
    let p = many.start(DownloadRequest {
        file_name: Some("many.bin".into()),
        ..req(srv.url("t.bin"), &dir)
    });
    verify(&std::fs::read(p.wait().await.unwrap()).unwrap(), size);
    let t_many = t.elapsed();

    eprintln!("1 connection: {t_one:?}, many: {t_many:?}");
    assert!(
        t_one.as_secs_f64() > 4.0 * t_many.as_secs_f64(),
        "expected >4x speedup: one={t_one:?} many={t_many:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_is_published() {
    let size = 4 * MIB;
    let mut o = Opts::new(size);
    o.throttle_bps = Some(2 * MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("progress");
    let engine = Engine::new(cfg());
    let h = engine.start(req(srv.url("p.bin"), &dir));
    let mut rx = h.subscribe();
    let mut saw_downloading = false;
    let mut saw_speed = false;
    while rx.changed().await.is_ok() {
        let p = rx.borrow().clone();
        if p.status == Status::Downloading {
            saw_downloading = true;
            assert!(p.resumable);
            if p.speed_bps > 0.0 && p.connections > 1 && !p.segments.is_empty() {
                saw_speed = true;
            }
        }
        if p.status == Status::Completed {
            break;
        }
    }
    assert!(saw_downloading && saw_speed);
    h.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_limit_of_six_keeps_503s_low() {
    let size = 24 * MIB;
    let cap = 6;
    let mut o = Opts::new(size);
    o.max_concurrent = Some(cap);
    o.throttle_bps = Some(4 * MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("limit6");
    let engine = Engine::new(EngineConfig {
        initial_connections: 8,
        max_connections: 32,
        ..cfg()
    });
    let h = engine.start(req(srv.url("limited6.bin"), &dir));
    let path = h.wait().await.expect("should complete despite 503s");
    verify(&std::fs::read(&path).unwrap(), size);
    let rejected = srv.stats.rejected.load(Ordering::SeqCst);
    let requests = srv.stats.requests.load(Ordering::SeqCst);
    eprintln!("limit {cap}: rejected {rejected} of {requests} requests");
    assert!(rejected > 0, "server should have rejected some");
    assert!(rejected < 3 * cap, "too many 503s: {rejected}");
    assert!(srv.stats.max_active.load(Ordering::SeqCst) <= cap);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ramps_up_connections_quickly() {
    // 2 MiB/s per connection: more connections keep paying off up to the cap.
    let size = 128 * MIB;
    let mut o = Opts::new(size);
    o.throttle_bps = Some(2 * MIB);
    let srv = TestServer::start(o).await;
    let dir = temp_dir("ramp");
    let engine = Engine::new(EngineConfig {
        max_connections: 32,
        min_split_size: MIB,
        ..cfg()
    });
    let t = Instant::now();
    let path = engine
        .start(req(srv.url("ramp.bin"), &dir))
        .wait()
        .await
        .unwrap();
    let el = t.elapsed();
    verify(&std::fs::read(&path).unwrap(), size);
    let peak = srv.stats.max_active.load(Ordering::SeqCst);
    let avg = size as f64 / el.as_secs_f64() / MIB as f64;
    eprintln!("ramp: {el:?}, avg {avg:.1} MiB/s, peak connections {peak}");
    assert!(
        peak >= 24,
        "should have grown well past 8 connections (peak {peak})"
    );
    // 8 fixed connections would need 8 s; ramping to ~32 must be clearly faster.
    assert!(el < Duration::from_secs(5), "ramp-up too slow: {el:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn html_instead_of_file_is_rejected() {
    let mut o = Opts::new(5000);
    o.content_type = "text/html; charset=utf-8".into();
    let srv = TestServer::start(o).await;
    let dir = temp_dir("html");
    let engine = Engine::new(cfg());
    match engine
        .start(req(srv.url("files/setup.exe?token=expired"), &dir))
        .wait()
        .await
    {
        Err(Error::Other(m)) => assert!(m.contains("web page instead of the file"), "{m}"),
        other => panic!("expected html rejection, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_dir(&*dir).unwrap().count(),
        0,
        "nothing should be left on disk"
    );
    // An .html URL, or an explicit opt-out, is fine.
    engine
        .start(req(srv.url("page.html"), &dir))
        .wait()
        .await
        .unwrap();
    let h = engine.start_with_options(
        req(srv.url("files/setup.exe"), &dir),
        thrennix_core::StartOptions { allow_html: true },
    );
    verify(&std::fs::read(h.wait().await.unwrap()).unwrap(), 5000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probing_progress_has_no_path_yet() {
    let srv = TestServer::start(Opts::new(10)).await;
    let dir = temp_dir("probing");
    let engine = Engine::new(cfg());
    let h = engine.start(DownloadRequest {
        file_name: Some("x.bin".into()),
        ..req(srv.url("x"), &dir)
    });
    let p = h.progress();
    if p.status == Status::Probing {
        assert!(
            p.path.as_os_str().is_empty(),
            "path must be empty while probing"
        );
        assert!(p.file_name.is_empty());
    }
    assert!(!h.is_finished() || h.progress().status == Status::Completed);
    let path = h.wait().await.unwrap();
    assert!(h.is_finished());
    let p = h.progress();
    assert_eq!(p.status, Status::Completed);
    assert_eq!(p.path, path);
    assert_eq!(p.file_name, "x.bin");
}
